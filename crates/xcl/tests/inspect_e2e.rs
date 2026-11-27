//! End-to-end integration tests for `xcl inspect`.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn inspect_missing_argument_names_the_operand_and_exits_two() {
    let out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .arg("inspect")
        .output()
        .expect("spawn xcl inspect");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "xcl inspect: missing <file.exe|file.dll>; see xcl inspect --help\n"
    );
}

#[test]
fn inspect_unknown_option_and_stray_argument_exit_two() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let file = std::env::temp_dir().join(format!("xcl-inspect-args-{}-{nonce}.exe", std::process::id()));
    std::fs::write(&file, b"NOT_A_VALID_PE_IMAGE").unwrap();

    for (bad, expected) in [
        ("--bogus", "xcl inspect: unknown option: --bogus; see `xcl --help`"),
        ("extra", "xcl inspect: unexpected argument: extra; see `xcl --help`"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_xcl"))
            .arg("inspect")
            .arg(&file)
            .arg(bad)
            .output()
            .expect("spawn xcl inspect with a bad argument");
        assert_eq!(out.status.code(), Some(2), "{bad}: {out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(expected), "{bad}: {stderr}");
        assert!(!stderr.contains("invalid signature"), "{bad}: analysis ran despite the bad argument: {stderr}");
        assert!(out.stdout.is_empty(), "{bad}: analysis ran despite the bad argument");
    }
    std::fs::remove_file(&file).unwrap();
}

#[test]
fn inspect_nonexistent_file_exits_two_with_error() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let missing = std::env::temp_dir().join(format!("xcl-inspect-missing-{nonce}.exe"));
    let out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .arg("inspect")
        .arg(&missing)
        .output()
        .expect("spawn xcl inspect missing");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("xcl inspect: cannot read"));
}

#[test]
fn inspect_non_pe_file_exits_three_with_parse_error() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_file = std::env::temp_dir().join(format!("xcl-inspect-bad-{nonce}.bin"));
    std::fs::write(&temp_file, b"NOT_A_VALID_PE_IMAGE").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_xcl"))
        .arg("inspect")
        .arg(&temp_file)
        .output()
        .expect("spawn xcl inspect bad");
    std::fs::remove_file(&temp_file).unwrap();

    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("xcl inspect:"));
    assert!(stderr.contains("already unpacked files"));
}

