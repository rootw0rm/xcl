//! Linux `preflight` surface: the `xcl preflight` env checks without starting a game.
//!
//! The Windows twin is `preflight.rs` (D3D12/DXC/XGameRuntime probes through the
//! `windows` crate); this twin reports what a Linux run needs instead: the console
//! Zen 2 CPU features `xcl run` refuses without, a Vulkan device behind the null
//! default, and notes where a Windows-only piece (DXC, the D3D12 adapter choice,
//! the PC XGameRuntime) does not apply. `Level`/`Check`, their serde shape and
//! `print` match the Windows twin so `report.json` stays compatible.

use serde::{Deserialize, Serialize};

/// How much a missing piece costs the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    /// runs, with a known limit
    Note,
    /// part of the game is missing (no hardware rendering)
    Warning,
    /// the game cannot run like this
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub level: Level,
    pub what: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn check(level: Level, what: impl Into<String>, fix: Option<&str>) -> Check {
    Check { level, what: what.into(), fix: fix.map(str::to_string) }
}

/// Exit code for the `preflight` CLI: 1 when any check is an `Error`, else 0.
/// (Same rule as upstream #1064's inline `any Error` arm, factored so the CLI
/// mapping is unit-testable; `main.rs` calls this.)
pub fn exit_code(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| matches!(c.level, Level::Error)) { 1 } else { 0 }
}

/// The checks as console lines: OK ones first as `  label`, the rest as WARNING / ERROR with
/// what to do. (Same layout as the Windows twin.)
pub fn print(checks: &[Check]) {
    for c in checks {
        let tag = match c.level {
            Level::Ok => "  ",
            Level::Note => "  NOTE: ",
            Level::Warning => "  WARNING: ",
            Level::Error => "  ERROR: ",
        };
        println!("{tag}{}", c.what);
        if let Some(fix) = &c.fix {
            println!("           → {fix}");
        }
    }
}

/// Build the check list from injected probe results (pure, for tests): `request`
/// is the backend the process asked for, `setting` the `d3d12_adapter` value
/// `xcl preflight [gpu]` was given, `gpu` the Vulkan subgroup probe, and
/// `missing_cpu` the console Zen 2 features this CPU lacks.
fn checks_for(
    request: crate::hle::vulkan_linux::Backend,
    setting: &str,
    gpu: Result<u32, String>,
    missing_cpu: &[&str],
) -> Vec<Check> {
    use crate::hle::vulkan_linux::Backend;
    let mut v = Vec::new();
    if !missing_cpu.is_empty() {
        v.push(check(
            Level::Error,
            format!(
                "this CPU lacks {}: Xbox Series X|S games need the console's Zen 2 features (xcl run refuses without them)",
                missing_cpu.join(", ")
            ),
            Some("run on a Zen-2-class x86-64 CPU, or set XCL_SKIP_CPU_CHECK=1 to run anyway (as xcl run does)"),
        ));
    }
    match gpu {
        Ok(subgroup) => {
            let native = crate::gpu::recomp::vulkan_runner::lane_path_for(subgroup)
                == crate::gpu::recomp::vulkan_runner::LanePath::Native64;
            v.push(check(
                Level::Ok,
                format!(
                    "Vulkan device present (subgroupSize {subgroup}){}",
                    if request == Backend::Vulkan {
                        ": the Vulkan backend will render this run"
                    } else {
                        "; the null backend stays default (XCL_VULKAN_BACKEND=vulkan to render)"
                    }
                ),
                None,
            ));
            if !native {
                v.push(check(Level::Note, format!("subgroupSize {subgroup}: the console's 64-lane programs run as two 32-lane halves (slower; vertex / pixel shaders that read lanes 32–63 can differ)"), None));
            }
        }
        Err(e) => {
            if request == Backend::Vulkan {
                v.push(check(Level::Error, format!("XCL_VULKAN_BACKEND=vulkan but no Vulkan device: {e}"), Some("install a Vulkan driver (mesa-vulkan-drivers / NVIDIA) and retry")));
            } else {
                v.push(check(Level::Warning, format!("no Vulkan device ({e}): the game runs on the null device and draws nothing"), Some("install a Vulkan driver (mesa-vulkan-drivers / NVIDIA), or set XCL_VULKAN_BACKEND=vulkan once one is present")));
            }
        }
    }
    if setting != "auto" {
        v.push(check(Level::Note, format!("d3d12_adapter {setting:?} ignored: D3D12 adapter selection is Windows-only; Linux renders through Vulkan"), None));
    }
    v.push(check(Level::Note, "DXC/D3D12 checks are Windows-only: Linux compiles shaders to SPIR-V, no dxcompiler.dll needed", None));
    v.push(check(Level::Note, "no System32\\xgameruntime.dll: dialogs, text entry, game invites and game streaming use XCL's own task queues, async and networking", None));
    v
}

