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
        ("strstr", strstr as usize),
        ("strrchr", strrchr as usize),
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
        ("__stdio_common_vswprintf_s", __stdio_common_vswprintf_s as usize),
        ("__stdio_common_vswprintf", __stdio_common_vswprintf as usize),
        ("__stdio_common_vsprintf_s", __stdio_common_vsprintf_s as usize),
        ("__stdio_common_vsprintf", __stdio_common_vsprintf as usize),
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
}
