//! `xcl <command> --help|-h` prints that command's usage block and exits 0.
//!
//! Every block is the one `xcl --help` prints, so the two cannot drift apart.
//! A command with no operand names the missing operand instead of dumping the whole help.

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

/// The operand each command's usage line requires.
const MISSING_OPERAND: [(&str, &str); 5] = [
    ("run", "<game folder | file.exe>"),
    ("inspect", "<file.exe|file.dll>"),
    ("disasm", "<file>"),
    ("shaders", "<file | folder | package.rpf>"),
    ("rpf", "<package.rpf>"),
];

fn missing_operand_message(command: &str, operand: &str) -> String {
    format!("xcl {command}: missing {operand}; see xcl {command} --help\n")
}

#[test]
fn missing_operand_is_named_on_stderr_instead_of_printing_the_help() {
    for (command, operand) in MISSING_OPERAND {
        let out = xcl(&[command]);
        assert_eq!(out.status.code(), Some(2), "xcl {command}: {out:?}");
        assert!(
            out.stdout.is_empty(),
            "help printed for xcl {command}: {out:?}"
        );
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            missing_operand_message(command, operand),
            "xcl {command}"
        );
    }
}

#[test]
fn run_separator_without_a_target_names_the_missing_operand() {
    for args in [&["run", "--"][..], &["run", "--", "foo"][..]] {
        let out = xcl(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "help printed for {args:?}: {out:?}");
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            missing_operand_message("run", "<game folder | file.exe>"),
            "{args:?}"
        );
    }
}

#[test]
fn command_separator_names_the_operand_instead_of_taking_it_literally() {
    for (command, operand) in [
        ("inspect", "<file.exe|file.dll>"),
        ("disasm", "<file>"),
        ("shaders", "<file | folder | package.rpf>"),
        ("rpf", "<package.rpf>"),
    ] {
        let out = xcl(&[command, "--"]);
        assert_eq!(out.status.code(), Some(2), "xcl {command} --: {out:?}");
        assert!(out.stdout.is_empty(), "xcl {command} --: {out:?}");
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            missing_operand_message(command, operand),
            "xcl {command} --"
        );

        // After the separator the next argument is the operand, not another option.
        let out = xcl(&[command, "--", "/nonexistent/xcl-no-such-input"]);
        assert_eq!(out.status.code(), Some(2), "xcl {command} -- <path>: {out:?}");
        assert!(out.stdout.is_empty(), "xcl {command} -- <path>: {out:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains("/nonexistent/xcl-no-such-input"),
            "xcl {command} -- <path> did not read the operand: {stderr}"
        );
        assert!(
            !stderr.contains("unexpected argument"),
            "xcl {command} -- <path> rejected its operand: {stderr}"
        );
    }
}

#[test]
fn help_and_error_controls_are_unchanged() {
    let help = xcl(&["--help"]);
    assert_eq!(help.status.code(), Some(0), "{help:?}");
    assert!(help.stderr.is_empty(), "{help:?}");

    // An unknown command keeps the full-help answer.
    let unknown = xcl(&["bogus"]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
    assert!(unknown.stderr.is_empty(), "{unknown:?}");
    assert_eq!(unknown.stdout, help.stdout, "{unknown:?}");

    // A given target keeps its read error.
    let unreadable = xcl(&["run", "/nonexistent/xcl-no-such-game"]);
    assert_eq!(unreadable.status.code(), Some(2), "{unreadable:?}");
    assert!(unreadable.stdout.is_empty(), "{unreadable:?}");
    assert!(!unreadable.stderr.is_empty(), "{unreadable:?}");
}

#[cfg(not(windows))]
#[test]
fn bare_invocation_prints_the_help() {
    let bare = xcl(&[]);
    let help = xcl(&["--help"]);
    assert_eq!(bare.status.code(), Some(0), "{bare:?}");
    assert!(bare.stderr.is_empty(), "{bare:?}");
    assert_eq!(bare.stdout, help.stdout, "{bare:?}");
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
