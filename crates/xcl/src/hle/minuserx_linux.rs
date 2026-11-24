//! Linux minuserx window slice (Phase C step 11m): headless console window.
//!
//! The Windows `hle/user.rs` stays the implementation source of truth (real Win32
//! `CreateWindowExW`, cloaking, style fixups). On Linux there is no user32, so the
//! test title's `user32:` check gets headless fakes with console semantics:
//! a top-level window is the 1920x1080 screen, frameless (no caption/border),
//! toolwindow (no taskbar button), cloaked. All real `win64` code addresses so the
//! imports resolve as HLE; `user32.dll`/`dwmapi.dll` have no route and resolve
//! through the cross-HLE `find_any` name lookup (same pattern as the CRT slice).

#![allow(non_snake_case)]

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::util::{HRESULT, S_OK};

pub const CONSOLE_W: i32 = 1920;
pub const CONSOLE_H: i32 = 1080;

/// `WNDCLASSW` as the title passes it to `RegisterClassW` (no `cbSize`).
#[repr(C)]
pub struct WndClassW {
    pub style: u32,
    pub proc_: usize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: usize,
    pub icon: usize,
    pub cursor: usize,
    pub background: usize,
    pub menu: *const u16,
    pub class: *const u16,
}

/// `WNDCLASSEXW` (`u16`) / `WNDCLASSEXA` (`u8`): same layout, strings differ.
#[repr(C)]
pub struct WndClassEx<C> {
    pub cb_size: u32,
    pub style: u32,
    pub wnd_proc: usize,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: usize,
    pub icon: usize,
    pub cursor: usize,
    pub background: usize,
    pub menu_name: *const C,
    pub class_name: *const C,
    pub icon_small: usize,
}

static NEXT_ATOM: AtomicU16 = AtomicU16::new(1);
static NEXT_HWND: AtomicUsize = AtomicUsize::new(0x2_0000);
static WINDOWS: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

fn track_hwnd(hwnd: usize) {
    let mut g = WINDOWS.lock().unwrap();
    let set = g.get_or_insert_with(HashSet::new);
    set.insert(hwnd);
}

fn untrack_hwnd(hwnd: usize) -> bool {
    let mut g = WINDOWS.lock().unwrap();
    match g.as_mut() {
        Some(set) => set.remove(&hwnd),
        None => false,
    }
}

fn next_atom() -> u16 {
    // 0 is never a valid atom; skip it on wrap.
    let mut a = NEXT_ATOM.fetch_add(1, Ordering::Relaxed);
    if a == 0 {
        a = NEXT_ATOM.fetch_add(1, Ordering::Relaxed);
    }
    if a == 0 { 1 } else { a }
}

pub unsafe extern "win64" fn RegisterClassW(c: *const WndClassW) -> u16 {
    if c.is_null() || (*c).class.is_null() {
        return 0;
    }
    let atom = next_atom();
    crate::xlog_info!("minuserx: RegisterClassW -> atom {atom:#x}");
    atom
}

pub unsafe extern "win64" fn RegisterClassExW(c: *const WndClassEx<u16>) -> u16 {
    if c.is_null() || (*c).class_name.is_null() {
        return 0;
    }
    let atom = next_atom();
    crate::xlog_info!("minuserx: RegisterClassExW -> atom {atom:#x}");
    atom
}

pub unsafe extern "win64" fn RegisterClassA(c: *const WndClassW) -> u16 {
    // ANSI class name sits where the Unicode one does; only nullness matters here.
    if c.is_null() || (*c).class.is_null() {
        return 0;
    }
    let atom = next_atom();
    crate::xlog_info!("minuserx: RegisterClassA -> atom {atom:#x}");
    atom
}

pub unsafe extern "win64" fn RegisterClassExA(c: *const WndClassEx<u8>) -> u16 {
    if c.is_null() || (*c).class_name.is_null() {
        return 0;
    }
    let atom = next_atom();
    crate::xlog_info!("minuserx: RegisterClassExA -> atom {atom:#x}");
    atom
}

#[allow(clippy::too_many_arguments)]
pub unsafe extern "win64" fn CreateWindowExW(
    _ex_style: u32,
    class: *const u16,
    _title: *const u16,
    _style: u32,
    _x: i32,
    _y: i32,
    _w: i32,
    _h: i32,
    _parent: *mut c_void,
    _menu: *mut c_void,
    _instance: *mut c_void,
    _param: *const c_void,
) -> *mut c_void {
    if class.is_null() {
        return std::ptr::null_mut();
    }
    let hwnd = NEXT_HWND.fetch_add(0x1000, Ordering::Relaxed);
    track_hwnd(hwnd);
    crate::xlog_info!("minuserx: CreateWindowExW -> hwnd {hwnd:#x} (console {CONSOLE_W}x{CONSOLE_H})");
    hwnd as *mut c_void
}

#[allow(clippy::too_many_arguments)]
pub unsafe extern "win64" fn CreateWindowExA(
    _ex_style: u32,
    class: *const u8,
    _title: *const u8,
    _style: u32,
    _x: i32,
    _y: i32,
    _w: i32,
    _h: i32,
    _parent: *mut c_void,
    _menu: *mut c_void,
    _instance: *mut c_void,
    _param: *const c_void,
) -> *mut c_void {
    if class.is_null() {
        return std::ptr::null_mut();
    }
    let hwnd = NEXT_HWND.fetch_add(0x1000, Ordering::Relaxed);
    track_hwnd(hwnd);
    crate::xlog_info!("minuserx: CreateWindowExA -> hwnd {hwnd:#x} (console {CONSOLE_W}x{CONSOLE_H})");
    hwnd as *mut c_void
}

