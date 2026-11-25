//! Linux mfplat slice (Phase C step 12f): headless Media Foundation manager.
//!
//! The Windows `hle/mfplat.rs` stays the implementation source of truth (full
//! Media Foundation on top of the system `mfplat.dll`, including the D3D11
//! bridge for `MFResetDXGIDeviceManagerX`). On Linux there is no system
//! Media Foundation, so the test title's `MFPlat:` check gets a headless
//! manager: `MFStartup`/`MFShutdown` are idempotent `S_OK`, and
//! `MFCreateDXGIDeviceManager` hands out a static `IMFDXGIDeviceManager`
//! whose vtable slots the title exercises (`Release`, `OpenDeviceHandle` /
//! `CloseDeviceHandle`, `TestDevice`) are real. All other manager slots are
//! safe logging stubs (fail closed) so later title checks get `E_NOTIMPL`
//! instead of a crash. `MFResetDXGIDeviceManagerX` validates the reset token
//! (`E_INVALIDARG` on stale token, `S_OK` on the current token) and records
//! the device. Real decode/present stays Phase D/E work.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::util::{E_INVALIDARG, E_NOINTERFACE, E_POINTER, HRESULT, S_OK};

pub const MF_VERSION: u32 = 0x0002_0070;
pub const MFSTARTUP_LITE: u32 = 1;
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as HRESULT;

/// Current reset token handed out by `MFCreateDXGIDeviceManager` (0 = none yet).
static CURRENT_TOKEN: AtomicU32 = AtomicU32::new(0);
/// Monotonic token source so repeated creates invalidate older tokens.
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);
/// Device recorded by the last successful `MFResetDXGIDeviceManagerX` (0 = none).
static BOUND_DEVICE: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Manager {
    vtable: *const usize,
}

// Static COM object: vtable is a leaked table, never freed.
unsafe impl Sync for Manager {}
unsafe impl Send for Manager {}

static REFS: AtomicU32 = AtomicU32::new(1);

unsafe extern "win64" fn manager_qi(
    _this: *mut Manager,
    _iid: *const u8,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "win64" fn manager_add_ref(_this: *mut Manager) -> u32 {
    REFS.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn manager_release(_this: *mut Manager) -> u32 {
    let prev = REFS.fetch_sub(1, Ordering::Relaxed);
    // Static object: never freed; clamp at 1 so repeated releases stay callable.
    if prev <= 1 {
        REFS.store(1, Ordering::Relaxed);
        return 1;
    }
    prev - 1
}

/// `CloseDeviceHandle(manager, handle)`: headless close always succeeds.
unsafe extern "win64" fn manager_close_device_handle(
    _this: *mut Manager,
    _handle: *mut c_void,
) -> HRESULT {
    S_OK
}

/// `OpenDeviceHandle(manager, &handle)`: hands out a fixed non-null handle.
unsafe extern "win64" fn manager_open_device_handle(
    _this: *mut Manager,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1 as *mut c_void;
    S_OK
}

/// `TestDevice(manager, handle)`: headless device is always usable.
unsafe extern "win64" fn manager_test_device(
    _this: *mut Manager,
    _handle: *mut c_void,
) -> HRESULT {
    S_OK
}

// Title-observed vtable layout (see tests/testtitle/src/gpu.rs):
// 2 = Release, 3 = CloseDeviceHandle, 6 = OpenDeviceHandle, 8 = TestDevice.
const MANAGER_SLOTS: usize = 12;
const SLOT_RELEASE: usize = 2;
const SLOT_CLOSE_HANDLE: usize = 3;
const SLOT_OPEN_HANDLE: usize = 6;
const SLOT_TEST_DEVICE: usize = 8;

fn manager_vtable() -> *const usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let mut v: Vec<usize> = (0..MANAGER_SLOTS)
            .map(|i| {
                crate::stubs::make_stub_info(
                    "mfplat.dll",
                    &format!("IMFDXGIDeviceManager::#{i}"),
                    E_NOTIMPL as u32 as u64,
                )
                .0
            })
            .collect();
        v[0] = manager_qi as usize;
        v[1] = manager_add_ref as usize;
        v[SLOT_RELEASE] = manager_release as usize;
        v[SLOT_CLOSE_HANDLE] = manager_close_device_handle as usize;
        v[SLOT_OPEN_HANDLE] = manager_open_device_handle as usize;
        v[SLOT_TEST_DEVICE] = manager_test_device as usize;
        Box::leak(v.into_boxed_slice()).as_ptr() as usize
    }) as *const usize
}

