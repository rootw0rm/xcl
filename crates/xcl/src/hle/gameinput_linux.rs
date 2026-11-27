//! Linux GameInput slice (Phase C step 11n): headless IGameInput.
//!
//! The Windows `hle/gameinput.rs` stays the implementation source of truth (full
//! gamepad/keyboard/XInput polling, readings, callbacks). On Linux there is no
//! XInput/user32 yet, so the test title's `GameInput:` check gets a minimal COM
//! object: `GameInputCreate` hands out a static `IGameInput` whose vtable slot 3
//! (`GetCurrentTimestamp`) advances monotonically and whose slot 2 (`Release`) is
//! callable. All other `IGameInput` slots are safe logging stubs (fail closed)
//! so later title checks get `E_NOTIMPL`/`READING_NOT_FOUND` instead of a crash.
//! Full controller input (evdev/XInput/keyboard) stays Phase E (task-a56a open).

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use crate::util::{HRESULT, S_OK};

const E_POINTER: HRESULT = 0x8000_4003u32 as HRESULT;
const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as HRESULT;
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as HRESULT;

/// Monotonic host clock for `GetCurrentTimestamp` (microseconds since first call + 1).
fn start() -> Instant {
    static T: OnceLock<Instant> = OnceLock::new();
    *T.get_or_init(Instant::now)
}

fn now_us() -> u64 {
    start().elapsed().as_micros() as u64 + 1
}

#[repr(C)]
struct Object {
    vtable: *const usize,
}

// Static COM objects: vtables are leaked tables, never freed.
unsafe impl Sync for Object {}
unsafe impl Send for Object {}

static REFS: AtomicU32 = AtomicU32::new(1);

unsafe extern "win64" fn input_qi(
    _this: *mut Object,
    _iid: *const u8,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "win64" fn add_ref(_this: *mut Object) -> u32 {
    REFS.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn release(_this: *mut Object) -> u32 {
    let prev = REFS.fetch_sub(1, Ordering::Relaxed);
    // Static object: never freed; clamp at 1 so repeated releases stay callable.
    if prev <= 1 {
        REFS.store(1, Ordering::Relaxed);
        return 1;
    }
    prev - 1
}

unsafe extern "win64" fn get_current_timestamp(_this: *mut Object) -> u64 {
    now_us()
}

fn input_vtable() -> *const usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        // 24 IGameInput slots: logging stubs that fail, with 0-3 filled in.
        let mut v: Vec<usize> = (0..24)
            .map(|i| {
                crate::stubs::make_stub_info(
                    "GameInput.dll",
                    &format!("IGameInput::#{i}"),
                    E_NOTIMPL as u32 as u64,
                )
                .0
            })
            .collect();
        v[0] = input_qi as usize;
        v[1] = add_ref as usize;
        v[2] = release as usize;
        v[3] = get_current_timestamp as usize;
        Box::leak(v.into_boxed_slice()).as_ptr() as usize
    }) as *const usize
}

fn input_object() -> *mut Object {
    static G: OnceLock<usize> = OnceLock::new();
    *G.get_or_init(|| {
        let v = input_vtable();
        Box::leak(Box::new(Object { vtable: v })) as *mut Object as usize
    }) as *mut Object
}

/// `GameInputCreate(&gameInput)`: hands out the headless static `IGameInput`.
pub unsafe extern "win64" fn GameInputCreate(out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = input_object() as *mut c_void;
    crate::xlog_info!("gameinput: GameInputCreate -> headless IGameInput (timestamps only)");
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![("GameInputCreate", GameInputCreate as usize)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_hands_out_timestamps_that_advance() {
        unsafe {
            assert_eq!(GameInputCreate(std::ptr::null_mut()), E_POINTER);
            let mut gi: *mut c_void = std::ptr::null_mut();
            assert_eq!(GameInputCreate(&mut gi), S_OK);
            assert!(!gi.is_null());
            let vt = *(gi as *const *const usize);
            let ts: unsafe extern "win64" fn(*mut c_void) -> u64 =
                std::mem::transmute(*vt.add(3));
            let a = ts(gi);
            std::thread::sleep(std::time::Duration::from_millis(2));
            let b = ts(gi);
            assert!(a > 0, "timestamp {a}");
            assert!(b > a, "timestamps {a} {b}");
            let release: unsafe extern "win64" fn(*mut c_void) -> u32 =
                std::mem::transmute(*vt.add(2));
            let _ = release(gi);
            // Still usable after a release (static object).
            let c = ts(gi);
            assert!(c >= b, "timestamps {b} {c}");
        }
    }

    #[test]
    fn unimplemented_slots_are_stub_thunks() {
        unsafe {
            let mut gi: *mut c_void = std::ptr::null_mut();
            assert_eq!(GameInputCreate(&mut gi), S_OK);
            let vt = *(gi as *const *const usize);
            // Slot 4 (GetCurrentReading) stays a stub, never null.
            let slot4 = *vt.add(4);
            assert_ne!(slot4, 0);
            assert!(crate::stubs::info_of_stub(slot4).is_some());
        }
    }

    #[test]
    fn slot_9_is_an_honest_e_notimpl_stub() {
        // Retail `0x14592586a` calls slot 9 (RegisterDeviceCallback) and ignores
        // the HRESULT; the token out-param is never read. E_NOTIMPL is therefore
        // the honest answer, and a failed registration must leave the token alone.
        unsafe {
            let mut gi: *mut c_void = std::ptr::null_mut();
            assert_eq!(GameInputCreate(&mut gi), S_OK);
            let vt = *(gi as *const *const usize);
            let slot9 = *vt.add(9);
            let info = crate::stubs::info_of_stub(slot9).expect("slot 9 is a stub thunk");
            assert_eq!(info.name, "IGameInput::#9");
            assert_eq!(info.ret, E_NOTIMPL as u32 as u64);
            let call: unsafe extern "win64" fn(
                *mut c_void,
                *mut c_void,
                u32,
                u32,
                u32,
                *mut c_void,
                usize,
                *mut u64,
            ) -> u64 = std::mem::transmute(slot9);
            let mut token: u64 = 0xDEAD_BEEF;
            assert_eq!(
                call(
                    gi,
                    std::ptr::null_mut(),
                    0x4_0000,
                    1,
                    2,
                    std::ptr::null_mut(),
                    0,
                    &mut token,
                ),
                E_NOTIMPL as u32 as u64
            );
            assert_eq!(token, 0xDEAD_BEEF, "a failed registration must not write the token");
        }
    }
}
