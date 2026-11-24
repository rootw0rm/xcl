//! Linux kernelx core (Phase C steps 10b–10c): memory, threads, sync, time, and files.
//!
//! The Windows `hle/kernelx.rs` stays the implementation source of truth for the full
//! title path. This module is the Linux twin for the step-10b slice (virtual memory via
//! `mmap`/`mprotect`, game threads via [`crate::tls::spawn`], synchronization primitives
//! with inline Windows layout semantics, and monotonic time) plus the step-10c slice
//! (title-facing file operations through the linked-in Linux VFS path in
//! `vfs_hooks_linux`, with libc-backed kernel-family fallbacks). Xbox thread priorities
//! degrade gracefully: the requested value is remembered and returned, while the host
//! scheduler change is best-effort (no abort without `CAP_SYS_NICE`).
//!
//! Calling convention: the game is a Windows PE, so every export is
//! `unsafe extern "win64"`, matching the stub thunks and the TLS callbacks.
//!
//! Unrelated exports (module loading, physical pages, XAudio/GPU paths) stay
//! `Stub` in `hle_linux` until steps 10d and Phase D/E land.
//!
//! Handle model: events, threads, files, and finds are monotonic ids in process-wide
//! tables. `GetCurrentProcess` returns `-1` and `GetCurrentThread` returns `-2`, as on
//! Windows. File handles hold `std::fs::File` opened through the VFS, so reads see the
//! read-only game layer and writes land in the copy-on-write overlay; `S:`/`T:` use the
//! title data dir.

#![allow(non_snake_case)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::{from_ansi_ptr, from_wide_ptr};

// ---------------------------------------------------------------------------------------
// Last error (per-thread, as on Windows)
// ---------------------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn set_error(e: u32) {
    LAST_ERROR.with(|c| c.set(e));
}

fn get_error() -> u32 {
    LAST_ERROR.with(|c| c.get())
}

pub unsafe extern "win64" fn GetLastError() -> u32 {
    get_error()
}

pub unsafe extern "win64" fn SetLastError(e: u32) {
    set_error(e);
}

// Windows error codes used here.
const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INVALID_ADDRESS: u32 = 487;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_MOD_NOT_FOUND: u32 = 126;

// Wait results.
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;
pub const WAIT_FAILED: u32 = 0xFFFF_FFFF;
pub const INFINITE: u32 = 0xFFFF_FFFF;

// Memory types.
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;

// Windows page protections.
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READONLY: u32 = 0x02;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE: u32 = 0x10;
const PAGE_EXECUTE_READ: u32 = 0x20;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const PAGE_GUARD: u32 = 0x100;

fn page_size() -> usize {
    static SIZE: OnceLock<usize> = OnceLock::new();
    *SIZE.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n > 0 { n as usize } else { 4096 }
    })
}

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// Console allocation flags that survive on Linux (mirrors the Windows sanitizer).
pub fn sanitize_alloc_type(t: u32) -> u32 {
    const MASK: u32 = 0x1000 | 0x2000 | 0x8_0000 | 0x10_0000 | 0x20_0000 | 0x100_0000;
    let s = t & MASK;
    if s == 0 { MEM_COMMIT | MEM_RESERVE } else { s }
}

pub fn sanitize_protect(p: u32) -> u32 {
    const MASK: u32 = 0xFF | 0x100 | 0x200 | 0x400;
    let s = p & MASK;
    if s & 0xFF == 0 {
        (s & !0xFF) | PAGE_READWRITE
    } else {
        s
    }
}

fn win_to_mmap_prot(p: u32) -> libc::c_int {
    let base = match p & 0xFF {
        PAGE_NOACCESS => libc::PROT_NONE,
        PAGE_READONLY => libc::PROT_READ,
        PAGE_READWRITE | PAGE_WRITECOPY => libc::PROT_READ | libc::PROT_WRITE,
        PAGE_EXECUTE => libc::PROT_EXEC,
        PAGE_EXECUTE_READ => libc::PROT_READ | libc::PROT_EXEC,
        PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY => {
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC
        }
        _ => libc::PROT_READ | libc::PROT_WRITE,
    };
    base
}

/// base → (size, windows protect). Tracks reservations for `MEM_RELEASE size==0`.
static ALLOCATIONS: OnceLock<Mutex<BTreeMap<usize, (usize, u32)>>> = OnceLock::new();

fn allocations() -> &'static Mutex<BTreeMap<usize, (usize, u32)>> {
    ALLOCATIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn mmap_prot_for_new(committed: bool, protect: u32) -> libc::c_int {
    if committed {
        win_to_mmap_prot(protect)
    } else {
        libc::PROT_NONE
    }
}

