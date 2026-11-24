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
        ("strcpy", strcpy as usize),
        ("strncpy", strncpy as usize),
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
    fn string_compare_and_search_match_libc() {        unsafe {
            assert_eq!(strcmp(b"abc\0".as_ptr(), b"abc\0".as_ptr()), 0);
            assert!(strcmp(b"abc\0".as_ptr(), b"abd\0".as_ptr()) < 0);
            assert!(strcmp(b"abd\0".as_ptr(), b"abc\0".as_ptr()) > 0);
            assert_eq!(strncmp(b"abcdef\0".as_ptr(), b"abcxyz\0".as_ptr(), 3), 0);
            assert!(strncmp(b"abcdef\0".as_ptr(), b"abcxyz\0".as_ptr(), 4) < 0);
            assert_eq!(strncmp(b"abc\0".as_ptr(), b"abc\0".as_ptr(), 0), 0);
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
}
