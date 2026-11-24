//! Linux CRT startup slice (Phase C step 11): the pointers MSVC startup needs.
//!
//! Rust/Windows `mainCRTStartup` copies the link-time `_fmode`/`_commode` into the
//! running CRT through `__p__fmode`/`__p__commode`, then reads `argc`/`argv`/`env`
//! through `__p___argc`/`__p___argv`/`__p__environ` and `errno` through `_errno`.
//! Auto-stubs return `0` for all of these, so the first write (`movl %esi, (%rax)`
//! after `__p__fmode`) faults with address `0x0`. The shims below hand out real,
//! process-lifetime addresses instead; narrow init helpers (`_configure_narrow_argv`,
//! `_initialize_narrow_environment`, `_initterm`, …) report success, and
//! `_initterm`/`_initterm_e` genuinely walk their tables in Windows order.
//!
//! Calling convention: the game is a Windows PE, so every export is
//! `unsafe extern "win64"`, matching the stub thunks (DEC-013).
//!
//! Deliberately out of scope (later slices): real command-line/environment wiring
//! (`argv` is a static placeholder until launcher→host→CRT arg plumbing lands),
//! stdio over the VFS (`fopen`/`fread`/… stay stubs), and `Heap*` (kernelx slice).

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------------------
// Process-lifetime cells (lazily `malloc`ed once; a lost race leaks a few bytes and the
// loser keeps a valid pointer, which is harmless for idempotent startup reads).
// ---------------------------------------------------------------------------------------

/// Allocate `n` zeroed bytes that live for the rest of the process.
fn immortal_zeroed(n: usize) -> *mut u8 {
    unsafe {
        let p = libc::malloc(n) as *mut u8;
        assert!(!p.is_null(), "crt: out of memory");
        std::ptr::write_bytes(p, 0, n);
        p
    }
}

fn cached_ptr(slot: &AtomicUsize, n: usize) -> *mut u8 {
    let p = slot.load(Ordering::Relaxed);
    if p != 0 {
        return p as *mut u8;
    }
    let q = immortal_zeroed(n);
    // A racing thread may win; either pointer is valid zeroed storage.
    let _ = slot.compare_exchange(0, q as usize, Ordering::Relaxed, Ordering::Relaxed);
    slot.load(Ordering::Relaxed) as *mut u8
}

static FMODE_PTR: AtomicUsize = AtomicUsize::new(0);
static COMMODE_PTR: AtomicUsize = AtomicUsize::new(0);
static ARGC_PTR: AtomicUsize = AtomicUsize::new(0);
static ARGV_PTR: AtomicUsize = AtomicUsize::new(0);
static WARGV_PTR: AtomicUsize = AtomicUsize::new(0);
static ENVIRON_PTR: AtomicUsize = AtomicUsize::new(0);
static WENVIRON_PTR: AtomicUsize = AtomicUsize::new(0);
static IOB_PTR: AtomicUsize = AtomicUsize::new(0);
static LOCALE_PTR: AtomicUsize = AtomicUsize::new(0);

/// `int*` to the CRT `_fmode` variable (link-time file-translation default).
pub unsafe extern "win64" fn __p__fmode() -> *mut i32 {
    cached_ptr(&FMODE_PTR, 4) as *mut i32
}

/// `int*` to the CRT `_commode` variable (commit-mode default).
pub unsafe extern "win64" fn __p__commode() -> *mut i32 {
    cached_ptr(&COMMODE_PTR, 4) as *mut i32
}

