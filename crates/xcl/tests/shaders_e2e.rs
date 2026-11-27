#[path = "common/shader_container.rs"]
mod shader_container;

use shader_container::{container, dxil_part, fssm};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// s_mov_b32 s0, s1 ; v_add_f32 v0, v1, v5 ; s_endpgm
const CODE: [u8; 12] = [0x01, 0x03, 0x80, 0xbe, 0x01, 0x0b, 0x00, 0x06, 0x00, 0x00, 0x81, 0xbf];

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("xcl-shaders-{tag}-{}-{nonce}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn xbox_shader(kind: u32, fssms: &[Vec<u8>]) -> Vec<u8> {
    container(&[(b"SFI0", &[0; 8]), (b"HASH", &[0; 20]), (b"DXIL", &dxil_part(kind, fssms)), (b"PRIV", &[7; 16])])
}

#[cfg(windows)]
fn xbox_shader_with_osg(kind: u32, fssms: &[Vec<u8>], osg: &[u8]) -> Vec<u8> {
    let base = xbox_shader(kind, fssms);
    let dxil = xcl::gpu::shader::container_parts(&base).unwrap().into_iter().find(|p| p.fourcc == "DXIL").unwrap();
    let dxil_bytes = &base[dxil.offset..dxil.offset + dxil.size];
    dxbc_container(&[(b"SFI0", &[0; 8]), (b"OSG1", osg), (b"HASH", &[0; 20]), (b"DXIL", dxil_bytes), (b"PRIV", &[7; 16])])
}

fn rpf9_record(name: &str, payload: &[u8], usize_override: Option<u32>) -> Vec<u8> {
    let z = miniz_oxide::deflate::compress_to_vec(payload, 6);
    let mut v = vec![(name.len() + 1) as u8];
    v.extend_from_slice(name.as_bytes());
    v.push(0);
    v.push(50);
    for x in [z.len() as u32, 0xddce7, z.len() as u32, usize_override.unwrap_or(payload.len() as u32)] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&z);
    v
}

fn run_shaders(target: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xcl"))
        .arg("shaders")
        .arg(target)
        .args(extra)
        .env_remove("XCL_DUMP_HLSL")
        .env_remove("XCL_RECOMP_ONLY")
        .env_remove("XCL_EXAMPLE_HLSL")
        .output()
        .expect("spawn xcl shaders")
}

#[test]
fn help_lists_shader_precompile_options() {
    let out = Command::new(env!("CARGO_BIN_EXE_xcl")).arg("--help").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let help = String::from_utf8(out.stdout).unwrap();
    for flag in ["--precompile", "--only", "--pipelines", "--used-only"] {
        assert!(help.contains(flag), "missing {flag} in xcl --help");
    }
}

