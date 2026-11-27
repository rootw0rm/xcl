//! Linux `xcl replay`: the command is advertised in `xcl --help` but the module is
//! Windows-only. The front-end must say so instead of falling through to the top-level help.

#![cfg(not(windows))]

use std::process::{Command, Output};

const UNAVAILABLE: &str =
    "xcl replay: not available on Linux (Windows-only D3D12 replay; see `xcl --help`)\n";

fn xcl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(args)
        .env_remove("XCL_DATA_ROOT")
        .output()
        .expect("spawn xcl")
}

#[test]
fn replay_reports_that_linux_has_no_replay_instead_of_the_top_level_help() {
    let capture = std::env::temp_dir().join("xcl-replay-capture");
    let capture = capture.to_str().unwrap();
    // Every argument shape reaches the same refusal: the top-level help (3.5 KB with a usage
    // banner) must never be the answer, and `--bogus` is not silently ignored either.
    for args in [
        &["replay"][..],
        &["replay", capture][..],
        &["replay", capture, "--bogus"][..],
        &["replay", capture, "--timeout", "5"][..],
    ] {
        let out = xcl(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(
            out.stdout.is_empty(),
            "top-level help printed for {args:?}: {out:?}"
        );
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            UNAVAILABLE,
            "{args:?}"
        );
    }
}

#[test]
fn linux_help_block_says_replay_is_not_built_here() {
    let top = xcl(&["--help"]);
    assert_eq!(top.status.code(), Some(0), "{top:?}");
    let help = String::from_utf8(top.stdout).unwrap();
    let start = help
        .find("\n  xcl replay ")
        .expect("no replay block in xcl --help")
        + 1;
    let block = &help[start..];
    let end = block[1..]
        .find("\n  xcl ")
        .map(|i| i + 1)
        .unwrap_or(block.len());
    let block = &block[..end];
    assert!(
        block.contains("Not built on Linux"),
        "replay block does not name the Linux gap:\n{block}"
    );

    // The per-command block is the same text (single source of truth).
    let own = xcl(&["replay", "--help"]);
    assert_eq!(own.status.code(), Some(0), "{own:?}");
    assert_eq!(
        String::from_utf8(own.stdout).unwrap(),
        format!("Usage:\n{block}\n")
    );
}
