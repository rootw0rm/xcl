//! Linux Vulkan backend selector (Phase D step 17a).
//!
//! `Backend::{Null, Vulkan}` + `select()` in front of the null D3D12.X device.
//! `Null` stays the headless/CI default; an explicit `XCL_VULKAN_BACKEND`
//! override requests the real Vulkan path, which still falls back to `Null`
//! with a logged reason when no usable device exists (never a panic).
//!
//! D3D12.X creation queries [`select()`] but still returns the existing null
//! device in this slice (no vtable/behavior change for the title). The Vulkan
//! request path consumes `vulkan_runner::device_lane_path()` (the step-16
//! lane decision), so the selection agrees with the runner; the probe reuses
//! the runner's `pick_device`-style query (one instance per `select()` call)
//! instead of a second throwaway instance. No `winit`, no swapchain, no
//! window, no graphics pipeline, no new deps.

use crate::gpu::recomp::vulkan_runner::{self, LanePath};

/// Env override selecting the backend (`"vulkan"` requests real Vulkan,
/// anything else — including unset — stays `Null`).
pub const ENV_OVERRIDE: &str = "XCL_VULKAN_BACKEND";

/// Which graphics backend the D3D12.X device should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Headless null device (default; always available).
    Null,
    /// Real Vulkan device (falls back to `Null` when unusable).
    Vulkan,
}

/// Result of [`select()`]: the backend to use, the runner lane decision when
/// Vulkan was selected, and a human-readable reason (logged, never panics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub backend: Backend,
    pub lane: Option<LanePath>,
    pub reason: String,
}

/// Parse an [`ENV_OVERRIDE`] value (pure, for tests). `None` (unset) and any
/// non-`"vulkan"` value stay `Null`; `"vulkan"` (ASCII case-insensitive,
/// trimmed) requests `Vulkan`.
pub fn parse_override(raw: Option<&str>) -> Backend {
    match raw.map(str::trim).map(|s| s.to_ascii_lowercase()) {
        Some(s) if s == "vulkan" => Backend::Vulkan,
        _ => Backend::Null,
    }
}

/// Backend requested via the process environment (pure parse over
/// [`ENV_OVERRIDE`]; unset/unknown stays `Null`).
pub fn requested_backend() -> Backend {
    parse_override(std::env::var(ENV_OVERRIDE).ok().as_deref())
}

/// Select with an injected device probe (pure, for tests): `Null` never calls
/// `probe`; `Vulkan` calls it once and falls back to `Null` on error.
pub fn select_with(
    request: Backend,
    probe: impl FnOnce() -> Result<LanePath, String>,
) -> Selection {
    match request {
        Backend::Null => Selection {
            backend: Backend::Null,
            lane: None,
            reason: "null backend (default/headless)".into(),
        },
        Backend::Vulkan => match probe() {
            Ok(lane) => {
                let reason = match lane {
                    LanePath::Native64 => "vulkan backend (device subgroupSize 64 -> native-64)",
                    LanePath::Split32 => "vulkan backend (device split-32 lane path)",
                };
                eprintln!("vulkan backend: {reason}");
                Selection {
                    backend: Backend::Vulkan,
                    lane: Some(lane),
                    reason: reason.into(),
                }
            }
            Err(e) => {
                eprintln!("vulkan backend unavailable ({e}); falling back to null device");
                Selection {
                    backend: Backend::Null,
                    lane: None,
                    reason: format!("vulkan requested, fallback to null: {e}"),
                }
            }
        },
    }
}

/// Select the backend: read [`ENV_OVERRIDE`], probe the real device only when
/// Vulkan was requested (one `create_instance` via `device_lane_path`, the
/// runner's `pick_device`-style query — no second throwaway instance).
pub fn select() -> Selection {
    select_with(requested_backend(), vulkan_runner::device_lane_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_unset_or_unknown_stays_null() {
        assert_eq!(parse_override(None), Backend::Null);
        assert_eq!(parse_override(Some("")), Backend::Null);
        assert_eq!(parse_override(Some("null")), Backend::Null);
        assert_eq!(parse_override(Some("auto")), Backend::Null);
        assert_eq!(parse_override(Some("  VoOdOo  ")), Backend::Null);
    }

    #[test]
    fn override_vulkan_is_case_insensitive() {
        assert_eq!(parse_override(Some("vulkan")), Backend::Vulkan);
        assert_eq!(parse_override(Some("VULKAN")), Backend::Vulkan);
        assert_eq!(parse_override(Some("  Vulkan  ")), Backend::Vulkan);
    }

    #[test]
    fn null_request_never_probes_device() {
        let sel = select_with(Backend::Null, || panic!("null must not probe"));
        assert_eq!(sel.backend, Backend::Null);
        assert_eq!(sel.lane, None);
    }

    #[test]
    fn vulkan_request_with_lane_selects_vulkan() {
        for lane in [LanePath::Native64, LanePath::Split32] {
            let sel = select_with(Backend::Vulkan, || Ok(lane));
            assert_eq!(sel.backend, Backend::Vulkan);
            assert_eq!(sel.lane, Some(lane));
        }
    }

    #[test]
    fn vulkan_request_without_device_falls_back_to_null() {
        let sel = select_with(Backend::Vulkan, || Err("no device: test".into()));
        assert_eq!(sel.backend, Backend::Null);
        assert_eq!(sel.lane, None);
        assert!(sel.reason.contains("no device"), "{}", sel.reason);
    }

    #[test]
    fn selection_lane_agrees_with_runner() {
        // Same 64-vs-split-32 decision the runner makes (`runner::split_wave64`
        // mirror): whatever subgroupSize the device reports maps through the
        // shared `lane_path_for`.
        for size in [64u32, 32, 16, 0] {
            let lane = vulkan_runner::lane_path_for(size);
            let sel = select_with(Backend::Vulkan, || Ok(lane));
            assert_eq!(sel.lane, Some(vulkan_runner::lane_path_for(size)));
        }
    }

    #[test]
    fn env_null_or_unset_selects_null_without_device() {
        // Only the Null paths touch the real environment (deterministic with
        // or without a GPU: neither probes the device).
        let key = ENV_OVERRIDE;
        let saved = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, "null");
        }
        assert_eq!(select().backend, Backend::Null);
        unsafe {
            std::env::remove_var(key);
        }
        assert_eq!(select().backend, Backend::Null);
        if let Some(v) = saved {
            unsafe {
                std::env::set_var(key, v);
            }
        }
    }
}