#[test]
fn shaders_single_file_reports_text_top_json_recomp_and_dxc_compile() {
    let dir = TempDir::new("single");
    let file = dir.path().join("ps.bin");
    std::fs::write(&file, xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();

    // Default text output
    let text_out = run_shaders(&file, &["--recomp"]);
    assert!(text_out.status.success(), "{text_out:?}");
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(text.contains("single file"), "{text}");
    assert!(text.contains("shaders: 1"), "{text}");
    assert!(text.contains("formats: xbox-native 1"), "{text}");
    assert!(text.contains("stages (DXIL): PS 1"), "{text}");
    assert!(text.contains("FSSM variants by hardware stage: PS 1"), "{text}");
    assert!(text.contains("RDNA2 instructions: 3, undecoded dwords: 0 (0.0000%), distinct mnemonics: 3"), "{text}");
    assert!(text.contains("top 3 mnemonics"), "{text}");
    assert!(text.contains("recompiler (kernel mode): programs 1, fully translated 1 (100.0%), translation errors 0"), "{text}");

    // Plain --recomp JSON reports the same translation without a DXC request (all platforms).
    let recomp_out = run_shaders(&file, &["--json", "--recomp"]);
    assert!(recomp_out.status.success(), "{recomp_out:?}");
    let recomp: serde_json::Value = serde_json::from_slice(&recomp_out.stdout).unwrap();
    let rr = &recomp["recomp"];
    assert_eq!(rr["programs"], 1, "{recomp}");
    assert_eq!(rr["complete"], 1, "{recomp}");
    assert_eq!(rr["translate_errors"], 0, "{recomp}");
    assert!(rr["hlsl_bytes"].as_u64().unwrap() > 0, "{recomp}");
    assert!(rr["dxc_compiled"].is_null(), "{recomp}");

    // --top 1 limits the printed mnemonic table
    let top_out = run_shaders(&file, &["--top", "1"]);
    assert!(top_out.status.success(), "{top_out:?}");
    let top_text = String::from_utf8(top_out.stdout).unwrap();
    assert!(top_text.contains("top 1 mnemonics"), "{top_text}");
    let ranked_lines: Vec<&str> = top_text
        .lines()
        .skip_while(|l| !l.starts_with("top 1 mnemonics"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .collect();
    assert_eq!(ranked_lines.len(), 1, "{top_text}");

    // --json --recomp --compile 1: translation is counted everywhere; DXC is Windows-only.
    let json_out = run_shaders(&file, &["--json", "--recomp", "--compile", "1"]);
    #[cfg(windows)]
    assert!(json_out.status.success(), "{json_out:?}");
    #[cfg(not(windows))]
    {
        // Honest refusal: the analysis JSON is still printed, and the command fails with a
        // message naming DXC as Windows-only instead of claiming a compile it did not do.
        assert_eq!(json_out.status.code(), Some(2), "{json_out:?}");
        let stderr = String::from_utf8_lossy(&json_out.stderr);
        assert!(stderr.contains("DXC is Windows-only"), "{stderr}");
        assert!(stderr.contains("Phase D"), "{stderr}");
    }
    let json: serde_json::Value = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(json["source"], "single file");
    assert_eq!(json["shaders"], 1);
    assert_eq!(json["inflate_errors"], 0);
    assert_eq!(json["by_kind"]["xbox-native"], 1);
    assert_eq!(json["by_dxil_stage"]["PS"], 1);
    assert_eq!(json["by_hw_stage"]["PS"], 1);
    assert_eq!(json["fssm"], 1);
    assert_eq!(json["programs"], 1);
    assert_eq!(json["parse_problems"], 0);
    assert_eq!(json["instructions"], 3);
    assert_eq!(json["undecoded_dwords"], 0);
    assert_eq!(json["distinct_mnemonics"], 3);
    assert_eq!(json["mnemonics"].as_array().unwrap().len(), 3);
    let rc = &json["recomp"];
    assert_eq!(rc["programs"], 1, "{json}");
    assert_eq!(rc["complete"], 1, "{json}");
    assert_eq!(rc["translate_errors"], 0, "{json}");
    assert!(rc["hlsl_bytes"].as_u64().unwrap() > 0, "{json}");
    #[cfg(windows)]
    {
        assert_eq!(rc["dxc_compiled"], 1, "{json}");
        assert_eq!(rc["dxc_failed"].as_array().unwrap().len(), 0, "{json}");
    }
    #[cfg(not(windows))]
    {
        assert!(rc["dxc_compiled"].is_null(), "{json}"); // no compile ran: no false success
        assert!(rc["dxc_failed"].is_null(), "{json}");
    }
}

#[test]
fn shaders_folder_recurses_and_skips_side_files() {
    let dir = TempDir::new("folder");
    let nested = dir.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();

    std::fs::write(dir.path().join("ps.bin"), xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();
    std::fs::write(
        nested.join("vs.bin"),
        xbox_shader(1, &[fssm(&CODE, &[(2, 0)]), fssm(&CODE, &[(4, 0)])]),
    )
    .unwrap();
    std::fs::write(dir.path().join("plain.bin"), b"not a shader").unwrap();
    // Side files written by XCL itself in <data>\shaders\ must be skipped by collect_files.
    std::fs::write(dir.path().join("listing.txt"), b"disassembly side file").unwrap();
    std::fs::write(nested.join("dump.fssm.bin"), xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();

    let out = run_shaders(dir.path(), &["--json"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["source"], "folder, 3 files", "{json}");
    assert_eq!(json["shaders"], 3);
    assert_eq!(json["by_kind"]["xbox-native"], 2);
    assert_eq!(json["by_kind"]["unknown"], 1);
    assert_eq!(json["by_dxil_stage"]["PS"], 1);
    assert_eq!(json["by_dxil_stage"]["VS"], 1);
    assert_eq!(json["by_hw_stage"]["ES"], 1);
    assert_eq!(json["by_hw_stage"]["VS"], 1);
    assert_eq!(json["by_hw_stage"]["PS"], 1);
    assert_eq!(json["fssm"], 3);
    assert_eq!(json["programs"], 3);
    assert_eq!(json["instructions"], 9);
    assert_eq!(json["parse_problems"], 0);
}

#[test]
fn shaders_rpf9_pack_scans_records_and_counts_inflate_errors() {
    let dir = TempDir::new("rpf9");
    let pack_path = dir.path().join("shaders.rpf");
    let ps = xbox_shader(0, &[fssm(&CODE, &[(5, 1)])]);
    let mut pack = b"9FPR".to_vec();
    pack.resize(0x40, 0xAA);
    pack.extend(rpf9_record("PS_Synthetic_1234abcd_Wrapped", &ps, None));
    pack.extend_from_slice(&[0x13, 0x37, 0, 1, 2, 3]);
    pack.extend(rpf9_record("VS_PlainBlob", b"DXBC-not-console", None));
    // Mismatched uncompressed size forces pack::inflate to return None -> inflate_errors == 1.
    pack.extend(rpf9_record("CS_BadInflate", b"synthetic payload", Some(999)));
    pack.extend_from_slice(&[0u8; 40]);
    std::fs::write(&pack_path, &pack).unwrap();

    let out = run_shaders(&pack_path, &["--json"]);
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        json["source"].as_str().unwrap().starts_with("RPF9 package "),
        "{json}"
    );
    assert_eq!(json["shaders"], 2, "{json}");
    assert_eq!(json["inflate_errors"], 1, "{json}");
    assert_eq!(json["by_kind"]["xbox-native"], 1);
    assert_eq!(json["by_kind"]["unknown"], 1);
    assert_eq!(json["by_dxil_stage"]["PS"], 1);
    assert_eq!(json["instructions"], 3);
    assert_eq!(json["undecoded_dwords"], 0);
}

#[test]
fn shaders_handles_non_console_file_and_missing_path() {
    let dir = TempDir::new("edge");
    let plain = dir.path().join("plain.bin");
    std::fs::write(&plain, b"not a shader").unwrap();

    let out = run_shaders(&plain, &[]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("shaders: 1"), "{text}");
    assert!(text.contains("no console containers (xbox-native)"), "{text}");

    let missing = dir.path().join("does-not-exist.bin");
    let err_out = run_shaders(&missing, &[]);
    assert_eq!(err_out.status.code(), Some(2), "{err_out:?}");
    let stderr = String::from_utf8(err_out.stderr).unwrap();
    assert!(stderr.contains("xcl shaders:"), "{stderr}");
}

#[test]
fn shaders_single_file_runs_the_requested_sweep_modes() {
    let dir = TempDir::new("single-modes");
    let file = dir.path().join("ps.bin");
    std::fs::write(&file, xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();

    // `--direct` is a mode: on one file it runs the direct sweep, as it does on a folder.
    let direct = run_shaders(&file, &["--direct"]);
    assert!(direct.status.success(), "{direct:?}");
    let text = String::from_utf8(direct.stdout).unwrap();
    assert!(text.contains("direct DXIL coverage"), "{text}");
    assert!(!text.contains("single file"), "{text}");

    // `--precompile` is a mode too: Linux refuses it by name instead of analysing the file.
    let pre = run_shaders(&file, &["--precompile"]);
    #[cfg(windows)]
    {
        assert!(pre.status.success(), "{pre:?}");
        let text = String::from_utf8(pre.stdout).unwrap();
        assert!(!text.contains("single file"), "{text}");
    }
    #[cfg(not(windows))]
    {
        assert_eq!(pre.status.code(), Some(2), "{pre:?}");
        assert!(pre.stdout.is_empty(), "{pre:?}");
        let stderr = String::from_utf8_lossy(&pre.stderr);
        assert!(stderr.contains("--precompile") && stderr.contains("DXC is Windows-only"), "{stderr}");
    }
}

#[test]
fn shaders_rejects_two_exclusive_modes() {
    let dir = TempDir::new("mode-conflict");
    let file = dir.path().join("ps.bin");
    std::fs::write(&file, xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();

    let cases: &[&[&str]] = &[
        &["--direct", "--record-vs"],
        &["--precompile", "--direct"],
        &["--export-hlsl", "out", "--measure-pso", "CAFE"],
        &["--record-vs", "--measure-pso", "CAFE"],
    ];
    for args in cases {
        let out = run_shaders(&file, args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "args {args:?}: unexpected stdout {:?}", String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("xcl shaders:"), "args {args:?}: {stderr}");
        assert!(stderr.contains("exclusive modes"), "args {args:?}: {stderr}");
        assert!(stderr.contains("run one mode per invocation"), "args {args:?}: {stderr}");
    }

    // One mode alone still runs.
    let one = run_shaders(&file, &["--direct", "--sample", "1"]);
    assert!(one.status.success(), "{one:?}");
    assert!(String::from_utf8_lossy(&one.stdout).contains("direct DXIL coverage"), "{one:?}");
}

/// An option read only inside one mode is a request that mode; given without it, the run would
/// parse the value and silently drop it.
#[test]
fn shaders_mode_options_require_their_mode() {
    let dir = TempDir::new("mode-options");
    let file = dir.path().join("ps.bin");
    std::fs::write(&file, xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();
    let dump = dir.path().join("dump");
    let dump_arg = dump.to_str().unwrap();

    let cases: &[(&[&str], &str)] = &[
        (&["--count", "2"], "--count requires --export-hlsl"),
        (&["--min-kb", "1"], "--min-kb requires --export-hlsl"),
        (&["--record-hash", "1"], "--record-hash requires --export-hlsl"),
        (&["--repeat", "3"], "--repeat requires --measure-pso"),
        (&["--limit", "1"], "--limit requires --precompile"),
        (&["--only", "cs"], "--only requires --precompile"),
        (&["--pipelines"], "--pipelines requires --precompile"),
        (&["--used-only"], "--used-only requires --precompile"),
        (&["--sample", "1"], "--sample requires one of --record-vs, --direct"),
        (&["--seed", "2"], "--seed requires one of --record-vs, --direct"),
        (&["--threads", "2"], "--threads requires one of --record-vs, --direct"),
        (&["--trace"], "--trace requires one of --record-vs, --direct"),
        (&["--validate"], "--validate requires --direct"),
        (&["--compare"], "--compare requires --direct"),
        (&["--max-bytes", "16"], "--max-bytes requires --direct"),
        (&["--timeout", "2"], "--timeout requires --direct"),
        (&["--lanes"], "--lanes requires --direct"),
        (&["--hash", "1"], "--hash requires --direct"),
        (&["--dump", dump_arg], "--dump requires --direct"),
        (&["--split", "1"], "--split requires one of --export-hlsl, --measure-pso, --precompile, --direct"),
        (&["--windows", "2"], "--windows requires one of --export-hlsl, --measure-pso, --precompile, --direct"),
    ];
    for (args, expected) in cases {
        let out = run_shaders(&file, args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "args {args:?}: unexpected stdout {:?}", String::from_utf8_lossy(&out.stdout));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("xcl shaders:"), "args {args:?}: {stderr}");
        assert!(stderr.contains(expected), "args {args:?}: expected {expected:?} in {stderr:?}");
    }
    assert!(!dump.exists(), "a rejected --dump must not create {}", dump.display());

    // With its mode each option reaches that mode: no `requires` error, the mode's own outcome.
    let direct = run_shaders(&file, &["--direct", "--sample", "1", "--seed", "2", "--threads", "1", "--dump", dump_arg]);
    assert!(direct.status.success(), "{direct:?}");
    assert!(String::from_utf8_lossy(&direct.stdout).contains("direct DXIL coverage"), "{direct:?}");

    let record_vs = run_shaders(&file, &["--record-vs", "--sample", "1"]);
    assert!(record_vs.status.success(), "{record_vs:?}");
    assert!(String::from_utf8_lossy(&record_vs.stdout).contains("records from"), "{record_vs:?}");

    // On Linux these modes refuse by name (DXC/Phase D): the option's mode was found, so the
    // run must get past the scope check, not stop at it.
    let cases: &[(&[&str], &str)] = &[
        (&["--precompile", "--limit", "1", "--only", "cs", "--pipelines", "--used-only"], "--precompile"),
        (&["--measure-pso", "1", "--repeat", "2"], "--measure-pso"),
        (&["--export-hlsl", dump_arg, "--min-kb", "1", "--count", "2"], "--export-hlsl"),
        (&["--direct", "--validate", "--compare"], "--direct"),
    ];
    for (args, mode) in cases {
        let out = run_shaders(&file, args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains(" requires "), "args {args:?}: unexpected scope error {stderr:?}");
        assert!(stderr.contains(mode), "args {args:?}: expected the {mode} refusal in {stderr:?}");
        assert!(out.stdout.is_empty(), "args {args:?}: unexpected stdout {:?}", String::from_utf8_lossy(&out.stdout));
    }
}

#[test]
fn shaders_direct_reports_an_unusable_dump_directory() {
    let dir = TempDir::new("dump-fail");
    let file = dir.path().join("ps.bin");
    std::fs::write(&file, xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();
    let dump = blocker.join("sub");

    let out = run_shaders(&file, &["--direct", "--sample", "1", "--dump", dump.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "unexpected stdout {:?}", String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("xcl shaders --direct: cannot create"), "{stderr}");
}

#[test]
fn shaders_direct_hash_without_a_match_is_an_error() {
    let dir = TempDir::new("hash-miss");
    let file = dir.path().join("ps.bin");
    let blob = xbox_shader(0, &[fssm(&CODE, &[(5, 1)])]);
    let hash = format!("{:016x}", xcl::gpu::shader::fnv64(&blob));
    std::fs::write(&file, &blob).unwrap();

    let dump = dir.path().join("dump");
    let hit = run_shaders(&file, &["--direct", "--hash", &hash, "--dump", dump.to_str().unwrap()]);
    assert!(hit.status.success(), "{hit:?}");
    let text = String::from_utf8_lossy(&hit.stdout);
    assert!(text.starts_with("1 programs from"), "{text}");
    assert!(std::fs::read_dir(&dump).unwrap().next().is_some(), "no files in {}", dump.display());

    let miss = run_shaders(&file, &["--direct", "--hash", "0xdeadbeefdeadbeef"]);
    assert_eq!(miss.status.code(), Some(2), "{miss:?}");
    assert!(miss.stdout.is_empty(), "unexpected stdout {:?}", String::from_utf8_lossy(&miss.stdout));
    let stderr = String::from_utf8_lossy(&miss.stderr);
    assert!(stderr.contains("record deadbeefdeadbeef not found"), "{stderr}");

    // A record that exists but carries no console program cannot satisfy the request either.
    let plain = dir.path().join("plain.bin");
    std::fs::write(&plain, b"not a shader").unwrap();
    let plain_hash = format!("{:016x}", xcl::gpu::shader::fnv64(b"not a shader"));
    let unusable = run_shaders(&plain, &["--direct", "--hash", &plain_hash]);
    assert_eq!(unusable.status.code(), Some(2), "{unusable:?}");
    assert!(unusable.stdout.is_empty(), "unexpected stdout {:?}", String::from_utf8_lossy(&unusable.stdout));
    let stderr = String::from_utf8_lossy(&unusable.stderr);
    assert!(stderr.contains("has no console programs"), "{stderr}");
}

#[test]
fn shaders_missing_arguments_prints_help_and_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .arg("shaders")
        .output()
        .expect("spawn xcl shaders");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("xcl shaders <file | folder | package.rpf>"), "{stdout}");
}

#[test]
fn shaders_required_options_reject_missing_values() {
    let dir = TempDir::new("val");
    let file = dir.path().join("plain.bin");
    std::fs::write(&file, b"not a shader").unwrap();

    let cases: &[(&[&str], &str)] = &[
        (&["--export-hlsl"], "--export-hlsl needs a value"),
        (&["--export-hlsl", "--record-hash", "DEADBEEF"], "--export-hlsl needs a value"),
        (&["--record-hash"], "--record-hash needs a value"),
        (&["--record-hash", "--measure-pso", "CAFE"], "--record-hash needs a value"),
        (&["--measure-pso"], "--measure-pso needs a value"),
        (&["--measure-pso", "--json"], "--measure-pso needs a value"),
    ];

    for (args, expected_msg) in cases {
        let out = run_shaders(&file, args);
        assert_eq!(out.status.code(), Some(2), "args {args:?} unexpected status: {out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(expected_msg), "args {args:?}: {stderr}");
    }
}

#[test]
fn shaders_rejects_unknown_options_missing_values_and_mistyped_values() {
    let dir = TempDir::new("badopts");
    let file = dir.path().join("plain.bin");
    std::fs::write(&file, b"not a shader").unwrap();

    let cases: &[(&[&str], &str)] = &[
        (&["--fnid", "foo"], "unknown option: --fnid"),
        (&["-x"], "unknown option: -x"),
        (&["stray"], "unexpected argument: stray"),
        (&["--top"], "--top needs a value"),
        (&["--top", "abc"], "invalid value for --top: abc"),
        (&["--compile", "nope"], "invalid value for --compile: nope"),
        (&["--windows", "four"], "invalid value for --windows: four"),
        (&["--split", "2"], "invalid value for --split: 2"),
        (&["--min-kb", "abc"], "invalid value for --min-kb: abc"),
        (&["--count", "abc"], "invalid value for --count: abc"),
        (&["--repeat", "abc"], "invalid value for --repeat: abc"),
        (&["--limit", "abc"], "invalid value for --limit: abc"),
        (&["--only"], "--only needs a value"),
        (&["--only", "--pipelines"], "--only needs a value"),
        (&["--example"], "--example needs a value"),
        (&["--direct", "--sample", "abc"], "invalid value for --sample: abc"),
        (&["--direct", "--seed", "abc"], "invalid value for --seed: abc"),
        (&["--direct", "--threads", "abc"], "invalid value for --threads: abc"),
        (&["--direct", "--max-bytes", "abc"], "invalid value for --max-bytes: abc"),
        (&["--direct", "--timeout", "abc"], "invalid value for --timeout: abc"),
        (&["--direct", "--hash"], "--hash needs a value"),
        (&["--direct", "--hash", "0xZZ"], "--hash:"),
        (&["--direct", "--dump"], "--dump needs a value"),
    ];

    for (args, expected_msg) in cases {
        let out = run_shaders(&file, args);
        assert_eq!(out.status.code(), Some(2), "args {args:?} unexpected status: {out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("xcl shaders:"), "args {args:?}: {stderr}");
        assert!(stderr.contains(expected_msg), "args {args:?}: expected {expected_msg:?} in {stderr:?}");
        assert!(out.stdout.is_empty(), "args {args:?}: unexpected stdout {:?}", String::from_utf8_lossy(&out.stdout));
    }
}


/// Constant S# in s[24:27] (point/clamp) + image_sample_lz -> v[10:13] + export (keeps sampler live) + s_endpgm.
#[cfg(windows)]
fn static_sampler_code(exp_w0: u32) -> Vec<u8> {
    let words: [u32; 11] = [
        0xBE98_03FF, 0x0000_0092, 0xBE99_03FF, 0x00FF_F000, 0xBE9A_0380, 0xBE9B_0380,
        0xF09C_0F08, 0x00C4_0A00, exp_w0, 0x0D0C_0B0A, 0xBF81_0000,
    ];
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[cfg(windows)]
fn find_files_named(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(find_files_named(&p, needle));
        } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(needle)) {
            out.push(p);
        }
    }
    out
}

#[cfg(windows)]
fn time_native_graphics_pso(vs: &xcl::gpu::recomp::pso::Compiled, ps: &xcl::gpu::recomp::pso::Compiled) -> Result<Option<f64>, String> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, IDXGIAdapter1, IDXGIFactory6, DXGI_CREATE_FACTORY_FLAGS};
    let Ok(factory) = (unsafe { CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) }) else {
        return Ok(None);
    };
    let sel = xcl::gpu::adapter::Selector::from_env_or("auto");
    let adapter: IDXGIAdapter1 = if sel == xcl::gpu::adapter::Selector::Warp {
        let Ok(a) = (unsafe { factory.EnumWarpAdapter() }) else { return Ok(None) };
        a
    } else {
        let Some(a) = xcl::gpu::adapter::candidates(&factory, &sel)
            .into_iter()
            .next()
            .map(|(a, _)| a)
            .or_else(|| unsafe { factory.EnumWarpAdapter().ok() })
        else {
            return Ok(None);
        };
        a
    };
    let mut dev: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut dev) }.is_err() {
        return Ok(None);
    }
    let Some(dev) = dev else { return Ok(None) };
    let mut samplers = vs.samplers.clone();
    samplers.extend_from_slice(&ps.samplers);
    let rs = xcl::gpu::recomp::pso::abi_root_signature_with(&dev, true, &samplers)
        .map_err(|e| format!("abi_root_signature_with failed: {e}"))?;
    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(rs)),
        VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.dxil.as_ptr().cast(), BytecodeLength: vs.dxil.len() },
        PS: D3D12_SHADER_BYTECODE { pShaderBytecode: ps.dxil.as_ptr().cast(), BytecodeLength: ps.dxil.len() },
        SampleMask: u32::MAX,
        RasterizerState: D3D12_RASTERIZER_DESC { FillMode: D3D12_FILL_MODE_SOLID, CullMode: D3D12_CULL_MODE_NONE, ..Default::default() },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    desc.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
    desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    let t0 = std::time::Instant::now();
    let pso: Result<ID3D12PipelineState, _> = unsafe { dev.CreateGraphicsPipelineState(&desc) };
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    unsafe { std::mem::ManuallyDrop::drop(&mut desc.pRootSignature) };
    pso.map(|_| Some(ms))
        .map_err(|e| format!("CreateGraphicsPipelineState failed: {e} ({:#x})", e.code().0 as u32))
}