/// Minimal PE32+ console image: x64, subsystem 3 (CUI), one named import
/// (`kernel32.dll!ExitProcess`) and one `.pdata` entry.
///
/// `CARGO_BIN_EXE_xcl` is a real PE on Windows but an ELF on Linux, so the Linux run
/// needs a PE of its own to cover `xcl inspect`'s success path.
#[cfg(not(windows))]
fn synthetic_console_pe() -> Vec<u8> {
    const OPT: usize = 0x58;
    const SEC: usize = OPT + 240;
    const RAW: usize = 0x400;
    const RAW_LEN: usize = 0x200;
    const RVA: usize = 0x1000;

    let mut data = vec![0u8; RAW + RAW_LEN];
    data[..2].copy_from_slice(b"MZ");
    data[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    data[0x40..0x44].copy_from_slice(b"PE\0\0");
    data[0x44..0x46].copy_from_slice(&0x8664u16.to_le_bytes()); // machine x64
    data[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // one section
    data[0x54..0x56].copy_from_slice(&240u16.to_le_bytes()); // SizeOfOptionalHeader
    data[OPT..OPT + 2].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+
    data[OPT + 16..OPT + 20].copy_from_slice(&(RVA as u32).to_le_bytes()); // entry point
    data[OPT + 24..OPT + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes()); // ImageBase
    data[OPT + 32..OPT + 36].copy_from_slice(&0x1000u32.to_le_bytes()); // SectionAlignment
    data[OPT + 36..OPT + 40].copy_from_slice(&0x200u32.to_le_bytes()); // FileAlignment
    data[OPT + 56..OPT + 60].copy_from_slice(&0x2000u32.to_le_bytes()); // SizeOfImage
    data[OPT + 60..OPT + 64].copy_from_slice(&(RAW as u32).to_le_bytes()); // SizeOfHeaders
    data[OPT + 68..OPT + 70].copy_from_slice(&3u16.to_le_bytes()); // subsystem: console
    data[OPT + 72..OPT + 80].copy_from_slice(&0x10_0000u64.to_le_bytes()); // stack reserve
    data[OPT + 108..OPT + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
    data[SEC..SEC + 5].copy_from_slice(b".text");
    data[SEC + 8..SEC + 12].copy_from_slice(&(RAW_LEN as u32).to_le_bytes()); // VirtualSize
    data[SEC + 12..SEC + 16].copy_from_slice(&(RVA as u32).to_le_bytes()); // VirtualAddress
    data[SEC + 16..SEC + 20].copy_from_slice(&(RAW_LEN as u32).to_le_bytes()); // SizeOfRawData
    data[SEC + 20..SEC + 24].copy_from_slice(&(RAW as u32).to_le_bytes()); // PointerToRawData
    data[SEC + 36..SEC + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes()); // code|execute|read

    // Import directory: one descriptor (INT 0x1050, name 0x1080, IAT 0x1040) + terminator.
    let desc = RAW;
    data[desc..desc + 4].copy_from_slice(&0x1050u32.to_le_bytes());
    data[desc + 12..desc + 16].copy_from_slice(&0x1080u32.to_le_bytes());
    data[desc + 16..desc + 20].copy_from_slice(&0x1040u32.to_le_bytes());
    let iat = RAW + 0x40;
    data[iat..iat + 8].copy_from_slice(&0x1090u64.to_le_bytes());
    let int = RAW + 0x50;
    data[int..int + 8].copy_from_slice(&0x1090u64.to_le_bytes());
    let dll = RAW + 0x80;
    data[dll..dll + 12].copy_from_slice(b"kernel32.dll");
    let hint_name = RAW + 0x90;
    data[hint_name + 2..hint_name + 2 + 11].copy_from_slice(b"ExitProcess");
    // One RUNTIME_FUNCTION (12 bytes) = one .pdata entry.
    let pdata = RAW + 0x100;
    data[pdata..pdata + 4].copy_from_slice(&(RVA as u32).to_le_bytes());
    data[pdata + 4..pdata + 8].copy_from_slice(&(RVA as u32 + 0x10).to_le_bytes());
    data[pdata + 8..pdata + 12].copy_from_slice(&(RVA as u32 + 0xA0).to_le_bytes());

    let dir = |i: usize| OPT + 112 + i * 8;
    data[dir(1)..dir(1) + 4].copy_from_slice(&(RVA as u32).to_le_bytes()); // import RVA
    data[dir(1) + 4..dir(1) + 8].copy_from_slice(&40u32.to_le_bytes()); // two descriptors
    data[dir(3)..dir(3) + 4].copy_from_slice(&(RVA as u32 + 0x100).to_le_bytes()); // .pdata RVA
    data[dir(3) + 4..dir(3) + 8].copy_from_slice(&12u32.to_le_bytes());
    data
}

#[test]
fn inspect_valid_pe_in_text_and_json_modes() {
    let xcl_exe = env!("CARGO_BIN_EXE_xcl");

    #[cfg(windows)]
    let (target, fixture_dir): (std::path::PathBuf, Option<std::path::PathBuf>) =
        (xcl_exe.into(), None);
    #[cfg(not(windows))]
    let (target, fixture_dir) = {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("xcl-inspect-pe-{}-{nonce}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("console.exe");
        std::fs::write(&file, synthetic_console_pe()).unwrap();
        (file, Some(dir))
    };

    // Text output mode
    let out_text = Command::new(xcl_exe)
        .arg("inspect")
        .arg(&target)
        .output()
        .expect("spawn xcl inspect text");
    assert_eq!(out_text.status.code(), Some(0));
    let stdout_text = String::from_utf8(out_text.stdout).unwrap();
    assert!(stdout_text.contains("File:"));
    assert!(stdout_text.contains("Type:          EXE x64, subsystem"));
    assert!(stdout_text.contains("ImageBase:"));
    assert!(stdout_text.contains("Imports:"));

    // JSON output mode
    let out_json = Command::new(xcl_exe)
        .arg("inspect")
        .arg(&target)
        .arg("--json")
        .output()
        .expect("spawn xcl inspect json");
    assert_eq!(out_json.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_slice(&out_json.stdout).expect("parse inspect json output");
    assert!(json["file"].as_str().is_some());
    assert_eq!(json["machine"], "0x8664");
    assert_eq!(json["is_dll"], false);
    assert!(json["imports"].is_array());
    assert!(json["size_of_image"].as_str().unwrap().starts_with("0x"));
    assert_eq!(json["subsystem"], 3);
    assert!(json["exception_entries"].as_u64().unwrap_or(0) > 0);
    #[cfg(not(windows))]
    assert_eq!(json["imports"].as_array().unwrap().len(), 1, "{json}");

    if let Some(dir) = fixture_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
}
