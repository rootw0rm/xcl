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
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

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
// `__p___argv` is `char***`: `ARGV_VAR` is the `char**` variable, pointing at the
// `char*[2]` array in `ARGV_ARRAY`. One extra level versus the array itself, matching
// what mingw `__getmainargs` dereferences once before storing the `argv` global
// (returning the array directly made the startup copy loop `strlen` the bytes of the
// program-name string as if they were pointers).
static ARGV_VAR: AtomicUsize = AtomicUsize::new(0);
static ARGV_ARRAY: AtomicUsize = AtomicUsize::new(0);
static WARGV_VAR: AtomicUsize = AtomicUsize::new(0);
static WARGV_ARRAY: AtomicUsize = AtomicUsize::new(0);
static ENVIRON_VAR: AtomicUsize = AtomicUsize::new(0);
static ENVIRON_ARRAY: AtomicUsize = AtomicUsize::new(0);
static WENVIRON_VAR: AtomicUsize = AtomicUsize::new(0);
static WENVIRON_ARRAY: AtomicUsize = AtomicUsize::new(0);
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

fn narrow_argv() -> *mut *mut *mut u8 {
    let array = cached_ptr(&ARGV_ARRAY, 16) as *mut *mut u8;
    unsafe {
        if (*array).is_null() {
            // Static placeholder program name; real argv wiring is a later slice.
            static NAME: &[u8] = b"xcl\0";
            *array = NAME.as_ptr() as *mut u8;
            *array.add(1) = std::ptr::null_mut();
        }
    }
    let var = cached_ptr(&ARGV_VAR, 8) as *mut *mut *mut u8;
    unsafe {
        if (*var).is_null() {
            *var = array;
        }
    }
    var
}

/// `char***` to the narrow argument vector (`argv[argc] == NULL`).
pub unsafe extern "win64" fn __p___argv() -> *mut *mut *mut u8 {
    narrow_argv()
}

fn wide_argv() -> *mut *mut *mut u16 {
    let array = cached_ptr(&WARGV_ARRAY, 16) as *mut *mut u16;
    unsafe {
        if (*array).is_null() {
            static NAME: &[u16] = &[b'x' as u16, b'c' as u16, b'l' as u16, 0];
            *array = NAME.as_ptr() as *mut u16;
            *array.add(1) = std::ptr::null_mut();
        }
    }
    let var = cached_ptr(&WARGV_VAR, 8) as *mut *mut *mut u16;
    unsafe {
        if (*var).is_null() {
            *var = array;
        }
    }
    var
}

/// `wchar_t***` to the wide argument vector.
pub unsafe extern "win64" fn __p___wargv() -> *mut *mut *mut u16 {
    wide_argv()
}

fn narrow_environ() -> *mut *mut *mut u8 {
    let array = cached_ptr(&ENVIRON_ARRAY, 8) as *mut *mut u8;
    unsafe {
        // Empty environment: a single NULL entry.
        *array = std::ptr::null_mut();
    }
    let var = cached_ptr(&ENVIRON_VAR, 8) as *mut *mut *mut u8;
    unsafe {
        if (*var).is_null() {
            *var = array;
        }
    }
    var
}

/// `char***` to the narrow environment block (currently empty).
pub unsafe extern "win64" fn __p__environ() -> *mut *mut *mut u8 {
    narrow_environ()
}

fn wide_environ() -> *mut *mut *mut u16 {
    let array = cached_ptr(&WENVIRON_ARRAY, 8) as *mut *mut u16;
    unsafe {
        *array = std::ptr::null_mut();
    }
    let var = cached_ptr(&WENVIRON_VAR, 8) as *mut *mut *mut u16;
    unsafe {
        if (*var).is_null() {
            *var = array;
        }
    }
    var
}

/// `wchar_t***` to the wide environment block (currently empty).
pub unsafe extern "win64" fn __p__wenviron() -> *mut *mut *mut u16 {
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
// Time / RNG / stdio mode (M1.1a): the auto-stubs returned epoch 0 and a constant.
// ---------------------------------------------------------------------------------------

/// `__time64_t _time64(__time64_t *destTime)` — Unix epoch seconds from the host
/// realtime clock, also mirrored through `destTime` when non-NULL. The title feeds the
/// return to `imul rax,0xf4240` to build a millisecond stamp; the auto-stub's epoch 0
/// made every derived timestamp 1970.
pub unsafe extern "win64" fn _time64(dest: *mut i64) -> i64 {
    let mut ts: libc::timespec = std::mem::zeroed();
    libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    let now = ts.tv_sec as i64;
    if !dest.is_null() {
        *dest = now;
    }
    now
}

/// `struct tm` as MSVC's `_gmtime64` returns it: nine `int` fields, no timezone
/// tail (the glibc layout is copied field by field, never reinterpreted).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Tm {
    pub sec: i32,
    pub min: i32,
    pub hour: i32,
    pub mday: i32,
    pub mon: i32,
    pub year: i32,
    pub wday: i32,
    pub yday: i32,
    pub isdst: i32,
}

thread_local! {
    /// Per-thread `struct tm` storage (the MSVC CRT contract: the caller's pointer
    /// stays valid until the next `_gmtime64` in the same thread).
    static GMTIME64_TM: std::cell::UnsafeCell<Tm> = std::cell::UnsafeCell::new(Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
    });
}

/// `struct tm *_gmtime64(const __time64_t *time)` — a real UTC breakdown of the
/// epoch value. Step 64: the title's TLS certificate loader validates the
/// certificate validity window with this; a null auto-stub return rejected the
/// shipped CA bundle and aborted the engine runtime init at ~7 s.
pub unsafe extern "win64" fn _gmtime64(time: *const i64) -> *mut Tm {
    if time.is_null() {
        set_win_errno(EINVAL_WIN);
        return std::ptr::null_mut();
    }
    let secs = *time;
    let mut lt: libc::tm = std::mem::zeroed();
    if libc::gmtime_r(&secs as *const i64 as *const libc::time_t, &mut lt).is_null() {
        set_win_errno(EINVAL_WIN);
        return std::ptr::null_mut();
    }
    GMTIME64_TM
        .try_with(|cell| {
            let out = cell.get();
            (*out).sec = lt.tm_sec;
            (*out).min = lt.tm_min;
            (*out).hour = lt.tm_hour;
            (*out).mday = lt.tm_mday;
            (*out).mon = lt.tm_mon;
            (*out).year = lt.tm_year;
            (*out).wday = lt.tm_wday;
            (*out).yday = lt.tm_yday;
            (*out).isdst = lt.tm_isdst;
            out
        })
        .unwrap_or(std::ptr::null_mut())
}

/// `errno_t _set_fmode(int mode)` — report success without applying `mode`: this HLE
/// has no backed CRT stdio translation, and the title only checks the return. The
/// `__p__fmode` cell keeps its zeroed default so a read-back stays stable.
pub unsafe extern "win64" fn _set_fmode(_mode: i32) -> i32 {
    0
}

/// `int _register_thread_local_exe_atexit_callback(void (__cdecl *)(void))` — accept
/// the pointer and report registered (`0`). XCL does not run the CRT's per-thread
/// teardown, so the callback is deliberately not stored or invoked; the contract test
/// pins that documented behaviour.
pub unsafe extern "win64" fn _register_thread_local_exe_atexit_callback(_cb: usize) -> i32 {
    0
}

/// SplitMix64 finalizer — the deterministic last resort for `_Random_device`.
fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// OS entropy for `std::_Random_device`: `getrandom(2)` first, `/dev/urandom` second.
fn os_entropy_u32() -> Option<u32> {
    let mut buf = [0u8; 4];
    let n = unsafe {
        libc::syscall(
            libc::SYS_getrandom,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len() as libc::size_t,
            0 as libc::c_uint,
        )
    };
    if n == buf.len() as libc::c_long {
        return Some(u32::from_ne_bytes(buf));
    }
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom").ok()?;
    f.read_exact(&mut buf).ok()?;
    Some(u32::from_ne_bytes(buf))
}

/// Last resort when neither entropy source answers: mix the monotonic clock, which
/// still advances between draws (never the auto-stub's constant 0).
fn entropy_fallback() -> u32 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let seed = (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64);
    (mix64(seed) as u32) ^ ((mix64(seed ^ 0x9e37_79b9_7f4a_7c15) >> 32) as u32)
}

/// `unsigned int ?_Random_device@std@@YAIXZ` (MSVC `std::_Random_device`) — a live OS
/// entropy draw, falling back to the monotonic-clock mix if both sources fail.
pub unsafe extern "win64" fn random_device_std() -> u32 {
    os_entropy_u32().unwrap_or_else(entropy_fallback)
}

// ---------------------------------------------------------------------------------------
// Narrow init / atexit / initterm (success codes, with real table walks).
// ---------------------------------------------------------------------------------------

/// `int _set_app_type(int)` — remember nothing, report the previous type (`0`).
pub unsafe extern "win64" fn _set_app_type(_t: i32) -> i32 {
    0
}

static PURECALL_HANDLER: AtomicUsize = AtomicUsize::new(0);
static TERMINATE_HANDLER: AtomicUsize = AtomicUsize::new(0);
static UNEXPECTED_HANDLER: AtomicUsize = AtomicUsize::new(0);
static INVALID_PARAM_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// `__acrt_thread_parameter* _set_invalid_parameter_handler(...)` — store the
/// handler, return the previous one (`0` = none). `msvcp140` `DllMain` installs
/// one during facet init; without storage a later `_invalid_parameter` call
/// would have nowhere to dispatch.
pub unsafe extern "win64" fn _set_invalid_parameter_handler(h: usize) -> usize {
    INVALID_PARAM_HANDLER.swap(h, Ordering::Relaxed)
}