thread_local! {
    static ERRNO_CELL: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// `int*` to the calling thread's `errno` (per-thread, as on Windows).
pub unsafe extern "win64" fn _errno() -> *mut i32 {
    ERRNO_CELL.with(|c| c.as_ptr())
}

/// `int*` to `__argc` (placeholder `1` until arg plumbing lands).
pub unsafe extern "win64" fn __p___argc() -> *mut i32 {
    let p = cached_ptr(&ARGC_PTR, 4) as *mut i32;
    if *p == 0 {
        *p = 1;
    }
    p
}

fn narrow_argv() -> *mut *mut u8 {
    let table = cached_ptr(&ARGV_PTR, 16) as *mut *mut u8;
    unsafe {
        if (*table).is_null() {
            // Static placeholder program name; real argv wiring is a later slice.
            static NAME: &[u8] = b"xcl\0";
            *table = NAME.as_ptr() as *mut u8;
            *table.add(1) = std::ptr::null_mut();
        }
    }
    table
}

/// `char***` to the narrow argument vector (`argv[argc] == NULL`).
pub unsafe extern "win64" fn __p___argv() -> *mut *mut u8 {
    narrow_argv()
}

fn wide_argv() -> *mut *mut u16 {
    let table = cached_ptr(&WARGV_PTR, 16) as *mut *mut u16;
    unsafe {
        if (*table).is_null() {
            static NAME: &[u16] = &[b'x' as u16, b'c' as u16, b'l' as u16, 0];
            *table = NAME.as_ptr() as *mut u16;
            *table.add(1) = std::ptr::null_mut();
        }
    }
    table
}

/// `wchar_t***` to the wide argument vector.
pub unsafe extern "win64" fn __p___wargv() -> *mut *mut u16 {
    wide_argv()
}

fn narrow_environ() -> *mut *mut u8 {
    let table = cached_ptr(&ENVIRON_PTR, 8) as *mut *mut u8;
    unsafe {
        // Empty environment: a single NULL entry.
        *table = std::ptr::null_mut();
    }
    table
}

/// `char***` to the narrow environment block (currently empty).
pub unsafe extern "win64" fn __p__environ() -> *mut *mut u8 {
    narrow_environ()
}

fn wide_environ() -> *mut *mut u16 {
    let table = cached_ptr(&WENVIRON_PTR, 8) as *mut *mut u16;
    unsafe {
        *table = std::ptr::null_mut();
    }
    table
}

/// `wchar_t***` to the wide environment block (currently empty).
pub unsafe extern "win64" fn __p__wenviron() -> *mut *mut u16 {
    wide_environ()
}

/// `FILE*` for `fd` in `{0, 1, 2}` (opaque 64-byte slots; stdio stays unbacked).
pub unsafe extern "win64" fn __acrt_iob_func(fd: u32) -> *mut u8 {
    if fd > 2 {
        return std::ptr::null_mut();
    }
    cached_ptr(&IOB_PTR, 3 * 64).add(fd as usize * 64)
}

// ---------------------------------------------------------------------------------------
// Narrow init / atexit / initterm (success codes, with real table walks).
// ---------------------------------------------------------------------------------------

/// `int _set_app_type(int)` — remember nothing, report the previous type (`0`).
pub unsafe extern "win64" fn _set_app_type(_t: i32) -> i32 {
    0
}

/// `__acrt_thread_parameter* _set_invalid_parameter_handler(...)` — was NULL.
pub unsafe extern "win64" fn _set_invalid_parameter_handler(_h: usize) -> usize {
    0
}

/// `int _crt_atexit(void (__cdecl* f)(void))` — `0` = registered.
pub unsafe extern "win64" fn _crt_atexit(_f: usize) -> i32 {
    0
}

/// `int _register_onexit_function(...)` — `0` = registered.
pub unsafe extern "win64" fn _register_onexit_function(_table: usize, _f: usize) -> i32 {
    0
}

/// `int _initialize_onexit_table(...)` — `0` = success.
pub unsafe extern "win64" fn _initialize_onexit_table(_table: usize) -> i32 {
    0
}

/// `int _initialize_narrow_environment(void)` — `0` = success.
pub unsafe extern "win64" fn _initialize_narrow_environment() -> i32 {
    0
}

/// `int _configure_narrow_argv(int)` — `0` = success.
pub unsafe extern "win64" fn _configure_narrow_argv(_mode: i32) -> i32 {
    0
}

/// `int _seh_filter_exe(...)` — `EXCEPTION_EXECUTE_HANDLER` (`1`).
pub unsafe extern "win64" fn _seh_filter_exe(_code: u32, _info: usize) -> i32 {
    1
}

/// `int __setusermatherr(...)` — no-op (void upstream; `0` keeps the stub shape).
pub unsafe extern "win64" fn __setusermatherr(_f: usize) {
}

/// `int _configthreadlocale(int)` — best-effort `0` (previous setting).
pub unsafe extern "win64" fn _configthreadlocale(_flag: i32) -> i32 {
    0
}

/// `int _set_new_mode(int)` — previous mode was `0` (disabled).
pub unsafe extern "win64" fn _set_new_mode(_mode: i32) -> i32 {
    0
}

/// `void _lock_file(FILE*)` — single-threaded startup needs no locking.
pub unsafe extern "win64" fn _lock_file(_f: *mut u8) {}

/// `void _unlock_file(FILE*)` — see `_lock_file`.
pub unsafe extern "win64" fn _unlock_file(_f: *mut u8) {}

type InitFn = unsafe extern "win64" fn();
type InitEFn = unsafe extern "win64" fn() -> i32;

/// `int _initterm(first, last)` — run the C initializer table in order.
pub unsafe extern "win64" fn _initterm(first: *mut usize, last: *mut usize) -> i32 {
    if first.is_null() || last.is_null() {
        return 0;
    }
    let mut p = first;
    while p < last {
        let f = *p;
        if f != 0 {
            (std::mem::transmute::<usize, InitFn>(f))();
        }
        p = p.add(1);
    }
    0
}

/// `int _initterm_e(first, last)` — run the C++ initializer table, stop on error.
pub unsafe extern "win64" fn _initterm_e(first: *mut usize, last: *mut usize) -> i32 {
    if first.is_null() || last.is_null() {
        return 0;
    }
    let mut p = first;
    while p < last {
        let f = *p;
        if f != 0 {
            let rc = (std::mem::transmute::<usize, InitEFn>(f))();
            if rc != 0 {
                return rc;
            }
        }
        p = p.add(1);
    }
    0
}

/// `void abort(void)` — abnormal termination.
pub unsafe extern "win64" fn abort() -> ! {
    libc::abort();
}

/// `void exit(int)` — flush-free termination (stdio is unbacked).
pub unsafe extern "win64" fn exit(code: i32) -> ! {
    libc::_exit(code);
}

/// `void _exit(int)` — immediate termination.
pub unsafe extern "win64" fn _exit(code: i32) -> ! {
    libc::_exit(code);
}

/// `void _cexit(void)` — run C terminators, then terminate with code `0`.
pub unsafe extern "win64" fn _cexit() -> ! {
    libc::_exit(0);
}

/// `char* strerror(int)` — static generic message (per-code text is a later slice).
pub unsafe extern "win64" fn strerror(_err: i32) -> *const u8 {
    static MSG: &[u8] = b"unknown error\0";
    MSG.as_ptr()
}

/// `struct lconv* localeconv(void)` — zeroed placeholder (fields read as empty/zero).
pub unsafe extern "win64" fn localeconv() -> *mut u8 {
    cached_ptr(&LOCALE_PTR, 128)
}

// ---------------------------------------------------------------------------------------
// Heap and memory/string primitives the CRT forwards to.
// ---------------------------------------------------------------------------------------

/// `void* malloc(size_t)` — libc heap, zero-size yields a freeable minimum block.
pub unsafe extern "win64" fn malloc(n: usize) -> *mut c_void {
    let p = libc::malloc(n.max(1)) as *mut c_void;
    assert!(!p.is_null(), "crt: out of memory");
    p
}

/// `void* calloc(size_t, size_t)` — zeroed libc heap.
pub unsafe extern "win64" fn calloc(count: usize, size: usize) -> *mut c_void {
    let p = libc::calloc(count.max(1), size.max(1)) as *mut c_void;
    assert!(!p.is_null(), "crt: out of memory");
    p
}

/// `void* realloc(void*, size_t)` — `NULL` behaves as `malloc`.
pub unsafe extern "win64" fn realloc(p: *mut c_void, n: usize) -> *mut c_void {
    let q = libc::realloc(p as *mut libc::c_void, n.max(1)) as *mut c_void;
    assert!(!q.is_null(), "crt: out of memory");
    q
}

/// `void free(void*)` — `NULL` is a no-op, as on Windows.
pub unsafe extern "win64" fn free(p: *mut c_void) {
    if !p.is_null() {
        libc::free(p as *mut libc::c_void);
    }
}

/// `void* memcpy(dst, src, n)` — returns `dst`.
pub unsafe extern "win64" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n != 0 {
        std::ptr::copy_nonoverlapping(src, dst, n);
    }
    dst
}