fn manager_object() -> *mut Manager {
    static G: OnceLock<usize> = OnceLock::new();
    *G.get_or_init(|| {
        let v = manager_vtable();
        Box::leak(Box::new(Manager { vtable: v })) as *mut Manager as usize
    }) as *mut Manager
}

/// `MFStartup(version, flags)`: headless start, always `S_OK` (idempotent).
/// The title passes `MF_VERSION` + `MFSTARTUP_LITE`; other callers' versions
/// and flags are accepted and ignored — there is no real pipeline to configure.
pub unsafe extern "win64" fn MFStartup(version: u32, flags: u32) -> HRESULT {
    crate::xlog_info!("mfplat: MFStartup version={version:#x} flags={flags:#x} -> headless S_OK");
    S_OK
}

/// `MFShutdown()`: headless stop, always `S_OK`.
pub unsafe extern "win64" fn MFShutdown() -> HRESULT {
    crate::xlog_info!("mfplat: MFShutdown -> S_OK");
    S_OK
}

/// `MFCreateDXGIDeviceManager(&token, &manager)`: hands out the static
/// headless manager plus a fresh reset token (invalidating older tokens).
pub unsafe extern "win64" fn MFCreateDXGIDeviceManager(
    reset_token: *mut u32,
    manager: *mut *mut c_void,
) -> HRESULT {
    if reset_token.is_null() || manager.is_null() {
        return E_POINTER;
    }
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    // Avoid the 0 "no token yet" sentinel (wraps once per 4G creates at most).
    let token = if token == 0 {
        NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
    } else {
        token
    };
    CURRENT_TOKEN.store(token, Ordering::SeqCst);
    *reset_token = token;
    *manager = manager_object() as *mut c_void;
    crate::xlog_info!("mfplat: MFCreateDXGIDeviceManager -> headless manager (token {token})");
    S_OK
}