/// `... _get_invalid_parameter_handler(void)` — current handler or `0`.
pub unsafe extern "win64" fn _get_invalid_parameter_handler() -> usize {
    INVALID_PARAM_HANDLER.load(Ordering::Relaxed)
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

/// `int _execute_onexit_table(...)` — run C++ terminators; nothing is queued in
/// this HLE (matching the `_register_onexit_function`/`_initialize_onexit_table`
/// successes above), so report success (`0`) without touching the table.
pub unsafe extern "win64" fn _execute_onexit_table(_table: usize) -> i32 {
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

// ---------------------------------------------------------------------------------------
// C++ termination / EH plumbing (purecall, Watson, terminate/unexpected, throw).
// Same NULL-stub fault class as memcpy/new: `msvcp140`/`vcruntime` `DllMain` and
// static initializers link these, and a logging stub that returns would resume into
// garbage. C++ semantics demand abort; the shims below log through `xlog` and abort
// via libc (or dispatch a stored handler first), never return a bogus code.
// ---------------------------------------------------------------------------------------

type VoidFn = unsafe extern "win64" fn();
type InvalidParamFn =
    unsafe extern "win64" fn(*const u16, *const u16, *const u16, u32, usize);

/// `void _purecall(void)` — pure-virtual call: run the installed handler if any,
/// else abnormal termination. Never returns.
pub unsafe extern "win64" fn _purecall() -> ! {
    let h = PURECALL_HANDLER.load(Ordering::Relaxed);
    if h != 0 {
        (std::mem::transmute::<usize, VoidFn>(h))();
    } else {
        crate::xlog_warn!("crt: _purecall with no handler — aborting");
    }
    libc::abort();
}

/// `_purecall_handler _set_purecall_handler(...)` — store, return previous.
pub unsafe extern "win64" fn _set_purecall_handler(h: usize) -> usize {
    PURECALL_HANDLER.swap(h, Ordering::Relaxed)
}

/// `_purecall_handler _get_purecall_handler(void)` — current handler or `0`.
pub unsafe extern "win64" fn _get_purecall_handler() -> usize {
    PURECALL_HANDLER.load(Ordering::Relaxed)
}

/// `void _invoke_watson(...)` — fatal CRT error: log and abort. Never returns.
pub unsafe extern "win64" fn _invoke_watson(
    _expr: *const u16,
    _func: *const u16,
    _file: *const u16,
    _line: u32,
    _reserved: usize,
) -> ! {
    crate::xlog_warn!("crt: _invoke_watson — aborting");
    libc::abort();
}

/// `void _invalid_parameter(...)` — call the installed handler if any, else no-op.
/// Returns `()` so facet init survives when no handler is installed.
pub unsafe extern "win64" fn _invalid_parameter(
    expr: *const u16,
    func: *const u16,
    file: *const u16,
    line: u32,
    reserved: usize,
) {
    let h = INVALID_PARAM_HANDLER.load(Ordering::Relaxed);
    if h != 0 {
        (std::mem::transmute::<usize, InvalidParamFn>(h))(expr, func, file, line, reserved);
    }
}

/// `void _invalid_parameter_noinfo(void)` — same handler, null source info.
pub unsafe extern "win64" fn _invalid_parameter_noinfo() {
    let h = INVALID_PARAM_HANDLER.load(Ordering::Relaxed);
    if h != 0 {
        (std::mem::transmute::<usize, InvalidParamFn>(h))(
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
        );
    }
}

/// `void terminate(void)` — run the installed handler if any, then abort.
pub unsafe extern "win64" fn terminate() -> ! {
    let h = TERMINATE_HANDLER.load(Ordering::Relaxed);
    if h != 0 {
        (std::mem::transmute::<usize, VoidFn>(h))();
    } else {
        crate::xlog_warn!("crt: terminate with no handler — aborting");
    }
    libc::abort();
}

/// `terminate_function _set_terminate(...)` — store, return previous.
pub unsafe extern "win64" fn _set_terminate(h: usize) -> usize {
    TERMINATE_HANDLER.swap(h, Ordering::Relaxed)
}

/// `terminate_function _get_terminate(void)` — current handler or `0`.
pub unsafe extern "win64" fn _get_terminate() -> usize {
    TERMINATE_HANDLER.load(Ordering::Relaxed)
}

/// `void unexpected(void)` — run the installed handler if any, then terminate.
pub unsafe extern "win64" fn unexpected() -> ! {
    let h = UNEXPECTED_HANDLER.load(Ordering::Relaxed);
    if h != 0 {
        (std::mem::transmute::<usize, VoidFn>(h))();
    }
    terminate();
}

/// `unexpected_function _set_unexpected(...)` — store, return previous.
pub unsafe extern "win64" fn _set_unexpected(h: usize) -> usize {
    UNEXPECTED_HANDLER.swap(h, Ordering::Relaxed)
}

/// `unexpected_function _get_unexpected(void)` — current handler or `0`.
pub unsafe extern "win64" fn _get_unexpected() -> usize {
    UNEXPECTED_HANDLER.load(Ordering::Relaxed)
}

/// `void _CxxThrowException(void*, _ThrowInfo*)` — real unwinding is a later
/// slice (`.pdata` translation); the honest short-term behavior is log-and-abort,
/// since returning would resume the throw site with no exception in flight.
pub unsafe extern "win64" fn _CxxThrowException(_obj: usize, _info: usize) -> ! {
    crate::xlog_warn!("crt: _CxxThrowException — C++ unwinding unimplemented, aborting");
    libc::abort();
}

/// `__CxxFrameHandler3` personality: no unwinding yet, so decline to handle
/// (`ExceptionContinueSearch` = 1) and let the search continue.
pub unsafe extern "win64" fn __CxxFrameHandler3() -> i32 {
    1
}

/// `__CxxFrameHandler4` personality: same as `_3` until unwinding lands.
pub unsafe extern "win64" fn __CxxFrameHandler4() -> i32 {
    1
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

// ---------------------------------------------------------------------------------------
// C++ operator new/delete (Phase C step-12c): msvcp140/vcruntime DllMain + game code
// allocate facets/exceptions through the mangled operators, not plain malloc. A logging
// stub returning NULL faults on first write, the same class as memcpy->NULL at
// msvcp140 DllMain+0x2efd. All forms forward to libc malloc/free; throwing forms abort
// on OOM like `malloc` above (a genuine bad_alloc throw needs EH plumbing, later).
// ---------------------------------------------------------------------------------------

fn scalar_new_bytes(n: usize) -> *mut c_void {
    unsafe {
        let p = libc::malloc(n.max(1)) as *mut c_void;
        assert!(!p.is_null(), "crt: out of memory");
        p
    }
}

/// `void* operator new(unsigned int)` (`??2@YAPAXI@Z`).
pub unsafe extern "win64" fn msvc_new_u32(n: u32) -> *mut c_void {
    scalar_new_bytes(n as usize)
}

/// `void* operator new(unsigned __int64)` (`??2@YAPEAX_K@Z`, x64 `size_t`).
pub unsafe extern "win64" fn msvc_new_u64(n: u64) -> *mut c_void {
    scalar_new_bytes(n as usize)
}

/// `void* operator new[](unsigned int)` (`??_U@YAPAXI@Z`).
pub unsafe extern "win64" fn msvc_new_array_u32(n: u32) -> *mut c_void {
    scalar_new_bytes(n as usize)
}

/// `void* operator new[](unsigned __int64)` (`??_U@YAPEAX_K@Z`).
pub unsafe extern "win64" fn msvc_new_array_u64(n: u64) -> *mut c_void {
    scalar_new_bytes(n as usize)
}

/// `void operator delete(void*)` (`??3@YAXPAX@Z` + `??3@YAXPEAX@Z`).
pub unsafe extern "win64" fn msvc_delete(p: *mut c_void) {
    if !p.is_null() {
        unsafe { libc::free(p as *mut libc::c_void) };
    }
}

/// `void operator delete[](void*)` (`??_V@YAXPAX@Z` + `??_V@YAXPEAX@Z`).
pub unsafe extern "win64" fn msvc_delete_array(p: *mut c_void) {
    if !p.is_null() {
        unsafe { libc::free(p as *mut libc::c_void) };
    }
}

/// `void operator delete(void*, unsigned __int64)` (sized delete).
pub unsafe extern "win64" fn msvc_delete_sized(p: *mut c_void, _n: u64) {
    if !p.is_null() {
        unsafe { libc::free(p as *mut libc::c_void) };
    }
}

/// `void operator delete[](void*, unsigned __int64)` (sized vector delete).
pub unsafe extern "win64" fn msvc_delete_array_sized(p: *mut c_void, _n: u64) {
    if !p.is_null() {
        unsafe { libc::free(p as *mut libc::c_void) };
    }
}

static NEWHANDLER: AtomicUsize = AtomicUsize::new(0);

/// `int _callnewh(size_t)` — invoke the installed new handler; `0` = none/failure.
pub unsafe extern "win64" fn _callnewh(_n: usize) -> i32 {
    let h = NEWHANDLER.load(Ordering::Relaxed);
    if h == 0 {
        return 0;
    }
    let f: unsafe extern "win64" fn(usize) -> i32 = unsafe { std::mem::transmute(h) };
    unsafe { f(_n) }
}

/// `_PNH _set_new_handler(_PNH)` — install, return previous (initially NULL).
pub unsafe extern "win64" fn _set_new_handler(h: usize) -> usize {
    NEWHANDLER.swap(h, Ordering::Relaxed)
}

/// `_PNH _query_new_handler(void)` — current handler or NULL.
pub unsafe extern "win64" fn _query_new_handler() -> usize {
    NEWHANDLER.load(Ordering::Relaxed)
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

/// `int strcmp(a, b)` — unsigned-byte order, `0` when equal.
pub unsafe extern "win64" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0;
    loop {
        let x = *a.add(i);
        let y = *b.add(i);
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}

/// `int strncmp(a, b, n)` — bounded `strcmp`, `0` when the first `n` bytes match.
pub unsafe extern "win64" fn strncmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut result = 0;
    for i in 0..n {
        let x = *a.add(i);
        let y = *b.add(i);
        if x != y {
            result = x as i32 - y as i32;
            break;
        }
        if x == 0 {
            break;
        }
    }
    trace_strncmp(a, b, n, result);
    result
}

/// `XCL_TRACE_STRNCMP`: log each `strncmp`/`_strnicmp` call's bounded arg
/// previews and result (retail M1 step-04: the `0x140968480` dispatcher's key
/// probes reach this shim, so the log names the keys the game looks up on the
/// abort path; step-05: full inputs + `_strnicmp` coverage, see below).
/// Off by default; capped so a boot flood stays a bounded xcl.log tail.
fn strncmp_tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XCL_TRACE_STRNCMP").is_some())
}

/// Preview cap: at most this many bytes per arg, and at most this many logged
/// calls per process (atomic counter, first-come).
const STRNCMP_PREVIEW_MAX: usize = 64;
const STRNCMP_LOG_MAX: usize = 512;
static STRNCMP_LOGGED: AtomicUsize = AtomicUsize::new(0);
static STRNICMP_LOGGED: AtomicUsize = AtomicUsize::new(0);

/// Full-input cap (retail M1 step-05): the dispatcher's `n` only covers the
/// scheme prefix, so the step-04 previews truncated the value that matters
/// (`G:/app…`, `packro…`). The probe also logs the whole NUL-terminated input
/// up to this cap. Reads past the `n` bytes `strncmp` itself compares — sound
/// only for the NUL-terminated inputs the title passes (the dispatcher runs a
/// `strlen`-like first; the Havok-style keys are C strings) — and runs only
/// when the triage env var is set (default-off path never reads past `n`).
const STRNCMP_FULL_MAX: usize = 256;

/// Full NUL-terminated, lossy input string, capped at [`STRNCMP_FULL_MAX`].
fn strncmp_cstr_full(s: *const u8) -> String {
    let mut buf = Vec::with_capacity(64);
    for i in 0..STRNCMP_FULL_MAX {
        // SAFETY: triage-only (see `STRNCMP_FULL_MAX`); stops at the first NUL.
        let c = unsafe { *s.add(i) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Game call-site attribution for the trace below is done statically, by
/// `(n, key)` (retail M1 step-05): every dispatcher key maps to a unique
/// `lea rdx,[rip+…]` site in the disassembly except `tcpip:` (two sites: the
/// `0x14096853b` dispatcher probe and the `0x1428c63c0` strip-prefix helper).
/// A first revision tried to log the caller by scanning stack words for
/// mapped-image addresses, but two live retail runs proved that misattributes:
/// the win64 home space spills the key pointer onto our stack ahead of any
/// return address, and HLE trampoline frames add more data words, so the
/// "caller" was a `.rdata`/`.data` operand, not code. Real unwind PCs would
/// need `Backtrace::frames` (unstable on this toolchain) or a new unwind
/// dependency for a default-off probe — not worth it when the static mapping
/// already answers which probe examined which input.

/// Bounded, NUL-terminated, lossy preview of a `strncmp` arg: reads at most
/// `min(n, STRNCMP_PREVIEW_MAX)` bytes — never more than `strncmp` itself
/// reads — and stops at the first NUL.
fn strncmp_arg_preview(s: *const u8, n: usize) -> String {
    let take = n.min(STRNCMP_PREVIEW_MAX);
    let mut buf = Vec::with_capacity(take);
    for i in 0..take {
        // SAFETY: caller guarantees `s` is valid for the `n` bytes the
        // `strncmp(a, b, n)` call itself reads; this reads a prefix of that.
        let c = unsafe { *s.add(i) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn trace_strncmp(a: *const u8, b: *const u8, n: usize, result: i32) {
    if !strncmp_tracing() {
        return;
    }
    if STRNCMP_LOGGED.fetch_add(1, Ordering::Relaxed) >= STRNCMP_LOG_MAX {
        return;
    }
    let pa = strncmp_arg_preview(a, n);
    let pb = strncmp_arg_preview(b, n);
    let fa = strncmp_cstr_full(a);
    let fb = strncmp_cstr_full(b);
    crate::xlog_info!("[STRNCMP] n={n} a={pa:?} a_full={fa:?} b={pb:?} b_full={fb:?} -> {result}");
}

/// `int _strnicmp(a, b, n)` — bounded case-insensitive `strcmp` (ASCII fold),
/// `0` when the first `n` bytes match ignoring case (retail abort 0x2237179:
/// the drive-prefix probe at `exe+0x54F8A70` compares `G:/...` against `xg:` /
/// `xd:`; the auto-stub always reported "equal", which stripped the drive
/// letter and broke the dump-path lookup in `sub_14056EB30`).
/// Traced under the same `XCL_TRACE_STRNCMP` gate (retail M1 step-05): the
/// `0x140968480` dispatcher issues its `t:`/`g:`/`R:`-style probes through
/// this slot (`0x1467947b8`), so without it the dispatcher's input source is
/// half-invisible.
pub unsafe extern "win64" fn _strnicmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut result = 0;
    for i in 0..n {
        let mut x = *a.add(i);
        let mut y = *b.add(i);
        if x.is_ascii_uppercase() {
            x += 32;
        }
        if y.is_ascii_uppercase() {
            y += 32;
        }
        if x != y {
            result = x as i32 - y as i32;
            break;
        }
        if x == 0 {
            break;
        }
    }
    trace_strnicmp(a, b, n, result);
    result
}

fn trace_strnicmp(a: *const u8, b: *const u8, n: usize, result: i32) {
    if !strncmp_tracing() {
        return;
    }
    if STRNICMP_LOGGED.fetch_add(1, Ordering::Relaxed) >= STRNCMP_LOG_MAX {
        return;
    }
    let pa = strncmp_arg_preview(a, n);
    let pb = strncmp_arg_preview(b, n);
    let fa = strncmp_cstr_full(a);
    let fb = strncmp_cstr_full(b);
    crate::xlog_info!("[STRNICMP] n={n} a={pa:?} a_full={fa:?} b={pb:?} b_full={fb:?} -> {result}");
}

// ---------------------------------------------------------------------------------------
// Classify/parse/utility pack (retail Step 38): the boot path's language/config init
// calls these through `api-ms-win-crt-*` imports. With no HLE they were auto-stubs:
// `isspace` always 0 (204 calls, consumers branch on it), `atoi` always 0, `_stricmp`
// always 0 ("equal"), `srand` a no-op, `qsort` a no-op that left arrays unsorted.
// Each is the C-locale UCRT contract, no locale backend.
// ---------------------------------------------------------------------------------------

/// C-locale whitespace: HT(0x09)–CR(0x0D) and space. `EOF`/non-bytes are not space.
#[inline]
fn is_c_space(c: i32) -> bool {
    matches!(c, 0x09..=0x0D | 0x20)
}

/// `int isspace(int c)` — nonzero for C-locale whitespace, `0` otherwise.
pub unsafe extern "win64" fn isspace(c: i32) -> i32 {
    is_c_space(c) as i32
}

/// `int isalnum(int c)` — nonzero for ASCII digits and letters (C locale).
pub unsafe extern "win64" fn isalnum(c: i32) -> i32 {
    matches!(c, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A) as i32
}

// ---------------------------------------------------------------------------------------
// C-locale ctype table and locale locks (the shipped msvcp140 DllMain)
// ---------------------------------------------------------------------------------------
//
// Reactivating the real `msvcp140.dll` (Step 62) makes its DllMain build its
// `ctype<char>` table from UCRT's `__pctype_func`: it copies the 256 `unsigned short`
// entries. The same DllMain imports the locale locks and the code page. The C
// locale's classifications are fixed, so the table is a static; the locale locks
// have nothing to protect (the table never changes).

const CTYPE_UPPER: u16 = 0x1;
const CTYPE_LOWER: u16 = 0x2;
const CTYPE_DIGIT: u16 = 0x4;
const CTYPE_SPACE: u16 = 0x8;
const CTYPE_PUNCT: u16 = 0x10;
const CTYPE_CONTROL: u16 = 0x20;
const CTYPE_BLANK: u16 = 0x40;
const CTYPE_HEX: u16 = 0x80;
const CTYPE_ALPHA: u16 = 0x100;

fn c_locale_ctype_table() -> &'static [u16; 256] {
    static TABLE: std::sync::LazyLock<[u16; 256]> = std::sync::LazyLock::new(|| {
        let mut t = [0u16; 256];
        for (b, slot) in t.iter_mut().enumerate() {
            *slot = match b as u8 {
                0x00..=0x08 | 0x0E..=0x1F | 0x7F => CTYPE_CONTROL,
                0x09..=0x0D => CTYPE_CONTROL | CTYPE_SPACE,
                0x20 => CTYPE_SPACE | CTYPE_BLANK,
                0x21..=0x2F | 0x3A..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E => CTYPE_PUNCT,
                0x30..=0x39 => CTYPE_DIGIT | CTYPE_HEX,
                0x41..=0x46 | 0x61..=0x66 => {
                    let letter = if (b as u8).is_ascii_uppercase() {
                        CTYPE_UPPER
                    } else {
                        CTYPE_LOWER
                    };
                    letter | CTYPE_HEX | CTYPE_ALPHA
                }
                0x47..=0x5A => CTYPE_UPPER | CTYPE_ALPHA,
                0x67..=0x7A => CTYPE_LOWER | CTYPE_ALPHA,
                // The C locale does not classify bytes above ASCII.
                _ => 0,
            };
        }
        t
    });
    &TABLE
}

/// `unsigned short* __pctype_func(void)` — the C-locale classification table.
pub unsafe extern "win64" fn __pctype_func() -> *const u16 {
    c_locale_ctype_table().as_ptr()
}

/// `unsigned int ___lc_codepage_func(void)` — XCL's ANSI code page is UTF-8, which the
/// conversion helpers spell as `CP_ACP` (`0`); a real Windows C locale would say 1252,
/// but 1252 is not an XCL conversion code page.
pub unsafe extern "win64" fn ___lc_codepage_func() -> u32 {
    0
}

/// `void _lock_locales(void)` — the C-locale tables are immutable.
pub unsafe extern "win64" fn _lock_locales() {}

/// `void _unlock_locales(void)` — see `_lock_locales`.
pub unsafe extern "win64" fn _unlock_locales() {}

/// `int ___mb_cur_max_func(void)` — the C locale is single-byte.
pub unsafe extern "win64" fn ___mb_cur_max_func() -> i32 {
    1
}

/// The C-locale names UCRT keeps per category (`LC_ALL` … `LC_TIME`, 6 entries).
/// The table is `usize` so the static stays `Sync`; the guest sees the same
/// pointer-sized entries.
static C_LOCALE_NAME: [u8; 2] = *b"C\0";

fn c_locale_names() -> &'static [usize; 6] {
    static NAMES: std::sync::LazyLock<[usize; 6]> = std::sync::LazyLock::new(|| {
        [C_LOCALE_NAME.as_ptr() as usize; 6]
    });
    &NAMES
}

/// `const char** ___lc_locale_name_func(void)` — the per-category C-locale names.
pub unsafe extern "win64" fn ___lc_locale_name_func() -> *const *const u8 {
    c_locale_names().as_ptr() as *const *const u8
}

/// MSVC's LC_* categories (`LC_ALL` … `LC_TIME`).
const LC_ALL: i32 = 0;
const LC_TIME: i32 = 5;

fn c_locale_matches(locale: *const u8) -> bool {
    // `""` selects the environment, which XCL keeps at "C"; "C" and "POSIX" are
    // the portable names for that locale.
    let mut i = 0usize;
    unsafe {
        if *locale.add(0) == 0 {
            return true;
        }
        let mut buf = [0u8; 8];
        while i < buf.len() && *locale.add(i) != 0 {
            buf[i] = *locale.add(i);
            i += 1;
        }
        if *locale.add(i) != 0 {
            return false;
        }
        let name = &buf[..i];
        name.eq_ignore_ascii_case(b"C") || name.eq_ignore_ascii_case(b"POSIX")
    }
}

/// `char* setlocale(int category, const char* locale)` — C locale only.
pub unsafe extern "win64" fn setlocale(category: i32, locale: *const u8) -> *const u8 {
    if !(LC_ALL..=LC_TIME).contains(&category) {
        return std::ptr::null();
    }
    if !locale.is_null() && !c_locale_matches(locale) {
        return std::ptr::null();
    }
    b"C\0".as_ptr()
}

/// Byte-wise ASCII fold of the C locale: `A`–`Z` only.
#[inline]
fn ascii_fold(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// `int _stricmp(a, b)` — unbounded case-insensitive compare (ASCII fold), the
/// difference of the first differing pair folded to lower case, `0` when equal.
/// A null pointer reads as the empty string so a broken argument cannot fault.
pub unsafe extern "win64" fn _stricmp(a: *const u8, b: *const u8) -> i32 {
    let mut i = 0usize;
    loop {
        let x = if a.is_null() { 0 } else { *a.add(i) };
        let y = if b.is_null() { 0 } else { *b.add(i) };
        let fx = ascii_fold(x);
        let fy = ascii_fold(y);
        if fx != fy {
            return fx as i32 - fy as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}

/// `int atoi(const char*)` — C-locale leading whitespace, optional sign, decimal
/// digits, stop at the first non-digit. Out-of-range values clamp to `i32::MIN`/
/// `i32::MAX` (the C contract leaves overflow undefined; a deterministic clamp is
/// better than a wrap the title would branch on). Null reads as the empty string.
pub unsafe extern "win64" fn atoi(s: *const u8) -> i32 {
    let mut v: i64 = 0;
    let mut neg = false;
    if !s.is_null() {
        let mut p = s;
        while is_c_space(*p as i32) {
            p = p.add(1);
        }
        if *p == b'+' {
            p = p.add(1);
        } else if *p == b'-' {
            neg = true;
            p = p.add(1);
        }
        while (*p).is_ascii_digit() {
            v = v.saturating_mul(10).saturating_add((*p - b'0') as i64);
            p = p.add(1);
        }
    }
    let v = if neg { -v } else { v };
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Length of the C string at `p` up to (excluding) its NUL terminator.
unsafe fn c_str_len(p: *const u8) -> usize {
    let mut n = 0usize;
    while *p.add(n) != 0 {
        n += 1;
    }
    n
}

/// Case-insensitive ASCII match of the lowercase `word` at byte `at`.
fn matches_ci(input: &[u8], at: usize, word: &[u8]) -> bool {
    input.len() >= at + word.len()
        && input[at..at + word.len()].iter().zip(word).all(|(a, b)| ascii_fold(*a) == *b)
}

/// C-locale `strtod` subject sequence: `(bytes consumed after the caller's
/// pointer, value, out-of-range)`. Zero bytes means no conversion, in which
/// case `endptr` must stay at the caller's pointer (not after leading space or
/// a sign). Out-of-range is overflow to infinity or underflow to zero from a
/// nonzero mantissa; `%a`'s `parse_hex_float` supplies the hex form.
fn scan_strtod(input: &[u8]) -> (usize, f64, bool) {
    let len = input.len();
    let mut i = 0usize;
    while i < len && is_c_space(input[i] as i32) {
        i += 1;
    }
    let start = i;
    let mut j = i;
    let mut neg = false;
    if j < len && (input[j] == b'+' || input[j] == b'-') {
        neg = input[j] == b'-';
        j += 1;
    }
    if matches_ci(input, j, b"infinity") || matches_ci(input, j, b"inf") {
        let n = if matches_ci(input, j, b"infinity") { 8 } else { 3 };
        let v = if neg { f64::NEG_INFINITY } else { f64::INFINITY };
        return (j + n, v, false);
    }
    if matches_ci(input, j, b"nan") {
        let mut k = j + 3;
        if k < len && input[k] == b'(' {
            let mut m = k + 1;
            while m < len && (input[m].is_ascii_alphanumeric() || input[m] == b'_') {
                m += 1;
            }
            if m < len && input[m] == b')' {
                k = m + 1;
            }
        }
        return (k, f64::NAN, false);
    }
    // Hexadecimal constant. An incomplete prefix (`0x`, `0x.`) is not part of
    // the subject sequence, so the decimal fallback below consumes just `0`.
    if j + 1 < len && input[j] == b'0' && matches!(input[j + 1], b'x' | b'X') {
        let mut k = j + 2;
        let mut digits = 0usize;
        let mut nonzero = false;
        while k < len && input[k].is_ascii_hexdigit() {
            nonzero |= input[k] != b'0';
            k += 1;
            digits += 1;
        }
        if k < len && input[k] == b'.' {
            let mut m = k + 1;
            let mut frac = 0usize;
            while m < len && input[m].is_ascii_hexdigit() {
                nonzero |= input[m] != b'0';
                m += 1;
                frac += 1;
            }
            if digits + frac > 0 {
                k = m;
                digits += frac;
            }
        }
        if digits > 0 {
            if k < len && matches!(input[k], b'p' | b'P') {
                let mut m = k + 1;
                if m < len && (input[m] == b'+' || input[m] == b'-') {
                    m += 1;
                }
                let first = m;
                while m < len && input[m].is_ascii_digit() {
                    m += 1;
                }
                if m > first {
                    k = m;
                }
            }
            let text = unsafe { std::str::from_utf8_unchecked(&input[j..k]) };
            let v = parse_hex_float(text).unwrap_or(0.0);
            let v = if neg { -v } else { v };
            return (k, v, v.is_infinite() || (v == 0.0 && nonzero));
        }
    }
    let mut k = j;
    let mut digits = 0usize;
    let mut nonzero = false;
    while k < len && input[k].is_ascii_digit() {
        nonzero |= input[k] != b'0';
        k += 1;
        digits += 1;
    }
    if k < len && input[k] == b'.' {
        let mut m = k + 1;
        let mut frac = 0usize;
        while m < len && input[m].is_ascii_digit() {
            nonzero |= input[m] != b'0';
            m += 1;
            frac += 1;
        }
        if digits + frac > 0 {
            k = m;
            digits += frac;
        }
    }
    if digits == 0 {
        return (0, 0.0, false);
    }
    if k < len && matches!(input[k], b'e' | b'E') {
        let mut m = k + 1;
        if m < len && (input[m] == b'+' || input[m] == b'-') {
            m += 1;
        }
        let first = m;
        while m < len && input[m].is_ascii_digit() {
            m += 1;
        }
        if m > first {
            k = m;
        }
    }
    let text = unsafe { std::str::from_utf8_unchecked(&input[start..k]) };
    let v = text.parse::<f64>().unwrap_or(0.0);
    (k, v, v.is_infinite() || (v == 0.0 && nonzero))
}

/// C-locale integer subject sequence with `base` already validated:
/// `(bytes consumed after the caller's pointer, negative, magnitude, magnitude
/// exceeds `u64`)`. Base 0 auto-detects `0x`/leading `0`; an explicit base 16
/// also accepts (but does not require) the `0x` prefix. No digits consumes
/// nothing. The magnitude stays exact in `u128` so the signed and unsigned
/// consumers apply their own limits.
fn scan_int_magnitude(input: &[u8], base: i32) -> (usize, bool, u128, bool) {
    let len = input.len();
    let mut i = 0usize;
    while i < len && is_c_space(input[i] as i32) {
        i += 1;
    }
    let mut j = i;
    let mut neg = false;
    if j < len && (input[j] == b'+' || input[j] == b'-') {
        neg = input[j] == b'-';
        j += 1;
    }
    let (used, mag, big) = if base == 0 {
        if j + 2 < len && input[j] == b'0' && matches!(input[j + 1], b'x' | b'X') && input[j + 2].is_ascii_hexdigit() {
            j += 2;
            scan_int_digits(input, j, 16)
        } else {
            let radix = if j < len && input[j] == b'0' { 8 } else { 10 };
            scan_int_digits(input, j, radix)
        }
    } else if base == 16 && j + 2 < len && input[j] == b'0' && matches!(input[j + 1], b'x' | b'X') && input[j + 2].is_ascii_hexdigit() {
        scan_int_digits(input, j + 2, 16)
    } else {
        scan_int_digits(input, j, base as u32)
    };
    (used, neg, mag, big)
}

/// `(bytes consumed after the caller's pointer, magnitude, exceeds `u64`)` for
/// one run of digits in `radix` starting at `start`.
fn scan_int_digits(input: &[u8], start: usize, radix: u32) -> (usize, u128, bool) {
    let len = input.len();
    let mut j = start;
    let mut mag: u128 = 0;
    let mut overflow = false;
    while j < len {
        let d = match input[j] {
            b'0'..=b'9' => (input[j] - b'0') as u128,
            b'a'..=b'z' => (input[j] - b'a' + 10) as u128,
            b'A'..=b'Z' => (input[j] - b'A' + 10) as u128,
            _ => break,
        };
        if d >= radix as u128 {
            break;
        }
        mag = mag.saturating_mul(radix as u128).saturating_add(d);
        overflow |= mag > u64::MAX as u128;
        j += 1;
    }
    if j == start {
        return (0, 0, false);
    }
    (j, mag, overflow)
}

/// C-locale `strtoui64` subject sequence: `(bytes consumed after the caller's
/// pointer, value, overflow)`. A minus sign negates modulo 2^64; overflow
/// returns `u64::MAX` and reports the range error.
fn scan_strtoull(input: &[u8], base: i32) -> (usize, u64, bool) {
    let (used, neg, mag, big) = scan_int_magnitude(input, base);
    if used == 0 {
        return (0, 0, false);
    }
    if big {
        return (used, u64::MAX, true);
    }
    let v = mag as u64;
    (used, if neg { v.wrapping_neg() } else { v }, false)
}

/// `double strtod(const char *nptr, char **endptr)` — C-locale parse of the
/// longest valid prefix (decimal, hex, `inf`, `nan`), with `*endptr` advanced
/// past it. A null `nptr` reads as an empty string; a null `endptr` is ignored.
pub unsafe extern "win64" fn strtod(nptr: *const u8, endptr: *mut *mut u8) -> f64 {
    if nptr.is_null() {
        if !endptr.is_null() {
            *endptr = std::ptr::null_mut();
        }
        return 0.0;
    }
    let input = std::slice::from_raw_parts(nptr, c_str_len(nptr));
    let (used, v, out_of_range) = scan_strtod(input);
    if !endptr.is_null() {
        *endptr = nptr.add(used) as *mut u8;
    }
    if out_of_range {
        set_win_errno(ERANGE_WIN);
    }
    v
}

/// `unsigned long long _strtoui64(const char *nptr, char **endptr, int base)` —
/// UCRT unsigned parse (`base` 0 or 2–36), `*endptr` at the first unconsumed
/// byte, `ULLONG_MAX` + `ERANGE` on overflow, a minus sign negating modulo 2^64.
pub unsafe extern "win64" fn _strtoui64(nptr: *const u8, endptr: *mut *mut u8, base: i32) -> u64 {
    if nptr.is_null() {
        if !endptr.is_null() {
            *endptr = std::ptr::null_mut();
        }
        return 0;
    }
    if base != 0 && !(2..=36).contains(&base) {
        if !endptr.is_null() {
            *endptr = nptr as *mut u8;
        }
        set_win_errno(EINVAL_WIN);
        return 0;
    }
    let input = std::slice::from_raw_parts(nptr, c_str_len(nptr));
    let (used, v, overflow) = scan_strtoull(input, base);
    if !endptr.is_null() {
        *endptr = nptr.add(used) as *mut u8;
    }
    if overflow {
        set_win_errno(ERANGE_WIN);
    }
    v
}

/// `unsigned long long strtoull(const char *nptr, char **endptr, int base)` —
/// UCRT unsigned parse, identical to `_strtoui64` under the standard name.
pub unsafe extern "win64" fn strtoull(nptr: *const u8, endptr: *mut *mut u8, base: i32) -> u64 {
    _strtoui64(nptr, endptr, base)
}

/// `long long strtoll(const char *nptr, char **endptr, int base)` — UCRT
/// signed parse (`base` 0 or 2–36), `*endptr` at the first unconsumed byte,
/// `LLONG_MAX`/`LLONG_MIN` + `ERANGE` outside the signed range, `EINVAL` and a
/// `nptr` `endptr` for a bad base.
pub unsafe extern "win64" fn strtoll(nptr: *const u8, endptr: *mut *mut u8, base: i32) -> i64 {
    if nptr.is_null() {
        if !endptr.is_null() {
            *endptr = std::ptr::null_mut();
        }
        return 0;
    }
    if base != 0 && !(2..=36).contains(&base) {
        if !endptr.is_null() {
            *endptr = nptr as *mut u8;
        }
        set_win_errno(EINVAL_WIN);
        return 0;
    }
    let input = std::slice::from_raw_parts(nptr, c_str_len(nptr));
    let (used, neg, mag, big) = scan_int_magnitude(input, base);
    if !endptr.is_null() {
        *endptr = nptr.add(used) as *mut u8;
    }
    if used == 0 {
        return 0;
    }
    let limit = if neg { 1u128 << 63 } else { i64::MAX as u128 };
    if big || mag > limit {
        set_win_errno(ERANGE_WIN);
        return if neg { i64::MIN } else { i64::MAX };
    }
    let v = mag as i64;
    if neg { v.wrapping_neg() } else { v }
}

/// `errno_t _set_errno(int value)` — store `value` in the calling thread's
/// `errno`; UCRT reports success for any value.
pub unsafe extern "win64" fn _set_errno(value: i32) -> i32 {
    *_errno() = value;
    0
}

/// `errno_t _get_errno(int *value)` — copy the calling thread's `errno` out.
/// A null `value` is `EINVAL` and writes nothing.
pub unsafe extern "win64" fn _get_errno(value: *mut i32) -> i32 {
    if value.is_null() {
        return EINVAL_WIN;
    }
    *value = *_errno();
    0
}

/// UCRT `rand` state (`srand` seeds it; the algorithm is MSVC's multiplicative
/// generator, result `(state >> 16) & 0x7FFF`). The title imports both names, so a
/// real `srand` needs the matching `rand` to be observable.
static RAND_STATE: AtomicU32 = AtomicU32::new(1);

/// `void srand(unsigned int seed)` — seed the process-wide UCRT sequence.
pub unsafe extern "win64" fn srand(seed: u32) {
    RAND_STATE.store(seed, Ordering::Relaxed);
}

/// `int rand(void)` — the UCRT sequence: `0..=RAND_MAX(0x7FFF)`.
pub unsafe extern "win64" fn rand() -> i32 {
    let mut cur = RAND_STATE.load(Ordering::Relaxed);
    loop {
        let next = cur.wrapping_mul(214_013).wrapping_add(2_531_011);
        match RAND_STATE.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ((next >> 16) & 0x7FFF) as i32,
            Err(actual) => cur = actual,
        }
    }
}

type QsortCmp = unsafe extern "win64" fn(*const u8, *const u8) -> i32;

unsafe fn qsort_lt(base: *mut u8, width: usize, a: usize, b: usize, cmp: QsortCmp) -> bool {
    cmp(base.add(a * width), base.add(b * width)) < 0
}

/// Sift the root of the max-heap at `root` down within `[0, end)`, swapping whole
/// elements (the comparator sees element pointers, as the C contract implies).
unsafe fn qsort_sift_down(base: *mut u8, width: usize, mut root: usize, end: usize, cmp: QsortCmp) {
    loop {
        let mut child = 2 * root + 1;
        if child >= end {
            break;
        }
        if child + 1 < end && qsort_lt(base, width, child, child + 1, cmp) {
            child += 1;
        }
        if qsort_lt(base, width, root, child, cmp) {
            std::ptr::swap_nonoverlapping(base.add(root * width), base.add(child * width), width);
            root = child;
        } else {
            break;
        }
    }
}

/// `void qsort(void* base, size_t num, size_t width, int (*compare)(const void*, const void*))`
/// — in-place heapsort with guest whole-element swaps (`O(n log n)`, no allocation,
/// no Rust comparator panics across the FFI boundary). No-op for `<2` elements,
/// a null base/comparator, or a zero/overflowing element count.
pub unsafe extern "win64" fn qsort(base: *mut u8, num: usize, width: usize, compare: usize) {
    if base.is_null() || compare == 0 || width == 0 || num < 2 {
        return;
    }
    if num.checked_mul(width).is_none() {
        return;
    }
    let cmp: QsortCmp = std::mem::transmute(compare);
    for root in (0..num / 2).rev() {
        qsort_sift_down(base, width, root, num, cmp);
    }
    for end in (1..num).rev() {
        std::ptr::swap_nonoverlapping(base, base.add(end * width), width);
        qsort_sift_down(base, width, 0, end, cmp);
    }
}

/// `int wcscmp(a, b)` — UTF-16 unit order, `0` when equal.
pub unsafe extern "win64" fn wcscmp(a: *const u16, b: *const u16) -> i32 {
    let mut i = 0;
    loop {
        let x = *a.add(i);
        let y = *b.add(i);
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}

/// `int wcsncmp(a, b, n)` — bounded `wcscmp`.
pub unsafe extern "win64" fn wcsncmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    for i in 0..n {
        let x = *a.add(i);
        let y = *b.add(i);
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
    }
    0
}

/// `char* strchr(s, c)` — first `c` (as `u8`) in `s` including the NUL; NULL when absent.
pub unsafe extern "win64" fn strchr(s: *const u8, c: i32) -> *mut u8 {
    let target = c as u8;
    let mut i = 0;
    loop {
        let x = *s.add(i);
        if x == target {
            return s.add(i) as *mut u8;
        }
        if x == 0 {
            return std::ptr::null_mut();
        }
        i += 1;
    }
}

/// `void* memchr(s, c, n)` — first `c` in the first `n` bytes; NULL when absent.
pub unsafe extern "win64" fn memchr(s: *const u8, c: i32, n: usize) -> *mut u8 {
    let target = c as u8;
    for i in 0..n {
        if *s.add(i) == target {
            return s.add(i) as *mut u8;
        }
    }
    std::ptr::null_mut()
}

// ---------------------------------------------------------------------------------------
// String copy/concat (Phase C step-12a): DllMain/facet init `strcpy`/`strcat` family.
// Same NULL-stub fault class as memcpy/compare: a logging stub returning NULL would
// fault on the first write/read, so these return `dst` (or `dst` + NUL handling)
// exactly like UCRT. Narrow ops are byte loops, wide ops are UTF-16 unit loops.
// `strncpy` pads with NUL to `n`; `strncat`/`wcsncat` append at most `n` units then
// NUL-terminate. `wmemcpy`/`wmemmove` copy `n` wide units (`wmemmove` is
// overlap-safe); zero lengths touch nothing but still return `dst`.
// ---------------------------------------------------------------------------------------

/// `char* strcpy(dst, src)` — copy `src` (incl. NUL), return `dst`.
pub unsafe extern "win64" fn strcpy(dst: *mut u8, src: *const u8) -> *mut u8 {
    let mut i = 0;
    loop {
        let b = *src.add(i);
        *dst.add(i) = b;
        if b == 0 {
            break;
        }
        i += 1;
    }
    dst
}

/// `char* strncpy(dst, src, n)` — copy at most `n` bytes, NUL-pad to `n`, return `dst`.
/// Truncation (src longer than `n`) leaves `dst[..n]` unterminated, as on Windows.
pub unsafe extern "win64" fn strncpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        let b = *src.add(i);
        *dst.add(i) = b;
        i += 1;
        if b == 0 {
            break;
        }
    }
    while i < n {
        *dst.add(i) = 0;
        i += 1;
    }
    dst
}

/// `errno_t strncpy_s(dest, destsz, src, count)` — bounded copy that always
/// NUL-terminates when it writes (MSVC Annex-K rules, retail Step 79):
/// a null `dest`, a zero `destsz`, a null `src`, or a source that cannot fit
/// in `destsz` writes `dest[0] = 0` where possible, sets `errno = EINVAL`
/// and returns `EINVAL`; `count == 0` writes the terminator only; otherwise
/// it copies `min(count, strlen(src))` bytes and the terminator, so
/// `count < destsz` truncates instead of overflowing. Never writes past
/// `destsz`, and never pads the tail (unlike `strncpy`).
pub unsafe extern "win64" fn strncpy_s(
    dest: *mut u8,
    destsz: usize,
    src: *const u8,
    count: usize,
) -> i32 {
    if dest.is_null() || destsz == 0 {
        set_win_errno(EINVAL_WIN);
        return EINVAL_WIN;
    }
    if count == 0 {
        *dest = 0;
        return 0;
    }
    if src.is_null() {
        *dest = 0;
        set_win_errno(EINVAL_WIN);
        return EINVAL_WIN;
    }
    if count >= destsz {
        // The whole source (not just `count`) must fit with its terminator.
        let len = strnlen(src, destsz);
        if len == destsz {
            *dest = 0;
            set_win_errno(EINVAL_WIN);
            return EINVAL_WIN;
        }
        std::ptr::copy_nonoverlapping(src, dest, len);
        *dest.add(len) = 0;
        return 0;
    }
    let len = strnlen(src, count);
    std::ptr::copy_nonoverlapping(src, dest, len);
    *dest.add(len) = 0;
    0
}

/// `char* strcat(dst, src)` — append `src` (incl. NUL) after `dst`'s NUL, return `dst`.
pub unsafe extern "win64" fn strcat(dst: *mut u8, src: *const u8) -> *mut u8 {
    let mut d = 0;
    while *dst.add(d) != 0 {
        d += 1;
    }
    let mut i = 0;
    loop {
        let b = *src.add(i);
        *dst.add(d + i) = b;
        if b == 0 {
            break;
        }
        i += 1;
    }
    dst
}

/// `char* strncat(dst, src, n)` — append at most `n` bytes of `src` plus NUL.
pub unsafe extern "win64" fn strncat(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut d = 0;
    while *dst.add(d) != 0 {
        d += 1;
    }
    let mut i = 0;
    while i < n {
        let b = *src.add(i);
        if b == 0 {
            break;
        }
        *dst.add(d + i) = b;
        i += 1;
    }
    *dst.add(d + i) = 0;
    dst
}

/// `wchar_t* wcscpy(dst, src)` — copy wide `src` (incl. NUL), return `dst`.
pub unsafe extern "win64" fn wcscpy(dst: *mut u16, src: *const u16) -> *mut u16 {
    let mut i = 0;
    loop {
        let w = *src.add(i);
        *dst.add(i) = w;
        if w == 0 {
            break;
        }
        i += 1;
    }
    dst
}

/// `wchar_t* wcsncpy(dst, src, n)` — copy at most `n` wide units, NUL-pad to `n`.
pub unsafe extern "win64" fn wcsncpy(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 {
    let mut i = 0;
    while i < n {
        let w = *src.add(i);
        *dst.add(i) = w;
        i += 1;
        if w == 0 {
            break;
        }
    }
    while i < n {
        *dst.add(i) = 0;
        i += 1;
    }
    dst
}

/// `wchar_t* wcscat(dst, src)` — append wide `src` (incl. NUL), return `dst`.
pub unsafe extern "win64" fn wcscat(dst: *mut u16, src: *const u16) -> *mut u16 {
    let mut d = 0;
    while *dst.add(d) != 0 {
        d += 1;
    }
    let mut i = 0;
    loop {
        let w = *src.add(i);
        *dst.add(d + i) = w;
        if w == 0 {
            break;
        }
        i += 1;
    }
    dst
}

/// `wchar_t* wcsncat(dst, src, n)` — append at most `n` wide units plus NUL.
pub unsafe extern "win64" fn wcsncat(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 {
    let mut d = 0;
    while *dst.add(d) != 0 {
        d += 1;
    }
    let mut i = 0;
    while i < n {
        let w = *src.add(i);
        if w == 0 {
            break;
        }
        *dst.add(d + i) = w;
        i += 1;
    }
    *dst.add(d + i) = 0;
    dst
}

/// `wchar_t* wmemcpy(dst, src, n)` — copy `n` wide units (no overlap), return `dst`.
pub unsafe extern "win64" fn wmemcpy(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 {
    if n != 0 {
        std::ptr::copy_nonoverlapping(src, dst, n);
    }
    dst
}

/// `wchar_t* wmemmove(dst, src, n)` — overlap-safe copy of `n` wide units.
pub unsafe extern "win64" fn wmemmove(dst: *mut u16, src: *const u16, n: usize) -> *mut u16 {
    if n != 0 {
        std::ptr::copy(src, dst, n);
    }
    dst
}

// ---------------------------------------------------------------------------------------
// String search/duplication (Phase C step-12b): DllMain/config parsing uses
// `strstr`/`strrchr`/`strdup` and the wide `wcschr`/`wcsstr`/`_wcsdup` forms. Same
// NULL-stub fault class as memcpy: a logging stub returning NULL would look like
// "not found" or a failed alloc and silently mis-route callers, so these are real
// NUL-safe win64 loops (narrow = bytes, wide = UTF-16 units). `strstr`/`wcsstr`
// with an empty needle return `haystack`; `strrchr`/`wcschr` with `c == 0` return
// the NUL terminator. `strdup`/`_strdup`/`_wcsdup` allocate via the CRT heap above
// (assert on OOM, like `malloc`) and return an owned copy.
// ---------------------------------------------------------------------------------------

/// `char* strstr(haystack, needle)` — first `needle` in `haystack`; NULL when absent.
/// Empty `needle` returns `haystack`, as on Windows.
pub unsafe extern "win64" fn strstr(haystack: *const u8, needle: *const u8) -> *mut u8 {
    if *needle == 0 {
        return haystack as *mut u8;
    }
    let mut h = 0;
    loop {
        let hb = *haystack.add(h);
        if hb == 0 {
            return std::ptr::null_mut();
        }
        let mut i = 0;
        loop {
            let nb = *needle.add(i);
            if nb == 0 {
                return haystack.add(h) as *mut u8;
            }
            if *haystack.add(h + i) != nb {
                break;
            }
            // If haystack hit its NUL mid-needle the bytes differ (nb != 0),
            // so the break above already fired; no extra bound check needed.
            i += 1;
        }
        h += 1;
    }
}

/// `char* strrchr(s, c)` — last `c` (as `u8`) in `s` including the NUL; NULL when absent.
pub unsafe extern "win64" fn strrchr(s: *const u8, c: i32) -> *mut u8 {
    let target = c as u8;
    let mut last: *mut u8 = std::ptr::null_mut();
    let mut i = 0;
    loop {
        let b = *s.add(i);
        if b == target {
            last = s.add(i) as *mut u8;
        }
        if b == 0 {
            break;
        }
        i += 1;
    }
    last
}

/// `size_t strcspn(s, reject)` — length of the initial segment of `s` holding
/// no byte from `reject` (index of the first rejected byte, or `strlen(s)` when
/// none matches). Retail `exe+0x2237320` NULL chain: the mount check at
/// `exe+0x53525C0` (via `exe+0x643C10`) calls `strcspn("packroot:/appdata0.rpf",
/// ":...")`; the auto-stub returned 0, forcing a vacuous match in the
/// drive-record loop. Bytes (not case-folded); the NUL terminator of `s` stops
/// the scan and is never reported as a match.
pub unsafe extern "win64" fn strcspn(s: *const u8, reject: *const u8) -> usize {
    let mut i = 0;
    loop {
        let b = *s.add(i);
        if b == 0 {
            return i;
        }
        let mut j = 0;
        loop {
            let r = *reject.add(j);
            if r == 0 {
                break;
            }
            if r == b {
                return i;
            }
            j += 1;
        }
        i += 1;
    }
}

/// `char* strdup(s)` — owned heap copy (incl. NUL); asserts on OOM like `malloc`.
pub unsafe extern "win64" fn strdup(s: *const u8) -> *mut u8 {
    let n = strlen(s);
    let p = malloc(n + 1) as *mut u8;
    std::ptr::copy_nonoverlapping(s, p, n + 1);
    p
}

/// `wchar_t* wcschr(s, c)` — first `c` (as `u16`) in `s` including NUL; NULL when absent.
pub unsafe extern "win64" fn wcschr(s: *const u16, c: i32) -> *mut u16 {
    let target = c as u16;
    let mut i = 0;
    loop {
        let w = *s.add(i);
        if w == target {
            return s.add(i) as *mut u16;
        }
        if w == 0 {
            return std::ptr::null_mut();
        }
        i += 1;
    }
}

/// `wchar_t* wcsrchr(s, c)` — last `c` (as `u16`) in `s` including NUL; NULL when
/// absent (retail abort 0x2237179: the game's path split at `exe+0x55E2974`
/// looks for a separator in the module path to derive the dump dir; the
/// auto-stub always reported "not found").
pub unsafe extern "win64" fn wcsrchr(s: *const u16, c: i32) -> *mut u16 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    let target = c as u16;
    let mut last: *mut u16 = std::ptr::null_mut();
    let mut i = 0;
    loop {
        let w = *s.add(i);
        if w == target {
            last = s.add(i) as *mut u16;
        }
        if w == 0 {
            break;
        }
        i += 1;
    }
    last
}

/// `wchar_t* wcsstr(haystack, needle)` — first wide `needle`; NULL when absent.
/// Empty `needle` returns `haystack`, as on Windows.
pub unsafe extern "win64" fn wcsstr(haystack: *const u16, needle: *const u16) -> *mut u16 {
    if *needle == 0 {
        return haystack as *mut u16;
    }
    let mut h = 0;
    loop {
        if *haystack.add(h) == 0 {
            return std::ptr::null_mut();
        }
        let mut i = 0;
        loop {
            let nb = *needle.add(i);
            if nb == 0 {
                return haystack.add(h) as *mut u16;
            }
            if *haystack.add(h + i) != nb {
                break;
            }
            i += 1;
        }
        h += 1;
    }
}

/// `wchar_t* _wcsdup(s)` — owned wide heap copy (incl. NUL); asserts on OOM.
pub unsafe extern "win64" fn wcsdup_impl(s: *const u16) -> *mut u16 {
    let n = wcslen(s);
    let p = malloc((n + 1) * 2) as *mut u16;
    std::ptr::copy_nonoverlapping(s, p, n + 1);
    p
}

/// `char* strtok_s(str, delim, ctx)` — re-entrant tokenization (retail abort
/// 0x2237179: the config-table init at `exe+0x162B780` splits a `;`-separated
/// string; the auto-stub returned NULL so the table count stayed 0 and the
/// later lookup bailed before the dump-path check). `NULL` delim/ctx fails
/// cleanly; a `NULL` `str` continues from `*ctx`.
pub unsafe extern "win64" fn strtok_s(
    s: *mut u8,
    delim: *const u8,
    ctx: *mut *mut u8,
) -> *mut u8 {
    if delim.is_null() || ctx.is_null() {
        return std::ptr::null_mut();
    }
    let mut cur = if !s.is_null() { s } else { *ctx };
    if cur.is_null() {
        return std::ptr::null_mut();
    }
    // Skip leading delimiters.
    loop {
        let b = *cur;
        if b == 0 {
            *ctx = cur;
            return std::ptr::null_mut();
        }
        let mut d = 0;
        let mut is_delim = false;
        loop {
            let db = *delim.add(d);
            if db == 0 {
                break;
            }
            if b == db {
                is_delim = true;
                break;
            }
            d += 1;
        }
        if !is_delim {
            break;
        }
        cur = cur.add(1);
    }
    let start = cur;
    // Run to the next delimiter or NUL.
    loop {
        let b = *cur;
        if b == 0 {
            *ctx = cur;
            return start;
        }
        let mut d = 0;
        let mut is_delim = false;
        loop {
            let db = *delim.add(d);
            if db == 0 {
                break;
            }
            if b == db {
                is_delim = true;
                break;
            }
            d += 1;
        }
        if is_delim {
            *cur = 0;
            *ctx = cur.add(1);
            return start;
        }
        cur = cur.add(1);
    }
}

// ---------------------------------------------------------------------------------------
// Restartable multibyte/wide conversion (Phase C step-12c): `mbrtowc`/`wcrtomb`
// over UTF-8 with a stateless `mbstate_t` (the `ps` argument is accepted and,
// for `wcrtomb` surrogate pairing, used as a pending-high slot, but each
// complete character restarts from `s`). Same NULL-stub fault class as memcpy:
// a logging stub returning NULL/`(size_t)-1` unconditionally would mis-route
// every locale/argv conversion, so these decode for real. `mbrtowc` returns the
// bytes consumed (`0` for NUL, `(size_t)-1` = `usize::MAX` on a bad sequence
// with `errno = 42 (EILSEQ)`, `(size_t)-2` = `usize::MAX - 1` on an incomplete
// prefix); `s == NULL` probes statelessness and returns `0`. `wcrtomb` returns
// the bytes stored (`1` for NUL, `(size_t)-1` on error); `s == NULL` probes and
// returns `1`. Astral planes arrive as surrogate pairs through `ps`: a lone
// high stores pending and returns `0`, the matching low emits 4 bytes.
// ---------------------------------------------------------------------------------------

/// Windows `EILSEQ` (`errno` value set by `mbrtowc`/`wcrtomb` on bad sequences).
const EILSEQ_WIN: i32 = 42;
/// Windows `ERANGE` — `strtod`/`_strtoui64` overflow or underflow.
const ERANGE_WIN: i32 = 34;
/// Windows `EINVAL` — `_strtoui64` called with a base outside 0/2..=36.
const EINVAL_WIN: i32 = 22;

fn set_win_errno(code: i32) {
    unsafe {
        *_errno() = code;
    }
}

/// Decode one UTF-8 character starting at `s` with at most `n` bytes available.
/// Returns the code point and its byte length, or an error marker.
fn utf8_decode_one(s: *const u8, n: usize) -> Result<(u32, usize), usize> {
    // `n >= 1` is established by the caller; `b0` is always readable.
    let b0 = unsafe { *s };
    if b0 < 0x80 {
        return Ok((b0 as u32, 1));
    }
    // Continuation byte as a lead, or never-valid leads, are bad sequences.
    if b0 < 0xC2 {
        return Err(usize::MAX);
    }
    let (mut cp, len, min): (u32, usize, u32) = if b0 < 0xE0 {
        ((b0 & 0x1F) as u32, 2, 0x80)
    } else if b0 < 0xF0 {
        ((b0 & 0x0F) as u32, 3, 0x800)
    } else if b0 < 0xF5 {
        ((b0 & 0x07) as u32, 4, 0x10000)
    } else {
        return Err(usize::MAX);
    };
    if n < len {
        return Err(usize::MAX - 1);
    }
    for i in 1..len {
        let b = unsafe { *s.add(i) };
        if b & 0xC0 != 0x80 {
            return Err(usize::MAX);
        }
        cp = (cp << 6) | (b & 0x3F) as u32;
    }
    // Overlong forms (incl. `C0 80` for NUL), surrogates and out-of-range.
    if cp < min || (0xD800..0xE000).contains(&cp) || cp > 0x10FFFF {
        return Err(usize::MAX);
    }
    Ok((cp, len))
}

/// `size_t mbrtowc(wchar_t* pwc, const char* s, size_t n, mbstate_t* ps)` —
/// convert one UTF-8 character to UTF-16. Stateless: `ps` is accepted but each
/// complete character restarts from `s`.
pub unsafe extern "win64" fn mbrtowc(
    pwc: *mut u16,
    s: *const u8,
    n: usize,
    _ps: *mut u8,
) -> usize {
    if s.is_null() {
        // Stateless UTF-8 has no shift states.
        return 0;
    }
    if n == 0 {
        return usize::MAX - 1;
    }
    if *s == 0 {
        if !pwc.is_null() {
            *pwc = 0;
        }
        return 0;
    }
    match utf8_decode_one(s, n) {
        Ok((cp, len)) => {
            if !pwc.is_null() {
                if cp < 0x10000 {
                    *pwc = cp as u16;
                } else {
                    // Astral: single-`wchar_t` destination holds the lead
                    // surrogate; the trail half has no state slot on this path.
                    *pwc = (0xD800 + ((cp - 0x10000) >> 10)) as u16;
                }
            }
            len
        }
        Err(e) => {
            if e == usize::MAX {
                set_win_errno(EILSEQ_WIN);
            }
            e
        }
    }
}

/// `size_t wcrtomb(char* s, wchar_t wc, mbstate_t* ps)` — convert one UTF-16
/// unit (or surrogate pair via `*ps`) to UTF-8. `*ps` holds a pending high
/// surrogate (`0` = none); without `ps` lone surrogates are errors.
pub unsafe extern "win64" fn wcrtomb(s: *mut u8, wc: u32, ps: *mut u32) -> usize {
    // Probe: stateless UTF-8 encodes NUL in one byte.
    if s.is_null() {
        return 1;
    }
    let w = (wc & 0xFFFF) as u16;
    let pending: u16 = if ps.is_null() {
        0
    } else {
        ((*ps) & 0xFFFF) as u16
    };
    let clear_pending = || {
        if !ps.is_null() {
            *ps = 0;
        }
    };
    // NUL clears any pending high (a pair never spans NUL) and stores one byte.
    if w == 0 {
        clear_pending();
        *s = 0;
        return 1;
    }
    // High surrogate: stash and wait for the low half.
    if (0xD800..0xDC00).contains(&w) {
        if ps.is_null() {
            set_win_errno(EILSEQ_WIN);
            return usize::MAX;
        }
        *ps = w as u32;
        return 0;
    }
    // Low surrogate: needs a stashed high half.
    if (0xDC00..0xE000).contains(&w) {
        if ps.is_null() || !(0xD800..0xDC00).contains(&pending) {
            set_win_errno(EILSEQ_WIN);
            return usize::MAX;
        }
        let cp =
            0x10000 + ((pending as u32 - 0xD800) << 10) + (w as u32 - 0xDC00);
        *ps = 0;
        *s.add(0) = (0xF0 | (cp >> 18)) as u8;
        *s.add(1) = (0x80 | ((cp >> 12) & 0x3F)) as u8;
        *s.add(2) = (0x80 | ((cp >> 6) & 0x3F)) as u8;
        *s.add(3) = (0x80 | (cp & 0x3F)) as u8;
        return 4;
    }
    // A pending high followed by a BMP unit is an unpaired surrogate.
    if pending != 0 {
        clear_pending();
        set_win_errno(EILSEQ_WIN);
        return usize::MAX;
    }
    let cp = w as u32;
    if cp < 0x80 {
        *s = cp as u8;
        1
    } else if cp < 0x800 {
        *s.add(0) = (0xC0 | (cp >> 6)) as u8;
        *s.add(1) = (0x80 | (cp & 0x3F)) as u8;
        2
    } else {
        *s.add(0) = (0xE0 | (cp >> 12)) as u8;
        *s.add(1) = (0x80 | ((cp >> 6) & 0x3F)) as u8;
        *s.add(2) = (0x80 | (cp & 0x3F)) as u8;
        3
    }
}

// ---------------------------------------------------------------------------------------
// f32 math (CRT math slice): pure-Rust wrappers over the std `f32` intrinsics.
// The game imports sinf/cosf/tanf/atan2f/powf/exp2f/log10f from
// api-ms-win-crt-math-l1-1-0.dll / ucrtbase.dll; with no HLE they resolved to
// auto-stubs whose integer thunk never sets xmm0 (Win64 float returns travel in
// xmm0), so the SIMD loop at exe+0x1bce1xx consumed garbage. These return real
// IEEE-754 values — NaN/±0/±inf and atan2 quadrants included, matching UCRT —
// with no new deps and no Win APIs.
// ---------------------------------------------------------------------------------------

/// `float sinf(float)` — sine in radians.
pub unsafe extern "win64" fn sinf(x: f32) -> f32 {
    x.sin()
}

/// `float cosf(float)` — cosine in radians.
pub unsafe extern "win64" fn cosf(x: f32) -> f32 {
    x.cos()
}

/// `float tanf(float)` — tangent in radians.
pub unsafe extern "win64" fn tanf(x: f32) -> f32 {
    x.tan()
}

/// `float atan2f(float y, float x)` — quadrant-aware arctangent of `y/x`.
pub unsafe extern "win64" fn atan2f(y: f32, x: f32) -> f32 {
    y.atan2(x)
}

/// `float powf(float x, float y)` — `x` raised to `y`.
pub unsafe extern "win64" fn powf(x: f32, y: f32) -> f32 {
    x.powf(y)
}

/// `float exp2f(float x)` — 2 raised to `x`.
pub unsafe extern "win64" fn exp2f(x: f32) -> f32 {
    x.exp2()
}

/// `float log10f(float x)` — base-10 logarithm.
pub unsafe extern "win64" fn log10f(x: f32) -> f32 {
    x.log10()
}

/// `float log2f(float x)` — base-2 logarithm. Zero maps to `-inf`, a negative
/// argument to NaN, and denormals keep their exact exponent (`2^-149 → -149`).
pub unsafe extern "win64" fn log2f(x: f32) -> f32 {
    x.log2()
}

/// `float fmodf(float x, float y)` — C `fmod` truncation remainder: the result
/// takes `x`'s sign, a finite `x` survives `y = ±inf`, and `x % 0`/`inf % y`
/// are NaN. Rust's float `%` is that same operation (LLVM `frem` → libm).
pub unsafe extern "win64" fn fmodf(x: f32, y: f32) -> f32 {
    x % y
}

/// `float erff(float x)` — Gauss error function (UCRT edges: `±0 → ±0`,
/// `±inf → ±1`, NaN → NaN). Computed in f64 and rounded once, so the f32
/// result is within a few ulps of the reference.
pub unsafe extern "win64" fn erff(x: f32) -> f32 {
    erf_f64(x as f64) as f32
}

fn erf_f64(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x.is_infinite() {
        return if x > 0.0 { 1.0 } else { -1.0 };
    }
    if x == 0.0 {
        return x;
    }
    let magnitude = if x.abs() < 2.0 {
        erf_series(x.abs())
    } else {
        1.0 - erfc_continued_fraction(x.abs())
    };
    if x < 0.0 { -magnitude } else { magnitude }
}

/// `erf` power series, `2/sqrt(pi) * sum((-1)^n x^(2n+1)/(n!(2n+1)))`. It keeps
/// full relative accuracy near zero, where `1 - erfc` would cancel.
fn erf_series(x: f64) -> f64 {
    const TWO_OVER_SQRT_PI: f64 = 1.128_379_167_095_512_6;
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    let mut n = 0.0f64;
    loop {
        n += 1.0;
        term *= -x2 / n * (2.0 * n - 1.0) / (2.0 * n + 1.0);
        sum += term;
        if term.abs() < 1e-18 * sum.abs() {
            break;
        }
    }
    TWO_OVER_SQRT_PI * sum
}

/// `erfc(x) = Q(1/2, x^2)` via the modified-Lentz continued fraction, valid
/// above the series switch (`x >= 2`); `1 - erfc` is then accurate because
/// `erf` is within 5e-3 of 1.
fn erfc_continued_fraction(x: f64) -> f64 {
    const TINY: f64 = 1e-300;
    let z = x * x;
    let mut b = z + 0.5; // z + 1 - a, with a = 1/2
    let mut c = 1.0 / TINY;
    let mut d = 1.0 / b;
    let mut h = d;
    for i in 1..=300 {
        let i = i as f64;
        let an = -i * (i - 0.5);
        b += 2.0;
        d = an * d + b;
        if d.abs() < TINY {
            d = TINY;
        }
        c = b + an / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = d * c;
        h *= delta;
        if (delta - 1.0).abs() < 1e-16 {
            break;
        }
    }
    (-z).exp() * z.sqrt() / std::f64::consts::PI.sqrt() * h
}

/// `double atan2(double y, double x)` — quadrant-aware arctangent. The title's
/// f32 callers promote both arguments through `exe+0x623b20` before the IAT
/// call at `0x146794618`.
pub unsafe extern "win64" fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

/// `char* getenv(const char*)` — process environment; NULL when missing, as on Windows.
pub unsafe extern "win64" fn getenv(name: *const u8) -> *mut u8 {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let Some(key) = crate::util::from_cstr_ptr(name) else {
        return std::ptr::null_mut();
    };
    match std::env::var(&key) {
        Ok(v) => immortal_cstr(&v),
        Err(_) => std::ptr::null_mut(),
    }
}

fn immortal_cstr(s: &str) -> *mut u8 {
    unsafe {
        let p = libc::malloc(s.len() + 1) as *mut u8;
        assert!(!p.is_null(), "crt: out of memory");
        std::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len());
        *p.add(s.len()) = 0;
        p
    }
}

/// `int setvbuf(FILE*, char*, int, size_t)` — stdio is unbacked (see
/// `__acrt_iob_func`), so buffering requests are accepted and ignored (`0`).
pub unsafe extern "win64" fn setvbuf(
    _stream: *mut u8,
    _buf: *mut u8,
    _mode: i32,
    _size: usize,
) -> i32 {
    0
}

// ---------------------------------------------------------------------------------------
// Step-11l file reads past stubs: `fopen`/`fread`/`fwrite`/`fclose`/`fflush` through
// the linked-in Linux VFS (same `vfs_hooks::open` the kernelx file APIs use).
// ---------------------------------------------------------------------------------------
//
// The Windows build hooks KERNELBASE so the CRT goes through the VFS. Linux has no
// KERNELBASE: these shims resolve the narrow path through `vfs_hooks::open` directly.
// `FILE*` is a boxed `CrtFile` owning its host `File`; `fread`/`fwrite` are itemwise
// (`size * count`) as on Windows, `fflush(NULL)` succeeds without iterating the table.

struct CrtFile {
    file: std::fs::File,
    append: bool,
}

fn crt_mode(mode: &str) -> Option<(u32, u32, bool)> {
    use crate::vfs::{CREATE_ALWAYS, GENERIC_READ, GENERIC_WRITE, OPEN_ALWAYS, OPEN_EXISTING};
    let mut chars = mode.chars();
    let first = chars.next()?;
    let mut read = false;
    let mut write = false;
    let mut append = false;
    match first {
        'r' => read = true,
        'w' => write = true,
        'a' => {
            write = true;
            append = true;
        }
        _ => return None,
    }
    for c in chars {
        match c {
            '+' => {
                read = true;
                write = true;
            }
            'b' | 't' | 'c' | 'n' | 'S' | 'R' | 'T' | 'D' => {}
            'x' => {}
            _ => {}
        }
    }
    let access = (if read { GENERIC_READ } else { 0 }) | (if write { GENERIC_WRITE } else { 0 });
    let disp = match first {
        'r' => OPEN_EXISTING,
        'w' => CREATE_ALWAYS,
        'a' => OPEN_ALWAYS,
        _ => return None,
    };
    Some((access, disp, append))
}

pub unsafe extern "win64" fn fopen(path: *const u8, mode: *const u8) -> *mut u8 {
    if path.is_null() || mode.is_null() {
        return std::ptr::null_mut();
    }
    let (Some(p), Some(m)) = (
        crate::util::from_cstr_ptr(path),
        crate::util::from_cstr_ptr(mode),
    ) else {
        return std::ptr::null_mut();
    };
    let Some((access, disp, append)) = crt_mode(&m) else {
        return std::ptr::null_mut();
    };
    let opened = match crate::vfs_hooks::open(&p, access, disp, 0) {
        Ok(o) => o,
        Err(_) => return std::ptr::null_mut(),
    };
    let mut file = opened.file;
    if append {
        use std::io::Seek;
        let _ = file.seek(std::io::SeekFrom::End(0));
    }
    let boxed = Box::new(CrtFile { file, append });
    Box::into_raw(boxed) as *mut u8
}

unsafe fn crt_file_mut<'a>(stream: *mut u8) -> Option<&'a mut CrtFile> {
    if stream.is_null() {
        return None;
    }
    Some(&mut *(stream as *mut CrtFile))
}

pub unsafe extern "win64" fn fread(
    ptr: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut u8,
) -> usize {
    let total = size.saturating_mul(count);
    if total == 0 || ptr.is_null() {
        return 0;
    }
    let Some(f) = crt_file_mut(stream) else {
        return 0;
    };
    let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, total);
    use std::io::Read;
    // `fread` may return a short count at EOF; loop until full or no progress.
    let mut done = 0usize;
    while done < total {
        match f.file.read(&mut slice[done..]) {
            Ok(0) => break,
            Ok(k) => done += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if size == 0 {
        0
    } else {
        done / size
    }
}

pub unsafe extern "win64" fn fwrite(
    ptr: *const c_void,
    size: usize,
    count: usize,
    stream: *mut u8,
) -> usize {
    let total = size.saturating_mul(count);
    if total == 0 || ptr.is_null() {
        return 0;
    }
    let Some(f) = crt_file_mut(stream) else {
        return 0;
    };
    if f.append {
        use std::io::Seek;
        let _ = f.file.seek(std::io::SeekFrom::End(0));
    }
    let slice = std::slice::from_raw_parts(ptr as *const u8, total);
    use std::io::Write;
    let mut done = 0usize;
    while done < total {
        match f.file.write(&slice[done..]) {
            Ok(0) => break,
            Ok(k) => done += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if size == 0 {
        0
    } else {
        done / size
    }
}

pub unsafe extern "win64" fn fputc(c: i32, stream: *mut u8) -> i32 {
    let b = [c as u8];
    let n = fwrite(
        b.as_ptr() as *const c_void,
        1,
        1,
        stream,
    );
    if n == 1 {
        c & 0xFF
    } else {
        -1
    }
}

pub unsafe extern "win64" fn fflush(stream: *mut u8) -> i32 {
    use std::io::Write;
    if stream.is_null() {
        return 0;
    }
    let Some(f) = crt_file_mut(stream) else {
        return -1;
    };
    match f.file.flush() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

pub unsafe extern "win64" fn fclose(stream: *mut u8) -> i32 {
    if stream.is_null() {
        return -1;
    }
    let mut boxed = Box::from_raw(stream as *mut CrtFile);
    use std::io::Write;
    let rc = match boxed.file.flush() {
        Ok(()) => 0,
        Err(_) => -1,
    };
    drop(boxed);
    rc
}

pub unsafe extern "win64" fn fseek(stream: *mut u8, offset: i64, origin: i32) -> i32 {
    let Some(f) = crt_file_mut(stream) else {
        return -1;
    };
    use std::io::Seek;
    let pos = match origin {
        0 => std::io::SeekFrom::Start(offset as u64),
        1 => std::io::SeekFrom::Current(offset),
        2 => std::io::SeekFrom::End(offset),
        _ => return -1,
    };
    match f.file.seek(pos) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

pub unsafe extern "win64" fn ftell(stream: *mut u8) -> i64 {
    let Some(f) = crt_file_mut(stream) else {
        return -1;
    };
    use std::io::Seek;
    match f.file.seek(std::io::SeekFrom::Current(0)) {
        Ok(p) => p as i64,
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------------------
// `__stdio_common_vfprintf` (Phase C step-12d): narrow formatted output over a win64
// `va_list`, writing into `CrtFile` through the `fwrite` path above.
//
// UCRT signature: `int __stdio_common_vfprintf(unsigned __int64 options, FILE*
// stream, const char* format, _locale_t locale, va_list arglist)`. On win64 a
// `va_list` is a `char*` to 8-byte stack slots: every argument (int, pointer,
// `double`) occupies one slot, which is what the reader below assumes. `options`
// carries `_CRT_INTERNAL_PRINTF_*` flags (legacy null termination, positional
// parameters, …) and `locale` is a `_locale_t` (`NULL` = global C locale); both are
// documented here and otherwise ignored — output is always C-locale narrow text.
// `%n` (in every length form) consumes its pointer slot and writes nothing, so a
// stray or hostile `%n` can never scribble guest memory. `NULL` stream/format (or
// a `NULL` arglist when the format needs an argument) is an error return (`-1`),
// never a crash; unknown verbs are an error return too. A lone trailing `%`
// emits nothing further (the prefix still counts). Non-finite floats print C99
// `nan`/`inf` (UCRT legacy spells `1.#QNAN0`/`1.#INF00`; not emulated), `%a`
// truncates rather than rounds to the requested precision, and `%ls` precision
// counts wide units.
// ---------------------------------------------------------------------------------------

/// `int __stdio_common_vfprintf(options, stream, format, locale, arglist)` —
/// format `format` with `arglist` into `stream`; returns bytes written or `-1`.
pub unsafe extern "win64" fn __stdio_common_vfprintf(
    _options: u64,
    stream: *mut u8,
    format: *const u8,
    _locale: usize,
    arglist: *const u8,
) -> i32 {
    if stream.is_null() || format.is_null() {
        return -1;
    }
    let slots = if arglist.is_null() {
        None
    } else {
        Some(arglist as *const u64)
    };
    let out = match render_narrow_format(format, slots) {
        Some(v) => v,
        None => return -1,
    };
    if out.len() > i32::MAX as usize {
        return -1;
    }
    if out.is_empty() {
        return 0;
    }
    let n = fwrite(out.as_ptr() as *const c_void, 1, out.len(), stream);
    if n == out.len() {
        out.len() as i32
    } else {
        -1
    }
}

/// Width/precision fields larger than this are rejected (the renderer fails, hence
/// `-1`) instead of allocating blindly for a hostile `%999999999d`.
const MAX_FIELD: usize = 1_000_000;

/// Integer length modifiers. Windows sizes: `l` is 32-bit (`long`), `ll` and the
/// `j`/`z`/`t`/bare-`I`/`I64` forms are 64-bit, `I32` is 32-bit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IntLen {
    Def,
    Hh,
    H,
    L,
    Ll,
}

impl IntLen {
    fn signed(self, slot: u64) -> i64 {
        match self {
            IntLen::Hh => (slot as u8) as i8 as i64,
            IntLen::H => (slot as u16) as i16 as i64,
            IntLen::Def | IntLen::L => (slot as u32) as i32 as i64,
            IntLen::Ll => slot as i64,
        }
    }

    fn unsigned(self, slot: u64) -> u64 {
        match self {
            IntLen::Hh => (slot as u8) as u64,
            IntLen::H => (slot as u16) as u64,
            IntLen::Def | IntLen::L => (slot as u32) as u64,
            IntLen::Ll => slot,
        }
    }
}

/// Win64 `va_list` cursor over 8-byte slots. A `NULL` list fails every fetch, so
/// formats needing arguments report an error instead of faulting.
struct VaReader {
    cur: *const u64,
    exhausted: bool,
}

impl VaReader {
    fn next(&mut self) -> Option<u64> {
        if self.exhausted {
            return None;
        }
        unsafe {
            let v = *self.cur;
            self.cur = self.cur.add(1);
            Some(v)
        }
    }
}

#[derive(Clone, Copy)]
struct FmtSpec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: Option<usize>,
    prec: Option<usize>,
}

/// Render a NUL-terminated narrow format with `args` into bytes. `None` means the
/// format needs an argument the (NULL) list cannot supply, a field is unbounded,
/// or the verb is unknown — the caller reports `-1` and writes nothing.
unsafe fn render_narrow_format(format: *const u8, args: Option<*const u64>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut ap = VaReader {
        cur: args.unwrap_or(std::ptr::null()),
        exhausted: args.is_none(),
    };
    let mut i = 0usize;
    loop {
        let b = *format.add(i);
        if b == 0 {
            break;
        }
        if b != b'%' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        if *format.add(i) == 0 {
            // Truncated: a lone `%` at the end emits nothing further.
            break;
        }
        if *format.add(i) == b'%' {
            out.push(b'%');
            i += 1;
            continue;
        }
        render_one_spec(format, &mut i, &mut ap, &mut out)?;
    }
    Some(out)
}

/// Render one `%`-spec starting at `*pos` (first spec byte); advances `*pos` past
/// the verb. A NUL where the verb belongs is a truncated format: emit nothing and
/// stop (the outer loop sees the NUL and ends the string).
unsafe fn render_one_spec(
    format: *const u8,
    pos: &mut usize,
    ap: &mut VaReader,
    out: &mut Vec<u8>,
) -> Option<()> {
    let mut spec = FmtSpec {
        left: false,
        plus: false,
        space: false,
        alt: false,
        zero: false,
        width: None,
        prec: None,
    };
    loop {
        match *format.add(*pos) {
            b'-' => spec.left = true,
            b'+' => spec.plus = true,
            b' ' => spec.space = true,
            b'#' => spec.alt = true,
            b'0' => spec.zero = true,
            _ => break,
        }
        *pos += 1;
    }
    if *format.add(*pos) == b'*' {
        *pos += 1;
        let w = ap.next()? as u32 as i32;
        if w < 0 {
            spec.left = true;
            let mag = (w as i64).unsigned_abs() as usize;
            if mag > MAX_FIELD {
                return None;
            }
            spec.width = Some(mag);
        } else {
            let wu = w as usize;
            if wu > MAX_FIELD {
                return None;
            }
            spec.width = Some(wu);
        }
    } else {
        let mut w = 0usize;
        let mut any = false;
        while (*format.add(*pos)).is_ascii_digit() {
            any = true;
            w = w
                .saturating_mul(10)
                .saturating_add((*format.add(*pos) - b'0') as usize);
            if w > MAX_FIELD {
                return None;
            }
            *pos += 1;
        }
        if any {
            spec.width = Some(w);
        }
    }
    if *format.add(*pos) == b'.' {
        *pos += 1;
        if *format.add(*pos) == b'*' {
            *pos += 1;
            let p = ap.next()? as u32 as i32;
            if p >= 0 {
                let pu = p as usize;
                if pu > MAX_FIELD {
                    return None;
                }
                spec.prec = Some(pu);
            }
        } else {
            let mut p = 0usize;
            while (*format.add(*pos)).is_ascii_digit() {
                p = p
                    .saturating_mul(10)
                    .saturating_add((*format.add(*pos) - b'0') as usize);
                if p > MAX_FIELD {
                    return None;
                }
                *pos += 1;
            }
            spec.prec = Some(p);
        }
    }
    // Lengths: `hh`/`h`/`ll`/`l`/`j`/`z`/`t`/`L`, plus MS `I`/`I32`/`I64`. A lone
    // `l` also marks `%ls`/`%lc` as wide (integer verbs ignore the flag).
    let mut len = IntLen::Def;
    let mut wide = false;
    match *format.add(*pos) {
        b'h' => {
            *pos += 1;
            if *format.add(*pos) == b'h' {
                *pos += 1;
                len = IntLen::Hh;
            } else {
                len = IntLen::H;
            }
        }
        b'l' => {
            *pos += 1;
            if *format.add(*pos) == b'l' {
                *pos += 1;
                len = IntLen::Ll;
            } else {
                len = IntLen::L;
                wide = true;
            }
        }
        b'j' | b'z' | b't' => {
            *pos += 1;
            len = IntLen::Ll;
        }
        b'L' => {
            // Windows `long double` == `double`; only the int width matters.
            *pos += 1;
            len = IntLen::Ll;
        }
        b'I' => {
            *pos += 1;
            if *format.add(*pos) == b'3' && *format.add(*pos + 1) == b'2' {
                *pos += 2;
                len = IntLen::L;
            } else if *format.add(*pos) == b'6' && *format.add(*pos + 1) == b'4' {
                *pos += 2;
                len = IntLen::Ll;
            } else {
                // Bare `I` is pointer-sized (64-bit here).
                len = IntLen::Ll;
            }
        }
        _ => {}
    }
    let verb = *format.add(*pos);
    if verb == 0 {
        // Truncated inside the spec (`"%5"`, `"%-"` …): emit nothing further.
        return Some(());
    }
    *pos += 1;
    match verb {
        b'd' | b'i' => push_signed(out, len.signed(ap.next()?), &spec),
        b'u' => push_unsigned(out, len.unsigned(ap.next()?), 10, false, &spec, false),
        b'o' => push_unsigned(out, len.unsigned(ap.next()?), 8, false, &spec, false),
        b'x' => push_unsigned(out, len.unsigned(ap.next()?), 16, false, &spec, false),
        b'X' => push_unsigned(out, len.unsigned(ap.next()?), 16, true, &spec, false),
        b'p' => push_unsigned(out, ap.next()?, 16, false, &spec, true),
        b'c' => {
            if wide {
                push_wchar(out, ap.next()? as u16, &spec);
            } else {
                push_padded_str(out, &[(ap.next()? & 0xFF) as u8], &spec);
            }
        }
        b'C' => push_wchar(out, ap.next()? as u16, &spec),
        b's' => {
            if wide {
                push_wide_str(out, ap.next()? as *const u16, &spec);
            } else {
                push_narrow_str(out, ap.next()? as *const u8, &spec);
            }
        }
        b'S' => push_wide_str(out, ap.next()? as *const u16, &spec),
        b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
            push_float(out, f64::from_bits(ap.next()?), verb, &spec)
        }
        // Never honored: consume the pointer slot, write nothing.
        b'n' => {
            ap.next()?;
        }
        _ => return None,
    }
    Some(())
}

/// Render `v` in `radix` (8/10/16); `upper` selects hex case. `0` yields `"0"`.
fn uint_digits(mut v: u64, radix: u32, upper: bool) -> Vec<u8> {
    if v == 0 {
        return vec![b'0'];
    }
    let mut rev = Vec::new();
    while v != 0 {
        let d = (v % radix as u64) as u8;
        rev.push(if d < 10 {
            b'0' + d
        } else {
            (if upper { b'A' } else { b'a' }) + d - 10
        });
        v /= radix as u64;
    }
    rev.reverse();
    rev
}

/// Join sign/prefix/digits with `width` padding: `0`-pad goes between prefix and
/// digits (a precision cancels it for integers), otherwise spaces; `left` pads
/// on the right.
fn emit_number(out: &mut Vec<u8>, sign: &[u8], prefix: &[u8], digits: &[u8], spec: &FmtSpec) {
    let width = spec.width.unwrap_or(0).min(MAX_FIELD + 1);
    let body = sign.len() + prefix.len() + digits.len();
    if spec.left {
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
        for _ in body..width {
            out.push(b' ');
        }
    } else if spec.zero && spec.prec.is_none() {
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        for _ in body..width {
            out.push(b'0');
        }
        out.extend_from_slice(digits);
    } else {
        for _ in body..width {
            out.push(b' ');
        }
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.extend_from_slice(digits);
    }
}

/// Decimal signed integer; `#` has no effect on `%d`/`%i`, as in C.
fn push_signed(out: &mut Vec<u8>, v: i64, spec: &FmtSpec) {
    let (sign, mag): (&[u8], u64) = if v < 0 {
        (b"-", (v as u64).wrapping_neg())
    } else if spec.plus {
        (b"+", v as u64)
    } else if spec.space {
        (b" ", v as u64)
    } else {
        (b"", v as u64)
    };
    let digits = if mag == 0 && spec.prec == Some(0) {
        Vec::new()
    } else {
        let mut d = uint_digits(mag, 10, false);
        if let Some(p) = spec.prec {
            while d.len() < p {
                d.insert(0, b'0');
            }
        }
        d
    };
    emit_number(out, sign, &[], &digits, spec);
}

/// Unsigned integer (`pointer` forces the `0x` prefix for `%p`). `+`/space have
/// no effect on unsigned conversions, as in C.
fn push_unsigned(
    out: &mut Vec<u8>,
    v: u64,
    radix: u32,
    upper: bool,
    spec: &FmtSpec,
    pointer: bool,
) {
    let mut digits = if v == 0 && spec.prec == Some(0) && !pointer {
        Vec::new()
    } else {
        let mut d = uint_digits(v, radix, upper);
        if let Some(p) = spec.prec {
            while d.len() < p {
                d.insert(0, b'0');
            }
        }
        d
    };
    let mut prefix: &[u8] = &[];
    if pointer {
        prefix = b"0x";
    } else if spec.alt {
        if radix == 8 {
            if digits.is_empty() {
                digits.push(b'0');
            } else if digits[0] != b'0' {
                digits.insert(0, b'0');
            }
        } else if radix == 16 && v != 0 {
            prefix = if upper { b"0X" } else { b"0x" };
        }
    }
    emit_number(out, &[], prefix, &digits, spec);
}

/// Strings/chars pad with spaces only (the `0` flag is ignored, as in C).
fn push_padded_str(out: &mut Vec<u8>, bytes: &[u8], spec: &FmtSpec) {
    let width = spec.width.unwrap_or(0).min(MAX_FIELD + 1);
    if spec.left {
        out.extend_from_slice(bytes);
        for _ in bytes.len()..width {
            out.push(b' ');
        }
    } else {
        for _ in bytes.len()..width {
            out.push(b' ');
        }
        out.extend_from_slice(bytes);
    }
}

/// Narrow `%s`: `NULL` prints `(null)`; precision caps the bytes.
fn push_narrow_str(out: &mut Vec<u8>, s: *const u8, spec: &FmtSpec) {
    let bytes: &[u8] = if s.is_null() {
        b"(null)"
    } else {
        unsafe {
            let n = strlen(s);
            std::slice::from_raw_parts(s, n)
        }
    };
    let end = spec.prec.map_or(bytes.len(), |p| p.min(bytes.len()));
    push_padded_str(out, &bytes[..end], spec);
}

/// Wide `%s`/`%S`: UTF-16 guest units re-encoded as UTF-8 (`NULL` prints `(null)`;
/// lone surrogates become U+FFFD); precision caps the wide units read.
fn push_wide_str(out: &mut Vec<u8>, s: *const u16, spec: &FmtSpec) {
    if s.is_null() {
        let end = spec.prec.map_or(6, |p| p.min(6));
        push_padded_str(out, &b"(null)"[..end], spec);
        return;
    }
    let mut tmp = Vec::new();
    unsafe {
        push_utf16_as_utf8(&mut tmp, s, spec.prec);
    }
    push_padded_str(out, &tmp, spec);
}

/// Single wide char (`%lc`/`%C`): re-encoded as UTF-8 (a surrogate becomes U+FFFD).
fn push_wchar(out: &mut Vec<u8>, w: u16, spec: &FmtSpec) {
    let mut tmp = [0u8; 3];
    let n = if (0xD800..0xE000).contains(&w) {
        tmp[0] = 0xEF;
        tmp[1] = 0xBF;
        tmp[2] = 0xBD;
        3
    } else if w < 0x80 {
        tmp[0] = w as u8;
        1
    } else if w < 0x800 {
        tmp[0] = (0xC0 | (w >> 6)) as u8;
        tmp[1] = (0x80 | (w & 0x3F)) as u8;
        2
    } else {
        tmp[0] = (0xE0 | (w >> 12)) as u8;
        tmp[1] = (0x80 | ((w >> 6) & 0x3F)) as u8;
        tmp[2] = (0x80 | (w & 0x3F)) as u8;
        3
    };
    push_padded_str(out, &tmp[..n], spec);
}

/// Append UTF-8 for `cp` (a full code point).
fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push((0xC0 | (cp >> 6)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    } else if cp < 0x10000 {
        out.push((0xE0 | (cp >> 12)) as u8);
        out.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    } else {
        out.push((0xF0 | (cp >> 18)) as u8);
        out.push((0x80 | ((cp >> 12) & 0x3F)) as u8);
        out.push((0x80 | ((cp >> 6) & 0x3F)) as u8);
        out.push((0x80 | (cp & 0x3F)) as u8);
    }
}

/// Copy NUL-terminated UTF-16 at `s` as UTF-8, reading at most `max_units` units
/// (`None` = to the NUL). Surrogate pairs combine; lone halves become U+FFFD.
unsafe fn push_utf16_as_utf8(out: &mut Vec<u8>, s: *const u16, max_units: Option<usize>) {
    let mut i = 0usize;
    loop {
        if let Some(m) = max_units {
            if i >= m {
                break;
            }
        }
        let w = *s.add(i);
        if w == 0 {
            break;
        }
        i += 1;
        if (0xD800..0xDC00).contains(&w) {
            let in_range = match max_units {
                Some(m) => i < m,
                None => true,
            };
            let lo = *s.add(i);
            if in_range && (0xDC00..0xE000).contains(&lo) {
                i += 1;
                push_utf8(
                    out,
                    0x10000 + ((w as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00),
                );
            } else {
                push_utf8(out, 0xFFFD);
            }
        } else if (0xDC00..0xE000).contains(&w) {
            push_utf8(out, 0xFFFD);
        } else {
            push_utf8(out, w as u32);
        }
    }
}

fn float_sign(v: f64, spec: &FmtSpec) -> &'static str {
    if v.is_sign_negative() {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    }
}

/// `%f`: exactly `prec` digits after the dot (`#` forces the dot when `prec == 0`).
fn float_fixed(a: f64, prec: usize, alt: bool) -> Vec<u8> {
    let mut s = format!("{a:.prec$}");
    if alt && prec == 0 && !s.contains('.') {
        s.push('.');
    }
    s.into_bytes()
}

/// `%e`: one digit, dot, `prec` digits, then `e±dd` (at least two exponent digits).
fn float_sci(a: f64, prec: usize, alt: bool, upper: bool) -> Vec<u8> {
    let s = format!("{a:.prec$e}");
    let cut = s.find('e').unwrap_or(s.len());
    let (mant, exp) = s.split_at(cut);
    let mut mant = mant.to_string();
    if alt && prec == 0 && !mant.contains('.') {
        mant.push('.');
    }
    let exp: i32 = exp[1..].parse().unwrap_or(0);
    let mut body = mant.into_bytes();
    body.push(if upper { b'E' } else { b'e' });
    body.extend_from_slice(format!("{exp:+03}").as_bytes());
    body
}

/// Strip trailing fraction zeros (then a bare dot) from a `%g` body, leaving any
/// `e±dd` exponent intact.
fn strip_float_zeros(body: &mut Vec<u8>) {
    let epos = body.iter().position(|&b| b == b'e' || b == b'E');
    let end = epos.unwrap_or(body.len());
    if let Some(d) = body[..end].iter().position(|&b| b == b'.') {
        let mut cut = end;
        while cut > d + 1 && body[cut - 1] == b'0' {
            cut -= 1;
        }
        if cut == d + 1 {
            cut = d;
        }
        body.drain(cut..end);
    }
}

/// `%g`: `prec` significant digits (default 6, `0` means 1); scientific when the
/// decimal exponent is `< -4` or `>= prec`, else fixed; trailing zeros (and the
/// dot) are stripped unless `#` is set.
fn float_general(a: f64, prec: usize, alt: bool, upper: bool) -> Vec<u8> {
    let sig = if prec == 0 { 1 } else { prec };
    if a == 0.0 {
        if alt {
            let mut body = vec![b'0', b'.'];
            for _ in 1..sig {
                body.push(b'0');
            }
            return body;
        }
        return b"0".to_vec();
    }
    // Exact decimal exponent from Rust's own scientific spelling (no `log10` drift).
    let probe = format!("{a:e}");
    let exp: i32 = probe
        .split('e')
        .nth(1)
        .and_then(|e| e.parse().ok())
        .unwrap_or(0);
    let mut body = if exp < -4 || exp >= sig as i32 {
        float_sci(a, sig - 1, true, upper)
    } else {
        let frac = (sig as i32 - 1 - exp).max(0) as usize;
        float_fixed(a, frac, true)
    };
    if !alt {
        strip_float_zeros(&mut body);
    }
    body
}

/// `%a`: C hex float `0xh.hhhhp±d` (`prec` = hex digits after the dot; default
/// strips trailing zeros). Truncation (no rounding) on a short `prec` is a
/// documented HLE approximation.
fn float_hex(a: f64, prec: Option<usize>, alt: bool, upper: bool) -> Vec<u8> {
    let bits = a.to_bits();
    let exp_bits = ((bits >> 52) & 0x7FF) as i32;
    let frac = bits & 0x000F_FFFF_FFFF_FFFF;
    let (int_digit, exp_out) = if a == 0.0 {
        (b'0', 0)
    } else if exp_bits == 0 {
        (b'0', -1022)
    } else {
        (b'1', exp_bits - 1023)
    };
    let mut digits = format!("{frac:013x}").into_bytes();
    match prec {
        Some(p) => {
            let p = p.min(MAX_FIELD);
            if digits.len() > p {
                digits.truncate(p);
            }
            while digits.len() < p {
                digits.push(b'0');
            }
        }
        None => {
            while digits.last() == Some(&b'0') {
                digits.pop();
            }
        }
    }
    let mut body = Vec::new();
    body.extend_from_slice(b"0x");
    body.push(int_digit);
    if !digits.is_empty() || alt {
        body.push(b'.');
        body.extend_from_slice(&digits);
    }
    body.push(b'p');
    body.extend_from_slice(format!("{exp_out:+}").as_bytes());
    if upper {
        body.make_ascii_uppercase();
    }
    body
}

fn push_float(out: &mut Vec<u8>, v: f64, verb: u8, spec: &FmtSpec) {
    if v.is_nan() || v.is_infinite() {
        let body = if v.is_nan() {
            if verb.is_ascii_uppercase() {
                "NAN"
            } else {
                "nan"
            }
        } else if verb.is_ascii_uppercase() {
            "INF"
        } else {
            "inf"
        };
        // Non-finite values pad with spaces (a `0` flag would mislead).
        let mut plain = *spec;
        plain.zero = false;
        emit_number(out, float_sign(v, spec).as_bytes(), &[], body.as_bytes(), &plain);
        return;
    }
    let neg = v < 0.0 || (v == 0.0 && v.is_sign_negative());
    let a = v.abs();
    let sign: &[u8] = if neg {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    };
    let prec = spec.prec.unwrap_or(6).min(MAX_FIELD);
    let mut body: Vec<u8> = match verb {
        b'f' | b'F' => float_fixed(a, prec, spec.alt),
        b'e' | b'E' => float_sci(a, prec, spec.alt, verb == b'E'),
        b'g' | b'G' => float_general(a, prec, spec.alt, verb == b'G'),
        _ => float_hex(a, spec.prec, spec.alt, verb == b'A'),
    };
    if matches!(verb, b'F' | b'E' | b'G') {
        body.make_ascii_uppercase();
    }
    emit_number(out, sign, &[], &body, spec);
}

// ---------------------------------------------------------------------------------------
// `__stdio_common_vswprintf_s` / `__stdio_common_vswprintf` (retail abort 0x2237179)
// ---------------------------------------------------------------------------------------
//
// The fault function reads a dump-path buffer through the config lookup whose format
// table lives at `exe+0x678D1BB` (`%llu`, `D:\LocalDumps`, `D:/LocalDumps`, `%s%s`,
// `%s\%s`, `upload_file_minidump`, `%lc%d%lc%n`, ...). The game's wrapper at
// `exe+0x5B8A5C0` formats into caller buffers through `vswprintf_s`; the auto-stub
// returned 0 and left every such buffer empty, so the absolute-path predicate
// (`sub_140643C90`: leading `/`/`\` or a `:`) never passed. This implements the real
// Win32 wide formatter over a win64 `va_list` (one u64 slot per argument, as for
// `vfprintf` above), C-locale output, `%n` consuming its slot but writing nothing
// (never scribbling guest memory, matching the narrow renderer).
//
// Wide verbs: `%s`/`%ls`/`%ws` read UTF-16 guest strings (re-encoded as UTF-16
// output; lone surrogates become U+FFFD; precision caps wide units), `%hs`/`%S`
// read UTF-8 guest strings, `%c`/`%lc`/`%C` are wide chars, `%hc` narrow chars.
// Integers, `%p`, floats and `%n` mirror the narrow contract above.

fn wlen(p: *const u16) -> usize {
    unsafe {
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        n
    }
}

unsafe fn guest_wide_to_string_capped(s: *const u16, max_units: Option<usize>) -> String {
    if s.is_null() {
        return "(null)".to_string();
    }
    let mut units: Vec<u16> = Vec::new();
    let mut i = 0usize;
    loop {
        if let Some(m) = max_units {
            if i >= m {
                break;
            }
        }
        let w = *s.add(i);
        if w == 0 {
            break;
        }
        units.push(w);
        i += 1;
    }
    String::from_utf16_lossy(&units)
}

unsafe fn guest_narrow_to_string_capped(s: *const u8, max_bytes: Option<usize>) -> String {
    if s.is_null() {
        return "(null)".to_string();
    }
    let n = strlen(s);
    let end = max_bytes.map_or(n, |p| p.min(n));
    String::from_utf8_lossy(std::slice::from_raw_parts(s, end)).into_owned()
}

fn push_u16_str(out: &mut Vec<u16>, text: &str, spec: &FmtSpec) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let width = spec.width.unwrap_or(0).min(MAX_FIELD + 1);
    if spec.left {
        out.extend_from_slice(&wide);
        for _ in wide.len()..width {
            out.push(' ' as u16);
        }
    } else {
        for _ in wide.len()..width {
            out.push(' ' as u16);
        }
        out.extend_from_slice(&wide);
    }
}

fn push_u16_number(out: &mut Vec<u16>, sign: &[u8], prefix: &[u8], digits: &[u8], spec: &FmtSpec) {
    let mut tmp: Vec<u8> = Vec::new();
    emit_number(&mut tmp, sign, prefix, digits, spec);
    out.extend(tmp.iter().map(|&b| b as u16));
}

fn push_signed_w(out: &mut Vec<u16>, v: i64, spec: &FmtSpec) {
    let (sign, mag): (&[u8], u64) = if v < 0 {
        (b"-", (v as u64).wrapping_neg())
    } else if spec.plus {
        (b"+", v as u64)
    } else if spec.space {
        (b" ", v as u64)
    } else {
        (b"", v as u64)
    };
    let digits = if mag == 0 && spec.prec == Some(0) {
        Vec::new()
    } else {
        let mut d = uint_digits(mag, 10, false);
        if let Some(p) = spec.prec {
            while d.len() < p {
                d.insert(0, b'0');
            }
        }
        d
    };
    push_u16_number(out, sign, &[], &digits, spec);
}

fn push_unsigned_w(out: &mut Vec<u16>, v: u64, radix: u32, upper: bool, spec: &FmtSpec, pointer: bool) {
    let mut digits = if v == 0 && spec.prec == Some(0) && !pointer {
        Vec::new()
    } else {
        let mut d = uint_digits(v, radix, upper);
        if let Some(p) = spec.prec {
            while d.len() < p {
                d.insert(0, b'0');
            }
        }
        d
    };
    let mut prefix: &[u8] = &[];
    if pointer {
        prefix = b"0x";
    } else if spec.alt {
        if radix == 8 {
            if digits.is_empty() {
                digits.push(b'0');
            } else if digits[0] != b'0' {
                digits.insert(0, b'0');
            }
        } else if radix == 16 && v != 0 {
            prefix = if upper { b"0X" } else { b"0x" };
        }
    }
    push_u16_number(out, &[], prefix, &digits, spec);
}

fn push_float_w(out: &mut Vec<u16>, v: f64, verb: u16, spec: &FmtSpec) {
    let vb = (verb & 0xFF) as u8;
    if v.is_nan() || v.is_infinite() {
        let body = if v.is_nan() {
            if (vb as char).is_ascii_uppercase() { "NAN" } else { "nan" }
        } else if (vb as char).is_ascii_uppercase() { "INF" } else { "inf" };
        let mut plain = *spec;
        plain.zero = false;
        let sign: &[u8] = if v.is_sign_negative() {
            b"-"
        } else if spec.plus {
            b"+"
        } else if spec.space {
            b" "
        } else {
            b""
        };
        let mut tmp: Vec<u8> = Vec::new();
        emit_number(&mut tmp, sign, &[], body.as_bytes(), &plain);
        out.extend(tmp.iter().map(|&b| b as u16));
        return;
    }
    let neg = v < 0.0 || (v == 0.0 && v.is_sign_negative());
    let a = v.abs();
    let sign: &[u8] = if neg {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    };
    let prec = spec.prec.unwrap_or(6).min(MAX_FIELD);
    let mut body: Vec<u8> = match vb {
        0x66 | 0x46 => float_fixed(a, prec, spec.alt),
        0x65 | 0x45 => float_sci(a, prec, spec.alt, vb == 0x45),
        0x67 | 0x47 => float_general(a, prec, spec.alt, vb == 0x47),
        _ => float_hex(a, spec.prec, spec.alt, vb == 0x41),
    };
    if matches!(vb, 0x46 | 0x45 | 0x47) {
        body.make_ascii_uppercase();
    }
    let mut tmp: Vec<u8> = Vec::new();
    emit_number(&mut tmp, sign, &[], &body, spec);
    out.extend(tmp.iter().map(|&b| b as u16));
}

/// Render a NUL-terminated wide format with `args` into UTF-16 units. `None` means
/// the format needs an argument the (NULL) list cannot supply, a field is
/// unbounded, or the verb is unknown.
unsafe fn render_wide_format(format: *const u16, args: Option<*const u64>) -> Option<Vec<u16>> {
    let mut out: Vec<u16> = Vec::new();
    let mut ap = VaReader {
        cur: args.unwrap_or(std::ptr::null()),
        exhausted: args.is_none(),
    };
    let mut i = 0usize;
    loop {
        let w = *format.add(i);
        if w == 0 {
            break;
        }
        if w != '%' as u16 {
            out.push(w);
            i += 1;
            continue;
        }
        i += 1;
        if *format.add(i) == 0 {
            break;
        }
        if *format.add(i) == '%' as u16 {
            out.push('%' as u16);
            i += 1;
            continue;
        }
        render_one_wspec(format, &mut i, &mut ap, &mut out)?;
    }
    Some(out)
}

unsafe fn render_one_wspec(
    format: *const u16,
    pos: &mut usize,
    ap: &mut VaReader,
    out: &mut Vec<u16>,
) -> Option<()> {
    let mut spec = FmtSpec {
        left: false,
        plus: false,
        space: false,
        alt: false,
        zero: false,
        width: None,
        prec: None,
    };
    loop {
        match *format.add(*pos) {
            0x2D => spec.left = true,   // '-'
            0x2B => spec.plus = true,   // '+'
            0x20 => spec.space = true,  // ' '
            0x23 => spec.alt = true,    // '#'
            0x30 => spec.zero = true,   // '0'
            _ => break,
        }
        *pos += 1;
    }
    if *format.add(*pos) == '*' as u16 {
        *pos += 1;
        let w = ap.next()? as u32 as i32;
        if w < 0 {
            spec.left = true;
            let mag = (w as i64).unsigned_abs() as usize;
            if mag > MAX_FIELD {
                return None;
            }
            spec.width = Some(mag);
        } else {
            let wu = w as usize;
            if wu > MAX_FIELD {
                return None;
            }
            spec.width = Some(wu);
        }
    } else {
        let mut w = 0usize;
        let mut any = false;
        while (*format.add(*pos) as u8).is_ascii_digit() && *format.add(*pos) < 0x80 {
            any = true;
            w = w
                .saturating_mul(10)
                .saturating_add((*format.add(*pos) - '0' as u16) as usize);
            if w > MAX_FIELD {
                return None;
            }
            *pos += 1;
        }
        if any {
            spec.width = Some(w);
        }
    }
    if *format.add(*pos) == '.' as u16 {
        *pos += 1;
        if *format.add(*pos) == '*' as u16 {
            *pos += 1;
            let p = ap.next()? as u32 as i32;
            if p >= 0 {
                let pu = p as usize;
                if pu > MAX_FIELD {
                    return None;
                }
                spec.prec = Some(pu);
            }
        } else {
            let mut p = 0usize;
            while (*format.add(*pos) as u8).is_ascii_digit() && *format.add(*pos) < 0x80 {
                p = p
                    .saturating_mul(10)
                    .saturating_add((*format.add(*pos) - '0' as u16) as usize);
                if p > MAX_FIELD {
                    return None;
                }
                *pos += 1;
            }
            spec.prec = Some(p);
        }
    }
    let mut len = IntLen::Def;
    let mut wide_flag = false;
    let mut narrow_flag = false;
    match *format.add(*pos) {
        0x68 => {
            // 'h'
            *pos += 1;
            if *format.add(*pos) == 0x68 {
                *pos += 1;
                len = IntLen::Hh;
            } else {
                len = IntLen::H;
            }
            // `%hs` = narrow string/char in wide output; `%hhd` etc. keep int width.
            if *format.add(*pos) == 's' as u16 || *format.add(*pos) == 'c' as u16 {
                narrow_flag = true;
            }
        }
        0x6C => {
            // 'l'
            *pos += 1;
            if *format.add(*pos) == 0x6C {
                *pos += 1;
                len = IntLen::Ll;
            } else {
                len = IntLen::L;
                wide_flag = true;
            }
        }
        0x77 => {
            // 'w' (`%ws` = wide string, Windows extension)
            *pos += 1;
            wide_flag = true;
        }
        0x6A | 0x7A | 0x74 => {
            // 'j' | 'z' | 't'
            *pos += 1;
            len = IntLen::Ll;
        }
        0x4C => {
            // 'L'
            *pos += 1;
            len = IntLen::Ll;
        }
        0x49 => {
            // 'I'
            *pos += 1;
            if *format.add(*pos) == '3' as u16 && *format.add(*pos + 1) == '2' as u16 {
                *pos += 2;
                len = IntLen::L;
            } else if *format.add(*pos) == '6' as u16 && *format.add(*pos + 1) == '4' as u16 {
                *pos += 2;
                len = IntLen::Ll;
            } else {
                len = IntLen::Ll;
            }
        }
        _ => {}
    }
    let _ = wide_flag;
    let verb = *format.add(*pos);
    if verb == 0 {
        return Some(());
    }
    *pos += 1;
    match verb {
        0x64 | 0x69 => push_signed_w(out, len.signed(ap.next()?), &spec), // d,i
        0x75 => push_unsigned_w(out, len.unsigned(ap.next()?), 10, false, &spec, false), // u
        0x6F => push_unsigned_w(out, len.unsigned(ap.next()?), 8, false, &spec, false), // o
        0x78 => push_unsigned_w(out, len.unsigned(ap.next()?), 16, false, &spec, false), // x
        0x58 => push_unsigned_w(out, len.unsigned(ap.next()?), 16, true, &spec, false), // X
        0x70 => push_unsigned_w(out, ap.next()?, 16, false, &spec, true), // p
        0x63 | 0x43 => {
            // c,C: wide unless `%hc`
            if narrow_flag {
                let b = (ap.next()? & 0xFF) as u8;
                push_u16_str(out, &(b as char).to_string(), &spec);
            } else {
                let w = ap.next()? as u16;
                if (0xD800..0xE000).contains(&w) {
                    push_u16_str(out, "\u{FFFD}", &spec);
                } else {
                    let s = String::from_utf16_lossy(&[w]);
                    push_u16_str(out, &s, &spec);
                }
            }
        }
        0x73 | 0x53 => {
            // s,S: wide unless `%hs` (narrow); bare `%S` is narrow in wprintf.
            let is_narrow = narrow_flag || verb == 0x53;
            if is_narrow {
                let s = guest_narrow_to_string_capped(ap.next()? as *const u8, spec.prec);
                push_u16_str(out, &s, &spec);
            } else {
                let s = guest_wide_to_string_capped(ap.next()? as *const u16, spec.prec);
                push_u16_str(out, &s, &spec);
            }
        }
        0x66 | 0x46 | 0x65 | 0x45 | 0x67 | 0x47 | 0x61 | 0x41 => {
            push_float_w(out, f64::from_bits(ap.next()?), verb, &spec)
        }
        0x6E => {
            // Never honored: consume the pointer slot, write nothing.
            ap.next()?;
        }
        _ => return None,
    }
    Some(())
}

// ---------------------------------------------------------------------------------------
// Scan (`sscanf` family) slice. `__stdio_common_vsscanf` is the UCRT entry the title's
// `sscanf`/`_snscanf` wrappers land on. The auto-stub returned 0, so every scan read
// nothing (the language/config init saw empty fields). This implements the C-locale
// conversions the CRT wrappers can emit, writing through the caller's `va_list`
// (one 8-byte stack slot per argument, like `vfprintf` above).
// ---------------------------------------------------------------------------------------

/// Store width of a scanf length modifier, in bytes (`l` is Windows `long`, 32-bit).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanLen {
    Byte,
    Word,
    Dword,
    Qword,
}

/// `scanf` destination writes (guest memory is mapped in this process). A null
/// destination is skipped, never dereferenced.
unsafe fn store_signed(ptr: usize, v: i64, len: ScanLen) {
    if ptr == 0 {
        return;
    }
    match len {
        ScanLen::Byte => *(ptr as *mut i8) = v as i8,
        ScanLen::Word => *(ptr as *mut i16) = v as i16,
        ScanLen::Dword => *(ptr as *mut i32) = v as i32,
        ScanLen::Qword => *(ptr as *mut i64) = v,
    }
}

unsafe fn store_unsigned(ptr: usize, v: u64, len: ScanLen) {
    if ptr == 0 {
        return;
    }
    match len {
        ScanLen::Byte => *(ptr as *mut u8) = v as u8,
        ScanLen::Word => *(ptr as *mut u16) = v as u16,
        ScanLen::Dword => *(ptr as *mut u32) = v as u32,
        ScanLen::Qword => *(ptr as *mut u64) = v,
    }
}

struct Scan<'a> {
    input: &'a [u8],
    pos: usize,
    assigned: i32,
    args: VaReader,
}

impl<'a> Scan<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn skip_space(&mut self) {
        while !self.at_end() && is_c_space(self.input[self.pos] as i32) {
            self.pos += 1;
        }
    }

    /// Field width caps the characters a conversion may consume.
    fn field_end(&self, width: Option<usize>) -> usize {
        match width {
            Some(w) => self.pos.saturating_add(w).min(self.input.len()),
            None => self.input.len(),
        }
    }

    /// Input failure before the first conversion is `EOF`; afterwards the count.
    fn input_fail(&self) -> i32 {
        if self.assigned == 0 {
            -1
        } else {
            self.assigned
        }
    }

    fn next_arg(&mut self) -> Option<usize> {
        self.args.next().map(|v| v as usize)
    }
}

/// Parse an integer token (optional sign, base from the verb; `%i` auto-detects
/// `0x`/leading `0`). Returns the magnitude and sign; `None` when no digit matched.
fn scan_integer(input: &[u8], pos: &mut usize, width: Option<usize>, verb: u8) -> Option<(u128, bool)> {
    let end = match width {
        Some(w) => pos.saturating_add(w).min(input.len()),
        None => input.len(),
    };
    let mut p = *pos;
    if p >= end {
        return None;
    }
    let mut neg = false;
    if input[p] == b'+' {
        p += 1;
    } else if input[p] == b'-' {
        neg = true;
        p += 1;
    }
    let mut base: u32 = match verb {
        b'd' | b'u' => 10,
        b'o' => 8,
        b'x' | b'X' | b'p' => 16,
        b'i' => 0, // auto-detect
        _ => 10,
    };
    if base == 0 {
        if p + 1 < end && input[p] == b'0' && matches!(input[p + 1], b'x' | b'X') {
            base = 16;
            p += 2;
        } else if p < end && input[p] == b'0' {
            base = 8;
        } else {
            base = 10;
        }
    } else if base == 16 && p + 1 < end && input[p] == b'0' && matches!(input[p + 1], b'x' | b'X') {
        p += 2;
    }
    let digits = p;
    let mut mag: u128 = 0;
    while p < end {
        let d = match input[p] {
            b'0'..=b'9' => input[p] - b'0',
            b'a'..=b'f' => input[p] - b'a' + 10,
            b'A'..=b'F' => input[p] - b'A' + 10,
            _ => break,
        };
        if u32::from(d) >= base {
            break;
        }
        mag = mag.saturating_mul(base as u128).saturating_add(d as u128);
        p += 1;
    }
    if p == digits {
        return None;
    }
    *pos = p;
    Some((mag, neg))
}

/// Parse one floating token, decimal (`strtod` grammar) or hex (`%a`).
fn scan_float(input: &[u8], pos: &mut usize, width: Option<usize>) -> Option<f64> {
    let end = match width {
        Some(w) => pos.saturating_add(w).min(input.len()),
        None => input.len(),
    };
    let start = *pos;
    let mut p = start;
    while p < end && matches!(input[p], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E' | b'a'..=b'd' | b'f' | b'x' | b'X' | b'p' | b'P' | b'i' | b'n' | b'A'..=b'D' | b'F' | b'I' | b'N' | b't' | b'y' | b'T' | b'Y') {
        p += 1;
    }
    if p == start {
        return None;
    }
    let token = &input[start..p];
    // Try the whole token first, then shorter prefixes: the scan above is deliberately
    // over-inclusive (hex digits, inf/nan letters) and trailing junk belongs to the next
    // format conversion (`"1.5f"` scans 1.5 and leaves `f`).
    for cut in (1..=token.len()).rev() {
        let Ok(text) = std::str::from_utf8(&token[..cut]) else { continue };
        let v = if text.trim_start_matches(|c| c == '+' || c == '-').starts_with("0x") || text.trim_start_matches(|c| c == '+' || c == '-').starts_with("0X") {
            parse_hex_float(text)
        } else {
            text.parse::<f64>().ok()
        };
        if let Some(v) = v {
            *pos = start + cut;
            return Some(v);
        }
    }
    None
}

/// Binary exponent of a hex float, saturating at the `i32` range so a huge
/// exponent still yields `inf`/`0` instead of failing the whole conversion.
fn parse_hex_exponent(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    if i == bytes.len() {
        return None;
    }
    let mut v: i64 = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            return None;
        }
        v = v.saturating_mul(10).saturating_add((bytes[i] - b'0') as i64);
        if v > i32::MAX as i64 {
            v = i32::MAX as i64;
        }
        i += 1;
    }
    Some(if neg { -v } else { v } as i32)
}

/// Hex float (`0x1.8p3`), what `%a` scans. Sign, `0x`, hex digits/fraction, `p` exponent.
fn parse_hex_float(s: &str) -> Option<f64> {
    let (neg, rest) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let body = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))?;
    let (mantissa, exp) = match body.find(|c| c == 'p' || c == 'P') {
        Some(k) => (&body[..k], parse_hex_exponent(&body[k + 1..])?),
        None => (body, 0),
    };
    let (whole, frac) = match mantissa.find('.') {
        Some(k) => (&mantissa[..k], &mantissa[k + 1..]),
        None => (mantissa, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    let mut m = 0f64;
    for b in whole.bytes() {
        m = m * 16.0 + f64::from((b as char).to_digit(16)?);
    }
    let mut scale = 1.0 / 16.0;
    for b in frac.bytes() {
        m += f64::from((b as char).to_digit(16)?) * scale;
        scale /= 16.0;
    }
    let v = m * 2f64.powi(exp);
    Some(if neg { -v } else { v })
}

/// `int __stdio_common_vsscanf(options, buffer, count, format, locale, arglist)` —
/// scan `buffer` (at most `count` characters, `SIZE_MAX` = unbounded) per the C
/// format into the `arglist` pointers; returns the assigned-field count, `-1` on an
/// input failure before the first field, or the count so far on a matching failure.
pub unsafe extern "win64" fn __stdio_common_vsscanf(
    _options: u64,
    buffer: *const u8,
    count: usize,
    format: *const u8,
    _locale: usize,
    arglist: *const u64,
) -> i32 {
    if buffer.is_null() || format.is_null() {
        return -1;
    }
    // `count` is the buffer size; for `sscanf` it is SIZE_MAX (unbounded).
    let max = if count == usize::MAX { usize::MAX } else { count };
    let mut len = 0usize;
    while len < max && *buffer.add(len) != 0 {
        len += 1;
    }
    let input = std::slice::from_raw_parts(buffer, len);
    let mut s = Scan {
        input,
        pos: 0,
        assigned: 0,
        args: VaReader { cur: arglist, exhausted: arglist.is_null() },
    };
    let mut i = 0usize;
    loop {
        let f = *format.add(i);
        if f == 0 {
            break;
        }
        if is_c_space(f as i32) {
            s.skip_space();
            i += 1;
            continue;
        }
        if f != b'%' {
            if s.at_end() || s.input[s.pos] != f {
                break;
            }
            s.pos += 1;
            i += 1;
            continue;
        }
        i += 1;
        let mut fb = *format.add(i);
        if fb == 0 {
            break; // trailing '%'
        }
        if fb == b'%' {
            if s.at_end() || s.input[s.pos] != b'%' {
                break;
            }
            s.pos += 1;
            i += 1;
            continue;
        }
        let suppress = fb == b'*';
        if suppress {
            i += 1;
            fb = *format.add(i);
        }
        let mut width: Option<usize> = None;
        while fb.is_ascii_digit() {
            width = Some(width.unwrap_or(0).saturating_mul(10).saturating_add(usize::from(fb - b'0')));
            i += 1;
            fb = *format.add(i);
        }
        if width == Some(0) {
            // undefined in C; treat like an unmatchable field
            break;
        }
        let mut len_mod = ScanLen::Dword;
        let mut wide = false;
        if fb == b'h' {
            i += 1;
            if *format.add(i) == b'h' {
                len_mod = ScanLen::Byte;
                i += 1;
            } else {
                len_mod = ScanLen::Word;
            }
        } else if fb == b'l' {
            i += 1;
            if *format.add(i) == b'l' {
                len_mod = ScanLen::Qword;
                i += 1;
            } else {
                wide = true; // `%ls`/`%lc` wide; `%lf` double
            }
        } else if fb == b'L' {
            len_mod = ScanLen::Qword;
            i += 1;
        } else if matches!(fb, b'j' | b'z' | b't') {
            len_mod = ScanLen::Qword;
            i += 1;
        } else if fb == b'I' {
            i += 1;
            if *format.add(i) == b'6' && *format.add(i + 1) == b'4' {
                len_mod = ScanLen::Qword;
                i += 2;
            } else if *format.add(i) == b'3' && *format.add(i + 1) == b'2' {
                len_mod = ScanLen::Dword;
                i += 2;
            } else {
                len_mod = ScanLen::Qword;
            }
        }
        let verb = *format.add(i);
        if verb == 0 {
            break;
        }
        i += 1;
        // `%c`, `%[` and `%n` never skip leading whitespace.
        if !matches!(verb, b'c' | b'[' | b'n') {
            s.skip_space();
        }
        match verb {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let Some((mag, neg)) = scan_integer(s.input, &mut s.pos, width, verb) else {
                    if s.pos >= s.input.len() {
                        return s.input_fail();
                    }
                    return s.assigned;
                };
                if !suppress {
                    let Some(ptr) = s.next_arg() else { return s.assigned };
                    if matches!(verb, b'd' | b'i') {
                        let m = mag.min(i64::MAX as u128) as i64;
                        store_signed(ptr, if neg { m.wrapping_neg() } else { m }, len_mod);
                    } else {
                        let m = mag.min(u64::MAX as u128) as u64;
                        // `%p` is a pointer conversion: UCRT stores
                        // pointer-width regardless of the (absent) length
                        // modifier.
                        let len = if verb == b'p' { ScanLen::Qword } else { len_mod };
                        store_unsigned(ptr, if neg { m.wrapping_neg() } else { m }, len);
                    }
                    s.assigned += 1;
                }
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                let Some(v) = scan_float(s.input, &mut s.pos, width) else {
                    if s.pos >= s.input.len() {
                        return s.input_fail();
                    }
                    return s.assigned;
                };
                if !suppress {
                    let Some(ptr) = s.next_arg() else { return s.assigned };
                    if wide || len_mod == ScanLen::Qword {
                        if ptr != 0 {
                            *(ptr as *mut f64) = v;
                        }
                    } else if ptr != 0 {
                        *(ptr as *mut f32) = v as f32;
                    }
                    s.assigned += 1;
                }
            }
            b'c' => {
                let w = width.unwrap_or(1);
                if s.pos.saturating_add(w) > s.input.len() {
                    return s.input_fail();
                }
                if !suppress {
                    let Some(ptr) = s.next_arg() else { return s.assigned };
                    if ptr != 0 {
                        if wide {
                            for k in 0..w {
                                *(ptr as *mut u16).add(k) = u16::from(s.input[s.pos + k]);
                            }
                        } else {
                            std::ptr::copy_nonoverlapping(s.input.as_ptr().add(s.pos), ptr as *mut u8, w);
                        }
                    }
                    s.assigned += 1;
                }
                s.pos += w;
            }
            b's' => {
                let end = s.field_end(width);
                let start = s.pos;
                let mut p = start;
                while p < end && !is_c_space(s.input[p] as i32) {
                    p += 1;
                }
                if p == start {
                    return s.assigned;
                }
                if !suppress {
                    let Some(ptr) = s.next_arg() else { return s.assigned };
                    if ptr != 0 {
                        if wide {
                            for k in 0..p - start {
                                *(ptr as *mut u16).add(k) = u16::from(s.input[start + k]);
                            }
                            *(ptr as *mut u16).add(p - start) = 0;
                        } else {
                            std::ptr::copy_nonoverlapping(s.input.as_ptr().add(start), ptr as *mut u8, p - start);
                            *(ptr as *mut u8).add(p - start) = 0;
                        }
                    }
                    s.assigned += 1;
                }
                s.pos = p;
            }
            b'[' => {
                let mut set = [false; 256];
                let mut negate = false;
                let mut fb = *format.add(i);
                if fb == b'^' {
                    negate = true;
                    i += 1;
                }
                let mut first = true;
                loop {
                    let lo = *format.add(i);
                    if lo == 0 {
                        return s.assigned; // unterminated set
                    }
                    if lo == b']' && !first {
                        i += 1;
                        break;
                    }
                    first = false;
                    i += 1;
                    let next = *format.add(i);
                    if next == b'-' && *format.add(i + 1) != b']' && *format.add(i + 1) != 0 {
                        let hi = *format.add(i + 1);
                        for c in lo..=hi {
                            set[c as usize] = true;
                        }
                        i += 2;
                    } else {
                        set[lo as usize] = true;
                    }
                }
                let end = s.field_end(width);
                let start = s.pos;
                let mut p = start;
                while p < end && set[s.input[p] as usize] != negate {
                    p += 1;
                }
                if p == start {
                    return s.assigned;
                }
                if !suppress {
                    let Some(ptr) = s.next_arg() else { return s.assigned };
                    if ptr != 0 {
                        if wide {
                            for k in 0..p - start {
                                *(ptr as *mut u16).add(k) = u16::from(s.input[start + k]);
                            }
                            *(ptr as *mut u16).add(p - start) = 0;
                        } else {
                            std::ptr::copy_nonoverlapping(s.input.as_ptr().add(start), ptr as *mut u8, p - start);
                            *(ptr as *mut u8).add(p - start) = 0;
                        }
                    }
                    s.assigned += 1;
                }
                s.pos = p;
            }
            b'n' => {
                if !suppress {
                    let Some(ptr) = s.next_arg() else { return s.assigned };
                    store_signed(ptr, s.pos as i64, len_mod);
                }
            }
            _ => {
                // `%q`-style extension or a broken format: stop honestly.
                static UNSUPPORTED: AtomicUsize = AtomicUsize::new(0);
                if UNSUPPORTED.fetch_add(1, Ordering::Relaxed) < 4 {
                    crate::xlog_warn!("sscanf: conversion %{} is not supported – scanning stopped at field {}", verb as char, s.assigned);
                }
                return s.assigned;
            }
        }
    }
    s.assigned
}

/// `int __stdio_common_vswprintf_s(options, buffer, count, format, locale, args)` —
/// secure wide format; `-1` on any failure (buffer emptied), else chars written.
pub unsafe extern "win64" fn __stdio_common_vswprintf_s(
    _options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: usize,
    arglist: *const u8,
) -> i32 {
    if format.is_null() {
        if !buffer.is_null() && count > 0 {
            *buffer = 0;
        }
        return -1;
    }
    if buffer.is_null() && count != 0 {
        return -1;
    }
    if count == 0 {
        // Zero-size secure buffer: nothing to write, but aBAB empty format succeeds.
        if wlen(format) == 0 {
            return 0;
        }
        return -1;
    }
    let slots = if arglist.is_null() {
        None
    } else {
        Some(arglist as *const u64)
    };
    let rendered = match render_wide_format(format, slots) {
        Some(v) => v,
        None => {
            *buffer = 0;
            return -1;
        }
    };
    if rendered.len() > i32::MAX as usize {
        *buffer = 0;
        return -1;
    }
    if rendered.len() + 1 > count {
        *buffer = 0;
        return -1;
    }
    std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, rendered.len());
    *buffer.add(rendered.len()) = 0;
    rendered.len() as i32
}

/// `int __stdio_common_vswprintf(options, buffer, count, format, locale, args)` —
/// sized wide format; truncates to `count` (NUL-included) and returns `-1` when the
/// output does not fit, else chars written.
pub unsafe extern "win64" fn __stdio_common_vswprintf(
    _options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: usize,
    arglist: *const u8,
) -> i32 {
    if format.is_null() || (buffer.is_null() && count != 0) {
        return -1;
    }
    let slots = if arglist.is_null() {
        None
    } else {
        Some(arglist as *const u64)
    };
    let rendered = match render_wide_format(format, slots) {
        Some(v) => v,
        None => return -1,
    };
    if rendered.len() > i32::MAX as usize {
        return -1;
    }
    if count == 0 {
        return if rendered.is_empty() { 0 } else { -1 };
    }
    if count == usize::MAX {
        if buffer.is_null() {
            return -1;
        }
        std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, rendered.len());
        *buffer.add(rendered.len()) = 0;
        return rendered.len() as i32;
    }
    if rendered.len() + 1 > count {
        if count > 0 && !buffer.is_null() {
            let keep = count - 1;
            std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, keep.min(rendered.len()));
            *buffer.add(keep) = 0;
        }
        return -1;
    }
    std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, rendered.len());
    *buffer.add(rendered.len()) = 0;
    rendered.len() as i32
}