#[cfg(windows)]
fn spawn_child_check(test_name: &str, dir: &Path, cache: &Path, direct: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env("XCL_SHADERS_E2E_CHILD", "1")
        .env("XCL_SHADERS_E2E_DIR", dir)
        .env("XCL_DIRECT_DXIL", direct)
        .env("XCL_DXIL_CACHE", cache)
        .env_remove("XCL_TRANSLATED_CACHE")
        .output()
        .expect("spawn fresh child test process")
}

#[cfg(windows)]
#[test]
fn precompile_warms_direct_vs_ps_and_hits_in_fresh_process() {
    use xcl::gpu::recomp::{dxc, pso};
    if std::env::var_os("XCL_SHADERS_E2E_CHILD").is_some() {
        let dir = PathBuf::from(std::env::var_os("XCL_SHADERS_E2E_DIR").unwrap());
        let (vs_blob, ps_blob) = (std::fs::read(dir.join("vs.bin")).unwrap(), std::fs::read(dir.join("ps.bin")).unwrap());
        let t0 = std::time::Instant::now();
        let (vs_res, vs_missed) = dxc::cached_only(|| pso::console_to_dxil_in(&vs_blob, pso::Stage::Vertex, 0, false, 1));
        let (ps_res, ps_missed) = dxc::cached_only(|| pso::console_to_dxil_in(&ps_blob, pso::Stage::Pixel, 0, false, 1));
        let warm_ms = t0.elapsed().as_secs_f64() * 1e3;
        let (vs_c, ps_c) = (vs_res.expect("warm direct VS hit"), ps_res.expect("warm direct PS hit"));
        assert!(!vs_missed && !ps_missed, "fresh process must hit translated/v2 cache");
        assert_eq!(dxc::compile_counts().1, 0, "fresh process direct hit must invoke 0 DXC compiles");
        match time_native_graphics_pso(&vs_c, &ps_c).expect("direct graphics PSO creation failed") {
            Some(pso_ms) => println!("fresh_child_timing: mode=direct warm_translate_dxc_ms={warm_ms:.3} native_pso_ms={pso_ms:.3}"),
            None => {
                xcl::gpu::recomp::runner::skip_test("no D3D12 device");
                println!("fresh_child_timing: mode=direct warm_translate_dxc_ms={warm_ms:.3} native_pso_ms=skipped(no_gpu)");
            }
        }
        return;
    }
    let (dir, cache) = (TempDir::new("direct-gfx"), TempDir::new("direct-gfx-cache"));
    std::fs::write(dir.path().join("vs.bin"), xbox_shader(1, &[fssm(&CODE, &[(4, 0)])])).unwrap();
    std::fs::write(dir.path().join("ps.bin"), xbox_shader(0, &[fssm(&CODE, &[(5, 1)])])).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(["shaders", dir.path().to_str().unwrap(), "--precompile", "--windows", "1", "--split", "0"])
        .env("XCL_DIRECT_DXIL", "all")
        .env("XCL_DXIL_CACHE", cache.path())
        .env_remove("XCL_TRANSLATED_CACHE")
        .output()
        .expect("spawn xcl shaders --precompile");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "precompile failed: {out:?}");
    assert!(stdout.contains("compiled 2, already cached 0, errors 0"), "{stdout}");
    assert!(stdout.contains("PS warmed at assumed sampler_base=0: 1"), "{stdout}");
    let warm_out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(["shaders", dir.path().to_str().unwrap(), "--precompile", "--windows", "1", "--split", "0"])
        .env("XCL_DIRECT_DXIL", "all")
        .env("XCL_DXIL_CACHE", cache.path())
        .env_remove("XCL_TRANSLATED_CACHE")
        .output()
        .expect("spawn warm xcl shaders --precompile");
    let warm_stdout = String::from_utf8_lossy(&warm_out.stdout);
    assert!(warm_out.status.success(), "warm precompile failed: {warm_out:?}");
    assert!(warm_stdout.contains("compiled 0, already cached 2, errors 0"), "{warm_stdout}");
    let v2 = cache.path().join("translated").join("v2");
    assert!(!find_files_named(&v2, "_Vertex_0_").is_empty(), "missing _Vertex_0_ in {}", v2.display());
    assert!(!find_files_named(&v2, "_Pixel_0_").is_empty(), "missing _Pixel_0_ in {}", v2.display());
    let child = spawn_child_check("precompile_warms_direct_vs_ps_and_hits_in_fresh_process", dir.path(), cache.path(), "all");
    let c_out = String::from_utf8_lossy(&child.stdout);
    print!("{c_out}");
    assert!(child.status.success(), "fresh child failed: {child:?}\n{c_out}");
    assert!(c_out.contains("fresh_child_timing: mode=direct"), "{c_out}");
}