pub unsafe extern "win64" fn GetWindowLongW(_hwnd: *mut c_void, index: i32) -> i32 {
    // GWL_EXSTYLE (-20): toolwindow, no appwindow/frame edges.
    // GWL_STYLE (-16): popup, no caption/border.
    match index {
        -20 => 0x80u32 as i32,
        -16 => 0x8000_0000u32 as i32,
        _ => 0,
    }
}

pub unsafe extern "win64" fn GetWindowLongA(hwnd: *mut c_void, index: i32) -> i32 {
    GetWindowLongW(hwnd, index)
}

pub unsafe extern "win64" fn GetClientRect(_hwnd: *mut c_void, r: *mut [i32; 4]) -> i32 {
    if r.is_null() {
        return 0;
    }
    (*r)[0] = 0;
    (*r)[1] = 0;
    (*r)[2] = CONSOLE_W;
    (*r)[3] = CONSOLE_H;
    1
}

pub unsafe extern "win64" fn DefWindowProcW(
    _hwnd: *mut c_void,
    _msg: u32,
    _wp: usize,
    _lp: isize,
) -> isize {
    0
}

pub unsafe extern "win64" fn DestroyWindow(hwnd: *mut c_void) -> i32 {
    if hwnd.is_null() {
        return 0;
    }
    untrack_hwnd(hwnd as usize);
    1
}

/// `DwmGetWindowAttribute` for the cloaked check (`attr == 14` / `DWMWA_CLOAKED`).
/// Reports every headless window as cloaked; other attributes zero-fill.
pub unsafe extern "win64" fn DwmGetWindowAttribute(
    _hwnd: *mut c_void,
    attr: u32,
    value: *mut c_void,
    size: u32,
) -> HRESULT {
    if value.is_null() || size == 0 {
        return 0x8007_000Du32 as HRESULT; // E_INVALIDARG
    }
    let n = (size as usize).min(4);
    std::ptr::write_bytes(value as *mut u8, 0, n);
    if attr == 14 && size >= 4 {
        *(value as *mut u32) = 1;
    }
    crate::xlog_info!("minuserx: DwmGetWindowAttribute(attr={attr}) -> cloaked");
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("CreateWindowExA", CreateWindowExA as usize),
        ("RegisterClassExA", RegisterClassExA as usize),
        ("CreateWindowExW", CreateWindowExW as usize),
        ("RegisterClassW", RegisterClassW as usize),
        ("RegisterClassExW", RegisterClassExW as usize),
        ("RegisterClassA", RegisterClassA as usize),
        ("GetWindowLongW", GetWindowLongW as usize),
        ("GetWindowLongA", GetWindowLongA as usize),
        ("GetClientRect", GetClientRect as usize),
        ("DefWindowProcW", DefWindowProcW as usize),
        ("DestroyWindow", DestroyWindow as usize),
        ("DwmGetWindowAttribute", DwmGetWindowAttribute as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_window_looks_like_the_console_screen() {
        unsafe {
            let class: Vec<u16> = "XclTest\0".encode_utf16().collect();
            let title: Vec<u16> = "t\0".encode_utf16().collect();
            let wc = WndClassW {
                style: 0,
                proc_: DefWindowProcW as usize,
                cls_extra: 0,
                wnd_extra: 0,
                instance: 1,
                icon: 0,
                cursor: 0,
                background: 0,
                menu: std::ptr::null(),
                class: class.as_ptr(),
            };
            assert_ne!(RegisterClassW(&wc), 0);
            assert_eq!(RegisterClassW(std::ptr::null()), 0);
            let w = CreateWindowExW(
                0x4_0000,
                class.as_ptr(),
                title.as_ptr(),
                0x00CF_0000 | 0x1000_0000,
                100,
                100,
                1280,
                720,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1 as *mut c_void,
                std::ptr::null(),
            );
            assert!(!w.is_null());
            let ex = GetWindowLongW(w, -20) as u32;
            let st = GetWindowLongW(w, -16) as u32;
            assert_ne!(ex & 0x80, 0);
            assert_eq!(ex & 0x4_0000, 0);
            assert_ne!(st & 0x8000_0000, 0);
            assert_eq!(st & 0x00C0_0000, 0);
            let mut r = [0i32; 4];
            assert_eq!(GetClientRect(w, &mut r), 1);
            assert_eq!((r[2] - r[0], r[3] - r[1]), (1920, 1080));
            let mut cloaked = 0u32;
            assert_eq!(
                DwmGetWindowAttribute(
                    w,
                    14,
                    &mut cloaked as *mut u32 as *mut c_void,
                    4
                ),
                S_OK
            );
            assert_ne!(cloaked, 0);
            assert_eq!(DestroyWindow(w), 1);
            assert_eq!(DestroyWindow(std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn ansi_entry_points_match_the_unicode_screen() {
        unsafe {
            let class = b"XclTestA\0";
            let title = b"t\0";
            let w = CreateWindowExA(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0x00CF_0000,
                100,
                100,
                1280,
                720,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                1 as *mut c_void,
                std::ptr::null(),
            );
            assert!(!w.is_null());
            assert_eq!(CreateWindowExA(0, std::ptr::null(), title.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), 1 as *mut c_void, std::ptr::null()), std::ptr::null_mut());
            let mut r = [0i32; 4];
            assert_eq!(GetClientRect(w, &mut r), 1);
            assert_eq!((r[2], r[3]), (1920, 1080));
            assert_eq!(GetClientRect(std::ptr::null_mut(), std::ptr::null_mut()), 0);
            assert_eq!(DestroyWindow(w), 1);
        }
    }
}