/// `int __stdio_common_vsprintf_s(options, buffer, count, format, locale, args)` —
/// secure narrow format; `-1` on any failure (buffer emptied), else bytes written.
pub unsafe extern "win64" fn __stdio_common_vsprintf_s(
    _options: u64,
    buffer: *mut u8,
    count: usize,
    format: *const u8,
    _locale: usize,
    arglist: *const u8,
) -> i32 {
    if format.is_null() {
        if !buffer.is_null() && count > 0 {
            *buffer = 0;
        }
        return -1;
    }
    if buffer.is_null() && count != 0 {
        return -1;
    }
    if count == 0 {
        if *format == 0 {
            return 0;
        }
        return -1;
    }
    let slots = if arglist.is_null() {
        None
    } else {
        Some(arglist as *const u64)
    };
    let rendered = match render_narrow_format(format, slots) {
        Some(v) => v,
        None => {
            *buffer = 0;
            return -1;
        }
    };
    if rendered.len() > i32::MAX as usize {
        *buffer = 0;
        return -1;
    }
    if rendered.len() + 1 > count {
        *buffer = 0;
        return -1;
    }
    std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, rendered.len());
    *buffer.add(rendered.len()) = 0;
    rendered.len() as i32
}

/// `int __stdio_common_vsprintf(options, buffer, count, format, locale, args)` —
/// sized narrow format; truncates to `count` (NUL-included) and returns `-1` when
/// the output does not fit, else bytes written. `(size_t)-1` means unbounded.
pub unsafe extern "win64" fn __stdio_common_vsprintf(
    _options: u64,
    buffer: *mut u8,
    count: usize,
    format: *const u8,
    _locale: usize,
    arglist: *const u8,
) -> i32 {
    if format.is_null() || (buffer.is_null() && count != 0 && count != usize::MAX) {
        return -1;
    }
    let slots = if arglist.is_null() {
        None
    } else {
        Some(arglist as *const u64)
    };
    let rendered = match render_narrow_format(format, slots) {
        Some(v) => v,
        None => return -1,
    };
    if rendered.len() > i32::MAX as usize {
        return -1;
    }
    if count == 0 {
        return if rendered.is_empty() { 0 } else { -1 };
    }
    if count == usize::MAX {
        if buffer.is_null() {
            return -1;
        }
        std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, rendered.len());
        *buffer.add(rendered.len()) = 0;
        return rendered.len() as i32;
    }
    if rendered.len() + 1 > count {
        if count > 0 && !buffer.is_null() {
            let keep = count - 1;
            std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, keep.min(rendered.len()));
            *buffer.add(keep) = 0;
        }
        return -1;
    }
    std::ptr::copy_nonoverlapping(rendered.as_ptr(), buffer, rendered.len());
    *buffer.add(rendered.len()) = 0;
    rendered.len() as i32
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
        ("_time64", _time64 as usize),
        ("_gmtime64", _gmtime64 as usize),
        ("_set_fmode", _set_fmode as usize),
        (
            "_register_thread_local_exe_atexit_callback",
            _register_thread_local_exe_atexit_callback as usize,
        ),
        ("?_Random_device@std@@YAIXZ", random_device_std as usize),
        ("_set_app_type", _set_app_type as usize),
        ("_set_invalid_parameter_handler", _set_invalid_parameter_handler as usize),
        ("_get_invalid_parameter_handler", _get_invalid_parameter_handler as usize),
        ("_invalid_parameter", _invalid_parameter as usize),
        ("_invalid_parameter_noinfo", _invalid_parameter_noinfo as usize),
        ("_invoke_watson", _invoke_watson as usize),
        ("_purecall", _purecall as usize),
        ("_set_purecall_handler", _set_purecall_handler as usize),
        ("_get_purecall_handler", _get_purecall_handler as usize),
        ("terminate", terminate as usize),
        ("_set_terminate", _set_terminate as usize),
        ("_get_terminate", _get_terminate as usize),
        ("unexpected", unexpected as usize),
        ("_set_unexpected", _set_unexpected as usize),
        ("_get_unexpected", _get_unexpected as usize),
        ("_CxxThrowException", _CxxThrowException as usize),
        ("__CxxFrameHandler3", __CxxFrameHandler3 as usize),
        ("__CxxFrameHandler4", __CxxFrameHandler4 as usize),
        ("_crt_atexit", _crt_atexit as usize),
        ("_register_onexit_function", _register_onexit_function as usize),
        ("_initialize_onexit_table", _initialize_onexit_table as usize),
        ("_execute_onexit_table", _execute_onexit_table as usize),
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
        ("??2@YAPAXI@Z", msvc_new_u32 as usize),
        ("??2@YAPEAX_K@Z", msvc_new_u64 as usize),
        ("??_U@YAPAXI@Z", msvc_new_array_u32 as usize),
        ("??_U@YAPEAX_K@Z", msvc_new_array_u64 as usize),
        ("??3@YAXPAX@Z", msvc_delete as usize),
        ("??3@YAXPEAX@Z", msvc_delete as usize),
        ("??_V@YAXPAX@Z", msvc_delete_array as usize),
        ("??_V@YAXPEAX@Z", msvc_delete_array as usize),
        ("??3@YAXPEAX_K@Z", msvc_delete_sized as usize),
        ("??_V@YAXPEAX_K@Z", msvc_delete_array_sized as usize),
        ("_callnewh", _callnewh as usize),
        ("_set_new_handler", _set_new_handler as usize),
        ("_query_new_handler", _query_new_handler as usize),
        ("memcpy", memcpy as usize),
        ("memmove", memmove as usize),
        ("memcmp", memcmp as usize),
        ("memset", memset as usize),
        ("strlen", strlen as usize),
        ("strnlen", strnlen as usize),
        ("strcmp", strcmp as usize),
        ("strncmp", strncmp as usize),
        ("_strnicmp", _strnicmp as usize),
        ("_stricmp", _stricmp as usize),
        ("isspace", isspace as usize),
        ("isalnum", isalnum as usize),
        ("__pctype_func", __pctype_func as usize),
        ("___lc_codepage_func", ___lc_codepage_func as usize),
        ("_lock_locales", _lock_locales as usize),
        ("_unlock_locales", _unlock_locales as usize),
        ("___mb_cur_max_func", ___mb_cur_max_func as usize),
        ("___lc_locale_name_func", ___lc_locale_name_func as usize),
        ("setlocale", setlocale as usize),
        ("atoi", atoi as usize),
        ("strtod", strtod as usize),
        ("_strtoui64", _strtoui64 as usize),
        ("strtoll", strtoll as usize),
        ("strtoull", strtoull as usize),
        ("_set_errno", _set_errno as usize),
        ("_get_errno", _get_errno as usize),
        ("srand", srand as usize),
        ("rand", rand as usize),
        ("qsort", qsort as usize),
        ("strcpy", strcpy as usize),
        ("strncpy", strncpy as usize),
        ("strncpy_s", strncpy_s as usize),
        ("strcat", strcat as usize),
        ("strncat", strncat as usize),
        ("strchr", strchr as usize),
        ("memchr", memchr as usize),
        ("wcslen", wcslen as usize),
        ("wcsnlen", wcsnlen as usize),
        ("wcscmp", wcscmp as usize),
        ("wcsncmp", wcsncmp as usize),
        ("wcscpy", wcscpy as usize),
        ("wcsncpy", wcsncpy as usize),
        ("wcscat", wcscat as usize),
        ("wcsncat", wcsncat as usize),
        ("wmemcpy", wmemcpy as usize),
        ("wmemmove", wmemmove as usize),
        ("strstr", strstr as usize),
        ("strrchr", strrchr as usize),
        ("strcspn", strcspn as usize),
        ("strtok_s", strtok_s as usize),
        ("strdup", strdup as usize),
        ("_strdup", strdup as usize),
        ("wcschr", wcschr as usize),
        ("wcsrchr", wcsrchr as usize),
        ("wcsstr", wcsstr as usize),
        ("_wcsdup", wcsdup_impl as usize),
        ("wcsdup", wcsdup_impl as usize),
        ("mbrtowc", mbrtowc as usize),
        ("wcrtomb", wcrtomb as usize),
        ("getenv", getenv as usize),
        ("setvbuf", setvbuf as usize),
        ("fopen", fopen as usize),
        ("fread", fread as usize),
        ("fwrite", fwrite as usize),
        ("fputc", fputc as usize),
        ("fflush", fflush as usize),
        ("fclose", fclose as usize),
        ("fseek", fseek as usize),
        ("ftell", ftell as usize),
        ("__stdio_common_vfprintf", __stdio_common_vfprintf as usize),
        ("__stdio_common_vsscanf", __stdio_common_vsscanf as usize),
        ("__stdio_common_vswprintf_s", __stdio_common_vswprintf_s as usize),
        ("__stdio_common_vswprintf", __stdio_common_vswprintf as usize),
        ("__stdio_common_vsprintf_s", __stdio_common_vsprintf_s as usize),
        ("__stdio_common_vsprintf", __stdio_common_vsprintf as usize),
        ("sinf", sinf as usize),
        ("cosf", cosf as usize),
        ("tanf", tanf as usize),
        ("atan2f", atan2f as usize),
        ("powf", powf as usize),
        ("exp2f", exp2f as usize),
        ("log10f", log10f as usize),
        ("log2f", log2f as usize),
        ("fmodf", fmodf as usize),
        ("erff", erff as usize),
        ("atan2", atan2 as usize),
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
            // `__p___argv` is `char***`: one deref is the `argv` array itself.
            let argv_var = __p___argv();
            assert!(!argv_var.is_null() && !(*argv_var).is_null());
            let argv = *argv_var;
            assert!(!(*argv).is_null());
            assert!((*argv.add(1)).is_null());
            assert_eq!(strlen(*argv), 3);
            assert_eq!(__p___argv(), argv_var);
            let wargv_var = __p___wargv();
            assert!(!wargv_var.is_null() && !(*wargv_var).is_null());
            let wargv = *wargv_var;
            assert!(!(*wargv).is_null());
            assert!((*wargv.add(1)).is_null());
            assert!((**__p__environ()).is_null());
            assert!((**__p__wenviron()).is_null());
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
            // Handler storage is global; save/restore so parallel tests don't flake.
            let prev_ip = _set_invalid_parameter_handler(0);
            assert_eq!(_get_invalid_parameter_handler(), 0);
            _set_invalid_parameter_handler(prev_ip);
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

    #[test]
    fn time_rng_and_mode_contracts_are_live() {
        // Retail M1.1a: these names resolved to the generic auto-stub (epoch 0, a
        // constant RNG, no write at all). Wiring is only real when the export table
        // carries the implementations.
        for name in [
            "_time64",
            "_set_fmode",
            "_register_thread_local_exe_atexit_callback",
            "?_Random_device@std@@YAIXZ",
        ] {
            assert!(exports().iter().any(|(n, _)| *n == name), "{name} must be wired");
        }
        unsafe {
            // `_time64` is the live epoch clock and mirrors it through the out-param.
            let mut out = 0i64;
            let t = _time64(&mut out);
            assert_eq!(t, out);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            assert!(t >= 1_577_836_800, "epoch {t} is before 2020-01-01");
            assert!((t - now).abs() <= 2, "epoch {t} vs now {now}");
            assert!(_time64(std::ptr::null_mut()) >= t - 1);
            // `_set_fmode` is a success no-op (CRT stdio is unbacked here).
            assert_eq!(_set_fmode(0x4000), 0);
            assert_eq!(_set_fmode(0x8000), 0);
            // The thread-local exe-atexit registration reports success; XCL runs no
            // CRT thread teardown, so the pointer is accepted and never invoked.
            assert_eq!(_register_thread_local_exe_atexit_callback(0), 0);
            assert_eq!(_register_thread_local_exe_atexit_callback(probe_a as usize), 0);
            // `std::_Random_device` draws live entropy, never the constant 0.
            assert!(os_entropy_u32().is_some(), "no OS entropy source answered");
            let draws: Vec<u32> = (0..64).map(|_| random_device_std()).collect();
            assert!(
                draws.windows(2).any(|w| w[0] != w[1]),
                "constant RNG: {draws:?}"
            );
            // The deterministic fallback still derives distinct values from distinct
            // seeds (it is never the auto-stub's constant).
            assert_ne!(mix64(1), mix64(2));
            assert_ne!(mix64(1), 0);
        }
    }

    #[test]
    fn gmtime64_returns_a_valid_utc_tm() {
        // Retail Step 64: the TLS certificate loader calls `_gmtime64` for the
        // certificate validity window; the auto-stub's null return rejects every
        // certificate and the engine init fails at ~7 s.
        assert!(exports().iter().any(|(n, _)| *n == "_gmtime64"), "_gmtime64 must be wired");
        unsafe {
            let epoch = 0i64;
            let tm = _gmtime64(&epoch);
            assert!(!tm.is_null(), "epoch 0 must convert");
            assert_eq!(((*tm).sec, (*tm).min, (*tm).hour), (0, 0, 0));
            assert_eq!(((*tm).mday, (*tm).mon, (*tm).year), (1, 0, 70));
            assert_eq!((*tm).yday, 0);
            // 2024-02-29T12:34:56Z.
            let leap = 1_709_210_096i64;
            let tm = _gmtime64(&leap);
            assert!(!tm.is_null());
            assert_eq!(((*tm).sec, (*tm).min, (*tm).hour), (56, 34, 12));
            assert_eq!(((*tm).mday, (*tm).mon, (*tm).year), (29, 1, 124));
            // Per-thread buffer: two calls on one thread share storage (MSVC contract).
            assert_eq!(tm, _gmtime64(&epoch));
            assert!(_gmtime64(std::ptr::null()).is_null());
            assert_eq!(*_errno(), EINVAL_WIN);
            let extreme = i64::MAX;
            assert!(_gmtime64(&extreme).is_null(), "out-of-range time must be rejected");
            assert_eq!(*_errno(), EINVAL_WIN);
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

    #[test]
    fn operator_new_delete_roundtrip_and_handlers() {
        unsafe {
            // Scalar + vector, 32/64-bit size forms, all backed by the libc heap.
            let a = msvc_new_u32(32) as *mut u8;
            assert!(!a.is_null());
            memset(a, 0x5A, 32);
            assert_eq!(*a, 0x5A);
            let b = msvc_new_u64(48) as *mut u8;
            assert!(!b.is_null());
            memset(b, 0xA5, 48);
            assert_eq!(*b.add(47), 0xA5);
            let c = msvc_new_array_u32(16) as *mut u8;
            let d = msvc_new_array_u64(16) as *mut u8;
            assert!(!c.is_null() && !d.is_null());
            msvc_delete(a as *mut c_void);
            msvc_delete(b as *mut c_void);
            msvc_delete_array(c as *mut c_void);
            msvc_delete_array(d as *mut c_void);
            // Null deletes are no-ops (C++ delete on nullptr).
            msvc_delete(std::ptr::null_mut());
            msvc_delete_array(std::ptr::null_mut());
            msvc_delete_sized(std::ptr::null_mut(), 8);
            // Sized delete frees a live block.
            let e = msvc_new_u64(24) as *mut c_void;
            msvc_delete_sized(e, 24);
            let f = msvc_new_array_u64(24) as *mut c_void;
            msvc_delete_array_sized(f, 24);
            // New-handler plumbing: none installed -> _callnewh fails closed.
            assert_eq!(_query_new_handler(), 0);
            assert_eq!(_callnewh(64), 0);
            unsafe extern "win64" fn probe(_n: usize) -> i32 {
                1
            }
            assert_eq!(_set_new_handler(probe as usize), 0);
            assert_eq!(_query_new_handler(), probe as usize);
            assert_eq!(_callnewh(64), 1);
            assert_eq!(_set_new_handler(0), probe as usize);
            assert_eq!(_query_new_handler(), 0);
        }
    }

    #[test]
    fn terminate_handlers_store_and_dispatch() {
        unsafe {
            // Frame personalities decline (ContinueSearch) without aborting.
            assert_eq!(__CxxFrameHandler3(), 1);
            assert_eq!(__CxxFrameHandler4(), 1);
            // Purecall / terminate / unexpected handler slots round-trip.
            unsafe extern "win64" fn pure_probe() {}
            unsafe extern "win64" fn term_probe() {}
            unsafe extern "win64" fn unex_probe() {}
            let prev_pure = _set_purecall_handler(pure_probe as usize);
            assert_eq!(_get_purecall_handler(), pure_probe as usize);
            _set_purecall_handler(prev_pure);
            let prev_term = _set_terminate(term_probe as usize);
            assert_eq!(_get_terminate(), term_probe as usize);
            _set_terminate(prev_term);
            let prev_unex = _set_unexpected(unex_probe as usize);
            assert_eq!(_get_unexpected(), unex_probe as usize);
            _set_unexpected(prev_unex);
            // Invalid-parameter dispatch calls the installed handler, no-ops without.
            static HIT: AtomicUsize = AtomicUsize::new(0);
            unsafe extern "win64" fn ip_probe(
                _e: *const u16,
                _f: *const u16,
                _d: *const u16,
                _l: u32,
                _r: usize,
            ) {
                HIT.fetch_add(1, Ordering::SeqCst);
            }
            let prev_ip = _set_invalid_parameter_handler(ip_probe as usize);
            assert_eq!(_get_invalid_parameter_handler(), ip_probe as usize);
            HIT.store(0, Ordering::SeqCst);
            _invalid_parameter(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                0,
            );
            assert_eq!(HIT.load(Ordering::SeqCst), 1);
            HIT.store(0, Ordering::SeqCst);
            _invalid_parameter_noinfo();
            assert_eq!(HIT.load(Ordering::SeqCst), 1);
            _set_invalid_parameter_handler(0);
            _invalid_parameter(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                0,
            );
            _invalid_parameter_noinfo();
            _set_invalid_parameter_handler(prev_ip);
        }
    }

    #[test]
    fn strncmp_trace_preview_renders_bounded_lossy_args() {
        unsafe {
            // Previews stop at NUL and are bounded: the full key is longer
            // than the 64-byte preview cap.
            let long = b"globalroot_extra_long_key_material_padding_0123456789abcdef\0";
            let p = strncmp_arg_preview(long.as_ptr(), 128);
            assert!(p.len() <= 64, "preview must be capped, got {p:?}");
            assert!(p.starts_with("globalroot"), "got {p:?}");
            // Stops at NUL even when n runs past it.
            assert_eq!(strncmp_arg_preview(b"ab\0cd\0".as_ptr(), 5), "ab");
            // Non-UTF8 bytes render lossy instead of panicking.
            assert_eq!(strncmp_arg_preview(b"\xff\xfe\0".as_ptr(), 3), "��");
            // Zero length previews empty.
            assert_eq!(strncmp_arg_preview(b"abc\0".as_ptr(), 0), "");
        }
    }

    #[test]
    fn strncmp_full_reports_beyond_n() {
        unsafe {
            // Step-05: the full input extends past the `n`-byte prefix the
            // dispatcher compares (step-04 only logged `G:/app…`/`packro…`).
            let s = b"packroot:/game/content\0";
            assert_eq!(strncmp_cstr_full(s.as_ptr()), "packroot:/game/content");
            // Stops at NUL and stays lossy + capped like the prefix preview.
            assert_eq!(strncmp_cstr_full(b"ab\0cd\0".as_ptr()), "ab");
            assert_eq!(strncmp_cstr_full(b"\xff\xfe\0".as_ptr()), "��");
            let mut long = vec![b'k'; 300];
            long.push(0);
            let full = strncmp_cstr_full(long.as_ptr());
            assert_eq!(full.len(), STRNCMP_FULL_MAX, "full input is capped, got len {}", full.len());
        }
    }

    #[test]
    fn string_compare_and_search_match_libc() {        unsafe {
            assert_eq!(strcmp(b"abc\0".as_ptr(), b"abc\0".as_ptr()), 0);
            assert!(strcmp(b"abc\0".as_ptr(), b"abd\0".as_ptr()) < 0);
            assert!(strcmp(b"abd\0".as_ptr(), b"abc\0".as_ptr()) > 0);
            assert_eq!(strncmp(b"abcdef\0".as_ptr(), b"abcxyz\0".as_ptr(), 3), 0);
            assert!(strncmp(b"abcdef\0".as_ptr(), b"abcxyz\0".as_ptr(), 4) < 0);
            assert_eq!(strncmp(b"abc\0".as_ptr(), b"abc\0".as_ptr(), 0), 0);
            assert_eq!(_strnicmp(b"AbC\0".as_ptr(), b"aBc\0".as_ptr(), 3), 0);
            assert_eq!(_strnicmp(b"abcdef\0".as_ptr(), b"ABCXYZ\0".as_ptr(), 3), 0);
            assert!(_strnicmp(b"abcdef\0".as_ptr(), b"ABCXYZ\0".as_ptr(), 4) < 0);
            assert!(_strnicmp(b"ABCXYZ\0".as_ptr(), b"abcdef\0".as_ptr(), 4) > 0);
            assert_eq!(_strnicmp(b"abc\0".as_ptr(), b"abc\0".as_ptr(), 0), 0);
            assert_eq!(_strnicmp(b"ab\0x\0".as_ptr(), b"AB\0y\0".as_ptr(), 5), 0);
            // Retail abort 0x2237179: the drive-prefix probe in `exe+0x54F8A70`
            // compares `G:/...` against `xg:`/`xd:` — a stub returning 0
            // ("equal") strips the drive letter and breaks the dump-path lookup.
            assert!(_strnicmp(b"G:/appdata0.rpf\0".as_ptr(), b"xg:\0".as_ptr(), 3) != 0);
            assert!(_strnicmp(b"G:/appdata0.rpf\0".as_ptr(), b"xd:\0".as_ptr(), 3) != 0);
            assert_eq!(_strnicmp(b"XD:foo\0".as_ptr(), b"xd:\0".as_ptr(), 3), 0);
            assert_eq!(_strnicmp(b"XG:foo\0".as_ptr(), b"xg:\0".as_ptr(), 3), 0);
            let wa: [u16; 4] = [0x41, 0x42, 0x43, 0];
            let wb: [u16; 4] = [0x41, 0x42, 0x44, 0];
            assert_eq!(wcscmp(wa.as_ptr(), wa.as_ptr()), 0);
            assert!(wcscmp(wa.as_ptr(), wb.as_ptr()) < 0);
            assert!(wcscmp(wb.as_ptr(), wa.as_ptr()) > 0);
            assert_eq!(wcsncmp(wa.as_ptr(), wb.as_ptr(), 2), 0);
            assert!(wcsncmp(wa.as_ptr(), wb.as_ptr(), 3) < 0);
            let s = b"hello\0";
            assert_eq!(strchr(s.as_ptr(), b'e' as i32), s.as_ptr().add(1) as *mut u8);
            assert_eq!(strchr(s.as_ptr(), 0), s.as_ptr().add(5) as *mut u8);
            assert!(strchr(s.as_ptr(), b'z' as i32).is_null());
            assert_eq!(memchr(s.as_ptr(), b'l' as i32, 5), s.as_ptr().add(2) as *mut u8);
            assert!(memchr(s.as_ptr(), b'l' as i32, 2).is_null());
            assert!(memchr(s.as_ptr(), b'z' as i32, 5).is_null());
        }
    }

    #[test]
    fn string_copy_and_concat_match_libc() {
        unsafe {
            // strcpy returns dst and copies the NUL (never the NULL stub).
            let mut dst = [0u8; 8];
            let src = b"hi\0";
            assert_eq!(strcpy(dst.as_mut_ptr(), src.as_ptr()), dst.as_mut_ptr());
            assert_eq!(&dst[..3], b"hi\0");
            // strncpy pads with NUL; truncation leaves no NUL (UCRT semantics).
            let mut pad = [0x7Fu8; 6];
            assert_eq!(
                strncpy(pad.as_mut_ptr(), b"ab\0".as_ptr(), 5),
                pad.as_mut_ptr()
            );
            assert_eq!(&pad[..5], b"ab\0\0\0");
            let mut trunc = [0u8; 3];
            strncpy(trunc.as_mut_ptr(), b"abcd\0".as_ptr(), 3);
            assert_eq!(&trunc, b"abc");
            // Zero length touches nothing but still returns dst.
            let mut zero = [0x41u8; 2];
            assert_eq!(
                strncpy(zero.as_mut_ptr(), b"zzz\0".as_ptr(), 0),
                zero.as_mut_ptr()
            );
            assert_eq!(&zero, b"AA");
            // strcat/strncat append after the dst NUL and NUL-terminate.
            let mut cat = [0u8; 12];
            strcpy(cat.as_mut_ptr(), b"foo\0".as_ptr());
            assert_eq!(strcat(cat.as_mut_ptr(), b"bar\0".as_ptr()), cat.as_mut_ptr());
            assert_eq!(&cat[..7], b"foobar\0");
            let mut ncat = [0u8; 12];
            strcpy(ncat.as_mut_ptr(), b"foo\0".as_ptr());
            assert_eq!(
                strncat(ncat.as_mut_ptr(), b"barbaz\0".as_ptr(), 3),
                ncat.as_mut_ptr()
            );
            assert_eq!(&ncat[..7], b"foobar\0");
            // strncat with n == 0 only re-terminates.
            strncat(ncat.as_mut_ptr(), b"ZZZ\0".as_ptr(), 0);
            assert_eq!(&ncat[..7], b"foobar\0");
            // Wide copy/concat mirror the narrow semantics in UTF-16 units.
            let mut wdst = [0u16; 8];
            let wsrc: [u16; 3] = [0x48, 0x69, 0];
            assert_eq!(wcscpy(wdst.as_mut_ptr(), wsrc.as_ptr()), wdst.as_mut_ptr());
            assert_eq!(&wdst[..3], &[0x48, 0x69, 0]);
            let mut wpad = [0x7Fu16; 6];
            wcsncpy(wpad.as_mut_ptr(), [0x41, 0].as_ptr(), 4);
            assert_eq!(&wpad[..4], &[0x41, 0, 0, 0]);
            let mut wtrunc = [0u16; 2];
            wcsncpy(wtrunc.as_mut_ptr(), [0x41, 0x42, 0x43, 0].as_ptr(), 2);
            assert_eq!(&wtrunc, &[0x41, 0x42]);
            let mut wcat = [0u16; 12];
            wcscpy(wcat.as_mut_ptr(), [0x66, 0x6F, 0x6F, 0].as_ptr());
            wcscat(wcat.as_mut_ptr(), [0x62, 0x61, 0x72, 0].as_ptr());
            assert_eq!(&wcat[..7], &[0x66, 0x6F, 0x6F, 0x62, 0x61, 0x72, 0]);
            let mut wncat = [0u16; 12];
            wcscpy(wncat.as_mut_ptr(), [0x66, 0x6F, 0x6F, 0].as_ptr());
            wcsncat(wncat.as_mut_ptr(), [0x62, 0x61, 0x72, 0x62, 0x61, 0x7A, 0].as_ptr(), 3);
            assert_eq!(&wncat[..7], &[0x66, 0x6F, 0x6F, 0x62, 0x61, 0x72, 0]);
            // wmemcpy copies units; wmemmove is overlap-safe; zero length is a no-op.
            let wsrc2: [u16; 4] = [1, 2, 3, 4];
            let mut wmem = [0u16; 4];
            assert_eq!(
                wmemcpy(wmem.as_mut_ptr(), wsrc2.as_ptr(), 4),
                wmem.as_mut_ptr()
            );
            assert_eq!(&wmem, &wsrc2);
            let mut wov = [1u16, 2, 3, 4, 0, 0];
            wmemmove(wov.as_mut_ptr().add(1), wov.as_ptr(), 4);
            assert_eq!(&wov[..5], &[1, 1, 2, 3, 4]);
            let mut wzero = [9u16, 9];
            wmemcpy(wzero.as_mut_ptr(), wsrc2.as_ptr(), 0);
            wmemmove(wzero.as_mut_ptr(), wsrc2.as_ptr(), 0);
            assert_eq!(&wzero, &[9, 9]);
        }
    }

    #[test]
    fn strncpy_s_copies_bounded_and_terminates() {
        // Retail Step 79: api-ms-win-crt-string!strncpy_s (IAT 0x146794850)
        // is called 35x/run; the auto-stub returned 0 without copying, so the
        // callers hashed an all-zero 256-byte stack buffer and compared it
        // with a constant name id.
        assert!(
            exports().iter().any(|(n, _)| *n == "strncpy_s"),
            "strncpy_s must be wired"
        );
        type StrncpyS = unsafe extern "win64" fn(*mut u8, usize, *const u8, usize) -> i32;
        let addr = exports()
            .into_iter()
            .find_map(|(n, a)| (n == "strncpy_s").then_some(a))
            .expect("export");
        let copy: StrncpyS = unsafe { std::mem::transmute(addr) };
        unsafe {
            *_errno() = 0;
            // A short source copies with its NUL and pads nothing (unlike
            // strncpy); the bytes after the NUL keep their sentinel.
            let mut dst = [0xAAu8; 8];
            assert_eq!(copy(dst.as_mut_ptr(), 8, b"hi\0".as_ptr(), 7), 0);
            assert_eq!(&dst[..3], b"hi\0");
            assert_eq!(&dst[3..], &[0xAAu8; 5]);
            // count < destsz truncates at count and NUL-terminates there.
            let mut t = [0xAAu8; 8];
            assert_eq!(copy(t.as_mut_ptr(), 8, b"abcdef\0".as_ptr(), 3), 0);
            assert_eq!(&t[..4], b"abc\0");
            assert_eq!(t[4], 0xAA);
            // count >= destsz with a source that fits is still a success.
            let mut s = [0xAAu8; 4];
            assert_eq!(copy(s.as_mut_ptr(), 4, b"ab\0".as_ptr(), 9), 0);
            assert_eq!(&s[..3], b"ab\0");
            assert_eq!(s[3], 0xAA);
            // ... but a source that cannot fit is EINVAL, dest[0] = 0, and
            // nothing past destsz is written.
            let mut e = [0xAAu8; 4];
            assert_eq!(copy(e.as_mut_ptr(), 4, b"abcd\0".as_ptr(), 9), EINVAL_WIN);
            assert_eq!(e, [0, 0xAA, 0xAA, 0xAA]);
            assert_eq!(*_errno(), EINVAL_WIN);
            // count == 0 copies nothing and writes the terminator only.
            let mut z = [0xAAu8; 4];
            assert_eq!(copy(z.as_mut_ptr(), 4, b"xyz\0".as_ptr(), 0), 0);
            assert_eq!(z, [0, 0xAA, 0xAA, 0xAA]);
            // Invalid arguments: null source, zero destsz, null dest.
            let mut n = [0xAAu8; 4];
            assert_eq!(copy(n.as_mut_ptr(), 4, std::ptr::null(), 4), EINVAL_WIN);
            assert_eq!(n, [0, 0xAA, 0xAA, 0xAA]);
            let mut zs = [0xAAu8; 4];
            assert_eq!(copy(zs.as_mut_ptr(), 0, b"x\0".as_ptr(), 1), EINVAL_WIN);
            assert_eq!(zs, [0xAAu8; 4]);
            assert_eq!(
                copy(std::ptr::null_mut(), 4, b"x\0".as_ptr(), 1),
                EINVAL_WIN
            );
            assert_eq!(*_errno(), EINVAL_WIN);
        }
    }

    #[test]
    fn string_search_and_dup_match_libc() {
        unsafe {
            // strstr finds first needle, misses NULL, empty needle returns haystack.
            let hay = b"hello world\0";
            assert_eq!(
                strstr(hay.as_ptr(), b"world\0".as_ptr()),
                hay.as_ptr().add(6) as *mut u8
            );
            assert_eq!(
                strstr(hay.as_ptr(), b"o w\0".as_ptr()),
                hay.as_ptr().add(4) as *mut u8
            );
            assert!(strstr(hay.as_ptr(), b"zzz\0".as_ptr()).is_null());
            assert!(strstr(hay.as_ptr(), b"world!\0".as_ptr()).is_null());
            assert_eq!(
                strstr(hay.as_ptr(), b"\0".as_ptr()),
                hay.as_ptr() as *mut u8
            );
            // strrchr finds the last occurrence; c == 0 returns the terminator.
            assert_eq!(
                strrchr(hay.as_ptr(), b'l' as i32),
                hay.as_ptr().add(9) as *mut u8
            );
            assert_eq!(
                strrchr(hay.as_ptr(), b'h' as i32),
                hay.as_ptr() as *mut u8
            );
            assert_eq!(
                strrchr(hay.as_ptr(), 0),
                hay.as_ptr().add(11) as *mut u8
            );
            assert!(strrchr(hay.as_ptr(), b'z' as i32).is_null());
            // strcspn returns the first rejected index, or the length when clean.
            // Retail exe+0x2237320: the mount check calls
            // strcspn("packroot:/appdata0.rpf", ":...") — 8, not the stub's 0.
            assert_eq!(strcspn(b"packroot:/appdata0.rpf\0".as_ptr(), b":\0".as_ptr()), 8);
            assert_eq!(strcspn(b"appdata0.rpf\0".as_ptr(), b":\0".as_ptr()), 12);
            assert_eq!(strcspn(b"hello\0".as_ptr(), b"aeiou\0".as_ptr()), 1);
            assert_eq!(strcspn(b"hello\0".as_ptr(), b"xyz\0".as_ptr()), 5);
            assert_eq!(strcspn(b"hello\0".as_ptr(), b"\0".as_ptr()), 5);
            assert_eq!(strcspn(b"\0".as_ptr(), b"abc\0".as_ptr()), 0);
            // strtok_s splits on any delimiter, skips runs, NUL-terminates tokens.
            let mut text = *b"a;b;;c\0";
            let delim = b";\0";
            let mut ctx: *mut u8 = std::ptr::null_mut();
            let t1 = strtok_s(text.as_mut_ptr(), delim.as_ptr(), &mut ctx);
            assert_eq!(t1, text.as_mut_ptr());
            assert_eq!(strlen(t1), 1);
            let t2 = strtok_s(std::ptr::null_mut(), delim.as_ptr(), &mut ctx);
            assert_eq!(crate::util::from_cstr_ptr(t2).unwrap(), "b");
            let t3 = strtok_s(std::ptr::null_mut(), delim.as_ptr(), &mut ctx);
            assert_eq!(crate::util::from_cstr_ptr(t3).unwrap(), "c");
            assert!(strtok_s(std::ptr::null_mut(), delim.as_ptr(), &mut ctx).is_null());
            // NULL delim/ctx fails cleanly; NULL start with NULL context ends.
            assert!(strtok_s(text.as_mut_ptr(), std::ptr::null(), &mut ctx).is_null());
            assert!(strtok_s(text.as_mut_ptr(), delim.as_ptr(), std::ptr::null_mut()).is_null());
            let mut ctx2: *mut u8 = std::ptr::null_mut();
            assert!(strtok_s(std::ptr::null_mut(), delim.as_ptr(), &mut ctx2).is_null());
            // strdup/_strdup return an owned equal copy at a different address.
            for dup in [strdup as unsafe extern "win64" fn(*const u8) -> *mut u8] {
                let p = dup(hay.as_ptr());
                assert!(!p.is_null() && p != hay.as_ptr() as *mut u8);
                assert_eq!(strlen(p), strlen(hay.as_ptr()));
                assert_eq!(strcmp(p, hay.as_ptr()), 0);
                free(p as *mut c_void);
            }
            // Wide search mirrors the narrow semantics in UTF-16 units.
            let whay: [u16; 12] = [0x68, 0x65, 0x6C, 0x6C, 0x6F, 0x20, 0x77, 0x6F, 0x72, 0x6C, 0x64, 0];
            assert_eq!(
                wcschr(whay.as_ptr(), 0x6C as i32),
                whay.as_ptr().add(2) as *mut u16
            );
            assert_eq!(
                wcschr(whay.as_ptr(), 0),
                whay.as_ptr().add(11) as *mut u16
            );
            assert!(wcschr(whay.as_ptr(), 0x7A as i32).is_null());
            assert_eq!(
                wcsrchr(whay.as_ptr(), 0x6C as i32),
                whay.as_ptr().add(9) as *mut u16
            );
            assert_eq!(
                wcsrchr(whay.as_ptr(), 0),
                whay.as_ptr().add(11) as *mut u16
            );
            assert!(wcsrchr(whay.as_ptr(), 0x7A as i32).is_null());
            assert!(wcsrchr(std::ptr::null(), 0x61 as i32).is_null());
            assert_eq!(
                wcsstr(whay.as_ptr(), [0x77, 0x6F, 0x72, 0x6C, 0x64, 0].as_ptr()),
                whay.as_ptr().add(6) as *mut u16
            );
            assert!(wcsstr(
                whay.as_ptr(),
                [0x7A, 0x7A, 0x7A, 0].as_ptr()
            )
            .is_null());
            assert_eq!(
                wcsstr(whay.as_ptr(), [0].as_ptr()),
                whay.as_ptr() as *mut u16
            );
            let w = wcsdup_impl(whay.as_ptr());
            assert!(!w.is_null() && w != whay.as_ptr() as *mut u16);
            assert_eq!(wcslen(w), wcslen(whay.as_ptr()));
            assert_eq!(wcscmp(w, whay.as_ptr()), 0);
            free(w as *mut c_void);
        }
    }

    #[test]
    fn convert_and_onexit_match_ucrt() {
        unsafe {
            // mbrtowc: ASCII fast path, NUL handling and stateless probe.
            let mut wc: u16 = 0xFFFF;
            assert_eq!(mbrtowc(&mut wc, b"A\0".as_ptr(), 1, std::ptr::null_mut()), 1);
            assert_eq!(wc, 0x41);
            assert_eq!(mbrtowc(&mut wc, b"\0".as_ptr(), 1, std::ptr::null_mut()), 0);
            assert_eq!(wc, 0);
            assert_eq!(
                mbrtowc(std::ptr::null_mut(), std::ptr::null(), 1, std::ptr::null_mut()),
                0
            );
            assert_eq!(
                mbrtowc(std::ptr::null_mut(), b"A\0".as_ptr(), 1, std::ptr::null_mut()),
                1
            );
            // 2-byte (U+00E9) and 3-byte (U+20AC) UTF-8.
            let e_acute = [0xC3u8, 0xA9, 0];
            assert_eq!(mbrtowc(&mut wc, e_acute.as_ptr(), 2, std::ptr::null_mut()), 2);
            assert_eq!(wc, 0xE9);
            let euro = [0xE2u8, 0x82, 0xAC, 0];
            assert_eq!(mbrtowc(&mut wc, euro.as_ptr(), 3, std::ptr::null_mut()), 3);
            assert_eq!(wc, 0x20AC);
            // Incomplete prefix returns -2 without errno; bad sequences return -1.
            assert_eq!(
                mbrtowc(&mut wc, e_acute.as_ptr(), 1, std::ptr::null_mut()),
                usize::MAX - 1
            );
            assert_eq!(mbrtowc(&mut wc, e_acute.as_ptr(), 0, std::ptr::null_mut()), usize::MAX - 1);
            for bad in [[0xFFu8, 0, 0], [0x80, 0, 0], [0xC0, 0xAF, 0]] {
                *_errno() = 0;
                assert_eq!(
                    mbrtowc(&mut wc, bad.as_ptr(), bad.len() - 1, std::ptr::null_mut()),
                    usize::MAX
                );
                assert_eq!(*_errno(), 42);
            }
            // wcrtomb: ASCII, NUL and multibyte round-trips; NULL probe returns 1.
            let mut buf = [0u8; 8];
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0x41, std::ptr::null_mut()), 1);
            assert_eq!(buf[0], 0x41);
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0, std::ptr::null_mut()), 1);
            assert_eq!(buf[0], 0);
            assert_eq!(wcrtomb(std::ptr::null_mut(), 0, std::ptr::null_mut()), 1);
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0xE9, std::ptr::null_mut()), 2);
            assert_eq!(&buf[..2], &[0xC3, 0xA9]);
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0x20AC, std::ptr::null_mut()), 3);
            assert_eq!(&buf[..3], &[0xE2, 0x82, 0xAC]);
            // Lone low surrogate is an error.
            *_errno() = 0;
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0xDE00, std::ptr::null_mut()), usize::MAX);
            assert_eq!(*_errno(), 42);
            // Surrogate pair (U+1F600) through the restartable state.
            let mut st: u32 = 0;
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0xD83D, &mut st), 0);
            assert_eq!(wcrtomb(buf.as_mut_ptr(), 0xDE00, &mut st), 4);
            assert_eq!(&buf[..4], &[0xF0, 0x9F, 0x98, 0x80]);
            assert_eq!(st, 0);
            // onexit table executes as a no-op success like register/initialize.
            assert_eq!(_execute_onexit_table(0), 0);
        }
    }

    #[test]
    fn getenv_reads_the_process_environment() {
        unsafe {
            std::env::set_var("XCL_CRT_PROBE_VAR", "crt-probe-value");
            let name = c"XCL_CRT_PROBE_VAR";
            let p = getenv(name.as_ptr() as *const u8);
            assert!(!p.is_null());
            assert_eq!(crate::util::from_cstr_ptr(p).as_deref(), Some("crt-probe-value"));
            // Same key keeps working after the host removes it? No: missing reads NULL.
            std::env::remove_var("XCL_CRT_PROBE_VAR");
            assert!(getenv(name.as_ptr() as *const u8).is_null());
            // Null names never fault.
            assert!(getenv(std::ptr::null()).is_null());
        }
    }

    #[test]
    fn setvbuf_reports_success_without_backing() {
        unsafe {
            assert_eq!(setvbuf(std::ptr::null_mut(), std::ptr::null_mut(), 0, 0), 0);
            assert_eq!(__acrt_iob_func(2).is_null(), false);
            assert_eq!(setvbuf(__acrt_iob_func(2), std::ptr::null_mut(), 2, 4096), 0);
        }
    }

    #[test]
    fn fopen_reads_and_writes_through_the_host() {
        unsafe {
            let dir = std::env::temp_dir().join(format!("xcl_crt_11l_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("probe.txt");
            let cpath = format!("{}\0", path.display());
            let f = fopen(cpath.as_ptr(), b"wb\0".as_ptr());
            assert!(!f.is_null());
            let payload = b"hello-crt-11l";
            assert_eq!(
                fwrite(
                    payload.as_ptr() as *const c_void,
                    1,
                    payload.len(),
                    f
                ),
                payload.len()
            );
            assert_eq!(fflush(f), 0);
            assert_eq!(fclose(f), 0);
            let g = fopen(cpath.as_ptr(), b"rb\0".as_ptr());
            assert!(!g.is_null());
            let mut buf = [0u8; 32];
            assert_eq!(
                fread(buf.as_mut_ptr() as *mut c_void, 1, payload.len(), g),
                payload.len()
            );
            assert_eq!(&buf[..payload.len()], payload);
            // Seek + tell round-trip.
            assert_eq!(fseek(g, 6, 0), 0);
            assert_eq!(ftell(g), 6);
            let mut tail = [0u8; 16];
            assert_eq!(
                fread(tail.as_mut_ptr() as *mut c_void, 1, payload.len() - 6, g),
                payload.len() - 6
            );
            assert_eq!(&tail[..payload.len() - 6], &payload[6..]);
            assert_eq!(fclose(g), 0);
            // Missing files fail closed.
            let miss = dir.join("missing.txt");
            let cmiss = format!("{}\0", miss.display());
            assert!(fopen(cmiss.as_ptr(), b"rb\0".as_ptr()).is_null());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn vfprintf_formats_into_crt_files() {
        let dir = std::env::temp_dir().join(format!("xcl_crt_12d_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Format `fmt` with `slots` (win64 va_list: one u64 per argument) into a
        // fresh file; returns the HLE return code plus the file bytes.
        let run = |name: &str, fmt: &[u8], slots: &[u64]| -> (i32, Vec<u8>) {
            unsafe {
                let path = dir.join(name);
                let cpath = format!("{}\0", path.display());
                let f = fopen(cpath.as_ptr(), b"wb\0".as_ptr());
                assert!(!f.is_null());
                let arglist = if slots.is_empty() {
                    std::ptr::null()
                } else {
                    slots.as_ptr() as *const u8
                };
                let rc = __stdio_common_vfprintf(0, f, fmt.as_ptr(), 0, arglist);
                assert_eq!(fclose(f), 0);
                (rc, std::fs::read(&path).unwrap())
            }
        };
        unsafe {
            // Integers, strings, pointers, chars, lengths, literal percent.
            let hello = b"val\0";
            let slots = [
                hello.as_ptr() as u64,
                (-42i32) as u32 as u64,
                42u64,
                0xdead_beefu64,
                0xdead_beefu64,
                0o17u64,
                b'A' as u64,
                0x1234u64,
                0x1122_3344_5566_7788u64,
            ];
            let (rc, bytes) = run(
                "ints.txt",
                b"%s=%d %u %x %X %o %c %% %p %llx\0",
                &slots,
            );
            assert_eq!(
                bytes,
                b"val=-42 42 deadbeef DEADBEEF 17 A % 0x1234 1122334455667788"
            );
            assert_eq!(rc as usize, bytes.len());
            // Width, precision, `*`, signs, floats.
            let two5 = 2.5f64;
            let slots = [3u64, 3, 7, 7, 7, 7, 7, 6, 7, 2, two5.to_bits()];
            let (rc, bytes) = run(
                "width.txt",
                b"[%+d][% d][%5d][%-5d][%05d][%.3d][%8.3d][%*d][%.*f]\0",
                &slots,
            );
            assert_eq!(
                bytes,
                b"[+3][ 3][    7][7    ][00007][007][     007][     7][2.50]"
            );
            assert_eq!(rc as usize, bytes.len());
            // Float verbs, including the title's `%g` and a big-exponent switch.
            let one5 = 1.5f64;
            let big = 1_234_567.0f64;
            let slots = [
                one5.to_bits(),
                one5.to_bits(),
                one5.to_bits(),
                one5.to_bits(),
                big.to_bits(),
            ];
            let (rc, bytes) = run("floats.txt", b"%f|%e|%g|%a|%g\0", &slots);
            assert_eq!(bytes, b"1.500000|1.500000e+00|1.5|0x1.8p+0|1.23457e+06");
            assert_eq!(rc as usize, bytes.len());
            // Windows lengths: `l` stays 32-bit, `ll`/`z` are 64-bit.
            let slots = [
                0x1FFu64,
                0x1FFFFu64,
                0xFFFF_FFFFu64,
                0xFFFF_FFFF_FFFF_FFFFu64,
                0xFFFF_FFFF_FFFF_FFFFu64,
                0x1ABu64,
                0x1ABCDu64,
                0x1DEAD_BEEFu64,
                0x1122_3344_5566_7788u64,
                0x1122_3344_5566_7788u64,
            ];
            let (rc, bytes) = run(
                "lengths.txt",
                b"%hhd %hd %ld %lld %zd %hhx %hx %lx %llx %zx\0",
                &slots,
            );
            assert_eq!(
                bytes,
                b"-1 -1 -1 -1 -1 ab abcd deadbeef 1122334455667788 1122334455667788"
            );
            assert_eq!(rc as usize, bytes.len());
            // Wide strings re-encode as UTF-8; NULL narrow strings print `(null)`.
            let w: [u16; 3] = [0x48, 0x69, 0];
            let slots = [w.as_ptr() as u64, 0u64];
            let (rc, bytes) = run("wide.txt", b"%ls|%s\0", &slots);
            assert_eq!(bytes, b"Hi|(null)");
            assert_eq!(rc as usize, bytes.len());
            // `%n` consumes its slot and writes nothing (sentinel untouched).
            let mut sentinel: u32 = 0x1234_5678;
            let p = &mut sentinel as *mut u32 as u64;
            let (rc, bytes) = run("n.txt", b"AB%nCD\0", &[p]);
            assert_eq!(bytes, b"ABCD");
            assert_eq!(rc, 4);
            assert_eq!(sentinel, 0x1234_5678);
            // Truncated formats stop at the `%` without crashing.
            let (rc, bytes) = run("trunc.txt", b"abc%\0", &[]);
            assert_eq!(bytes, b"abc");
            assert_eq!(rc, 3);
            let (rc, bytes) = run("trunc2.txt", b"abc%5\0", &[]);
            assert_eq!(bytes, b"abc");
            assert_eq!(rc, 3);
            // Unknown verbs and missing arguments are error returns.
            let (rc, _) = run("unknown.txt", b"%q\0", &[]);
            assert_eq!(rc, -1);
            let path = dir.join("nullargs.txt");
            let cpath = format!("{}\0", path.display());
            let f = fopen(cpath.as_ptr(), b"wb\0".as_ptr());
            assert!(!f.is_null());
            assert_eq!(
                __stdio_common_vfprintf(0, f, b"%d\0".as_ptr(), 0, std::ptr::null()),
                -1
            );
            assert_eq!(fclose(f), 0);
            // NULL stream/format are error returns, never faults.
            assert_eq!(
                __stdio_common_vfprintf(
                    0,
                    std::ptr::null_mut(),
                    b"x\0".as_ptr(),
                    0,
                    std::ptr::null()
                ),
                -1
            );
            let path = dir.join("nullfmt.txt");
            let cpath = format!("{}\0", path.display());
            let f = fopen(cpath.as_ptr(), b"wb\0".as_ptr());
            assert!(!f.is_null());
            assert_eq!(
                __stdio_common_vfprintf(0, f, std::ptr::null(), 0, std::ptr::null()),
                -1
            );
            assert_eq!(fclose(f), 0);
            // Options/locale are accepted and ignored (C-locale narrow output).
            let path = dir.join("ignored.txt");
            let cpath = format!("{}\0", path.display());
            let f = fopen(cpath.as_ptr(), b"wb\0".as_ptr());
            assert!(!f.is_null());
            assert_eq!(
                __stdio_common_vfprintf(0xDEAD, f, b"hi\0".as_ptr(), 0x12345, std::ptr::null()),
                2
            );
            assert_eq!(fclose(f), 0);
            assert_eq!(std::fs::read(&path).unwrap(), b"hi");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn common_vsprintf_family_formats_buffers() {
        unsafe {
            fn wide(s: &str) -> Vec<u16> {
                s.encode_utf16().chain(std::iter::once(0)).collect()
            }
            // Wide secure: the dump-table `%s\%s` join used by the retail wrapper.
            let base = wide("D:\\LocalDumps");
            let leaf = wide("crash");
            let fmt = wide("%s\\%s");
            let slots = [base.as_ptr() as u64, leaf.as_ptr() as u64];
            let mut buf = [0u16; 32];
            let n = __stdio_common_vswprintf_s(
                0,
                buf.as_mut_ptr(),
                32,
                fmt.as_ptr(),
                0,
                slots.as_ptr() as *const u8,
            );
            assert_eq!(n, 19);
            assert_eq!(String::from_utf16_lossy(&buf[..19]), "D:\\LocalDumps\\crash");
            // `%llu` + `%s%s` + `%lc%d%lc` + `%n` (consumed, never written).
            let fmt2 = wide("%llu|%s%s|%lc%d%lc%n");
            let wmid = wide("AB");
            let mut count_slot: u32 = 0xDEAD_BEEF;
            let slots2 = [
                123456789012345u64,
                wmid.as_ptr() as u64,
                wmid.as_ptr() as u64,
                0x41u64,
                42u64,
                0x42u64,
                &mut count_slot as *mut u32 as u64,
            ];
            let mut buf2 = [0u16; 64];
            let m = __stdio_common_vswprintf_s(
                0,
                buf2.as_mut_ptr(),
                64,
                fmt2.as_ptr(),
                0,
                slots2.as_ptr() as *const u8,
            );
            assert!(m > 0);
            assert_eq!(
                String::from_utf16_lossy(&buf2[..m as usize]),
                "123456789012345|ABAB|A42B"
            );
            assert_eq!(count_slot, 0xDEAD_BEEF);
            // `%hs` reads a narrow guest string into wide output.
            let narrow = b"narrow\0";
            let fmt3 = wide("[%hs]");
            let slots3 = [narrow.as_ptr() as u64];
            let mut buf3 = [0u16; 16];
            let k = __stdio_common_vswprintf_s(
                0,
                buf3.as_mut_ptr(),
                16,
                fmt3.as_ptr(),
                0,
                slots3.as_ptr() as *const u8,
            );
            assert_eq!(k, 8);
            assert_eq!(String::from_utf16_lossy(&buf3[..8]), "[narrow]");
            // Secure truncation empties the buffer and returns -1.
            let mut tiny = [0xBEEFu16; 4];
            assert_eq!(
                __stdio_common_vswprintf_s(0, tiny.as_mut_ptr(), 4, fmt.as_ptr(), 0, slots.as_ptr() as *const u8),
                -1
            );
            assert_eq!(tiny[0], 0);
            // Unknown verbs and NULL format fail without touching the caller.
            assert_eq!(
                __stdio_common_vswprintf_s(0, buf.as_mut_ptr(), 32, wide("%q").as_ptr(), 0, std::ptr::null()),
                -1
            );
            assert_eq!(
                __stdio_common_vswprintf_s(0, buf.as_mut_ptr(), 32, std::ptr::null(), 0, std::ptr::null()),
                -1
            );
            // Narrow secure twin agrees on the same join.
            let mut mb = [0u8; 32];
            let fmt_n = b"%s\\%s\0";
            let bbase = b"D:\\LocalDumps\0";
            let slots_n = [bbase.as_ptr() as u64, b"crash\0".as_ptr() as u64];
            let q = __stdio_common_vsprintf_s(
                0,
                mb.as_mut_ptr(),
                32,
                fmt_n.as_ptr(),
                0,
                slots_n.as_ptr() as *const u8,
            );
            assert_eq!(q, 19);
            assert_eq!(&mb[..19], b"D:\\LocalDumps\\crash");
            assert_eq!(
                __stdio_common_vsprintf_s(0, mb.as_mut_ptr(), 4, fmt_n.as_ptr(), 0, slots_n.as_ptr() as *const u8),
                -1
            );
            assert_eq!(mb[0], 0);
            // Non-secure twins truncate (wide) / go unbounded (narrow -1 count).
            let mut wtr = [0u16; 4];
            assert_eq!(
                __stdio_common_vswprintf(0, wtr.as_mut_ptr(), 4, fmt.as_ptr(), 0, slots.as_ptr() as *const u8),
                -1
            );
            assert_eq!(String::from_utf16_lossy(&wtr[..3]), "D:\\");
            let mut mbu = [0u8; 32];
            assert_eq!(
                __stdio_common_vsprintf(
                    0, mbu.as_mut_ptr(), usize::MAX, fmt_n.as_ptr(), 0, slots_n.as_ptr() as *const u8,
                ),
                19
            );
            assert_eq!(&mbu[..19], b"D:\\LocalDumps\\crash");
        }
    }

    #[test]
    fn f32_math_matches_reference() {
        unsafe {
            // Spot values against the f64 reference (tighter than the game's
            // SIMD loop needs; the old stubs returned thunk garbage here).
            for &x in &[0.5f32, 1.0, -1.0, 2.5, 100.0] {
                let r = x as f64;
                assert!((sinf(x) - r.sin() as f32).abs() < 1e-6, "sinf({x})");
                assert!((cosf(x) - r.cos() as f32).abs() < 1e-6, "cosf({x})");
                assert!((tanf(x) - r.tan() as f32).abs() < 1e-5, "tanf({x})");
                assert!((exp2f(x) - r.exp2() as f32).abs() < 1e-4, "exp2f({x})");
            }
            for &x in &[0.5f32, 1.0, 2.5, 100.0, 1000.0] {
                assert!((log10f(x) - (x as f64).log10() as f32).abs() < 1e-6, "log10f({x})");
            }
            for &(x, y) in &[(2.0f32, 10.0f32), (0.5, -2.0), (9.0, 0.5), (-2.0, 3.0)] {
                assert!(
                    (powf(x, y) - (x as f64).powf(y as f64) as f32).abs() < 1e-4,
                    "powf({x}, {y})"
                );
            }
            // atan2 quadrants + axes (radians).
            use std::f32::consts::{FRAC_PI_2, PI};
            assert!((atan2f(1.0, 1.0) - FRAC_PI_2 / 2.0).abs() < 1e-6);
            assert!((atan2f(1.0, -1.0) - 3.0 * FRAC_PI_2 / 2.0).abs() < 1e-6);
            assert!((atan2f(-1.0, -1.0) + 3.0 * FRAC_PI_2 / 2.0).abs() < 1e-6);
            assert!((atan2f(-1.0, 1.0) + FRAC_PI_2 / 2.0).abs() < 1e-6);
            assert!((atan2f(1.0, 0.0) - FRAC_PI_2).abs() < 1e-6);
            assert!((atan2f(-1.0, 0.0) + FRAC_PI_2).abs() < 1e-6);
            assert!((atan2f(0.0, -1.0) - PI).abs() < 1e-6);
            assert!((atan2f(-0.0, -1.0) + PI).abs() < 1e-6);
            assert_eq!(atan2f(0.0, 0.0), 0.0);
            assert!(atan2f(f32::NAN, 1.0).is_nan());
            // Zeros keep their sign; infinities/NaN follow IEEE-754 (as UCRT).
            assert_eq!(sinf(0.0).to_bits(), 0.0f32.to_bits());
            assert!(sinf(-0.0f32).is_sign_negative());
            assert!(sinf(f32::INFINITY).is_nan() && sinf(f32::NEG_INFINITY).is_nan());
            assert!(sinf(f32::NAN).is_nan());
            assert_eq!(cosf(0.0), 1.0);
            assert!(cosf(f32::INFINITY).is_nan());
            assert_eq!(tanf(0.0), 0.0);
            assert_eq!(exp2f(10.0), 1024.0);
            assert_eq!(exp2f(f32::NEG_INFINITY), 0.0);
            assert_eq!(exp2f(f32::INFINITY), f32::INFINITY);
            assert!(exp2f(f32::NAN).is_nan());
            assert!((log10f(1000.0) - 3.0).abs() < 1e-6);
            assert_eq!(log10f(0.0), f32::NEG_INFINITY);
            assert_eq!(log10f(f32::INFINITY), f32::INFINITY);
            assert!(log10f(-1.0).is_nan());
            assert_eq!(powf(2.0, 10.0), 1024.0);
            assert_eq!(powf(0.0, 0.0), 1.0);
            assert_eq!(powf(-1.0, f32::INFINITY), 1.0);
        }
    }

    #[test]
    fn erf_fmod_log2_and_atan2_are_real() {
        // Retail Step 78: erff 4,800× (call at exe+0x1cb636c, IAT
        // 0x146794650), log2f 129× (IAT 0x1467946a0), fmodf 52× (IAT
        // 0x146794680) and the double atan2 84× (IAT 0x146794618, f32
        // callers promote through exe+0x623b20) resolved to auto-stubs that
        // returned 0 without touching xmm0.
        for name in ["erff", "fmodf", "log2f", "atan2"] {
            assert!(exports().iter().any(|(n, _)| *n == name), "{name} must be wired");
        }
        type Erff = unsafe extern "win64" fn(f32) -> f32;
        type Fmodf = unsafe extern "win64" fn(f32, f32) -> f32;
        type Log2f = unsafe extern "win64" fn(f32) -> f32;
        type Atan2 = unsafe extern "win64" fn(f64, f64) -> f64;
        let addr = |name: &str| {
            exports()
                .into_iter()
                .find_map(|(n, a)| (n == name).then_some(a))
                .expect("export")
        };
        let erf: Erff = unsafe { std::mem::transmute(addr("erff")) };
        let fmod: Fmodf = unsafe { std::mem::transmute(addr("fmodf")) };
        let log2: Log2f = unsafe { std::mem::transmute(addr("log2f")) };
        let atan2: Atan2 = unsafe { std::mem::transmute(addr("atan2")) };
        unsafe {
            // Reference table is `math.erf(x as f64)` for the exact f32
            // inputs (small ones included, so a `1 - erfc` shortcut fails).
            for &(x, reference) in &[
                (-2.5f32, -0.999593047982555),
                (-0.5, -0.5204998778130465),
                (1e-6, 1.1283791642462529e-06),
                (1e-3, 0.0011283788445643173),
                (0.1, 0.11246291768297051),
                (0.5, 0.5204998778130465),
                (1.0, 0.8427007929497149),
                (1.5, 0.9661051464753108),
                (2.0, 0.9953222650189527),
                (2.5, 0.999593047982555),
                (3.0, 0.9999779095030014),
                (4.0, 0.9999999845827421),
            ] {
                let got = erf(x) as f64;
                let relative = ((got - reference) / reference).abs();
                assert!(relative < 1e-6, "erff({x}) = {got}, reference {reference}");
            }
            assert_eq!(erf(0.0).to_bits(), 0.0f32.to_bits());
            assert!(erf(-0.0).is_sign_negative() && erf(-0.0) == 0.0);
            assert_eq!(erf(f32::INFINITY), 1.0);
            assert_eq!(erf(f32::NEG_INFINITY), -1.0);
            assert!(erf(f32::NAN).is_nan());
            let mut previous = f32::NEG_INFINITY;
            for step in 0..=1200 {
                let x = -6.0 + step as f32 * 0.01;
                let y = erf(x);
                assert!(y >= previous, "erff must be monotone at {x}");
                previous = y;
            }

            // fmod keeps the dividend's sign; a finite dividend survives an
            // infinite divisor; zero divisor and infinite dividend are NaN.
            assert_eq!(fmod(5.5, 2.0), 1.5);
            assert_eq!(fmod(-5.5, 2.0), -1.5);
            assert_eq!(fmod(5.5, -2.0), 1.5);
            assert_eq!(fmod(-5.5, -2.0), -1.5);
            assert_eq!(fmod(100.0, 100.0).to_bits(), 0.0f32.to_bits());
            assert!(fmod(-0.0, 3.0).is_sign_negative() && fmod(-0.0, 3.0) == 0.0);
            assert_eq!(fmod(5.5, f32::INFINITY), 5.5);
            assert_eq!(fmod(-5.5, f32::NEG_INFINITY), -5.5);
            assert!(fmod(f32::INFINITY, 2.0).is_nan());
            assert!(fmod(2.0, 0.0).is_nan());
            assert!(fmod(f32::NAN, 2.0).is_nan());

            assert_eq!(log2(8.0), 3.0);
            assert_eq!(log2(0.5), -1.0);
            assert_eq!(log2(1.0), 0.0);
            assert!((log2(3.0) - 1.584_962_5).abs() < 1e-6);
            assert_eq!(log2(0.0), f32::NEG_INFINITY);
            assert_eq!(log2(-0.0), f32::NEG_INFINITY);
            assert!(log2(-1.0).is_nan());
            assert_eq!(log2(f32::INFINITY), f32::INFINITY);
            assert!(log2(f32::NAN).is_nan());
            // Smallest positive denormal: exponent -149, not -inf/NaN.
            assert_eq!(log2(f32::from_bits(1)), -149.0);

            // Double atan2 (the f32 callers promote at exe+0x623b20).
            use std::f64::consts::{FRAC_PI_4, PI};
            assert!((atan2(1.0, 1.0) - FRAC_PI_4).abs() < 1e-12);
            assert!((atan2(-1.0, 1.0) + FRAC_PI_4).abs() < 1e-12);
            assert!((atan2(1.0, -1.0) - 3.0 * FRAC_PI_4).abs() < 1e-12);
            assert!((atan2(-1.0, -1.0) + 3.0 * FRAC_PI_4).abs() < 1e-12);
            assert_eq!(atan2(0.0, 0.0), 0.0);
            assert_eq!(atan2(0.0, -1.0), PI);
            assert_eq!(atan2(-0.0, -1.0), -PI);
            assert_eq!(atan2(1.0, f64::INFINITY), 0.0);
            assert!(atan2(f64::NAN, 1.0).is_nan());
        }
    }

    #[test]
    fn classify_parse_sort_and_rng_contracts_are_live() {
        // Retail Step 38: all six resolved to generic auto-stubs (0/0/"equal"/no-op),
        // which the language/config init consumed as empty results. Wiring is only
        // real when the export table carries the implementations.
        for name in ["isspace", "atoi", "_stricmp", "srand", "rand", "qsort"] {
            assert!(exports().iter().any(|(n, _)| *n == name), "{name} must be wired");
        }
        unsafe {
            for c in [0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x20] {
                assert_ne!(isspace(c), 0, "isspace({c:#x})");
            }
            for c in [0, b'A' as i32, b'0' as i32, 0x7F, 0xA0, -1] {
                assert_eq!(isspace(c), 0, "isspace({c:#x})");
            }

            assert_eq!(atoi(c"  -42xyz".as_ptr() as *const u8), -42);
            assert_eq!(atoi(c"+7".as_ptr() as *const u8), 7);
            assert_eq!(atoi(c"abc".as_ptr() as *const u8), 0);
            assert_eq!(atoi(c"2147483648".as_ptr() as *const u8), i32::MAX);
            assert_eq!(atoi(std::ptr::null()), 0);

            assert_eq!(_stricmp(c"AbC".as_ptr() as *const u8, c"aBc".as_ptr() as *const u8), 0);
            assert!(_stricmp(c"abc".as_ptr() as *const u8, c"abd".as_ptr() as *const u8) < 0);
            assert!(_stricmp(c"B".as_ptr() as *const u8, c"a".as_ptr() as *const u8) > 0);

            // `srand` seeds the UCRT sequence: reseeding replays it exactly, and
            // seed 1's first value is the MSVC generator's 41 (not a stub constant).
            srand(1);
            let first = rand();
            srand(1);
            assert_eq!(rand(), first);
            assert_eq!(first, 41);
            for _ in 0..100 {
                assert!((0..=0x7FFF).contains(&rand()));
            }

            // `qsort` orders whole elements: a 16-byte element keeps its payload, so a
            // key-only swap would fail the pad assertion.
            #[repr(C)]
            #[derive(Clone, Copy)]
            struct E {
                key: i32,
                pad: [u8; 12],
            }
            unsafe extern "win64" fn by_key(a: *const u8, b: *const u8) -> i32 {
                let x = &*(a as *const E);
                let y = &*(b as *const E);
                match x.key.cmp(&y.key) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                }
            }
            let mut arr = [
                E { key: 5, pad: [5; 12] },
                E { key: 1, pad: [1; 12] },
                E { key: 4, pad: [4; 12] },
                E { key: 2, pad: [2; 12] },
                E { key: 3, pad: [3; 12] },
            ];
            qsort(arr.as_mut_ptr() as *mut u8, arr.len(), std::mem::size_of::<E>(), by_key as usize);
            assert_eq!(arr.iter().map(|e| e.key).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
            for e in arr {
                assert_eq!(e.pad, [e.key as u8; 12], "payload moved with its key");
            }
            // Degenerate inputs are no-ops, never faults.
            qsort(arr.as_mut_ptr() as *mut u8, 0, 16, by_key as usize);
            qsort(std::ptr::null_mut(), 5, 16, by_key as usize);
            qsort(arr.as_mut_ptr() as *mut u8, 5, 0, by_key as usize);
            qsort(arr.as_mut_ptr() as *mut u8, usize::MAX, 3, by_key as usize);
        }
    }

    #[test]
    fn vsscanf_assigns_fields_through_the_va_list() {
        assert!(exports().iter().any(|(n, _)| *n == "__stdio_common_vsscanf"));
        unsafe fn scan(input: &[u8], format: &[u8], args: &[u64]) -> i32 {
            unsafe {
                __stdio_common_vsscanf(0, input.as_ptr(), usize::MAX, format.as_ptr(), 0, args.as_ptr())
            }
        }
        unsafe {
            // Integer, string and hex fields; the format's spaces cross input whitespace.
            let mut i = 0i32;
            let mut buf = [0u8; 16];
            let mut x = 0u32;
            let args = [&mut i as *mut i32 as u64, buf.as_mut_ptr() as u64, &mut x as *mut u32 as u64];
            let n = scan(c"  -7 hello 1F".to_bytes(), b"%d %s %x\0", &args);
            assert_eq!(n, 3);
            assert_eq!(i, -7);
            assert_eq!(&buf[..6], b"hello\0");
            assert_eq!(x, 0x1F);

            // Floating point: `%lf` is a double, bare `%f` a float; `%n` records progress.
            let mut d = 0f64;
            let mut f = 0f32;
            let mut used = -1i32;
            let args = [&mut d as *mut f64 as u64, &mut f as *mut f32 as u64, &mut used as *mut i32 as u64];
            assert_eq!(scan(c"1.5,-2.25".to_bytes(), b"%lf,%f%n\0", &args), 2);
            assert_eq!(d, 1.5);
            assert_eq!(f, -2.25);
            assert_eq!(used, 9);

            // Scanset, width, assignment suppression and a partial matching failure.
            let mut word = [0u8; 8];
            let mut v = 0i32;
            let args = [word.as_mut_ptr() as u64, &mut v as *mut i32 as u64];
            assert_eq!(scan(c"abcdef,42".to_bytes(), b"%[^,],%d\0", &args), 2);
            assert_eq!(&word[..7], b"abcdef\0");
            assert_eq!(v, 42);
            let mut cut = [0u8; 8];
            let args = [cut.as_mut_ptr() as u64];
            assert_eq!(scan(c"abcdef".to_bytes(), b"%3s\0", &args), 1);
            assert_eq!(&cut[..4], b"abc\0");
            let mut only = 0i32;
            let args = [&mut only as *mut i32 as u64];
            assert_eq!(scan(c"7 9".to_bytes(), b"%*d %d\0", &args), 1);
            assert_eq!(only, 9);
            let mut a = 0i32;
            let mut b = 0i32;
            let args = [&mut a as *mut i32 as u64, &mut b as *mut i32 as u64];
            assert_eq!(scan(c"12x".to_bytes(), b"%d%d\0", &args), 1);
            assert_eq!(a, 12);
            // Input failure before the first field is EOF, like UCRT.
            assert_eq!(scan(c"".to_bytes(), b"%d\0", &args), -1);
            // Wide `%ls` writes UTF-16 units.
            let mut w = [0u16; 4];
            let args = [w.as_mut_ptr() as u64];
            assert_eq!(scan(c"ok".to_bytes(), b"%ls\0", &args), 1);
            assert_eq!(&w[..3], &[b'o' as u16, b'k' as u16, 0]);
            // `count` bounds the readable input (`_snscanf`): the trailing "x" is invisible.
            let mut c = -1i32;
            let args = [&mut c as *mut i32 as u64];
            assert_eq!(__stdio_common_vsscanf(0, c" 42x".as_ptr() as *const u8, 3, b"%d\0".as_ptr(), 0, args.as_ptr()), 1);
            assert_eq!(c, 42);

            // `%p` is a pointer conversion: UCRT stores pointer-width even
            // without an `l` modifier. The title round-trips a static address
            // as "0x14688e110" and re-parses it with `%p`; a 32-bit store
            // truncates it to an unmapped pointer.
            let mut p = 0xDEAD_BEEF_0000_0000u64;
            let args = [&mut p as *mut u64 as u64];
            assert_eq!(scan(c"0x14688e110".to_bytes(), b"%p\0", &args), 1);
            assert_eq!(p, 0x1_4688_e110);
        }
    }

    #[test]
    fn strtod_and_strtoui64_write_their_end_pointers() {
        // Retail Step 52: both names resolved to the generic auto-stub, which
        // returned 0 without writing `*endptr`; the consumer at exe+0x5270e4d
        // then dereferenced null (`cmpb $0x23,(%rcx)` at exe+0x5270e50).
        // Wiring is only real when the export table carries the implementation.
        for name in ["strtod", "_strtoui64"] {
            assert!(exports().iter().any(|(n, _)| *n == name), "{name} must be wired");
        }
        type Strtod = unsafe extern "win64" fn(*const u8, *mut *mut u8) -> f64;
        type Strtou64 = unsafe extern "win64" fn(*const u8, *mut *mut u8, i32) -> u64;
        let addr = |name: &str| {
            exports()
                .into_iter()
                .find_map(|(n, a)| (n == name).then_some(a))
                .expect("export")
        };
        let parse_f64: Strtod = unsafe { std::mem::transmute(addr("strtod")) };
        let parse_u64: Strtou64 = unsafe { std::mem::transmute(addr("_strtoui64")) };
        unsafe {
            let mut end: *mut u8 = std::ptr::null_mut();

            let text = c"  3.5e2xyz";
            assert_eq!(parse_f64(text.as_ptr() as *const u8, &mut end), 350.0);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(7));
            let text = c"0x1.8p3!";
            assert_eq!(parse_f64(text.as_ptr() as *const u8, &mut end), 12.0);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(7));
            let text = c"-inf";
            assert_eq!(parse_f64(text.as_ptr() as *const u8, &mut end), f64::NEG_INFINITY);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(4));
            let text = c"nan(0x7)";
            assert!(parse_f64(text.as_ptr() as *const u8, &mut end).is_nan());
            assert_eq!(end, (text.as_ptr() as *mut u8).add(8));
            // No conversion: `endptr` stays at the caller's pointer, not after `+`.
            let text = c"+#ff";
            assert_eq!(parse_f64(text.as_ptr() as *const u8, &mut end), 0.0);
            assert_eq!(end, text.as_ptr() as *mut u8);
            // An incomplete hex prefix is only the decimal `0`.
            let text = c"0xzz";
            assert_eq!(parse_f64(text.as_ptr() as *const u8, &mut end), 0.0);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(1));
            // Overflow returns infinity with ERANGE and still consumes the token.
            *_errno() = 0;
            let text = c"1e999";
            assert_eq!(parse_f64(text.as_ptr() as *const u8, &mut end), f64::INFINITY);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(5));
            assert_eq!(*_errno(), 34);
            assert_eq!(parse_f64(c"2.5".as_ptr() as *const u8, std::ptr::null_mut()), 2.5);

            let text = c"0x1f";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 0), 31);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(4));
            let text = c"0777";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 0), 511);
            let text = c"101";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 2), 5);
            let text = c"z";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 36), 35);
            let text = c"-1";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), u64::MAX);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(2));
            *_errno() = 0;
            let text = c"18446744073709551615";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), u64::MAX);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(20));
            assert_eq!(*_errno(), 0);
            *_errno() = 0;
            let text = c"18446744073709551616";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), u64::MAX);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(20));
            assert_eq!(*_errno(), 34);
            let text = c"  +42rest";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), 42);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(5));
            let text = c"xyz";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), 0);
            assert_eq!(end, text.as_ptr() as *mut u8);
            // `0x` with no hex digit in base 16 is the digit `0`, then stops at `x`.
            let text = c"0x";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 16), 0);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(1));
            *_errno() = 0;
            let text = c"7";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 37), 0);
            assert_eq!(end, text.as_ptr() as *mut u8);
            assert_eq!(*_errno(), 22);
        }
    }

    #[test]
    fn strtoll_strtoull_and_errno_write_their_outputs() {
        // Retail Step 65: caller exe+0x1418e1060 does `_set_errno(0)`, then
        // `strtoll(str, &endptr, 10)` (IAT 0x146794768) or, for a `0x` prefix,
        // `strtoull(str + 2, &endptr, 16)` (0x146794778), then dereferences
        // `endptr` at exe+0x18e10c7. The auto-stub returned 0 without writing
        // `*endptr`, so the title read through a NULL pointer.
        for name in ["strtoll", "strtoull", "_set_errno", "_get_errno"] {
            assert!(exports().iter().any(|(n, _)| *n == name), "{name} must be wired");
        }
        type Strtoll = unsafe extern "win64" fn(*const u8, *mut *mut u8, i32) -> i64;
        type Strtoull = unsafe extern "win64" fn(*const u8, *mut *mut u8, i32) -> u64;
        type SetErrno = unsafe extern "win64" fn(i32) -> i32;
        type GetErrno = unsafe extern "win64" fn(*mut i32) -> i32;
        let addr = |name: &str| {
            exports()
                .into_iter()
                .find_map(|(n, a)| (n == name).then_some(a))
                .expect("export")
        };
        let parse_i64: Strtoll = unsafe { std::mem::transmute(addr("strtoll")) };
        let parse_u64: Strtoull = unsafe { std::mem::transmute(addr("strtoull")) };
        let set_errno: SetErrno = unsafe { std::mem::transmute(addr("_set_errno")) };
        let get_errno: GetErrno = unsafe { std::mem::transmute(addr("_get_errno")) };
        unsafe {
            let mut end: *mut u8 = std::ptr::null_mut();

            // The caller's decimal path: leading space, sign, then junk.
            let text = c" -42rest";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 10), -42);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(4));
            // The `0x` path passes base 16 with the prefix already skipped.
            let text = c"1f";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 16), 31);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(2));
            let text = c"0x1f";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 16), 31);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(4));
            // Both signed limits are valid and do not set errno.
            set_errno(0);
            let text = c"9223372036854775807";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 10), i64::MAX);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(19));
            assert_eq!(*_errno(), 0);
            let text = c"-9223372036854775808";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 10), i64::MIN);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(20));
            assert_eq!(*_errno(), 0);
            // One past either limit saturates and reports ERANGE, consuming all
            // of the digits so `endptr` still lands on the trailing junk.
            set_errno(0);
            let text = c"9223372036854775808x";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 10), i64::MAX);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(19));
            assert_eq!(*_errno(), 34);
            set_errno(0);
            let text = c"-9223372036854775809x";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 10), i64::MIN);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(20));
            assert_eq!(*_errno(), 34);
            // No conversion leaves `endptr` at the caller's pointer, and a bad
            // base reports EINVAL the same way `_strtoui64` does.
            let text = c"+#ff";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 10), 0);
            assert_eq!(end, text.as_ptr() as *mut u8);
            set_errno(0);
            let text = c"7";
            assert_eq!(parse_i64(text.as_ptr() as *const u8, &mut end, 37), 0);
            assert_eq!(end, text.as_ptr() as *mut u8);
            assert_eq!(*_errno(), 22);
            // Signed parse of a negative is not an overflow of `i64`.
            assert_eq!(parse_i64(c"-1".as_ptr() as *const u8, &mut end, 10), -1);
            assert_eq!(parse_i64(c"10".as_ptr() as *const u8, std::ptr::null_mut(), 10), 10);

            // `strtoull` is `_strtoui64` under its own name.
            let text = c"0x1f";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 0), 31);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(4));
            let text = c"-1";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), u64::MAX);
            set_errno(0);
            let text = c"18446744073709551616x";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), u64::MAX);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(20));
            assert_eq!(*_errno(), 34);
            set_errno(0);
            let text = c"xyz";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 10), 0);
            assert_eq!(end, text.as_ptr() as *mut u8);
            assert_eq!(*_errno(), 0);
            // `0x` with no hex digit in base 16 is the digit `0`, then stops.
            let text = c"0x";
            assert_eq!(parse_u64(text.as_ptr() as *const u8, &mut end, 16), 0);
            assert_eq!(end, (text.as_ptr() as *mut u8).add(1));
            assert_eq!(parse_u64(c"42".as_ptr() as *const u8, std::ptr::null_mut(), 10), 42);

            // errno accessors: the caller clears with `_set_errno(0)` and reads
            // back with `_get_errno(&value)` at exe+0x1418e1118.
            let mut value = -1;
            assert_eq!(set_errno(34), 0);
            assert_eq!(get_errno(&mut value), 0);
            assert_eq!(value, 34);
            assert_eq!(set_errno(0), 0);
            assert_eq!(get_errno(&mut value), 0);
            assert_eq!(value, 0);
            assert_eq!(get_errno(std::ptr::null_mut()), 22);
        }
    }

    #[test]
    fn isalnum_matches_the_c_locale() {
        // Retail Step 53: the title's identifier scanner at exe+0x2749412 loops
        // on the `api-ms-win-crt-string` isalnum import (exe+0x2749423); the
        // auto-stub returned 0 for every byte, so no token ever matched.
        assert!(
            exports().iter().any(|(n, _)| *n == "isalnum"),
            "isalnum must be wired"
        );
        let addr = exports()
            .into_iter()
            .find_map(|(n, a)| (n == "isalnum").then_some(a))
            .expect("export");
        type IsAlnum = unsafe extern "win64" fn(i32) -> i32;
        let f: IsAlnum = unsafe { std::mem::transmute(addr) };
        unsafe {
            for c in b'0'..=b'9' {
                assert_ne!(f(c as i32), 0, "{c:?} is alphanumeric");
            }
            for c in b'A'..=b'Z' {
                assert_ne!(f(c as i32), 0, "{c:?} is alphanumeric");
            }
            for c in b'a'..=b'z' {
                assert_ne!(f(c as i32), 0, "{c:?} is alphanumeric");
            }
            for c in [0i32, 0x20, b'_' as i32, b'#' as i32, b'/' as i32, b':' as i32, 0x7f, 0x80, 0x100, -1] {
                assert_eq!(f(c), 0, "{c:#x} is not alphanumeric");
            }
        }
    }

    #[test]
    fn c_locale_ctype_table_and_setlocale_match_ucrt() {
        unsafe {
            let table = __pctype_func();
            assert!(!table.is_null(), "__pctype_func must return a table");
            let t = std::slice::from_raw_parts(table, 256);
            for c in b'A'..=b'F' {
                assert_ne!(t[c as usize] & CTYPE_UPPER, 0, "{c:?} upper");
                assert_ne!(t[c as usize] & CTYPE_HEX, 0, "{c:?} hex");
                assert_ne!(t[c as usize] & CTYPE_ALPHA, 0, "{c:?} alpha");
            }
            for c in b'a'..=b'f' {
                assert_ne!(t[c as usize] & CTYPE_LOWER, 0, "{c:?} lower");
                assert_ne!(t[c as usize] & CTYPE_HEX, 0, "{c:?} hex");
            }
            assert_eq!(t[b'G' as usize] & CTYPE_HEX, 0, "G is a letter, not hex");
            assert_eq!(t[b'G' as usize] & CTYPE_ALPHA, CTYPE_ALPHA);
            for c in b'0'..=b'9' {
                assert_ne!(t[c as usize] & CTYPE_DIGIT, 0, "{c:?} digit");
                assert_ne!(t[c as usize] & CTYPE_HEX, 0, "{c:?} hex");
            }
            assert_eq!(t[b' ' as usize] & CTYPE_SPACE, CTYPE_SPACE);
            assert_eq!(t[0x09] & CTYPE_SPACE, CTYPE_SPACE);
            assert_eq!(t[0x00] & CTYPE_CONTROL, CTYPE_CONTROL);
            assert_eq!(t[0x7F] & CTYPE_CONTROL, CTYPE_CONTROL);
            assert_eq!(t[0x80], 0, "the C locale does not classify high bytes");

            assert_eq!(___lc_codepage_func(), 0, "XCL's ANSI page is UTF-8 (CP_ACP)");
            assert_eq!(___mb_cur_max_func(), 1, "the C locale is single-byte");
            _lock_locales();
            _unlock_locales();
            let names = ___lc_locale_name_func();
            assert!(!names.is_null());
            for i in 0..6 {
                let name = *names.add(i);
                assert!(!name.is_null(), "category {i}");
                assert_eq!(std::ffi::CStr::from_ptr(name as *const _).to_bytes(), b"C");
            }

            let c = setlocale(LC_ALL, std::ptr::null());
            assert!(!c.is_null());
            assert_eq!(std::ffi::CStr::from_ptr(c as *const _).to_bytes(), b"C");
            for name in [b"C\0".as_slice(), b"\0".as_slice(), b"POSIX\0".as_slice()] {
                assert!(!setlocale(LC_ALL, name.as_ptr()).is_null(), "{name:?}");
            }
            assert!(setlocale(LC_ALL, b"fr_FR\0".as_ptr()).is_null());
            assert!(setlocale(-1, std::ptr::null()).is_null());
            assert!(setlocale(6, std::ptr::null()).is_null());
        }
    }
}
