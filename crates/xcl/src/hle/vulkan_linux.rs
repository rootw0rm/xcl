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

/// Cached [`select()`]: the D3D12.X device is created repeatedly in one title
/// run, and each `select()` with `XCL_VULKAN_BACKEND=vulkan` would otherwise
/// create + destroy a throwaway Vulkan instance per call (step-17a review
/// follow-up). The device set cannot change mid-process, so cache the first
/// selection. Tests use [`select_with`]/[`select`] directly; only the hot
/// device-creation path uses the cache.
pub fn select_cached() -> &'static Selection {
    static CACHED: std::sync::OnceLock<Selection> = std::sync::OnceLock::new();
    CACHED.get_or_init(select)
}

/// A real Vulkan device behind a console D3D12.X device object (step 17,
/// device slice): the `ash` instance + logical device with one graphics queue,
/// picked through the runner's shared graphics query (no second throwaway
/// instance beyond the one `select()` already probed — the set cannot change
/// mid-process, so this is created once and intentionally leaked like the
/// null device objects).
pub struct VulkanState {
    /// Adapter name (`pick_graphics_device`), for logs.
    pub name: String,
    /// Compute lane path from the device `subgroupSize` (step-16 decision).
    pub lane: LanePath,
    /// Raw `subgroupSize` the lane was derived from (self-consistency gate).
    pub subgroup_size: u32,
    /// Graphics queue family index (the single queue this device owns).
    pub queue_family: u32,
    /// Whether swapchain presentation is available (instance WSI extensions
    /// plus the `VK_KHR_swapchain` device extension; false stays headless).
    pub wsi: bool,
    /// Owns the loader lifetime: must outlive `device` (never dropped —
    /// the state lives in a process singleton).
    #[allow(dead_code)]
    instance: ash::Instance,
    /// Owns `libvulkan.so.1` itself: `Instance`/`Device` hold bare function
    /// pointers into it, so dropping the `Entry` would `dlclose` the loader
    /// (and its ICDs) under the live device.
    #[allow(dead_code)]
    entry: ash::Entry,
    device: ash::Device,
}

/// Acquire the process Vulkan device (`None` when unavailable — missing
/// loader, no device, no graphics queue — never a panic). Cached: one
/// instance + one logical device per process, whatever the title creates.
pub fn acquire() -> Option<&'static VulkanState> {
    static STATE: std::sync::OnceLock<Result<VulkanState, String>> = std::sync::OnceLock::new();
    STATE.get_or_init(create_state).as_ref().ok()
}

/// The single queue of [`acquire()`] (cheap `get_device_queue` wrapper for
/// future submit paths; the handle stays valid while the state lives).
#[allow(dead_code)]
pub fn queue_of(state: &VulkanState) -> ash::vk::Queue {
    unsafe { state.device.get_device_queue(state.queue_family, 0) }
}

fn create_state() -> Result<VulkanState, String> {
    unsafe {
        let entry = ash::Entry::load().map_err(|e| format!("no Vulkan: Entry::load: {e}"))?;
        let app = std::ffi::CString::new("xcl").unwrap();
        let app_info = ash::vk::ApplicationInfo::default()
            .application_name(&app)
            .api_version(ash::vk::make_api_version(0, 1, 3, 0));
        // WSI extensions are enabled unconditionally (never probed with the
        // racy global `enumerate_instance_extension_properties` query — see
        // `vulkan_present`): absence fails closed at `create_instance` and
        // retries headless below.
        let surface_fn = std::ffi::CString::new("VK_KHR_surface").unwrap();
        let xlib_fn = std::ffi::CString::new("VK_KHR_xlib_surface").unwrap();
        let with_wsi = [surface_fn.as_ptr(), xlib_fn.as_ptr()];
        let bare_info = ash::vk::InstanceCreateInfo::default().application_info(&app_info);
        let wsi_info = ash::vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&with_wsi);
        let (instance, wsi) = match entry.create_instance(&wsi_info, None) {
            Ok(i) => (i, true),
            Err(ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT) => (
                entry
                    .create_instance(&bare_info, None)
                    .map_err(|e| format!("no Vulkan: create_instance: {e}"))?,
                false,
            ),
            Err(e) => return Err(format!("no Vulkan: create_instance: {e}")),
        };
        let picked = vulkan_runner::pick_graphics_device(&instance).map_err(|e| {
            instance.destroy_instance(None);
            e
        });
        let (phys, qfi, name, subgroup) = match picked {
            Ok(v) => v,
            Err(e) => {
                instance.destroy_instance(None);
                return Err(e);
            }
        };
        let lane = vulkan_runner::lane_path_for(subgroup);
        // Logical device with one graphics queue; the swapchain extension
        // rides along only when the instance has WSI, and its absence fails
        // closed to a headless device (never to no device at all).
        let prio = [1.0f32];
        let queue_info = ash::vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&prio);
        let swap_fn = std::ffi::CString::new("VK_KHR_swapchain").unwrap();
        let swap_ext = [swap_fn.as_ptr()];
        let mk_dev = |with_swap: bool| {
            let mut info = ash::vk::DeviceCreateInfo::default()
                .queue_create_infos(std::slice::from_ref(&queue_info));
            if with_swap {
                info = info.enabled_extension_names(&swap_ext);
            }
            instance.create_device(phys, &info, None)
        };
        let (device, wsi) = match mk_dev(wsi) {
            Ok(d) => (d, wsi),
            Err(ash::vk::Result::ERROR_EXTENSION_NOT_PRESENT) if wsi => {
                (mk_dev(false).map_err(|e| format!("create_device: {e}"))?, false)
            }
            Err(e) => {
                instance.destroy_instance(None);
                return Err(format!("create_device: {e}"));
            }
        };
        // Validate the queue right away (proves the logical device works);
        // the handle is re-queried per use via `queue_of`.
        let q = device.get_device_queue(qfi, 0);
        if q == ash::vk::Queue::null() {
            device.destroy_device(None);
            instance.destroy_instance(None);
            return Err("no device: null graphics queue".into());
        }
        eprintln!("vulkan: device {name} subgroupSize {subgroup} -> {lane:?} (wsi {wsi})");
        Ok(VulkanState {
            name,
            lane,
            subgroup_size: subgroup,
            queue_family: qfi,
            wsi,
            instance,
            entry,
            device,
        })
    }
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
    fn acquire_returns_real_device_or_skips() {
        // Step-17 device slice: one process Vulkan device (graphics queue,
        // lane from the real subgroupSize), or a clean skip with no device.
        match acquire() {
            Some(st) => {
                assert!(!st.name.is_empty(), "device name must not be empty");
                assert_eq!(st.lane, vulkan_runner::lane_path_for(st.subgroup_size));
                let q = queue_of(st);
                assert!(q != ash::vk::Queue::null(), "graphics queue must exist");
            }
            None => {
                vulkan_runner::skip_test("no Vulkan");
            }
        }
    }

    #[test]
    fn cached_selection_is_stable() {
        // The D3D12.X hot path uses `select_cached()`; repeated calls must
        // agree (one probe per process, not one per device creation). No env
        // touch here, so this cannot race the env-mutating test below.
        let a = select_cached();
        let b = select_cached();
        assert_eq!(a.backend, b.backend);
        assert_eq!(a.lane, b.lane);
        assert_eq!(a.reason, b.reason);
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
