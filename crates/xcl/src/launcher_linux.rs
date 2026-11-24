//! Linux `xcl run` launcher (Phase C step 10e): resolve the game folder/exe,
//! prepare the title data dir + config + manifest, then run the title in-process
//! through `host_linux::run_boot`.
//!
//! Mirrors the Windows `launcher.rs` flow without `CreateProcess`, COM/menu,
//! DXC/D3D12 preflight or the TLS carrier cache. No Windows DLL is loaded.

use std::path::{Path, PathBuf};

use crate::config::{Boot, Config};
use crate::manifest::{decode_game_config, xml_attrs};

fn xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    xml_attrs(xml, tag, attr).into_iter().next()
}

/// (executables, title) from MicrosoftGame.config if present.
pub fn display_name(root: &Path) -> Option<String> {
    let xml = std::fs::read(root.join("MicrosoftGame.config"))
        .ok()
        .and_then(|raw| decode_game_config(&raw))?;
    xml_attr(&xml, "ShellVisuals", "DefaultDisplayName")
        .filter(|n| !n.trim().is_empty() && !n.starts_with("ms-resource:"))
}

pub fn read_game_config(root: &Path) -> (Vec<String>, Option<String>) {
    let p = root.join("MicrosoftGame.config");
    let Some(xml) = std::fs::read(p)
        .ok()
        .and_then(|raw| decode_game_config(&raw))
    else {
        return (Vec::new(), None);
    };
    (
        xml_attrs(&xml, "Executable", "Name"),
        xml_attr(&xml, "Identity", "Name"),
    )
}

/// `xcl` itself (by name, path or identical bytes) must never be picked as the game.
fn is_xcl_copy(p: &Path) -> bool {
    if p.file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("xcl.exe") || n.eq_ignore_ascii_case("xcl"))
    {
        return true;
    }
    let Ok(me) = std::env::current_exe() else {
        return false;
    };
    if std::fs::canonicalize(p).ok() == std::fs::canonicalize(&me).ok() {
        return true;
    }
    match (std::fs::metadata(p), std::fs::metadata(&me)) {
        (Ok(a), Ok(b)) if a.len() == b.len() => std::fs::read(p).ok() == std::fs::read(&me).ok(),
        _ => false,
    }
}

