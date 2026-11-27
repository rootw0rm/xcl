//! End-to-end tests for `xcl preflight` argument handling.
//!
//! `xcl preflight [gpu]` takes one optional `d3d12_adapter` value; like `inspect`/`disasm`,
//! anything else must be a usage error instead of being silently dropped or probed as the
//! setting.

use std::process::{Command, Output};

fn xcl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(args)
        .env_remove("XCL_DATA_ROOT")
        .output()
        .expect("spawn xcl")
}

#[test]
fn preflight_rejects_unknown_options_and_stray_arguments() {
    let flag = xcl(&["preflight", "--bogus"]);
    assert_eq!(flag.status.code(), Some(2), "{flag:?}");
    assert!(flag.stdout.is_empty(), "checks ran despite the unknown option: {flag:?}");
    assert_eq!(
        String::from_utf8(flag.stderr).unwrap(),
        "xcl preflight: unknown option: --bogus; see `xcl --help`\n"
    );

    let stray = xcl(&["preflight", "auto", "extra"]);
    assert_eq!(stray.status.code(), Some(2), "{stray:?}");
    assert!(stray.stdout.is_empty(), "checks ran despite the stray argument: {stray:?}");
    assert_eq!(
        String::from_utf8(stray.stderr).unwrap(),
        "xcl preflight: unexpected argument: extra; see `xcl --help`\n"
    );

    let flag_after_value = xcl(&["preflight", "auto", "--json"]);
    assert_eq!(flag_after_value.status.code(), Some(2), "{flag_after_value:?}");
    assert!(flag_after_value.stdout.is_empty(), "{flag_after_value:?}");
    assert_eq!(
        String::from_utf8(flag_after_value.stderr).unwrap(),
        "xcl preflight: unknown option: --json; see `xcl --help`\n"
    );
}

#[test]
fn preflight_still_accepts_the_optional_adapter_value() {
    // No value, `auto`, or a real adapter name all probe; the note names a non-auto value.
    for args in [&["preflight"][..], &["preflight", "auto"][..], &["preflight", "nvidia"][..]] {
        let out = xcl(args);
        assert_ne!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        // The platform note is unconditional, so a live probe always prints it.
        assert!(stdout.contains("no dxcompiler.dll needed"), "{args:?}: {stdout}");
    }
    let nvidia = xcl(&["preflight", "nvidia"]);
    let stdout = String::from_utf8(nvidia.stdout).unwrap();
    assert!(stdout.contains("d3d12_adapter \"nvidia\" ignored"), "{stdout}");
}
