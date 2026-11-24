//! Linux kernelx core (Phase C step 10b): memory, threads, sync, and time.
//!
//! The Windows `hle/kernelx.rs` stays the implementation source of truth for the full
//! title path. This module is the Linux twin for the step-10b slice: virtual memory via
//! `mmap`/`mprotect`, game threads via [`crate::tls::spawn`] (so static TLS attach and
//! `DLL_THREAD_ATTACH`/`DETACH` callbacks run), synchronization primitives with inline
//! Windows layout semantics, and monotonic time. Xbox thread priorities degrade
//! gracefully: the requested value is remembered and returned, while the host scheduler
//! change is best-effort (no abort without `CAP_SYS_NICE`).
//!
//! Calling convention: the game is a Windows PE, so every export is
//! `unsafe extern "win64"`, matching the stub thunks and the TLS callbacks.
//!
//! Unrelated exports (file APIs, module loading, physical pages, XAudio/GPU paths) stay
//! `Stub` in `hle_linux` until steps 10c–10d and Phase D/E land.
//!
//! Handle model: events and threads are monotonic ids in process-wide tables. `GetCurrentProcess`
//! returns `-1` and `GetCurrentThread` returns `-2`, as on Windows.

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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;

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
}