/// `void* memmove(dst, src, n)` — overlap-safe, returns `dst`.
pub unsafe extern "win64" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n != 0 {
        std::ptr::copy(src, dst, n);
    }
    dst
}

/// `int memcmp(a, b, n)` — byte-wise order, `0` when equal.
pub unsafe extern "win64" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let x = *a.add(i);
        let y = *b.add(i);
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

/// `void* memset(dst, c, n)` — returns `dst`.
pub unsafe extern "win64" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    if n != 0 {
        std::ptr::write_bytes(dst, c as u8, n);
    }
    dst
}

/// `size_t strlen(s)` — bytes before the first NUL.
pub unsafe extern "win64" fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// `size_t strnlen(s, max)` — bounded `strlen`.
pub unsafe extern "win64" fn strnlen(s: *const u8, max: usize) -> usize {
    let mut n = 0;
    while n < max && *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// `size_t wcslen(s)` — UTF-16 units before the first NUL.
pub unsafe extern "win64" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// `size_t wcsnlen(s, max)` — bounded `wcslen`.
pub unsafe extern "win64" fn wcsnlen(s: *const u16, max: usize) -> usize {
    let mut n = 0;
    while n < max && *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// Export table: name → real code address (never `0`/stub metadata).
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("__p__fmode", __p__fmode as usize),
        ("__p__commode", __p__commode as usize),
        ("__p___argc", __p___argc as usize),
        ("__p___argv", __p___argv as usize),
        ("__p___wargv", __p___wargv as usize),
        ("__p__environ", __p__environ as usize),
        ("__p__wenviron", __p__wenviron as usize),
        ("_errno", _errno as usize),
        ("__acrt_iob_func", __acrt_iob_func as usize),
        ("_set_app_type", _set_app_type as usize),
        ("_set_invalid_parameter_handler", _set_invalid_parameter_handler as usize),
        ("_crt_atexit", _crt_atexit as usize),
        ("_register_onexit_function", _register_onexit_function as usize),
        ("_initialize_onexit_table", _initialize_onexit_table as usize),
        ("_initialize_narrow_environment", _initialize_narrow_environment as usize),
        ("_configure_narrow_argv", _configure_narrow_argv as usize),
        ("_seh_filter_exe", _seh_filter_exe as usize),
        ("__setusermatherr", __setusermatherr as usize),
        ("_configthreadlocale", _configthreadlocale as usize),
        ("_set_new_mode", _set_new_mode as usize),
        ("_lock_file", _lock_file as usize),
        ("_unlock_file", _unlock_file as usize),
        ("_initterm", _initterm as usize),
        ("_initterm_e", _initterm_e as usize),
        ("abort", abort as usize),
        ("exit", exit as usize),
        ("_exit", _exit as usize),
        ("_cexit", _cexit as usize),
        ("strerror", strerror as usize),
        ("localeconv", localeconv as usize),
        ("malloc", malloc as usize),
        ("calloc", calloc as usize),
        ("realloc", realloc as usize),
        ("free", free as usize),
        ("memcpy", memcpy as usize),
        ("memmove", memmove as usize),
        ("memcmp", memcmp as usize),
        ("memset", memset as usize),
        ("strlen", strlen as usize),
        ("strnlen", strnlen as usize),
        ("wcslen", wcslen as usize),
        ("wcsnlen", wcsnlen as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn pointer_shims_hand_out_writable_storage() {
        unsafe {
            assert!(!__p__fmode().is_null());
            *__p__fmode() = 0x4000;
            assert_eq!(*__p__fmode(), 0x4000);
            assert!(!__p__commode().is_null());
            *__p__commode() = 1;
            assert_eq!(*__p__commode(), 1);
            // Stable across calls (same CRT variable).
            assert_eq!(__p__fmode(), __p__fmode());
            assert_eq!(__p__commode(), __p__commode());
        }
    }

    #[test]
    fn argc_argv_environ_have_c_shape() {
        unsafe {
            assert_eq!(*__p___argc(), 1);
            let argv = __p___argv();
            assert!(!argv.is_null() && !(*argv).is_null());
            assert!((*argv.add(1)).is_null());
            assert_eq!(strlen(*argv), 3);
            let wargv = __p___wargv();
            assert!(!wargv.is_null() && !(*wargv).is_null());
            assert!((*wargv.add(1)).is_null());
            assert!((*__p__environ()).is_null());
            assert!((*__p__wenviron()).is_null());
        }
    }

    #[test]
    fn errno_is_thread_local_storage() {
        unsafe {
            *_errno() = 22;
            assert_eq!(*_errno(), 22);
        }
    }

    #[test]
    fn iob_func_serves_three_opaque_slots() {
        unsafe {
            for fd in 0..3 {
                let p = __acrt_iob_func(fd);
                assert!(!p.is_null());
                *p = fd as u8;
            }
            assert_eq!(*__acrt_iob_func(0), 0);
            assert_eq!(*__acrt_iob_func(2), 2);
            assert!(__acrt_iob_func(3).is_null());
        }
    }

    #[test]
    fn narrow_init_helpers_report_success() {
        unsafe {
            assert_eq!(_set_app_type(1), 0);
            assert_eq!(_set_invalid_parameter_handler(0), 0);
            assert_eq!(_crt_atexit(0), 0);
            assert_eq!(_register_onexit_function(0, 0), 0);
            assert_eq!(_initialize_onexit_table(0), 0);
            assert_eq!(_initialize_narrow_environment(), 0);
            assert_eq!(_configure_narrow_argv(2), 0);
            assert_eq!(_seh_filter_exe(0xC000_0005, 0), 1);
            assert_eq!(_configthreadlocale(1), 0);
            assert_eq!(_set_new_mode(1), 0);
            assert!(!strerror(2).is_null());
            assert_eq!(strlen(strerror(2)), 13);
            assert!(!localeconv().is_null());
        }
    }

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "win64" fn probe_a() {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "win64" fn probe_b() {
        CALLS.fetch_add(10, Ordering::SeqCst);
    }
    static ECALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "win64" fn probe_ok() -> i32 {
        ECALLS.fetch_add(1, Ordering::SeqCst);
        0
    }
    unsafe extern "win64" fn probe_fail() -> i32 {
        ECALLS.fetch_add(100, Ordering::SeqCst);
        7
    }

    #[test]
    fn initterm_walks_tables_in_order() {
        unsafe {
            CALLS.store(0, Ordering::SeqCst);
            let mut table = [probe_a as usize, 0, probe_b as usize];
            assert_eq!(_initterm(table.as_mut_ptr(), table.as_mut_ptr().add(3)), 0);
            assert_eq!(CALLS.load(Ordering::SeqCst), 11);
            // Null tables are a success no-op (defensive; CRT never passes them).
            assert_eq!(_initterm(std::ptr::null_mut(), std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn initterm_e_stops_on_first_error() {
        unsafe {
            ECALLS.store(0, Ordering::SeqCst);
            let mut table = [probe_ok as usize, probe_fail as usize, probe_ok as usize];
            assert_eq!(_initterm_e(table.as_mut_ptr(), table.as_mut_ptr().add(3)), 7);
            assert_eq!(ECALLS.load(Ordering::SeqCst), 101);
        }
    }

    #[test]
    fn heap_roundtrips_and_strings_measure() {
        unsafe {
            let p = malloc(64) as *mut u8;
            memset(p, 0xAB, 64);
            assert_eq!(*p, 0xAB);
            let q = calloc(4, 8) as *mut u8;
            assert_eq!(*q.add(31), 0);
            let r = realloc(q as *mut c_void, 128) as *mut u8;
            assert_eq!(*r.add(31), 0);
            memcpy(p, b"hi\0".as_ptr(), 3);
            assert_eq!(strlen(p), 2);
            assert_eq!(strnlen(p, 1), 1);
            memmove(p.add(1), p, 2);
            assert_eq!((*p.add(1), *p.add(2)), (b'h', b'i'));
            assert_eq!(memcmp(b"abc\0".as_ptr(), b"abd\0".as_ptr(), 3) < 0, true);
            let w: [u16; 3] = [0x41, 0x42, 0];
            assert_eq!(wcslen(w.as_ptr()), 2);
            assert_eq!(wcsnlen(w.as_ptr(), 1), 1);
            free(p as *mut c_void);
            free(r as *mut c_void);
            free(std::ptr::null_mut());
        }
    }
}
