//! `xcl <command> --help|-h` prints that command's usage block and exits 0.
//!
//! Every block is the one `xcl --help` prints, so the two cannot drift apart.

use std::process::{Command, Output};

const COMMANDS: [&str; 7] = [
    "run",
    "inspect",
    "preflight",
    "disasm",
    "shaders",
    "rpf",
    "replay",
];

fn xcl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xcl"))
        .args(args)
        .env_remove("XCL_DATA_ROOT")
        .output()
        .expect("spawn xcl")
}

/// The command's block in the top-level help: its header line and indented description,
/// up to (not including) the next command line or the final `xcl --version`.
fn block_of(help: &str, command: &str) -> String {
    let prefix = format!("\n  xcl {command} ");
    let start = help
        .find(&prefix)
        .unwrap_or_else(|| panic!("no `{command}` block in xcl --help"))
        + 1;
    let rest = &help[start..];
    let end = rest[1..]
        .find("\n  xcl ")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn subcommand_help_prints_its_top_level_block_and_exits_zero() {
    let top = xcl(&["--help"]);
    assert_eq!(top.status.code(), Some(0), "{top:?}");
    let help = String::from_utf8(top.stdout).unwrap();

    for command in COMMANDS {
        let block = block_of(&help, command);
        for flag in ["--help", "-h"] {
            let out = xcl(&[command, flag]);
            assert_eq!(out.status.code(), Some(0), "xcl {command} {flag}: {out:?}");
            assert!(out.stderr.is_empty(), "xcl {command} {flag}: {out:?}");
            let stdout = String::from_utf8(out.stdout).unwrap();
            assert_eq!(stdout, format!("Usage:\n{block}\n"), "xcl {command} {flag}");
        }
    }
}
