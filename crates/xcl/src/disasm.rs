//! `xcl disasm <file> [--offset N] [--length N] [--all] [--hex] [--fssm FILE]` – RDNA2
//! disassembler for captured shader blobs (or any binary with console GPU code).
//! Console containers (XDAT/XBCH/FSSM) are unpacked and their layout is printed.

use std::path::Path;

use crate::cli::Parser;
use crate::gpu::{rdna2, shader, xbox};

fn parse_num(s: &str) -> Option<usize> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => usize::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

#[derive(Debug, Default)]
struct Args {
    offset: Option<usize>,
    length: Option<usize>,
    all: bool,
    hex: bool,
    fssm: Option<String>,
}

/// Parse `xcl disasm` options and reject anything unknown, so a typo cannot silently
/// disassemble with defaults (`rpf`/`shaders` use the same `Parser`).
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut p = Parser::new(args);
    while let Some(arg) = p.next_arg() {
        match arg {
            "--offset" => {
                let value = p.value("--offset")?;
                parsed.offset =
                    Some(parse_num(value).ok_or_else(|| format!("invalid value for --offset: {value}"))?);
            }
            "--length" => {
                let value = p.value("--length")?;
                parsed.length =
                    Some(parse_num(value).ok_or_else(|| format!("invalid value for --length: {value}"))?);
            }
            "--fssm" => parsed.fssm = Some(p.value("--fssm")?.to_owned()),
            "--all" => parsed.all = true,
            "--hex" => parsed.hex = true,
            other => return Err(Parser::unexpected(other)),
        }
    }
    // `--all` only widens a forced range (`print_lines`); without one it would be dropped.
    if parsed.all && parsed.offset.is_none() && parsed.length.is_none() {
        return Err("--all requires --offset or --length".to_string());
    }
    // A forced range bypasses container parsing, so `--fssm` could never save there.
    if parsed.fssm.is_some() && (parsed.offset.is_some() || parsed.length.is_some()) {
        return Err("--fssm cannot be combined with --offset/--length".to_string());
    }
    Ok(parsed)
}

fn print_lines(code: &[u8], base: usize, stop: bool) {
    for l in rdna2::disassemble(code, base as u64, stop) {
        let hex: Vec<String> = l.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mark = if l.flags & rdna2::F_BRANCH != 0 { "  ; →" } else { "" };
        println!("  {:08x}: {:<52} {}{mark}", l.pc, l.text, hex.join(" "));
    }
}

fn hexdump(data: &[u8], base: usize) {
    for (i, row) in data.chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let txt: String = row.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        println!("    {:06x}: {:<47}  {txt}", base + i * 16, hex.join(" "));
    }
}

fn xdat_label(id: u32) -> &'static str {
    match id {
        xbox::XDAT_HASH => "hash",
        xbox::XDAT_XBCH => "XBCH → FSSM (native code)",
        xbox::XDAT_NESTED_HEADER => "nested DXIL header (NOBC)",
        _ => "?",
    }
}

/// "dw7 (s7)": a user-data dword and, where known for the stage, its SGPR
fn user_dword(hw: u8, dword: u8) -> String {
    match xbox::user_sgpr(hw, dword) {
        Some(s) => format!("dw{dword} (s{s})"),
        None => format!("dw{dword}"),
    }
}

/// The stage's root layout and the resources it uses (the first 8 without --hex).
fn print_root_layout(data: &[u8], stage: &xbox::HwStage, all: bool) {
    let l = match xbox::root_layout(data, stage) {
        Ok(l) => l,
        Err(e) => {
            println!("    root layout: not readable ({e})");
            return;
        }
    };
    let mut parts: Vec<String> = l.slots.iter().map(|s| format!("slot {} {} ×{} @ {}", s.slot, s.kind.name(), s.sgprs, user_dword(stage.hw, s.dword))).collect();
    for (name, dword) in [("view heap", l.view_heap), ("sampler heap", l.sampler_heap)] {
        if let Some(d) = dword {
            parts.push(format!("{name} @ {}", user_dword(stage.hw, d)));
        }
    }
    if l.static_samplers > 0 {
        parts.push(format!("{} static samplers", l.static_samplers));
    }
    println!("    root layout: {}", if parts.is_empty() { "none".into() } else { parts.join(", ") });
    let shown = if all { l.used.len() } else { l.used.len().min(8) };
    for u in &l.used[..shown] {
        let via = match u.via {
            // a static sampler is in no user SGPR: its slot byte is the sampler's index
            xbox::RootKind::StaticSampler => format!("static sampler {}", u.slot),
            v => format!("{} slot {} @ {}, table +{} dw", v.name(), u.slot, user_dword(stage.hw, u.dword), u.offset),
        };
        println!("    uses {:?} via {via}, register {:#x}, descriptor {} dw", u.kind, u.register, u.dwords);
    }
    if shown < l.used.len() {
        println!("    … {} more used resources (--hex: all)", l.used.len() - shown);
    }
}