/// Xbox GDK `MFResetDXGIDeviceManagerX(manager, device, token)`: binds the
/// D3D12 device to the headless manager. A stale token is `E_INVALIDARG`;
/// the current token records the device and returns `S_OK`.
pub unsafe extern "win64" fn MFResetDXGIDeviceManagerX(
    manager: *mut c_void,
    device: *mut c_void,
    token: u32,
) -> HRESULT {
    if manager.is_null() || device.is_null() {
        return E_POINTER;
    }
    if manager != manager_object() as *mut c_void {
        return E_INVALIDARG;
    }
    let current = CURRENT_TOKEN.load(Ordering::SeqCst);
    if current == 0 || token != current {
        return E_INVALIDARG;
    }
    BOUND_DEVICE.store(device as usize, Ordering::SeqCst);
    crate::xlog_info!("mfplat: MFResetDXGIDeviceManagerX bound device (token {token})");
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("MFStartup", MFStartup as usize),
        ("MFShutdown", MFShutdown as usize),
        ("MFCreateDXGIDeviceManager", MFCreateDXGIDeviceManager as usize),
        ("MFResetDXGIDeviceManagerX", MFResetDXGIDeviceManagerX as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn vtable_slot(mgr: *mut c_void, slot: usize) -> usize {
        let vt = *(mgr as *const *const usize);
        *vt.add(slot)
    }

    #[test]
    fn startup_shutdown_are_idempotent() {
        unsafe {
            assert_eq!(MFStartup(MF_VERSION, MFSTARTUP_LITE), S_OK);
            assert_eq!(MFStartup(MF_VERSION, MFSTARTUP_LITE), S_OK);
            assert_eq!(MFStartup(MF_VERSION, 0), S_OK);
            assert_eq!(MFShutdown(), S_OK);
            assert_eq!(MFShutdown(), S_OK);
        }
    }

    #[test]
    fn create_reset_open_test_close_release() {
        unsafe {
            assert_eq!(MFStartup(MF_VERSION, MFSTARTUP_LITE), S_OK);
            // NULL outputs fail closed, never crash.
            assert_eq!(
                MFCreateDXGIDeviceManager(std::ptr::null_mut(), std::ptr::null_mut()),
                E_POINTER
            );
            let mut token: u32 = 0;
            assert_eq!(
                MFCreateDXGIDeviceManager(&mut token, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(
                MFCreateDXGIDeviceManager(std::ptr::null_mut(), &mut (std::ptr::null_mut())),
                E_POINTER
            );

            let mut mgr: *mut c_void = std::ptr::null_mut();
            assert_eq!(MFCreateDXGIDeviceManager(&mut token, &mut mgr), S_OK);
            assert_ne!(token, 0);
            assert!(!mgr.is_null());

            // NULL manager/device fail closed.
            assert_eq!(MFResetDXGIDeviceManagerX(std::ptr::null_mut(), 0x1000 as *mut c_void, token), E_POINTER);
            assert_eq!(MFResetDXGIDeviceManagerX(mgr, std::ptr::null_mut(), token), E_POINTER);
            // Wrong manager rejects.
            assert_eq!(
                MFResetDXGIDeviceManagerX(0x1000 as *mut c_void, 0x2000 as *mut c_void, token),
                E_INVALIDARG
            );
            // Stale token rejects.
            assert_eq!(
                MFResetDXGIDeviceManagerX(mgr, 0x2000 as *mut c_void, token.wrapping_add(1)),
                E_INVALIDARG
            );
            // Current token binds.
            let fake_device = 0x2000 as *mut c_void;
            assert_eq!(MFResetDXGIDeviceManagerX(mgr, fake_device, token), S_OK);
            assert_eq!(BOUND_DEVICE.load(Ordering::SeqCst), fake_device as usize);

            // A second create invalidates the first token.
            let mut token2: u32 = 0;
            let mut mgr2: *mut c_void = std::ptr::null_mut();
            assert_eq!(MFCreateDXGIDeviceManager(&mut token2, &mut mgr2), S_OK);
            assert_eq!(mgr2, mgr);
            assert_ne!(token2, token);
            assert_eq!(
                MFResetDXGIDeviceManagerX(mgr, fake_device, token),
                E_INVALIDARG
            );
            assert_eq!(MFResetDXGIDeviceManagerX(mgr, fake_device, token2), S_OK);

            // Manager vtable: title slots are real, never NULL.
            type ReleaseFn = unsafe extern "win64" fn(*mut c_void) -> u32;
            type CloseFn = unsafe extern "win64" fn(*mut c_void, *mut c_void) -> HRESULT;
            type OpenFn = unsafe extern "win64" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
            type TestFn = unsafe extern "win64" fn(*mut c_void, *mut c_void) -> HRESULT;
            let release: ReleaseFn = std::mem::transmute(vtable_slot(mgr, SLOT_RELEASE));
            let close: CloseFn = std::mem::transmute(vtable_slot(mgr, SLOT_CLOSE_HANDLE));
            let open: OpenFn = std::mem::transmute(vtable_slot(mgr, SLOT_OPEN_HANDLE));
            let test: TestFn = std::mem::transmute(vtable_slot(mgr, SLOT_TEST_DEVICE));
            let mut handle: *mut c_void = std::ptr::null_mut();
            assert_eq!(open(mgr, std::ptr::null_mut()), E_POINTER);
            assert_eq!(open(mgr, &mut handle), S_OK);
            assert!(!handle.is_null());
            assert_eq!(test(mgr, handle), S_OK);
            assert_eq!(close(mgr, handle), S_OK);
            // Other slots stay stub thunks, never NULL.
            let other = vtable_slot(mgr, 4);
            assert_ne!(other, 0);
            assert!(crate::stubs::info_of_stub(other).is_some());

            // Release clamps at 1: the static object stays usable.
            let _ = release(mgr);
            let open2: OpenFn = std::mem::transmute(vtable_slot(mgr, SLOT_OPEN_HANDLE));
            let mut handle2: *mut c_void = std::ptr::null_mut();
            assert_eq!(open2(mgr, &mut handle2), S_OK);
            assert!(!handle2.is_null());

            assert_eq!(MFShutdown(), S_OK);
        }
    }
}