#[cfg(windows)]
#[test]
fn precompile_warms_hlsl_static_samplers_and_reports_base_shift() {
    use xcl::gpu::recomp::{dxc, pso};
    if std::env::var_os("XCL_SHADERS_E2E_CHILD").is_some() {
        let dir = PathBuf::from(std::env::var_os("XCL_SHADERS_E2E_DIR").unwrap());
        let (vs_blob, ps_blob) = (std::fs::read(dir.join("vs.bin")).unwrap(), std::fs::read(dir.join("ps.bin")).unwrap());
        let t0 = std::time::Instant::now();
        let (vs_res, vs_missed) = dxc::cached_only(|| pso::console_to_dxil_in(&vs_blob, pso::Stage::Vertex, 0, false, 1));
        let (ps0_res, ps0_missed) = dxc::cached_only(|| pso::console_to_dxil_in(&ps_blob, pso::Stage::Pixel, 0, false, 1));
        let warm_ms = t0.elapsed().as_secs_f64() * 1e3;
        let (vs_c, ps0_c) = (vs_res.expect("warm HLSL VS hit"), ps0_res.expect("warm HLSL PS base=0 hit"));
        assert!(!vs_missed && !ps0_missed, "fresh process must hit VS and PS(base=0)");
        assert_eq!((vs_c.samplers.len(), ps0_c.samplers.len()), (1, 1));
        assert_eq!(dxc::compile_counts().1, 0, "warm hits must not invoke DXC");
        let (_ps1_probe, ps1_missed) = dxc::cached_only(|| pso::console_to_dxil_in(&ps_blob, pso::Stage::Pixel, 1, false, 1));
        assert!(ps1_missed, "paired PS(base=1) must miss base=0 entry");
        let ps1_c = pso::console_to_dxil_in(&ps_blob, pso::Stage::Pixel, 1, false, 1).expect("PS base=1 compile");
        assert_eq!(ps1_c.samplers.len(), 1);
        assert_ne!(ps1_c.dxil, ps0_c.dxil, "PS(base=1) DXIL must differ from PS(base=0)");
        match time_native_graphics_pso(&vs_c, &ps1_c).expect("HLSL graphics PSO creation failed") {
            Some(pso_ms) => println!("fresh_child_timing: mode=hlsl warm_translate_dxc_ms={warm_ms:.3} native_pso_ms={pso_ms:.3}"),
            None => {
                xcl::gpu::recomp::runner::skip_test("no D3D12 device");
                println!("fresh_child_timing: mode=hlsl warm_translate_dxc_ms={warm_ms:.3} native_pso_ms=skipped(no_gpu)");
            }
        }
        return;
    }
    if dxc::location().is_err() {
        xcl::gpu::recomp::runner::skip_test("no DXC");
        return;
    }
    let (dir, cache) = (TempDir::new("hlsl-gfx"), TempDir::new("hlsl-gfx-cache"));
    use xcl::gpu::dxil::container::{signature_part, SigPartElement};
    let ps_osg = signature_part(&[SigPartElement { stream: 0, name: "SV_Target".into(), index: 0, system_value: 64, comp_type: 3, register: 0, mask: 0xF, rw_mask: 0, min_precision: 0 }], &[]);
    // exp pos0 (TGT=12, DONE=1, EN=0xf) for VS and exp mrt0 (TGT=0, VM=1, DONE=1, EN=0xf) for PS.
    std::fs::write(dir.path().join("vs.bin"), xbox_shader(1, &[fssm(&static_sampler_code(0xF800_08CF), &[(4, 0)])])).unwrap();
    std::fs::write(dir.path().join("ps.bin"), xbox_shader_with_osg(0, &[fssm(&static_sampler_code(0xF800_180F), &[(5, 1)])], &ps_osg)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(["shaders", dir.path().to_str().unwrap(), "--precompile", "--windows", "1", "--split", "0"])
        .env("XCL_DIRECT_DXIL", "0")
        .env("XCL_DXIL_CACHE", cache.path())
        .env_remove("XCL_TRANSLATED_CACHE")
        .output()
        .expect("spawn xcl shaders --precompile");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "precompile failed: {out:?}");
    assert!(stdout.contains("compiled 2, already cached 0, errors 0"), "{stdout}");
    assert!(stdout.contains("PS warmed at assumed sampler_base=0: 1 (1 with static samplers); VS with static samplers shifting paired PS base: 1"), "{stdout}");
    let warm_out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(["shaders", dir.path().to_str().unwrap(), "--precompile", "--windows", "1", "--split", "0"])
        .env("XCL_DIRECT_DXIL", "0")
        .env("XCL_DXIL_CACHE", cache.path())
        .env_remove("XCL_TRANSLATED_CACHE")
        .output()
        .expect("spawn warm hlsl xcl shaders --precompile");
    let warm_stdout = String::from_utf8_lossy(&warm_out.stdout);
    assert!(warm_out.status.success(), "warm hlsl precompile failed: {warm_out:?}");
    assert!(warm_stdout.contains("compiled 0, already cached 2, errors 0"), "{warm_stdout}");
    let child = spawn_child_check("precompile_warms_hlsl_static_samplers_and_reports_base_shift", dir.path(), cache.path(), "0");
    let c_out = String::from_utf8_lossy(&child.stdout);
    print!("{c_out}");
    assert!(child.status.success(), "fresh child failed: {child:?}\n{c_out}");
    assert!(c_out.contains("fresh_child_timing: mode=hlsl"), "{c_out}");
}