/// Prints the container layout and, with `--fssm`, saves every FSSM.
/// Returns how many were written; `Err(())` if a requested write failed.
fn print_xbox(sh: &xbox::XboxShader, full_hex: bool, fssm_out: Option<&str>) -> Result<usize, ()> {
    let mut saved = 0usize;
    let mut failed = false;
    println!(
        "console container: stage {} (DXIL), SM {}.{}, XDAT @ +{:#x}",
        sh.stage, sh.shader_model.0, sh.shader_model.1, sh.xdat_offset
    );
    for e in &sh.entries {
        println!("  XDAT {:#04x}  +{:#x}  {} B  {}", e.id, e.offset, e.size, xdat_label(e.id));
    }
    if let Some(h) = sh.hash {
        println!("  hash: {}", h.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    for n in &sh.notes {
        println!("  NOTE: {n}");
    }
    for (i, f) in sh.fssm.iter().enumerate() {
        println!(
            "FSSM #{i}: XBCH v{} @ +{:#x}, zlib {} B → {} B, FSSM v{:#x}",
            f.xbch_version,
            f.xbch_offset,
            f.compressed,
            f.data.len(),
            f.version
        );
        if let Some(e) = &f.error {
            println!("  WARNING: {e}");
        }
        for s in &f.stages {
            println!("  hardware stage: {} – descriptor @ +{:#x}", s.label(), s.offset);
            print_root_layout(&f.data, s, full_hex);
        }
        match f.code {
            Some((o, l)) => println!(
                "  code: +{o:#x}, {l} B ({}), programs: {}",
                if f.code_from_header { "pointer from header +0xC0" } else { "found by scanning" },
                f.programs.len()
            ),
            None => println!("  code: not found"),
        }
        let limit = if full_hex { usize::MAX } else { 0x140 };
        println!("  header – non-zero qwords (without the 0xDEADBEEF filler){}:", if full_hex { "" } else { ", up to +0x140; --hex: all" });
        for (o, q) in xbox::header_qwords(f, limit) {
            println!("    +{o:#05x}: {q:016x}");
        }
        if full_hex {
            if let Some((o, l)) = f.code {
                println!("  data after the code (+{:#x}):", o + l);
                hexdump(&f.data[o + l..], o + l);
            }
        }
        if let Some(out) = fssm_out {
            let name = if i == 0 { out.to_string() } else { format!("{out}.{i}") };
            match std::fs::write(&name, &f.data) {
                Ok(()) => {
                    println!("  saved FSSM to {name}");
                    saved += 1;
                }
                Err(e) => {
                    eprintln!("xcl disasm: {name}: {e}");
                    failed = true;
                }
            }
        }
    }
    if failed { Err(()) } else { Ok(saved) }
}

pub fn run(path: &Path, args: &[String]) -> i32 {
    let args = match parse_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("xcl disasm: {error}");
            return 2;
        }
    };
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xcl disasm: {}: {e}", path.display());
            return 2;
        }
    };
    println!("{} – {} B, decoder: {}", path.display(), data.len(), crate::gpu::RDNA2_ARCH);

    if args.offset.is_some() || args.length.is_some() {
        let off = args.offset.unwrap_or(0);
        if off >= data.len() {
            eprintln!("xcl disasm: offset {off:#x} is past the end of the file");
            return 2;
        }
        let len = args.length.unwrap_or(data.len() - off).min(data.len() - off);
        print_lines(&data[off..off + len], off, !args.all);
        return 0;
    }

    let (kind, parts) = shader::classify(&data);
    println!("format: {}", kind.label());
    if !parts.is_empty() {
        println!("container parts:");
        for p in &parts {
            println!("  {}  +{:#x}  {} B", p.fourcc, p.offset, p.size);
        }
    }
    let mut fssm_saved = 0usize;
    let mut fssm_failed = false;
    if kind == shader::Kind::XboxNative {
        if let Some(sh) = xbox::parse(&data) {
            match print_xbox(&sh, args.hex, args.fssm.as_deref()) {
                Ok(n) => fssm_saved = n,
                Err(()) => fssm_failed = true,
            }
        }
    }
    let listing = shader::rdna2_listing(&data);
    if listing.is_empty() {
        println!("no RDNA2 programs found (ending in s_endpgm); use --offset N to force decoding");
        if matches!(kind, shader::Kind::Dxil | shader::Kind::Dxbc) {
            println!("(this is a PC-format shader – no recompilation needed)");
        }
    } else {
        print!("{listing}");
    }
    if fssm_failed {
        return 2;
    }
    if args.fssm.is_some() && fssm_saved == 0 {
        eprintln!("xcl disasm: {}: no FSSM to save", path.display());
        return 2;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn options_parse_values_and_reject_unknown_or_missing_ones() {
        let parsed = parse_args(&args(&["--offset", "0x40", "--length", "16", "--all", "--hex"])).unwrap();
        assert_eq!(parsed.offset, Some(0x40));
        assert_eq!(parsed.length, Some(16));
        assert!(parsed.all);
        assert!(parsed.hex);
        assert_eq!(parse_args(&args(&["--fssm", "out.fssm"])).unwrap().fssm.as_deref(), Some("out.fssm"));

        assert_eq!(parse_args(&args(&["--bogus"])).unwrap_err(), "unknown option: --bogus; see `xcl --help`");
        assert_eq!(parse_args(&args(&["stray"])).unwrap_err(), "unexpected argument: stray; see `xcl --help`");
        assert_eq!(parse_args(&args(&["--offset"])).unwrap_err(), "--offset needs a value");
        assert_eq!(parse_args(&args(&["--offset", "--hex"])).unwrap_err(), "--offset needs a value");
        assert_eq!(parse_args(&args(&["--offset", "xyz"])).unwrap_err(), "invalid value for --offset: xyz");
        assert_eq!(parse_args(&args(&["--fssm"])).unwrap_err(), "--fssm needs a value");
        // options whose consumer is absent are errors, not silent no-ops
        assert_eq!(parse_args(&args(&["--all"])).unwrap_err(), "--all requires --offset or --length");
        assert_eq!(parse_args(&args(&["--all", "--hex"])).unwrap_err(), "--all requires --offset or --length");
        assert_eq!(
            parse_args(&args(&["--offset", "0", "--fssm", "out.fssm"])).unwrap_err(),
            "--fssm cannot be combined with --offset/--length"
        );
        assert_eq!(
            parse_args(&args(&["--length", "4", "--fssm", "out.fssm"])).unwrap_err(),
            "--fssm cannot be combined with --offset/--length"
        );
    }
}