/// Check this PC for a run. `setting` is the `d3d12_adapter` value (default
/// `auto`); D3D12 adapter selection is Windows-only, so a non-`auto` value is
/// reported back as a note rather than applied.
pub fn run(setting: &str) -> Vec<Check> {
    checks_for(
        crate::hle::vulkan_linux::requested_backend(),
        setting,
        crate::gpu::recomp::vulkan_runner::device_subgroup_size(),
        &crate::launcher::missing_console_cpu_features(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hle::vulkan_linux::Backend;

    fn lvl(level: Level) -> Check {
        check(level, "x", if level == Level::Ok { None } else { Some("fix it") })
    }

    #[test]
    fn exit_code_is_1_only_on_error() {
        assert_eq!(exit_code(&[lvl(Level::Ok), lvl(Level::Note), lvl(Level::Warning)]), 0);
        assert_eq!(exit_code(&[lvl(Level::Ok), lvl(Level::Error)]), 1);
        assert_eq!(exit_code(&[]), 0);
    }

    #[test]
    fn levels_round_trip_through_json_like_the_report() {
        let c = vec![lvl(Level::Ok), lvl(Level::Error)];
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"level\":\"error\""), "{json}");
        assert_eq!(serde_json::from_str::<Vec<Check>>(&json).unwrap(), c);
    }

    #[test]
    fn requested_vulkan_without_a_device_is_an_error() {
        let c = checks_for(Backend::Vulkan, "auto", Err("no Vulkan: Entry::load: x".into()), &[]);
        assert!(c.iter().any(|c| c.level == Level::Error), "{c:?}");
        assert_eq!(exit_code(&c), 1);
    }

    #[test]
    fn default_backend_without_a_device_warns_but_stays_zero() {
        // The null/headless path still runs (CI depends on it), so a missing
        // Vulkan device is a Warning, not an Error, unless Vulkan was asked for.
        let c = checks_for(Backend::Null, "auto", Err("no Vulkan: Entry::load: x".into()), &[]);
        assert!(c.iter().any(|c| c.level == Level::Warning), "{c:?}");
        assert!(!c.iter().any(|c| c.level == Level::Error), "{c:?}");
        assert_eq!(exit_code(&c), 0);
    }

    #[test]
    fn present_device_is_ok_with_a_split_lane_note() {
        let c = checks_for(Backend::Vulkan, "auto", Ok(64), &[]);
        assert!(c.iter().any(|c| c.level == Level::Ok), "{c:?}");
        assert!(!c.iter().any(|c| c.level == Level::Error), "{c:?}");
        assert_eq!(exit_code(&c), 0);
        let c32 = checks_for(Backend::Null, "auto", Ok(32), &[]);
        assert!(c32.iter().any(|c| c.level == Level::Note && c.what.contains("two 32-lane halves")), "{c32:?}");
    }

    #[test]
    fn missing_cpu_features_are_an_error() {
        let c = checks_for(Backend::Null, "auto", Ok(64), &["avx2", "bmi2"]);
        assert!(c.iter().any(|c| c.level == Level::Error && c.what.contains("avx2")), "{c:?}");
        assert_eq!(exit_code(&c), 1);
    }

    #[test]
    fn non_auto_adapter_setting_is_reported_not_applied() {
        let c = checks_for(Backend::Null, "auto", Ok(64), &[]);
        assert!(!c.iter().any(|c| c.what.contains("d3d12_adapter")), "{c:?}");
        let c = checks_for(Backend::Null, "nvidia", Ok(64), &[]);
        assert!(c.iter().any(|c| c.level == Level::Note && c.what.contains("d3d12_adapter")), "{c:?}");
        assert_eq!(exit_code(&c), 0);
    }

    #[test]
    fn windows_only_pieces_are_notes_not_errors() {
        let c = checks_for(Backend::Null, "auto", Ok(64), &[]);
        assert!(c.iter().any(|c| c.level == Level::Note && c.what.contains("SPIR-V")), "{c:?}");
        assert!(c.iter().any(|c| c.level == Level::Note && c.what.contains("xgameruntime")), "{c:?}");
    }

    #[test]
    fn live_run_always_carries_the_platform_notes() {
        // Host-dependent levels (CPU/Vulkan) are covered above via `checks_for`;
        // the live probe must at least return the unconditional platform notes.
        let c = run("auto");
        assert!(!c.is_empty(), "{c:?}");
        assert!(c.iter().any(|c| c.what.contains("SPIR-V")), "{c:?}");
        assert_eq!(exit_code(&c), if c.iter().any(|c| c.level == Level::Error) { 1 } else { 0 });
    }

    #[test]
    fn print_does_not_panic() {
        print(&[lvl(Level::Ok), lvl(Level::Note), lvl(Level::Warning), lvl(Level::Error)]);
    }
}