pub unsafe extern "win64" fn VirtualAlloc(
    addr: *const c_void,
    size: usize,
    atype: u32,
    protect: u32,
) -> *mut c_void {
    // A flagless type is a caller bug (Windows fails it); the sanitizer only fills in
    // console-specific combinations, it never rescues zero.
    if atype & (MEM_RESERVE | MEM_COMMIT) == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let t = sanitize_alloc_type(atype);
    let p = sanitize_protect(protect);
    if size == 0 || t & (MEM_RESERVE | MEM_COMMIT) == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let page = page_size();
    let len = align_up(size, page);
    let committed = t & MEM_COMMIT != 0;
    let prot = mmap_prot_for_new(committed, p);
    let base_addr = addr as usize;
    let ptr = if base_addr == 0 {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    } else {
        if base_addr % page != 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        const MAP_FIXED_NOREPLACE: libc::c_int = 0x10_0000;
        libc::mmap(
            base_addr as *mut c_void,
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        set_error(if e == libc::EEXIST {
            ERROR_INVALID_ADDRESS
        } else {
            ERROR_NOT_ENOUGH_MEMORY
        });
        return std::ptr::null_mut();
    }
    allocations()
        .lock()
        .unwrap()
        .insert(ptr as usize, (len, p));
    set_error(ERROR_SUCCESS);
    ptr as *mut c_void
}

pub unsafe extern "win64" fn VirtualFree(addr: *mut c_void, size: usize, ftype: u32) -> i32 {
    if addr.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let base = addr as usize;
    if ftype & MEM_RELEASE != 0 {
        if size != 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let tracked = allocations().lock().unwrap().remove(&base);
        let Some((len, _)) = tracked else {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        if libc::munmap(base as *mut c_void, len) != 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if ftype & MEM_DECOMMIT != 0 {
        let page = page_size();
        if base % page != 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let len = align_up(size, page);
        if len == 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        // The reservation stays; its contents read as zeros afterwards.
        libc::madvise(base as *mut c_void, len, libc::MADV_DONTNEED);
        set_error(ERROR_SUCCESS);
        return 1;
    }
    set_error(ERROR_INVALID_PARAMETER);
    0
}

pub unsafe extern "win64" fn VirtualProtect(
    addr: *const c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> i32 {
    if addr.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let p = sanitize_protect(protect);
    let page = page_size();
    let base = (addr as usize) & !(page - 1);
    let end = align_up(addr as usize + size, page);
    if end <= base {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if libc::mprotect(
        base as *mut c_void,
        end - base,
        win_to_mmap_prot(p),
    ) != 0
    {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Report the reservation's previous protection when known.
    let prev = allocations()
        .lock()
        .unwrap()
        .get(&base)
        .map(|&(_, prev)| prev)
        .unwrap_or(PAGE_READWRITE);
    if !old.is_null() {
        *old = prev;
    }
    if let Some(entry) = allocations().lock().unwrap().get_mut(&base) {
        entry.1 = p;
    }
    set_error(ERROR_SUCCESS);
    1
}

fn current_process_handle() -> usize {
    usize::MAX // -1, as on Windows
}

fn current_thread_handle() -> usize {
    usize::MAX - 1 // -2, as on Windows
}

pub unsafe extern "win64" fn VirtualAllocEx(
    proc_: usize,
    addr: *const c_void,
    size: usize,
    atype: u32,
    protect: u32,
) -> *mut c_void {
    if proc_ != current_process_handle() {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    VirtualAlloc(addr, size, atype, protect)
}

pub unsafe extern "win64" fn VirtualFreeEx(
    proc_: usize,
    addr: *mut c_void,
    size: usize,
    ftype: u32,
) -> i32 {
    if proc_ != current_process_handle() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    VirtualFree(addr, size, ftype)
}

pub unsafe extern "win64" fn VirtualProtectEx(
    proc_: usize,
    addr: *const c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> i32 {
    if proc_ != current_process_handle() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    VirtualProtect(addr, size, protect, old)
}

// ---------------------------------------------------------------------------------------
// Threads: creation through tls::spawn, ids, priorities (graceful), affinity, names
// ---------------------------------------------------------------------------------------

const CREATE_SUSPENDED: u32 = 0x4;

type ThreadStart = unsafe extern "win64" fn(*mut c_void) -> u32;

struct ThreadRecord {
    tid: AtomicU32,
    priority: AtomicU32,
    name: Mutex<String>,
    suspended_gate: Option<std::sync::Arc<(Mutex<bool>, Condvar)>>,
}

static THREADS: OnceLock<Mutex<HashMap<usize, std::sync::Arc<ThreadRecord>>>> = OnceLock::new();

fn threads() -> &'static Mutex<HashMap<usize, std::sync::Arc<ThreadRecord>>> {
    THREADS.get_or_init(|| Mutex::new(HashMap::new()))
}

static THREAD_JOINS: OnceLock<Mutex<HashMap<usize, std::thread::JoinHandle<u32>>>> =
    OnceLock::new();

fn joins() -> &'static Mutex<HashMap<usize, std::thread::JoinHandle<u32>>> {
    THREAD_JOINS.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x1_0000);

fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

fn resolve_tid(handle: usize) -> Option<u32> {
    if handle == current_thread_handle() || handle == 0 {
        return Some(current_tid());
    }
    let tid = threads()
        .lock()
        .unwrap()
        .get(&handle)
        .map(|r| r.tid.load(Ordering::Relaxed))?;
    if tid == 0 {
        // The thread has not stored its kernel tid yet; the caller operates on the
        // record (priority/affinity are best-effort) rather than failing.
        None
    } else {
        Some(tid)
    }
}

pub unsafe extern "win64" fn GetCurrentProcess() -> usize {
    current_process_handle()
}

pub unsafe extern "win64" fn GetCurrentThread() -> usize {
    current_thread_handle()
}

pub unsafe extern "win64" fn GetCurrentProcessId() -> u32 {
    libc::getpid() as u32
}

pub unsafe extern "win64" fn GetCurrentThreadId() -> u32 {
    current_tid()
}

pub unsafe extern "win64" fn GetThreadId(handle: usize) -> u32 {
    match resolve_tid(handle) {
        Some(t) => t,
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub unsafe extern "win64" fn CreateThread(
    _attrs: *const c_void,
    stack: usize,
    start: usize,
    param: *mut c_void,
    flags: u32,
    out_id: *mut u32,
) -> usize {
    if start == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let suspended = flags & CREATE_SUSPENDED != 0;
    let gate = suspended.then(|| std::sync::Arc::new((Mutex::new(false), Condvar::new())));
    let gate_in_thread = gate.clone();
    let param_usize = param as usize;
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let record = std::sync::Arc::new(ThreadRecord {
        tid: AtomicU32::new(0),
        priority: AtomicU32::new(0),
        name: Mutex::new(format!("xcl-game-{handle:x}")),
        suspended_gate: gate,
    });
    let record_for_thread = record.clone();
    threads().lock().unwrap().insert(handle, record);
    let thread_name = format!("xcl-game-{handle:x}");
    let mut builder = std::thread::Builder::new().name(thread_name);
    if stack != 0 {
        builder = builder.stack_size(stack);
    }
    let spawned = builder.spawn(move || {
        // Record the kernel tid for affinity targeting.
        record_for_thread
            .tid
            .store(current_tid(), Ordering::Relaxed);
        // Suspended start: wait for ResumeThread before running game code.
        if let Some(g) = gate_in_thread.as_ref() {
            let (lock, cvar) = &**g;
            let mut go = lock.lock().unwrap();
            while !*go {
                go = cvar.wait(go).unwrap();
            }
        }
        // Every game thread enters through the TLS wrapper so attach/callbacks run.
        // `tls::spawn` cannot be used here because the OS thread already exists;
        // attach explicitly instead (same callbacks, same ordering).
        if let Err(e) = crate::tls::attach_current_thread() {
            crate::xlog_warn!("CreateThread: TLS attach failed: {e}");
            return 1;
        }
        let f: ThreadStart = std::mem::transmute(start);
        let code = f(param_usize as *mut c_void);
        crate::tls::detach_current_thread();
        code
    });
    let join = match spawned {
        Ok(j) => j,
        Err(_) => {
            threads().lock().unwrap().remove(&handle);
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            return 0;
        }
    };
    joins().lock().unwrap().insert(handle, join);
    if !out_id.is_null() {
        *out_id = handle as u32;
    }
    // If the caller did not ask for a suspended start, nothing more to do: the thread
    // is already running.
    set_error(ERROR_SUCCESS);
    handle
}

pub unsafe extern "win64" fn ResumeThread(handle: usize) -> u32 {
    let g = threads().lock().unwrap().get(&handle).cloned();
    let Some(rec) = g else {
        set_error(ERROR_INVALID_PARAMETER);
        return u32::MAX;
    };
    if let Some(gate) = rec.suspended_gate.as_ref() {
        let (lock, cvar) = &**gate;
        let mut go = lock.lock().unwrap();
        let was = !*go;
        *go = true;
        cvar.notify_all();
        return if was { 1 } else { 0 };
    }
    0
}

pub unsafe extern "win64" fn SuspendThread(_handle: usize) -> u32 {
    // True suspension (SIGSTOP of one thread) is deliberately not implemented: game
    // threads run TLS-wrapped closures that must not be stopped asynchronously.
    // Return "not suspended" gracefully instead of aborting the title.
    crate::xlog_debug!("SuspendThread: suspension is a no-op on Linux");
    0
}

/// Remember `prio` per handle; the host scheduler change is best-effort and its failure
/// (e.g. `EPERM` without `CAP_SYS_NICE`) never fails the call.
pub unsafe extern "win64" fn SetThreadPriority(handle: usize, prio: i32) -> i32 {
    if handle == current_thread_handle() || handle == 0 {
        // Best effort: lowering niceness requires no privilege only for raising it.
        // Attempt, ignore the result, and always succeed like the degraded path.
        let _ = libc::nice(0);
        static WARNED: AtomicU32 = AtomicU32::new(0);
        if WARNED.swap(1, Ordering::Relaxed) == 0 {
            crate::xlog_info!("SetThreadPriority({prio}): host scheduling is best-effort on Linux");
        }
        // Remember per pseudo-handle? Pseudo handles are process-wide; keep a single slot.
        static CURRENT_PRIO: AtomicU32 = AtomicU32::new(0);
        CURRENT_PRIO.store(prio as u32, Ordering::Relaxed);
        set_error(ERROR_SUCCESS);
        return 1;
    }
    let g = threads().lock().unwrap();
    match g.get(&handle) {
        Some(rec) => {
            rec.priority.store(prio as u32, Ordering::Relaxed);
            set_error(ERROR_SUCCESS);
            1
        }
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub unsafe extern "win64" fn GetThreadPriority(handle: usize) -> i32 {
    if handle == current_thread_handle() || handle == 0 {
        return 0;
    }
    threads()
        .lock()
        .unwrap()
        .get(&handle)
        .map(|r| r.priority.load(Ordering::Relaxed) as i32)
        .unwrap_or(0)
}

/// A mask from the PC's processors passes through; a console mask naming processors outside
/// the process is cut to those present, spreading onto them by index when none overlap.
/// Pure function (mirrors the Windows helper) so unit tests pin the mapping.
pub fn fit_affinity_mask(mask: usize, allowed: usize) -> usize {
    if mask == 0 || allowed == 0 || mask & !allowed == 0 {
        return mask;
    }
    if mask & allowed != 0 {
        return mask & allowed;
    }
    let pcs: Vec<u32> = (0..usize::BITS).filter(|b| allowed >> b & 1 != 0).collect();
    (0..usize::BITS)
        .filter(|b| mask >> b & 1 != 0)
        .fold(0, |m, b| m | 1 << pcs[b as usize % pcs.len()])
}

fn process_affinity() -> usize {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) != 0 {
            return usize::MAX;
        }
        let mut mask = 0usize;
        for cpu in 0..(8 * std::mem::size_of::<libc::cpu_set_t>() as u32).min(usize::BITS) {
            if libc::CPU_ISSET(cpu as usize, &set) {
                mask |= 1 << cpu;
            }
        }
        if mask == 0 { usize::MAX } else { mask }
    }
}

fn fitted_affinity(mask: usize, what: &str) -> usize {
    static LOGGED: OnceLock<Mutex<std::collections::BTreeSet<usize>>> = OnceLock::new();
    let allowed = process_affinity();
    let fit = fit_affinity_mask(mask, allowed);
    if fit != mask {
        let mut g = LOGGED
            .get_or_init(|| Mutex::new(std::collections::BTreeSet::new()))
            .lock()
            .unwrap();
        if g.insert(mask) {
            crate::xlog_info!(
                "{what}: mask {mask:#x} names processors outside the process's {allowed:#x} – {fit:#x}"
            );
        }
    }
    fit
}

fn apply_affinity_to_tid(tid: u32, mask: usize) -> bool {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for cpu in 0..usize::BITS {
            if mask >> cpu & 1 != 0 {
                libc::CPU_SET(cpu as usize, &mut set);
            }
        }
        // pid 0 = calling thread; a nonzero tid targets that kernel thread.
        libc::sched_setaffinity(tid as libc::pid_t, std::mem::size_of_val(&set), &set) == 0
    }
}

pub unsafe extern "win64" fn SetThreadAffinityMask(handle: usize, mask: usize) -> usize {
    let fit = fitted_affinity(mask, "SetThreadAffinityMask");
    let tid = resolve_tid(handle).unwrap_or(0);
    if !apply_affinity_to_tid(if handle == current_thread_handle() || handle == 0 { 0 } else { tid }, fit) {
        // Best effort: a mask the host rejects (e.g. offline CPU) does not fail the title.
        crate::xlog_debug!("SetThreadAffinityMask({mask:#x}): host rejected, kept {fit:#x}");
    }
    set_error(ERROR_SUCCESS);
    fit
}

#[repr(C)]
pub struct GroupAffinity {
    pub mask: usize,
    pub group: u16,
    pub reserved: [u16; 3],
}

pub unsafe extern "win64" fn SetThreadGroupAffinity(
    handle: usize,
    affinity: *const GroupAffinity,
    prev: *mut GroupAffinity,
) -> i32 {
    if affinity.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (*affinity).group != 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let fit = fitted_affinity((*affinity).mask, "SetThreadGroupAffinity");
    let tid = resolve_tid(handle).unwrap_or(0);
    let target = if handle == current_thread_handle() || handle == 0 {
        0
    } else {
        tid
    };
    if !apply_affinity_to_tid(target, fit) {
        crate::xlog_debug!("SetThreadGroupAffinity: host rejected the mask");
    }
    if !prev.is_null() {
        (*prev).mask = fit;
        (*prev).group = 0;
        (*prev).reserved = [0; 3];
    }
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn SetThreadpoolAffinityMask(
    _pool: *mut c_void,
    _mask: usize,
) -> i32 {
    1
}

pub unsafe extern "win64" fn SetThreadName(handle: usize, name: *const u16) -> i32 {
    let Some(n) = from_wide_ptr(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    crate::xlog_debug!("SetThreadName({n})");
    // Host-visible name: at most 15 bytes for pthread_setname_np.
    let short: Vec<u8> = n.bytes().take(15).collect();
    let target_tid_known = resolve_tid(handle);
    if handle == current_thread_handle() || handle == 0 || target_tid_known == Some(current_tid()) {
        if let Ok(c) = std::ffi::CString::new(short.clone()) {
            unsafe { libc::pthread_setname_np(libc::pthread_self(), c.as_ptr()) };
        }
    }
    if let Some(rec) = threads().lock().unwrap().get(&handle) {
        *rec.name.lock().unwrap() = n;
    }
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn GetThreadName(
    handle: usize,
    buf: *mut u16,
    len: usize,
    ret_len: *mut usize,
) -> i32 {
    let stored = threads()
        .lock()
        .unwrap()
        .get(&handle)
        .map(|r| r.name.lock().unwrap().clone());
    let current = handle == current_thread_handle() || handle == 0;
    let s = if current && stored.as_deref().is_none_or(|s| s.is_empty()) {
        let mut tmp = [0i8; 16];
        if unsafe { libc::pthread_getname_np(libc::pthread_self(), tmp.as_mut_ptr(), tmp.len()) } == 0 {
            let bytes: Vec<u8> = tmp
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            String::new()
        }
    } else {
        stored.unwrap_or_default()
    };
    let n = s.encode_utf16().count();
    if !ret_len.is_null() {
        *ret_len = n;
    }
    if buf.is_null() && len > 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if buf.is_null() || len <= n {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    crate::util::write_wide_out(&s, buf, len.min(u32::MAX as usize) as u32);
    set_error(ERROR_SUCCESS);
    1
}

// ---------------------------------------------------------------------------------------
// Sync: critical sections (inline, reentrant), SRW exclusive (inline atomic), events
// ---------------------------------------------------------------------------------------

/// Windows `RTL_CRITICAL_SECTION` layout (40 bytes on x86-64).
#[repr(C)]
pub struct CritSec {
    pub debug_info: usize,
    pub lock_count: i32,
    pub recursion: i32,
    pub owner: usize,
    pub sem: usize,
    pub spin: u32,
    _pad: u32,
}

struct CsState {
    owner: Mutex<(u32, u32)>, // (tid, recursion)
    cvar: Condvar,
}

static CS_MAP: OnceLock<Mutex<HashMap<usize, std::sync::Arc<CsState>>>> = OnceLock::new();

fn cs_map() -> &'static Mutex<HashMap<usize, std::sync::Arc<CsState>>> {
    CS_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

pub unsafe extern "win64" fn InitializeCriticalSection(cs: *mut CritSec) -> i32 {
    InitializeCriticalSectionAndSpinCount(cs, 0)
}

pub unsafe extern "win64" fn InitializeCriticalSectionAndSpinCount(
    cs: *mut CritSec,
    spin: u32,
) -> i32 {
    if cs.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    (*cs).debug_info = 0;
    (*cs).lock_count = -1;
    (*cs).recursion = 0;
    (*cs).owner = 0;
    (*cs).sem = 0;
    (*cs).spin = spin;
    cs_map().lock().unwrap().insert(
        cs as usize,
        std::sync::Arc::new(CsState {
            owner: Mutex::new((0, 0)),
            cvar: Condvar::new(),
        }),
    );
    set_error(ERROR_SUCCESS);
    1
}

fn cs_state_for(cs: usize) -> Option<std::sync::Arc<CsState>> {
    cs_map().lock().unwrap().get(&cs).cloned()
}

pub unsafe extern "win64" fn EnterCriticalSection(cs: *mut CritSec) {
    let Some(st) = cs_state_for(cs as usize) else {
        return;
    };
    let me = current_tid();
    let mut g = st.owner.lock().unwrap();
    if g.0 == me {
        g.1 += 1;
        return;
    }
    while g.0 != 0 {
        g = st.cvar.wait(g).unwrap();
    }
    *g = (me, 1);
}

pub unsafe extern "win64" fn TryEnterCriticalSection(cs: *mut CritSec) -> i32 {
    let Some(st) = cs_state_for(cs as usize) else {
        return 0;
    };
    let me = current_tid();
    let mut g = st.owner.lock().unwrap();
    if g.0 == 0 {
        *g = (me, 1);
        return 1;
    }
    if g.0 == me {
        g.1 += 1;
        return 1;
    }
    0
}

pub unsafe extern "win64" fn LeaveCriticalSection(cs: *mut CritSec) {
    let Some(st) = cs_state_for(cs as usize) else {
        return;
    };
    let me = current_tid();
    let mut g = st.owner.lock().unwrap();
    if g.0 != me {
        return;
    }
    g.1 -= 1;
    if g.1 == 0 {
        *g = (0, 0);
        st.cvar.notify_one();
    }
}

pub unsafe extern "win64" fn DeleteCriticalSection(cs: *mut CritSec) -> i32 {
    if cs.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    cs_map().lock().unwrap().remove(&(cs as usize));
    set_error(ERROR_SUCCESS);
    1
}

// SRWLOCK: 8 bytes, 0 = free, 1 = held (exclusive-only path; shared acquires are separate
// imports and remain stubs until the shared path lands).
pub unsafe extern "win64" fn InitializeSRWLock(lock: *mut usize) {
    if !lock.is_null() {
        *lock = 0;
    }
}

pub unsafe extern "win64" fn AcquireSRWLockExclusive(lock: *mut usize) {
    if lock.is_null() {
        return;
    }
    let a = &*(lock as *const AtomicUsize);
    let mut spins = 0u32;
    loop {
        if a
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        spins += 1;
        if spins < 1000 {
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
            spins = 0;
        }
    }
}

pub unsafe extern "win64" fn ReleaseSRWLockExclusive(lock: *mut usize) {
    if lock.is_null() {
        return;
    }
    (&*(lock as *const AtomicUsize)).store(0, Ordering::Release);
}

pub unsafe extern "win64" fn TryAcquireSRWLockExclusive(lock: *mut usize) -> i32 {
    if lock.is_null() {
        return 0;
    }
    (&*(lock as *const AtomicUsize))
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok() as i32
}

// Events.

struct EventState {
    manual: bool,
    signaled: Mutex<bool>,
    cvar: Condvar,
}

static EVENTS: OnceLock<Mutex<HashMap<usize, std::sync::Arc<EventState>>>> = OnceLock::new();

fn events() -> &'static Mutex<HashMap<usize, std::sync::Arc<EventState>>> {
    EVENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_event(manual: bool, initial: bool) -> usize {
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    events().lock().unwrap().insert(
        id,
        std::sync::Arc::new(EventState {
            manual,
            signaled: Mutex::new(initial),
            cvar: Condvar::new(),
        }),
    );
    id
}

pub unsafe extern "win64" fn CreateEventW(
    _attrs: *const c_void,
    manual: i32,
    initial: i32,
    _name: *const u16,
) -> usize {
    let id = new_event(manual != 0, initial != 0);
    set_error(ERROR_SUCCESS);
    id
}

pub unsafe extern "win64" fn CreateEventA(
    _attrs: *const c_void,
    manual: i32,
    initial: i32,
    _name: *const u8,
) -> usize {
    let id = new_event(manual != 0, initial != 0);
    set_error(ERROR_SUCCESS);
    id
}

pub unsafe extern "win64" fn SetEvent(handle: usize) -> i32 {
    let Some(ev) = events().lock().unwrap().get(&handle).cloned() else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    *ev.signaled.lock().unwrap() = true;
    if ev.manual {
        ev.cvar.notify_all();
    } else {
        ev.cvar.notify_one();
    }
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn ResetEvent(handle: usize) -> i32 {
    let Some(ev) = events().lock().unwrap().get(&handle).cloned() else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    *ev.signaled.lock().unwrap() = false;
    set_error(ERROR_SUCCESS);
    1
}

fn wait_event(ev: &std::sync::Arc<EventState>, timeout_ms: u32) -> bool {
    let mut g = ev.signaled.lock().unwrap();
    if *g {
        if !ev.manual {
            *g = false;
        }
        return true;
    }
    if timeout_ms == 0 {
        return false;
    }
    if timeout_ms == INFINITE {
        while !*g {
            g = ev.cvar.wait(g).unwrap();
        }
        if !ev.manual {
            *g = false;
        }
        return true;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    while !*g {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (guard, timeout) = ev.cvar.wait_timeout(g, deadline - now).unwrap();
        g = guard;
        if *g {
            break;
        }
        if timeout.timed_out() {
            return false;
        }
    }
    if !ev.manual {
        *g = false;
    }
    true
}

fn is_event(handle: usize) -> Option<std::sync::Arc<EventState>> {
    events().lock().unwrap().get(&handle).cloned()
}

fn join_if_finished(handle: usize) -> Option<u32> {
    let mut g = joins().lock().unwrap();
    let h = g.get(&handle)?;
    if !h.is_finished() {
        return None;
    }
    let h = g.remove(&handle)?;
    drop(g);
    h.join().ok()
}

pub unsafe extern "win64" fn WaitForSingleObject(handle: usize, timeout_ms: u32) -> u32 {
    if let Some(ev) = is_event(handle) {
        return if wait_event(&ev, timeout_ms) {
            WAIT_OBJECT_0
        } else {
            WAIT_TIMEOUT
        };
    }
    if joins().lock().unwrap().contains_key(&handle) {
        if let Some(code) = join_if_finished(handle) {
            let _ = code;
            return WAIT_OBJECT_0;
        }
        if timeout_ms == 0 {
            return WAIT_TIMEOUT;
        }
        let deadline = if timeout_ms == INFINITE {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
        };
        loop {
            std::thread::sleep(Duration::from_millis(1));
            if let Some(code) = join_if_finished(handle) {
                let _ = code;
                return WAIT_OBJECT_0;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return WAIT_TIMEOUT;
            }
        }
    }
    set_error(ERROR_INVALID_PARAMETER);
    WAIT_FAILED
}

pub unsafe extern "win64" fn WaitForMultipleObjects(
    n: u32,
    handles: *const usize,
    wait_all: i32,
    timeout_ms: u32,
) -> u32 {
    if handles.is_null() || n == 0 || n > 64 {
        set_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    let list = std::slice::from_raw_parts(handles, n as usize).to_vec();
    // Fast path: anything already signaled.
    let deadline = if timeout_ms == INFINITE {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
    };
    loop {
        if wait_all != 0 {
            let mut all = true;
            for &h in &list {
                if let Some(ev) = is_event(h) {
                    if !*ev.signaled.lock().unwrap() {
                        all = false;
                        break;
                    }
                } else if joins().lock().unwrap().contains_key(&h) {
                    if join_if_finished(h).is_none() {
                        // `join_if_finished` consumes a finished join; re-check below.
                        // If it returned None the thread is still running — unless it just
                        // finished and was consumed, in which case the handle is gone and
                        // the wait is satisfied. Distinguish via presence.
                        if joins().lock().unwrap().contains_key(&h) {
                            all = false;
                            break;
                        }
                    }
                } else {
                    set_error(ERROR_INVALID_PARAMETER);
                    return WAIT_FAILED;
                }
            }
            if all {
                // Consume auto-reset events.
                for &h in &list {
                    if let Some(ev) = is_event(h) {
                        if !ev.manual {
                            *ev.signaled.lock().unwrap() = false;
                        }
                    }
                }
                return WAIT_OBJECT_0;
            }
        } else {
            for (i, &h) in list.iter().enumerate() {
                if let Some(ev) = is_event(h) {
                    let mut g = ev.signaled.lock().unwrap();
                    if *g {
                        if !ev.manual {
                            *g = false;
                        }
                        return WAIT_OBJECT_0 + i as u32;
                    }
                } else if joins().lock().unwrap().contains_key(&h) {
                    if join_if_finished(h).is_some() || !joins().lock().unwrap().contains_key(&h) {
                        return WAIT_OBJECT_0 + i as u32;
                    }
                } else {
                    set_error(ERROR_INVALID_PARAMETER);
                    return WAIT_FAILED;
                }
            }
        }
        if timeout_ms == 0 {
            return WAIT_TIMEOUT;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return WAIT_TIMEOUT;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub unsafe extern "win64" fn CloseHandle(handle: usize) -> i32 {
    if events().lock().unwrap().remove(&handle).is_some() {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if close_file_handle(handle) {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if close_find_handle(handle) {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if threads().lock().unwrap().contains_key(&handle) {
        // A running game thread cannot be force-closed; detach the record and let it run.
        // A finished thread's join is dropped here.
        joins().lock().unwrap().remove(&handle);
        set_error(ERROR_SUCCESS);
        return 1;
    }
    set_error(ERROR_INVALID_PARAMETER);
    0
}

// ---------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------

const QPC_FREQUENCY: u64 = 10_000_000;

pub unsafe extern "win64" fn QueryPerformanceFrequency(out: *mut u64) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    *out = QPC_FREQUENCY;
    set_error(ERROR_SUCCESS);
    1
}

fn monotonic_now() -> (u64, u32) {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64, ts.tv_nsec as u32)
}

pub unsafe extern "win64" fn QueryPerformanceCounter(out: *mut u64) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let (sec, nsec) = monotonic_now();
    *out = sec * QPC_FREQUENCY + nsec as u64 / 100;
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn GetTickCount64() -> u64 {
    let (sec, nsec) = monotonic_now();
    sec * 1000 + nsec as u64 / 1_000_000
}

pub unsafe extern "win64" fn GetTickCount() -> u32 {
    GetTickCount64() as u32
}

#[repr(C)]
pub struct FileTime {
    pub low: u32,
    pub high: u32,
}

pub unsafe extern "win64" fn GetSystemTimeAsFileTime(out: *mut FileTime) {
    if out.is_null() {
        return;
    }
    let mut ts: libc::timespec = std::mem::zeroed();
    libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    let ticks = (ts.tv_sec as u64 + 11_644_473_600) * 10_000_000 + ts.tv_nsec as u64 / 100;
    (*out).low = ticks as u32;
    (*out).high = (ticks >> 32) as u32;
}

pub unsafe extern "win64" fn Sleep(ms: u32) {
    if ms == 0 {
        std::thread::yield_now();
        return;
    }
    let req = libc::timespec {
        tv_sec: (ms / 1000) as libc::time_t,
        tv_nsec: ((ms % 1000) * 1_000_000) as libc::c_long,
    };
    libc::nanosleep(&req, std::ptr::null_mut());
}

pub unsafe extern "win64" fn SleepEx(ms: u32, _alertable: i32) -> u32 {
    Sleep(ms);
    0
}

// ---------------------------------------------------------------------------------------
// Console identity, memory budget, debug output, registry stub, exception filter
// ---------------------------------------------------------------------------------------

pub unsafe extern "win64" fn GetConsoleType() -> u32 {
    crate::state::config().console_type_raw
}

#[repr(C)]
pub struct SystemOsVersionInfo {
    pub major: u8,
    pub minor: u8,
    pub build: u16,
    pub revision: u16,
}

pub unsafe extern "win64" fn GetSystemOSVersion(out: *mut SystemOsVersionInfo) {
    if out.is_null() {
        return;
    }
    *out = SystemOsVersionInfo {
        major: 10,
        minor: 0,
        build: 22631,
        revision: 0,
    };
}

#[repr(C)]
pub struct TitleMemoryStatusT {
    pub length: u32,
    pub reserved: u32,
    pub total: u64,
    pub avail: u64,
    pub legacy_used: u64,
    pub legacy_peak: u64,
    pub legacy_avail: u64,
    pub title_used: u64,
    pub title_avail: u64,
    pub legacy_page_table_used: u64,
    pub title_page_table_used: u64,
}

pub unsafe extern "win64" fn TitleMemoryStatus(buf: *mut TitleMemoryStatusT) -> i32 {
    if buf.is_null() || (*buf).length < std::mem::size_of::<TitleMemoryStatusT>() as u32 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let total = crate::state::config().title_memory_budget_mb() * 1024 * 1024;
    let used = crate::platform::process_private_bytes().min(total);
    let b = &mut *buf;
    b.total = total;
    b.avail = total - used;
    b.legacy_used = 0;
    b.legacy_peak = 0;
    b.legacy_avail = 0;
    b.title_used = used;
    b.title_avail = total - used;
    b.legacy_page_table_used = 0;
    b.title_page_table_used = 0;
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn JobTitleMemoryStatus(buf: *mut TitleMemoryStatusT) -> i32 {
    TitleMemoryStatus(buf)
}

pub unsafe extern "win64" fn ToolingMemoryStatus(buf: *mut u32) -> i32 {
    if buf.is_null() || *buf < 40 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    std::ptr::write_bytes((buf as *mut u8).add(8), 0, 32);
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn XMemGetWorkingSetStatistics(
    _flags: u32,
    stats: *mut u8,
) -> i32 {
    if stats.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let total = crate::state::config().title_memory_budget_mb() * 1024 * 1024;
    (stats as *mut u64).write_unaligned(total);
    (stats.add(8) as *mut u64)
        .write_unaligned(crate::platform::process_private_bytes().min(total));
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn XMemTransferMemory(_direction: u32, _size: usize) -> i32 {
    1
}

static XMEM_ALLOC_HOOK: AtomicUsize = AtomicUsize::new(0);
static XMEM_FREE_HOOK: AtomicUsize = AtomicUsize::new(0);

pub unsafe extern "win64" fn XMemSetAllocationHooks(alloc: usize, free: usize) {
    if alloc == 0 {
        XMEM_ALLOC_HOOK.store(0, Ordering::Release);
        XMEM_FREE_HOOK.store(0, Ordering::Release);
    } else {
        XMEM_ALLOC_HOOK.store(alloc, Ordering::Release);
        XMEM_FREE_HOOK.store(free, Ordering::Release);
    }
}

pub unsafe extern "win64" fn OutputDebugStringA(s: *const u8) {
    if let Some(m) = from_ansi_ptr(s) {
        crate::xlog_info!("[debug] {}", m.trim_end());
    }
}

pub unsafe extern "win64" fn OutputDebugStringW(s: *const u16) {
    if let Some(m) = from_wide_ptr(s) {
        crate::xlog_info!("[debug] {}", m.trim_end());
    }
}

static TITLE_FILTER: AtomicUsize = AtomicUsize::new(0);

pub unsafe extern "win64" fn SetUnhandledExceptionFilter(f: usize) -> usize {
    TITLE_FILTER.swap(f, Ordering::AcqRel)
}

const OEM_KEY: &str = "software\\microsoft\\windows nt\\currentversion\\oem";
const OEM_HANDLE: usize = 0x5843_4C4F; // "XCLO"

fn console_mode() -> u32 {
    if crate::state::config().is_series_s() {
        0x60
    } else {
        0x64
    }
}

unsafe fn is_oem_key(root: usize, path: *const u16) -> bool {
    const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
    root == HKEY_LOCAL_MACHINE
        && from_wide_ptr(path).is_some_and(|p| p.trim_start_matches('\\').eq_ignore_ascii_case(OEM_KEY))
}

pub unsafe extern "win64" fn RegOpenKeyExW(
    root: usize,
    path: *const u16,
    _options: u32,
    _sam: u32,
    out: *mut usize,
) -> u32 {
    if is_oem_key(root, path) && !out.is_null() {
        *out = OEM_HANDLE;
        return 0;
    }
    // No Win32 registry on Linux: only the console-mode key is answered.
    ERROR_FILE_NOT_FOUND
}

pub unsafe extern "win64" fn RegOpenKeyW(root: usize, path: *const u16, out: *mut usize) -> u32 {
    RegOpenKeyExW(root, path, 0, 0, out)
}

pub unsafe extern "win64" fn RegQueryValueExW(
    key: usize,
    name: *const u16,
    _reserved: *mut u32,
    ty: *mut u32,
    data: *mut u8,
    size: *mut u32,
) -> u32 {
    if key != OEM_HANDLE {
        return ERROR_FILE_NOT_FOUND;
    }
    const ERROR_MORE_DATA: u32 = 234;
    if !from_wide_ptr(name).is_some_and(|n| n.eq_ignore_ascii_case("ConsoleMode")) {
        return ERROR_FILE_NOT_FOUND;
    }
    if !ty.is_null() {
        *ty = 4; // REG_DWORD
    }
    if !data.is_null() {
        if size.is_null() || *size < 4 {
            if !size.is_null() {
                *size = 4;
            }
            return ERROR_MORE_DATA;
        }
        std::ptr::copy_nonoverlapping(console_mode().to_le_bytes().as_ptr(), data, 4);
    }
    if !size.is_null() {
        *size = 4;
    }
    0
}

pub unsafe extern "win64" fn RegCloseKey(key: usize) -> u32 {
    if key == OEM_HANDLE {
        0
    } else {
        ERROR_FILE_NOT_FOUND
    }
}

/// `ULONG RegisterAppStateChangeNotification(...)`: a title is never suspended here.
static APP_NOTIFICATION: u8 = 0;

pub unsafe extern "win64" fn RegisterAppStateChangeNotification(
    _routine: usize,
    _ctx: *mut c_void,
    out: *mut *const u8,
) -> u32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return ERROR_INVALID_PARAMETER;
    }
    *out = &APP_NOTIFICATION;
    0
}

pub unsafe extern "win64" fn UnregisterAppStateChangeNotification(_reg: *const u8) {}

pub unsafe extern "win64" fn RegisterAppConstrainedChangeNotification(
    routine: usize,
    ctx: *mut c_void,
    out: *mut *const u8,
) -> u32 {
    RegisterAppStateChangeNotification(routine, ctx, out)
}

pub unsafe extern "win64" fn UnregisterAppConstrainedChangeNotification(_reg: *const u8) {}

pub unsafe extern "win64" fn QueryProcessorSchedulingStatistics(out: *mut [u64; 3]) {
    if out.is_null() {
        return;
    }
    // From /proc/stat: [busy, idle, total] in USER_HZ ticks.
    let mut busy = 0u64;
    let mut idle = 0u64;
    if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
        if let Some(line) = stat.lines().next() {
            let nums: Vec<u64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|w| w.parse().ok())
                .collect();
            if nums.len() >= 4 {
                idle = nums[3] + nums.get(4).copied().unwrap_or(0);
                busy = nums.iter().sum::<u64>().saturating_sub(idle);
            }
        }
    }
    *out = [busy, idle, busy + idle];
}

/// Query a mapped range's state for `XMemVirtualQuery`-style callers (minimal Linux twin:
/// parses `/proc/self/maps` for the containing run).
pub unsafe extern "win64" fn XMemVirtualQuery(
    proc_: usize,
    address: *const c_void,
    info: *mut u8,
) -> i32 {
    if info.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if proc_ != current_process_handle() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Minimal MEMORY_BASIC_INFORMATION (28 bytes on 64-bit Windows layout used here:
    // BaseAddress, AllocationBase, AllocationProtect, RegionSize, State, Protect, Type).
    // The caller also reads a CPU-readable bit at +48; only that and State/Protect matter.
    let va = address as usize;
    let mut state = 0x10000u32; // MEM_FREE
    let mut protect = PAGE_NOACCESS;
    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
        for line in maps.lines() {
            let mut it = line.split_whitespace();
            let (Some(range), Some(perms)) = (it.next(), it.next()) else {
                continue;
            };
            let Some((a, b)) = range.split_once('-') else {
                continue;
            };
            let (Ok(a), Ok(b)) = (
                usize::from_str_radix(a, 16),
                usize::from_str_radix(b, 16),
            ) else {
                continue;
            };
            if va >= a && va < b {
                state = 0x1000; // MEM_COMMIT
                protect = if perms.starts_with("---") {
                    PAGE_NOACCESS
                } else if perms.starts_with("r--") {
                    PAGE_READONLY
                } else if perms.starts_with("rw-") {
                    PAGE_READWRITE
                } else if perms.starts_with("r-x") {
                    PAGE_EXECUTE_READ
                } else {
                    PAGE_READWRITE
                };
                break;
            }
        }
    }
    // Zero the 64-byte caller buffer, then write State/Protect and the CPU bit.
    std::ptr::write_bytes(info, 0, 64);
    // Offsets in the Windows struct (64-bit): State +16, Protect +20.
    (info.add(16) as *mut u32).write_unaligned(state);
    (info.add(20) as *mut u32).write_unaligned(protect);
    const READABLE: u32 = PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ
        | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    let cpu = (state == 0x1000 && protect & READABLE != 0) as u32;
    (info.add(48) as *mut u32).write_unaligned(cpu);
    set_error(ERROR_SUCCESS);
    1
}

// ---------------------------------------------------------------------------------------
// Files (Phase C step 10c): title-facing operations through the linked-in Linux VFS.
// ---------------------------------------------------------------------------------------
//
// The Windows build hooks KERNELBASE so the CRT (`fopen` → `CreateFileW`) and every
// system DLL go through the VFS. Linux has no KERNELBASE: these HLE `kernelx` file APIs
// call the VFS directly on the host call path (`vfs_hooks_linux::open` for data files,
// `Vfs::{delete_file,create_dir,remove_dir,stage_write,commit_write}` for metadata).
// Nothing is interposed process-wide (`LD_PRELOAD` is not used).
//
// Reads see the read-only game layer, writes land in the copy-on-write overlay, and
// `S:`/`T:` use the title data dir. An unmapped drive or a traversal/symlink escape is
// a clean Win32 error, never a host file. `CloseHandle` below also closes file and find
// handles.

const INVALID_HANDLE_VALUE: usize = usize::MAX;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

// Win32 creation dispositions used here (match `vfs`).
const CREATE_ALWAYS: u32 = 2;
const OPEN_ALWAYS: u32 = 4;

// Move/Copy flags.
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
const MOVEFILE_COPY_ALLOWED: u32 = 0x2;
const COPY_FILE_FAIL_IF_EXISTS: u32 = 0x1;

// File attributes.
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;

// Seek origins.
const FILE_BEGIN: u32 = 0;
const FILE_CURRENT: u32 = 1;
const FILE_END: u32 = 2;

/// Windows `WIN32_FIND_DATAW` (592 bytes on x86-64). The game reads this layout
/// directly, so it must match the Windows definition field for field.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FindDataW {
    pub attrs: u32,
    pub creation_low: u32,
    pub creation_high: u32,
    pub access_low: u32,
    pub access_high: u32,
    pub write_low: u32,
    pub write_high: u32,
    pub size_high: u32,
    pub size_low: u32,
    pub reserved0: u32,
    pub reserved1: u32,
    pub name: [u16; 260],
    pub alt_name: [u16; 14],
}

struct FindState {
    items: Vec<FindDataW>,
    pos: usize,
}

static FILES: OnceLock<Mutex<HashMap<usize, std::sync::Arc<Mutex<std::fs::File>>>>> =
    OnceLock::new();
static FINDS: OnceLock<Mutex<HashMap<usize, FindState>>> = OnceLock::new();

fn files() -> &'static Mutex<HashMap<usize, std::sync::Arc<Mutex<std::fs::File>>>> {
    FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn finds() -> &'static Mutex<HashMap<usize, FindState>> {
    FINDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_file(f: std::fs::File) -> usize {
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    files()
        .lock()
        .unwrap()
        .insert(h, std::sync::Arc::new(Mutex::new(f)));
    h
}

fn with_file<T>(h: usize, f: impl FnOnce(&std::fs::File) -> T) -> Option<T> {
    let arc = files().lock().unwrap().get(&h).cloned()?;
    let g = arc.lock().unwrap();
    Some(f(&g))
}

fn with_file_mut<T>(h: usize, f: impl FnOnce(&mut std::fs::File) -> T) -> Option<T> {
    let arc = files().lock().unwrap().get(&h).cloned()?;
    let mut g = arc.lock().unwrap();
    Some(f(&mut g))
}

fn close_file_handle(h: usize) -> bool {
    files().lock().unwrap().remove(&h).is_some()
}

fn close_find_handle(h: usize) -> bool {
    finds().lock().unwrap().remove(&h).is_some()
}

/// Case-insensitive `*`/`?` match for Find patterns (Windows semantics, ASCII only).
fn pattern_matches(pat: &str, name: &str) -> bool {
    let (p, n) = (pat.as_bytes(), name.as_bytes());
    // Lowercase ASCII copies for case-insensitive compare.
    let lower = |b: &[u8]| b.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<u8>>();
    let p = lower(p);
    let n = lower(n);
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if star.is_some() {
            pi = star.unwrap() + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn filetime_from_system(t: std::time::SystemTime) -> (u32, u32) {
    // Windows FILETIME: 100ns ticks since 1601-01-01.
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let ticks = dur.as_secs() * 10_000_000 + dur.subsec_nanos() as u64 / 100 + 11_644_473_600 * 10_000_000;
    (ticks as u32, (ticks >> 32) as u32)
}

fn find_data_for(name: &str, meta: &std::fs::Metadata) -> FindDataW {
    let mut wname = [0u16; 260];
    let enc: Vec<u16> = name.encode_utf16().take(259).collect();
    wname[..enc.len()].copy_from_slice(&enc);
    let is_dir = meta.is_dir();
    let mut attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    if meta.permissions().readonly() {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }
    let len = meta.len();
    let (cw_l, cw_h) = meta
        .created()
        .map(filetime_from_system)
        .unwrap_or((0, 0));
    let (ca_l, ca_h) = meta
        .accessed()
        .map(filetime_from_system)
        .unwrap_or((cw_l, cw_h));
    let (ww_l, ww_h) = meta
        .modified()
        .map(filetime_from_system)
        .unwrap_or((cw_l, cw_h));
    FindDataW {
        attrs,
        creation_low: cw_l,
        creation_high: cw_h,
        access_low: ca_l,
        access_high: ca_h,
        write_low: ww_l,
        write_high: ww_h,
        size_high: (len >> 32) as u32,
        size_low: len as u32,
        reserved0: 0,
        reserved1: 0,
        name: wname,
        alt_name: [0u16; 14],
    }
}

fn list_host_dir(path: &str, pattern: &str) -> Vec<FindDataW> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(path) else {
        return out;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n == ".xcl_whiteouts" {
            continue;
        }
        if !pattern_matches(pattern, &n) {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        out.push(find_data_for(&n, &m));
    }
    out.sort_by(|a, b| {
        let an = a.name.iter().position(|&c| c == 0).unwrap_or(a.name.len());
        let bn = b.name.iter().position(|&c| c == 0).unwrap_or(b.name.len());
        String::from_utf16_lossy(&a.name[..an]).cmp(&String::from_utf16_lossy(&b.name[..bn]))
    });
    out
}

fn game_dir_entries(v: &crate::vfs::Vfs, dir_rel: &str, pattern: &str) -> Result<Vec<FindDataW>, u32> {
    use crate::vfs::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    if !dir_rel.is_empty() && !v.is_dir(dir_rel) {
        return Err(ERROR_PATH_NOT_FOUND);
    }
    let wildcard = pattern.contains('*') || pattern.contains('?');
    if !wildcard {
        // Exact name: single entry when visible.
        let child = if dir_rel.is_empty() {
            pattern.to_string()
        } else {
            format!("{dir_rel}\\{pattern}")
        };
        if v.is_whiteout(&child) {
            return Err(ERROR_FILE_NOT_FOUND);
        }
        if v.visible_in_overlay(&child) || v.visible_in_original(&child) {
            let host = v.read_path_rel(&child).ok_or(ERROR_FILE_NOT_FOUND)?;
            let meta = std::fs::metadata(&host).map_err(|_| ERROR_FILE_NOT_FOUND)?;
            return Ok(vec![find_data_for(pattern, &meta)]);
        }
        return Err(ERROR_FILE_NOT_FOUND);
    }
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let overlay_dir = v.overlay_path(dir_rel);
    if v.needs_merge(dir_rel) || std::path::Path::new(&overlay_dir).is_dir() {
        for d in list_host_dir(&overlay_dir, pattern) {
            let n = {
                let len = d.name.iter().position(|&c| c == 0).unwrap_or(d.name.len());
                String::from_utf16_lossy(&d.name[..len])
            };
            if seen.insert(n.to_lowercase()) {
                items.push(d);
            }
        }
    }
    if dir_rel.is_empty() || !v.is_whiteout(dir_rel) {
        let orig_dir = v.original_path(dir_rel);
        for d in list_host_dir(&orig_dir, pattern) {
            let n = {
                let len = d.name.iter().position(|&c| c == 0).unwrap_or(d.name.len());
                String::from_utf16_lossy(&d.name[..len])
            };
            if n == "." || n == ".." {
                continue;
            }
            let child = if dir_rel.is_empty() {
                n.clone()
            } else {
                format!("{dir_rel}\\{n}")
            };
            if v.is_whiteout(&child) {
                continue;
            }
            if seen.insert(n.to_lowercase()) {
                items.push(d);
            }
        }
    }
    if items.is_empty() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    Ok(items)
}

/// Split `G:\dir\pat*` into (`dir_rel`, `pattern`) for game paths. `dir_rel` uses `\`.
fn split_find_rel(rel: &str) -> (String, String) {
    match rel.rsplit_once('\\') {
        Some((d, p)) => (d.to_string(), p.to_string()),
        None => (String::new(), rel.to_string()),
    }
}

fn find_first_impl(path: &str, out: *mut FindDataW) -> Result<usize, u32> {
    use crate::vfs::Target;
    let v = crate::vfs::get();
    let Some(v) = v else {
        // No VFS yet: plain host listing.
        let (dir, pat) = match path.rsplit_once(['/', '\\']) {
            Some((d, p)) => (d.to_string(), p.to_string()),
            None => (".".to_string(), path.to_string()),
        };
        let items = list_host_dir(&dir, &pat);
        if items.is_empty() {
            return Err(crate::vfs::ERROR_FILE_NOT_FOUND);
        }
        unsafe {
            *out = items[0];
        }
        let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        finds().lock().unwrap().insert(
            h,
            FindState {
                items,
                pos: 1,
            },
        );
        return Ok(h);
    };
    match v.classify(path) {
        Target::Pass => {
            let (dir, pat) = match path.rsplit_once(['/', '\\']) {
                Some((d, p)) => (d.to_string(), p.to_string()),
                None => (".".to_string(), path.to_string()),
            };
            let items = list_host_dir(&dir, &pat);
            if items.is_empty() {
                return Err(crate::vfs::ERROR_FILE_NOT_FOUND);
            }
            unsafe {
                *out = items[0];
            }
            let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            finds().lock().unwrap().insert(
                h,
                FindState {
                    items,
                    pos: 1,
                },
            );
            Ok(h)
        }
        Target::Translated(p) => {
            let (dir, pat) = match p.rsplit_once('/') {
                Some((d, q)) => (d.to_string(), q.to_string()),
                None => (p.clone(), "*".to_string()),
            };
            // A pattern without a directory (bare `*`) lists the translated dir itself.
            let (dir, pat) = if dir.is_empty() { (p.clone(), pat) } else { (dir, pat) };
            let items = list_host_dir(&dir, &pat);
            if items.is_empty() {
                return Err(crate::vfs::ERROR_FILE_NOT_FOUND);
            }
            unsafe {
                *out = items[0];
            }
            let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            finds().lock().unwrap().insert(
                h,
                FindState {
                    items,
                    pos: 1,
                },
            );
            Ok(h)
        }
        Target::Game { rel } => {
            let (dir_rel, pat) = split_find_rel(&rel);
            let items = game_dir_entries(v, &dir_rel, &pat)?;
            unsafe {
                *out = items[0];
            }
            let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            finds().lock().unwrap().insert(
                h,
                FindState {
                    items,
                    pos: 1,
                },
            );
            Ok(h)
        }
        #[cfg(not(windows))]
        Target::Reject(e) => Err(e),
    }
}

fn host_statvfs(path: &str, out: &mut libc::statvfs) -> bool {
    use std::ffi::CString;
    let Ok(c) = CString::new(path) else { return false };
    unsafe { libc::statvfs(c.as_ptr(), out) == 0 }
}

fn disk_space_for(path: &str) -> Option<(u64, u64)> {
    // Returns (available_bytes, total_bytes) for the volume behind `path`.
    let v = crate::vfs::get();
    let host: String = if let Some(v) = v {
        use crate::vfs::Target;
        match v.classify(path) {
            Target::Pass => path.to_string(),
            Target::Translated(p) => p,
            Target::Game { rel } => {
                if rel.is_empty() {
                    v.game_root().to_string()
                } else {
                    v.read_path_rel(&rel)?
                }
            }
            #[cfg(not(windows))]
            Target::Reject(_) => return None,
        }
    } else {
        path.to_string()
    };
    // `statvfs` needs an existing path: walk up to the nearest existing ancestor.
    let mut probe = std::path::PathBuf::from(&host);
    loop {
        if probe.exists() {
            break;
        }
        if !probe.pop() {
            return None;
        }
    }
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if !host_statvfs(&probe.to_string_lossy(), &mut st) {
        return None;
    }
    let bsize = st.f_bsize as u64;
    Some((st.f_bavail as u64 * bsize, st.f_blocks as u64 * bsize))
}

fn create_file_impl(path: &str, access: u32, disposition: u32, flags: u32) -> Result<usize, u32> {
    let opened = crate::vfs_hooks::open(path, access, disposition, flags)?;
    // `CREATE_ALWAYS`/`OPEN_ALWAYS` report pre-existence via the last error while
    // still succeeding, as on Windows.
    if opened.already_existed && (disposition == CREATE_ALWAYS || disposition == OPEN_ALWAYS) {
        set_error(crate::vfs::ERROR_ALREADY_EXISTS);
    } else {
        set_error(ERROR_SUCCESS);
    }
    // Directories opened with backup semantics hold an O_PATH descriptor that Rust
    // `File` cannot read; the VFS `open` already validated containment, and directory
    // reads go through Find, not ReadFile.
    Ok(register_file(opened.file))
}

unsafe fn wide_str(p: *const u16) -> Option<String> {
    from_wide_ptr(p)
}

unsafe fn ansi_str(p: *const u8) -> Option<String> {
    from_ansi_ptr(p)
}

pub unsafe extern "win64" fn CreateFileW(
    name: *const u16,
    access: u32,
    _share: u32,
    _attrs: *const c_void,
    disposition: u32,
    flags: u32,
    _template: usize,
) -> usize {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    };
    match create_file_impl(&path, access, disposition, flags) {
        Ok(h) => h,
        Err(e) => {
            set_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "win64" fn CreateFileA(
    name: *const u8,
    access: u32,
    _share: u32,
    _attrs: *const c_void,
    disposition: u32,
    flags: u32,
    _template: usize,
) -> usize {
    let Some(path) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    };
    match create_file_impl(&path, access, disposition, flags) {
        Ok(h) => h,
        Err(e) => {
            set_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "win64" fn CreateFile2(
    name: *const u16,
    access: u32,
    _share: u32,
    disposition: u32,
    params: *const c_void,
) -> usize {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    };
    // CREATEFILE2_EXTENDED_PARAMETERS { dwSize, dwFileAttributes, dwFileFlags, ... }.
    let flags = if params.is_null() {
        0
    } else {
        unsafe { *((params as *const u8).add(8) as *const u32) }
    };
    match create_file_impl(&path, access, disposition, flags) {
        Ok(h) => h,
        Err(e) => {
            set_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "win64" fn ReadFile(
    handle: usize,
    buf: *mut c_void,
    n: u32,
    done: *mut u32,
    _ov: *mut c_void,
) -> i32 {
    if buf.is_null() && n != 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let arc = files().lock().unwrap().get(&handle).cloned();
    let Some(arc) = arc else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, n as usize) };
    let mut g = arc.lock().unwrap();
    match std::io::Read::read(&mut *g, slice) {
        Ok(k) => {
            if !done.is_null() {
                *done = k as u32;
            }
            set_error(ERROR_SUCCESS);
            1
        }
        Err(e) => {
            set_error(e.raw_os_error().unwrap_or(5) as u32);
            0
        }
    }
}

pub unsafe extern "win64" fn WriteFile(
    handle: usize,
    buf: *const c_void,
    n: u32,
    done: *mut u32,
    _ov: *mut c_void,
) -> i32 {
    if buf.is_null() && n != 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let arc = files().lock().unwrap().get(&handle).cloned();
    let Some(arc) = arc else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let slice = unsafe { std::slice::from_raw_parts(buf as *const u8, n as usize) };
    let mut g = arc.lock().unwrap();
    match std::io::Write::write(&mut *g, slice) {
        Ok(k) => {
            if !done.is_null() {
                *done = k as u32;
            }
            set_error(ERROR_SUCCESS);
            1
        }
        Err(e) => {
            set_error(e.raw_os_error().unwrap_or(5) as u32);
            0
        }
    }
}

pub unsafe extern "win64" fn FlushFileBuffers(handle: usize) -> i32 {
    let ok = with_file(handle, |f| std::io::Write::flush(&mut &*f).is_ok()).unwrap_or(false);
    // `File::sync_all` is the durable flush; a shared read handle flushes trivially.
    if ok {
        set_error(ERROR_SUCCESS);
        1
    } else {
        // Try a metadata-only handle (O_PATH): nothing to flush, still success when known.
        if files().lock().unwrap().contains_key(&handle) {
            set_error(ERROR_SUCCESS);
            return 1;
        }
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

pub unsafe extern "win64" fn GetFileSizeEx(handle: usize, out: *mut u64) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let len = with_file(handle, |f| f.metadata().map(|m| m.len()).unwrap_or(u64::MAX));
    match len {
        Some(n) if n != u64::MAX => {
            *out = n;
            set_error(ERROR_SUCCESS);
            1
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub unsafe extern "win64" fn SetFilePointerEx(
    handle: usize,
    dist: i64,
    new_pos: *mut u64,
    origin: u32,
) -> i32 {
    use std::io::SeekFrom;
    let from = match origin {
        FILE_BEGIN => SeekFrom::Start(dist as u64),
        FILE_CURRENT => SeekFrom::Current(dist),
        FILE_END => SeekFrom::End(dist),
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
    };
    let pos = with_file_mut(handle, |f| {
        use std::io::Seek;
        f.seek(from).map(|p| p)
    });
    match pos {
        Some(Ok(p)) => {
            if !new_pos.is_null() {
                *new_pos = p;
            }
            set_error(ERROR_SUCCESS);
            1
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub unsafe extern "win64" fn SetEndOfFile(handle: usize) -> i32 {
    let ok = with_file_mut(handle, |f| {
        use std::io::Seek;
        let pos = f.seek(std::io::SeekFrom::Current(0)).ok()?;
        f.set_len(pos).ok()
    })
    .unwrap_or(None)
    .is_some();
    if ok {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

pub unsafe extern "win64" fn DeleteFileW(name: *const u16) -> i32 {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    delete_file_impl(&path)
}

pub unsafe extern "win64" fn DeleteFileA(name: *const u8) -> i32 {
    let Some(path) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    delete_file_impl(&path)
}

fn delete_file_impl(path: &str) -> i32 {
    let Some(v) = crate::vfs::get() else {
        return match std::fs::remove_file(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(2) as u32);
                0
            }
        };
    };
    use crate::vfs::Target;
    match v.classify(path) {
        Target::Pass => match std::fs::remove_file(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(2) as u32);
                0
            }
        },
        Target::Translated(p) => match std::fs::remove_file(&p) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(2) as u32);
                0
            }
        },
        Target::Game { rel } => match v.delete_file(&rel) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e);
                0
            }
        },
        #[cfg(not(windows))]
        Target::Reject(e) => {
            set_error(e);
            0
        }
    }
}

pub unsafe extern "win64" fn CreateDirectoryW(name: *const u16, _sa: *const c_void) -> i32 {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    create_dir_impl(&path)
}

pub unsafe extern "win64" fn CreateDirectoryA(name: *const u8, _sa: *const c_void) -> i32 {
    let Some(path) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    create_dir_impl(&path)
}

fn create_dir_impl(path: &str) -> i32 {
    let Some(v) = crate::vfs::get() else {
        return match std::fs::create_dir(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(3) as u32);
                0
            }
        };
    };
    use crate::vfs::Target;
    match v.classify(path) {
        Target::Pass => match std::fs::create_dir(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(3) as u32);
                0
            }
        },
        Target::Translated(p) => match std::fs::create_dir(&p) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(3) as u32);
                0
            }
        },
        Target::Game { rel } => match v.create_dir(&rel) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e);
                0
            }
        },
        #[cfg(not(windows))]
        Target::Reject(e) => {
            set_error(e);
            0
        }
    }
}

pub unsafe extern "win64" fn RemoveDirectoryW(name: *const u16) -> i32 {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    remove_dir_impl(&path)
}

pub unsafe extern "win64" fn RemoveDirectoryA(name: *const u8) -> i32 {
    let Some(path) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    remove_dir_impl(&path)
}

fn remove_dir_impl(path: &str) -> i32 {
    let Some(v) = crate::vfs::get() else {
        return match std::fs::remove_dir(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(3) as u32);
                0
            }
        };
    };
    use crate::vfs::Target;
    match v.classify(path) {
        Target::Pass => match std::fs::remove_dir(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(3) as u32);
                0
            }
        },
        Target::Translated(p) => match std::fs::remove_dir(&p) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e.raw_os_error().unwrap_or(3) as u32);
                0
            }
        },
        Target::Game { rel } => match v.remove_dir(&rel) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(e);
                0
            }
        },
        #[cfg(not(windows))]
        Target::Reject(e) => {
            set_error(e);
            0
        }
    }
}

fn move_impl(src: &str, dst: &str, replace: bool) -> Result<(), u32> {
    use crate::vfs::{Target, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND};
    let Some(v) = crate::vfs::get() else {
        if !replace && std::path::Path::new(dst).exists() {
            return Err(ERROR_ALREADY_EXISTS);
        }
        return std::fs::rename(src, dst).or_else(|e| {
            if e.kind() == std::io::ErrorKind::CrossesDevices {
                std::fs::copy(src, dst)
                    .map(|_| ())
                    .and_then(|_| std::fs::remove_file(src))
                    .map_err(|e| e.raw_os_error().unwrap_or(5) as u32)
            } else {
                Err(e.raw_os_error().unwrap_or(2) as u32)
            }
        });
    };
    let s_target = v.classify(src);
    let d_target = v.classify(dst);
    #[cfg(not(windows))]
    {
        if let Target::Reject(e) = &s_target {
            return Err(*e);
        }
        if let Target::Reject(e) = &d_target {
            return Err(*e);
        }
    }
    let s_game = matches!(s_target, Target::Game { .. });
    let d_game = matches!(d_target, Target::Game { .. });
    if !s_game && !d_game {
        // Both host-side: resolve translated paths when present.
        let rs = match &s_target {
            Target::Translated(p) => p.clone(),
            _ => src.to_string(),
        };
        let rd = match &d_target {
            Target::Translated(p) => p.clone(),
            _ => dst.to_string(),
        };
        if let Some(parent) = std::path::Path::new(&rd).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if !replace && std::path::Path::new(&rd).exists() {
            return Err(ERROR_ALREADY_EXISTS);
        }
        return std::fs::rename(&rs, &rd).or_else(|e| {
            if e.kind() == std::io::ErrorKind::CrossesDevices {
                std::fs::copy(&rs, &rd)
                    .map(|_| ())
                    .and_then(|_| std::fs::remove_file(&rs))
                    .map_err(|e| e.raw_os_error().unwrap_or(5) as u32)
            } else {
                Err(e.raw_os_error().unwrap_or(2) as u32)
            }
        });
    }
    // At least one side is a game path: resolve through the VFS with overlay staging.
    let (rs, srel) = match &s_target {
        Target::Pass => (src.to_string(), None),
        Target::Translated(p) => (p.clone(), None),
        Target::Game { rel } => (
            v.read_path_rel(rel).ok_or(ERROR_FILE_NOT_FOUND)?,
            Some(rel.clone()),
        ),
        #[cfg(not(windows))]
        Target::Reject(e) => return Err(*e),
    };
    if !std::path::Path::new(&rs).exists() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    if std::path::Path::new(&rs).is_dir() {
        return Err(crate::vfs::ERROR_ACCESS_DENIED);
    }
    // Destination bookkeeping first (without publishing), then the host move.
    let (rd, drel) = match &d_target {
        Target::Pass => (dst.to_string(), None),
        Target::Translated(p) => {
            if let Some(parent) = std::path::Path::new(p).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            (p.clone(), None)
        }
        Target::Game { rel } => {
            if v.is_dir(rel) {
                return Err(crate::vfs::ERROR_ACCESS_DENIED);
            }
            if !replace && v.exists(rel) {
                return Err(ERROR_ALREADY_EXISTS);
            }
            // Case-only rename inside the game folder over the original: copy overlay.
            if let Some(sr) = &srel {
                if sr.to_lowercase() == rel.to_lowercase() && sr != rel {
                    let staged = v.stage_write(rel)?;
                    std::fs::copy(&rs, &staged).map_err(|_| crate::vfs::ERROR_ACCESS_DENIED)?;
                    v.commit_write(rel);
                    return Ok(());
                }
            }
            (v.stage_write(rel)?, Some(rel.clone()))
        }
        #[cfg(not(windows))]
        Target::Reject(e) => return Err(*e),
    };
    // Same-volume rename when the source already lives in the overlay.
    if let (Some(sr), Some(dr)) = (&srel, &drel) {
        let in_overlay = rs == v.overlay_path(sr);
        if in_overlay {
            if std::fs::rename(&rs, &rd).is_ok() {
                v.commit_write(dr);
                let _ = v.delete_file(sr);
                return Ok(());
            }
        }
    }
    // General path: copy the bytes, publish the destination, hide the source.
    std::fs::copy(&rs, &rd).map_err(|_| crate::vfs::ERROR_ACCESS_DENIED)?;
    if let Some(dr) = &drel {
        v.commit_write(dr);
    }
    if let Some(sr) = &srel {
        let _ = v.delete_file(sr);
    } else if matches!(s_target, Target::Translated(_)) {
        let _ = std::fs::remove_file(&rs);
    }
    Ok(())
}

pub unsafe extern "win64" fn MoveFileExW(src: *const u16, dst: *const u16, flags: u32) -> i32 {
    let (Some(s), Some(d)) = (wide_str(src), wide_str(dst)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match move_impl(&s, &d, flags & MOVEFILE_REPLACE_EXISTING != 0) {
        Ok(()) => {
            set_error(ERROR_SUCCESS);
            1
        }
        Err(e) => {
            set_error(e);
            0
        }
    }
}

pub unsafe extern "win64" fn MoveFileWithProgressW(
    src: *const u16,
    dst: *const u16,
    _prog: *const c_void,
    _data: *const c_void,
    flags: u32,
) -> i32 {
    MoveFileExW(src, dst, flags)
}

pub unsafe extern "win64" fn MoveFileW(src: *const u16, dst: *const u16) -> i32 {
    MoveFileExW(src, dst, MOVEFILE_COPY_ALLOWED)
}

pub unsafe extern "win64" fn MoveFileExA(src: *const u8, dst: *const u8, flags: u32) -> i32 {
    let (Some(s), Some(d)) = (ansi_str(src), ansi_str(dst)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match move_impl(&s, &d, flags & MOVEFILE_REPLACE_EXISTING != 0) {
        Ok(()) => {
            set_error(ERROR_SUCCESS);
            1
        }
        Err(e) => {
            set_error(e);
            0
        }
    }
}

pub unsafe extern "win64" fn MoveFileWithProgressA(
    src: *const u8,
    dst: *const u8,
    _prog: *const c_void,
    _data: *const c_void,
    flags: u32,
) -> i32 {
    MoveFileExA(src, dst, flags)
}

fn copy_impl(src: &str, dst: &str, fail_if_exists: bool) -> Result<(), u32> {
    use crate::vfs::{Target, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND};
    let Some(v) = crate::vfs::get() else {
        if fail_if_exists && std::path::Path::new(dst).exists() {
            return Err(ERROR_FILE_EXISTS);
        }
        return std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| e.raw_os_error().unwrap_or(2) as u32);
    };
    let s_target = v.classify(src);
    let d_target = v.classify(dst);
    #[cfg(not(windows))]
    {
        if let Target::Reject(e) = &s_target {
            return Err(*e);
        }
        if let Target::Reject(e) = &d_target {
            return Err(*e);
        }
    }
    let (rs, _) = match &s_target {
        Target::Pass => (src.to_string(), None),
        Target::Translated(p) => (p.clone(), None),
        Target::Game { rel } => (
            v.read_path_rel(rel).ok_or(ERROR_FILE_NOT_FOUND)?,
            Some(rel.clone()),
        ),
        #[cfg(not(windows))]
        Target::Reject(e) => return Err(*e),
    };
    if !std::path::Path::new(&rs).exists() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    let (rd, drel) = match &d_target {
        Target::Pass => (dst.to_string(), None),
        Target::Translated(p) => {
            if let Some(parent) = std::path::Path::new(p).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if fail_if_exists && std::path::Path::new(p).exists() {
                return Err(ERROR_FILE_EXISTS);
            }
            (p.clone(), None)
        }
        Target::Game { rel } => {
            if v.is_dir(rel) {
                return Err(crate::vfs::ERROR_ACCESS_DENIED);
            }
            if fail_if_exists && v.exists(rel) {
                return Err(ERROR_FILE_EXISTS);
            }
            (v.stage_write(rel)?, Some(rel.clone()))
        }
        #[cfg(not(windows))]
        Target::Reject(e) => return Err(*e),
    };
    std::fs::copy(&rs, &rd).map_err(|_| crate::vfs::ERROR_ACCESS_DENIED)?;
    if let Some(dr) = drel {
        v.commit_write(&dr);
    }
    Ok(())
}

pub unsafe extern "win64" fn CopyFileW(src: *const u16, dst: *const u16, fail: i32) -> i32 {
    let (Some(s), Some(d)) = (wide_str(src), wide_str(dst)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match copy_impl(&s, &d, fail != 0) {
        Ok(()) => {
            set_error(ERROR_SUCCESS);
            1
        }
        Err(e) => {
            set_error(e);
            0
        }
    }
}

pub unsafe extern "win64" fn CopyFileExW(
    src: *const u16,
    dst: *const u16,
    _prog: *const c_void,
    _data: *const c_void,
    _cancel: *mut i32,
    flags: u32,
) -> i32 {
    let (Some(s), Some(d)) = (wide_str(src), wide_str(dst)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match copy_impl(&s, &d, flags & COPY_FILE_FAIL_IF_EXISTS != 0) {
        Ok(()) => {
            set_error(ERROR_SUCCESS);
            1
        }
        Err(e) => {
            set_error(e);
            0
        }
    }
}

pub unsafe extern "win64" fn FindFirstFileExW(
    name: *const u16,
    level: i32,
    data: *mut c_void,
    _op: i32,
    _filter: *const c_void,
    _flags: u32,
) -> usize {
    if level > 1 {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    }
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    };
    match find_first_impl(&path, data as *mut FindDataW) {
        Ok(h) => {
            set_error(ERROR_SUCCESS);
            h
        }
        Err(e) => {
            set_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "win64" fn FindFirstFileW(name: *const u16, data: *mut FindDataW) -> usize {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    };
    match find_first_impl(&path, data) {
        Ok(h) => {
            set_error(ERROR_SUCCESS);
            h
        }
        Err(e) => {
            set_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "win64" fn FindFirstFileA(name: *const u8, data: *mut FindDataW) -> usize {
    // ANSI callers read the wide layout through the same struct; the name bytes are
    // UTF-8 on Linux, converted here, and the wide name is filled as usual.
    let Some(path) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    };
    // Reuse the wide path: encode back is unnecessary, call the impl directly.
    let mut w = std::mem::zeroed::<FindDataW>();
    match find_first_impl(&path, &mut w) {
        Ok(h) => {
            if !data.is_null() {
                *data = w;
            }
            set_error(ERROR_SUCCESS);
            h
        }
        Err(e) => {
            set_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub unsafe extern "win64" fn FindNextFileW(handle: usize, data: *mut FindDataW) -> i32 {
    if data.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut g = finds().lock().unwrap();
    let Some(st) = g.get_mut(&handle) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match st.items.get(st.pos).copied() {
        Some(d) => {
            unsafe { *data = d };
            st.pos += 1;
            set_error(ERROR_SUCCESS);
            1
        }
        None => {
            set_error(18); // ERROR_NO_MORE_FILES
            0
        }
    }
}

pub unsafe extern "win64" fn FindNextFileA(handle: usize, data: *mut FindDataW) -> i32 {
    FindNextFileW(handle, data)
}

pub unsafe extern "win64" fn FindClose(handle: usize) -> i32 {
    if close_find_handle(handle) {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

fn attrs_for(path: &str) -> Result<u32, u32> {
    let Some(v) = crate::vfs::get() else {
        let m = std::fs::metadata(path).map_err(|e| e.raw_os_error().unwrap_or(2) as u32)?;
        return Ok(if m.is_dir() {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        });
    };
    use crate::vfs::Target;
    match v.classify(path) {
        Target::Pass => {
            let m = std::fs::metadata(path).map_err(|e| e.raw_os_error().unwrap_or(2) as u32)?;
            Ok(if m.is_dir() {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            })
        }
        Target::Translated(p) => {
            let m = std::fs::metadata(&p).map_err(|e| e.raw_os_error().unwrap_or(2) as u32)?;
            Ok(if m.is_dir() {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            })
        }
        Target::Game { rel } => {
            if v.is_whiteout(&rel) && !v.visible_in_overlay(&rel) {
                return Err(crate::vfs::ERROR_FILE_NOT_FOUND);
            }
            if v.visible_in_overlay(&rel) || v.visible_in_original(&rel) {
                if v.is_dir(&rel) {
                    return Ok(FILE_ATTRIBUTE_DIRECTORY);
                }
                return Ok(FILE_ATTRIBUTE_ARCHIVE);
            }
            Err(crate::vfs::ERROR_FILE_NOT_FOUND)
        }
        #[cfg(not(windows))]
        Target::Reject(e) => Err(e),
    }
}

pub unsafe extern "win64" fn GetFileAttributesW(name: *const u16) -> u32 {
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_FILE_ATTRIBUTES;
    };
    match attrs_for(&path) {
        Ok(a) => {
            set_error(ERROR_SUCCESS);
            a
        }
        Err(e) => {
            set_error(e);
            INVALID_FILE_ATTRIBUTES
        }
    }
}

pub unsafe extern "win64" fn GetFileAttributesA(name: *const u8) -> u32 {
    let Some(path) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_FILE_ATTRIBUTES;
    };
    match attrs_for(&path) {
        Ok(a) => {
            set_error(ERROR_SUCCESS);
            a
        }
        Err(e) => {
            set_error(e);
            INVALID_FILE_ATTRIBUTES
        }
    }
}

/// `WIN32_FILE_ATTRIBUTE_DATA` (36 bytes): attributes + 3 FILETIMEs + size high/low.
pub unsafe extern "win64" fn GetFileAttributesExW(name: *const u16, level: i32, out: *mut u8) -> i32 {
    if level != 0 || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(path) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let attrs = match attrs_for(&path) {
        Ok(a) => a,
        Err(e) => {
            set_error(e);
            return 0;
        }
    };
    // Resolve the host path for times/sizes.
    let host: Option<String> = (|| {
        let v = crate::vfs::get()?;
        use crate::vfs::Target;
        match v.classify(&path) {
            Target::Pass => Some(path.clone()),
            Target::Translated(p) => Some(p),
            Target::Game { rel } => v.read_path_rel(&rel),
            #[cfg(not(windows))]
            Target::Reject(_) => None,
        }
    })();
    let host = host.unwrap_or(path);
    let meta = std::fs::metadata(&host).ok();
    let (c_l, c_h) = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .map(filetime_from_system)
        .unwrap_or((0, 0));
    let (a_l, a_h) = meta
        .as_ref()
        .and_then(|m| m.accessed().ok())
        .map(filetime_from_system)
        .unwrap_or((c_l, c_h));
    let (w_l, w_h) = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(filetime_from_system)
        .unwrap_or((c_l, c_h));
    let len = meta.map(|m| m.len()).unwrap_or(0);
    unsafe {
        (out as *mut u32).write_unaligned(attrs);
        (out.add(4) as *mut u32).write_unaligned(c_l);
        (out.add(8) as *mut u32).write_unaligned(c_h);
        (out.add(12) as *mut u32).write_unaligned(a_l);
        (out.add(16) as *mut u32).write_unaligned(a_h);
        (out.add(20) as *mut u32).write_unaligned(w_l);
        (out.add(24) as *mut u32).write_unaligned(w_h);
        (out.add(28) as *mut u32).write_unaligned((len >> 32) as u32);
        (out.add(32) as *mut u32).write_unaligned(len as u32);
    }
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn GetDiskFreeSpaceExW(
    dir: *const u16,
    avail: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let Some(path) = wide_str(dir) else {
        // NULL means the current directory's volume: report the host cwd volume.
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string());
        return disk_free_answer(&cwd, avail, total, free);
    };
    disk_free_answer(&path, avail, total, free)
}

pub unsafe extern "win64" fn GetDiskFreeSpaceExA(
    dir: *const u8,
    avail: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let Some(path) = ansi_str(dir) else {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string());
        return disk_free_answer(&cwd, avail, total, free);
    };
    disk_free_answer(&path, avail, total, free)
}

fn disk_free_answer(path: &str, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32 {
    match disk_space_for(path) {
        Some((a, t)) => {
            unsafe {
                if !avail.is_null() {
                    *avail = a;
                }
                if !total.is_null() {
                    *total = t;
                }
                if !free.is_null() {
                    *free = a;
                }
            }
            set_error(ERROR_SUCCESS);
            1
        }
        None => {
            set_error(crate::vfs::ERROR_PATH_NOT_FOUND);
            0
        }
    }
}

pub unsafe extern "win64" fn GetDiskFreeSpaceW(
    root: *const u16,
    spc: *mut u32,
    bps: *mut u32,
    free_cl: *mut u32,
    total_cl: *mut u32,
) -> i32 {
    let Some(path) = wide_str(root) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    disk_clusters_answer(&path, spc, bps, free_cl, total_cl)
}

pub unsafe extern "win64" fn GetDiskFreeSpaceA(
    root: *const u8,
    spc: *mut u32,
    bps: *mut u32,
    free_cl: *mut u32,
    total_cl: *mut u32,
) -> i32 {
    let Some(path) = ansi_str(root) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    disk_clusters_answer(&path, spc, bps, free_cl, total_cl)
}

fn disk_clusters_answer(
    path: &str,
    spc: *mut u32,
    bps: *mut u32,
    free_cl: *mut u32,
    total_cl: *mut u32,
) -> i32 {
    match disk_space_for(path) {
        Some((avail, total)) => {
            // Fixed 4 KiB clusters: titles only check relative magnitudes.
            const CLUSTER: u64 = 4096;
            unsafe {
                if !spc.is_null() {
                    *spc = 1;
                }
                if !bps.is_null() {
                    *bps = CLUSTER as u32;
                }
                if !free_cl.is_null() {
                    *free_cl = (avail / CLUSTER).min(u32::MAX as u64) as u32;
                }
                if !total_cl.is_null() {
                    *total_cl = (total / CLUSTER).min(u32::MAX as u64) as u32;
                }
            }
            set_error(ERROR_SUCCESS);
            1
        }
        None => {
            set_error(crate::vfs::ERROR_PATH_NOT_FOUND);
            0
        }
    }
}

// ---------------------------------------------------------------------------------------
// Startup: command line, process heap, module file names (Phase C step 11b)
// ---------------------------------------------------------------------------------------
//
// mingw `mainCRTStartup` parses `GetCommandLineW` into `argc`/`argv` (`__getmainargs`),
// allocating the vector with `HeapAlloc(GetProcessHeap(), ...)`. The auto-stub returned
// null for the command line, so `__getmainargs` built a garbage `argv` and the CRT
// `strlen` faulted on it. These shims hand out a real command line (the mapped exe plus
// the game args the launcher saw), a libc-backed process heap, and the mapped image path
// for `GetModuleFileNameW`, all with `extern "win64"` (DEC-013).

/// Quote one command-line argument with the same rules as the Windows launcher (`quote`
/// in `launcher.rs`): bare when it has no spaces/tabs/quotes, otherwise `"`-quoted with
/// backslash/doubled-quote escaping.
fn quote_cmd_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut s = String::from("\"");
    let mut bs = 0;
    for c in a.chars() {
        match c {
            '\\' => bs += 1,
            '"' => {
                for _ in 0..bs * 2 + 1 {
                    s.push('\\');
                }
                s.push('"');
                bs = 0;
            }
            _ => {
                for _ in 0..bs {
                    s.push('\\');
                }
                bs = 0;
                s.push(c);
            }
        }
    }
    for _ in 0..bs * 2 {
        s.push('\\');
    }
    s.push('"');
    s
}

static GAME_ARGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn game_args_slot() -> &'static Mutex<Vec<String>> {
    GAME_ARGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Remember the game args the launcher saw; the command line is built from them plus
/// the mapped exe. Called once by `launcher::run` before `host::run_boot`.
pub fn set_game_args(args: &[String]) {
    *game_args_slot().lock().unwrap() = args.to_vec();
}

fn game_args() -> Vec<String> {
    game_args_slot().lock().unwrap().clone()
}

fn exe_for_cmdline() -> String {
    if let Some(m) = crate::modules::main_module() {
        return m.path.to_string_lossy().into_owned();
    }
    if let Some(b) = crate::state::boot() {
        return b.exe_path.to_string_lossy().into_owned();
    }
    "xcl".to_string()
}

fn current_command_line() -> String {
    let mut s = quote_cmd_arg(&exe_for_cmdline());
    for a in game_args() {
        s.push(' ');
        s.push_str(&quote_cmd_arg(&a));
    }
    s
}

fn immortal_wide(s: &str) -> *const u16 {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let p = libc::malloc(w.len() * 2) as *mut u16;
        assert!(!p.is_null(), "kernelx: out of memory");
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        p as *const u16
    }
}

fn immortal_ansi(s: &str) -> *const u8 {
    let b = s.as_bytes();
    unsafe {
        let p = libc::malloc(b.len() + 1) as *mut u8;
        assert!(!p.is_null(), "kernelx: out of memory");
        std::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
        *p.add(b.len()) = 0;
        p as *const u8
    }
}

static CMDLINE_W: OnceLock<Mutex<Option<(String, usize)>>> = OnceLock::new();
static CMDLINE_A: OnceLock<Mutex<Option<(String, usize)>>> = OnceLock::new();

/// `LPWSTR GetCommandLineW(void)` — the mapped exe plus game args, quoted. The pointer
/// is process-lifetime; the same source string keeps the same pointer.
pub unsafe extern "win64" fn GetCommandLineW() -> *const u16 {
    let src = current_command_line();
    let mut g = CMDLINE_W.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((cached, ptr)) = g.as_ref() {
        if *cached == src {
            return *ptr as *const u16;
        }
    }
    let p = immortal_wide(&src);
    *g = Some((src, p as usize));
    p
}

/// `LPSTR GetCommandLineA(void)` — ANSI (UTF-8 on Linux) form of the same command line.
pub unsafe extern "win64" fn GetCommandLineA() -> *const u8 {
    let src = current_command_line();
    let mut g = CMDLINE_A.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((cached, ptr)) = g.as_ref() {
        if *cached == src {
            return *ptr as *const u8;
        }
    }
    let p = immortal_ansi(&src);
    *g = Some((src, p as usize));
    p
}

// Process heap: a single libc-backed heap; private heaps alias to it.

/// Pseudo-handle returned by `GetProcessHeap` (`"XCLH"`; never collides with the
/// `0x1_0000`-based thread/event/file handles or the mmap-backed HLE handles).
pub const PROCESS_HEAP: usize = 0x5843_4C48;

const HEAP_ZERO_MEMORY: u32 = 0x8;

static HEAP_SIZES: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

fn heap_sizes() -> &'static Mutex<HashMap<usize, usize>> {
    HEAP_SIZES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `HANDLE GetProcessHeap(void)` — the process heap pseudo-handle.
pub unsafe extern "win64" fn GetProcessHeap() -> usize {
    PROCESS_HEAP
}

/// `HANDLE HeapCreate(DWORD, SIZE_T, SIZE_T)` — alias to the process heap (no private
/// heaps on Linux; every heap is the same libc heap).
pub unsafe extern "win64" fn HeapCreate(
    _options: u32,
    _init: usize,
    _max: usize,
) -> usize {
    set_error(ERROR_SUCCESS);
    PROCESS_HEAP
}

/// `BOOL HeapDestroy(HANDLE)` — success for the process heap, clean failure otherwise.
pub unsafe extern "win64" fn HeapDestroy(h: usize) -> i32 {
    if h == PROCESS_HEAP {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

/// `LPVOID HeapAlloc(HANDLE, DWORD, SIZE_T)` — `libc` memory; `0` bytes yields a
/// freeable minimum block; `HEAP_ZERO_MEMORY` zeroes. Null (with `ERROR_NOT_ENOUGH_MEMORY`
/// or `ERROR_INVALID_PARAMETER`) on failure, never a host abort.
pub unsafe extern "win64" fn HeapAlloc(h: usize, flags: u32, bytes: usize) -> *mut c_void {
    if h != PROCESS_HEAP {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let n = bytes.max(1);
    let p = if flags & HEAP_ZERO_MEMORY != 0 {
        libc::calloc(1, n) as *mut c_void
    } else {
        libc::malloc(n) as *mut c_void
    };
    if p.is_null() {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    heap_sizes().lock().unwrap().insert(p as usize, n);
    set_error(ERROR_SUCCESS);
    p
}

/// `LPVOID HeapReAlloc(HANDLE, DWORD, LPVOID, SIZE_T)` — `NULL` behaves as `HeapAlloc`;
/// `0` bytes frees the block and returns `NULL`. Grown tails are zeroed under
/// `HEAP_ZERO_MEMORY` (sizes are tracked for this).
pub unsafe extern "win64" fn HeapReAlloc(
    h: usize,
    flags: u32,
    p: *mut c_void,
    bytes: usize,
) -> *mut c_void {
    if h != PROCESS_HEAP {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    if p.is_null() {
        return HeapAlloc(h, flags, bytes);
    }
    if bytes == 0 {
        HeapFree(h, flags, p);
        return std::ptr::null_mut();
    }
    let old = heap_sizes().lock().unwrap().get(&(p as usize)).copied().unwrap_or(0);
    let q = libc::realloc(p as *mut libc::c_void, bytes) as *mut c_void;
    if q.is_null() {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    if q as usize != p as usize {
        let mut g = heap_sizes().lock().unwrap();
        g.remove(&(p as usize));
        g.insert(q as usize, bytes);
    } else {
        heap_sizes().lock().unwrap().insert(q as usize, bytes);
    }
    if flags & HEAP_ZERO_MEMORY != 0 && bytes > old {
        std::ptr::write_bytes((q as *mut u8).add(old), 0, bytes - old);
    }
    set_error(ERROR_SUCCESS);
    q
}

/// `BOOL HeapFree(HANDLE, DWORD, LPVOID)` — `NULL` is a no-op success (as for `free`);
/// unknown pointers fail cleanly instead of aborting the host in `libc::free`.
pub unsafe extern "win64" fn HeapFree(h: usize, _flags: u32, p: *mut c_void) -> i32 {
    if p.is_null() {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if h != PROCESS_HEAP {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if heap_sizes().lock().unwrap().remove(&(p as usize)).is_none() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    libc::free(p as *mut libc::c_void);
    set_error(ERROR_SUCCESS);
    1
}

/// `SIZE_T HeapSize(HANDLE, DWORD, LPCVOID)` — tracked size, or `SIZE_T(-1)` on failure.
pub unsafe extern "win64" fn HeapSize(h: usize, _flags: u32, p: *const c_void) -> usize {
    if h != PROCESS_HEAP || p.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return usize::MAX;
    }
    match heap_sizes().lock().unwrap().get(&(p as usize)).copied() {
        Some(n) => {
            set_error(ERROR_SUCCESS);
            n
        }
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            usize::MAX
        }
    }
}

/// `BOOL HeapValidate(HANDLE, DWORD, LPCVOID)` — `NULL` validates the whole heap
/// (always fine); otherwise the pointer must be a live heap block.
pub unsafe extern "win64" fn HeapValidate(h: usize, _flags: u32, p: *const c_void) -> i32 {
    if h != PROCESS_HEAP {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if p.is_null() {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if heap_sizes().lock().unwrap().contains_key(&(p as usize)) {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

fn module_path_for(h: usize) -> Option<String> {
    if h == 0 {
        if let Some(m) = crate::modules::main_module() {
            return Some(m.path.to_string_lossy().into_owned());
        }
        return crate::state::boot().map(|b| b.exe_path.to_string_lossy().into_owned());
    }
    if let Some(m) = crate::modules::main_module() {
        if m.base == h {
            return Some(m.path.to_string_lossy().into_owned());
        }
    }
    if let Some(m) = crate::modules::find_mapped_by_base(h) {
        return Some(m.path.to_string_lossy().into_owned());
    }
    crate::modules::hle_name_by_handle(h).map(|n| format!("C:\\Windows\\System32\\{n}"))
}

/// `DWORD GetModuleFileNameW(HMODULE, LPWSTR, DWORD)` — the mapped image path for XCL
/// images (or a `C:\Windows\System32\…` stand-in for HLE pseudo-handles); `0` for
/// unknown handles. Truncation reports `ERROR_INSUFFICIENT_BUFFER`, as on Windows.
pub unsafe extern "win64" fn GetModuleFileNameW(h: usize, buf: *mut u16, size: u32) -> u32 {
    let Some(p) = module_path_for(h) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if buf.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let n = crate::util::write_wide_out(&p, buf, size);
    if n == size {
        set_error(ERROR_INSUFFICIENT_BUFFER);
    } else {
        set_error(ERROR_SUCCESS);
    }
    n
}

/// `DWORD GetModuleFileNameA(HMODULE, LPSTR, DWORD)` — ANSI (UTF-8 on Linux) form.
pub unsafe extern "win64" fn GetModuleFileNameA(h: usize, buf: *mut u8, size: u32) -> u32 {
    let Some(p) = module_path_for(h) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if buf.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let n = crate::util::write_ansi_out(&p, buf, size);
    if n == size {
        set_error(ERROR_INSUFFICIENT_BUFFER);
    } else {
        set_error(ERROR_SUCCESS);
    }
    n
}

// ---------------------------------------------------------------------------------------
// Export table
// ---------------------------------------------------------------------------------------

/// Implemented exports with real code addresses. Everything else in `hle_linux` stays
/// `Stub` (address 0) until its slice lands.
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("RegOpenKeyW", RegOpenKeyW as usize),
        ("RegOpenKeyExW", RegOpenKeyExW as usize),
        ("RegQueryValueExW", RegQueryValueExW as usize),
        ("RegCloseKey", RegCloseKey as usize),
        ("VirtualAlloc", VirtualAlloc as usize),
        ("VirtualAllocEx", VirtualAllocEx as usize),
        ("VirtualFree", VirtualFree as usize),
        ("VirtualFreeEx", VirtualFreeEx as usize),
        ("VirtualProtect", VirtualProtect as usize),
        ("VirtualProtectEx", VirtualProtectEx as usize),
        ("XMemSetAllocationHooks", XMemSetAllocationHooks as usize),
        ("XMemVirtualQuery", XMemVirtualQuery as usize),
        ("XMemGetWorkingSetStatistics", XMemGetWorkingSetStatistics as usize),
        ("XMemTransferMemory", XMemTransferMemory as usize),
        ("TitleMemoryStatus", TitleMemoryStatus as usize),
        ("JobTitleMemoryStatus", JobTitleMemoryStatus as usize),
        ("ToolingMemoryStatus", ToolingMemoryStatus as usize),
        ("SetUnhandledExceptionFilter", SetUnhandledExceptionFilter as usize),
        ("SetThreadName", SetThreadName as usize),
        ("GetThreadName", GetThreadName as usize),
        ("SetThreadpoolAffinityMask", SetThreadpoolAffinityMask as usize),
        ("SetThreadAffinityMask", SetThreadAffinityMask as usize),
        ("SetThreadGroupAffinity", SetThreadGroupAffinity as usize),
        ("QueryProcessorSchedulingStatistics", QueryProcessorSchedulingStatistics as usize),
        ("GetConsoleType", GetConsoleType as usize),
        ("OutputDebugStringA", OutputDebugStringA as usize),
        ("OutputDebugStringW", OutputDebugStringW as usize),
        ("GetSystemOSVersion", GetSystemOSVersion as usize),
        ("RegisterAppStateChangeNotification", RegisterAppStateChangeNotification as usize),
        (
            "UnregisterAppStateChangeNotification",
            UnregisterAppStateChangeNotification as usize,
        ),
        (
            "RegisterAppConstrainedChangeNotification",
            RegisterAppConstrainedChangeNotification as usize,
        ),
        (
            "UnregisterAppConstrainedChangeNotification",
            UnregisterAppConstrainedChangeNotification as usize,
        ),
        // Kernel-family fallbacks routed to KERNELX (api-ms-win-core-*).
        ("CreateThread", CreateThread as usize),
        ("ResumeThread", ResumeThread as usize),
        ("SuspendThread", SuspendThread as usize),
        ("GetCurrentProcess", GetCurrentProcess as usize),
        ("GetCurrentThread", GetCurrentThread as usize),
        ("GetCurrentProcessId", GetCurrentProcessId as usize),
        ("GetCurrentThreadId", GetCurrentThreadId as usize),
        ("GetThreadId", GetThreadId as usize),
        ("GetThreadPriority", GetThreadPriority as usize),
        ("SetThreadPriority", SetThreadPriority as usize),
        ("GetLastError", GetLastError as usize),
        ("SetLastError", SetLastError as usize),
        ("Sleep", Sleep as usize),
        ("SleepEx", SleepEx as usize),
        ("GetTickCount", GetTickCount as usize),
        ("GetTickCount64", GetTickCount64 as usize),
        ("QueryPerformanceCounter", QueryPerformanceCounter as usize),
        ("QueryPerformanceFrequency", QueryPerformanceFrequency as usize),
        ("GetSystemTimeAsFileTime", GetSystemTimeAsFileTime as usize),
        ("CreateEventW", CreateEventW as usize),
        ("CreateEventA", CreateEventA as usize),
        ("SetEvent", SetEvent as usize),
        ("ResetEvent", ResetEvent as usize),
        ("CloseHandle", CloseHandle as usize),
        ("WaitForSingleObject", WaitForSingleObject as usize),
        ("WaitForMultipleObjects", WaitForMultipleObjects as usize),
        ("InitializeCriticalSection", InitializeCriticalSection as usize),
        (
            "InitializeCriticalSectionAndSpinCount",
            InitializeCriticalSectionAndSpinCount as usize,
        ),
        ("EnterCriticalSection", EnterCriticalSection as usize),
        ("TryEnterCriticalSection", TryEnterCriticalSection as usize),
        ("LeaveCriticalSection", LeaveCriticalSection as usize),
        ("DeleteCriticalSection", DeleteCriticalSection as usize),
        ("InitializeSRWLock", InitializeSRWLock as usize),
        ("AcquireSRWLockExclusive", AcquireSRWLockExclusive as usize),
        ("ReleaseSRWLockExclusive", ReleaseSRWLockExclusive as usize),
        ("TryAcquireSRWLockExclusive", TryAcquireSRWLockExclusive as usize),
        // Step-11b startup: command line, process heap, module file names.
        ("GetCommandLineW", GetCommandLineW as usize),
        ("GetCommandLineA", GetCommandLineA as usize),
        ("GetProcessHeap", GetProcessHeap as usize),
        ("HeapCreate", HeapCreate as usize),
        ("HeapDestroy", HeapDestroy as usize),
        ("HeapAlloc", HeapAlloc as usize),
        ("HeapReAlloc", HeapReAlloc as usize),
        ("HeapFree", HeapFree as usize),
        ("HeapSize", HeapSize as usize),
        ("HeapValidate", HeapValidate as usize),
        ("GetModuleFileNameW", GetModuleFileNameW as usize),
        ("GetModuleFileNameA", GetModuleFileNameA as usize),
        // Step-10c file APIs through the linked-in Linux VFS (kernel-family fallbacks).
        ("CreateFileW", CreateFileW as usize),
        ("CreateFileA", CreateFileA as usize),
        ("CreateFile2", CreateFile2 as usize),
        ("ReadFile", ReadFile as usize),
        ("WriteFile", WriteFile as usize),
        ("FlushFileBuffers", FlushFileBuffers as usize),
        ("GetFileSizeEx", GetFileSizeEx as usize),
        ("SetFilePointerEx", SetFilePointerEx as usize),
        ("SetEndOfFile", SetEndOfFile as usize),
        ("DeleteFileW", DeleteFileW as usize),
        ("DeleteFileA", DeleteFileA as usize),
        ("CreateDirectoryW", CreateDirectoryW as usize),
        ("CreateDirectoryA", CreateDirectoryA as usize),
        ("RemoveDirectoryW", RemoveDirectoryW as usize),
        ("RemoveDirectoryA", RemoveDirectoryA as usize),
        ("MoveFileExW", MoveFileExW as usize),
        ("MoveFileWithProgressW", MoveFileWithProgressW as usize),
        ("MoveFileW", MoveFileW as usize),
        ("MoveFileExA", MoveFileExA as usize),
        ("MoveFileWithProgressA", MoveFileWithProgressA as usize),
        ("CopyFileW", CopyFileW as usize),
        ("CopyFileExW", CopyFileExW as usize),
        ("FindFirstFileExW", FindFirstFileExW as usize),
        ("FindFirstFileW", FindFirstFileW as usize),
        ("FindFirstFileA", FindFirstFileA as usize),
        ("FindNextFileW", FindNextFileW as usize),
        ("FindNextFileA", FindNextFileA as usize),
        ("FindClose", FindClose as usize),
        ("GetFileAttributesW", GetFileAttributesW as usize),
        ("GetFileAttributesA", GetFileAttributesA as usize),
        ("GetFileAttributesExW", GetFileAttributesExW as usize),
        ("GetDiskFreeSpaceExW", GetDiskFreeSpaceExW as usize),
        ("GetDiskFreeSpaceExA", GetDiskFreeSpaceExA as usize),
        ("GetDiskFreeSpaceW", GetDiskFreeSpaceW as usize),
        ("GetDiskFreeSpaceA", GetDiskFreeSpaceA as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;

    /// Serializes the two command-line tests: they share the process-global
    /// `GAME_ARGS`, and the suite runs tests on multiple threads.
    static CMDLINE_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn perms_at(addr: usize) -> Option<String> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        for line in maps.lines() {
            let mut it = line.split_whitespace();
            let range = it.next()?;
            let perms = it.next()?.to_string();
            let (a, b) = range.split_once('-')?;
            let (a, b) = (
                usize::from_str_radix(a, 16).ok()?,
                usize::from_str_radix(b, 16).ok()?,
            );
            if addr >= a && addr < b {
                return Some(perms);
            }
        }
        None
    }

    #[test]
    fn affinity_masks_outside_the_process_are_fitted() {
        assert_eq!(fit_affinity_mask(0x3F, 0xFF), 0x3F);
        assert_eq!(fit_affinity_mask(0, 0xF), 0);
        assert_eq!(fit_affinity_mask(0x7F, 0xF), 0xF);
        assert_eq!(fit_affinity_mask(1 << 6, 0xF), 1 << 2);
        assert_eq!(fit_affinity_mask(0x3000, 0xF), 0x3);
        assert_eq!(fit_affinity_mask(1 << 20, 0b1010_1010), 0b10);
    }

    #[test]
    fn virtual_memory_commits_protects_and_releases() {
        unsafe {
            let page = page_size();
            let p = VirtualAlloc(
                std::ptr::null(),
                2 * page,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            );
            assert!(!p.is_null());
            std::ptr::write_bytes(p as *mut u8, 0xAB, 2 * page);
            assert_eq!(perms_at(p as usize).as_deref(), Some("rw-p"));
            let mut old = 0u32;
            assert_eq!(VirtualProtect(p, page, PAGE_READONLY, &mut old), 1);
            assert_eq!(old, PAGE_READWRITE);
            assert_eq!(perms_at(p as usize).as_deref(), Some("r--p"));
            assert_eq!(VirtualProtect(p, page, PAGE_READWRITE, &mut old), 1);
            assert_eq!(old, PAGE_READONLY);
            std::ptr::write_bytes(p as *mut u8, 0xCD, page);
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
            // A second release of the same base fails instead of unmapping something else.
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Zero-size and flagless requests fail cleanly.
            assert!(VirtualAlloc(std::ptr::null(), 0, MEM_COMMIT, PAGE_READWRITE).is_null());
            assert!(VirtualAlloc(std::ptr::null(), page, 0, PAGE_READWRITE).is_null());
        }
    }

    #[test]
    fn virtual_free_decommit_zeroes_but_keeps_the_reservation() {
        unsafe {
            let page = page_size();
            let p = VirtualAlloc(
                std::ptr::null(),
                page,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            );
            assert!(!p.is_null());
            *(p as *mut u32) = 0x1234_5678;
            assert_eq!(VirtualFree(p, page, MEM_DECOMMIT), 1);
            assert_eq!(*(p as *const u32), 0);
            *(p as *mut u32) = 0x9ABC_DEF0;
            assert_eq!(VirtualFree(p, 0, MEM_RELEASE), 1);
        }
    }

    #[test]
    fn fixed_address_mapping_rejects_an_occupied_base() {
        unsafe {
            let page = page_size();
            let holder = VirtualAlloc(
                std::ptr::null(),
                page,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            );
            assert!(!holder.is_null());
            let again = VirtualAlloc(holder, page, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
            assert!(again.is_null());
            assert_eq!(GetLastError(), ERROR_INVALID_ADDRESS);
            assert_eq!(VirtualFree(holder, 0, MEM_RELEASE), 1);
        }
    }

    #[test]
    fn create_thread_runs_through_tls_with_template_visible() {
        // A synthetic TLS image: the game thread must see its template through gs:[0x58].
        static TEMPLATE: [u8; 8] = [7, 0, 0, 0, 0, 0, 0, 0];
        static SEEN: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);
        unsafe extern "win64" fn probe(_param: *mut c_void) -> u32 {
            let slots: *mut *mut u8;
            core::arch::asm!(
                "mov {}, gs:[0x58]",
                out(reg) slots,
                options(nostack, readonly, preserves_flags)
            );
            // Slot 0 is the main image (installed by other tests or absent); find ours by
            // scanning for the template value is racy — instead read the index we stored.
            let index = PROBE_INDEX.load(Ordering::Acquire);
            let byte = *(*slots.add(index as usize));
            SEEN.store(byte as u32, Ordering::Release);
            byte as u32
        }
        static PROBE_INDEX: AtomicU32 = AtomicU32::new(u32::MAX);
        let index_ptr = Box::into_raw(Box::new(u32::MAX)) as usize;
        let index = unsafe {
            crate::tls::install(crate::tls::ImageSpec {
                name: "kernelx-thread-probe".into(),
                template: TEMPLATE.as_ptr() as usize,
                template_len: TEMPLATE.len(),
                total_len: TEMPLATE.len() as u64,
                index_ptr,
                hinst: 0,
                callbacks: Vec::new(),
                main: false,
            })
        }
        .expect("probe TLS image installs");
        PROBE_INDEX.store(index, Ordering::Release);
        crate::tls::attach_current_thread().expect("main attaches");
        let h = unsafe {
            CreateThread(
                std::ptr::null(),
                0,
                probe as usize,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(h, 0);
        assert_eq!(unsafe { WaitForSingleObject(h, 5000) }, WAIT_OBJECT_0);
        assert_eq!(SEEN.load(Ordering::Acquire), 7);
        assert_eq!(unsafe { CloseHandle(h) }, 1);
        crate::tls::detach_current_thread();
    }

    #[test]
    fn events_critical_sections_and_srw_guard_a_counter() {
        unsafe {
            // Manual-reset event: wait(0) before set times out, after set succeeds.
            let manual = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            assert_ne!(manual, 0);
            assert_eq!(WaitForSingleObject(manual, 0), WAIT_TIMEOUT);
            assert_eq!(SetEvent(manual), 1);
            assert_eq!(WaitForSingleObject(manual, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(manual, 0), WAIT_OBJECT_0);
            assert_eq!(ResetEvent(manual), 1);
            assert_eq!(WaitForSingleObject(manual, 0), WAIT_TIMEOUT);
            // Auto-reset: one waiter consumes the signal.
            let auto = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            assert_eq!(SetEvent(auto), 1);
            assert_eq!(WaitForSingleObject(auto, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(auto, 0), WAIT_TIMEOUT);
            // A setter thread wakes a 5 s wait.
            let wake = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            let setter = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                SetEvent(wake)
            });
            assert_eq!(WaitForSingleObject(wake, 5000), WAIT_OBJECT_0);
            setter.join().unwrap();
            // Critical section + SRW: 4 threads × 1k increments stay exact.
            let cs = Box::into_raw(Box::new(std::mem::zeroed::<CritSec>()));
            assert_eq!(InitializeCriticalSection(cs), 1);
            let srw = Box::into_raw(Box::new(0usize));
            InitializeSRWLock(srw);
            let counter = Arc::new(AtomicU32::new(0));
            let barrier = Arc::new(Barrier::new(4));
            let mut handles = Vec::new();
            let cs_addr = cs as usize;
            let srw_addr = srw as usize;
            for _ in 0..4 {
                let (c, b) = (counter.clone(), barrier.clone());
                handles.push(
                    crate::tls::spawn("kernelx-sync-worker", move || {
                        let cs = cs_addr as *mut CritSec;
                        let srw = srw_addr as *mut usize;
                        b.wait();
                        for _ in 0..1000 {
                            EnterCriticalSection(cs);
                            c.fetch_add(1, Ordering::Relaxed);
                            LeaveCriticalSection(cs);
                        }
                        AcquireSRWLockExclusive(srw);
                        c.fetch_add(1, Ordering::Relaxed);
                        ReleaseSRWLockExclusive(srw);
                    })
                    .unwrap(),
                );
            }
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(counter.load(Ordering::Relaxed), 4 * 1001);
            assert_eq!(DeleteCriticalSection(cs), 1);
            let _ = Box::from_raw(cs);
            let _ = Box::from_raw(srw);
            assert_eq!(CloseHandle(manual), 1);
            assert_eq!(CloseHandle(auto), 1);
            assert_eq!(CloseHandle(wake), 1);
            // Unknown handles fail instead of blocking forever.
            assert_eq!(WaitForSingleObject(0xDEAD_BEEF, 0), WAIT_FAILED);
        }
    }

    #[test]
    fn monotonic_time_and_sleep_behave() {
        unsafe {
            let mut freq = 0u64;
            assert_eq!(QueryPerformanceFrequency(&mut freq), 1);
            assert_eq!(freq, QPC_FREQUENCY);
            let mut a = 0u64;
            let mut b = 0u64;
            assert_eq!(QueryPerformanceCounter(&mut a), 1);
            Sleep(5);
            assert_eq!(QueryPerformanceCounter(&mut b), 1);
            assert!(b > a, "QPC must advance across Sleep(5)");
            let t0 = GetTickCount64();
            Sleep(5);
            let t1 = GetTickCount64();
            assert!(t1 >= t0);
            assert!(t1 - t0 < 1000, "Sleep(5) took {} ms", t1 - t0);
            let mut ft = FileTime { low: 0, high: 0 };
            GetSystemTimeAsFileTime(&mut ft);
            assert!(ft.high != 0 || ft.low != 0);
        }
    }

    #[test]
    fn thread_priority_and_affinity_degrade_gracefully() {
        unsafe {
            // A time-critical request without CAP_SYS_NICE must not fail the title.
            assert_eq!(SetThreadPriority(current_thread_handle(), 15), 1);
            // Stored per real handle and returned back.
            let h = CreateThread(
                std::ptr::null(),
                0,
                thread_exit_zero as usize,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_ne!(h, 0);
            assert_eq!(SetThreadPriority(h, 2), 1);
            assert_eq!(GetThreadPriority(h), 2);
            // Console-wide masks are fitted to the host instead of failing.
            let allowed = process_affinity();
            if allowed != 0 && allowed != usize::MAX {
                let fit = SetThreadAffinityMask(current_thread_handle(), usize::MAX);
                assert_ne!(fit, 0);
            }
            assert_eq!(WaitForSingleObject(h, 5000), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(h), 1);
            // Unknown handles fail cleanly.
            assert_eq!(SetThreadPriority(0xDEAD_BEEF, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    unsafe extern "win64" fn thread_exit_zero(_param: *mut c_void) -> u32 {
        0
    }

    #[test]
    fn console_registry_and_status_paths() {
        unsafe {
            let hklm = 0x8000_0002usize;
            let path = crate::util::wide("Software\\Microsoft\\Windows NT\\CurrentVersion\\OEM");
            let mut k = 0usize;
            assert_eq!(RegOpenKeyW(hklm, path.as_ptr(), &mut k), 0);
            let name = crate::util::wide("ConsoleMode");
            let (mut ty, mut v, mut n) = (0u32, 0u32, 4u32);
            assert_eq!(
                RegQueryValueExW(
                    k,
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    &mut ty,
                    &mut v as *mut u32 as *mut u8,
                    &mut n
                ),
                0
            );
            assert_eq!((ty, n), (4, 4));
            assert!(v == 0x64 || v == 0x60);
            assert_eq!(RegCloseKey(k), 0);
            // The registry reports the console mode (0x64 X / 0x60 S); GetConsoleType
            // reports the configured raw type (default 6), as on Windows.
            assert_eq!(GetConsoleType(), crate::state::config().console_type_raw);
            // Memory budget + debug output never fail the caller.
            let mut status = TitleMemoryStatusT {
                length: std::mem::size_of::<TitleMemoryStatusT>() as u32,
                reserved: 0,
                total: 0,
                avail: 0,
                legacy_used: 0,
                legacy_peak: 0,
                legacy_avail: 0,
                title_used: 0,
                title_avail: 0,
                legacy_page_table_used: 0,
                title_page_table_used: 0,
            };
            assert_eq!(TitleMemoryStatus(&mut status), 1);
            assert!(status.total > 0 && status.title_used <= status.total);
            let mut stats = [0u8; 16];
            assert_eq!(XMemGetWorkingSetStatistics(0x10000001, stats.as_mut_ptr()), 1);
            OutputDebugStringA(c"kernelx-linux-probe".as_ptr() as *const u8);
            let mut out: [u64; 3] = [0; 3];
            QueryProcessorSchedulingStatistics(&mut out);
            assert!(out[2] >= out[0] && out[2] >= out[1]);
        }
    }

    fn test_vfs_root(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("xcl_kernelx_files_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let game = dir.join("game");
        let data = dir.join("xcldata");
        std::fs::create_dir_all(game.join("data")).unwrap();
        std::fs::write(game.join("data").join("hello.txt"), b"original").unwrap();
        std::fs::write(game.join("data").join("gone.txt"), b"bye").unwrap();
        (game, data)
    }

    fn ensure_test_vfs() -> (std::path::PathBuf, std::path::PathBuf) {
        use crate::vfs::Vfs;
        if crate::vfs::get().is_some() {
            // Already initialised by this test (single comprehensive test below).
            // Return placeholder paths; the test uses its own captured roots.
            return (std::path::PathBuf::new(), std::path::PathBuf::new());
        }
        let (game, data) = test_vfs_root("global");
        let mounts = vec![
            ("G:".to_string(), game.to_string_lossy().to_string()),
            ("GAME:".to_string(), game.to_string_lossy().to_string()),
            ("D:".to_string(), game.to_string_lossy().to_string()),
            (
                "T:".to_string(),
                data.join("temp").to_string_lossy().to_string(),
            ),
            (
                "S:".to_string(),
                data.join("pls").to_string_lossy().to_string(),
            ),
        ];
        let v = Vfs::new(&game, &data, mounts);
        crate::vfs::init(v);
        (game, data)
    }

    fn tree_hash(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = entry.path();
                let ft = entry.file_type().unwrap();
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if ft.is_dir() {
                    stack.push(p);
                } else if ft.is_symlink() {
                    out.insert(
                        rel,
                        std::fs::read_link(&p)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned()
                            .into_bytes(),
                    );
                } else {
                    out.insert(rel, std::fs::read(&p).unwrap());
                }
            }
        }
        out
    }

    unsafe fn open_w(path: &str, access: u32, disp: u32) -> usize {
        let w = crate::util::wide(path);
        let h = CreateFileW(
            w.as_ptr(),
            access,
            0,
            std::ptr::null(),
            disp,
            0,
            0,
        );
        assert_ne!(h, INVALID_HANDLE_VALUE, "open {path} failed err {}", GetLastError());
        h
    }

    unsafe fn read_all(h: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let mut done = 0u32;
            let r = ReadFile(h, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut done, std::ptr::null_mut());
            assert_eq!(r, 1);
            if done == 0 {
                break;
            }
            out.extend_from_slice(&buf[..done as usize]);
            if done < buf.len() as u32 {
                break;
            }
        }
        out
    }

    #[test]
    fn kernelx_files_go_through_vfs_overlay() {
        use crate::vfs::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
        let (game, data) = test_vfs_root("files");
        let mounts = vec![
            ("G:".to_string(), game.to_string_lossy().to_string()),
            ("GAME:".to_string(), game.to_string_lossy().to_string()),
            ("D:".to_string(), game.to_string_lossy().to_string()),
            (
                "T:".to_string(),
                data.join("temp").to_string_lossy().to_string(),
            ),
            (
                "S:".to_string(),
                data.join("pls").to_string_lossy().to_string(),
            ),
        ];
        // Isolated VFS for direct API checks (global VFS is exercised through the
        // exported CreateFileW path after `ensure_test_vfs` below).
        let v = crate::vfs::Vfs::new(&game, &data, mounts.clone());
        let before = tree_hash(&game);
        // Direct VFS read sees the game layer.
        let p = v.read_path_rel("data\\hello.txt").expect("game read path");
        assert_eq!(std::fs::read(&p).unwrap(), b"original");

        // Global path: the exported shims must use the same VFS. Initialise once per
        // process; a second test run in the same process reuses it.
        let _ = ensure_test_vfs();
        let gv = crate::vfs::get().expect("global VFS initialised");
        // Seed the global game root with the same fixture content when this is the
        // first initialisation in the process.
        let global_game = std::path::Path::new(gv.game_root());
        if !global_game.join("data").join("hello.txt").exists() {
            std::fs::create_dir_all(global_game.join("data")).ok();
            std::fs::write(global_game.join("data").join("hello.txt"), b"original").ok();
            std::fs::write(global_game.join("data").join("gone.txt"), b"bye").ok();
        }
        let global_before = tree_hash(global_game);

        unsafe {
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const OPEN_EXISTING: u32 = 3;
            const CREATE_ALWAYS: u32 = 2;
            // Read through the shim.
            let h = open_w("G:\\data\\hello.txt", GENERIC_READ, OPEN_EXISTING);
            assert_eq!(read_all(h), b"original");
            assert_eq!(CloseHandle(h), 1);

            // Overlay write is visible; game folder unchanged.
            let h = open_w("G:\\data\\hello.txt", GENERIC_WRITE, OPEN_EXISTING);
            let mut done = 0u32;
            let bytes = b"modified";
            assert_eq!(
                WriteFile(
                    h,
                    bytes.as_ptr() as *const c_void,
                    bytes.len() as u32,
                    &mut done,
                    std::ptr::null_mut()
                ),
                1
            );
            assert_eq!(done as usize, bytes.len());
            assert_eq!(CloseHandle(h), 1);
            let h = open_w("G:\\data\\hello.txt", GENERIC_READ, OPEN_EXISTING);
            assert_eq!(read_all(h), b"modified");
            assert_eq!(CloseHandle(h), 1);

            // New file + rename + delete hide without touching the original.
            let h = open_w("G:\\data\\new_save.bin", GENERIC_WRITE, CREATE_ALWAYS);
            let mut done = 0u32;
            let payload = b"save-data";
            assert_eq!(
                WriteFile(
                    h,
                    payload.as_ptr() as *const c_void,
                    payload.len() as u32,
                    &mut done,
                    std::ptr::null_mut()
                ),
                1
            );
            assert_eq!(CloseHandle(h), 1);
            let s = crate::util::wide("G:\\data\\new_save.bin");
            let d = crate::util::wide("G:\\data\\renamed.bin");
            assert_eq!(MoveFileExW(s.as_ptr(), d.as_ptr(), 0), 1);
            let h = open_w("G:\\data\\renamed.bin", GENERIC_READ, OPEN_EXISTING);
            assert_eq!(read_all(h), b"save-data");
            assert_eq!(CloseHandle(h), 1);

            let del = crate::util::wide("G:\\data\\gone.txt");
            assert_eq!(DeleteFileW(del.as_ptr()), 1);
            let gone = crate::util::wide("G:\\data\\gone.txt");
            assert_eq!(
                CreateFileW(
                    gone.as_ptr(),
                    GENERIC_READ,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);

            // Adversarial paths never open host files.
            for bad in ["Z:\\Windows\\notepad.exe", "G:\\..\\outside.txt"] {
                let w = crate::util::wide(bad);
                let h = CreateFileW(
                    w.as_ptr(),
                    GENERIC_READ,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    0,
                );
                assert_eq!(h, INVALID_HANDLE_VALUE, "{bad}");
                let e = GetLastError();
                assert!(
                    e == ERROR_PATH_NOT_FOUND || e == ERROR_ACCESS_DENIED,
                    "{bad} err {e}"
                );
            }

            // T: lands under title data, free space reports the host volume.
            let h = open_w("T:\\scratch.bin", GENERIC_WRITE, CREATE_ALWAYS);
            let mut done = 0u32;
            assert_eq!(
                WriteFile(
                    h,
                    b"tmp".as_ptr() as *const c_void,
                    3,
                    &mut done,
                    std::ptr::null_mut()
                ),
                1
            );
            assert_eq!(CloseHandle(h), 1);
            let t = crate::util::wide("T:\\");
            let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
            assert_eq!(
                GetDiskFreeSpaceExW(t.as_ptr(), &mut avail, &mut total, &mut free),
                1
            );
            assert!(total > 0 && avail <= total);

            // Directory listing merges overlay and hides the whiteout.
            let pat = crate::util::wide("G:\\data\\*");
            let mut data: FindDataW = std::mem::zeroed();
            let fh = FindFirstFileW(pat.as_ptr(), &mut data);
            assert_ne!(fh, INVALID_HANDLE_VALUE);
            let mut names = Vec::new();
            let wname = |d: &FindDataW| {
                let n = d.name.iter().position(|&c| c == 0).unwrap_or(d.name.len());
                String::from_utf16_lossy(&d.name[..n])
            };
            names.push(wname(&data));
            loop {
                let mut next: FindDataW = std::mem::zeroed();
                if FindNextFileW(fh, &mut next) == 0 {
                    break;
                }
                names.push(wname(&next));
            }
            assert_eq!(FindClose(fh), 1);
            for want in ["hello.txt", "renamed.bin"] {
                assert!(names.iter().any(|n| n == want), "missing {want} in {names:?}");
            }
            assert!(!names.iter().any(|n| n == "gone.txt"), "stale {names:?}");
        }

        // Recursive game-tree hash unchanged on the isolated fixture.
        assert_eq!(tree_hash(&game)["data/hello.txt"], b"original");
        assert_eq!(tree_hash(&game), before);
        // Global game root: only overlay writes happened, originals intact.
        assert_eq!(
            std::fs::read(global_game.join("data").join("hello.txt")).unwrap(),
            b"original"
        );
        assert_eq!(tree_hash(global_game), global_before);
        let _ = data;
    }

    #[test]
    fn command_line_is_a_parseable_wide_string() {
        let _serial = CMDLINE_TEST_SERIAL.lock().unwrap();
        let prev = game_args();
        set_game_args(&[]);
        unsafe {
            let w = GetCommandLineW();
            assert!(!w.is_null());
            let n = crate::util::wcslen(w);
            assert!(n > 0, "command line must not be empty");
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(w, n));
            // The exe (or the "xcl" fallback when no image is mapped) is always argv[0].
            assert!(!s.is_empty());
            let a = GetCommandLineA();
            assert!(!a.is_null());
            let an = crate::util::wcslen(w);
            assert_eq!(an, n);
            let sa = crate::util::from_cstr_ptr(a).expect("ansi command line");
            assert_eq!(sa, s, "W and A forms must agree");
            // Same source keeps the same pointer (Windows stability).
            assert_eq!(GetCommandLineW(), w);
            assert_eq!(GetCommandLineA(), a);
        }
        set_game_args(&prev);
    }

    #[test]
    fn command_line_quoting_matches_the_launcher() {
        assert_eq!(quote_cmd_arg("game.exe"), "game.exe");
        assert_eq!(quote_cmd_arg(""), "\"\"");
        assert_eq!(quote_cmd_arg("a b"), "\"a b\"");
        assert_eq!(quote_cmd_arg("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn command_line_carries_the_game_args() {
        let _serial = CMDLINE_TEST_SERIAL.lock().unwrap();
        let prev = game_args();
        set_game_args(&["--level".to_string(), "a b".to_string()]);
        unsafe {
            let w = GetCommandLineW();
            let n = crate::util::wcslen(w);
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(w, n));
            assert!(s.contains("--level"), "{s}");
            assert!(s.contains("\"a b\""), "{s}");
        }
        set_game_args(&prev);
    }

    #[test]
    fn process_heap_allocs_frees_and_resizes() {
        unsafe {
            assert_eq!(GetProcessHeap(), PROCESS_HEAP);
            assert_ne!(HeapCreate(0, 0, 0), 0);
            // Zero bytes still yields a freeable block.
            let p = HeapAlloc(GetProcessHeap(), 0, 0);
            assert!(!p.is_null());
            assert!(HeapSize(GetProcessHeap(), 0, p) >= 1);
            assert_eq!(HeapFree(GetProcessHeap(), 0, p), 1);
            // NULL free is a no-op success; double free fails cleanly (no host abort).
            assert_eq!(HeapFree(GetProcessHeap(), 0, std::ptr::null_mut()), 1);
            assert_eq!(HeapFree(GetProcessHeap(), 0, p), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Wrong heap fails cleanly.
            assert!(HeapAlloc(0x1234, 0, 16).is_null());
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Zeroed alloc reads as zeros.
            let z = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, 32) as *mut u8;
            assert!(!z.is_null());
            assert!(std::slice::from_raw_parts(z, 32).iter().all(|&b| b == 0));
            // Realloc grows, keeps content, zeroes the tail under HEAP_ZERO_MEMORY.
            std::ptr::write_bytes(z, 0xAB, 32);
            let q = HeapReAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, z as *mut c_void, 64) as *mut u8;
            assert!(!q.is_null());
            assert!(std::slice::from_raw_parts(q, 32).iter().all(|&b| b == 0xAB));
            assert!(std::slice::from_raw_parts(q.add(32), 32).iter().all(|&b| b == 0));
            assert_eq!(HeapSize(GetProcessHeap(), 0, q as *const c_void), 64);
            // Realloc to 0 frees and returns NULL.
            assert!(HeapReAlloc(GetProcessHeap(), 0, q as *mut c_void, 0).is_null());
            // NULL realloc behaves as alloc.
            let r = HeapReAlloc(GetProcessHeap(), 0, std::ptr::null_mut(), 16);
            assert!(!r.is_null());
            assert_eq!(HeapFree(GetProcessHeap(), 0, r), 1);
            assert_eq!(HeapValidate(GetProcessHeap(), 0, std::ptr::null()), 1);
            assert_eq!(HeapDestroy(GetProcessHeap()), 1);
            assert_eq!(HeapDestroy(0x1234), 0);
        }
    }

    #[test]
    fn module_file_name_answers_mapped_and_hle_images() {
        unsafe {
            // HLE pseudo-handles report a System32 stand-in.
            let h = crate::modules::hle_handle("kernelx_unit_probe.dll");
            let mut wbuf = [0u16; 260];
            let n = GetModuleFileNameW(h, wbuf.as_mut_ptr(), 260);
            assert!(n > 0);
            let s = String::from_utf16_lossy(&wbuf[..n as usize]);
            assert!(s.ends_with("kernelx_unit_probe.dll"), "{s}");
            let mut abuf = [0u8; 260];
            let m = GetModuleFileNameA(h, abuf.as_mut_ptr(), 260);
            assert!(m > 0);
            // Unknown handles fail cleanly.
            assert_eq!(GetModuleFileNameW(0xDEAD_BEEF_1234, wbuf.as_mut_ptr(), 260), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Truncation reports ERROR_INSUFFICIENT_BUFFER with `size` chars, as on Windows.
            let mut tiny = [0u16; 4];
            let t = GetModuleFileNameW(h, tiny.as_mut_ptr(), 4);
            assert_eq!(t, 4);
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            // Null buffer/size fails cleanly.
            assert_eq!(GetModuleFileNameW(h, std::ptr::null_mut(), 260), 0);
            assert_eq!(GetModuleFileNameW(h, wbuf.as_mut_ptr(), 0), 0);
        }
    }
}
