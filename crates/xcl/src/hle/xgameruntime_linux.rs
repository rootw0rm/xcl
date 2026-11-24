//! Linux xgameruntime identity slice (Phase C step 11i): console type and package.
//!
//! The Windows `hle/xgameruntime/mod.rs` stays the implementation source of truth.
//! This module is the Linux twin for the trivial identity queries the test title
//! checks first: `XSystemGetDeviceType` (7 Series X / 6 Series S), runtime init
//! (`S_OK`, idempotent), and the package/console identity strings. All real
//! `win64` code addresses so the imports resolve as HLE; everything else in
//! `XGAMERUNTIME` stays `Stub` until later slices (task queue, users, saves, PLS).

#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::util::{write_cstr_out, HRESULT, S_OK};

pub const DEVICE_XBOX_SCARLETT_LOCKHART: u32 = 0x06;
pub const DEVICE_XBOX_SCARLETT_ANACONDA: u32 = 0x07;

static INIT_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub unsafe extern "win64" fn XGameRuntimeInitialize() -> HRESULT {
    use std::sync::atomic::Ordering;
    if INIT_COUNT.fetch_add(1, Ordering::AcqRel) == 0 {
        crate::xlog_info!("XGameRuntimeInitialize (XCL offline runtime)");
    }
    S_OK
}

pub unsafe extern "win64" fn XGameRuntimeInitializeWithOptions(_options: *const c_void) -> HRESULT {
    XGameRuntimeInitialize()
}

pub unsafe extern "win64" fn XGameRuntimeUninitialize() {
    use std::sync::atomic::Ordering;
    INIT_COUNT.fetch_sub(1, Ordering::AcqRel);
}

pub unsafe extern "win64" fn XGameRuntimeIsFeatureAvailable(feature: u32) -> bool {
    matches!(feature, 0 | 2..=13 | 15..=21)
}

pub unsafe extern "win64" fn XSystemGetDeviceType() -> u32 {
    if crate::state::config().is_series_s() {
        DEVICE_XBOX_SCARLETT_LOCKHART
    } else {
        DEVICE_XBOX_SCARLETT_ANACONDA
    }
}

pub unsafe extern "win64" fn XSystemGetConsoleId(
    size: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HRESULT {
    write_cstr_out(&crate::state::config().console_id, buf, size, used)
}

pub unsafe extern "win64" fn XSystemGetXboxLiveSandboxId(
    size: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HRESULT {
    write_cstr_out(&crate::state::config().sandbox_id, buf, size, used)
}

fn package_identifier() -> String {
    let c = crate::state::config();
    if !c.package_identifier.is_empty() {
        return c.package_identifier.clone();
    }
    let title = crate::state::boot()
        .map(|b| b.title.clone())
        .unwrap_or_else(|| "XCL.Title".into());
    format!("{title}_1.0.0.0_x64__xcl")
}

pub unsafe extern "win64" fn XPackageIsPackagedProcess() -> bool {
    true
}

pub unsafe extern "win64" fn XPackageGetCurrentProcessPackageIdentifier(
    size: usize,
    buf: *mut u8,
) -> HRESULT {
    write_cstr_out(&package_identifier(), buf, size, std::ptr::null_mut())
}

pub unsafe extern "win64" fn XPackageGetIdentifier(
    _path_or_name: *const u8,
    size: usize,
    buf: *mut u8,
) -> HRESULT {
    write_cstr_out(&package_identifier(), buf, size, std::ptr::null_mut())
}

/// Implemented exports with real code addresses. Everything else in `XGAMERUNTIME`
/// stays `Stub` (metadata `0`) until later slices land.
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("XGameRuntimeInitialize", XGameRuntimeInitialize as usize),
        (
            "XGameRuntimeInitializeWithOptions",
            XGameRuntimeInitializeWithOptions as usize,
        ),
        (
            "XGameRuntimeUninitialize",
            XGameRuntimeUninitialize as usize,
        ),
        (
            "XGameRuntimeIsFeatureAvailable",
            XGameRuntimeIsFeatureAvailable as usize,
        ),
        ("XSystemGetDeviceType", XSystemGetDeviceType as usize),
        ("XSystemGetConsoleId", XSystemGetConsoleId as usize),
        (
            "XSystemGetXboxLiveSandboxId",
            XSystemGetXboxLiveSandboxId as usize,
        ),
        (
            "XPackageIsPackagedProcess",
            XPackageIsPackagedProcess as usize,
        ),
        (
            "XPackageGetCurrentProcessPackageIdentifier",
            XPackageGetCurrentProcessPackageIdentifier as usize,
        ),
        ("XPackageGetIdentifier", XPackageGetIdentifier as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_type_matches_config() {
        let got = unsafe { XSystemGetDeviceType() };
        assert!(got == DEVICE_XBOX_SCARLETT_ANACONDA || got == DEVICE_XBOX_SCARLETT_LOCKHART);
        assert_eq!(
            got,
            if crate::state::config().is_series_s() {
                DEVICE_XBOX_SCARLETT_LOCKHART
            } else {
                DEVICE_XBOX_SCARLETT_ANACONDA
            }
        );
    }

    #[test]
    fn runtime_init_is_idempotent_s_ok() {
        assert_eq!(unsafe { XGameRuntimeInitialize() }, S_OK);
        assert_eq!(unsafe { XGameRuntimeInitialize() }, S_OK);
        unsafe { XGameRuntimeUninitialize() };
        unsafe { XGameRuntimeUninitialize() };
    }

    #[test]
    fn package_id_starts_with_boot_title() {
        let mut buf = [0u8; 256];
        let hr = unsafe { XPackageGetCurrentProcessPackageIdentifier(256, buf.as_mut_ptr()) };
        assert_eq!(hr, S_OK);
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
        let title = crate::state::boot()
            .map(|b| b.title.clone())
            .unwrap_or_else(|| "XCL.Title".into());
        assert!(s.starts_with(&title), "package id {s:?} title {title:?}");
    }

    #[test]
    fn feature_availability_matches_windows_table() {
        assert!(unsafe { XGameRuntimeIsFeatureAvailable(0) });
        assert!(unsafe { XGameRuntimeIsFeatureAvailable(7) });
        assert!(!unsafe { XGameRuntimeIsFeatureAvailable(1) });
        assert!(!unsafe { XGameRuntimeIsFeatureAvailable(14) });
    }
}