fn is_exe(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .map(|x| x.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
}

/// Executables in `dir` and its subfolders (up to `depth` levels), excluding XCL itself.
fn find_exes(dir: &Path, depth: u32, skipped_self: &mut bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() && depth > 0 {
            out.extend(find_exes(&p, depth - 1, skipped_self));
        } else if is_exe(&p) {
            if is_xcl_copy(&p) {
                *skipped_self = true;
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn pick_exe(root: &Path) -> Result<PathBuf, String> {
    let (cfg_exes, _) = read_game_config(root);
    if !cfg_exes.is_empty() {
        for e in &cfg_exes {
            // Config names use Windows separators; normalise for the host.
            let p = root.join(e.replace('\\', "/"));
            if is_exe(&p) && !is_xcl_copy(&p) {
                return Ok(p);
            }
        }
        return Err(format!(
            "MicrosoftGame.config names {}, but there is no such file in {}",
            cfg_exes.join(", "),
            root.display()
        ));
    }
    let mut skipped_self = false;
    let rel = |p: &PathBuf| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let top = find_exes(root, 0, &mut skipped_self);
    let candidates = if top.is_empty() {
        find_exes(root, 2, &mut skipped_self)
    } else {
        top
    };
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().unwrap()),
        0 => Err(format!(
            "no game .exe file in {}{}",
            root.display(),
            if skipped_self {
                " (only xcl – that is XCL itself; do not copy it into the game folder)"
            } else {
                ""
            }
        )),
        _ => Err(format!(
            "several .exe files ({}) – point to the game exe directly or set 'exe' in xcl.toml",
            candidates.iter().map(rel).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn sanitize(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if t.is_empty() {
        "title".into()
    } else {
        t
    }
}

/// The package folder of `exe`: the nearest folder above it (up to 4 levels) whose
/// MicrosoftGame.config names it.
fn config_root(exe: &Path) -> Option<PathBuf> {
    let mut dir = exe.parent()?;
    for _ in 0..5 {
        let (exes, _) = read_game_config(dir);
        let canonical_exe = std::fs::canonicalize(exe).ok();
        if exes.iter().any(|e| {
            let p = dir.join(e.replace('\\', "/"));
            std::fs::canonicalize(&p).ok() == canonical_exe
        }) {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
    None
}

fn resolve_location(target: &Path) -> Result<(PathBuf, Option<PathBuf>, String), String> {
    let root;
    let exe;
    if target.is_dir() {
        let canonical =
            std::fs::canonicalize(target).map_err(|e| format!("{}: {e}", target.display()))?;
        root = canonical;
        exe = None;
    } else {
        if is_xcl_copy(target) {
            return Err(format!(
                "{} is XCL itself, not a game – give the game exe or folder",
                target.display()
            ));
        }
        let canonical =
            std::fs::canonicalize(target).map_err(|e| format!("{}: {e}", target.display()))?;
        let parent = canonical.parent().ok_or("no parent folder")?.to_path_buf();
        root = config_root(&canonical).unwrap_or(parent);
        exe = Some(canonical);
    }
    let (_, identity) = read_game_config(&root);
    let title = sanitize(&identity.unwrap_or_else(|| {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }));
    Ok((root, exe, title))
}

pub fn resolve_target(target: &Path) -> Result<(PathBuf, PathBuf, String), String> {
    let (root, explicit_exe, title) = resolve_location(target)?;
    let exe = match explicit_exe {
        Some(exe) => exe,
        None => pick_exe(&root)?,
    };
    Ok((root, exe, title))
}

/// Instruction set extensions of the console's Zen 2 that this CPU does not have.
pub fn missing_console_cpu_features() -> Vec<&'static str> {
    check_cpu_features(|f| match f {
        "sse4.2" => std::arch::is_x86_feature_detected!("sse4.2"),
        "popcnt" => std::arch::is_x86_feature_detected!("popcnt"),
        "avx" => std::arch::is_x86_feature_detected!("avx"),
        "avx2" => std::arch::is_x86_feature_detected!("avx2"),
        "fma" => std::arch::is_x86_feature_detected!("fma"),
        "f16c" => std::arch::is_x86_feature_detected!("f16c"),
        "bmi1" => std::arch::is_x86_feature_detected!("bmi1"),
        "bmi2" => std::arch::is_x86_feature_detected!("bmi2"),
        "lzcnt" => std::arch::is_x86_feature_detected!("lzcnt"),
        "movbe" => std::arch::is_x86_feature_detected!("movbe"),
        _ => false,
    })
}

/// Helper to evaluate CPU features using a query closure, returning missing feature names.
pub fn check_cpu_features<F: Fn(&str) -> bool>(has: F) -> Vec<&'static str> {
    const REQUIRED: &[(&str, &str)] = &[
        ("sse4.2", "SSE4.2"),
        ("popcnt", "POPCNT"),
        ("avx", "AVX"),
        ("avx2", "AVX2"),
        ("fma", "FMA"),
        ("f16c", "F16C"),
        ("bmi1", "BMI1"),
        ("bmi2", "BMI2"),
        ("lzcnt", "LZCNT"),
        ("movbe", "MOVBE"),
    ];
    let mut missing = Vec::new();
    for &(feature, name) in REQUIRED {
        if !has(feature) {
            missing.push(name);
        }
    }
    missing
}

/// Formats a friendly error message listing the missing console Zen 2 CPU features.
pub fn format_missing_cpu_message(missing: &[&str]) -> String {
    format!(
        "this CPU lacks {}: Xbox Series X|S games are compiled for the console's Zen 2 CPU\n     and need them (they would stop with an illegal instruction). XCL_SKIP_CPU_CHECK=1 runs anyway.",
        missing.join(", ")
    )
}

pub fn run(target: &Path, _args: &[String]) -> i32 {
    let (root, explicit_exe, title) = match resolve_location(target) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("xcl: {e}");
            return 2;
        }
    };
    let data_dir = crate::platform::data_root().join(&title);
    let mut config = match Config::load_or_create(&data_dir.join("xcl.toml")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xcl: error in xcl.toml: {e}");
            return 2;
        }
    };
    let configured_exe = root.join(config.exe.replace('\\', "/"));
    let exe = if let Some(exe) = explicit_exe {
        exe
    } else if !config.exe.is_empty() && is_exe(&configured_exe) && !is_xcl_copy(&configured_exe) {
        configured_exe
    } else {
        match pick_exe(&root) {
            Ok(exe) => exe,
            Err(error) => {
                eprintln!("xcl: {error}");
                return 2;
            }
        }
    };
    for d in ["logs", "cache", "overlay", "saves", "pls", "temp"] {
        let _ = std::fs::create_dir_all(data_dir.join(d));
    }
    config.exe = exe
        .strip_prefix(&root)
        .ok()
        .map(|p| p.to_string_lossy().replace('/', "\\"))
        .or_else(|| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let manifest = crate::manifest::load(&root);
    let from_manifest = config.resolve(manifest.as_ref());
    // LINUX-TODO: Phase C step-10e owns the TLS carrier cache on Linux; no cleanup yet.
    let boot = Boot {
        title: title.clone(),
        exe_path: exe.clone(),
        game_root: root.clone(),
        data_dir: data_dir.clone(),
        config,
    };
    let boot_path = data_dir.join("cache").join("boot.json");
    if let Err(e) = std::fs::write(&boot_path, serde_json::to_string_pretty(&boot).unwrap()) {
        eprintln!("xcl: writing {}: {e}", boot_path.display());
        return 2;
    }
    let logs = boot.logs_dir();
    let _ = std::fs::rename(logs.join("xcl.log"), logs.join("xcl.prev.log"));
    let _ = std::fs::rename(boot.report_path(), logs.join("report.prev.json"));
    let _ = std::fs::remove_file(boot.report_path());

    println!("XCL {} – starting '{title}'", env!("CARGO_PKG_VERSION"));
    println!("  exe:   {}", exe.display());
    println!("  data:  {}", data_dir.display());
    if !from_manifest.is_empty() {
        println!("  MicrosoftGame.Config: {}", from_manifest.join(", "));
    }
    match crate::inspect::console_imports(&exe) {
        Some(c) if c.is_empty() => println!(
            "  NOTE: this file imports no console library (kernelx / xgameruntime / d3d12_xs) – it may be a synthetic test title."
        ),
        Some(c) => println!("  console: imports {}", c.join(", ")),
        None => println!("  WARNING: this is not a valid x64 PE file."),
    }
    // Linux preflight is the CPU check only; DXC/D3D12 probes are Windows-only.
    println!("  preflight: GPU/DXC checks are Windows-only on Linux (Phase D)");
    let missing = missing_console_cpu_features();
    if !missing.is_empty() {
        if std::env::var_os("XCL_SKIP_CPU_CHECK").is_some() {
            println!(
                "  WARNING: this CPU lacks {} (XCL_SKIP_CPU_CHECK set, continuing)",
                missing.join(", ")
            );
        } else {
            eprintln!("xcl: {}", format_missing_cpu_message(&missing));
            return 2;
        }
    }

    let code = crate::host::run_boot(&boot);
    summarize(&boot, code);
    code as i32
}

fn summarize(boot: &Boot, code: u32) {
    let path = boot.report_path();
    let mut report = crate::report::load(&path);
    if let Some(r) = report.as_mut() {
        r.exit_code = Some(code as i64);
        let _ = std::fs::write(&path, serde_json::to_string_pretty(r).unwrap_or_default());
    }
    println!();
    println!("Game exited with code {code:#x} ({code})");
    let Some(r) = report else {
        println!(
            "No report – see the log: {}",
            boot.logs_dir().join("xcl.log").display()
        );
        return;
    };
    if let Some(c) = &r.crash {
        println!("CRASH: {c}");
    }
    let total: usize = r.imports_by_source.values().sum();
    println!(
        "Imports: {total} ({})",
        r.imports_by_source
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !r.called_stubs.is_empty() {
        println!("Unimplemented functions called ({}):", r.called_stubs.len());
        for s in r.called_stubs.iter().take(15) {
            println!(
                "  {:>8}×  {}!{}   (first call: {})",
                s.calls, s.dll, s.function, s.first_caller
            );
        }
    }
    if !r.vfs.unmapped_paths.is_empty() {
        println!("Unmapped console paths: {}", r.vfs.unmapped_paths.len());
        for p in r.vfs.unmapped_paths.iter().take(5) {
            println!("  {p}");
        }
    }
    if !r.vfs.redirected_writes.is_empty() {
        println!(
            "Writes to game files redirected to the overlay: {}",
            r.vfs.redirected_writes.len()
        );
    }
    println!("Report: {}", path.display());
    println!("Log:    {}", boot.logs_dir().join("xcl.log").display());
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let p = std::env::temp_dir()
                .join(format!("xcl_launcher_linux_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_folder_is_a_clean_error() {
        let code = run(Path::new("/tmp/xcl-linux-no-such-title-17955209"), &[]);
        assert_eq!(code, 2);
    }

    #[test]
    fn folder_without_an_exe_is_a_clean_error() {
        // Isolate XCL_DATA_ROOT so the test never touches the real data dir.
        let data = TempDir::new("nodata");
        let prev = std::env::var_os("XCL_DATA_ROOT");
        std::env::set_var("XCL_DATA_ROOT", &data.0);
        let game = TempDir::new("noexe");
        let code = run(&game.0, &[]);
        match prev {
            Some(v) => std::env::set_var("XCL_DATA_ROOT", v),
            None => std::env::remove_var("XCL_DATA_ROOT"),
        }
        assert_eq!(code, 2);
    }

    #[test]
    fn cpu_feature_detection_identifies_missing_features() {
        assert!(check_cpu_features(|_| true).is_empty());
        assert_eq!(
            check_cpu_features(|_| false),
            vec![
                "SSE4.2", "POPCNT", "AVX", "AVX2", "FMA", "F16C", "BMI1", "BMI2", "LZCNT", "MOVBE"
            ]
        );
        assert_eq!(check_cpu_features(|f| f != "avx2"), vec!["AVX2"]);
    }

    #[test]
    fn missing_cpu_message_format() {
        let msg = format_missing_cpu_message(&["AVX2", "FMA"]);
        assert!(msg.contains("this CPU lacks AVX2, FMA"));
        assert!(msg.contains("Zen 2 CPU"));
        assert!(msg.contains("XCL_SKIP_CPU_CHECK=1"));
    }

    #[test]
    fn never_picks_xcl_itself() {
        let d = TempDir::new("self");
        let me = std::env::current_exe().unwrap();
        std::fs::copy(&me, d.0.join("xcl.exe")).unwrap();
        std::fs::copy(&me, d.0.join("renamed_copy.exe")).unwrap(); // byte-identical copy of XCL
        let e = resolve_target(&d.0).unwrap_err();
        assert!(e.contains("XCL itself"), "{e}");
        assert!(resolve_target(&d.0.join("xcl.exe"))
            .unwrap_err()
            .contains("XCL itself"));
        // a real game exe next to it wins
        std::fs::write(d.0.join("game.exe"), b"MZ not really a game").unwrap();
        let (_, exe, _) = resolve_target(&d.0).unwrap();
        assert_eq!(exe.file_name().unwrap(), "game.exe");
    }

    #[test]
    fn finds_exe_in_subfolder_and_via_config() {
        let d = TempDir::new("sub");
        std::fs::create_dir_all(d.0.join("Bin")).unwrap();
        std::fs::write(d.0.join("Bin").join("game.exe"), b"MZ").unwrap();
        let (_, exe, _) = resolve_target(&d.0).unwrap();
        assert!(
            exe.ends_with("Bin/game.exe") || exe.ends_with("Bin\\game.exe"),
            "{exe:?}"
        );
        std::fs::write(
            d.0.join("MicrosoftGame.config"),
            r#"<Game><Identity Name="Contoso.Test"/><ExecutableList><Executable Name="Bin\missing.exe"/></ExecutableList></Game>"#,
        )
        .unwrap();
        let e = resolve_target(&d.0).unwrap_err();
        assert!(e.contains("missing.exe"), "{e}");
    }

    fn tiny_exe_returning(code: u8, image_base: u64) -> Vec<u8> {
        let entry: Vec<u8> = vec![0xB8, code, 0, 0, 0, 0xC3];
        let mut f = vec![0u8; 0x400];
        f[0..2].copy_from_slice(b"MZ");
        f[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        f[0x40..0x44].copy_from_slice(b"PE\0\0");
        f[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes());
        f[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        f[0x54..0x56].copy_from_slice(&0xF0u16.to_le_bytes());
        f[0x56..0x58].copy_from_slice(&0x0022u16.to_le_bytes());
        let o = 0x58usize;
        f[o..o + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        f[o + 0x10..o + 0x14].copy_from_slice(&0x1000u32.to_le_bytes());
        f[o + 0x14..o + 0x18].copy_from_slice(&0x1000u32.to_le_bytes());
        f[o + 0x18..o + 0x20].copy_from_slice(&image_base.to_le_bytes());
        f[o + 0x20..o + 0x24].copy_from_slice(&0x1000u32.to_le_bytes());
        f[o + 0x24..o + 0x28].copy_from_slice(&0x200u32.to_le_bytes());
        f[o + 0x38..o + 0x3C].copy_from_slice(&0x3000u32.to_le_bytes());
        f[o + 0x3C..o + 0x40].copy_from_slice(&0x200u32.to_le_bytes());
        f[o + 0x44..o + 0x46].copy_from_slice(&3u16.to_le_bytes());
        f[o + 0x48..o + 0x50].copy_from_slice(&0x10_0000u64.to_le_bytes());
        f[o + 0x50..o + 0x58].copy_from_slice(&0x1000u64.to_le_bytes());
        f[o + 0x58..o + 0x60].copy_from_slice(&0x10_0000u64.to_le_bytes());
        f[o + 0x60..o + 0x68].copy_from_slice(&0x1000u64.to_le_bytes());
        f[o + 0x6C..o + 0x70].copy_from_slice(&16u32.to_le_bytes());
        let s = o + 0xF0;
        f[s..s + 5].copy_from_slice(b".text");
        f[s + 8..s + 12].copy_from_slice(&0x200u32.to_le_bytes());
        f[s + 12..s + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        f[s + 16..s + 20].copy_from_slice(&0x200u32.to_le_bytes());
        f[s + 20..s + 24].copy_from_slice(&0x200u32.to_le_bytes());
        f[s + 36..s + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        f[0x200..0x200 + entry.len()].copy_from_slice(&entry);
        f
    }

    #[test]
    fn synthetic_game_folder_runs_through_the_launcher() {
        const CHILD: &str = "XCL_LAUNCHER_LINUX_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let game = PathBuf::from(std::env::var("XCL_LAUNCHER_LINUX_GAME").unwrap());
            // Skip the CPU gate in CI containers; the synthetic title needs no AVX2.
            std::env::set_var("XCL_SKIP_CPU_CHECK", "1");
            let code = run(&game, &[]);
            assert_eq!(
                code, 0x2a,
                "synthetic entry returns 0x2a through launcher::run"
            );
            return;
        }
        let game = TempDir::new("e2e");
        std::fs::write(
            game.0.join("game.exe"),
            tiny_exe_returning(0x2a, 0x6E62_0000_0000),
        )
        .unwrap();
        let data = TempDir::new("e2edata");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("launcher::tests::synthetic_game_folder_runs_through_the_launcher")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("XCL_LAUNCHER_LINUX_GAME", &game.0)
            .env("XCL_DATA_ROOT", &data.0)
            .env("XCL_SKIP_CPU_CHECK", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "launcher child failed: {}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Game exited with code 0x2a"),
            "launcher summary names the entry code"
        );
        // The title name derives from the game folder; the report records the exit code.
        let report = std::fs::read_dir(&data.0)
            .unwrap()
            .flatten()
            .map(|e| e.path().join("logs").join("report.json"))
            .find(|p| p.is_file())
            .expect("launcher writes a title report under XCL_DATA_ROOT");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
        assert_eq!(v["exit_code"], serde_json::json!(42));
        // The game folder itself is untouched apart from the exe we wrote.
        assert!(game.0.join("game.exe").is_file());
    }
}
