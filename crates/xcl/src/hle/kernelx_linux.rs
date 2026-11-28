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
use std::sync::{Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pe::ImportName;
use crate::util::{from_ansi_ptr, from_wide_ptr, E_INVALIDARG, HRESULT, S_FALSE, S_OK};

// ---------------------------------------------------------------------------------------
// Last error (per-thread, as on Windows)
// ---------------------------------------------------------------------------------------

fn set_error(e: u32) {
    crate::util::set_last_error(e);
}

fn get_error() -> u32 {
    crate::util::get_last_error()
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
pub const WAIT_ABANDONED: u32 = 0x80;
pub const WAIT_TIMEOUT: u32 = 258;
pub const WAIT_FAILED: u32 = 0xFFFF_FFFF;
pub const INFINITE: u32 = 0xFFFF_FFFF;

const ERROR_TIMEOUT: u32 = 1460;
const ERROR_TOO_MANY_POSTS: u32 = 298;
const ERROR_NOT_OWNER: u32 = 288;

// Memory types.
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_DECOMMIT: u32 = 0x4000;
const MEM_RELEASE: u32 = 0x8000;
// Placeholder reservations (VirtualAlloc/VirtualFree flags, not types).
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
const MEM_PRESERVE_PLACEHOLDER: u32 = 0x2;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_RESERVE_PLACEHOLDER: u32 = 0x40000;

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

/// One address range of a placeholder reservation. Splitting and replacing change the
/// logical regions in place, never release their addresses; `backing` is the graphics-pool
/// allocation that owns every range (title placeholders back physical pages, so the VA
/// space must stay GPU-visible).
#[derive(Clone)]
struct PlaceholderRegion {
    end: usize,
    placeholder: bool,
    backing: usize,
}

static PLACEHOLDERS: LazyLock<Mutex<BTreeMap<usize, PlaceholderRegion>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn placeholders() -> &'static Mutex<BTreeMap<usize, PlaceholderRegion>> {
    &PLACEHOLDERS
}

/// `None`: not a placeholder operation; the ordinary allocation path handles it.
unsafe fn placeholder_alloc(addr: *const c_void, size: usize, atype: u32, protect: u32) -> Option<*mut c_void> {
    if atype & MEM_RESERVE_PLACEHOLDER != 0 {
        if atype & (MEM_RESERVE | MEM_COMMIT) != MEM_RESERVE || protect != PAGE_NOACCESS || size == 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return Some(std::ptr::null_mut());
        }
        if !addr.is_null() {
            set_error(ERROR_INVALID_ADDRESS);
            return Some(std::ptr::null_mut());
        }
        let Some(len) = size.checked_next_multiple_of(GFX_GRAIN) else {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            return Some(std::ptr::null_mut());
        };
        let Some(base) = gfx_alloc(len, GFX_GRAIN, false) else {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            return Some(std::ptr::null_mut());
        };
        placeholders().lock().unwrap().insert(
            base,
            PlaceholderRegion { end: base + len, placeholder: true, backing: base },
        );
        set_error(ERROR_SUCCESS);
        return Some(base as *mut c_void);
    }
    if atype & MEM_REPLACE_PLACEHOLDER != 0 && atype & MEM_RESERVE != 0 {
        let base = addr as usize;
        let mut regions = placeholders().lock().unwrap();
        let Some(r) = regions
            .get_mut(&base)
            .filter(|r| r.placeholder && base.checked_add(size) == Some(r.end))
        else {
            set_error(ERROR_INVALID_ADDRESS);
            return Some(std::ptr::null_mut());
        };
        if atype & MEM_COMMIT != 0 && !gfx_commit_at(base, size) {
            set_error(ERROR_INVALID_ADDRESS);
            return Some(std::ptr::null_mut());
        }
        r.placeholder = false;
        set_error(ERROR_SUCCESS);
        return Some(base as *mut c_void);
    }
    // Committing a range of a replaced placeholder: `VirtualAlloc(base, size, MEM_COMMIT)`.
    if atype & (MEM_REPLACE_PLACEHOLDER | MEM_RESERVE) == 0 && atype & MEM_COMMIT != 0 && !addr.is_null() && size != 0 {
        let base = addr as usize;
        let mut regions = placeholders().lock().unwrap();
        if let Some(r) = regions.range(..=base).next_back().map(|(_, r)| r) {
            if base < r.end
                && !r.placeholder
                && base.checked_add(size).is_some_and(|end| end <= r.end)
            {
                if !gfx_commit_at(base, size) {
                    set_error(ERROR_INVALID_ADDRESS);
                    return Some(std::ptr::null_mut());
                }
                set_error(ERROR_SUCCESS);
                return Some(base as *mut c_void);
            }
        }
    }
    None
}

/// `None`: the address is not part of a placeholder reservation.
unsafe fn placeholder_free(addr: *mut c_void, size: usize, ftype: u32) -> Option<i32> {
    let base = addr as usize;
    let mut regions = placeholders().lock().unwrap();
    let (&start, region) = regions.range(..=base).next_back().filter(|(_, r)| base < r.end)?;
    let r = region.clone();
    let fail = || {
        set_error(ERROR_INVALID_PARAMETER);
        Some(0)
    };
    if ftype == MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER {
        if r.placeholder {
            let Some(end) = base
                .checked_add(size)
                .filter(|&end| size > 0 && end <= r.end && end.is_multiple_of(GFX_GRAIN))
            else {
                return fail();
            };
            if !base.is_multiple_of(GFX_GRAIN) {
                return fail();
            }
            regions.remove(&start);
            if start < base {
                regions.insert(start, PlaceholderRegion { end: base, ..r.clone() });
            }
            regions.insert(base, PlaceholderRegion { end, ..r.clone() });
            if end < r.end {
                regions.insert(end, r);
            }
            set_error(ERROR_SUCCESS);
            return Some(1);
        }
        // Returning a replaced range to a placeholder releases its pages.
        if start != base || (size != 0 && base.checked_add(size) != Some(r.end)) {
            return fail();
        }
        gfx_decommit_at(base, r.end - base);
        if let Some(region) = regions.get_mut(&base) {
            region.placeholder = true;
        }
        set_error(ERROR_SUCCESS);
        return Some(1);
    }
    if ftype == MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS {
        let Some(end) = base.checked_add(size).filter(|&end| end > base) else { return fail() };
        let mut next = base;
        let mut keys = Vec::new();
        for (&at, region) in regions.range(base..end) {
            if at != next || !region.placeholder || region.backing != r.backing || region.end > end {
                return fail();
            }
            keys.push(at);
            next = region.end;
        }
        if start != base || next != end {
            return fail();
        }
        for key in keys {
            regions.remove(&key);
        }
        regions.insert(base, PlaceholderRegion { end, ..r });
        set_error(ERROR_SUCCESS);
        return Some(1);
    }
    if ftype == MEM_DECOMMIT {
        if r.placeholder || (size == 0 && start != base) || base.checked_add(size).is_none_or(|end| end > r.end) {
            return fail();
        }
        let len = if size == 0 { r.end - base } else { size };
        gfx_decommit_at(base, len);
        set_error(ERROR_SUCCESS);
        return Some(1);
    }
    if start != base || ftype != MEM_RELEASE || size != 0 {
        return fail();
    }
    let r = regions.remove(&base).unwrap();
    if !r.placeholder {
        gfx_decommit_at(base, r.end - base);
    }
    if !regions.values().any(|region| region.backing == r.backing) {
        gfx_free(r.backing);
    }
    set_error(ERROR_SUCCESS);
    Some(1)
}

pub unsafe extern "win64" fn VirtualAlloc(
    addr: *const c_void,
    size: usize,
    atype: u32,
    protect: u32,
) -> *mut c_void {
    if let Some(q) = placeholder_alloc(addr, size, atype, protect) {
        return q;
    }
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
    if committed {
        crate::memstat::note(crate::memstat::WINDOWS, len);
    }
    set_error(ERROR_SUCCESS);
    ptr as *mut c_void
}

pub unsafe extern "win64" fn VirtualFree(addr: *mut c_void, size: usize, ftype: u32) -> i32 {
    if addr.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if let Some(ok) = placeholder_free(addr, size, ftype) {
        return ok;
    }
    let base = addr as usize;
    // Graphics-pool ranges (XMemVirtualAlloc/XMemAlloc/XMemMapPhysicalPages) release
    // back to the pool; decommit keeps the VA reserved.
    if gfx_contains(base) {
        if ftype & MEM_RELEASE != 0 {
            if size != 0 {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            }
            // Only whole-allocation releases are supported (the title always does
            // `VirtualFree(base, 0, MEM_RELEASE)`); anything else is a caller bug.
            let Some(len) = gfx_allocation_size(base) else {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            };
            // Dropping a reservation also drops every page mapping inside it (each
            // page's data belongs home again); forgetting only the exact base left
            // stale entries behind for the rest of the range.
            unmap_away_range(base, len);
            if !gfx_free(base) {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            }
            set_error(ERROR_SUCCESS);
            return 1;
        }
        if ftype & MEM_DECOMMIT != 0 {
            let len = align_up(size, page_size());
            if base % page_size() != 0 || len == 0 {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            }
            // Decommitted pages are unmapped first (their data belongs home again);
            // otherwise stale address→page entries survive the decommit.
            unmap_away_range(base, len);
            gfx_decommit_at(base, len);
            set_error(ERROR_SUCCESS);
            return 1;
        }
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
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
        // Physical pages mapped into this reservation go home before it disappears.
        unmap_away_range(base, len);
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
        unmap_away_range(base, len);
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

/// Host floor for every guest thread stack (DEC-017). Retail requests as little as
/// 0x8000 of stack, but HLE calls run on the guest thread and can descend deep into
/// host libraries: the Vulkan loader/ICDs alone overflowed a 0x8000 stack inside
/// `vkCreateInstance` from `Queue_PresentX`, faulting at the stack floor. The title's
/// `dwStackSize` is a reserve hint, so a larger request is honoured and a smaller one
/// is raised to this minimum; reservation is virtual only.
const MIN_HOST_THREAD_STACK: usize = 8 << 20;

type ThreadStart = unsafe extern "win64" fn(*mut c_void) -> u32;

struct ThreadRecord {
    tid: AtomicU32,
    priority: AtomicU32,
    boost: AtomicU32,
    ideal: AtomicU32,
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

/// Whether `tid` still names a live thread of this process (`tgkill(..., 0)` probe).
fn thread_is_alive(tid: u32) -> bool {
    unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            libc::getpid() as libc::c_long,
            tid as libc::c_long,
            0 as libc::c_long,
        ) == 0
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

/// `HANDLE OpenThread(DWORD access, BOOL inherit, DWORD tid)` — opens a fresh handle for a
/// live thread of this process. Access and inheritance are accepted (`-1` pseudo handles
/// already carry all rights); the handle joins the `THREADS` table, so thread-state calls
/// through it see the same record as the creation handle. Waits stay registered on the
/// creation handle: a wait needs that thread's join, and an opened handle has none.
pub unsafe extern "win64" fn OpenThread(_access: u32, _inherit: i32, tid: u32) -> usize {
    if tid == 0 || !thread_is_alive(tid) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let record = threads()
        .lock()
        .unwrap()
        .values()
        .find(|r| r.tid.load(Ordering::Relaxed) == tid)
        .cloned()
        .unwrap_or_else(|| {
            let current = tid == current_tid();
            std::sync::Arc::new(ThreadRecord {
                tid: AtomicU32::new(tid),
                priority: AtomicU32::new(if current {
                    CURRENT_PRIO.load(Ordering::Relaxed)
                } else {
                    THREAD_PRIORITY_NORMAL as u32
                }),
                boost: AtomicU32::new(if current {
                    CURRENT_BOOST.load(Ordering::Relaxed)
                } else {
                    0
                }),
                ideal: AtomicU32::new(0),
                name: Mutex::new(format!("xcl-open-{tid:x}")),
                suspended_gate: None,
            })
        });
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    threads().lock().unwrap().insert(handle, record);
    set_error(ERROR_SUCCESS);
    handle
}

// ---------------------------------------------------------------------------------------
// Process termination: ExitProcess / TerminateProcess (step 11h)
// ---------------------------------------------------------------------------------------
//
// The test title ends with `std::process::exit(failures)`, which calls
// `kernel32!ExitProcess`. The auto-stub used to return 0, so the guest fell through
// into the compiler's `ud2` after the call (SIGILL at the shared rust-panic address).
// These shims never return: they write `report.json` with the title's exit code
// (the launcher's `summarize` patch only runs on the entry-return path) and end the
// host process with the same code.

pub unsafe extern "win64" fn ExitProcess(code: u32) -> ! {
    // Port of upstream #1479: the ending is logged and reported once, for the
    // current process only. On the Linux host there is no process-detach to
    // write the buffered report (like Wine), so the predicate is always true
    // and the report is written before the log flush.
    if crate::eh::begin_termination(
        -1,
        &crate::eh::ENDING,
        || true,
        || crate::report::write_final_with_code(None, code as i64),
    ) {
        crate::xlog_info!("ExitProcess({code:#x})");
        if code != 0 {
            // Port of upstream #194: the memory line at an error exit (a nonzero
            // exit is the process ending with an error, like a crash).
            crate::memstat::log();
        }
        // Best-effort DLL/FLS/TLS process-detach would run here on Windows; on Linux the
        // mapped images stay valid until `_exit`, and taking more locks from inside an
        // arbitrary guest call risks deadlock, so only the report is written.
        // (The report itself was written by `begin_termination` above.)
        crate::log::flush();
    }
    // Upstream #2698 twin: past any cleanup path that allocates — a title thread may hold
    // the allocator at the end of a run; the report is written and the log flushed above.
    crate::eh::end_process_now(code as i32);
}

pub unsafe extern "win64" fn TerminateProcess(proc_: usize, code: u32) -> i32 {
    if proc_ != current_process_handle() {
        crate::xlog_warn!("TerminateProcess({proc_:#x}): only the current process can be terminated on Linux");
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if crate::eh::begin_termination(
        -1,
        &crate::eh::ENDING,
        || true,
        || crate::report::write_final_with_code(None, code as i64),
    ) {
        crate::xlog_info!("TerminateProcess(current, {code:#x})");
        if code != 0 {
            // Port of upstream #194: the memory line at an error exit.
            crate::memstat::log();
        }
        crate::log::flush();
    }
    // Upstream #2698 twin: past any cleanup path that allocates — a title thread may hold
    // the allocator at the end of a run; the report is written and the log flushed above.
    crate::eh::end_process_now(code as i32);
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
        boost: AtomicU32::new(0),
        ideal: AtomicU32::new(0),
        name: Mutex::new(format!("xcl-game-{handle:x}")),
        suspended_gate: gate,
    });
    let record_for_thread = record.clone();
    threads().lock().unwrap().insert(handle, record);
    let thread_name = format!("xcl-game-{handle:x}");
    let builder = std::thread::Builder::new()
        .name(thread_name)
        .stack_size(stack.max(MIN_HOST_THREAD_STACK));
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
        // Run FLS callbacks for dying values before the static-TLS blocks go away.
        fls_thread_exit();
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
pub const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;

// Win32 thread-priority constants (mirrors `windows_sys::Win32::System::Threading` values,
// which never compile on Linux).
pub const THREAD_PRIORITY_IDLE: i32 = -15;
pub const THREAD_PRIORITY_LOWEST: i32 = -2;
pub const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
pub const THREAD_PRIORITY_NORMAL: i32 = 0;
pub const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;
pub const THREAD_PRIORITY_HIGHEST: i32 = 2;
pub const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;
pub const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
pub const THREAD_MODE_BACKGROUND_END: i32 = 0x0002_0000;

static CURRENT_PRIO: AtomicU32 = AtomicU32::new(0);
static CURRENT_BOOST: AtomicU32 = AtomicU32::new(0);

/// Map console / non-standard thread priority levels to valid Win32 priority constants.
/// Pure function (mirrors the Windows helper) so unit tests pin the mapping.
pub fn map_thread_priority(priority: i32) -> i32 {
    match priority {
        THREAD_PRIORITY_IDLE
        | THREAD_PRIORITY_LOWEST
        | THREAD_PRIORITY_BELOW_NORMAL
        | THREAD_PRIORITY_NORMAL
        | THREAD_PRIORITY_ABOVE_NORMAL
        | THREAD_PRIORITY_HIGHEST
        | THREAD_PRIORITY_TIME_CRITICAL
        | THREAD_MODE_BACKGROUND_BEGIN
        | THREAD_MODE_BACKGROUND_END => priority,

        p if p <= -15 => THREAD_PRIORITY_IDLE,
        p if p < -2 => THREAD_PRIORITY_LOWEST,
        p if (3..=6).contains(&p) => THREAD_PRIORITY_HIGHEST,
        p if p >= 7 => THREAD_PRIORITY_TIME_CRITICAL,

        _ => priority,
    }
}

pub unsafe extern "win64" fn SetThreadPriority(handle: usize, prio: i32) -> i32 {
    let mapped = map_thread_priority(prio);
    if handle == current_thread_handle() || handle == 0 {
        // Best effort: lowering niceness requires no privilege only for raising it.
        // Attempt, ignore the result, and always succeed like the degraded path.
        let _ = libc::nice(0);
        static WARNED: AtomicU32 = AtomicU32::new(0);
        if WARNED.swap(1, Ordering::Relaxed) == 0 {
            crate::xlog_info!("SetThreadPriority({prio}): host scheduling is best-effort on Linux");
        }
        // Remember per pseudo-handle? Pseudo handles are process-wide; keep a single slot.
        // Stored mapped so GetThreadPriority returns a conformant Win32 value.
        CURRENT_PRIO.store(mapped as u32, Ordering::Relaxed);
        set_error(ERROR_SUCCESS);
        return 1;
    }
    let g = threads().lock().unwrap();
    match g.get(&handle) {
        Some(rec) => {
            rec.priority.store(mapped as u32, Ordering::Relaxed);
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
        return CURRENT_PRIO.load(Ordering::Relaxed) as i32;
    }
    let g = threads().lock().unwrap();
    match g.get(&handle) {
        Some(rec) => rec.priority.load(Ordering::Relaxed) as i32,
        None => {
            set_error(ERROR_INVALID_HANDLE);
            THREAD_PRIORITY_ERROR_RETURN
        }
    }
}

pub unsafe extern "win64" fn SetThreadPriorityBoost(handle: usize, disable: i32) -> i32 {
    let known = handle == current_thread_handle()
        || handle == 0
        || threads().lock().unwrap().contains_key(&handle);
    if !known {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if handle == current_thread_handle() || handle == 0 {
        CURRENT_BOOST.store(disable as u32, Ordering::Relaxed);
    } else {
        let g = threads().lock().unwrap();
        if let Some(rec) = g.get(&handle) {
            rec.boost.store(disable as u32, Ordering::Relaxed);
        }
    }
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn GetThreadPriorityBoost(handle: usize, disable: *mut i32) -> i32 {
    let rec_opt = if handle == current_thread_handle() || handle == 0 {
        None
    } else {
        threads().lock().unwrap().get(&handle).cloned()
    };
    let is_current = handle == current_thread_handle() || handle == 0;
    if !is_current && rec_opt.is_none() {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if disable.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let v = if is_current {
        CURRENT_BOOST.load(Ordering::Relaxed)
    } else {
        rec_opt.unwrap().boost.load(Ordering::Relaxed)
    };
    *disable = v as i32;
    set_error(ERROR_SUCCESS);
    1
}

/// `BOOL SetProcessPriorityBoost(HANDLE process, BOOL disable)` — Linux has no dynamic
/// priority boost to toggle, so the request is an accepted scheduling hint: a disable
/// request describes the host's behavior, and an enable request is logged once and
/// ignored. A handle other than the current process fails like Windows.
pub unsafe extern "win64" fn SetProcessPriorityBoost(handle: usize, disable: i32) -> i32 {
    if handle != current_process_handle() {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if disable == 0 {
        static WARNED: AtomicU32 = AtomicU32::new(0);
        if WARNED.swap(1, Ordering::Relaxed) == 0 {
            crate::xlog_debug!(
                "SetProcessPriorityBoost(enable): Linux has no dynamic priority boost; ignored"
            );
        }
    }
    set_error(ERROR_SUCCESS);
    1
}

/// The console's dedicated title cores (7 cores, cores 0..6).
pub const CONSOLE_TITLE_CORES: u32 = 7;
/// The console's title core affinity mask (cores 0..6).
pub const CONSOLE_TITLE_AFFINITY_MASK: usize = (1 << CONSOLE_TITLE_CORES) - 1; // 0x7F
/// With SMT enabled: 7 cores x 2 threads = 14 threads (threads 0..13).
pub const CONSOLE_TITLE_SMT_AFFINITY_MASK: usize = (1 << (CONSOLE_TITLE_CORES * 2)) - 1; // 0x3FFF

/// An affinity mask the PC takes. The title sees the PC's processors, so a mask made from them
/// passes unchanged. A console mask (its 7 cores / 14 threads) that names processors outside the
/// process's affinity is fitted: every processor in the mask that belongs to the process's
/// affinity is kept, and any processor outside it is mapped modulo the count of available
/// processors. Pure function (mirrors the Windows helper) so unit tests pin the mapping.
pub fn fit_affinity_mask(mask: usize, allowed: usize) -> usize {
    if mask == 0 || allowed == 0 || mask & !allowed == 0 {
        return mask;
    }
    let pcs: Vec<u32> = (0..usize::BITS).filter(|b| allowed >> b & 1 != 0).collect();
    if pcs.is_empty() {
        return mask;
    }
    (0..usize::BITS)
        .filter(|b| mask >> b & 1 != 0)
        .fold(0, |m, b| {
            if (allowed >> b) & 1 != 0 {
                m | (1 << b)
            } else {
                m | (1 << pcs[b as usize % pcs.len()])
            }
        })
}

/// Live affinity mask of one host thread (`tid == 0` = the calling thread).
fn affinity_mask_for_tid(tid: u32) -> Option<usize> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(tid as libc::pid_t, std::mem::size_of_val(&set), &mut set) != 0 {
            return None;
        }
        let mut mask = 0usize;
        for cpu in 0..(8 * std::mem::size_of::<libc::cpu_set_t>() as u32).min(usize::BITS) {
            if libc::CPU_ISSET(cpu as usize, &set) {
                mask |= 1 << cpu;
            }
        }
        (mask != 0).then_some(mask)
    }
}

fn process_affinity() -> usize {
    affinity_mask_for_tid(0).unwrap_or(usize::MAX)
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
    if handle == 0 {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if mask == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
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
    if handle == 0 {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if affinity.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (*affinity).mask == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (*affinity).group != 0 {
        // Multi-group processes have no Linux equivalent (one cpu_set_t group);
        // fail cleanly instead of mis-applying the mask.
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

/// `BOOL GetThreadGroupAffinity(HANDLE hThread, PGROUP_AFFINITY GroupAffinity)` — the target
/// thread's live host mask in the single processor group Linux exposes. Mirrors
/// [`SetThreadGroupAffinity`]: pseudo handles target the caller, a known record whose kernel
/// tid is not stored yet inherits the process affinity, and an unknown handle fails.
pub unsafe extern "win64" fn GetThreadGroupAffinity(
    handle: usize,
    out: *mut GroupAffinity,
) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if handle == 0 {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let tid = if handle == current_thread_handle() {
        current_tid()
    } else {
        match threads().lock().unwrap().get(&handle).map(|r| r.tid.load(Ordering::Relaxed)) {
            Some(0) => 0, // created thread has not stored its kernel tid: inherit affinity
            Some(tid) => tid,
            None => {
                set_error(ERROR_INVALID_HANDLE);
                return 0;
            }
        }
    };
    let mask = affinity_mask_for_tid(tid)
        .or_else(|| affinity_mask_for_tid(0))
        .unwrap_or(usize::MAX);
    (*out).mask = mask;
    (*out).group = 0;
    (*out).reserved = [0; 3];
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn SetThreadpoolAffinityMask(
    pool: *mut c_void,
    mask: usize,
) -> i32 {
    if pool.is_null() || mask == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let _fit = fitted_affinity(mask, "SetThreadpoolAffinityMask");
    set_error(ERROR_SUCCESS);
    1
}

/// A console ideal-processor number: group 0 only (Linux has a single cpu_set_t group).
/// Mirrors Windows `PROCESSOR_NUMBER`; field names follow the twin's `GroupAffinity` style.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessorNumber {
    pub group: u16,
    pub number: u8,
    pub reserved: u8,
}

static CURRENT_IDEAL: AtomicU32 = AtomicU32::new(0);

/// An ideal processor number fitted to the process's affinity mask.
/// Pure function (mirrors the Windows helper) so unit tests pin the mapping.
pub fn fit_ideal_processor(ideal: u32, allowed: usize) -> u32 {
    if ideal == u32::MAX || allowed == 0 || (ideal < usize::BITS && (allowed >> ideal) & 1 != 0) {
        return ideal;
    }
    let pcs: Vec<u32> = (0..usize::BITS).filter(|b| allowed >> b & 1 != 0).collect();
    if pcs.is_empty() {
        return ideal;
    }
    pcs[ideal as usize % pcs.len()]
}

/// Remember the ideal processor per handle and report the previous one. The host
/// scheduler hint is best-effort and never changes the thread's affinity mask (an
/// ideal processor is a hint, not a pin), so the call succeeds once validated.
pub unsafe extern "win64" fn SetThreadIdealProcessor(handle: usize, ideal: u32) -> u32 {
    if handle == 0 {
        set_error(ERROR_INVALID_HANDLE);
        return u32::MAX;
    }
    let allowed = process_affinity();
    let fit = fit_ideal_processor(ideal, allowed);
    if fit != ideal {
        crate::xlog_debug!(
            "SetThreadIdealProcessor: ideal processor {ideal} outside process mask {allowed:#x} -> {fit}"
        );
    }
    if handle == current_thread_handle() {
        let prev = CURRENT_IDEAL.swap(fit, Ordering::Relaxed);
        set_error(ERROR_SUCCESS);
        return prev;
    }
    let g = threads().lock().unwrap();
    match g.get(&handle) {
        Some(rec) => {
            let prev = rec.ideal.swap(fit, Ordering::Relaxed);
            set_error(ERROR_SUCCESS);
            prev
        }
        None => {
            set_error(ERROR_INVALID_HANDLE);
            u32::MAX
        }
    }
}

pub unsafe extern "win64" fn SetThreadIdealProcessorEx(
    handle: usize,
    ideal: *const ProcessorNumber,
    prev: *mut ProcessorNumber,
) -> i32 {
    if handle == 0 {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if ideal.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (*ideal).group != 0 {
        // Multi-group processes have no Linux equivalent; fail cleanly.
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let allowed = process_affinity();
    let fit_num = fit_ideal_processor((*ideal).number as u32, allowed) as u8;
    let store = |slot: &AtomicU32| slot.swap(fit_num as u32, Ordering::Relaxed) as u8;
    let prev_num = if handle == current_thread_handle() {
        store(&CURRENT_IDEAL)
    } else {
        let g = threads().lock().unwrap();
        match g.get(&handle) {
            Some(rec) => store(&rec.ideal),
            None => {
                set_error(ERROR_INVALID_HANDLE);
                return 0;
            }
        }
    };
    if !prev.is_null() {
        *prev = ProcessorNumber {
            group: 0,
            number: prev_num,
            reserved: 0,
        };
    }
    set_error(ERROR_SUCCESS);
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

/// `BOOL InitializeCriticalSectionEx(cs, spin, flags)`: the flags select debug info and
/// a few legacy behaviours; XCL's CS has no debug metadata, so they are accepted and the
/// spin count is used.
pub unsafe extern "win64" fn InitializeCriticalSectionEx(
    cs: *mut CritSec,
    spin: u32,
    _flags: u32,
) -> i32 {
    InitializeCriticalSectionAndSpinCount(cs, spin)
}

/// `PVOID EncodePointer(PVOID)` — XCL has no pointer cookies; identity is the
/// documented "no encoding" behaviour and keeps `DecodePointer` symmetric.
pub unsafe extern "win64" fn EncodePointer(p: *mut c_void) -> *mut c_void {
    p
}

/// `PVOID DecodePointer(PVOID)` — see `EncodePointer`.
pub unsafe extern "win64" fn DecodePointer(p: *mut c_void) -> *mut c_void {
    p
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
    if let Some(sem) = is_sem(handle) {
        return if wait_sem(&sem, timeout_ms) {
            WAIT_OBJECT_0
        } else {
            WAIT_TIMEOUT
        };
    }
    if let Some(t) = is_timer(handle) {
        return if wait_timer(&t, timeout_ms) {
            WAIT_OBJECT_0
        } else {
            WAIT_TIMEOUT
        };
    }
    if let Some(m) = is_mutex(handle) {
        return match wait_mutex(&m, timeout_ms) {
            MutexOutcome::Acquired => WAIT_OBJECT_0,
            MutexOutcome::Abandoned => WAIT_ABANDONED,
            MutexOutcome::Timeout => WAIT_TIMEOUT,
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
                } else if let Some(t) = is_timer(h) {
                    if !*t.signaled.lock().unwrap() {
                        all = false;
                        break;
                    }
                } else if let Some(s) = is_sem(h) {
                    if *s.count.lock().unwrap() <= 0 {
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
                    } else if let Some(s) = is_sem(h) {
                        try_acquire_sem(&s);
                    } else if let Some(t) = is_timer(h) {
                        if !t.manual {
                            *t.signaled.lock().unwrap() = false;
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
                } else if let Some(t) = is_timer(h) {
                    let mut g = t.signaled.lock().unwrap();
                    if *g {
                        if !t.manual {
                            *g = false;
                        }
                        return WAIT_OBJECT_0 + i as u32;
                    }
                } else if let Some(s) = is_sem(h) {
                    if try_acquire_sem(&s) {
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

// ---------------------------------------------------------------------------------------
// Semaphores: Linux-backed counting objects (retail `CreateSemaphoreExW` path).
// ---------------------------------------------------------------------------------------
//
// The game creates a semaphore early in startup and retry-loops the call: the old
// auto-stub returned NULL, so the loop fell through to the title's deliberate `int3`.
// These are real host objects in their own handle table (closed through `CloseHandle`)
// and waitable through `WaitForSingleObject`/`WaitForMultipleObjects` above: each
// successful wait consumes one count, `ReleaseSemaphore` adds back up to the maximum.
// Names/security/flags/access are accepted and ignored (no cross-process sharing on
// Linux); validation mirrors Windows (`max <= 0` or `initial` outside `0..=max`
// fails with `ERROR_INVALID_PARAMETER`, over-release with `ERROR_TOO_MANY_POSTS`).
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

struct SemState {
    count: Mutex<i32>,
    max: i32,
    cvar: Condvar,
}

static SEMAPHORES: OnceLock<Mutex<HashMap<usize, std::sync::Arc<SemState>>>> = OnceLock::new();

fn semaphores() -> &'static Mutex<HashMap<usize, std::sync::Arc<SemState>>> {
    SEMAPHORES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_sem(handle: usize) -> Option<std::sync::Arc<SemState>> {
    semaphores().lock().unwrap().get(&handle).cloned()
}

fn close_sem_handle(h: usize) -> bool {
    if let Some(s) = semaphores().lock().unwrap().remove(&h) {
        s.cvar.notify_all();
        true
    } else {
        false
    }
}

fn wait_sem(sem: &std::sync::Arc<SemState>, timeout_ms: u32) -> bool {
    let mut g = sem.count.lock().unwrap();
    if *g > 0 {
        *g -= 1;
        return true;
    }
    if timeout_ms == 0 {
        return false;
    }
    if timeout_ms == INFINITE {
        while *g <= 0 {
            g = sem.cvar.wait(g).unwrap();
        }
        *g -= 1;
        return true;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    while *g <= 0 {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (guard, timeout) = sem.cvar.wait_timeout(g, deadline - now).unwrap();
        g = guard;
        if *g > 0 {
            break;
        }
        if timeout.timed_out() {
            return false;
        }
    }
    *g -= 1;
    true
}

fn try_acquire_sem(sem: &std::sync::Arc<SemState>) -> bool {
    let mut g = sem.count.lock().unwrap();
    if *g > 0 {
        *g -= 1;
        true
    } else {
        false
    }
}

/// `HANDLE CreateSemaphoreExW(SECURITY_ATTRIBUTES*, LONG initial, LONG max, LPCWSTR name,
/// DWORD flags, DWORD access)` — counting semaphore, never NULL on valid counts.
pub unsafe extern "win64" fn CreateSemaphoreExW(
    _attrs: *const c_void,
    initial: i32,
    max: i32,
    _name: *const u16,
    _flags: u32,
    _access: u32,
) -> usize {
    if max <= 0 || initial < 0 || initial > max {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    semaphores().lock().unwrap().insert(
        id,
        std::sync::Arc::new(SemState {
            count: Mutex::new(initial),
            max,
            cvar: Condvar::new(),
        }),
    );
    set_error(ERROR_SUCCESS);
    id
}

/// `HANDLE CreateSemaphoreW(SECURITY_ATTRIBUTES*, LONG initial, LONG max, LPCWSTR name)` —
/// legacy spelling of the same semaphore.
pub unsafe extern "win64" fn CreateSemaphoreW(
    attrs: *const c_void,
    initial: i32,
    max: i32,
    name: *const u16,
) -> usize {
    CreateSemaphoreExW(attrs, initial, max, name, 0, 0)
}

/// `HANDLE CreateSemaphoreExA(SECURITY_ATTRIBUTES*, LONG initial, LONG max, LPCSTR name,
/// DWORD flags, DWORD access)` — ANSI (UTF-8 on Linux) twin.
pub unsafe extern "win64" fn CreateSemaphoreExA(
    attrs: *const c_void,
    initial: i32,
    max: i32,
    _name: *const u8,
    flags: u32,
    access: u32,
) -> usize {
    CreateSemaphoreExW(attrs, initial, max, std::ptr::null(), flags, access)
}

/// `HANDLE CreateSemaphoreA(SECURITY_ATTRIBUTES*, LONG initial, LONG max, LPCSTR name)` —
/// ANSI legacy spelling.
pub unsafe extern "win64" fn CreateSemaphoreA(
    attrs: *const c_void,
    initial: i32,
    max: i32,
    name: *const u8,
) -> usize {
    CreateSemaphoreExA(attrs, initial, max, name, 0, 0)
}

/// `BOOL ReleaseSemaphore(HANDLE, LONG count, LPLONG previous)` — adds `count` up to the
/// maximum, reporting the previous count. Over-release fails without changing the count.
pub unsafe extern "win64" fn ReleaseSemaphore(
    handle: usize,
    release: i32,
    prev: *mut i32,
) -> i32 {
    let Some(sem) = is_sem(handle) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if release <= 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut g = sem.count.lock().unwrap();
    let cur = *g;
    let Some(next) = cur.checked_add(release) else {
        set_error(ERROR_TOO_MANY_POSTS);
        return 0;
    };
    if next > sem.max {
        set_error(ERROR_TOO_MANY_POSTS);
        return 0;
    }
    *g = next;
    drop(g);
    if !prev.is_null() {
        *prev = cur;
    }
    if release == 1 {
        sem.cvar.notify_one();
    } else {
        sem.cvar.notify_all();
    }
    set_error(ERROR_SUCCESS);
    1
}

// ---------------------------------------------------------------------------------------
// Mutexes: Linux-backed recursive mutexes (retail `CreateMutexA` path).
// ---------------------------------------------------------------------------------------
//
// The title's early startup creates unnamed mutexes and releases them through
// `ReleaseMutex`; the old auto-stubs returned NULL / FALSE, so every later wait on the
// returned handle failed. These are real host objects in their own handle table (closed
// through `CloseHandle`) and waitable through `WaitForSingleObject`: acquiring records the
// owner's kernel tid and recurses for repeat acquires by that thread, `ReleaseMutex`
// unwinds the recursion and wakes one waiter, and a release by anyone else fails with
// `ERROR_NOT_OWNER`. Names/security/access are accepted and ignored (no cross-process
// sharing on Linux), like the semaphore table.
//
// A waiter whose owner dies without releasing reports `WAIT_ABANDONED` and takes
// ownership, as on Windows; the owner's liveness is probed with `tgkill`, so blocked
// waits recheck it on a 20 ms tick. Multi-object waits (`WaitForMultipleObjects`) do not
// cover mutexes yet: the title has no such consumer.
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

struct MutexState {
    inner: Mutex<MutexInner>,
    cvar: Condvar,
}

struct MutexInner {
    owner: u32,
    recursion: u32,
}

static MUTEXES: OnceLock<Mutex<HashMap<usize, std::sync::Arc<MutexState>>>> = OnceLock::new();

fn mutexes() -> &'static Mutex<HashMap<usize, std::sync::Arc<MutexState>>> {
    MUTEXES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_mutex(handle: usize) -> Option<std::sync::Arc<MutexState>> {
    mutexes().lock().unwrap().get(&handle).cloned()
}

fn close_mutex_handle(handle: usize) -> bool {
    if let Some(m) = mutexes().lock().unwrap().remove(&handle) {
        let mut g = m.inner.lock().unwrap();
        g.owner = 0;
        g.recursion = 0;
        drop(g);
        // Blocked waiters hold their own reference; wake them so a closed handle never
        // strands a wait, then let them take the orphaned object.
        m.cvar.notify_all();
        true
    } else {
        false
    }
}

enum MutexOutcome {
    Acquired,
    Abandoned,
    Timeout,
}

const MUTEX_ABANDON_POLL: Duration = Duration::from_millis(20);

fn wait_mutex(m: &std::sync::Arc<MutexState>, timeout_ms: u32) -> MutexOutcome {
    let me = current_tid();
    let deadline = if timeout_ms == 0 || timeout_ms == INFINITE {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
    };
    let mut g = m.inner.lock().unwrap();
    loop {
        if g.owner == 0 {
            g.owner = me;
            g.recursion = 1;
            return MutexOutcome::Acquired;
        }
        if g.owner == me {
            g.recursion = g.recursion.saturating_add(1);
            return MutexOutcome::Acquired;
        }
        if !thread_is_alive(g.owner) {
            g.owner = me;
            g.recursion = 1;
            return MutexOutcome::Abandoned;
        }
        if timeout_ms == 0 {
            return MutexOutcome::Timeout;
        }
        let step = match deadline {
            None => MUTEX_ABANDON_POLL,
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return MutexOutcome::Timeout;
                }
                (d - now).min(MUTEX_ABANDON_POLL)
            }
        };
        let (guard, _) = m.cvar.wait_timeout(g, step).unwrap();
        g = guard;
    }
}

/// `HANDLE CreateMutexA(SECURITY_ATTRIBUTES*, BOOL initialOwner, LPCSTR name)` — a recursive
/// mutex, owned by the caller when `initialOwner` is nonzero. Never NULL for valid args.
pub unsafe extern "win64" fn CreateMutexA(
    _attrs: *const c_void,
    initial_owner: i32,
    _name: *const u8,
) -> usize {
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let owned = initial_owner != 0;
    mutexes().lock().unwrap().insert(
        id,
        std::sync::Arc::new(MutexState {
            inner: Mutex::new(MutexInner {
                owner: if owned { current_tid() } else { 0 },
                recursion: u32::from(owned),
            }),
            cvar: Condvar::new(),
        }),
    );
    set_error(ERROR_SUCCESS);
    id
}

/// `BOOL ReleaseMutex(HANDLE)` — unwinds one recursion level; the last release wakes one
/// waiter. A thread that does not own the mutex fails with `ERROR_NOT_OWNER`.
pub unsafe extern "win64" fn ReleaseMutex(handle: usize) -> i32 {
    let Some(m) = is_mutex(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let mut g = m.inner.lock().unwrap();
    if g.owner != current_tid() {
        set_error(ERROR_NOT_OWNER);
        return 0;
    }
    g.recursion = g.recursion.saturating_sub(1);
    if g.recursion == 0 {
        g.owner = 0;
        drop(g);
        m.cvar.notify_one();
    }
    set_error(ERROR_SUCCESS);
    1
}

// ---------------------------------------------------------------------------------------
// SList: the x64 SLIST_HEADER is two 64-bit words; an empty list is all zeros.
// ---------------------------------------------------------------------------------------

/// `VOID InitializeSListHead(PSLIST_HEADER)` — zero the 16-byte header (NULL is a no-op).
pub unsafe extern "win64" fn InitializeSListHead(head: *mut c_void) {
    if head.is_null() {
        return;
    }
    std::ptr::write_bytes(head as *mut u8, 0, 16);
}

// ---------------------------------------------------------------------------------------
// Condition variables: guest-memory CONDITION_VARIABLEs against the CS table.
// ---------------------------------------------------------------------------------------
//
// The game probes `SleepConditionVariableCS`/`WakeAllConditionVariable` through
// `GetProcAddress`; the auto-stub NULLs failed the probe. A Windows
// `CONDITION_VARIABLE` is one pointer (zero-initialized); host wait state lives here
// keyed by guest address, so static (`{0}`) and `InitializeConditionVariable` forms
// both work. `SleepConditionVariableCS` fully releases the critical section while
// blocked (recursion preserved) and reacquires it before returning, as on Windows;
// wakes are a generation bump, so a `Wake` racing the release is never missed and a
// `Wake` with no waiters is lost, as on Windows.
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

struct CondWaiter {
    signaled: Mutex<bool>,
    cvar: Condvar,
}

static CONDVARS: OnceLock<Mutex<HashMap<usize, Vec<std::sync::Arc<CondWaiter>>>>> =
    OnceLock::new();

fn condvars() -> &'static Mutex<HashMap<usize, Vec<std::sync::Arc<CondWaiter>>>> {
    CONDVARS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop `me` from the guest address's waiter list, signalling it first when asked.
fn unpark_cond_waiter(key: usize, me: &std::sync::Arc<CondWaiter>, signal: bool) {
    if signal {
        *me.signaled.lock().unwrap() = true;
        me.cvar.notify_one();
    }
    let mut g = condvars().lock().unwrap();
    if let Some(v) = g.get_mut(&key) {
        v.retain(|w| !std::sync::Arc::ptr_eq(w, me));
        if v.is_empty() {
            g.remove(&key);
        }
    }
}

/// `VOID InitializeConditionVariable(PCONDITION_VARIABLE)` — zeroes the guest slot.
pub unsafe extern "win64" fn InitializeConditionVariable(cv: *mut usize) {
    if !cv.is_null() {
        *cv = 0;
    }
}

/// `BOOL SleepConditionVariableCS(PCONDITION_VARIABLE, PCRITICAL_SECTION, DWORD ms)` —
/// releases `cs` while blocked, reacquires before returning; 1 on wake, 0 on timeout
/// (`ERROR_TIMEOUT`) or on a null/unknown/unowned CS (`ERROR_INVALID_PARAMETER`).
pub unsafe extern "win64" fn SleepConditionVariableCS(
    cv: *mut usize,
    cs: *mut CritSec,
    timeout_ms: u32,
) -> i32 {
    if cv.is_null() || cs.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(st) = cs_state_for(cs as usize) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let me = current_tid();
    // The caller must hold the CS; snapshot the recursion to restore it.
    let saved = {
        let g = st.owner.lock().unwrap();
        if g.0 != me || g.1 == 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        g.1
    };
    let cond_key = cv as usize;
    let waiter = std::sync::Arc::new(CondWaiter {
        signaled: Mutex::new(false),
        cvar: Condvar::new(),
    });
    condvars().lock().unwrap().entry(cond_key).or_default().push(waiter.clone());
    // Fully release, as Windows does (a contender wakes here). A Wake that lands from
    // here on finds us in the list, so it is never missed; a Wake from before we
    // registered is lost, as on Windows.
    {
        let mut g = st.owner.lock().unwrap();
        *g = (0, 0);
        st.cvar.notify_one();
    }
    // Block on our own waiter until signalled (or the timeout lapses).
    let woken = {
        let mut g = waiter.signaled.lock().unwrap();
        if *g {
            true
        } else if timeout_ms == 0 {
            false
        } else if timeout_ms == INFINITE {
            while !*g {
                g = waiter.cvar.wait(g).unwrap();
            }
            true
        } else {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
            while !*g {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (guard, timeout) = waiter.cvar.wait_timeout(g, deadline - now).unwrap();
                g = guard;
                if *g {
                    break;
                }
                if timeout.timed_out() {
                    break;
                }
            }
            *g
        }
    };
    unpark_cond_waiter(cond_key, &waiter, false);
    // Reacquire before returning (same order as `EnterCriticalSection`).
    {
        let mut g = st.owner.lock().unwrap();
        while g.0 != 0 {
            g = st.cvar.wait(g).unwrap();
        }
        *g = (me, saved);
    }
    if woken {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_TIMEOUT);
        0
    }
}

fn wake_condvar(cv: *mut usize, all: bool) {
    if cv.is_null() {
        return;
    }
    let key = cv as usize;
    if all {
        let waiters = condvars().lock().unwrap().remove(&key).unwrap_or_default();
        for w in waiters {
            *w.signaled.lock().unwrap() = true;
            w.cvar.notify_one();
        }
    } else {
        let one = condvars().lock().unwrap().get(&key).and_then(|v| v.first().cloned());
        if let Some(w) = one {
            unpark_cond_waiter(key, &w, true);
        }
    }
}

/// `VOID WakeConditionVariable(PCONDITION_VARIABLE)` — wakes one waiter, if any.
pub unsafe extern "win64" fn WakeConditionVariable(cv: *mut usize) {
    wake_condvar(cv, false);
}

/// `VOID WakeAllConditionVariable(PCONDITION_VARIABLE)` — wakes every waiter, if any.
pub unsafe extern "win64" fn WakeAllConditionVariable(cv: *mut usize) {
    wake_condvar(cv, true);
}

// ---------------------------------------------------------------------------------------
// Waitable timers + WaitOnAddress family (Phase C step 11e)
// ---------------------------------------------------------------------------------------
//
// Rust `std` on Windows parks threads with `WaitOnAddress`/`WakeByAddress*` and
// implements `thread::sleep` with a one-shot waitable timer (`CreateWaitableTimerExW`
// + `SetWaitableTimer` + `WaitForSingleObject`). The auto-stubs returned NULL for the
// timer, so the first sleep-capable check panicked. These are real host shims:
//
// * Timers live in their own handle table (closed through `CloseHandle`) and are
//   waitable through `WaitForSingleObject`/`WaitForMultipleObjects` above. A sync
//   (auto-reset) timer consumes its signal on wake; a manual-reset timer stays
//   signaled. `SetWaitableTimer` disarms first (bumping the generation so a pending
//   sleeper is cancelled), then arms a detached host thread that fires after the
//   due time and every period when periodic.
// * `WaitOnAddress` is a futex emulation keyed by guest address: waiters register a
//   private condvar, re-check the comparison after registering (so a write+wake that
//   raced the registration is not missed), then block with the timeout. `WakeBy*`
//   signal one/all waiters on that address. APC/completion routines are logged and
//   ignored (no APC delivery on Linux); `fResume` is accepted and ignored.
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

const CREATE_WAITABLE_TIMER_MANUAL_RESET: u32 = 0x1;

struct TimerState {
    manual: bool,
    signaled: Mutex<bool>,
    cvar: Condvar,
    generation: Mutex<u64>,
}

static TIMERS: OnceLock<Mutex<HashMap<usize, std::sync::Arc<TimerState>>>> = OnceLock::new();

fn timers() -> &'static Mutex<HashMap<usize, std::sync::Arc<TimerState>>> {
    TIMERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn is_timer(handle: usize) -> Option<std::sync::Arc<TimerState>> {
    timers().lock().unwrap().get(&handle).cloned()
}

fn close_timer_handle(h: usize) -> bool {
    if let Some(t) = timers().lock().unwrap().remove(&h) {
        // Disarm: pending sleepers see a stale generation and never fire.
        let next = t.generation.lock().unwrap().wrapping_add(1);
        *t.generation.lock().unwrap() = next;
        t.cvar.notify_all();
        true
    } else {
        false
    }
}

fn wait_timer(t: &std::sync::Arc<TimerState>, timeout_ms: u32) -> bool {
    let mut g = t.signaled.lock().unwrap();
    if *g {
        if !t.manual {
            *g = false;
        }
        return true;
    }
    if timeout_ms == 0 {
        return false;
    }
    if timeout_ms == INFINITE {
        while !*g {
            g = t.cvar.wait(g).unwrap();
        }
        if !t.manual {
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
        let (guard, timeout) = t.cvar.wait_timeout(g, deadline - now).unwrap();
        g = guard;
        if *g {
            break;
        }
        if timeout.timed_out() {
            return false;
        }
    }
    if !t.manual {
        *g = false;
    }
    true
}

fn filetime_now_100ns() -> i64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    (ts.tv_sec as i64 + 11_644_473_600) * 10_000_000 + ts.tv_nsec as i64 / 100
}

fn new_timer(manual: bool) -> usize {
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    timers().lock().unwrap().insert(
        h,
        std::sync::Arc::new(TimerState {
            manual,
            signaled: Mutex::new(false),
            cvar: Condvar::new(),
            generation: Mutex::new(0),
        }),
    );
    set_error(ERROR_SUCCESS);
    h
}

/// `HANDLE CreateWaitableTimerExW(SECURITY_ATTRIBUTES*, LPCWSTR, DWORD flags, DWORD access)` —
/// `flags & MANUAL_RESET` selects a manual-reset timer, otherwise a synchronization
/// (auto-reset) timer. Name/security/access are accepted and ignored.
pub unsafe extern "win64" fn CreateWaitableTimerExW(
    _attrs: *const c_void,
    _name: *const u16,
    flags: u32,
    _access: u32,
) -> usize {
    new_timer(flags & CREATE_WAITABLE_TIMER_MANUAL_RESET != 0)
}

/// `HANDLE CreateWaitableTimerW(SECURITY_ATTRIBUTES*, BOOL manual, LPCWSTR)` —
/// legacy spelling of the same timer; `manual != 0` selects manual-reset.
pub unsafe extern "win64" fn CreateWaitableTimerW(
    _attrs: *const c_void,
    manual: i32,
    _name: *const u16,
) -> usize {
    new_timer(manual != 0)
}

fn arm_timer(handle: usize, due: i64, period_ms: u32) {
    let t = is_timer(handle).expect("arm_timer: known handle");
    // Disarm first: drop any pending signal and cancel the pending sleeper.
    *t.signaled.lock().unwrap() = false;
    let gen = {
        let mut g = t.generation.lock().unwrap();
        *g = g.wrapping_add(1);
        *g
    };
    // Resolve the due time to a host delay.
    let delay = if due == 0 {
        Duration::ZERO
    } else if due > 0 {
        let delta_100ns = due.saturating_sub(filetime_now_100ns()).max(0) as u64;
        Duration::from_nanos(delta_100ns * 100)
    } else {
        Duration::from_nanos(due.unsigned_abs() * 100)
    };
    let period = if period_ms == 0 { None } else { Some(Duration::from_millis(period_ms as u64)) };
    let tt = t.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        loop {
            // Stale generation (Cancel/Close/Set superseded us): never fire.
            if *tt.generation.lock().unwrap() != gen {
                return;
            }
            *tt.signaled.lock().unwrap() = true;
            tt.cvar.notify_all();
            match period {
                None => return,
                Some(p) => {
                    std::thread::sleep(p);
                    // Periodic timers re-fire while armed; manual ones stay signaled.
                    if *tt.generation.lock().unwrap() != gen {
                        return;
                    }
                    if !tt.manual {
                        // Re-arm the next period: clear so each period is observable.
                        // A waiter that consumed the signal still sees the next fire.
                        *tt.signaled.lock().unwrap() = true;
                        tt.cvar.notify_all();
                    }
                }
            }
        }
    });
}

/// `BOOL SetWaitableTimer(HANDLE, LARGE_INTEGER* due, LONG period_ms, PTIMERAPCROUTINE,
/// LPVOID, BOOL resume)` — one-shot when `period_ms == 0`, periodic otherwise.
/// A NULL due fails; APC/resume are accepted and ignored (logged once).
pub unsafe extern "win64" fn SetWaitableTimer(
    handle: usize,
    due: *const i64,
    period_ms: i32,
    _apc: usize,
    _apc_ctx: *mut c_void,
    _resume: i32,
) -> i32 {
    if due.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if is_timer(handle).is_none() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if period_ms < 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let d = *due;
    if _apc != 0 {
        static WARNED: AtomicU32 = AtomicU32::new(0);
        if WARNED.swap(1, Ordering::Relaxed) == 0 {
            crate::xlog_debug!("SetWaitableTimer: APC completion routines are ignored on Linux");
        }
    }
    arm_timer(handle, d, period_ms as u32);
    set_error(ERROR_SUCCESS);
    1
}

/// `BOOL CancelWaitableTimer(HANDLE)` — disarms the timer; waiters keep waiting.
pub unsafe extern "win64" fn CancelWaitableTimer(handle: usize) -> i32 {
    let Some(t) = is_timer(handle) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let next = t.generation.lock().unwrap().wrapping_add(1);
    *t.generation.lock().unwrap() = next;
    *t.signaled.lock().unwrap() = false;
    set_error(ERROR_SUCCESS);
    1
}

// --- WaitOnAddress family: futex emulation keyed by guest address ---

struct AddrWaiter {
    signaled: Mutex<bool>,
    cvar: Condvar,
}

static WAIT_ADDR: OnceLock<Mutex<HashMap<usize, Vec<std::sync::Arc<AddrWaiter>>>>> = OnceLock::new();

fn wait_addrs() -> &'static Mutex<HashMap<usize, Vec<std::sync::Arc<AddrWaiter>>>> {
    WAIT_ADDR.get_or_init(|| Mutex::new(HashMap::new()))
}

fn addr_bytes(addr: *const c_void, size: usize) -> Option<Vec<u8>> {
    if addr.is_null() || size == 0 || size > 64 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(addr as *const u8, size).to_vec() })
}

/// `BOOL WaitOnAddress(VOID* addr, PVOID cmp, SIZE_T size, DWORD ms)` — waits while
/// `*addr == *cmp` (compared over `size` bytes). Returns 1 on wake, 0 on timeout
/// (`ERROR_TIMEOUT`) or on an already-differing value. Unknown sizes/NULLs fail
/// with `ERROR_INVALID_PARAMETER`.
pub unsafe extern "win64" fn WaitOnAddress(
    addr: *const c_void,
    cmp: *const c_void,
    size: usize,
    timeout_ms: u32,
) -> i32 {
    let (Some(cur), Some(want)) = (addr_bytes(addr, size), addr_bytes(cmp, size)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if cur != want {
        // Already changed: return immediately, as on Windows.
        set_error(ERROR_SUCCESS);
        return 0;
    }
    let me = std::sync::Arc::new(AddrWaiter {
        signaled: Mutex::new(false),
        cvar: Condvar::new(),
    });
    let key = addr as usize;
    wait_addrs().lock().unwrap().entry(key).or_default().push(me.clone());
    // A write+wake that raced the registration must not be missed: re-check after
    // registering; a change now counts as a wake.
    if addr_bytes(addr, size).as_ref() != Some(&want) {
        unpark_addr_waiter(key, &me, true);
        set_error(ERROR_SUCCESS);
        return 1;
    }
    let mut g = me.signaled.lock().unwrap();
    if timeout_ms == INFINITE {
        while !*g {
            g = me.cvar.wait(g).unwrap();
        }
        set_error(ERROR_SUCCESS);
        1
    } else if timeout_ms == 0 {
        let woken = *g;
        drop(g);
        unpark_addr_waiter(key, &me, false);
        if woken {
            set_error(ERROR_SUCCESS);
            1
        } else {
            set_error(ERROR_TIMEOUT);
            0
        }
    } else {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        while !*g {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, timeout) = me.cvar.wait_timeout(g, deadline - now).unwrap();
            g = guard;
            if *g {
                break;
            }
            if timeout.timed_out() {
                break;
            }
        }
        let woken = *g;
        drop(g);
        unpark_addr_waiter(key, &me, false);
        if woken {
            set_error(ERROR_SUCCESS);
            1
        } else {
            // Re-check: a write that landed just before the timeout still counts.
            if addr_bytes(addr, size).as_ref() != Some(&want) {
                set_error(ERROR_SUCCESS);
                1
            } else {
                set_error(ERROR_TIMEOUT);
                0
            }
        }
    }
}

fn unpark_addr_waiter(key: usize, me: &std::sync::Arc<AddrWaiter>, signal: bool) {
    if signal {
        *me.signaled.lock().unwrap() = true;
        me.cvar.notify_one();
    }
    let mut g = wait_addrs().lock().unwrap();
    if let Some(v) = g.get_mut(&key) {
        v.retain(|w| !std::sync::Arc::ptr_eq(w, me));
        if v.is_empty() {
            g.remove(&key);
        }
    }
}

/// `VOID WakeByAddressSingle(PVOID)` — wakes one waiter on the address, if any.
pub unsafe extern "win64" fn WakeByAddressSingle(addr: *const c_void) {
    if addr.is_null() {
        return;
    }
    let key = addr as usize;
    let one = wait_addrs().lock().unwrap().get(&key).and_then(|v| v.first().cloned());
    if let Some(w) = one {
        unpark_addr_waiter(key, &w, true);
    }
}

/// `VOID WakeByAddressAll(PVOID)` — wakes every waiter on the address, if any.
pub unsafe extern "win64" fn WakeByAddressAll(addr: *const c_void) {
    if addr.is_null() {
        return;
    }
    let key = addr as usize;
    let all = wait_addrs().lock().unwrap().remove(&key).unwrap_or_default();
    for w in all {
        *w.signaled.lock().unwrap() = true;
        w.cvar.notify_one();
    }
}

pub unsafe extern "win64" fn CloseHandle(handle: usize) -> i32 {
    if events().lock().unwrap().remove(&handle).is_some() {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if close_sem_handle(handle) {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if close_mutex_handle(handle) {
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if close_timer_handle(handle) {
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

/// `void GetSystemTimePreciseAsFileTime(LPFILETIME)` — 100 ns ticks since 1601-01-01
/// from the host realtime clock. `clock_gettime` already yields nanosecond resolution,
/// so this is the precise variant's precision class; NULL is a safe no-op (the
/// auto-stub never wrote the out-param, leaving callers reading stale stack).
pub unsafe extern "win64" fn GetSystemTimePreciseAsFileTime(out: *mut FileTime) {
    GetSystemTimeAsFileTime(out);
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
// Processor topology + system info (retail `GetLogicalProcessorInformation` path).
// ---------------------------------------------------------------------------------------
//
// The game's startup validation calls `GetLogicalProcessorInformation` through the
// sysinfo API set and executes a deliberate `int3` when it returns FALSE (the stub
// did). These shims report the host topology the process actually runs on (through
// the same `process_affinity` the affinity calls fit to): one `RelationProcessorCore`
// entry per online processor plus a single `RelationProcessorPackage` entry, and a
// `SYSTEM_INFO` with AMD64 architecture, 4 KiB pages and 64 KiB granularity.
// `GetLogicalProcessorInformation` follows the Windows size-probe contract (NULL or
// short buffer → FALSE with `ERROR_INSUFFICIENT_BUFFER`, required size out).
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

/// `SYSTEM_INFO` (48 bytes on x86-64). Field order matches the Windows definition.
#[repr(C)]
pub struct SystemInfo {
    pub arch: u16,
    pub reserved: u16,
    pub page: u32,
    pub min_app: usize,
    pub max_app: usize,
    pub active_mask: usize,
    pub num_processors: u32,
    pub processor_type: u32,
    pub allocation_granularity: u32,
    pub processor_level: u16,
    pub processor_revision: u16,
}

fn system_info_for() -> SystemInfo {
    let allowed = process_affinity();
    SystemInfo {
        arch: 9, // PROCESSOR_ARCHITECTURE_AMD64
        reserved: 0,
        page: page_size() as u32,
        min_app: 0x1_0000,
        max_app: 0x7FFF_FFFE_FFFF,
        active_mask: allowed,
        num_processors: allowed.count_ones(),
        processor_type: 8664, // PROCESSOR_AMD_X8664
        allocation_granularity: 0x1_0000,
        processor_level: 6,
        processor_revision: 0,
    }
}

/// `VOID GetSystemInfo(LPSYSTEM_INFO)` — always succeeds.
pub unsafe extern "win64" fn GetSystemInfo(out: *mut SystemInfo) {
    if out.is_null() {
        return;
    }
    *out = system_info_for();
}

/// `VOID GetNativeSystemInfo(LPSYSTEM_INFO)` — same answers (never WOW64 here).
pub unsafe extern "win64" fn GetNativeSystemInfo(out: *mut SystemInfo) {
    GetSystemInfo(out)
}

const RELATION_PROCESSOR_CORE: u32 = 0;
#[allow(dead_code)]
const RELATION_NUMA_NODE: u32 = 1;
#[allow(dead_code)]
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;

/// One `SYSTEM_LOGICAL_PROCESSOR_INFORMATION` (32 bytes on x86-64).
fn write_processor_info_entry(buf: *mut u8, mask: usize, relation: u32) {
    unsafe {
        (buf as *mut usize).write_unaligned(mask);
        (buf.add(8) as *mut u32).write_unaligned(relation);
        (buf.add(12) as *mut u32).write_unaligned(0);
        // The 16-byte union: zero except `ProcessorCore.Flags` (1 = hyperthreaded
        // when the package holds more logical processors than core entries).
        std::ptr::write_bytes(buf.add(16), 0, 16);
    }
}

/// `BOOL GetLogicalProcessorInformation(PSYSTEM_LOGICAL_PROCESSOR_INFORMATION, PDWORD)` —
/// one core entry per online processor plus one package entry. Size probes follow the
/// Windows contract; the entries' masks are the process affinity (a zero affinity —
/// the host refused the query — fails with `ERROR_INVALID_PARAMETER`).
pub unsafe extern "win64" fn GetLogicalProcessorInformation(
    out: *mut u8,
    ret_len: *mut u32,
) -> i32 {
    if ret_len.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let allowed = process_affinity();
    if allowed == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let cpus: Vec<u32> = (0..usize::BITS).filter(|b| allowed >> b & 1 != 0).collect();
    let need = ((cpus.len() + 1) * 32) as u32;
    if out.is_null() || *ret_len < need {
        *ret_len = need;
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    let hyperthreaded = cpus.len() > 1;
    for (i, &cpu) in cpus.iter().enumerate() {
        let entry = out.add(i * 32);
        write_processor_info_entry(entry, 1usize << cpu, RELATION_PROCESSOR_CORE);
        if hyperthreaded {
            unsafe {
                (entry.add(16) as *mut u8).write_unaligned(1u8);
            }
        }
    }
    write_processor_info_entry(out.add(cpus.len() * 32), allowed, RELATION_PROCESSOR_PACKAGE);
    *ret_len = need;
    set_error(ERROR_SUCCESS);
    1
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

// ---------------------------------------------------------------------------------------
// XMem heap + graphics memory + physical pages (Phase C step 11j)
// ---------------------------------------------------------------------------------------
//
// The Windows twin serves heap types from an aligned `HeapAlloc` and graphics types
// from the sparse-VRAM pool / UMA. Linux has neither `HeapAlloc` alignment nor UMA
// (`gpu::uma` is `cfg(windows)`), so this slice provides the same title-visible
// contract with host means:
//
// * heap (`memory_type == 0`): `libc` memory, aligned to `1 << alignment`, zeroed.
//   The raw pointer hides 8 bytes below the aligned block; sizes live in
//   `XMEM_HEAP_SIZES` so unknown pointers fail cleanly instead of aborting libc.
// * graphics (`memory_type != 0`): a process-lifetime high-VA pool (one 20 GiB
//   `mmap` at/above 16 GiB, `PROT_NONE`) with a first-fit free list, 64 KiB grain
//   (4 MiB when `page_size == 2`), alignment from the attrs, zeroed on commit,
//   coalescing on free so freed blocks merge and are reused at the same address.
// * physical pages (64 KiB): `malloc`-backed homes plus page numbers counting
//   4 KiB frames (as on the console); mapping commits graphics-VA copies and moves
//   data with the page, unmapping (null pages) copies back home and decommits.
//   Freed pages keep their last VA contents; the mapping goes.
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

/// `XALLOC_ATTRIBUTES` bit fields (matches the Windows twin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XAllocAttrs {
    pub object_type: u32,
    pub page_size: u32,
    pub allocator_id: u32,
    pub alignment: u32,
    pub memory_type: u32,
}

impl XAllocAttrs {
    pub fn decode(a: u64) -> Self {
        XAllocAttrs {
            object_type: (a & 0x3FFF) as u32,
            page_size: ((a >> 14) & 0x3) as u32,
            allocator_id: ((a >> 16) & 0xFF) as u32,
            alignment: ((a >> 24) & 0x1F) as u32,
            memory_type: ((a >> 29) & 0xF) as u32,
        }
    }
}

const GFX_GRAIN: usize = 0x1_0000;
/// 20 GiB: the title's 8 GiB+ graphics reservation first-fits at the base, and a
/// later window-0 range ([base+16 GiB, base+20 GiB)) stays free for descriptor-heap
/// metadata, which must stay in window 0 past the reservation.
const GFX_POOL: usize = 20 << 30;

struct GfxState {
    base: usize,
    size: usize,
    free: BTreeMap<usize, usize>,
    /// addr → (size, committed)
    allocs: BTreeMap<usize, (usize, bool)>,
}

static GFX: OnceLock<Mutex<GfxState>> = OnceLock::new();

fn gfx_state() -> Option<&'static Mutex<GfxState>> {
    GFX.get_or_init(|| {
        // High-VA pool so `addr >> 32 >= 4` holds, as titles require of graphics
        // memory. `MAP_FIXED_NOREPLACE` probes a few 16 GiB-aligned candidates;
        // the pool is `PROT_NONE` until sub-ranges commit.
        const MAP_FIXED_NOREPLACE: libc::c_int = 0x10_0000;
        let mut base = 0usize;
        for cand in [0x10_0000_0000usize, 0x20_0000_0000, 0x30_0000_0000, 0x40_0000_0000] {
            let p = unsafe {
                libc::mmap(
                    cand as *mut c_void,
                    GFX_POOL,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            if p != libc::MAP_FAILED {
                base = p as usize;
                break;
            }
        }
        if base == 0 {
            // Last resort: anywhere (low addresses fail the title's high-VA check,
            // but allocation still works and the failure is visible, not a null).
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    GFX_POOL,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                crate::xlog_warn!("XMem: graphics pool mmap failed; graphics allocations fall back to plain memory");
                return Mutex::new(GfxState {
                    base: 0,
                    size: 0,
                    free: BTreeMap::new(),
                    allocs: BTreeMap::new(),
                });
            }
            base = p as usize;
            crate::xlog_warn!("XMem: graphics pool at low {base:#x}; high-VA checks will fail");
        }
        let mut free = BTreeMap::new();
        free.insert(base, GFX_POOL);
        Mutex::new(GfxState {
            base,
            size: GFX_POOL,
            free,
            allocs: BTreeMap::new(),
        })
    });
    let g = GFX.get().unwrap().lock().unwrap();
    if g.base == 0 {
        drop(g);
        return None;
    }
    drop(g);
    Some(GFX.get().unwrap())
}

fn gfx_contains(addr: usize) -> bool {
    GFX.get().is_some_and(|m| {
        let g = m.lock().unwrap();
        g.base != 0 && addr >= g.base && addr < g.base + g.size
    })
}

/// Whether all of `[addr, addr + len)` is inside the graphics pool.
pub(crate) fn gfx_contains_range(addr: usize, len: usize) -> bool {
    len != 0
        && gfx_contains(addr)
        && addr.checked_add(len - 1).is_some_and(gfx_contains)
}

/// Whether every page of `[p, p + len)` lies in a readable `/proc/self/maps` run: the
/// Linux twin of `gpu::shader::readable` for graphics-pool memory (commit `mprotect`s
/// the range `PROT_READ | PROT_WRITE`). That function stays a refusal on Linux because
/// the crash handler calls it inside a signal context, where reading `/proc` would be
/// async-signal-unsafe; this one is only used off that path (descriptor-heap creation and
/// console descriptor writes from title pointers).
pub(crate) fn gfx_range_readable(p: usize, len: usize) -> bool {
    if p == 0 || len == 0 || len > 256 << 20 {
        return false;
    }
    let Some(end) = p.checked_add(len) else { return false };
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else { return false };
    let mut runs: Vec<(usize, usize, bool)> = Vec::new();
    for line in maps.lines() {
        let mut it = line.split_whitespace();
        let (Some(range), Some(perms)) = (it.next(), it.next()) else { continue };
        let Some((a, b)) = range.split_once('-') else { continue };
        let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else {
            continue;
        };
        if b > p && a < end {
            runs.push((a.max(p), b.min(end), perms.starts_with('r')));
        }
    }
    runs.sort_unstable_by_key(|r| r.0);
    let mut cur = p;
    for (a, b, readable) in runs {
        if a > cur {
            return false;
        }
        if b <= cur {
            continue;
        }
        if !readable {
            return false;
        }
        cur = b;
        if cur >= end {
            return true;
        }
    }
    false
}

/// First-fit `size` bytes at `align` inside the graphics pool. Committed ranges are
/// `mprotect`ed `READWRITE` and zeroed; reserved ranges stay `PROT_NONE`.
pub(crate) fn gfx_alloc(size: usize, align: usize, commit: bool) -> Option<usize> {
    gfx_alloc_where(size, align, commit, |at, _| Some(at))
}

/// First-fit `size` bytes at `align` inside the graphics pool whose whole range sits
/// in unified-memory window 0 (`(addr >> 32) & 3 == 0` with `addr >> 32 >= 4`):
/// console descriptor-heap starts must stay there even after the title's 8 GiB+
/// reservation takes the pool's low window-0 range, so heaps skip ahead within the
/// same free range to the next window-0 window instead of landing past it.
pub(crate) fn gfx_alloc_window0(size: usize, align: usize, commit: bool) -> Option<usize> {
    gfx_alloc_where(size, align, commit, |at, end| {
        // jumps are multiples of 4 GiB, which every real alignment (a power of two
        // at or below 4 GiB) divides, so `at` stays aligned throughout.
        let mut c = at;
        loop {
            let hi = c >> 32;
            if hi >= 4 && hi & 3 == 0 {
                if c.checked_add(size).is_some_and(|e| e <= end) && (c + size - 1) >> 32 == hi {
                    return Some(c);
                }
                // no room left in this window: continue past its end
                c = hi.checked_add(1)?.checked_shl(32)? as usize;
            } else if hi < 4 {
                c = 4 << 32;
            } else {
                c = hi.checked_add(4 - (hi & 3))?.checked_shl(32)? as usize;
            }
            if c >= end {
                return None;
            }
        }
    })
}

/// First-fit `size` bytes at `align` inside the graphics pool whose whole range sits
/// in unified-memory window 3 (`(addr >> 32) & 3 == 3` with `addr >> 32 >= 4`):
/// the coherent room committed textures leave free, so small readback buffers made
/// after them still get coherent memory (the title's `committed-tex` leg).
pub(crate) fn gfx_alloc_window3(size: usize, align: usize, commit: bool) -> Option<usize> {
    gfx_alloc_where(size, align, commit, |at, end| {
        // jumps are multiples of 4 GiB, which every real alignment (a power of two
        // at or below 4 GiB) divides, so `at` stays aligned throughout.
        let mut c = at;
        loop {
            let hi = c >> 32;
            if hi >= 4 && hi & 3 == 3 {
                if c.checked_add(size).is_some_and(|e| e <= end) && (c + size - 1) >> 32 == hi {
                    return Some(c);
                }
                // no room left in this window: continue past its end
                c = hi.checked_add(1)?.checked_shl(32)? as usize;
            } else if hi < 4 {
                c = 4 << 32;
            } else {
                c = hi.checked_add((3 + 4 - (hi & 3)) % 4)?.checked_shl(32)? as usize;
            }
            if c >= end {
                return None;
            }
        }
    })
}

/// First-fit `size` bytes at `align` inside the graphics pool whose whole range
/// avoids unified-memory window 3 (single-window containment, like UMA: no
/// allocation straddles windows): committed textures never eat the coherent
/// readback room. `None` when no such range is free — the texture is then a
/// plain PC resource, like the Windows slice past `COHERENT_KEPT`.
pub(crate) fn gfx_alloc_avoid_window3(size: usize, align: usize, commit: bool) -> Option<usize> {
    gfx_alloc_where(size, align, commit, |at, end| {
        let mut c = at;
        loop {
            let hi = c >> 32;
            if hi >= 4
                && hi & 3 != 3
                && c.checked_add(size).is_some_and(|e| e <= end)
                && (c + size - 1) >> 32 == hi
            {
                return Some(c);
            }
            if hi < 4 {
                c = 4 << 32;
            } else {
                // window 3, or no room / straddles the window end: past this window
                c = hi.checked_add(1)?.checked_shl(32)? as usize;
            }
            if c >= end {
                return None;
            }
        }
    })
}

fn gfx_alloc_where(
    size: usize,
    align: usize,
    commit: bool,
    place: impl Fn(usize, usize) -> Option<usize>,
) -> Option<usize> {
    let m = gfx_state()?;
    let mut g = m.lock().unwrap();
    let align = align.max(GFX_GRAIN).next_power_of_two();
    let size = size.max(1).div_ceil(GFX_GRAIN) * GFX_GRAIN;
    let mut pick: Option<(usize, usize, usize)> = None; // (free_base, free_len, aligned_at)
    for (&b, &n) in g.free.iter() {
        let end = b + n;
        let at = b.next_multiple_of(align);
        // `place` may move the candidate forward inside the same range (the window-0
        // sub-range starts later in the range); it re-checks room itself.
        if at.checked_add(size).is_some_and(|e| e <= end) {
            if let Some(ok) = place(at, end) {
                pick = Some((b, n, ok));
                break;
            }
        }
    }
    let (b, n, at) = pick?;
    g.free.remove(&b);
    let pad = at - b;
    if pad > 0 {
        g.free.insert(b, pad);
    }
    let tail = n - pad - size;
    if tail > 0 {
        g.free.insert(at + size, tail);
    }
    g.allocs.insert(at, (size, commit));
    drop(g);
    if commit {
        let m = gfx_state()?;
        let _ = m; // lock ordering: mprotect outside the table lock
        if unsafe { libc::mprotect(at as *mut c_void, size, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
            let mut g = m.lock().unwrap();
            g.allocs.remove(&at);
            g.free.insert(at, size);
            // coalesce with neighbours
            let mut cur = at;
            let mut len = size;
            if let Some((&pb, &pn)) = g.free.range(..cur).next_back() {
                if pb + pn == cur {
                    g.free.remove(&pb);
                    cur = pb;
                    len += pn;
                }
            }
            if let Some(&nn) = g.free.get(&(cur + len)) {
                g.free.remove(&(cur + len));
                len += nn;
            }
            g.free.insert(cur, len);
            return None;
        }
        unsafe { std::ptr::write_bytes(at as *mut u8, 0, size) };
        crate::memstat::note(crate::memstat::GRAPHICS, size);
    }
    Some(at)
}

fn gfx_coalesce_locked(g: &mut GfxState, mut base: usize, mut len: usize) {
    if let Some((&pb, &pn)) = g.free.range(..base).next_back() {
        if pb + pn == base {
            g.free.remove(&pb);
            base = pb;
            len += pn;
        }
    }
    if let Some(&nn) = g.free.get(&(base + len)) {
        g.free.remove(&(base + len));
        len += nn;
    }
    g.free.insert(base, len);
}

/// Release a graphics allocation back to the pool (decommitted, reusable).
fn gfx_free(addr: usize) -> bool {
    let Some(m) = gfx_state() else { return false };
    let mut g = m.lock().unwrap();
    let Some((size, _)) = g.allocs.remove(&addr) else { return false };
    let (size, addr) = (size, addr);
    drop(g);
    unsafe {
        libc::mprotect(addr as *mut c_void, size, libc::PROT_NONE);
        libc::madvise(addr as *mut c_void, size, libc::MADV_DONTNEED);
    }
    let Some(m) = gfx_state() else { return false };
    let mut g = m.lock().unwrap();
    gfx_coalesce_locked(&mut g, addr, size);
    true
}

/// Size of the graphics allocation based at `addr`, if `addr` is an allocation base.
fn gfx_allocation_size(base: usize) -> Option<usize> {
    let m = gfx_state()?;
    m.lock().unwrap().allocs.get(&base).map(|&(n, _)| n)
}

/// Commit `len` bytes at `at` inside an existing graphics allocation (no zeroing;
// the caller copies data or zeroes explicitly).
fn gfx_commit_at(at: usize, len: usize) -> bool {
    if unsafe { libc::mprotect(at as *mut c_void, len, libc::PROT_READ | libc::PROT_WRITE) } != 0 {
        return false;
    }
    // Mark the whole allocation committed when it lies inside one.
    if let Some(m) = GFX.get() {
        let mut g = m.lock().unwrap();
        if let Some((&b, &mut (n, ref mut c))) = g.allocs.range_mut(..=at).next_back() {
            if at + len <= b + n {
                *c = true;
            }
        }
    }
    true
}

fn gfx_decommit_at(at: usize, len: usize) {
    unsafe {
        libc::madvise(at as *mut c_void, len, libc::MADV_DONTNEED);
        libc::mprotect(at as *mut c_void, len, libc::PROT_NONE);
    }
}

/// Whether `[addr, addr + len)` lies inside one live plain `VirtualAlloc`
/// reservation. `XMemVirtualAlloc` with `memory_type == 0` takes that path (the
/// Windows twin too), and `XMemMapPhysicalPages` must accept the reserved VA.
fn plain_reservation_contains(addr: usize, len: usize) -> bool {
    let Some(end) = addr.checked_add(len) else { return false };
    let a = allocations().lock().unwrap();
    let Some((&base, &(size, _))) = a.range(..=addr).next_back() else { return false };
    end <= base.saturating_add(size)
}

/// Commit one physical page's VA in the graphics pool or a plain reservation.
fn commit_physical_page(at: usize) -> bool {
    if gfx_contains(at) && gfx_contains(at + PHYS_PAGE - 1) {
        return gfx_commit_at(at, PHYS_PAGE);
    }
    if !plain_reservation_contains(at, PHYS_PAGE) {
        return false;
    }
    unsafe { libc::mprotect(at as *mut c_void, PHYS_PAGE, libc::PROT_READ | libc::PROT_WRITE) == 0 }
}

static XMEM_HEAP_SIZES: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

fn xmem_heap_sizes() -> &'static Mutex<HashMap<usize, usize>> {
    XMEM_HEAP_SIZES.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe fn xmem_heap_alloc(size: usize, align: usize) -> *mut c_void {
    let align = align.max(16).next_power_of_two();
    let Some(total) = size.max(1).checked_add(align).and_then(|n| n.checked_add(16)) else {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    let raw = libc::malloc(total) as usize;
    if raw == 0 {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    let aligned = (raw + 16 + align - 1) & !(align - 1);
    *((aligned - 8) as *mut usize) = raw;
    unsafe { std::ptr::write_bytes(aligned as *mut u8, 0, size) };
    xmem_heap_sizes().lock().unwrap().insert(aligned, size);
    set_error(ERROR_SUCCESS);
    aligned as *mut c_void
}

unsafe fn xmem_heap_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    if xmem_heap_sizes().lock().unwrap().remove(&(p as usize)).is_none() {
        set_error(ERROR_INVALID_PARAMETER);
        return;
    }
    let raw = *((p as usize - 8) as *const usize);
    libc::free(raw as *mut libc::c_void);
    set_error(ERROR_SUCCESS);
}

/// Rounded size + alignment for a graphics allocation honoring `page_size`
/// (`2` = 4 MiB pages, otherwise the 64 KiB grain) and `alignment`.
fn xmem_graphics_layout(size: usize, a: XAllocAttrs) -> Option<(usize, usize)> {
    let page_grain = if a.page_size == 2 { 4 << 20 } else { GFX_GRAIN };
    let align = (1usize << a.alignment.min(usize::BITS - 2)).max(page_grain);
    let rounded = size.max(1).div_ceil(page_grain) * page_grain;
    Some((rounded, align))
}

pub unsafe extern "win64" fn XMemAllocDefault(size: usize, attrs: u64) -> *mut c_void {
    let a = XAllocAttrs::decode(attrs);
    if a.memory_type == 0 {
        let q = xmem_heap_alloc(size, 1usize << a.alignment.clamp(4, usize::BITS - 2));
        if !q.is_null() {
            crate::memstat::note(crate::memstat::HEAP, size);
        }
        return q;
    }
    let Some((rounded, align)) = xmem_graphics_layout(size, a) else {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    match gfx_alloc(rounded, align, true) {
        Some(at) => {
            set_error(ERROR_SUCCESS);
            at as *mut c_void
        }
        None => {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            std::ptr::null_mut()
        }
    }
}

pub unsafe extern "win64" fn XMemFreeDefault(p: *mut c_void, attrs: u64) {
    if p.is_null() {
        return;
    }
    if XAllocAttrs::decode(attrs).memory_type == 0 {
        xmem_heap_free(p);
    } else if !gfx_free(p as usize) {
        set_error(ERROR_INVALID_PARAMETER);
    } else {
        set_error(ERROR_SUCCESS);
    }
}

type XMemAllocFn = unsafe extern "win64" fn(usize, u64) -> *mut c_void;
type XMemFreeFn = unsafe extern "win64" fn(*mut c_void, u64);

pub unsafe extern "win64" fn XMemAlloc(size: usize, attrs: u64) -> *mut c_void {
    match XMEM_ALLOC_HOOK.load(Ordering::Acquire) {
        0 => XMemAllocDefault(size, attrs),
        f => {
            let g: XMemAllocFn = std::mem::transmute(f);
            g(size, attrs)
        }
    }
}

pub unsafe extern "win64" fn XMemFree(p: *mut c_void, attrs: u64) {
    match XMEM_FREE_HOOK.load(Ordering::Acquire) {
        0 => XMemFreeDefault(p, attrs),
        f => {
            let g: XMemFreeFn = std::mem::transmute(f);
            g(p, attrs)
        }
    }
}

/// The exe's exported data `__XMemAllocationHooks__` {alloc, free, …}: the allocator functions
/// of a title that replaces XMem's, which the console's runtime libraries call for the memory
/// they need (`runtime_alloc`). Titles see there what the runtime allocates – and may supply the
/// memory themselves. Linux twin of `hle/kernelx.rs`; the retail exe exports the table at RVA
/// `0x6844380` with `{0x144deb420, 0x144e38ec0}` (`.data`).
static TITLE_HOOKS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

pub unsafe fn adopt_title_hooks(main: &crate::modules::MappedModule) {
    let Some(crate::pe::ExportTarget::Rva(rva)) = main.exports.by_name.get("__XMemAllocationHooks__") else { return };
    let p = (main.base + *rva as usize) as *const usize;
    let (alloc, free) = (p.read_unaligned(), p.add(1).read_unaligned());
    let inside = |f: usize| f > main.base && f < main.base + main.size;
    if inside(alloc) && inside(free) {
        TITLE_HOOKS[1].store(free, Ordering::Release);
        TITLE_HOOKS[0].store(alloc, Ordering::Release);
        crate::xlog_info!(
            "XMem: the title has allocation hooks ({}, {})",
            crate::modules::describe_address(alloc),
            crate::modules::describe_address(free)
        );
    }
}

/// XALLOC_ATTRIBUTES of a descriptor heap's memory: object type 4, 64 KiB pages and alignment,
/// allocator 0x80 (the D3D runtime), graphics memory. The retail game's heap class (its alloc hook for
/// allocator 0x80 compares object type and size) gives the memory of its heaps this way and
/// asserts that they start there.
pub const D3D_DESCRIPTOR_HEAP: u64 = 4 | 1 << 14 | 0x80 << 16 | 16 << 24 | 1 << 29;

/// XALLOC_ATTRIBUTES of a display target's memory: 4 MiB pages (the retail game's hook for its back
/// buffers takes every request with them), allocator 0x80, graphics memory. The bits 0xc000 = 0x8000
/// are the event its registered region bumps on: the retail title hands its back buffers one block
/// and asserts that the block is used up.
pub const D3D_DISPLAY_TARGET: u64 = 2 << 14 | 0x80 << 16 | 16 << 24 | 1 << 29;

/// What the title's allocation hook gives a console runtime library (None: no hook, or null).
pub unsafe fn runtime_alloc(size: usize, attrs: u64) -> Option<usize> {
    let f = TITLE_HOOKS[0].load(Ordering::Acquire);
    if f == 0 {
        return None;
    }
    let p = std::mem::transmute::<usize, XMemAllocFn>(f)(size, attrs) as usize;
    (p != 0).then_some(p)
}

pub unsafe fn runtime_free(p: usize, attrs: u64) {
    let f = TITLE_HOOKS[1].load(Ordering::Acquire);
    if f != 0 {
        std::mem::transmute::<usize, XMemFreeFn>(f)(p as *mut c_void, attrs);
    }
}

/// Memory for a descriptor heap of `bytes` from the title's allocation hook – where the title
/// expects the heap then. Only a range descriptor tables can address (the graphics pool) that
/// is committed and readable is accepted; anything else goes back to the title.
pub(crate) unsafe fn title_descriptor_heap_memory(bytes: usize) -> Option<usize> {
    let Some(p) = runtime_alloc(bytes, D3D_DESCRIPTOR_HEAP) else {
        static NONE: AtomicU32 = AtomicU32::new(0);
        if NONE.fetch_add(1, Ordering::Relaxed) < 8 {
            crate::xlog_info!("console descriptor heap: the title's allocator gave no memory for {bytes:#x} bytes");
        }
        return None;
    };
    let in_pool = gfx_contains_range(p, bytes);
    if in_pool && gfx_range_readable(p, bytes) {
        static OK: AtomicU32 = AtomicU32::new(0);
        if OK.fetch_add(1, Ordering::Relaxed) < 8 {
            crate::xlog_info!("console descriptor heap: {bytes:#x} bytes from the title's memory at {p:#x}");
        }
        return Some(p);
    }
    static N: AtomicU32 = AtomicU32::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 8 {
        crate::xlog_warn!(
            "console descriptor heap: the title's memory {p:#x} ({bytes:#x} bytes) not used (in the pool {in_pool}, readable {})",
            gfx_range_readable(p, bytes)
        );
    }
    runtime_free(p, D3D_DESCRIPTOR_HEAP);
    None
}

/// Test seam: `Device_CreateDescriptorHeap` can be driven with a synthetic title allocator.
/// `alloc == 0` clears both hooks.
#[cfg(test)]
pub(crate) unsafe fn set_title_hooks_for_test(alloc: usize, free: usize) {
    TITLE_HOOKS[1].store(free, Ordering::Release);
    TITLE_HOOKS[0].store(alloc, Ordering::Release);
}

/// `void* XMemVirtualAlloc(void*, SIZE_T, DWORD, XALLOC_ATTRIBUTES, DWORD)`:
/// graphics types come from the high-VA pool (reserve or commit), everything else
/// from the plain `VirtualAlloc` path.
pub unsafe extern "win64" fn XMemVirtualAlloc(
    addr: *const c_void,
    size: usize,
    t: u32,
    attrs: u64,
    p: u32,
) -> *mut c_void {
    const MEM_GRAPHICS: u32 = 0x1000_0000;
    let a = XAllocAttrs::decode(attrs);
    if a.memory_type == 0 {
        return VirtualAlloc(addr, size, t, p);
    }
    let mut t = t | MEM_GRAPHICS;
    if a.memory_type != 0 && a.page_size == 2 {
        t |= 0x8000_0000;
    }
    let page_grain = if a.page_size == 2 { 4 << 20 } else { GFX_GRAIN };
    let align = (1usize << a.alignment.min(usize::BITS - 2)).max(page_grain);
    if !addr.is_null() {
        // Fixed-address commit inside a graphics reservation (physical-page maps take
        // this path through `VirtualAlloc`-equivalent semantics on the pool).
        let base = addr as usize;
        if base % GFX_GRAIN != 0 || size == 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        if !gfx_contains(base) || !gfx_contains(base + size - 1) {
            set_error(ERROR_INVALID_ADDRESS);
            return std::ptr::null_mut();
        }
        if t & MEM_COMMIT != 0 {
            if !gfx_commit_at(base, size.div_ceil(GFX_GRAIN) * GFX_GRAIN) {
                set_error(ERROR_INVALID_ADDRESS);
                return std::ptr::null_mut();
            }
            unsafe { std::ptr::write_bytes(base as *mut u8, 0, size.div_ceil(GFX_GRAIN) * GFX_GRAIN) };
        }
        set_error(ERROR_SUCCESS);
        return base as *mut c_void;
    }
    let commit = t & MEM_COMMIT != 0;
    let reserve_only = !commit && t & MEM_RESERVE != 0;
    let rounded = size.max(1).div_ceil(page_grain) * page_grain;
    // A bare reserve (no commit) still needs 64 KiB granularity for the page maps.
    let rounded = if reserve_only { size.max(1).div_ceil(GFX_GRAIN) * GFX_GRAIN } else { rounded };
    match gfx_alloc(rounded, align, commit) {
        Some(at) => {
            set_error(ERROR_SUCCESS);
            at as *mut c_void
        }
        None => {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            std::ptr::null_mut()
        }
    }
}

// --- Physical pages (64 KiB) ---

const PHYS_PAGE: usize = 0x1_0000;
/// A page number counts 4 KiB frames, as on the console (64 KiB = 16 frames).
const FRAMES: usize = PHYS_PAGE >> 12;

fn page_number(index: usize) -> usize {
    (index + 1) * FRAMES
}

fn page_index(page: usize) -> Option<usize> {
    page.is_multiple_of(FRAMES).then(|| (page / FRAMES).checked_sub(1)).flatten()
}

struct PhysicalBlock {
    count: usize,
    live: usize,
    home: usize,
    freed: Vec<usize>,
}

impl PhysicalBlock {
    const WORD_BITS: usize = usize::BITS as usize;

    fn new(count: usize, home: usize) -> Self {
        Self { count, live: count, home, freed: vec![0; count.div_ceil(Self::WORD_BITS)] }
    }

    fn is_live(&self, offset: usize) -> bool {
        offset < self.count && self.freed[offset / Self::WORD_BITS] & (1 << (offset % Self::WORD_BITS)) == 0
    }

    fn free_page(&mut self, offset: usize) -> bool {
        if !self.is_live(offset) {
            return false;
        }
        self.freed[offset / Self::WORD_BITS] |= 1 << (offset % Self::WORD_BITS);
        self.live -= 1;
        true
    }
}

/// first page index → its physical page allocation
static PHYS_BLOCKS: OnceLock<Mutex<BTreeMap<usize, PhysicalBlock>>> = OnceLock::new();

fn phys_blocks() -> &'static Mutex<BTreeMap<usize, PhysicalBlock>> {
    PHYS_BLOCKS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

static NEXT_PAGE_INDEX: AtomicUsize = AtomicUsize::new(0);

/// Addresses holding a page away from home: address → page, and page → address.
#[derive(Default)]
struct Away {
    at: BTreeMap<usize, usize>,
    page: HashMap<usize, usize>,
}

static AWAY: OnceLock<Mutex<Option<Away>>> = OnceLock::new();

fn away() -> &'static Mutex<Option<Away>> {
    AWAY.get_or_init(|| Mutex::new(None))
}

fn page_home(page: usize) -> Option<usize> {
    let index = page_index(page)?;
    let blocks = phys_blocks().lock().ok()?;
    let (&first, block) = blocks.range(..=index).next_back()?;
    let offset = index - first;
    block.is_live(offset).then(|| block.home.checked_add(offset.checked_mul(PHYS_PAGE)?)).flatten()
}

/// The contiguous home address of `pages` when they are consecutive pages of one live allocation.
fn contiguous_home_in(blocks: &std::collections::BTreeMap<usize, PhysicalBlock>, pages: &[usize]) -> Option<usize> {
    let first_page = *pages.first()?;
    if first_page == 0 || !(1..pages.len()).all(|i| i.checked_mul(FRAMES).and_then(|d| first_page.checked_add(d)) == Some(pages[i])) {
        return None;
    }
    let index = page_index(first_page)?;
    let end = index.checked_add(pages.len())?;
    let (&first, block) = blocks.range(..=index).next_back()?;
    if end > first.checked_add(block.count)? {
        return None;
    }
    // Every page in the span must still be allocated: a freed middle page
    // breaks the contiguous home even though the block itself is alive.
    if !(0..pages.len()).all(|i| block.is_live(index - first + i)) {
        return None;
    }
    block.home.checked_add((index - first).checked_mul(PHYS_PAGE)?)
}

fn contiguous_home(pages: &[usize]) -> Option<usize> {
    let blocks = phys_blocks().lock().ok()?;
    contiguous_home_in(&blocks, pages)
}

/// Copy home → `dst` for a live page (homes are always committed `malloc` memory).
unsafe fn copy_home_to(page: usize, dst: usize) -> bool {
    let Some(home) = page_home(page) else { return false };
    unsafe { std::ptr::copy_nonoverlapping(home as *const u8, dst as *mut u8, PHYS_PAGE) };
    true
}

/// Resolve the live virtual addresses of the physical pages covering
/// `offset..offset+len`. Caller must hold `AWAY` locked.
fn resolve_physical_pages(
    away: Option<&Away>,
    pages: &[usize],
    offset: usize,
    len: usize,
) -> Option<Vec<usize>> {
    if offset >= PHYS_PAGE {
        return None;
    }
    let span = offset.checked_add(len)?;
    let need = span.div_ceil(PHYS_PAGE);
    if pages.len() < need {
        return None;
    }
    let mut addrs = Vec::with_capacity(need);
    for &p in &pages[..need] {
        let home = page_home(p)?;
        let addr = match away.and_then(|a| a.page.get(&p).copied()) {
            Some(at) => at,
            None => home,
        };
        addrs.push(addr);
    }
    Some(addrs)
}

/// Copy `dst.len()` bytes starting at `offset` within `pages` (each a 64 KiB physical
/// page number from `AllocateTitlePhysicalPages`). Returns `false` if
/// `offset >= PHYS_PAGE`, `pages` is too short, or any page is not a live allocation.
pub unsafe fn read_physical_pages(pages: &[usize], offset: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return offset < PHYS_PAGE;
    }
    let g = away().lock().unwrap();
    let Some(addrs) = resolve_physical_pages(g.as_ref(), pages, offset, dst.len()) else {
        return false;
    };
    let mut done = 0;
    let mut page_off = offset;
    for addr in addrs {
        let chunk = (PHYS_PAGE - page_off).min(dst.len() - done);
        unsafe {
            std::ptr::copy_nonoverlapping((addr + page_off) as *const u8, dst.as_mut_ptr().add(done), chunk);
        }
        done += chunk;
        page_off = 0;
    }
    true
}

/// Copy `src` into `pages` starting at `offset` within the first page. Validates every
/// page before writing so the destination gets all bytes or nothing.
pub unsafe fn write_physical_pages(pages: &[usize], offset: usize, src: &[u8]) -> bool {
    if src.is_empty() {
        return offset < PHYS_PAGE;
    }
    let g = away().lock().unwrap();
    let Some(addrs) = resolve_physical_pages(g.as_ref(), pages, offset, src.len()) else {
        return false;
    };
    let mut done = 0;
    let mut page_off = offset;
    for addr in addrs {
        let chunk = (PHYS_PAGE - page_off).min(src.len() - done);
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr().add(done), (addr + page_off) as *mut u8, chunk);
        }
        done += chunk;
        page_off = 0;
    }
    true
}

/// Drop every away-mapped physical page overlapping `[start, start + len)`: each
/// page's data belongs home again, then the address is free for the decommit or
/// release that follows. (There is no `note_mapping_at` counterpart: the queue's
/// page-mapping record has no Linux twin.)
unsafe fn unmap_away_in(away: &mut Away, start: usize, len: usize) {
    if len == 0 || away.at.is_empty() {
        return;
    }
    let end = start.saturating_add(len);
    let addrs: Vec<usize> = away.at.range((start & !(PHYS_PAGE - 1))..end).map(|(&at, _)| at).collect();
    for at in addrs {
        if let Some(p) = away.at.remove(&at) {
            away.page.remove(&p);
            if let Some(home) = page_home(p) {
                std::ptr::copy_nonoverlapping(at as *const u8, home as *mut u8, PHYS_PAGE);
            }
        }
    }
}

/// Unmap every away-mapped physical page overlapping `[start, start + len)`.
unsafe fn unmap_away_range(start: usize, len: usize) {
    if len > 0 {
        if let Some(away) = away().lock().unwrap().as_mut() {
            unmap_away_in(away, start, len);
        }
    }
}

/// Map `n` pages at `base` (`arr` null unmaps). Returns `base` on success, null on failure.
unsafe fn map_pages(base: usize, n: usize, pages: Option<Vec<usize>>) -> *mut c_void {
    if n == 0 || (base == 0 && pages.is_none()) {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    if base % PHYS_PAGE != 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let pages: Vec<usize> = match pages {
        Some(v) => v,
        None => vec![0; n],
    };
    if pages.len() < n {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    if pages.iter().any(|&p| p != 0 && page_home(p).is_none()) {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    // Every target page must be inside the graphics pool or a live plain
    // reservation when an address is given: `XMemVirtualAlloc` with
    // `memory_type == 0` reserves ordinary `VirtualAlloc` memory, and the title
    // maps physical pages into it (both twins' `XMemVirtualAlloc` take that path).
    if base != 0 {
        for i in 0..n {
            let at = base + i * PHYS_PAGE;
            if !gfx_contains(at) && !plain_reservation_contains(at, PHYS_PAGE) {
                set_error(ERROR_INVALID_ADDRESS);
                return std::ptr::null_mut();
            }
        }
    }
    let mut g = away().lock().unwrap();
    let st = g.get_or_insert_with(Away::default);
    for (i, &p) in pages.iter().enumerate().take(n) {
        let at = base + i * PHYS_PAGE;
        // Whatever was mapped at `at` goes first: its data belongs home again.
        if let Some(old) = st.at.remove(&at) {
            st.page.remove(&old);
            if let Some(home) = page_home(old) {
                unsafe {
                    std::ptr::copy_nonoverlapping(at as *const u8, home as *mut u8, PHYS_PAGE)
                };
            }
            gfx_decommit_at(at, PHYS_PAGE);
        }
        if p == 0 {
            continue;
        }
        // A page mapped elsewhere moves: old VA → new VA, old VA given up.
        if let Some(&old) = st.page.get(&p) {
            if old != at {
                if !commit_physical_page(at) {
                    set_error(ERROR_INVALID_ADDRESS);
                    return std::ptr::null_mut();
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(old as *const u8, at as *mut u8, PHYS_PAGE)
                };
                gfx_decommit_at(old, PHYS_PAGE);
                st.at.remove(&old);
                st.at.insert(at, p);
                st.page.insert(p, at);
                continue;
            } else {
                continue;
            }
        }
        if !commit_physical_page(at) {
            set_error(ERROR_INVALID_ADDRESS);
            return std::ptr::null_mut();
        }
        if !copy_home_to(p, at) {
            gfx_decommit_at(at, PHYS_PAGE);
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        st.at.insert(at, p);
        st.page.insert(p, at);
    }
    set_error(ERROR_SUCCESS);
    base as *mut c_void
}

pub unsafe extern "win64" fn AllocateTitlePhysicalPages(
    _proc: usize,
    _t: u32,
    num: *mut usize,
    arr: *mut usize,
) -> i32 {
    if num.is_null() || arr.is_null() || unsafe { *num } == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let n = unsafe { *num };
    let Some(bytes) = n.checked_mul(PHYS_PAGE) else {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return 0;
    };
    let home = unsafe { libc::calloc(1, bytes) } as usize;
    if home == 0 {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return 0;
    }
    let first = NEXT_PAGE_INDEX.fetch_add(n, Ordering::Relaxed);
    for i in 0..n {
        unsafe { *arr.add(i) = page_number(first + i) };
    }
    phys_blocks().lock().unwrap().insert(first, PhysicalBlock::new(n, home));
    crate::memstat::note(crate::memstat::PHYSICAL, bytes);
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn FreeTitlePhysicalPages(_proc: usize, n: usize, arr: *mut usize) -> i32 {
    if arr.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Use the same lock order as map and read (AWAY then blocks) so they
    // cannot see a partially freed page. Homes free after both locks drop.
    let mut away_guard = away().lock().unwrap();
    let mut blocks_guard = phys_blocks().lock().unwrap();
    let mut homes = Vec::new();
    for i in 0..n {
        let p = unsafe { *arr.add(i) };
        let Some(index) = page_index(p) else { continue };
        let Some((&first, block)) = blocks_guard.range_mut(..=index).next_back() else { continue };
        if !block.free_page(index - first) {
            continue;
        }
        // A mapped address keeps its contents after free, but no longer tracks this page.
        if let Some(st) = away_guard.as_mut() {
            if let Some(at) = st.page.remove(&p) {
                st.at.remove(&at);
            }
        }
        if block.live == 0 {
            homes.push(block.home);
            blocks_guard.remove(&first);
        }
    }
    drop(blocks_guard);
    drop(away_guard);
    for home in homes {
        unsafe { libc::free(home as *mut libc::c_void) };
    }
    set_error(ERROR_SUCCESS);
    1
}

pub unsafe extern "win64" fn MapTitlePhysicalPages(
    va: *mut c_void,
    n: usize,
    _t: u32,
    _p: u32,
    arr: *mut usize,
) -> *mut c_void {
    if va.is_null() {
        // No address chosen: fresh graphics memory holding the pages' homes.
        if arr.is_null() || n == 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        let pages: Vec<usize> = unsafe { (0..n).map(|i| *arr.add(i)).collect() };
        if pages.iter().any(|&p| p != 0 && page_home(p).is_none()) {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        let bytes = n * PHYS_PAGE;
        let Some(at) = gfx_alloc(bytes, GFX_GRAIN, true) else {
            set_error(ERROR_NOT_ENOUGH_MEMORY);
            return std::ptr::null_mut();
        };
        let mut g = away().lock().unwrap();
        let st = g.get_or_insert_with(Away::default);
        // Consecutive pages of one live allocation share one home block: copy it whole.
        // Anything else (holes, scattered runs, consecutive numbers spanning two
        // allocations) falls through to the per-page loop, so cross-allocation runs
        // never merge into one block over unrelated memory.
        if let Some(home) = contiguous_home(&pages) {
            if pages.iter().all(|p| !st.page.contains_key(p)) {
                unsafe {
                    std::ptr::copy_nonoverlapping(home as *const u8, at as *mut u8, bytes)
                };
                for (i, &p) in pages.iter().enumerate() {
                    st.at.insert(at + i * PHYS_PAGE, p);
                    st.page.insert(p, at + i * PHYS_PAGE);
                }
                set_error(ERROR_SUCCESS);
                return at as *mut c_void;
            }
        }
        for (i, &p) in pages.iter().enumerate() {
            if p == 0 {
                continue;
            }
            // A page mapped elsewhere moves without touching home.
            if let Some(&old) = st.page.get(&p) {
                let dst = at + i * PHYS_PAGE;
                unsafe {
                    std::ptr::copy_nonoverlapping(old as *const u8, dst as *mut u8, PHYS_PAGE)
                };
                gfx_decommit_at(old, PHYS_PAGE);
                st.at.remove(&old);
                st.at.insert(dst, p);
                st.page.insert(p, dst);
                continue;
            }
            if !copy_home_to(p, at + i * PHYS_PAGE) {
                // Roll back this mapping; the VA stays allocated (released below).
                for (j, &q) in pages.iter().enumerate().take(i) {
                    if q != 0 {
                        if let Some(a) = st.page.remove(&q) {
                            st.at.remove(&a);
                        }
                    }
                }
                gfx_free(at);
                set_error(ERROR_INVALID_PARAMETER);
                return std::ptr::null_mut();
            }
            st.at.insert(at + i * PHYS_PAGE, p);
            st.page.insert(p, at + i * PHYS_PAGE);
        }
        set_error(ERROR_SUCCESS);
        return at as *mut c_void;
    }
    let base = va as usize;
    let pages = if arr.is_null() { None } else { Some(unsafe { (0..n).map(|i| *arr.add(i)).collect() }) };
    map_pages(base, n, pages)
}

/// `BOOL XMemAllocatePhysicalPages(XALLOC_ATTRIBUTES, SIZE_T*, ULONG64*)`.
pub unsafe extern "win64" fn XMemAllocatePhysicalPages(
    _attrs: u64,
    num: *mut usize,
    arr: *mut u64,
) -> i32 {
    AllocateTitlePhysicalPages(0, 0, num, arr as *mut usize)
}

/// `void* XMemMapPhysicalPages(void*, SIZE_T, ULONG64*)`.
pub unsafe extern "win64" fn XMemMapPhysicalPages(
    va: *mut c_void,
    n: usize,
    arr: *mut u64,
) -> *mut c_void {
    MapTitlePhysicalPages(va, n, 0, 0, arr as *mut usize)
}

/// `BOOL XMemMapPhysicalPagesScatter(void**, SIZE_T, ULONG64*)`: one page per address
/// (page 0: the address is unmapped). No page array unmaps every address: a
/// retail game unmaps that way while it streams its world and stops on its own
/// `int3` when the call fails.
pub unsafe extern "win64" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *mut u64,
) -> i32 {
    if addresses.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    for i in 0..n {
        let at = unsafe { *addresses.add(i) } as usize;
        // No page array: page 0 (unmap) for every address.
        let p = unsafe {
            if pages.is_null() {
                0
            } else {
                *(pages.add(i) as *mut usize)
            }
        };
        let one = vec![p];
        if map_pages(at, 1, Some(one)).is_null() {
            return 0;
        }
    }
    set_error(ERROR_SUCCESS);
    1
}

/// `BOOL XMemFreePhysicalPages(SIZE_T, ULONG64*)`.
pub unsafe extern "win64" fn XMemFreePhysicalPages(n: usize, arr: *mut u64) -> i32 {
    FreeTitlePhysicalPages(0, n, arr as *mut usize)
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

/// `NTSTATUS LsaOpenPolicy(...)`: headless success with a dummy policy handle.
/// The title's composite check only probes `GetProcAddress(advapi32legacy, "LsaOpenPolicy")`
/// for non-null; a real callable address keeps that probe working without an LSA database.
pub unsafe extern "win64" fn LsaOpenPolicy(
    _system: *const c_void,
    _attrs: *const c_void,
    _access: u32,
    out: *mut usize,
) -> u32 {
    static POLICY: u8 = 0;
    if out.is_null() {
        return 0xC000_000D; // STATUS_INVALID_PARAMETER
    }
    *out = &POLICY as *const u8 as usize;
    crate::xlog_info!("kernelx: LsaOpenPolicy -> headless success");
    0 // STATUS_SUCCESS
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

/// The field at +48 of `XMEM_BASIC_INFORMATION` is 16 bytes; callers inspect more
/// than its low u32. Zero all of it, then write the flags we know: bit 0 is
/// CPU-cached/readable memory, bit 1 is GPU-accessible memory. Never leave
/// stack garbage in the rest of the field.
unsafe fn write_xmem_flags(info: *mut u8, flags: u32) {
    let p = info.add(48);
    std::ptr::write_bytes(p, 0, 16);
    p.cast::<u32>().write_unaligned(flags);
}

/// Whether the address is memory the title can use from the GPU: inside the
/// Linux graphics pool (the local counterpart of upstream's coherent UMA /
/// sparse-VRAM windows — `gpu::uma` is `cfg(windows)`-only, so its
/// `is_committed || is_title_address` helper has no Linux twin; the pool is
/// exactly the title's GPU-addressable reservation set, and plain host memory
/// that merely falls in a wide numeric window is never reported here).
fn xmem_gpu_visible(address: *const c_void) -> bool {
    gfx_contains(address as usize)
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
    // The caller reads a CPU-readable bit at +48 plus the GPU-visible bit 1;
    // only those and State/Protect matter. The 64-byte zero above already
    // covers the 16-byte flags field (a superset of upstream's 16-byte zero),
    // and `write_xmem_flags` re-zeroes +48..+64 before writing the flags.
    // No committed-window fast path on Linux: the general path already
    // reports 3 for committed graphics memory (`rw` maps + pool membership),
    // and the pool's commit flag is not cleared by scatter-unmap decommits,
    // so a cached fast path would go stale (upstream's 40 ms VirtualQuery
    // walk concern is a Windows-D3D12-heap artifact).
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
    // Zero the 64-byte caller buffer, then write State/Protect and the flags.
    std::ptr::write_bytes(info, 0, 64);
    // Offsets in the Windows struct (64-bit): State +16, Protect +20.
    (info.add(16) as *mut u32).write_unaligned(state);
    (info.add(20) as *mut u32).write_unaligned(protect);
    const READABLE: u32 = PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ
        | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    let cpu = (state == 0x1000 && (protect & PAGE_GUARD) == 0 && protect & READABLE != 0) as u32;
    let gpu = xmem_gpu_visible(address) as u32;
    write_xmem_flags(info, cpu | (gpu << 1));
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
// Delete-on-close handles (upstream #1038): file handle -> game-relative path for
// opens that asked for `FILE_FLAG_DELETE_ON_CLOSE` on a game-original file. The
// overlay copy is deleted when the last handle for the path closes; the whiteout
// recorded at open keeps the original hidden afterwards. Non-game paths never
// enter this map.
static DELETE_ON_CLOSE: OnceLock<Mutex<HashMap<usize, String>>> = OnceLock::new();

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
    if files().lock().unwrap().remove(&h).is_none() {
        return false;
    }
    // NOTE: bind before the `if let`: a `delete_on_close_handles().lock()` temporary
    // in the scrutinee would live through the whole `if let` body and deadlock on the
    // last-close re-lock of the same map below (`std` mutexes are not reentrant).
    let rel = delete_on_close_handles().lock().unwrap().remove(&h);
    if let Some(rel) = rel {
        // Last-close semantics: finalise once no other handle still names `rel`.
        // Other handles keep working (Linux unlinks open files lazily); lookups
        // already hide the original behind the whiteout recorded at open.
        if !delete_on_close_handles().lock().unwrap().values().any(|r| r == &rel) {
            if let Some(v) = crate::vfs::get() {
                // Best effort: drop the overlay copy so the recorded whiteout hides
                // the original; Windows deletes the file itself on close.
                let _ = v.delete_file(&rel);
            }
        }
    }
    // Upstream #1054: a disposition-delete mark finalises when the marking
    // handle closes (Windows deletes the file at close). Best effort: drop the
    // overlay copy so the whiteout recorded at set time hides the original.
    // NOTE: bind before use (same non-reentrant-mutex rule as above).
    let mark = handle_delete_marks().lock().unwrap().remove(&h);
    if let Some(mark) = mark {
        match (crate::vfs::get(), mark.rel) {
            (Some(v), Some(rel)) => {
                let _ = v.delete_file(&rel);
            }
            _ => {
                if !mark.host.is_empty() {
                    let _ = std::fs::remove_file(&mark.host);
                }
            }
        }
    }
    true
}

fn delete_on_close_handles() -> &'static Mutex<HashMap<usize, String>> {
    DELETE_ON_CLOSE.get_or_init(|| Mutex::new(HashMap::new()))
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
    // A mount root has no entry of its own: FindFirstFileW("G:\\") fails with
    // ERROR_FILE_NOT_FOUND like a native drive root (upstream #761: `rel.is_empty()`
    // in `collect_game_entries`; here `rel` arrives already split, so the empty
    // full rel is `dir_rel` and `pattern` both empty).
    if dir_rel.is_empty() && pattern.is_empty() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
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
    if out.is_null() {
        return Err(ERROR_INVALID_PARAMETER);
    }
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
    let h = register_file(opened.file);
    // Delete-on-close (upstream #1038): track the handle so the last close for the
    // game file deletes the overlay copy; the whiteout recorded at open keeps the
    // original hidden afterwards. Non-game paths never carry this marker.
    if let Some(rel) = opened.delete_on_close_rel {
        delete_on_close_handles().lock().unwrap().insert(h, rel);
    }
    Ok(h)
}

unsafe fn wide_str(p: *const u16) -> Option<String> {
    from_wide_ptr(p)
}

unsafe fn ansi_str(p: *const u8) -> Option<String> {
    from_ansi_ptr(p)
}

/// Crash-safe read of `out.len()` guest bytes at `addr` through
/// `pread(/proc/self/mem)`: the OS checks the caller-supplied guest address, so
/// an unmapped or unreadable range fails instead of faulting this process.
/// Same syscall-only discipline as `crate::eh`'s `copy_stack`/`code_at`.
fn guest_read_exact(addr: usize, out: &mut [u8]) -> bool {
    if out.is_empty() {
        return true;
    }
    const PATH: &[u8] = b"/proc/self/mem\0";
    let fd =
        unsafe { libc::open(PATH.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    let mut off = 0usize;
    while off < out.len() {
        let r = unsafe {
            libc::pread(
                fd,
                out.as_mut_ptr().add(off) as *mut libc::c_void,
                out.len() - off,
                addr.wrapping_add(off) as libc::off_t,
            )
        };
        if r == 0 {
            break;
        }
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        off += r as usize;
    }
    unsafe { libc::close(fd) };
    off == out.len()
}

/// Read-only fields covered by `CREATEFILE2_EXTENDED_PARAMETERS::dwSize`.
///
/// Linux twin of `vfs_hooks::create_file_2_flags` (upstream #1433): NULL means
/// flags 0; otherwise the guest `dwSize` must cover the 12-byte
/// `{ dwSize, dwFileAttributes, dwFileFlags }` prefix and the whole prefix must
/// be readable. `None` means a short or unreadable structure (Linux rejects
/// with `ERROR_INVALID_PARAMETER`; Windows passes the call through to orig).
fn create_file_2_flags(params: *const c_void) -> Option<u32> {
    if params.is_null() {
        return Some(0);
    }
    let addr = params as usize;
    let mut data = [0u8; 12];
    if !guest_read_exact(addr, &mut data[..4]) {
        return None;
    }
    if u32::from_le_bytes(data[..4].try_into().unwrap()) < 12 {
        return None;
    }
    // Re-read the full prefix, as upstream does, in case the caller unmapped
    // the source after the first read.
    if !guest_read_exact(addr, &mut data) {
        return None;
    }
    Some(u32::from_le_bytes(data[8..12].try_into().unwrap()))
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
    // CREATEFILE2_EXTENDED_PARAMETERS { dwSize, dwFileAttributes, dwFileFlags, ... }:
    // never read past what the caller sized (upstream #1433). NULL means flags 0;
    // a short or unreadable structure fails instead of reading unowned guest memory.
    let Some(flags) = create_file_2_flags(params) else {
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

/// Windows `OVERLAPPED` (winnt.h): the status block at +0/+8 and the
/// `Offset`/`OffsetHigh` union at +0x10; the event handle at +0x18 is not used
/// here (XCL completes I/O synchronously).
///
/// A handle opened without `FILE_FLAG_OVERLAPPED` still completes such an I/O
/// synchronously, but the explicit offset decides where the transfer starts and
/// the file position afterwards is `offset + transferred`.
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
}

/// Read `Offset`/`OffsetHigh` from a possibly 4-byte-aligned guest `OVERLAPPED`.
unsafe fn overlapped_offset(ov: *mut c_void) -> u64 {
    let o = unsafe { ov.cast::<Overlapped>().read_unaligned() };
    (o.offset as u64) | ((o.offset_high as u64) << 32)
}

/// Publish a synchronous completion: `Internal = STATUS_SUCCESS`,
/// `InternalHigh = transferred` (what the OS writes for a completed I/O).
unsafe fn overlapped_complete(ov: *mut c_void, transferred: u32) {
    let p = ov.cast::<Overlapped>();
    unsafe {
        std::ptr::addr_of_mut!((*p).internal).write_unaligned(0);
        std::ptr::addr_of_mut!((*p).internal_high).write_unaligned(transferred as usize);
    }
}

pub unsafe extern "win64" fn ReadFile(
    handle: usize,
    buf: *mut c_void,
    n: u32,
    done: *mut u32,
    ov: *mut c_void,
) -> i32 {
    if buf.is_null() && n != 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Console stdin: the pseudo handle from `GetStdHandle(STD_INPUT_HANDLE)` reads fd 0.
    if handle == std_handle_value(STD_INPUT_HANDLE) {
        if n == 0 {
            if !done.is_null() {
                *done = 0;
            }
            set_error(ERROR_SUCCESS);
            return 1;
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, n as usize) };
        let r = unsafe { libc::read(0, slice.as_mut_ptr() as *mut c_void, slice.len()) };
        if r < 0 {
            set_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(5) as u32);
            return 0;
        }
        if !done.is_null() {
            *done = r as u32;
        }
        set_error(ERROR_SUCCESS);
        return 1;
    }
    let arc = files().lock().unwrap().get(&handle).cloned();
    let Some(arc) = arc else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, n as usize) };
    let mut g = arc.lock().unwrap();
    if !ov.is_null() {
        // Explicit offset: start there instead of at the shared file position.
        if let Err(e) = std::io::Seek::seek(
            &mut *g,
            std::io::SeekFrom::Start(unsafe { overlapped_offset(ov) }),
        ) {
            set_error(e.raw_os_error().unwrap_or(5) as u32);
            return 0;
        }
    }
    match std::io::Read::read(&mut *g, slice) {
        Ok(k) => {
            if !ov.is_null() {
                unsafe { overlapped_complete(ov, k as u32) };
            }
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
    ov: *mut c_void,
) -> i32 {
    if buf.is_null() && n != 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Console stdout/stderr: the pseudo handles from `GetStdHandle` write to fd 1/2.
    // Rust's TLS-guard failure path and `println!` both end up here.
    if handle == std_handle_value(STD_OUTPUT_HANDLE) || handle == std_handle_value(STD_ERROR_HANDLE) {
        let fd = if handle == std_handle_value(STD_OUTPUT_HANDLE) {
            1
        } else {
            2
        };
        if n == 0 {
            if !done.is_null() {
                *done = 0;
            }
            set_error(ERROR_SUCCESS);
            return 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(buf as *const u8, n as usize) };
        let mut written = 0usize;
        while written < slice.len() {
            let r = unsafe {
                libc::write(
                    fd,
                    slice.as_ptr().add(written) as *const c_void,
                    slice.len() - written,
                )
            };
            if r < 0 {
                let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(5);
                // Interrupted writes retry; anything else fails the call.
                if e == libc::EINTR {
                    continue;
                }
                set_error(e as u32);
                return 0;
            }
            written += r as usize;
            break; // pipes/ttys accept the write in one go for these sizes; no partial loop needed.
        }
        if !done.is_null() {
            *done = written as u32;
        }
        set_error(ERROR_SUCCESS);
        return 1;
    }
    let arc = files().lock().unwrap().get(&handle).cloned();
    let Some(arc) = arc else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let slice = unsafe { std::slice::from_raw_parts(buf as *const u8, n as usize) };
    let mut g = arc.lock().unwrap();
    if !ov.is_null() {
        // Explicit offset: start there instead of at the shared file position.
        if let Err(e) = std::io::Seek::seek(
            &mut *g,
            std::io::SeekFrom::Start(unsafe { overlapped_offset(ov) }),
        ) {
            set_error(e.raw_os_error().unwrap_or(5) as u32);
            return 0;
        }
    }
    match std::io::Write::write(&mut *g, slice) {
        Ok(k) => {
            if !ov.is_null() {
                unsafe { overlapped_complete(ov, k as u32) };
            }
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

// ---------------------------------------------------------------------------------------
// Step-11l file reads past stubs: `NtReadFile`, `GetFileInformationByHandle(Ex)` and
// `RtlNtStatusToDosError`. Rust `std::fs` reads through `NtReadFile` (synchronous) and
// sizes files via `GetFileInformationByHandle(Ex)`, so the auto-stubs returning 0 made
// every `G:\`/`GAME:` read look empty. These shims read the same VFS `FILES` table as
// `ReadFile`/`GetFileSizeEx` (read-only game layer, overlay writes).
// ---------------------------------------------------------------------------------------

/// Windows `BY_HANDLE_FILE_INFORMATION` (52 bytes). The game reads this layout
/// directly, so field order must match the Windows definition.
#[repr(C)]
pub struct ByHandleFileInformation {
    pub attrs: u32,
    pub creation_low: u32,
    pub creation_high: u32,
    pub access_low: u32,
    pub access_high: u32,
    pub write_low: u32,
    pub write_high: u32,
    pub volume_serial: u32,
    pub size_high: u32,
    pub size_low: u32,
    pub number_of_links: u32,
    pub index_high: u32,
    pub index_low: u32,
}

fn by_handle_info_for(handle: usize) -> Option<ByHandleFileInformation> {
    let (len, readonly, is_dir, m) = with_file(handle, |f| {
        let m = f.metadata().ok()?;
        Some((m.len(), m.permissions().readonly(), m.is_dir(), m))
    })??;
    let mut attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    if readonly {
        attrs |= FILE_ATTRIBUTE_READONLY;
    }
    let (c_l, c_h) = m
        .created()
        .map(filetime_from_system)
        .unwrap_or((0, 0));
    let (a_l, a_h) = m
        .accessed()
        .map(filetime_from_system)
        .unwrap_or((c_l, c_h));
    let (w_l, w_h) = m
        .modified()
        .map(filetime_from_system)
        .unwrap_or((c_l, c_h));
    Some(ByHandleFileInformation {
        attrs,
        creation_low: c_l,
        creation_high: c_h,
        access_low: a_l,
        access_high: a_h,
        write_low: w_l,
        write_high: w_h,
        volume_serial: 0,
        size_high: (len >> 32) as u32,
        size_low: len as u32,
        number_of_links: 1,
        index_high: 0,
        index_low: 0,
    })
}

pub unsafe extern "win64" fn GetFileInformationByHandle(
    handle: usize,
    out: *mut ByHandleFileInformation,
) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    match by_handle_info_for(handle) {
        Some(info) => {
            *out = info;
            set_error(ERROR_SUCCESS);
            1
        }
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

/// `FILE_INFO_BY_HANDLE_CLASS`: only the two classes Rust `std` needs are
/// implemented (`0` = `FileBasicInfo`, `1` = `FileStandardInfo`).
pub unsafe extern "win64" fn GetFileInformationByHandleEx(
    handle: usize,
    class: u32,
    out: *mut c_void,
    size: u32,
) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let info = match by_handle_info_for(handle) {
        Some(i) => i,
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
    };
    match class {
        0 => {
            // `FILE_BASIC_INFO` (40 bytes): 4x i64 FILETIME + attrs + pad.
            if size < 40 {
                set_error(ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            let c: i64 = ((info.creation_high as u64) << 32 | info.creation_low as u64) as i64;
            let a: i64 = ((info.access_high as u64) << 32 | info.access_low as u64) as i64;
            let w: i64 = ((info.write_high as u64) << 32 | info.write_low as u64) as i64;
            unsafe {
                (out as *mut i64).write_unaligned(c);
                (out.add(8) as *mut i64).write_unaligned(a);
                (out.add(16) as *mut i64).write_unaligned(w);
                (out.add(24) as *mut i64).write_unaligned(w);
                (out.add(32) as *mut u32).write_unaligned(info.attrs);
                (out.add(36) as *mut u32).write_unaligned(0);
            }
            set_error(ERROR_SUCCESS);
            1
        }
        1 => {
            // `FILE_STANDARD_INFO` (24 bytes): allocation + EOF + links + flags.
            if size < 24 {
                set_error(ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            let len: i64 = ((info.size_high as u64) << 32 | info.size_low as u64) as i64;
            let alloc = (len + 4095) & !4095;
            let is_dir = (info.attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
            unsafe {
                (out as *mut i64).write_unaligned(alloc);
                (out.add(8) as *mut i64).write_unaligned(len);
                (out.add(16) as *mut u32).write_unaligned(1);
                (out.add(20) as *mut u8).write_unaligned(0);
                (out.add(21) as *mut u8).write_unaligned(is_dir as u8);
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

/// `BOOL GetFileTime(HANDLE, LPFILETIME creation, LPFILETIME access, LPFILETIME write)`.
///
/// Retail abort `0x2237320`: the title calls this as `GetFileTime(h,NULL,NULL,&ft)`
/// from its `0x140F43800` wrapper (probed before the crashing append). Timestamps come
/// from the same VFS `FILES` metadata as `GetFileInformationByHandle`; any NULL out
/// pointer is skipped per the Windows contract, and unknown handles fail closed like
/// the `ByHandle` neighbors (never a fabricated time).
pub unsafe extern "win64" fn GetFileTime(
    handle: usize,
    creation: *mut FileTime,
    access: *mut FileTime,
    write: *mut FileTime,
) -> i32 {
    match by_handle_info_for(handle) {
        Some(info) => {
            if !creation.is_null() {
                (*creation).low = info.creation_low;
                (*creation).high = info.creation_high;
            }
            if !access.is_null() {
                (*access).low = info.access_low;
                (*access).high = info.access_high;
            }
            if !write.is_null() {
                (*write).low = info.write_low;
                (*write).high = info.write_high;
            }
            set_error(ERROR_SUCCESS);
            1
        }
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub unsafe extern "win64" fn NtReadFile(
    handle: usize,
    _event: usize,
    _apc: usize,
    _apc_ctx: usize,
    iosb: *mut IoStatusBlock,
    buf: *mut u8,
    len: u32,
    byte_offset: *const i64,
    _key: *mut u32,
) -> u32 {
    if len == 0 {
        return iosb_write(iosb, STATUS_SUCCESS, 0);
    }
    if buf.is_null() {
        return iosb_write(iosb, STATUS_INVALID_HANDLE, 0);
    }
    // Console stdin: the pseudo handle from `GetStdHandle(STD_INPUT_HANDLE)` reads fd 0.
    if handle == std_handle_value(STD_INPUT_HANDLE) {
        let slice = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
        let r = unsafe { libc::read(0, slice.as_mut_ptr() as *mut c_void, slice.len()) };
        if r < 0 {
            return iosb_write(iosb, STATUS_UNSUCCESSFUL, 0);
        }
        return iosb_write(iosb, STATUS_SUCCESS, r as usize);
    }
    let arc = files().lock().unwrap().get(&handle).cloned();
    let Some(arc) = arc else {
        return iosb_write(iosb, STATUS_INVALID_HANDLE, 0);
    };
    let mut g = arc.lock().unwrap();
    use std::io::{Read, Seek};
    if !byte_offset.is_null() {
        let off = unsafe { *byte_offset };
        if off >= 0 {
            let _ = g.seek(std::io::SeekFrom::Start(off as u64));
        }
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
    match g.read(slice) {
        Ok(k) => iosb_write(iosb, STATUS_SUCCESS, k),
        Err(_) => iosb_write(iosb, STATUS_UNSUCCESSFUL, 0),
    }
}

pub unsafe extern "win64" fn RtlNtStatusToDosError(status: u32) -> u32 {
    match status {
        STATUS_SUCCESS => ERROR_SUCCESS,
        STATUS_INVALID_HANDLE => 6, // ERROR_INVALID_HANDLE
        STATUS_UNSUCCESSFUL => 1,   // ERROR_INVALID_FUNCTION
        0xC000_0005 => 998,         // STATUS_ACCESS_VIOLATION -> ERROR_NOACCESS
        0xC000_000F => 2,           // STATUS_NO_SUCH_FILE -> ERROR_FILE_NOT_FOUND
        0xC000_0034 => 2,           // STATUS_OBJECT_NAME_NOT_FOUND -> ERROR_FILE_NOT_FOUND
        0xC000_0035 => 80,          // STATUS_OBJECT_NAME_COLLISION -> ERROR_FILE_EXISTS
        0xC000_003A => 3,           // STATUS_OBJECT_PATH_NOT_FOUND -> ERROR_PATH_NOT_FOUND
        _ => status & 0xFFFF,
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
        Target::Game { rel } => match crate::vfs_hooks::delete_file(v, &rel) {
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

// ---------------------------------------------------------------------------------------
// Handle-based deletes (Linux twin of upstream #1054).
// ---------------------------------------------------------------------------------------
//
// `SetFileInformationByHandle(FileDisposition*)` is how `std::filesystem::remove`
// deletes: the handle names the overlay copy, so removing only the copy would
// expose the original. The overlay rel is resolved BEFORE the mark (through
// `/proc/self/fd`, while the file's name is certainly still there) — the
// `GetFinalPathNameByHandleW` + overlay-root prefix check of upstream
// `overlay_file_rel`. Directory handles and handles outside the overlay are
// passed through with no VFS bookkeeping. A successful disposition-delete
// records the whiteout at set time and removes the overlay copy when the
// marking handle closes (Windows deletes at close, which is also what keeps a
// taken-back delete reversible); a taken-back delete records a write instead.
// The last error is preserved across the bookkeeping, and a successful call
// leaves the caller's last error alone, as on Windows.

const FILE_DISPOSITION_INFO_CLASS: u32 = 4;
const FILE_DISPOSITION_INFO_EX_CLASS: u32 = 21;
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x1;
const FILE_DISPOSITION_FLAG_ON_CLOSE: u32 = 0x8;

/// A disposition-delete mark: the whiteout is recorded at set time, the file
/// itself is removed when the marking handle closes.
struct HandleDeleteMark {
    /// Game-relative path when the handle names a game file (the overlay copy
    /// or, for a read-only handle, the original itself); `None` for host files.
    rel: Option<String>,
    /// Absolute host path behind the handle, for the non-game case.
    host: String,
}

static HANDLE_DELETE_MARKS: OnceLock<Mutex<HashMap<usize, HandleDeleteMark>>> =
    OnceLock::new();

fn handle_delete_marks() -> &'static Mutex<HashMap<usize, HandleDeleteMark>> {
    HANDLE_DELETE_MARKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Absolute host path behind a file handle via `/proc/self/fd`, without the
/// ` (deleted)` suffix the kernel appends after an unlink. `None` when the
/// handle is not a known file handle.
fn handle_fs_path(handle: usize) -> Option<std::path::PathBuf> {
    let fd = with_file(handle, |f| {
        use std::os::fd::{AsFd, AsRawFd};
        f.as_fd().as_raw_fd()
    })?;
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    let s = link.to_string_lossy();
    let s = s.strip_suffix(" (deleted)").unwrap_or(&s);
    Some(std::path::PathBuf::from(s.to_string()))
}

/// Game-relative path of the file behind `handle`. `None` for directory
/// handles and for handles outside the game folder (overlay copies and
/// read-only originals both resolve; host files do not).
fn game_rel_for_handle(v: &crate::vfs::Vfs, handle: usize) -> Option<String> {
    if with_file(handle, |f| f.metadata().map(|m| m.is_dir()).unwrap_or(true)).unwrap_or(true) {
        return None;
    }
    let path = handle_fs_path(handle)?;
    // The handle names the overlay copy: hide the game file it covers.
    let root = std::path::PathBuf::from(v.overlay_path(""));
    if let Ok(rel) = path.strip_prefix(&root) {
        if !rel.as_os_str().is_empty() {
            return Some(rel.to_string_lossy().replace('/', "\\"));
        }
        return None;
    }
    // A read-only handle can name the original itself (no overlay copy was
    // staged): whiteout the game file without touching the game folder.
    let groot = std::path::PathBuf::from(v.game_root());
    if let Ok(rel) = path.strip_prefix(&groot) {
        if !rel.as_os_str().is_empty() {
            return Some(rel.to_string_lossy().replace('/', "\\"));
        }
    }
    None
}

/// `BOOL SetFileInformationByHandle(HANDLE, FILE_INFO_BY_HANDLE_CLASS, LPVOID, DWORD)` —
/// only the two disposition classes carry VFS meaning; everything else
/// (other info classes, delete-on-close marks) passes through untouched.
pub unsafe extern "win64" fn SetFileInformationByHandle(
    handle: usize,
    class: u32,
    info: *const c_void,
    size: u32,
) -> i32 {
    // Matches upstream `set_file_info_vfs` class handling verbatim.
    let delete = match class {
        FILE_DISPOSITION_INFO_CLASS if !info.is_null() && size >= 1 => {
            Some((info as *const u8).read_unaligned() != 0)
        }
        FILE_DISPOSITION_INFO_EX_CLASS if !info.is_null() && size >= 4 => {
            let flags = (info as *const u32).read_unaligned();
            if flags & FILE_DISPOSITION_FLAG_ON_CLOSE != 0 {
                None
            } else {
                Some(flags & FILE_DISPOSITION_FLAG_DELETE != 0)
            }
        }
        _ => None,
    };
    // NOTE: bind before use: a map-guard temporary must never live across a
    // second lock of the same non-reentrant mutex (see `close_file_handle`).
    let known = files().lock().unwrap().contains_key(&handle);
    if !known {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let saved = get_error();
    let Some(delete) = delete else {
        // Passed through: a known file handle succeeds with no VFS change.
        set_error(saved);
        return 1;
    };
    if with_file(handle, |f| f.metadata().map(|m| m.is_dir()).unwrap_or(true)).unwrap_or(true) {
        // Directories are passed through: never hidden, never marked.
        set_error(saved);
        return 1;
    }
    // Resolved BEFORE the mark, while the file's name is certainly still there.
    let v = crate::vfs::get();
    let rel = v.and_then(|v| game_rel_for_handle(v, handle));
    if delete {
        let host = handle_fs_path(handle)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        handle_delete_marks()
            .lock()
            .unwrap()
            .insert(handle, HandleDeleteMark { rel: rel.clone(), host });
        if let (Some(v), Some(r)) = (v, rel.as_deref()) {
            let err = get_error();
            v.commit_overlay_removal(r);
            set_error(err);
        }
    } else {
        // The delete was taken back: the file stays.
        handle_delete_marks().lock().unwrap().remove(&handle);
        if let (Some(v), Some(r)) = (v, rel.as_deref()) {
            let err = get_error();
            v.commit_write(r);
            set_error(err);
        }
    }
    set_error(saved);
    1
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

fn host_mkdir_error(e: std::io::Error) -> u32 {
    // A reused data root replays `CreateDirectoryW` on an existing dir (the title's
    // `create_dir_all("S:/Users/xcl/games")` runs every boot). Windows reports that
    // as ERROR_ALREADY_EXISTS (183); the raw host EEXIST (17) must never leak, or the
    // guest std maps it to `os error 17` and the PLS check fails on the 2nd boot.
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        crate::vfs::ERROR_ALREADY_EXISTS
    } else {
        e.raw_os_error().unwrap_or(3) as u32
    }
}

fn create_dir_impl(path: &str) -> i32 {
    let Some(v) = crate::vfs::get() else {
        return match std::fs::create_dir(path) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(host_mkdir_error(e));
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
                set_error(host_mkdir_error(e));
                0
            }
        },
        Target::Translated(p) => match std::fs::create_dir(&p) {
            Ok(()) => {
                set_error(ERROR_SUCCESS);
                1
            }
            Err(e) => {
                set_error(host_mkdir_error(e));
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

/// Win32 error for a copy/move source that does not exist, so callers can fail before
/// staging any overlay directories. Other metadata errors are left to the host call.
fn missing_source_error(real: &str) -> Option<u32> {
    match std::fs::metadata(real) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(e.raw_os_error().unwrap_or(crate::vfs::ERROR_FILE_NOT_FOUND as i32) as u32)
        }
        _ => None,
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
        // Fail before creating any destination directories (upstream #1779).
        if let Some(e) = missing_source_error(&rs) {
            return Err(e);
        }
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
    if let Some(e) = missing_source_error(&rs) {
        return Err(e); // fail before stage_write creates overlay directories
    }
    if std::fs::metadata(&rs).is_ok_and(|m| m.is_dir()) {
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
            // Exact self-move is a safe no-op (upstream #1779 move arm).
            if srel.as_deref() == Some(rel.as_str()) {
                return Ok(());
            }
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
                let _ = v.remove_moved_file(sr);
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
        let _ = v.remove_moved_file(sr);
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
    if let Some(e) = missing_source_error(&rs) {
        return Err(e); // fail before stage_write creates overlay directories
    }
    if std::fs::metadata(&rs).is_ok_and(|m| m.is_dir()) {
        return Err(crate::vfs::ERROR_ACCESS_DENIED);
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
            // Copying a game file onto itself: fail without staging an overlay
            // copy (upstream hands the same path to Win32 so the host call
            // fails; here there is no host call to fail, so reject explicitly).
            if srel.as_ref().is_some_and(|sr| sr.to_lowercase() == rel.to_lowercase()) {
                return Err(crate::vfs::ERROR_ACCESS_DENIED);
            }
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
    // A NULL data pointer must fail with ERROR_INVALID_PARAMETER instead of
    // faulting: forward the null so the shared `find_first_impl` guard sees it.
    let mut w = std::mem::zeroed::<FindDataW>();
    let out_ptr: *mut FindDataW = if data.is_null() {
        std::ptr::null_mut()
    } else {
        &mut w
    };
    match find_first_impl(&path, out_ptr) {
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

pub unsafe extern "win64" fn FindFirstFileExA(
    name: *const u8,
    level: i32,
    data: *mut c_void,
    op: i32,
    _filter: *const c_void,
    _flags: u32,
) -> usize {
    // Linux twin of upstream #1264 (`hk_find_first_ex_a`): Windows' ExA calls
    // neither ExW nor A, so it gets its own entry. Only the common info
    // levels/ops are virtualised (mirroring ExW); exotic values are rejected
    // with INVALID_PARAMETER — Linux has no original to pass through to, and
    // this matches the existing ExW level gate. ANSI callers read the wide
    // layout through the same struct (UTF-8 on Linux), like `FindFirstFileA`.
    if level > 1 || op > 1 {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_HANDLE_VALUE;
    }
    let Some(path) = ansi_str(name) else {
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
// Step-11l path canonicalization: `GetCurrentDirectoryW/A` + `GetFullPathNameW/A`.
// Rust `std::fs` resolves every path through `get_long_path` (`GetFullPathNameW`
// before `CreateFileW`), so the auto-stub returning 0 made even present `GAME:`
// files fail with `ERROR_FILE_NOT_FOUND` before any open. Console-absolute paths
// (`G:`/`GAME:`/drive/UNC/device) are copied through; relative paths resolve
// against the game root as `G:\<rel>` (the console cwd is the game folder, and this
// keeps overlay reads coherent, unlike the host cwd).
// ---------------------------------------------------------------------------------------

fn game_cwd() -> String {
    if let Some(v) = crate::vfs::get() {
        return v.game_root().to_string();
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

fn is_console_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return true;
    }
    if p.starts_with("\\\\") || p.starts_with("//") {
        return true;
    }
    if p.starts_with('\\') || p.starts_with('/') {
        return true;
    }
    if p.starts_with("\\\\?\\") || p.starts_with("//?/") {
        return true;
    }
    // `GAME:`/`G:` mounts (and any `XXX:` console mount) are absolute.
    if let Some(colon) = p.find(':') {
        let prefix = &p[..colon];
        if !prefix.is_empty()
            && prefix
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return true;
        }
    }
    // NT device paths.
    if p.starts_with("\\??\\") || p.starts_with("\\\\.\\") || p.starts_with("//./") {
        return true;
    }
    false
}

fn full_path_for(input: &str) -> String {
    if is_console_absolute(input) {
        return input.to_string();
    }
    // Relative: game-relative `G:\<rel>` so the VFS overlay applies.
    let rel = input.replace('/', "\\");
    let rel = rel.strip_prefix('\\').unwrap_or(&rel);
    format!("G:\\{rel}")
}

unsafe fn write_wide_out(s: &[u16], n: u32, buf: *mut u16) -> u32 {
    let need = s.len() as u32 + 1;
    if n == 0 || buf.is_null() {
        return need;
    }
    if n < need {
        return need;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf, s.len());
        *buf.add(s.len()) = 0;
    }
    s.len() as u32
}

unsafe fn write_ansi_out(s: &str, n: u32, buf: *mut u8) -> u32 {
    let need = s.len() as u32 + 1;
    if n == 0 || buf.is_null() {
        return need;
    }
    if n < need {
        return need;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf, s.len());
        *buf.add(s.len()) = 0;
    }
    s.len() as u32
}

pub unsafe extern "win64" fn GetCurrentDirectoryW(n: u32, buf: *mut u16) -> u32 {
    let cwd = game_cwd();
    let w: Vec<u16> = cwd.encode_utf16().collect();
    let r = unsafe { write_wide_out(&w, n, buf) };
    if r == w.len() as u32 {
        set_error(ERROR_SUCCESS);
    } else if n != 0 && !buf.is_null() {
        set_error(ERROR_INSUFFICIENT_BUFFER);
    }
    r
}

pub unsafe extern "win64" fn GetCurrentDirectoryA(n: u32, buf: *mut u8) -> u32 {
    let cwd = game_cwd();
    let r = unsafe { write_ansi_out(&cwd, n, buf) };
    if r == cwd.len() as u32 {
        set_error(ERROR_SUCCESS);
    } else if n != 0 && !buf.is_null() {
        set_error(ERROR_INSUFFICIENT_BUFFER);
    }
    r
}

pub unsafe extern "win64" fn GetFullPathNameW(
    name: *const u16,
    n: u32,
    buf: *mut u16,
    part: *mut *mut u16,
) -> u32 {
    let Some(input) = wide_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let full = full_path_for(&input);
    let w: Vec<u16> = full.encode_utf16().collect();
    let r = unsafe { write_wide_out(&w, n, buf) };
    if r != w.len() as u32 {
        if n != 0 && !buf.is_null() {
            set_error(ERROR_INSUFFICIENT_BUFFER);
        }
        return r;
    }
    if !part.is_null() {
        unsafe {
            *part = std::ptr::null_mut();
            if !buf.is_null() {
                let mut last: *mut u16 = std::ptr::null_mut();
                for i in 0..w.len() {
                    if w[i] == b'\\' as u16 || w[i] == b'/' as u16 || w[i] == b':' as u16 {
                        last = buf.add(i + 1);
                    }
                }
                *part = last;
            }
        }
    }
    set_error(ERROR_SUCCESS);
    r
}

pub unsafe extern "win64" fn GetFullPathNameA(
    name: *const u8,
    n: u32,
    buf: *mut u8,
    part: *mut *mut u8,
) -> u32 {
    let Some(input) = ansi_str(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let full = full_path_for(&input);
    let r = unsafe { write_ansi_out(&full, n, buf) };
    if r != full.len() as u32 {
        if n != 0 && !buf.is_null() {
            set_error(ERROR_INSUFFICIENT_BUFFER);
        }
        return r;
    }
    if !part.is_null() {
        unsafe {
            *part = std::ptr::null_mut();
            if !buf.is_null() {
                let b = full.as_bytes();
                let mut last: *mut u8 = std::ptr::null_mut();
                for i in 0..b.len() {
                    if b[i] == b'\\' || b[i] == b'/' || b[i] == b':' {
                        last = buf.add(i + 1);
                    }
                }
                *part = last;
            }
        }
    }
    set_error(ERROR_SUCCESS);
    r
}

// ---------------------------------------------------------------------------------------
// FormatMessage (Phase C step 12e): title-called kernel32 formatting, no CRT needed
// ---------------------------------------------------------------------------------------
//
// The synthetic title calls `kernel32!FormatMessageW` once (its failure paths format a
// Win32 code before `ExitProcess`); the auto-stub returned 0, so the text was empty.
// This HLE covers the Win32 contract the title uses: `FROM_SYSTEM` code lookup,
// `FROM_STRING` with `%1` inserts (plus `ARGUMENT_ARRAY`), `ALLOCATE_BUFFER` via the
// process heap, truncation → `ERROR_INSUFFICIENT_BUFFER`, unknown-code fallback text,
// and `NULL` output without `ALLOCATE_BUFFER` → `ERROR_INVALID_PARAMETER`.

const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x0000_0100;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;
const FORMAT_MESSAGE_FROM_STRING: u32 = 0x0000_0400;
const FORMAT_MESSAGE_FROM_HMODULE: u32 = 0x0000_0800;
const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
const FORMAT_MESSAGE_ARGUMENT_ARRAY: u32 = 0x0000_2000;
const ERROR_MR_MID_NOT_FOUND: u32 = 317;

fn system_message_for(id: u32) -> Option<&'static str> {
    match id {
        0 => Some("The operation completed successfully.\r\n"),
        2 => Some("The system cannot find the file specified.\r\n"),
        3 => Some("The system cannot find the path specified.\r\n"),
        5 => Some("Access is denied.\r\n"),
        6 => Some("The handle is invalid.\r\n"),
        8 => Some("Not enough memory resources are available to process this command.\r\n"),
        14 => Some("Not enough storage is available to complete this operation.\r\n"),
        87 => Some("The parameter is incorrect.\r\n"),
        122 => Some("The data area passed to a system call is too small.\r\n"),
        126 => Some("The specified module could not be found.\r\n"),
        127 => Some("The specified procedure could not be found.\r\n"),
        145 => Some("The directory is not empty.\r\n"),
        183 => Some("Cannot create a file when that file already exists.\r\n"),
        203 => Some("The system could not find the environment option that was entered.\r\n"),
        317 => Some(
            "The system cannot find message text for message number 0x%1 in the message file for %2.\r\n",
        ),
        487 => Some("Attempt to access invalid address.\r\n"),
        1460 => Some("This operation returned because the timeout period expired.\r\n"),
        0x8000_4001 => Some("Not implemented.\r\n"),
        _ => {
            // HRESULT_FROM_WIN32 codes share the Win32 text of their low word.
            if id & 0xFFFF_0000 == 0x8007_0000 {
                system_message_for(id & 0xFFFF)
            } else {
                None
            }
        }
    }
}

fn format_unknown_message(id: u32) -> String {
    format!("Unknown error {id} (0x{id:08X}).\r\n")
}

/// `%0` always terminates the message, even with `IGNORE_INSERTS`.
fn format_truncate_at_terminator(template: &str) -> &str {
    let b = template.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'%' && b[i + 1] == b'0' {
            return &template[..i];
        }
        i += 1;
    }
    template
}

unsafe fn format_arg_val(args: *const c_void, flags: u32, index_1based: usize) -> usize {
    if args.is_null() || index_1based == 0 {
        return 0;
    }
    unsafe {
        if flags & FORMAT_MESSAGE_ARGUMENT_ARRAY != 0 {
            let base = args as *const usize;
            *base.add(index_1based - 1)
        } else {
            // `va_list *`: one indirection to the caller stack slots.
            let base = *(args as *const *const usize);
            if base.is_null() {
                return 0;
            }
            *base.add(index_1based - 1)
        }
    }
}

fn format_int_insert(val: usize, inner: &str) -> String {
    if inner.contains('X') {
        format!("{:X}", val)
    } else if inner.contains('x') {
        format!("{:x}", val)
    } else if inner.contains('o') || inner.contains('O') {
        format!("{:o}", val)
    } else if inner.contains('p') || inner.contains('P') {
        format!("0x{:X}", val)
    } else if inner.contains('c') || inner.contains('C') {
        char::from_u32(val as u32).map(|c| c.to_string()).unwrap_or_else(|| val.to_string())
    } else if inner.contains('d')
        || inner.contains('D')
        || inner.contains('i')
        || inner.contains('I')
    {
        format!("{}", val as i32 as i64)
    } else if inner.contains('u') || inner.contains('U') {
        format!("{}", val)
    } else {
        format!("{}", val)
    }
}

unsafe fn format_expand(
    template: &str,
    flags: u32,
    args: *const c_void,
    read_insert: &dyn Fn(usize) -> String,
) -> String {
    if flags & FORMAT_MESSAGE_IGNORE_INSERTS != 0 {
        return format_truncate_at_terminator(template).to_owned();
    }
    let b = template.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len() + 16);
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        if i + 1 >= b.len() {
            out.push(b'%');
            break;
        }
        let nxt = b[i + 1];
        if nxt == b'%' {
            out.push(b'%');
            i += 2;
            continue;
        }
        if nxt == b'0' {
            break;
        }
        if !nxt.is_ascii_digit() || nxt == b'0' {
            out.push(b'%');
            i += 1;
            continue;
        }
        // Insert number: 1..99 (one or two digits; `%100` is `%10` + `"0"`).
        let mut num = (nxt - b'0') as usize;
        let mut consumed = 2;
        if i + 2 < b.len() && b[i + 2].is_ascii_digit() {
            num = num * 10 + (b[i + 2] - b'0') as usize;
            consumed = 3;
        }
        if num == 0 || num > 99 {
            out.push(b'%');
            i += 1;
            continue;
        }
        // Optional `!format!` spec.
        let mut inner = "";
        if i + consumed < b.len() && b[i + consumed] == b'!' {
            if let Some(rel) = b[i + consumed + 1..].iter().position(|&c| c == b'!') {
                inner = std::str::from_utf8(&b[i + consumed + 1..i + consumed + 1 + rel])
                    .unwrap_or("");
                consumed += rel + 2;
            }
        }
        let val = unsafe { format_arg_val(args, flags, num) };
        let s = if inner.is_empty() || inner.contains('s') || inner.contains('S') {
            read_insert(val)
        } else {
            format_int_insert(val, inner)
        };
        out.extend_from_slice(s.as_bytes());
        i += consumed;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `DWORD FormatMessageW(DWORD, LPCVOID, DWORD, DWORD, LPWSTR, DWORD, va_list *)`.
pub unsafe extern "win64" fn FormatMessageW(
    flags: u32,
    source: *const c_void,
    message_id: u32,
    _language_id: u32,
    buffer: *mut u16,
    size: u32,
    args: *const c_void,
) -> u32 {
    let template: String = if flags & FORMAT_MESSAGE_FROM_STRING != 0 {
        if source.is_null() {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        match from_wide_ptr(source as *const u16) {
            Some(s) => s,
            None => {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            }
        }
    } else if flags & FORMAT_MESSAGE_FROM_SYSTEM != 0 {
        match system_message_for(message_id) {
            Some(s) => s.to_owned(),
            None => format_unknown_message(message_id),
        }
    } else if flags & FORMAT_MESSAGE_FROM_HMODULE != 0 {
        set_error(ERROR_MR_MID_NOT_FOUND);
        return 0;
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let read_wide = &|ptr: usize| -> String {
        if ptr == 0 {
            return String::new();
        }
        unsafe { from_wide_ptr(ptr as *const u16).unwrap_or_default() }
    };
    let expanded = unsafe { format_expand(&template, flags, args, read_wide) };
    let wide: Vec<u16> = expanded.encode_utf16().collect();
    let len = wide.len() as u32;
    if flags & FORMAT_MESSAGE_ALLOCATE_BUFFER != 0 {
        if buffer.is_null() {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let need = wide.len() + 1;
        let cap = (size as usize).max(need).max(1);
        let mem = HeapAlloc(GetProcessHeap(), 0, cap * std::mem::size_of::<u16>());
        if mem.is_null() {
            return 0;
        }
        let dst = mem as *mut u16;
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            *dst.add(wide.len()) = 0;
            *(buffer as *mut *mut u16) = dst;
        }
        set_error(ERROR_SUCCESS);
        return len;
    }
    if buffer.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if wide.len() + 1 > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), buffer, wide.len());
        *buffer.add(wide.len()) = 0;
    }
    set_error(ERROR_SUCCESS);
    len
}

/// `DWORD FormatMessageA(DWORD, LPCVOID, DWORD, DWORD, LPSTR, DWORD, va_list *)` —
/// ANSI (UTF-8 on Linux) twin of `FormatMessageW`.
pub unsafe extern "win64" fn FormatMessageA(
    flags: u32,
    source: *const c_void,
    message_id: u32,
    _language_id: u32,
    buffer: *mut u8,
    size: u32,
    args: *const c_void,
) -> u32 {
    let template: String = if flags & FORMAT_MESSAGE_FROM_STRING != 0 {
        if source.is_null() {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        match from_ansi_ptr(source as *const u8) {
            Some(s) => s,
            None => {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            }
        }
    } else if flags & FORMAT_MESSAGE_FROM_SYSTEM != 0 {
        match system_message_for(message_id) {
            Some(s) => s.to_owned(),
            None => format_unknown_message(message_id),
        }
    } else if flags & FORMAT_MESSAGE_FROM_HMODULE != 0 {
        set_error(ERROR_MR_MID_NOT_FOUND);
        return 0;
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let read_ansi = &|ptr: usize| -> String {
        if ptr == 0 {
            return String::new();
        }
        unsafe { from_ansi_ptr(ptr as *const u8).unwrap_or_default() }
    };
    let expanded = unsafe { format_expand(&template, flags, args, read_ansi) };
    let bytes = expanded.as_bytes();
    let len = bytes.len() as u32;
    if flags & FORMAT_MESSAGE_ALLOCATE_BUFFER != 0 {
        if buffer.is_null() {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let need = bytes.len() + 1;
        let cap = (size as usize).max(need).max(1);
        let mem = HeapAlloc(GetProcessHeap(), 0, cap);
        if mem.is_null() {
            return 0;
        }
        let dst = mem as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            *dst.add(bytes.len()) = 0;
            *(buffer as *mut *mut u8) = dst;
        }
        set_error(ERROR_SUCCESS);
        return len;
    }
    if buffer.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if bytes.len() + 1 > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
        *buffer.add(bytes.len()) = 0;
    }
    set_error(ERROR_SUCCESS);
    len
}

// ---------------------------------------------------------------------------------------
// COM apartment + activation (retail `CoCreateInstance(CLSID_MMDeviceEnumerator)`)
// ---------------------------------------------------------------------------------------
//
// XCL hosts no COM servers and registers no classes, so every activation must fail.
// Retail's audio init calls `CoCreateInstance` at `exe+0x43aea6a` and, on success,
// dereferences the returned `IMMDeviceEnumerator` at `exe+0x43aea86`; the generic
// auto-stub answered `S_OK` without writing the out pointer, hence the NULL `this`
// fault. COM's contract is to clear `*ppv` on every failure, and the honest failure
// for an unregistered class is `REGDB_E_CLASSNOTAVAILABLE`. `CoInitializeEx` keeps a
// real per-thread apartment so a title's init/uninit balance and a changed-mode
// mistake behave as on Windows (XAudio2 asks the same question).
//
// If XCL ever implements a COM class, `CoCreateInstance` is the single seam: match
// the CLSID here before returning the class-not-available result.

const RPC_E_CHANGED_MODE: HRESULT = 0x8001_0106u32 as HRESULT;
const REGDB_E_CLASSNOTAVAILABLE: HRESULT = 0x8004_0154u32 as HRESULT;
/// `COINIT_APARTMENTTHREADED`; `COINIT_MULTITHREADED` is 0.
const COINIT_APARTMENTTHREADED: u32 = 0x2;
/// `COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE | COINIT_SPEED_OVER_MEMORY`.
const COINIT_VALID_MASK: u32 = 0xE;

/// `(apartment mode, CoInitializeEx success count)` for the calling thread.
thread_local! {
    static COM_APARTMENT: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// Log each requested class once: the CLSIDs on retail's non-graphics path tell the
/// next slice which host service the title expects.
fn log_com_class_once(clsid: *const c_void) {
    static LOGGED: LazyLock<Mutex<std::collections::BTreeSet<[u8; 16]>>> =
        LazyLock::new(|| Mutex::new(std::collections::BTreeSet::new()));
    let key = unsafe { std::ptr::read_unaligned(clsid as *const [u8; 16]) };
    let mut g = LOGGED.lock().unwrap();
    if g.insert(key) {
        crate::xlog_info!(
            "COM: no class is registered on Linux; CoCreateInstance CLSID {key:02x?} → REGDB_E_CLASSNOTAVAILABLE"
        );
    }
}

/// `HRESULT CoInitializeEx(void*, DWORD)`: per-thread apartment, as documented.
pub unsafe extern "win64" fn CoInitializeEx(_reserved: *mut c_void, coinit: u32) -> HRESULT {
    if coinit & !COINIT_VALID_MASK != 0 {
        return E_INVALIDARG;
    }
    let mode = coinit & COINIT_APARTMENTTHREADED;
    COM_APARTMENT.with(|apt| {
        let (current, count) = apt.get();
        if count == 0 {
            apt.set((mode, 1));
            S_OK
        } else if current != mode {
            RPC_E_CHANGED_MODE
        } else {
            apt.set((current, count + 1));
            S_FALSE
        }
    })
}

/// `void CoUninitialize()`: drop one `CoInitializeEx` from the calling thread.
pub unsafe extern "win64" fn CoUninitialize() {
    COM_APARTMENT.with(|apt| {
        let (mode, count) = apt.get();
        if count > 0 {
            apt.set((mode, count - 1));
        }
    });
}

/// `HRESULT CoCreateInstance(REFCLSID, IUnknown*, DWORD, REFIID, void**)`.
///
/// The GUIDs are opaque: XCL has no class registry. The out pointer is cleared on
/// every failure path, so a caller that ignores the HRESULT still cannot call a
/// stale object; retail checks `eax` and skips its audio device path.
pub unsafe extern "win64" fn CoCreateInstance(
    rclsid: *const c_void,
    _outer: *mut c_void,
    _context: u32,
    riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    ppv.write(std::ptr::null_mut());
    if rclsid.is_null() || riid.is_null() {
        return E_INVALIDARG;
    }
    log_com_class_once(rclsid);
    REGDB_E_CLASSNOTAVAILABLE
}

/// `HRESULT CoCreateGuid(GUID*)` — a fresh version-4 GUID. The bytes come from
/// the same OS entropy source as `std::_Random_device` (no constant, no
/// seeded PRNG), and the version/variant bits are forced per RFC 4122. Retail
/// Step 80: the title's GUID helper `0x1455f3820` formats this value for a
/// save-container name; the auto-stub returned S_OK without writing it.
pub unsafe extern "win64" fn CoCreateGuid(guid: *mut u8) -> HRESULT {
    if guid.is_null() {
        return E_INVALIDARG;
    }
    let mut bytes = [0u8; 16];
    crate::hle::crt_linux::fill_os_entropy(&mut bytes);
    bytes[7] = (bytes[7] & 0x0F) | 0x40; // version 4 (high nibble of Data3)
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant (Data4[0])
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), guid, bytes.len());
    S_OK
}

/// `int StringFromGUID2(REFGUID, LPOLESTR, int cch)` — the brace form
/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` in UTF-16, uppercase hex like the
/// Win32 implementation. Returns the number of characters written including
/// the terminating NUL (39), or 0 when `cch < 39`, the buffer is null or the
/// GUID is null; a refused call writes nothing.
pub unsafe extern "win64" fn StringFromGUID2(guid: *const u8, out: *mut u16, cch: i32) -> i32 {
    const REQUIRED: i32 = 39; // 38 characters + NUL
    if guid.is_null() || out.is_null() || cch < REQUIRED {
        return 0;
    }
    let g = std::slice::from_raw_parts(guid, 16);
    let data1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let data2 = u16::from_le_bytes([g[4], g[5]]);
    let data3 = u16::from_le_bytes([g[6], g[7]]);
    let d4 = &g[8..16];
    let text = format!(
        "{{{data1:08X}-{data2:04X}-{data3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    );
    debug_assert_eq!(text.len(), 38);
    for (i, unit) in text.encode_utf16().enumerate() {
        *out.add(i) = unit;
    }
    *out.add(38) = 0;
    REQUIRED
}

// ---------------------------------------------------------------------------------------
// String conversion: MultiByteToWideChar / WideCharToMultiByte (retail abort 0x2237179)
// ---------------------------------------------------------------------------------------
//
// The abort at `exe+0x2237179` reads an all-zero path buffer (VMA `0x1474539F6`):
// the lookup predicate `sub_140643C90` wants an absolute path (leading `/` or `\`,
// else a `:` via `strchr`), but the buffer was never written. The game's narrow->wide
// wrapper at `exe+0x113492c` converts UTF-8 (`CodePage 0xFDE9` = `CP_UTF8`) into a
// caller wide buffer; the auto-stub returned 0 and left every such buffer empty.
// These shims implement the real Win32 contract with UTF-8 as the ANSI code page
// (the same choice `write_ansi_out`/`from_ansi_ptr` make on Linux), so a successful
// conversion writes a real absolute path and the predicate can pass.
//
// `CP_ACP`/`CP_OEMCP` are UTF-8 on Linux; `CP_UTF7`/UTF-16 code pages stay
// `ERROR_INVALID_PARAMETER` (never silently mis-decoded). `MB_ERR_INVALID_CHARS` /
// `WC_ERR_INVALID_CHARS` fail with `ERROR_NO_UNICODE_TRANSLATION` on bad input;
// without them the input is lossy (`U+FFFD` on decode, `'?'` on unpaired surrogates).

const CP_ACP: u32 = 0;
const CP_OEMCP: u32 = 1;
const CP_UTF7: u32 = 65000;
const CP_UTF8: u32 = 65001;

const MB_PRECOMPOSED: u32 = 0x1;
const MB_ERR_INVALID_CHARS: u32 = 0x8;
const WC_ERR_INVALID_CHARS: u32 = 0x80;

const ERROR_NO_UNICODE_TRANSLATION: u32 = 1113;
const ERROR_INVALID_FLAGS: u32 = 1004;

fn mb_codepage_is_utf8(code_page: u32) -> bool {
    matches!(code_page, CP_ACP | CP_OEMCP | CP_UTF8)
}

unsafe fn mb_input_bytes(mb_ptr: *const u8, mb_len: i32) -> Option<(Vec<u8>, bool)> {
    if mb_ptr.is_null() {
        return None;
    }
    if mb_len == -1 {
        let mut n = 0usize;
        while *mb_ptr.add(n) != 0 {
            n += 1;
        }
        let slice = std::slice::from_raw_parts(mb_ptr, n);
        Some((slice.to_vec(), true))
    } else if mb_len > 0 {
        let slice = std::slice::from_raw_parts(mb_ptr, mb_len as usize);
        Some((slice.to_vec(), false))
    } else {
        None
    }
}

unsafe fn wc_input_units(wc_ptr: *const u16, wc_len: i32) -> Option<(Vec<u16>, bool)> {
    if wc_ptr.is_null() {
        return None;
    }
    if wc_len == -1 {
        let mut n = 0usize;
        while *wc_ptr.add(n) != 0 {
            n += 1;
        }
        let slice = std::slice::from_raw_parts(wc_ptr, n);
        Some((slice.to_vec(), true))
    } else if wc_len > 0 {
        let slice = std::slice::from_raw_parts(wc_ptr, wc_len as usize);
        Some((slice.to_vec(), false))
    } else {
        None
    }
}

/// `int MultiByteToWideChar(UINT, DWORD, LPCCH, int, LPWSTR, int)` — UTF-8 (and the
/// UTF-8 aliases `CP_ACP`/`CP_OEMCP`) to UTF-16. Query with `wc_len == 0` returns the
/// required `wchar` count (including the terminator for `mb_len == -1`); conversion
/// fails with `ERROR_INSUFFICIENT_BUFFER` when the buffer is too small.
pub unsafe extern "win64" fn MultiByteToWideChar(
    code_page: u32,
    flags: u32,
    mb_ptr: *const u8,
    mb_len: i32,
    wc_buf: *mut u16,
    wc_len: i32,
) -> i32 {
    if !mb_codepage_is_utf8(code_page) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if flags & !(MB_PRECOMPOSED | MB_ERR_INVALID_CHARS) != 0 {
        set_error(ERROR_INVALID_FLAGS);
        return 0;
    }
    let Some((bytes, nul_terminated)) = mb_input_bytes(mb_ptr, mb_len) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let strict = flags & MB_ERR_INVALID_CHARS != 0;
    let text: String = if strict {
        match std::str::from_utf8(&bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => {
                set_error(ERROR_NO_UNICODE_TRANSLATION);
                return 0;
            }
        }
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let required: usize = if nul_terminated { wide.len() + 1 } else { wide.len() };
    let Ok(required_i32) = i32::try_from(required) else {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return 0;
    };
    if required == 0 {
        // Empty explicit-length input: success, nothing to write (matches Win32).
        set_error(ERROR_SUCCESS);
        return 0;
    }
    if wc_len == 0 {
        set_error(ERROR_SUCCESS);
        return required_i32;
    }
    if wc_buf.is_null() || wc_len < 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (wc_len as usize) < required {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    if nul_terminated {
        wide.push(0);
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), wc_buf, required);
    set_error(ERROR_SUCCESS);
    required_i32
}

/// `int WideCharToMultiByte(UINT, DWORD, LPCWCH, int, LPSTR, int, LPCCH, LPBOOL)` —
/// UTF-16 to UTF-8 (and the UTF-8 aliases `CP_ACP`/`CP_OEMCP`). Unpaired surrogates
/// fail with `ERROR_NO_UNICODE_TRANSLATION` under `WC_ERR_INVALID_CHARS`, else map to
/// `'?'`. For UTF-8 targets the default-char slots must be null, as on Windows.
pub unsafe extern "win64" fn WideCharToMultiByte(
    code_page: u32,
    flags: u32,
    wc_ptr: *const u16,
    wc_len: i32,
    mb_buf: *mut u8,
    mb_len: i32,
    default_char: *const u8,
    used_default: *mut i32,
) -> i32 {
    if !mb_codepage_is_utf8(code_page) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if flags & !WC_ERR_INVALID_CHARS != 0 {
        set_error(ERROR_INVALID_FLAGS);
        return 0;
    }
    if !default_char.is_null() || !used_default.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some((units, nul_terminated)) = wc_input_units(wc_ptr, wc_len) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let strict = flags & WC_ERR_INVALID_CHARS != 0;
    let mut text = String::new();
    let mut invalid = false;
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => text.push(c),
            Err(_) => {
                if strict {
                    set_error(ERROR_NO_UNICODE_TRANSLATION);
                    return 0;
                }
                invalid = true;
                text.push('?');
            }
        }
    }
    let _ = invalid;
    let bytes = text.as_bytes();
    let required: usize = if nul_terminated { bytes.len() + 1 } else { bytes.len() };
    let Ok(required_i32) = i32::try_from(required) else {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return 0;
    };
    if required == 0 {
        set_error(ERROR_SUCCESS);
        return 0;
    }
    if mb_len == 0 {
        set_error(ERROR_SUCCESS);
        return required_i32;
    }
    if mb_buf.is_null() || mb_len < 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (mb_len as usize) < required {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), mb_buf, bytes.len());
    if nul_terminated {
        *mb_buf.add(bytes.len()) = 0;
    }
    set_error(ERROR_SUCCESS);
    required_i32
}

/// `int GetUserDefaultGeoName(LPWSTR geoName, int cchGeoName)` — the user's two-letter
/// ISO 3166 region name (`L"US"` + NUL, i.e. what a US console reports). No OS locale
/// lookup: a fixed answer keeps the static Linux exe free of locale backends and is
/// deterministic for the title's resource compare. Zero length (or a null buffer with
/// zero length) is the size query and reports 3; otherwise the name is truncated to
/// `cchGeoName` and the return is chars written excluding NUL, per the Win32 contract.
pub unsafe extern "win64" fn GetUserDefaultGeoName(buf: *mut u16, len: i32) -> i32 {
    const GEO: &[u16] = &[b'U' as u16, b'S' as u16];
    const REQUIRED: i32 = 3; // "US" + NUL
    if len == 0 {
        set_error(ERROR_SUCCESS);
        return REQUIRED;
    }
    if len < 0 || buf.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let cap = len as usize;
    let n = GEO.len().min(cap.saturating_sub(1));
    std::ptr::copy_nonoverlapping(GEO.as_ptr(), buf, n);
    *buf.add(n) = 0;
    set_error(ERROR_SUCCESS);
    n as i32
}

/// `int GetUserDefaultLocaleName(LPWSTR lpLocaleName, int cchLocaleName)` — the user's
/// BCP-47 locale name (`L"en-US"`, matching the fixed `GetUserDefaultGeoName` region).
/// Returns characters copied including the terminating NUL; `0` for a too-small buffer
/// (ERROR_INSUFFICIENT_BUFFER) or an invalid argument, like `LCIDToLocaleName`.
/// No OS locale lookup: deterministic for the title's language/config init.
pub unsafe extern "win64" fn GetUserDefaultLocaleName(buf: *mut u16, len: i32) -> i32 {
    const LOCALE: &[u16] = &[b'e' as u16, b'n' as u16, b'-' as u16, b'U' as u16, b'S' as u16];
    const REQUIRED: i32 = 6; // "en-US" + NUL
    if len <= 0 || buf.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if len < REQUIRED {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    std::ptr::copy_nonoverlapping(LOCALE.as_ptr(), buf, LOCALE.len());
    *buf.add(LOCALE.len()) = 0;
    set_error(ERROR_SUCCESS);
    REQUIRED
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

/// `char* _get_narrow_winmain_command_line(void)` — the CRT narrow command line
/// (retail abort 0x2237179: the game's CRT init at `exe+0x56A1E46` passes this to
/// its argv builder; the auto-stub returned NULL, leaving `argv` empty so the
/// dump-dir init that derives from `argv[0]` never populated `0x1474539F6`).
/// Same process-lifetime buffer as `GetCommandLineA` (UTF-8 on Linux).
pub unsafe extern "win64" fn _get_narrow_winmain_command_line() -> *const u8 {
    GetCommandLineA()
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    _pad: u32,
    lp_reserved: *const u16,
    lp_desktop: *const u16,
    lp_title: *const u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *const u8,
    h_std_input: usize,
    h_std_output: usize,
    h_std_error: usize,
}

/// `void GetStartupInfoW(LPSTARTUPINFOW)` — default console startup info
/// (retail abort 0x2237179: the CRT entry at `exe+0x55E9B14` reads `dwFlags` at
/// `+0x5C` and `wShowWindow` at `+0x60`; the auto-stub left the memset-zeroed
/// struct untouched, which happens to match these defaults, but the explicit
/// fill documents the contract and hands out real std handles).
pub unsafe extern "win64" fn GetStartupInfoW(info: *mut StartupInfoW) {
    if info.is_null() {
        return;
    }
    let si = StartupInfoW {
        cb: 104,
        _pad: 0,
        lp_reserved: std::ptr::null(),
        lp_desktop: std::ptr::null(),
        lp_title: std::ptr::null(),
        dw_x: 0,
        dw_y: 0,
        dw_x_size: 0,
        dw_y_size: 0,
        dw_x_count_chars: 0,
        dw_y_count_chars: 0,
        dw_fill_attribute: 0,
        dw_flags: 0,
        w_show_window: 1,
        cb_reserved2: 0,
        lp_reserved2: std::ptr::null(),
        h_std_input: std_handle_value(STD_INPUT_HANDLE),
        h_std_output: std_handle_value(STD_OUTPUT_HANDLE),
        h_std_error: std_handle_value(STD_ERROR_HANDLE),
    };
    std::ptr::write_unaligned(info, si);
    set_error(ERROR_SUCCESS);
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
    if size == 0 {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    if buf.is_null() {
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
    if size == 0 {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    if buf.is_null() {
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

fn module_base_name_for(h: usize) -> Option<String> {
    fn file_name_of(p: &std::path::Path, fallback: &str) -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| fallback.to_string())
    }
    if h == 0 {
        if let Some(m) = crate::modules::main_module() {
            return Some(file_name_of(&m.path, &m.name));
        }
        return crate::state::boot().map(|b| file_name_of(&b.exe_path, "xcl"));
    }
    if let Some(m) = crate::modules::main_module() {
        if m.base == h {
            return Some(file_name_of(&m.path, &m.name));
        }
    }
    if let Some(m) = crate::modules::find_mapped_by_base(h) {
        return Some(file_name_of(&m.path, &m.name));
    }
    crate::modules::hle_name_by_handle(h)
}

fn is_own_process(process: usize) -> bool {
    process == current_process_handle()
}

/// Shared `GetModuleFileNameEx` body: the mapped image path when the caller asks about
/// the current process and XCL knows the module; `0` + `ERROR_INVALID_PARAMETER` for a
/// null buffer/zero capacity (as on Windows); `0` + `ERROR_INVALID_HANDLE` for a foreign
/// process or an unknown/native module (no host PEB fallback on Linux).
unsafe fn module_file_name_ex_w(process: usize, h: usize, buf: *mut u16, size: u32) -> u32 {
    if !is_own_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let Some(p) = module_path_for(h) else {
        set_error(ERROR_INVALID_HANDLE);
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

/// Shared `GetModuleFileNameEx` ANSI body (UTF-8 on Linux).
unsafe fn module_file_name_ex_a(process: usize, h: usize, buf: *mut u8, size: u32) -> u32 {
    if !is_own_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let Some(p) = module_path_for(h) else {
        set_error(ERROR_INVALID_HANDLE);
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

/// `DWORD K32GetModuleFileNameExW(HANDLE, HMODULE, LPWSTR, DWORD)` — mapped image path.
pub unsafe extern "win64" fn K32GetModuleFileNameExW(
    process: usize,
    h: usize,
    buf: *mut u16,
    size: u32,
) -> u32 {
    module_file_name_ex_w(process, h, buf, size)
}

/// `DWORD GetModuleFileNameExW(HANDLE, HMODULE, LPWSTR, DWORD)` — psapi alias.
pub unsafe extern "win64" fn GetModuleFileNameExW(
    process: usize,
    h: usize,
    buf: *mut u16,
    size: u32,
) -> u32 {
    module_file_name_ex_w(process, h, buf, size)
}

/// `DWORD K32GetModuleFileNameExA(HANDLE, HMODULE, LPSTR, DWORD)` — ANSI form.
pub unsafe extern "win64" fn K32GetModuleFileNameExA(
    process: usize,
    h: usize,
    buf: *mut u8,
    size: u32,
) -> u32 {
    module_file_name_ex_a(process, h, buf, size)
}

/// `DWORD GetModuleFileNameExA(HANDLE, HMODULE, LPSTR, DWORD)` — psapi alias.
pub unsafe extern "win64" fn GetModuleFileNameExA(
    process: usize,
    h: usize,
    buf: *mut u8,
    size: u32,
) -> u32 {
    module_file_name_ex_a(process, h, buf, size)
}

/// Shared `GetModuleBaseName` body: the file name of the mapped image path.
unsafe fn module_base_name_w(process: usize, h: usize, buf: *mut u16, size: u32) -> u32 {
    if !is_own_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let Some(b) = module_base_name_for(h) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if buf.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let n = crate::util::write_wide_out(&b, buf, size);
    if n == size {
        set_error(ERROR_INSUFFICIENT_BUFFER);
    } else {
        set_error(ERROR_SUCCESS);
    }
    n
}

/// Shared `GetModuleBaseName` ANSI body (UTF-8 on Linux).
unsafe fn module_base_name_a(process: usize, h: usize, buf: *mut u8, size: u32) -> u32 {
    if !is_own_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let Some(b) = module_base_name_for(h) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if buf.is_null() || size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let n = crate::util::write_ansi_out(&b, buf, size);
    if n == size {
        set_error(ERROR_INSUFFICIENT_BUFFER);
    } else {
        set_error(ERROR_SUCCESS);
    }
    n
}

/// `DWORD K32GetModuleBaseNameW(HANDLE, HMODULE, LPWSTR, DWORD)`.
pub unsafe extern "win64" fn K32GetModuleBaseNameW(
    process: usize,
    h: usize,
    buf: *mut u16,
    size: u32,
) -> u32 {
    module_base_name_w(process, h, buf, size)
}

/// `DWORD GetModuleBaseNameW(HANDLE, HMODULE, LPWSTR, DWORD)` — psapi alias.
pub unsafe extern "win64" fn GetModuleBaseNameW(
    process: usize,
    h: usize,
    buf: *mut u16,
    size: u32,
) -> u32 {
    module_base_name_w(process, h, buf, size)
}

/// `DWORD K32GetModuleBaseNameA(HANDLE, HMODULE, LPSTR, DWORD)`.
pub unsafe extern "win64" fn K32GetModuleBaseNameA(
    process: usize,
    h: usize,
    buf: *mut u8,
    size: u32,
) -> u32 {
    module_base_name_a(process, h, buf, size)
}

/// `DWORD GetModuleBaseNameA(HANDLE, HMODULE, LPSTR, DWORD)` — psapi alias.
pub unsafe extern "win64" fn GetModuleBaseNameA(
    process: usize,
    h: usize,
    buf: *mut u8,
    size: u32,
) -> u32 {
    module_base_name_a(process, h, buf, size)
}

/// Windows `MODULEINFO` layout (`psapi.h`): base, image size, entry point.
/// Own layout (never `windows_sys`, which never compiles on Linux); the `repr(C)`
/// field order matches the Windows struct so the size assert below pins it.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct ModuleInfo {
    pub lpBaseOfDll: *mut c_void,
    pub SizeOfImage: u32,
    pub EntryPoint: *mut c_void,
}

/// `BOOL K32GetModuleInformation(HANDLE, HMODULE, LPMODULEINFO, DWORD)` — base, size
/// and entry point for the images XCL mapped; `0` + `ERROR_INVALID_HANDLE` for a
/// foreign process or an unknown/native module (no host PEB fallback on Linux),
/// `0` + `ERROR_INSUFFICIENT_BUFFER` when `cb` is too small.
pub unsafe extern "win64" fn K32GetModuleInformation(
    process: usize,
    h: usize,
    info: *mut c_void,
    cb: u32,
) -> i32 {
    if !is_own_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let mapped = if h == 0 {
        crate::modules::main_module()
    } else {
        crate::modules::find_mapped_by_base(h)
    };
    let Some(m) = mapped else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if info.is_null() || (cb as usize) < std::mem::size_of::<ModuleInfo>() {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    let entry = if m.entry_rva != 0 {
        (m.base + m.entry_rva as usize) as *mut c_void
    } else {
        std::ptr::null_mut()
    };
    (info as *mut ModuleInfo).write_unaligned(ModuleInfo {
        lpBaseOfDll: m.base as *mut c_void,
        SizeOfImage: m.size as u32,
        EntryPoint: entry,
    });
    set_error(ERROR_SUCCESS);
    1
}

/// `BOOL GetModuleInformation(HANDLE, HMODULE, LPMODULEINFO, DWORD)` — psapi alias.
pub unsafe extern "win64" fn GetModuleInformation(
    process: usize,
    h: usize,
    info: *mut c_void,
    cb: u32,
) -> i32 {
    K32GetModuleInformation(process, h, info, cb)
}

// ---------------------------------------------------------------------------------------
// Std handles, module handles, environment, FLS and fibers (Phase C step 11c)
// ---------------------------------------------------------------------------------------
//
// The cross-built title's Rust std takes its TLS-guard path through `FlsAlloc` /
// `FlsSetValue`: the auto-stub returned 0, so the guard fell into its stderr-write
// failure path and faulted at `exe+0xae3e4` (fault 0x0). These shims hand out a real
// 128-slot FLS table (per-thread values, process-wide allocation bitmap, callbacks run
// on thread exit through `fls_thread_exit`), real std handles (fd-backed so the guard's
// `WriteFile(GetStdHandle(STD_ERROR))` lands on host stderr), module handles (mapped
// images plus HLE pseudo handles, non-loading probe semantics) and the process
// environment. Fibers are not implemented: game threads are host pthreads, so
// `IsThreadAFiber` reports false and the conversion entry points fail cleanly.
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

pub const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
pub const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
pub const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

const ERROR_ENVVAR_NOT_FOUND: u32 = 203;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_PROC_NOT_FOUND: u32 = 127;

fn std_handle_value(which: u32) -> usize {
    // Windows HANDLEs for the std streams are the sign-extended constants themselves.
    which as i32 as isize as usize
}

/// `HANDLE GetStdHandle(DWORD)` — pseudo handles for stdin/stdout/stderr backed by
/// host fds 0/1/2 (see `ReadFile`/`WriteFile`); 0 for anything else.
pub unsafe extern "win64" fn GetStdHandle(which: u32) -> usize {
    match which {
        STD_INPUT_HANDLE | STD_OUTPUT_HANDLE | STD_ERROR_HANDLE => {
            set_error(ERROR_SUCCESS);
            std_handle_value(which)
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

/// `BOOL SetStdHandle(DWORD, HANDLE)` — accept the three std pseudo handles (and only
/// those); the mapping is fixed to fds 0/1/2, so this only validates.
pub unsafe extern "win64" fn SetStdHandle(which: u32, h: usize) -> i32 {
    let ok = matches!(which, STD_INPUT_HANDLE | STD_OUTPUT_HANDLE | STD_ERROR_HANDLE)
        && (h == std_handle_value(which));
    if ok {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

fn main_or_zero_base() -> usize {
    crate::modules::main_module().map(|m| m.base).unwrap_or(0)
}

fn module_handle_impl(name: &str) -> usize {
    let key = crate::util::dll_key(name);
    // HLE-routed names hand out the stable pseudo handle (creating it, like LoadLibrary).
    if let Some(m) = crate::hle::registry().hle_for(&key) {
        return crate::modules::hle_handle(m.name);
    }
    // @stub routes are non-loading probes: only a previously seen handle answers.
    if let Some(r) = crate::hle::registry().route(&key) {
        if *r == crate::hle::Route::Stub {
            return crate::modules::find_hle_handle(&key).unwrap_or(0);
        }
    }
    if let Some(m) = crate::modules::find_mapped_by_name(&key) {
        return m.base;
    }
    // No system loader on Linux: unknown names miss (ERROR_MOD_NOT_FOUND at the caller).
    0
}

/// `HMODULE GetModuleHandleW(LPCWSTR)` — NULL is the main image base.
pub unsafe extern "win64" fn GetModuleHandleW(name: *const u16) -> usize {
    match from_wide_ptr(name) {
        None => main_or_zero_base(),
        Some(n) if n.is_empty() => main_or_zero_base(),
        Some(n) => {
            let h = module_handle_impl(&n);
            if h == 0 {
                set_error(ERROR_MOD_NOT_FOUND);
            } else {
                set_error(ERROR_SUCCESS);
            }
            h
        }
    }
}

/// `HMODULE GetModuleHandleA(LPCSTR)` — ANSI (UTF-8 on Linux) form.
pub unsafe extern "win64" fn GetModuleHandleA(name: *const u8) -> usize {
    match from_ansi_ptr(name) {
        None => main_or_zero_base(),
        Some(n) if n.is_empty() => main_or_zero_base(),
        Some(n) => {
            let h = module_handle_impl(&n);
            if h == 0 {
                set_error(ERROR_MOD_NOT_FOUND);
            } else {
                set_error(ERROR_SUCCESS);
            }
            h
        }
    }
}

const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x1;
const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x2;
const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;

/// Shared `GetModuleHandleEx` body: `FROM_ADDRESS` resolves the mapped image containing
/// `addr` (the game's own return address); otherwise it is the non-loading
/// `GetModuleHandle` probe above. NULL name without `FROM_ADDRESS` is the main image.
/// Mirrors the Windows guard order: unknown bits or PIN|UNCHANGED_REFCOUNT fail with
/// ERROR_INVALID_PARAMETER and leave *out null (no refcount hook on Linux — see
/// resolver_linux::system_module, always None — so only validation ports).
fn module_handle_ex(flags: u32, name: Option<String>, addr: usize, out: *mut usize) -> i32 {
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if flags & !(GET_MODULE_HANDLE_EX_FLAG_PIN
        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT
        | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS)
        != 0
        || flags
            & (GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT)
            == (GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT)
    {
        unsafe {
            *out = 0;
        }
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let h = if flags & GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS != 0 {
        match crate::modules::image_base_for_addr(addr) {
            Some(b) => b,
            None => {
                set_error(ERROR_MOD_NOT_FOUND);
                unsafe {
                    *out = 0;
                }
                return 0;
            }
        }
    } else {
        match name {
            None => main_or_zero_base(),
            Some(n) if n.is_empty() => main_or_zero_base(),
            Some(n) => module_handle_impl(&n),
        }
    };
    unsafe {
        *out = h;
    }
    if h == 0 {
        set_error(ERROR_MOD_NOT_FOUND);
        0
    } else {
        set_error(ERROR_SUCCESS);
        1
    }
}

/// `BOOL GetModuleHandleExW(DWORD, LPCWSTR, HMODULE*)`.
pub unsafe extern "win64" fn GetModuleHandleExW(flags: u32, name: *const u16, out: *mut usize) -> i32 {
    let n = if flags & GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS != 0 {
        None
    } else {
        from_wide_ptr(name)
    };
    module_handle_ex(flags, n, name as usize, out)
}

/// `BOOL GetModuleHandleExA(DWORD, LPCSTR, HMODULE*)`.
pub unsafe extern "win64" fn GetModuleHandleExA(flags: u32, name: *const u8, out: *mut usize) -> i32 {
    let n = if flags & GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS != 0 {
        None
    } else {
        from_ansi_ptr(name)
    };
    module_handle_ex(flags, n, name as usize, out)
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `DWORD GetEnvironmentVariableW(LPCWSTR, LPWSTR, DWORD)` — process environment.
/// Returns the chars copied (without NUL) on success, the required size (with NUL)
/// with `ERROR_INSUFFICIENT_BUFFER` when the buffer is too small, or 0 with
/// `ERROR_ENVVAR_NOT_FOUND` when missing.
pub unsafe extern "win64" fn GetEnvironmentVariableW(
    name: *const u16,
    buf: *mut u16,
    size: u32,
) -> u32 {
    let Some(n) = from_wide_ptr(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let Some(v) = env_lookup(&n) else {
        set_error(ERROR_ENVVAR_NOT_FOUND);
        return 0;
    };
    let w: Vec<u16> = v.encode_utf16().collect();
    let need = w.len() + 1;
    if size == 0 || buf.is_null() {
        set_error(ERROR_SUCCESS);
        return need as u32;
    }
    if (size as usize) < need {
        // Fill what fits (truncated, NUL-terminated) and report the required size.
        let n = crate::util::write_wide_out(&v, buf, size);
        let _ = n;
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return need as u32;
    }
    let n = crate::util::write_wide_out(&v, buf, size);
    set_error(ERROR_SUCCESS);
    n
}

/// `DWORD GetEnvironmentVariableA(LPCSTR, LPSTR, DWORD)` — UTF-8 form.
pub unsafe extern "win64" fn GetEnvironmentVariableA(
    name: *const u8,
    buf: *mut u8,
    size: u32,
) -> u32 {
    let Some(n) = from_ansi_ptr(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let Some(v) = env_lookup(&n) else {
        set_error(ERROR_ENVVAR_NOT_FOUND);
        return 0;
    };
    let need = v.len() + 1;
    if size == 0 || buf.is_null() {
        set_error(ERROR_SUCCESS);
        return need as u32;
    }
    if (size as usize) < need {
        let n = crate::util::write_ansi_out(&v, buf, size);
        let _ = n;
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return need as u32;
    }
    let n = crate::util::write_ansi_out(&v, buf, size);
    set_error(ERROR_SUCCESS);
    n
}

// FLS: 128 slots (Windows `FLS_MAXIMUM_AVAILABLE`), process-wide allocation bitmap plus
// per-thread values. Rust std allocates one slot for its thread-locals and never frees
// it; callbacks run on thread exit via `fls_thread_exit` (called from the `CreateThread`
// wrapper and the main-thread exit path).

pub const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
const FLS_SLOTS: usize = 128;

static FLS_STATE: OnceLock<Mutex<([bool; FLS_SLOTS], [usize; FLS_SLOTS])>> = OnceLock::new();

fn fls_state() -> &'static Mutex<([bool; FLS_SLOTS], [usize; FLS_SLOTS])> {
    FLS_STATE.get_or_init(|| Mutex::new(([false; FLS_SLOTS], [0; FLS_SLOTS])))
}

thread_local! {
    static FLS_VALUES: std::cell::RefCell<[usize; FLS_SLOTS]> =
        const { std::cell::RefCell::new([0; FLS_SLOTS]) };
}

/// `DWORD FlsAlloc(PFLS_CALLBACK_FUNCTION)` — first free slot, or `FLS_OUT_OF_INDEXES`.
pub unsafe extern "win64" fn FlsAlloc(callback: usize) -> u32 {
    let mut g = fls_state().lock().unwrap();
    for (i, used) in g.0.iter_mut().enumerate() {
        if !*used {
            *used = true;
            g.1[i] = callback;
            // A reused slot starts clean for this thread; other threads keep their own
            // values (freed slots are never reused by Rust std in practice).
            FLS_VALUES.with(|v| v.borrow_mut()[i] = 0);
            set_error(ERROR_SUCCESS);
            return i as u32;
        }
    }
    set_error(ERROR_NOT_ENOUGH_MEMORY);
    FLS_OUT_OF_INDEXES
}

/// Run the pending FLS callbacks for the dying thread: every non-zero value with a
/// non-zero callback is invoked (win64 ABI) and cleared. Lock-free across the call:
///
/// the callback address and value are copied out first so game code never runs under
/// the global lock.
pub fn fls_thread_exit() {
    let pending: Vec<(usize, usize)> = FLS_VALUES.with(|v| {
        let g = fls_state().lock().unwrap();
        let vals = v.borrow();
        vals.iter()
            .enumerate()
            .filter_map(|(i, &val)| {
                if val != 0 && g.0[i] && g.1[i] != 0 {
                    Some((g.1[i], val))
                } else {
                    None
                }
            })
            .collect()
    });
    type FlsCb = unsafe extern "win64" fn(usize);
    for (cb, val) in pending {
        let f: FlsCb = unsafe { std::mem::transmute(cb) };
        unsafe { f(val) };
    }
    FLS_VALUES.with(|v| {
        let g = fls_state().lock().unwrap();
        let mut vals = v.borrow_mut();
        for (i, slot) in vals.iter_mut().enumerate() {
            if *slot != 0 && g.0[i] && g.1[i] != 0 {
                *slot = 0;
            }
        }
    });
}

/// `BOOL FlsFree(DWORD)` — frees the slot; the current thread's callback runs first
/// when its value is non-zero. Unknown slots fail cleanly.
pub unsafe extern "win64" fn FlsFree(index: u32) -> i32 {
    let i = index as usize;
    if i >= FLS_SLOTS {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let (cb, val) = {
        let g = fls_state().lock().unwrap();
        if !g.0[i] {
            set_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        (g.1[i], FLS_VALUES.with(|v| v.borrow()[i]))
    };
    if val != 0 && cb != 0 {
        let f: unsafe extern "win64" fn(usize) = unsafe { std::mem::transmute(cb) };
        unsafe { f(val) };
        FLS_VALUES.with(|v| v.borrow_mut()[i] = 0);
    }
    {
        let mut g = fls_state().lock().unwrap();
        g.0[i] = false;
        g.1[i] = 0;
    }
    FLS_VALUES.with(|v| v.borrow_mut()[i] = 0);
    set_error(ERROR_SUCCESS);
    1
}

/// `PVOID FlsGetValue(DWORD)` — the calling thread's value, or NULL on a bad index.
pub unsafe extern "win64" fn FlsGetValue(index: u32) -> usize {
    let i = index as usize;
    if i >= FLS_SLOTS {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if !fls_state().lock().unwrap().0[i] {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    set_error(ERROR_SUCCESS);
    FLS_VALUES.with(|v| v.borrow()[i])
}

/// `BOOL FlsSetValue(DWORD, PVOID)` — stores the calling thread's value.
pub unsafe extern "win64" fn FlsSetValue(index: u32, data: usize) -> i32 {
    let i = index as usize;
    if i >= FLS_SLOTS {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if !fls_state().lock().unwrap().0[i] {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    FLS_VALUES.with(|v| v.borrow_mut()[i] = data);
    set_error(ERROR_SUCCESS);
    1
}

// Fibers: game threads are host pthreads, never fibers. `IsThreadAFiber` reports false;
// the conversion entry points fail cleanly instead of aborting the title.

/// `BOOL IsThreadAFiber(void)` — always false on Linux (threads, never fibers).
pub unsafe extern "win64" fn IsThreadAFiber() -> i32 {
    set_error(ERROR_SUCCESS);
    0
}

/// `LPVOID ConvertThreadToFiber(LPVOID)` — no fiber support; always NULL.
pub unsafe extern "win64" fn ConvertThreadToFiber(_param: *const c_void) -> usize {
    set_error(ERROR_INVALID_PARAMETER);
    0
}

/// `BOOL ConvertFiberToThread(void)` — there is nothing to convert; fail cleanly.
pub unsafe extern "win64" fn ConvertFiberToThread() -> i32 {
    set_error(ERROR_INVALID_PARAMETER);
    0
}

/// `LPVOID CreateFiber(SIZE_T, LPFIBER_START_ROUTINE, LPVOID)` — no fibers; NULL.
pub unsafe extern "win64" fn CreateFiber(
    _stack: usize,
    _start: usize,
    _param: *mut c_void,
) -> usize {
    set_error(ERROR_INVALID_PARAMETER);
    0
}

/// `void DeleteFiber(LPVOID)` — no-op (there is nothing allocated).
pub unsafe extern "win64" fn DeleteFiber(_fiber: usize) {}

/// `void SwitchToFiber(LPVOID)` — no-op (the target never exists).
pub unsafe extern "win64" fn SwitchToFiber(_fiber: usize) {}

// ---------------------------------------------------------------------------------------
// Exception / console shims (Phase C step 11d)
// ---------------------------------------------------------------------------------------
//
// The cross-built title's Rust runtime takes its panic-report path through these:
// `lang_start_internal` registers a stack-overflow vectored handler and sets the
// thread stack guarantee, `std::sys::stdio::windows::write` probes `GetConsoleMode`
// (redirected stderr fails, as on Windows) and then writes through `NtWriteFile`,
// and `SetThreadDescription::load` probes `GetProcAddress` (a miss is a benign
// fallback, as on Windows). The auto-stubs returned 0 for all of them: `NtWriteFile`
// reported success without filling the `IO_STATUS_BLOCK`, so the write looked like
// zero bytes and the panic message was lost on the way to the `int 29` fastfail.
//
// Calling convention: every export is `unsafe extern "win64"` (DEC-013).

/// Process-wide vectored-exception-handler table: handle → (first, handler).
/// Full dispatch (running these on a fault) is later Phase C work; recording them
/// unblocks `lang_start_internal`, which ignores the return otherwise.
static VEH: OnceLock<Mutex<HashMap<usize, (u32, usize)>>> = OnceLock::new();

fn veh() -> &'static Mutex<HashMap<usize, (u32, usize)>> {
    VEH.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `PVOID AddVectoredExceptionHandler(ULONG, PVECTORED_EXCEPTION_HANDLER)` —
/// a non-zero handle, or 0 on a null handler.
pub unsafe extern "win64" fn AddVectoredExceptionHandler(first: u32, handler: usize) -> usize {
    if handler == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    veh().lock().unwrap().insert(h, (first, handler));
    set_error(ERROR_SUCCESS);
    crate::xlog_debug!("AddVectoredExceptionHandler(first={first}): handle {h:#x}");
    h
}

/// `ULONG RemoveVectoredExceptionHandler(PVOID)` — 1 when the handle was registered.
pub unsafe extern "win64" fn RemoveVectoredExceptionHandler(handle: usize) -> u32 {
    if veh().lock().unwrap().remove(&handle).is_some() {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

/// `FARPROC GetProcAddress(HMODULE, LPCSTR)` — HLE/mapped lookups through the
/// Linux resolver (never a stub thunk, so optional-API probing keeps working);
/// a miss is NULL with `ERROR_PROC_NOT_FOUND`, as on Windows.
pub unsafe extern "win64" fn GetProcAddress(h: usize, name: *const u8) -> usize {
    if name.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // Ordinals arrive as small integers, names as pointers (mirrors the Windows twin).
    let what = if (name as usize) < 0x1_0000 {
        ImportName::Ordinal(name as usize as u16)
    } else {
        match from_ansi_ptr(name) {
            Some(n) => ImportName::Name { hint: 0, name: n },
            None => {
                set_error(ERROR_INVALID_PARAMETER);
                return 0;
            }
        }
    };
    let h = if h == 0 { main_or_zero_base() } else { h };
    match crate::resolver::get_proc_address(h, &what) {
        Some(a) => {
            set_error(ERROR_SUCCESS);
            a
        }
        None => {
            set_error(ERROR_PROC_NOT_FOUND);
            0
        }
    }
}

/// `HMODULE LoadLibraryW(LPCWSTR)` — HLE pseudo-handles, mapped bases and the
/// game-DLL load path through `resolver::load_library`; NULL names fail with
/// `ERROR_INVALID_PARAMETER`, misses with `ERROR_MOD_NOT_FOUND`, as on Windows.
pub unsafe extern "win64" fn LoadLibraryW(name: *const u16) -> usize {
    load_library_impl(from_wide_ptr(name))
}

/// `HMODULE LoadLibraryA(LPCSTR)` — ANSI (UTF-8 on Linux) form.
pub unsafe extern "win64" fn LoadLibraryA(name: *const u8) -> usize {
    load_library_impl(from_ansi_ptr(name))
}

fn load_library_impl(name: Option<String>) -> usize {
    let Some(n) = name else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    match crate::resolver::load_library(&n) {
        Some(h) => {
            crate::xlog_debug!("LoadLibrary({n}) → {h:#x}");
            set_error(ERROR_SUCCESS);
            h
        }
        None => {
            crate::xlog_warn!("LoadLibrary({n}) → NULL (not found)");
            set_error(ERROR_MOD_NOT_FOUND);
            0
        }
    }
}

/// `HMODULE LoadLibraryExW(LPCWSTR, HANDLE, DWORD)` — data-file flags have no
/// separate mapping on Linux (no resource-only view), so every flag combination
/// takes the same `load_library` path; a NULL name still fails clean.
pub unsafe extern "win64" fn LoadLibraryExW(name: *const u16, _file: usize, _flags: u32) -> usize {
    load_library_impl(from_wide_ptr(name))
}

/// `HMODULE LoadLibraryExA(LPCSTR, HANDLE, DWORD)` — ANSI form, same as above.
pub unsafe extern "win64" fn LoadLibraryExA(name: *const u8, _file: usize, _flags: u32) -> usize {
    load_library_impl(from_ansi_ptr(name))
}

/// `HMODULE LoadPackagedLibrary(LPCWSTR, DWORD)` — packaged titles resolve like
/// `LoadLibraryW` on Linux; reserved must be 0.
pub unsafe extern "win64" fn LoadPackagedLibrary(name: *const u16, _reserved: u32) -> usize {
    load_library_impl(from_wide_ptr(name))
}

fn free_library_known(h: usize) -> bool {
    if h == 0 {
        return false;
    }
    if h == main_or_zero_base() {
        return true;
    }
    if crate::modules::hle_name_by_handle(h).is_some() {
        return true;
    }
    crate::modules::find_mapped_by_base(h).is_some()
}

/// `BOOL FreeLibrary(HMODULE)` — XCL never unloads: known HLE/mapped/main
/// handles succeed without unloading; unknown handles fail, as on Windows.
pub unsafe extern "win64" fn FreeLibrary(h: usize) -> i32 {
    if free_library_known(h) {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

/// `void FreeLibraryWhenCallbackReturns(PVOID, HMODULE)` — no unload on Linux,
/// so known handles are a no-op (mirrors the Windows twin's early return for
/// XCL-mapped modules).
pub unsafe extern "win64" fn FreeLibraryWhenCallbackReturns(_instance: usize, _h: usize) {}

/// `BOOL DisableThreadLibraryCalls(HMODULE)` — records the opt-out on mapped
/// game DLLs; HLE/main handles trivially succeed.
pub unsafe extern "win64" fn DisableThreadLibraryCalls(h: usize) -> i32 {
    if let Some(m) = crate::modules::find_mapped_by_base(h) {
        m.no_thread_calls.store(true, std::sync::atomic::Ordering::Relaxed);
        set_error(ERROR_SUCCESS);
        return 1;
    }
    if free_library_known(h) {
        set_error(ERROR_SUCCESS);
        1
    } else {
        set_error(ERROR_INVALID_PARAMETER);
        0
    }
}

/// `BOOL GetConsoleMode(HANDLE, LPDWORD)` — succeeds only for the std pseudo
/// handles when the backing fd is a tty (a redirected stderr fails with
/// `ERROR_INVALID_HANDLE`, steering Rust's `write` onto the `NtWriteFile` path,
/// as on Windows). Console output defaults to `ENABLE_PROCESSED_OUTPUT |
/// ENABLE_WRAP_AT_EOL_OUTPUT`; input to the usual cooked set.
pub unsafe extern "win64" fn GetConsoleMode(h: usize, mode: *mut u32) -> i32 {
    if mode.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let fd = if h == std_handle_value(STD_INPUT_HANDLE) {
        0
    } else if h == std_handle_value(STD_OUTPUT_HANDLE) {
        1
    } else if h == std_handle_value(STD_ERROR_HANDLE) {
        2
    } else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if unsafe { libc::isatty(fd) } != 1 {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    *mode = if fd == 0 { 0x1F7 } else { 0x7 };
    set_error(ERROR_SUCCESS);
    1
}

/// `BOOL SetConsoleMode(HANDLE, DWORD)` — accepted for the std pseudo handles
/// when the backing fd is a tty; anything else fails, as on Windows.
pub unsafe extern "win64" fn SetConsoleMode(h: usize, _mode: u32) -> i32 {
    let fd = if h == std_handle_value(STD_INPUT_HANDLE) {
        0
    } else if h == std_handle_value(STD_OUTPUT_HANDLE) {
        1
    } else if h == std_handle_value(STD_ERROR_HANDLE) {
        2
    } else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if unsafe { libc::isatty(fd) } != 1 {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    set_error(ERROR_SUCCESS);
    1
}

/// `UINT GetConsoleOutputCP(void)` — `CP_UTF8` (65001), so Rust's console write
/// takes the UTF-8 `NtWriteFile` path instead of the `WriteConsoleW` path.
pub unsafe extern "win64" fn GetConsoleOutputCP() -> u32 {
    65001
}

/// `BOOL SetThreadStackGuarantee(PULONG)` — host stacks grow on demand, so the
/// guarantee is trivially satisfied; the value is left in place, as on Windows.
pub unsafe extern "win64" fn SetThreadStackGuarantee(size: *mut u32) -> i32 {
    if size.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    set_error(ERROR_SUCCESS);
    1
}

// `NTSTATUS NtWriteFile(HANDLE, HANDLE, PIO_APC_ROUTINE, PVOID, PIO_STATUS_BLOCK,
// PVOID, ULONG, PLARGE_INTEGER, PULONG)`: the console path Rust's
// `synchronous_write` takes. Forwards to the host fd (std pseudos) or the VFS
// file table, fills the `IO_STATUS_BLOCK` (`Status`, `Information =
// bytes-written`) and returns `STATUS_SUCCESS`. Event/APC/offset/key are
// synchronous no-ops here.

/// Windows `IO_STATUS_BLOCK` (16 bytes on x86-64): status + bytes-transferred.
#[repr(C)]
pub struct IoStatusBlock {
    pub status: u32,
    pub _pad: u32,
    pub info: usize,
}

const STATUS_SUCCESS: u32 = 0;
const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;

fn iosb_write(out: *mut IoStatusBlock, status: u32, info: usize) -> u32 {
    if !out.is_null() {
        unsafe {
            (*out).status = status;
            (*out)._pad = 0;
            (*out).info = info;
        }
    }
    status
}

fn fd_write_all(fd: i32, data: &[u8]) -> Result<usize, u32> {
    let mut written = 0usize;
    while written < data.len() {
        let r = unsafe {
            libc::write(
                fd,
                data.as_ptr().add(written) as *const c_void,
                data.len() - written,
            )
        };
        if r < 0 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(5);
            if e == libc::EINTR {
                continue;
            }
            return Err(e as u32);
        }
        // A zero return cannot happen for these sizes; guard against a spin anyway.
        if r == 0 {
            break;
        }
        written += r as usize;
        break; // pipes/ttys accept the write in one go for these sizes.
    }
    Ok(written)
}

pub unsafe extern "win64" fn NtWriteFile(
    handle: usize,
    _event: usize,
    _apc: usize,
    _apc_ctx: usize,
    iosb: *mut IoStatusBlock,
    buf: *const u8,
    len: u32,
    _byte_offset: *const i64,
    _key: *mut u32,
) -> u32 {
    if len == 0 {
        return iosb_write(iosb, STATUS_SUCCESS, 0);
    }
    if buf.is_null() {
        return iosb_write(iosb, STATUS_INVALID_HANDLE, 0);
    }
    let data = unsafe { std::slice::from_raw_parts(buf, len as usize) };
    // Console pseudos land on the host fds, as in `WriteFile`.
    if handle == std_handle_value(STD_OUTPUT_HANDLE) || handle == std_handle_value(STD_ERROR_HANDLE) {
        let fd = if handle == std_handle_value(STD_OUTPUT_HANDLE) { 1 } else { 2 };
        return match fd_write_all(fd, data) {
            Ok(k) => iosb_write(iosb, STATUS_SUCCESS, k),
            Err(_) => iosb_write(iosb, STATUS_UNSUCCESSFUL, 0),
        };
    }
    if handle == std_handle_value(STD_INPUT_HANDLE) {
        return iosb_write(iosb, STATUS_INVALID_HANDLE, 0);
    }
    let arc = files().lock().unwrap().get(&handle).cloned();
    let Some(arc) = arc else {
        return iosb_write(iosb, STATUS_INVALID_HANDLE, 0);
    };
    let mut g = arc.lock().unwrap();
    match std::io::Write::write(&mut *g, data) {
        Ok(k) => iosb_write(iosb, STATUS_SUCCESS, k),
        Err(_) => iosb_write(iosb, STATUS_UNSUCCESSFUL, 0),
    }
}

// ---------------------------------------------------------------------------------------
// Step-11f SEH search: `.pdata` reader plus Rtl* HLEs.
//
// The Linux host has no `RtlAddFunctionTable`: the mapped image already holds the
// Windows unwind tables, so lookup/virtual-unwind read them in place through
// `crate::pdata`. Step-11g adds the dispatch slice: real `RtlCaptureContext` /
// `RtlRestoreContext` naked shims plus `RaiseException` two-phase search/unwind
// and `RtlUnwindEx` targeting, so Rust `catch_unwind` (GCC 0x20474343) can reach
// its handler instead of aborting with error 5.
// ---------------------------------------------------------------------------------------

const CTX_FLAGS_OFF: usize = 0x30;
const CONTEXT_SIZE: usize = 0x4D0;
const CONTEXT_FULL_AMD64: u32 = 0x10000B;

const CTX_RAX: usize = 0x78;
const CTX_RCX: usize = 0x80;
const CTX_RDX: usize = 0x88;
const CTX_RBX: usize = 0x90;
const CTX_RSP: usize = 0x98;
const CTX_RBP: usize = 0xA0;
const CTX_RSI: usize = 0xA8;
const CTX_RDI: usize = 0xB0;
const CTX_R8: usize = 0xB8;
const CTX_R9: usize = 0xC0;
const CTX_R10: usize = 0xC8;
const CTX_R11: usize = 0xD0;
const CTX_R12: usize = 0xD8;
const CTX_R13: usize = 0xE0;
const CTX_R14: usize = 0xE8;
const CTX_R15: usize = 0xF0;
const CTX_RIP: usize = 0xF8;

unsafe fn ctx_u64(ctx: *const u8, off: usize) -> Option<u64> {
    if ctx.is_null() {
        return None;
    }
    Some(unsafe { std::ptr::read_unaligned(ctx.add(off) as *const u64) })
}

unsafe fn ctx_set_u64(ctx: *mut u8, off: usize, val: u64) -> bool {
    if ctx.is_null() {
        return false;
    }
    unsafe {
        std::ptr::write_unaligned(ctx.add(off) as *mut u64, val);
    }
    true
}

fn ctx_to_regs(ctx: *const u8) -> Option<crate::pdata::IntRegs> {
    unsafe {
        Some(crate::pdata::IntRegs {
            rax: ctx_u64(ctx, CTX_RAX)?,
            rcx: ctx_u64(ctx, CTX_RCX)?,
            rdx: ctx_u64(ctx, CTX_RDX)?,
            rbx: ctx_u64(ctx, CTX_RBX)?,
            rsp: ctx_u64(ctx, CTX_RSP)?,
            rbp: ctx_u64(ctx, CTX_RBP)?,
            rsi: ctx_u64(ctx, CTX_RSI)?,
            rdi: ctx_u64(ctx, CTX_RDI)?,
            r8: ctx_u64(ctx, CTX_R8)?,
            r9: ctx_u64(ctx, CTX_R9)?,
            r10: ctx_u64(ctx, CTX_R10)?,
            r11: ctx_u64(ctx, CTX_R11)?,
            r12: ctx_u64(ctx, CTX_R12)?,
            r13: ctx_u64(ctx, CTX_R13)?,
            r14: ctx_u64(ctx, CTX_R14)?,
            r15: ctx_u64(ctx, CTX_R15)?,
            rip: ctx_u64(ctx, CTX_RIP)?,
        })
    }
}

fn regs_to_ctx(regs: &crate::pdata::IntRegs, ctx: *mut u8) {
    let pairs = [
        (CTX_RAX, regs.rax),
        (CTX_RCX, regs.rcx),
        (CTX_RDX, regs.rdx),
        (CTX_RBX, regs.rbx),
        (CTX_RSP, regs.rsp),
        (CTX_RBP, regs.rbp),
        (CTX_RSI, regs.rsi),
        (CTX_RDI, regs.rdi),
        (CTX_R8, regs.r8),
        (CTX_R9, regs.r9),
        (CTX_R10, regs.r10),
        (CTX_R11, regs.r11),
        (CTX_R12, regs.r12),
        (CTX_R13, regs.r13),
        (CTX_R14, regs.r14),
        (CTX_R15, regs.r15),
        (CTX_RIP, regs.rip),
    ];
    for (off, val) in pairs {
        unsafe {
            ctx_set_u64(ctx, off, val);
        }
    }
}

/// Fault-free 8-byte load from the game's stack (the game runs on host stacks,
/// so a wrong `RSP` guess must fail closed instead of `SIGSEGV`-ing the host).
fn safe_read_u64(addr: u64) -> Option<u64> {
    if addr == 0 || addr.checked_add(8).is_none() {
        return None;
    }
    let mut out = 0u64;
    let local = libc::iovec {
        iov_base: &mut out as *mut _ as *mut c_void,
        iov_len: 8,
    };
    let remote = libc::iovec {
        iov_base: addr as *mut c_void,
        iov_len: 8,
    };
    let n = unsafe {
        libc::process_vm_readv(
            std::process::id() as i32,
            &local as *const _,
            1,
            &remote as *const _,
            1,
            0,
        )
    };
    if n == 8 {
        Some(out)
    } else {
        None
    }
}

fn exception_dir_of(base: usize, size: usize) -> Option<(u32, u32)> {
    if base == 0 || size == 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, size) };
    let pe = crate::pe::Pe::parse(bytes, crate::pe::Layout::Mapped).ok()?;
    let dir = pe.dir(crate::pe::DIR_EXCEPTION);
    if dir.rva == 0 || dir.size == 0 {
        return None;
    }
    Some((dir.rva, dir.size))
}

/// `PVOID RtlPcToFileHeader(PVOID Pc, PVOID *Base)` — image base for a PC, via
/// the loader registry (the Windows hook this replaces only knew modules on the
/// Windows loader list).
pub unsafe extern "win64" fn RtlPcToFileHeader(pc: usize, base_out: *mut usize) -> usize {
    match crate::modules::image_base_for_addr(pc) {
        Some(base) => {
            if !base_out.is_null() {
                unsafe {
                    *base_out = base;
                }
            }
            set_error(ERROR_SUCCESS);
            base
        }
        None => {
            if !base_out.is_null() {
                unsafe {
                    *base_out = 0;
                }
            }
            0
        }
    }
}

/// `PRUNTIME_FUNCTION RtlLookupFunctionEntry(DWORD64 Pc, PDWORD64 Base,
/// PUNWIND_HISTORY_TABLE History)` — pointer into the mapped `.pdata`, or
/// NULL when the PC is not in a mapped image with exception data. `History`
/// is ignored (no caching in this slice).
pub unsafe extern "win64" fn RtlLookupFunctionEntry(
    control_pc: usize,
    image_base_out: *mut usize,
    _history: *mut u8,
) -> usize {
    let Some(m) = crate::modules::find_mapped_by_addr(control_pc) else {
        return 0;
    };
    let Some((pdata_rva, pdata_size)) = exception_dir_of(m.base, m.size) else {
        return 0;
    };
    let table_bytes = match crate::pdata::guest_bytes(m.base, m.size, pdata_rva, pdata_size as usize) {
        Some(b) => b,
        None => return 0,
    };
    let table = match crate::pdata::parse_runtime_functions(&table_bytes) {
        Ok(t) => t,
        Err(e) => {
            crate::xlog_warn!("RtlLookupFunctionEntry: .pdata parse failed: {e}");
            return 0;
        }
    };
    let control_rva = match u32::try_from(control_pc.wrapping_sub(m.base)) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let Some(entry) = crate::pdata::lookup(&table, control_rva) else {
        return 0;
    };
    let index = (entry as *const _ as usize - table.as_ptr() as usize) / 12;
    if !image_base_out.is_null() {
        unsafe {
            *image_base_out = m.base;
        }
    }
    m.base.wrapping_add(pdata_rva as usize).wrapping_add(index * 12)
}

/// `BOOLEAN RtlAddFunctionTable(PRUNTIME_FUNCTION Table, DWORD Count,
/// DWORD64 Base)` — no OS registration on Linux; validate the table sits in a
/// mapped image and report success so the title's startup path proceeds. The
/// unwinder reads `.pdata` in place on every lookup.
pub unsafe extern "win64" fn RtlAddFunctionTable(table: usize, count: u32, base: usize) -> i32 {
    if table == 0 || count == 0 || base == 0 {
        return 0;
    }
    let len = (count as usize).checked_mul(12);
    let Some(len) = len else {
        return 0;
    };
    if crate::modules::find_mapped_by_addr(table).is_none()
        && crate::modules::find_mapped_by_addr(table.wrapping_add(len).wrapping_sub(1)).is_none()
    {
        crate::xlog_warn!("RtlAddFunctionTable({table:#x}, {count}, {base:#x}): outside mapped images");
        return 0;
    }
    set_error(ERROR_SUCCESS);
    1
}

/// `PEXCEPTION_ROUTINE RtlVirtualUnwind(DWORD HandlerType, DWORD64 ImageBase,
/// DWORD64 ControlPc, PRUNTIME_FUNCTION Entry, PCONTEXT Ctx, PVOID *Data,
/// PDWORD64 Establisher, PKNONVOLATILE_CONTEXT_POINTERS Ptrs)` — unwind one
/// frame for the SEH search. Returns the language-specific handler or NULL.
/// Unsupported ops fail closed (NULL) with a warn log; `Ptrs` is ignored.
pub unsafe extern "win64" fn RtlVirtualUnwind(
    _handler_type: u32,
    image_base: usize,
    control_pc: usize,
    function_entry: usize,
    context: *mut u8,
    handler_data_out: *mut usize,
    establisher_out: *mut usize,
    _context_pointers: *mut u8,
) -> usize {
    if context.is_null() {
        return 0;
    }
    let Some(m) = crate::modules::find_mapped_by_addr(control_pc) else {
        return 0;
    };
    let base = if image_base != 0 { image_base } else { m.base };
    // Function entry: use the caller's pointer when given, else look it up.
    let (begin_rva, end_rva, unwind_rva) = if function_entry != 0 {
        let r = |off: usize| unsafe { std::ptr::read_unaligned((function_entry + off) as *const u32) };
        // The pointer must live in a mapped image (or the HLE pseudo-table area);
        // a garbage pointer fails closed instead of faulting the host.
        if crate::modules::find_mapped_by_addr(function_entry).is_none() {
            // Still allow HLE-allocated tables? No — only mapped images in this slice.
            crate::xlog_warn!("RtlVirtualUnwind: FunctionEntry {function_entry:#x} outside mapped images");
            return 0;
        }
        (r(0), r(4), r(8))
    } else {
        let Some((pdata_rva, pdata_size)) = exception_dir_of(m.base, m.size) else {
            return 0;
        };
        let Some(bytes) = crate::pdata::guest_bytes(m.base, m.size, pdata_rva, pdata_size as usize) else {
            return 0;
        };
        let Ok(table) = crate::pdata::parse_runtime_functions(&bytes) else {
            return 0;
        };
        let Ok(rva) = u32::try_from(control_pc.wrapping_sub(m.base)) else {
            return 0;
        };
        let Some(f) = crate::pdata::lookup(&table, rva) else {
            return 0;
        };
        (f.begin_rva, f.end_rva, f.unwind_rva)
    };
    // Unwind bytes: header + codes + handler. 1 KiB covers the largest test-title
    // entry (514 + handler); entries past the image end fail closed.
    let Some(raw) = crate::pdata::guest_bytes(base, m.size, unwind_rva, 1024) else {
        // The unwind RVA may sit past the 1 KiB window check but inside the image:
        // fall back to a tight read of just the header to learn the real length.
        let Some(hdr) = crate::pdata::guest_bytes(base, m.size, unwind_rva, 4) else {
            return 0;
        };
        let count = hdr[2] as usize;
        let need = 4 + count * 2 + 8;
        let Some(full) = crate::pdata::guest_bytes(base, m.size, unwind_rva, need) else {
            return 0;
        };
        let Ok(info) = crate::pdata::parse_unwind_info(&full) else {
            return 0;
        };
        return virtual_unwind_with_info(base, &crate::pdata::RuntimeFunction { begin_rva: begin_rva, end_rva: end_rva, unwind_rva }, &info, context, handler_data_out, establisher_out);
    };
    let Ok(info) = crate::pdata::parse_unwind_info(&raw) else {
        crate::xlog_warn!("RtlVirtualUnwind: UNWIND_INFO parse failed at {unwind_rva:#x}");
        return 0;
    };
    virtual_unwind_with_info(
        base,
        &crate::pdata::RuntimeFunction { begin_rva: begin_rva, end_rva: end_rva, unwind_rva },
        &info,
        context,
        handler_data_out,
        establisher_out,
    )
}

fn handler_data_va(
    image_base: usize,
    func: &crate::pdata::RuntimeFunction,
    info: &crate::pdata::UnwindInfo,
) -> usize {
    if info.handler_rva.is_none() {
        return 0;
    }
    let data_slot = (info.slots + 1) & !1;
    let data_off = 4 + data_slot * 2;
    // Handler RVA (4 bytes) then language-specific data.
    image_base.wrapping_add(func.unwind_rva as usize + data_off + 4)
}

fn virtual_unwind_with_info(
    image_base: usize,
    func: &crate::pdata::RuntimeFunction,
    info: &crate::pdata::UnwindInfo,
    context: *mut u8,
    handler_data_out: *mut usize,
    establisher_out: *mut usize,
) -> usize {
    let Some(mut regs) = ctx_to_regs(context as *const u8) else {
        return 0;
    };
    let read = |addr: u64| safe_read_u64(addr);
    match crate::pdata::unwind_frame(&mut regs, &read, image_base as u64, func, info) {
        Ok(out) => {
            regs_to_ctx(&regs, context);
            if !establisher_out.is_null() {
                unsafe {
                    *establisher_out = out.establisher as usize;
                }
            }
            if !handler_data_out.is_null() {
                // Language-specific data follows the handler RVA (cf. MSDN
                // GetLanguageSpecificDataPtr). `_GCC_specific_handler` reads the
                // LSDA through this pointer, so it must be exact, not the
                // unwind base.
                let data = if info.handler_rva.is_some() {
                    handler_data_va(image_base, func, info)
                } else {
                    0
                };
                unsafe {
                    *handler_data_out = data;
                }
            }
            out.handler.map(|h| h as usize).unwrap_or(0)
        }
        Err(e) => {
            crate::xlog_warn!("RtlVirtualUnwind: {e}");
            0
        }
    }
}

/// `VOID RtlCaptureContext(PCONTEXT)` — real naked shim: stores the caller's
/// integer registers, `RSP` (caller `RSP` = `RSP+8`) and `RIP` (`[RSP]`) plus
/// `CONTEXT_FULL` flags. Volatile `XMM`/segments are left untouched; the
/// unwinder only needs the integer slice.
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlCaptureContext(ctx: *mut u8) {
    core::arch::naked_asm!(
        "mov QWORD PTR [rcx+0x78], rax",
        "mov QWORD PTR [rcx+0x80], rcx",
        "mov QWORD PTR [rcx+0x88], rdx",
        "mov QWORD PTR [rcx+0x90], rbx",
        "lea rax, [rsp+8]",
        "mov QWORD PTR [rcx+0x98], rax",
        "mov QWORD PTR [rcx+0xA0], rbp",
        "mov QWORD PTR [rcx+0xA8], rsi",
        "mov QWORD PTR [rcx+0xB0], rdi",
        "mov QWORD PTR [rcx+0xB8], r8",
        "mov QWORD PTR [rcx+0xC0], r9",
        "mov QWORD PTR [rcx+0xC8], r10",
        "mov QWORD PTR [rcx+0xD0], r11",
        "mov QWORD PTR [rcx+0xD8], r12",
        "mov QWORD PTR [rcx+0xE0], r13",
        "mov QWORD PTR [rcx+0xE8], r14",
        "mov QWORD PTR [rcx+0xF0], r15",
        "mov rax, QWORD PTR [rsp]",
        "mov QWORD PTR [rcx+0xF8], rax",
        "mov DWORD PTR [rcx+0x30], 0x10000B",
        "ret",
    )
}

/// `VOID RtlRestoreContext(PCONTEXT, PEXCEPTION_RECORD)` — real naked shim:
/// restores integer registers from `CONTEXT` and jumps to `RIP` with `RSP`.
/// `R11` holds the target `RIP` across the switch; it is volatile on Windows
/// x64 so leaving it as the target is harmless. The second argument is ignored.
#[unsafe(naked)]
pub unsafe extern "win64" fn RtlRestoreContext(ctx: *mut u8, _e: usize) {
    core::arch::naked_asm!(
        "mov rax, rcx",
        "mov rbx, QWORD PTR [rax+0x90]",
        "mov rbp, QWORD PTR [rax+0xA0]",
        "mov rsi, QWORD PTR [rax+0xA8]",
        "mov rdi, QWORD PTR [rax+0xB0]",
        "mov r12, QWORD PTR [rax+0xD8]",
        "mov r13, QWORD PTR [rax+0xE0]",
        "mov r14, QWORD PTR [rax+0xE8]",
        "mov r15, QWORD PTR [rax+0xF0]",
        "mov r10, QWORD PTR [rax+0xC8]",
        "mov r8, QWORD PTR [rax+0xB8]",
        "mov r9, QWORD PTR [rax+0xC0]",
        "mov rdx, QWORD PTR [rax+0x88]",
        "mov r11, QWORD PTR [rax+0xF8]",
        "mov rcx, QWORD PTR [rax+0x80]",
        "mov rsp, QWORD PTR [rax+0x98]",
        "mov rax, QWORD PTR [rax+0x78]",
        "jmp r11",
    )
}

// ---------------------------------------------------------------------------------------
// Step-11g SEH dispatch: RaiseException + RtlUnwindEx.
// ---------------------------------------------------------------------------------------
//
// `RaiseException` performs the Windows two-phase dispatch: VEH first, then an
// SEH search (`RtlVirtualUnwind` + language-handler calls without `UNWIND`),
// then an unwind pass (`UNWIND` set) up to the handler that returned
// `ExecuteHandler`. The GCC personality (`_GCC_specific_handler` via
// `rust_eh_personality`) does its own `RtlUnwindEx` call during the unwind
// pass to transfer to the catch block, so `RaiseException` itself only drives
// the handler calls — the final jump happens inside `RtlUnwindEx`.
//
// All guest-handler calls use the `win64` ABI (DEC-013). Faults inside a guest
// personality crash the host with the normal report path (parity with Windows
// nested faults); our own reads use `safe_read_u64` and fail closed.

const EXCEPTION_UNWIND: u32 = 0x2;
const EXCEPTION_EXIT_UNWIND: u32 = 0x4;
const EXCEPTION_TARGET_UNWIND: u32 = 0x20;
const EXCEPTION_COLLIDED_UNWIND: u32 = 0x40;

const DISP_CONTINUE_EXECUTION: u32 = 0;
const DISP_CONTINUE_SEARCH: u32 = 1;
const DISP_COLLIDED_UNWIND: u32 = 3;
const DISP_EXECUTE_HANDLER: u32 = 4;

const MAX_UNWIND_FRAMES: usize = 128;

#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: usize,
    addr: usize,
    num_params: u32,
    _pad: u32,
    info: [usize; 15],
}

#[repr(C)]
struct DispatcherContext {
    control_pc: u64,
    image_base: u64,
    function_entry: u64,
    establisher: u64,
    target_ip: u64,
    context: u64,
    language_handler: u64,
    handler_data: u64,
    history_table: u64,
    scope_index: u32,
    _pad: u32,
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut u8,
}

type LanguageHandlerFn =
    unsafe extern "win64" fn(*mut ExceptionRecord, usize, *mut u8, *mut DispatcherContext) -> u32;
type VehHandlerFn = unsafe extern "win64" fn(*mut ExceptionPointers) -> i32;

fn zero_context(ctx: *mut u8) {
    if ctx.is_null() {
        return;
    }
    unsafe {
        std::ptr::write_bytes(ctx, 0, CONTEXT_SIZE);
        std::ptr::write_unaligned(ctx.add(CTX_FLAGS_OFF) as *mut u32, CONTEXT_FULL_AMD64);
    }
}

/// Registers captured by the naked `RaiseException` trampoline before any Rust
/// prologue runs. Volatile `RAX`/`RCX`/`RDX`/`R8-R11` are not captured (they
/// hold the exception arguments or are clobbered by the call); the dispatcher
/// zeroes them in the initial `CONTEXT`.
#[repr(C)]
struct InitialCapture {
    rsp: u64,
    rip: u64,
    rbx: u64,
    rbp: u64,
    rsi: u64,
    rdi: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

fn build_initial_context(ctx: *mut u8, cap: &InitialCapture) {
    zero_context(ctx);
    unsafe {
        ctx_set_u64(ctx, CTX_RAX, 0);
        ctx_set_u64(ctx, CTX_RCX, 0);
        ctx_set_u64(ctx, CTX_RDX, 0);
        ctx_set_u64(ctx, CTX_RBX, cap.rbx);
        ctx_set_u64(ctx, CTX_RSP, cap.rsp);
        ctx_set_u64(ctx, CTX_RBP, cap.rbp);
        ctx_set_u64(ctx, CTX_RSI, cap.rsi);
        ctx_set_u64(ctx, CTX_RDI, cap.rdi);
        ctx_set_u64(ctx, CTX_R8, 0);
        ctx_set_u64(ctx, CTX_R9, 0);
        ctx_set_u64(ctx, CTX_R10, 0);
        ctx_set_u64(ctx, CTX_R11, 0);
        ctx_set_u64(ctx, CTX_R12, cap.r12);
        ctx_set_u64(ctx, CTX_R13, cap.r13);
        ctx_set_u64(ctx, CTX_R14, cap.r14);
        ctx_set_u64(ctx, CTX_R15, cap.r15);
        ctx_set_u64(ctx, CTX_RIP, cap.rip);
    }
}

fn call_language_handler(
    handler: usize,
    record: *mut ExceptionRecord,
    establisher: usize,
    context: *mut u8,
    dispatcher: *mut DispatcherContext,
) -> u32 {
    if handler == 0 {
        return DISP_CONTINUE_SEARCH;
    }
    let f: LanguageHandlerFn = unsafe { std::mem::transmute(handler) };
    unsafe { f(record, establisher, context, dispatcher) }
}

fn call_veh_chain(record: *mut ExceptionRecord, context: *mut u8) -> bool {
    // Returns true when a VEH claimed `ContinueExecution` (caller restores).
    let chain: Vec<(usize, u32, usize)> = veh()
        .lock()
        .unwrap()
        .iter()
        .map(|(h, (first, f))| (*h, *first, *f))
        .collect();
    if chain.is_empty() {
        return false;
    }
    // `first != 0` handlers run first, in registration order otherwise.
    let mut ordered = chain;
    ordered.sort_by_key(|&(_, first, _)| if first != 0 { 0 } else { 1 });
    for (_, first, handler) in ordered {
        if handler == 0 {
            continue;
        }
        let mut pointers = ExceptionPointers { record, context };
        let f: VehHandlerFn = unsafe { std::mem::transmute(handler) };
        let r = unsafe { f(&mut pointers) };
        // VEH: EXCEPTION_CONTINUE_EXECUTION (-1) resumes, anything else
        // (notably EXCEPTION_CONTINUE_SEARCH == 0) continues the search.
        // (SEH dispositions use 0/1/4 differently; do not conflate them.)
        if r == -1 {
            return true;
        }
    }
    false
}

/// Inner dispatch for `RaiseException`: builds the record + initial context
/// from the naked trampoline's `caller_rsp`/`caller_rip` and drives the
/// two-phase SEH walk. Returns to the trampoline (which returns to
/// `_Unwind_RaiseException`) only when no handler claimed the exception.
unsafe extern "win64" fn raise_exception_inner(
    code: u32,
    flags: u32,
    nargs: u32,
    args: *const usize,
    cap_ptr: *const InitialCapture,
) {
    let cap = if cap_ptr.is_null() {
        crate::xlog_warn!("RaiseException: null capture");
        return;
    } else {
        unsafe { &*cap_ptr }
    };
    crate::xlog_warn!(
        "RaiseException(code={code:#x}, flags={flags:#x}, args={nargs}, rip={:#x})",
        cap.rip as usize
    );
    let nargs = (nargs as usize).min(15);
    let mut record = Box::new(ExceptionRecord {
        code,
        flags: flags & !(EXCEPTION_UNWIND | EXCEPTION_EXIT_UNWIND | EXCEPTION_TARGET_UNWIND | EXCEPTION_COLLIDED_UNWIND),
        record: 0,
        addr: cap.rip as usize,
        num_params: nargs as u32,
        _pad: 0,
        info: [0; 15],
    });
    for i in 0..nargs {
        let v = if args.is_null() {
            0
        } else {
            unsafe { *args.add(i) }
        };
        record.info[i] = v;
    }
    let mut ctx_box = vec![0u8; CONTEXT_SIZE].into_boxed_slice();
    let ctx = ctx_box.as_mut_ptr();
    build_initial_context(ctx, cap);

    // VEH first (parity with Windows dispatch order).
    if call_veh_chain(record.as_mut() as *mut _, ctx) {
        unsafe {
            RtlRestoreContext(ctx, std::ptr::null_mut::<u8>() as usize);
        }
        return;
    }

    // Phase 1: search. Walk with a scratch copy so the original stays intact
    // for the unwind pass.
    let mut walk_box = vec![0u8; CONTEXT_SIZE].into_boxed_slice();
    let walk = walk_box.as_mut_ptr();
    unsafe {
        std::ptr::copy_nonoverlapping(ctx, walk, CONTEXT_SIZE);
    }
    struct Found {
        establisher: usize,
        control_pc: u64,
    }
    let mut found: Option<Found> = None;
    for _ in 0..MAX_UNWIND_FRAMES {
        let control_pc = unsafe { ctx_u64(walk as *const u8, CTX_RIP).unwrap_or(0) } as usize;
        let walk_rsp = unsafe { ctx_u64(walk as *const u8, CTX_RSP).unwrap_or(0) };
        if control_pc == 0 {
            crate::xlog_warn!("RaiseException walk: null PC");
            break;
        }
        if crate::modules::find_mapped_by_addr(control_pc).is_none() {
            crate::xlog_warn!("RaiseException walk: pc={control_pc:#x} rsp={walk_rsp:#x} unmapped");
            break;
        }
        let mut establisher: usize = 0;
        let mut handler_data: usize = 0;
        let mut image_base_out: usize = 0;
        let entry = unsafe {
            RtlLookupFunctionEntry(control_pc, &mut image_base_out, std::ptr::null_mut())
        };
        let image_base = if image_base_out != 0 {
            image_base_out
        } else {
            match crate::modules::find_mapped_by_addr(control_pc) {
                Some(mm) => mm.base,
                None => break,
            }
        };
        let handler = if entry == 0 {
            // Leaf pop (mirrors `unwind_one`).
            let rsp = match unsafe { ctx_u64(walk as *const u8, CTX_RSP) } {
                Some(v) => v,
                None => break,
            };
            let rip = match safe_read_u64(rsp) {
                Some(v) => v,
                None => break,
            };
            unsafe {
                ctx_set_u64(walk, CTX_RSP, rsp.wrapping_add(8));
                ctx_set_u64(walk, CTX_RIP, rip);
            }
            0
        } else {
            unsafe {
                RtlVirtualUnwind(
                    0,
                    image_base,
                    control_pc,
                    entry,
                    walk,
                    &mut handler_data,
                    &mut establisher,
                    std::ptr::null_mut(),
                )
            }
        };
        {
            let next_rip = unsafe { ctx_u64(walk as *const u8, CTX_RIP).unwrap_or(0) };
            let next_rsp = unsafe { ctx_u64(walk as *const u8, CTX_RSP).unwrap_or(0) };
            crate::xlog_debug!(
                "RaiseException frame: pc={control_pc:#x} entry={entry:#x} handler={handler:#x} est={establisher:#x} next_rip={next_rip:#x} next_rsp={next_rsp:#x} code={:#x} flags={:#x} nparams={}",
                record.code,
                record.flags,
                record.num_params,
            );
        }
        if handler != 0 {
            {
                let info0 = record.info[0];
                let hd_bytes = if handler_data != 0 {
                    unsafe {
                        let ptr = handler_data as *const u8;
                        let slice = std::slice::from_raw_parts(ptr, 16);
                        format!("{:02x?}", slice)
                    }
                } else {
                    "none".to_string()
                };
                crate::xlog_warn!(
                    "RaiseException search: pc={control_pc:#x} handler={handler:#x} establisher={establisher:#x} info0={info0:#x} hd={handler_data:#x} hd16={hd_bytes}"
                );
            }
            let mut dispatcher = DispatcherContext {
                control_pc: control_pc as u64,
                image_base: image_base as u64,
                function_entry: entry as u64,
                establisher: establisher as u64,
                target_ip: 0,
                context: walk as u64,
                language_handler: handler as u64,
                handler_data: handler_data as u64,
                history_table: 0,
                scope_index: 0,
                _pad: 0,
            };
            let disp = call_language_handler(
                handler,
                record.as_mut() as *mut _,
                establisher,
                walk,
                &mut dispatcher as *mut _,
            );
            crate::xlog_warn!("RaiseException search: pc={control_pc:#x} disp={disp}");
            if disp == DISP_CONTINUE_EXECUTION {
                unsafe {
                    std::ptr::copy_nonoverlapping(walk, ctx, CONTEXT_SIZE);
                    RtlRestoreContext(ctx, std::ptr::null_mut::<u8>() as usize);
                }
                return;
            } else if disp == DISP_EXECUTE_HANDLER {
                found = Some(Found {
                    establisher,
                    control_pc: control_pc as u64,
                });
                break;
            } else if disp == DISP_COLLIDED_UNWIND {
                crate::xlog_warn!("RaiseException: collided unwind at {control_pc:#x}");
            }
            // `ContinueSearch` (1) and anything else: keep walking.
        }
        // No handler on this frame (or ContinueSearch): `RtlVirtualUnwind`
        // already advanced `walk` (or we popped the leaf above); continue.
        // Detect a stuck walk: if RIP did not advance, stop.
        let next_pc = unsafe { ctx_u64(walk as *const u8, CTX_RIP).unwrap_or(0) } as usize;
        if next_pc == control_pc {
            crate::xlog_warn!("RaiseException search: stuck at pc={control_pc:#x} entry={entry:#x}");
            break;
        }
    }
    let Some(target) = found else {
        crate::xlog_warn!("RaiseException({code:#x}): no handler found, returning");
        return;
    };

    // Phase 2: unwind pass from the original context up to the target,
    // calling each handler with `UNWIND` set. The target's own handler
    // (`_GCC_specific_handler`) performs the final `RtlUnwindEx` jump to the
    // catch block and never returns; if it does return we fall back to a
    // plain return (the `_Unwind_RaiseException` wrapper reports error 5).
    unsafe {
        std::ptr::copy_nonoverlapping(ctx, walk, CONTEXT_SIZE);
    }
    // Mark the record as unwinding for this pass.
    record.flags |= EXCEPTION_UNWIND;
    for _ in 0..MAX_UNWIND_FRAMES {
        let control_pc = unsafe { ctx_u64(walk as *const u8, CTX_RIP).unwrap_or(0) } as usize;
        let walk_rsp = unsafe { ctx_u64(walk as *const u8, CTX_RSP).unwrap_or(0) };
        if control_pc == 0 {
            crate::xlog_warn!("RaiseException walk: null PC");
            break;
        }
        if crate::modules::find_mapped_by_addr(control_pc).is_none() {
            crate::xlog_warn!("RaiseException walk: pc={control_pc:#x} rsp={walk_rsp:#x} unmapped");
            break;
        }
        let mut establisher: usize = 0;
        let mut handler_data: usize = 0;
        let mut image_base_out: usize = 0;
        let entry = unsafe {
            RtlLookupFunctionEntry(control_pc, &mut image_base_out, std::ptr::null_mut())
        };
        let image_base = if image_base_out != 0 {
            image_base_out
        } else {
            match crate::modules::find_mapped_by_addr(control_pc) {
                Some(mm) => mm.base,
                None => break,
            }
        };
        let is_target = establisher == target.establisher && control_pc as u64 == target.control_pc;
        let handler = if entry == 0 {
            let rsp = match unsafe { ctx_u64(walk as *const u8, CTX_RSP) } {
                Some(v) => v,
                None => break,
            };
            let rip = match safe_read_u64(rsp) {
                Some(v) => v,
                None => break,
            };
            establisher = rsp as usize;
            unsafe {
                ctx_set_u64(walk, CTX_RSP, rsp.wrapping_add(8));
                ctx_set_u64(walk, CTX_RIP, rip);
            }
            0
        } else {
            unsafe {
                RtlVirtualUnwind(
                    0,
                    image_base,
                    control_pc,
                    entry,
                    walk,
                    &mut handler_data,
                    &mut establisher,
                    std::ptr::null_mut(),
                )
            }
        };
        if handler != 0 {
            let mut dispatcher = DispatcherContext {
                control_pc: control_pc as u64,
                image_base: image_base as u64,
                function_entry: entry as u64,
                establisher: establisher as u64,
                target_ip: 0,
                context: walk as u64,
                language_handler: handler as u64,
                handler_data: handler_data as u64,
                history_table: 0,
                scope_index: 0,
                _pad: 0,
            };
            let _ = call_language_handler(
                handler,
                record.as_mut() as *mut _,
                establisher,
                walk,
                &mut dispatcher as *mut _,
            );
            // During unwinding the handler drives the transfer itself via
            // `RtlUnwindEx`; whatever it returns, keep walking unless it
            // jumped (which never returns).
        }
        if is_target {
            break;
        }
        let next_pc = unsafe { ctx_u64(walk as *const u8, CTX_RIP).unwrap_or(0) } as usize;
        if next_pc == control_pc {
            break;
        }
        // Stop once we have unwound past the target establisher.
        if establisher == target.establisher {
            break;
        }
    }
    crate::xlog_warn!("RaiseException({code:#x}): unwind pass returned without transfer");
}

/// `VOID RaiseException(DWORD, DWORD, DWORD, const ULONG_PTR*)` — naked
/// trampoline: captures the guest caller's `RSP`/`RIP` plus the nonvolatile
/// registers before any Rust prologue runs, then tails into
/// `raise_exception_inner`. Returns only when no handler claimed the exception
/// (the `_Unwind_RaiseException` wrapper then reports error 5, as before).
/// Layout on entry: `RCX/RDX/R8/R9` hold the four args; only `RAX`/`R10`/`R11`
/// (volatile) are used as scratch so the nonvolatiles survive into the capture.
#[unsafe(naked)]
pub unsafe extern "win64" fn RaiseException(code: u32, flags: u32, nargs: u32, args: *const usize) {
    core::arch::naked_asm!(
        "sub rsp, 0xB8",
        "lea rax, [rsp+40]",
        "mov QWORD PTR [rsp+32], rax",
        "lea r10, [rsp+0xC0]",
        "mov r11, QWORD PTR [rsp+0xB8]",
        "mov QWORD PTR [rax+0], r10",
        "mov QWORD PTR [rax+8], r11",
        "mov QWORD PTR [rax+16], rbx",
        "mov QWORD PTR [rax+24], rbp",
        "mov QWORD PTR [rax+32], rsi",
        "mov QWORD PTR [rax+40], rdi",
        "mov QWORD PTR [rax+48], r12",
        "mov QWORD PTR [rax+56], r13",
        "mov QWORD PTR [rax+64], r14",
        "mov QWORD PTR [rax+72], r15",
        "call {inner}",
        "add rsp, 0xB8",
        "ret",
        inner = sym raise_exception_inner,
    )
}

/// `VOID RtlUnwindEx(ULONG64 TargetFrame, ULONG64 TargetIp,
/// PEXCEPTION_RECORD ExceptionRecord, PVOID ReturnValue, PCONTEXT Context,
/// PUNWIND_HISTORY_TABLE History)` — unwinds `Context` up to `TargetFrame`,
/// calling handlers with `UNWIND` (plus `TARGET_UNWIND` on the target), then
/// transfers to `TargetIp` via `RtlRestoreContext`. Never returns on success;
/// returns with a warn log when the target is unreachable.
pub unsafe extern "win64" fn RtlUnwindEx(
    target_frame: usize,
    target_ip: usize,
    exception_record: usize,
    _return_value: usize,
    context: *mut u8,
    _history: usize,
) {
    {
        let ctx_rip = if context.is_null() { 0 } else { unsafe { ctx_u64(context as *const u8, CTX_RIP).unwrap_or(0) } };
        let ctx_rsp = if context.is_null() { 0 } else { unsafe { ctx_u64(context as *const u8, CTX_RSP).unwrap_or(0) } };
        crate::xlog_warn!("RtlUnwindEx: target={target_frame:#x}->{target_ip:#x} rec={exception_record:#x} ctx_rip={ctx_rip:#x} ctx_rsp={ctx_rsp:#x}");
    }
    if context.is_null() || target_frame == 0 || target_ip == 0 {
        crate::xlog_warn!("RtlUnwindEx: bad target ({target_frame:#x} -> {target_ip:#x})");
        set_error(ERROR_INVALID_PARAMETER);
        return;
    }
    // The record may live in guest memory; mark unwind flags on a local copy
    // when it is writable, else proceed with the caller's flags.
    let record_ptr = exception_record as *mut ExceptionRecord;
    if !record_ptr.is_null() {
        unsafe {
            (*record_ptr).flags |= EXCEPTION_UNWIND;
        }
    }
    // Fast path for GCC `__rust_try` style transfers where the target frame
    // is at or below the current RSP (the search already unwound past it).
    // Windows restores directly to `TargetFrame`/`TargetIp` with
    // `RAX = ReturnValue` (and `RDX = Info[3]` for the catch payload) instead
    // of walking up. Without this the walk looks for an establisher above
    // the current RSP, never finds it, returns, and the guest calls `abort`.
    //
    // Narrow `__rust_try` normal-to-catch redirect (testtitle Rust
    // `catch_unwind`): the GCC personality currently resolves the landing
    // pad to the normal return (`EB 00; xor eax,eax; ...; ret`, returning 0)
    // instead of the catch block (`48 8B 4C 24 30 ...; mov $1,%al; ret`,
    // returning 1). Jumping to the normal path makes `__rust_try` return 0
    // so `catch_unwind` hits `ud2`. When the target looks like the normal
    // return, scan forward for the catch prolog within the same function
    // and transfer there instead. Pending generic LSDA landing-pad parsing.
    {
        let mut target_ip = target_ip;
        {
            let first = safe_read_u64(target_ip as u64).map(|v| (v & 0xFF) as u8);
            if first == Some(0xEB) {
                // Possible `__rust_try` normal return (`EB xx` jmp to `xor`).
                for off in 0..0x40usize {
                    let va = target_ip.wrapping_add(off);
                    // Catch prolog: `movq 0x30(%rsp),%rcx` = 48 8B 4C 24 30.
                    let mut ok = false;
                    if let Some(lo) = safe_read_u64(va as u64) {
                        let b0 = (lo & 0xFF) as u8;
                        let b1 = ((lo >> 8) & 0xFF) as u8;
                        let b2 = ((lo >> 16) & 0xFF) as u8;
                        let b3 = ((lo >> 24) & 0xFF) as u8;
                        let b4 = ((lo >> 32) & 0xFF) as u8;
                        ok = b0 == 0x48 && b1 == 0x8B && b2 == 0x4C && b3 == 0x24 && b4 == 0x30;
                    }
                    if ok {
                        crate::xlog_warn!(
                            "RtlUnwindEx: __rust_try normal {target_ip:#x} -> catch {va:#x}"
                        );
                        target_ip = va;
                        break;
                    }
                }
            }
        }
        let cur_rsp = unsafe { ctx_u64(context as *const u8, CTX_RSP).unwrap_or(0) } as usize;
        if cur_rsp != 0 && target_frame <= cur_rsp {
            unsafe {
                ctx_set_u64(context, CTX_RSP, target_frame as u64);
                ctx_set_u64(context, CTX_RIP, target_ip as u64);
                ctx_set_u64(context, CTX_RAX, _return_value as u64);
                if !record_ptr.is_null() {
                    let n = (*record_ptr).num_params as usize;
                    if n >= 4 {
                        let rdx = (*record_ptr).info[3];
                        ctx_set_u64(context, CTX_RDX, rdx as u64);
                    }
                    // The target will observe the unwind pass.
                    (*record_ptr).flags |= EXCEPTION_TARGET_UNWIND;
                }
                RtlRestoreContext(context, exception_record);
            }
            return;
        }
    }
    for _ in 0..MAX_UNWIND_FRAMES {
        let control_pc = match unsafe { ctx_u64(context as *const u8, CTX_RIP) } {
            Some(v) => v as usize,
            None => {
                crate::xlog_warn!("RtlUnwindEx: bad context");
                return;
            }
        };
        if control_pc == 0 || crate::modules::find_mapped_by_addr(control_pc).is_none() {
            crate::xlog_warn!("RtlUnwindEx: PC {control_pc:#x} outside mapped images");
            return;
        }
        let mut establisher: usize = 0;
        let mut handler_data: usize = 0;
        let mut image_base_out: usize = 0;
        let entry = unsafe {
            RtlLookupFunctionEntry(control_pc, &mut image_base_out, std::ptr::null_mut())
        };
        let image_base = if image_base_out != 0 {
            image_base_out
        } else {
            match crate::modules::find_mapped_by_addr(control_pc) {
                Some(mm) => mm.base,
                None => return,
            }
        };
        let is_target_frame = {
            // The establisher is only known after unwinding; peek by
            // unwinding into a scratch copy first when this might be it.
            // Simpler: unwind, then compare the reported establisher.
            false
        };
        let _ = is_target_frame;
        let handler = if entry == 0 {
            let rsp = match unsafe { ctx_u64(context as *const u8, CTX_RSP) } {
                Some(v) => v,
                None => return,
            };
            let rip = match safe_read_u64(rsp) {
                Some(v) => v,
                None => {
                    crate::xlog_warn!("RtlUnwindEx: leaf stack read failed");
                    return;
                }
            };
            establisher = rsp as usize;
            unsafe {
                ctx_set_u64(context, CTX_RSP, rsp.wrapping_add(8));
                ctx_set_u64(context, CTX_RIP, rip);
            }
            0
        } else {
            unsafe {
                RtlVirtualUnwind(
                    0,
                    image_base,
                    control_pc,
                    entry,
                    context,
                    &mut handler_data,
                    &mut establisher,
                    std::ptr::null_mut(),
                )
            }
        };
        let at_target = establisher == target_frame;
        if handler != 0 {
            let mut dispatcher = DispatcherContext {
                control_pc: control_pc as u64,
                image_base: image_base as u64,
                function_entry: entry as u64,
                establisher: establisher as u64,
                target_ip: target_ip as u64,
                context: context as u64,
                language_handler: handler as u64,
                handler_data: handler_data as u64,
                history_table: 0,
                scope_index: 0,
                _pad: 0,
            };
            if !record_ptr.is_null() && at_target {
                unsafe {
                    (*record_ptr).flags |= EXCEPTION_TARGET_UNWIND;
                }
            }
            let _ = call_language_handler(handler, record_ptr, establisher, context, &mut dispatcher as *mut _);
            if !record_ptr.is_null() && at_target {
                unsafe {
                    (*record_ptr).flags &= !EXCEPTION_TARGET_UNWIND;
                }
            }
        }
        if at_target {
            unsafe {
                ctx_set_u64(context, CTX_RIP, target_ip as u64);
                RtlRestoreContext(context, exception_record);
            }
            return;
        }
    }
    crate::xlog_warn!("RtlUnwindEx: target {target_frame:#x} unreachable");
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
        ("LsaOpenPolicy", LsaOpenPolicy as usize),
        ("CoInitializeEx", CoInitializeEx as usize),
        ("CoUninitialize", CoUninitialize as usize),
        ("CoCreateInstance", CoCreateInstance as usize),
        ("CoCreateGuid", CoCreateGuid as usize),
        ("StringFromGUID2", StringFromGUID2 as usize),
        ("VirtualAlloc", VirtualAlloc as usize),
        ("VirtualAllocEx", VirtualAllocEx as usize),
        ("VirtualFree", VirtualFree as usize),
        ("VirtualFreeEx", VirtualFreeEx as usize),
        ("VirtualProtect", VirtualProtect as usize),
        ("VirtualProtectEx", VirtualProtectEx as usize),
        ("XMemAlloc", XMemAlloc as usize),
        ("XMemFree", XMemFree as usize),
        ("XMemAllocDefault", XMemAllocDefault as usize),
        ("XMemFreeDefault", XMemFreeDefault as usize),
        ("XMemSetAllocationHooks", XMemSetAllocationHooks as usize),
        ("XMemVirtualAlloc", XMemVirtualAlloc as usize),
        ("XMemVirtualQuery", XMemVirtualQuery as usize),
        ("XMemAllocatePhysicalPages", XMemAllocatePhysicalPages as usize),
        ("XMemMapPhysicalPages", XMemMapPhysicalPages as usize),
        ("XMemMapPhysicalPagesScatter", XMemMapPhysicalPagesScatter as usize),
        ("XMemFreePhysicalPages", XMemFreePhysicalPages as usize),
        ("AllocateTitlePhysicalPages", AllocateTitlePhysicalPages as usize),
        ("FreeTitlePhysicalPages", FreeTitlePhysicalPages as usize),
        ("MapTitlePhysicalPages", MapTitlePhysicalPages as usize),
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
        ("GetThreadGroupAffinity", GetThreadGroupAffinity as usize),
        ("SetThreadGroupAffinity", SetThreadGroupAffinity as usize),
        ("SetThreadIdealProcessor", SetThreadIdealProcessor as usize),
        ("SetThreadIdealProcessorEx", SetThreadIdealProcessorEx as usize),
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
        ("OpenThread", OpenThread as usize),
        ("ExitProcess", ExitProcess as usize),
        ("TerminateProcess", TerminateProcess as usize),
        ("GetThreadPriority", GetThreadPriority as usize),
        ("SetThreadPriority", SetThreadPriority as usize),
        ("GetThreadPriorityBoost", GetThreadPriorityBoost as usize),
        ("SetThreadPriorityBoost", SetThreadPriorityBoost as usize),
        ("SetProcessPriorityBoost", SetProcessPriorityBoost as usize),
        ("GetLastError", GetLastError as usize),
        ("SetLastError", SetLastError as usize),
        ("Sleep", Sleep as usize),
        ("SleepEx", SleepEx as usize),
        ("GetTickCount", GetTickCount as usize),
        ("GetTickCount64", GetTickCount64 as usize),
        ("GetSystemInfo", GetSystemInfo as usize),
        ("GetNativeSystemInfo", GetNativeSystemInfo as usize),
        ("GetLogicalProcessorInformation", GetLogicalProcessorInformation as usize),
        ("QueryPerformanceCounter", QueryPerformanceCounter as usize),
        ("QueryPerformanceFrequency", QueryPerformanceFrequency as usize),
        ("GetSystemTimeAsFileTime", GetSystemTimeAsFileTime as usize),
        ("GetSystemTimePreciseAsFileTime", GetSystemTimePreciseAsFileTime as usize),
        ("CreateEventW", CreateEventW as usize),
        ("CreateEventA", CreateEventA as usize),
        ("CreateSemaphoreExW", CreateSemaphoreExW as usize),
        ("CreateSemaphoreW", CreateSemaphoreW as usize),
        ("CreateSemaphoreExA", CreateSemaphoreExA as usize),
        ("CreateSemaphoreA", CreateSemaphoreA as usize),
        ("ReleaseSemaphore", ReleaseSemaphore as usize),
        ("CreateMutexA", CreateMutexA as usize),
        ("ReleaseMutex", ReleaseMutex as usize),
        ("InitializeSListHead", InitializeSListHead as usize),
        ("SetEvent", SetEvent as usize),
        ("ResetEvent", ResetEvent as usize),
        ("CloseHandle", CloseHandle as usize),
        ("WaitForSingleObject", WaitForSingleObject as usize),
        ("WaitForMultipleObjects", WaitForMultipleObjects as usize),
        ("InitializeCriticalSection", InitializeCriticalSection as usize),
        ("InitializeCriticalSectionEx", InitializeCriticalSectionEx as usize),
        ("EncodePointer", EncodePointer as usize),
        ("DecodePointer", DecodePointer as usize),
        (
            "InitializeCriticalSectionAndSpinCount",
            InitializeCriticalSectionAndSpinCount as usize,
        ),
        ("EnterCriticalSection", EnterCriticalSection as usize),
        ("TryEnterCriticalSection", TryEnterCriticalSection as usize),
        ("LeaveCriticalSection", LeaveCriticalSection as usize),
        ("DeleteCriticalSection", DeleteCriticalSection as usize),
        ("InitializeConditionVariable", InitializeConditionVariable as usize),
        ("SleepConditionVariableCS", SleepConditionVariableCS as usize),
        ("WakeConditionVariable", WakeConditionVariable as usize),
        ("WakeAllConditionVariable", WakeAllConditionVariable as usize),
        ("InitializeSRWLock", InitializeSRWLock as usize),
        ("AcquireSRWLockExclusive", AcquireSRWLockExclusive as usize),
        ("ReleaseSRWLockExclusive", ReleaseSRWLockExclusive as usize),
        ("TryAcquireSRWLockExclusive", TryAcquireSRWLockExclusive as usize),
        // Step-11b startup: command line, process heap, module file names.
        ("GetCommandLineW", GetCommandLineW as usize),
        ("GetCommandLineA", GetCommandLineA as usize),
        ("_get_narrow_winmain_command_line", _get_narrow_winmain_command_line as usize),
        ("GetStartupInfoW", GetStartupInfoW as usize),
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
        ("GetModuleFileNameExW", GetModuleFileNameExW as usize),
        ("GetModuleFileNameExA", GetModuleFileNameExA as usize),
        ("K32GetModuleFileNameExW", K32GetModuleFileNameExW as usize),
        ("K32GetModuleFileNameExA", K32GetModuleFileNameExA as usize),
        ("GetModuleBaseNameW", GetModuleBaseNameW as usize),
        ("GetModuleBaseNameA", GetModuleBaseNameA as usize),
        ("K32GetModuleBaseNameW", K32GetModuleBaseNameW as usize),
        ("K32GetModuleBaseNameA", K32GetModuleBaseNameA as usize),
        ("K32GetModuleInformation", K32GetModuleInformation as usize),
        ("GetModuleInformation", GetModuleInformation as usize),
        // Step-11c startup neighbors: FLS, std handles, module handles, environment, fibers.
        ("FlsAlloc", FlsAlloc as usize),
        ("FlsFree", FlsFree as usize),
        ("FlsGetValue", FlsGetValue as usize),
        ("FlsSetValue", FlsSetValue as usize),
        ("IsThreadAFiber", IsThreadAFiber as usize),
        ("ConvertThreadToFiber", ConvertThreadToFiber as usize),
        ("ConvertFiberToThread", ConvertFiberToThread as usize),
        ("CreateFiber", CreateFiber as usize),
        ("DeleteFiber", DeleteFiber as usize),
        ("SwitchToFiber", SwitchToFiber as usize),
        ("GetStdHandle", GetStdHandle as usize),
        ("SetStdHandle", SetStdHandle as usize),
        ("GetModuleHandleW", GetModuleHandleW as usize),
        ("GetModuleHandleA", GetModuleHandleA as usize),
        ("GetModuleHandleExW", GetModuleHandleExW as usize),
        ("GetModuleHandleExA", GetModuleHandleExA as usize),
        ("LoadLibraryW", LoadLibraryW as usize),
        ("LoadLibraryA", LoadLibraryA as usize),
        ("LoadLibraryExW", LoadLibraryExW as usize),
        ("LoadLibraryExA", LoadLibraryExA as usize),
        ("LoadPackagedLibrary", LoadPackagedLibrary as usize),
        ("FreeLibrary", FreeLibrary as usize),
        ("FreeLibraryWhenCallbackReturns", FreeLibraryWhenCallbackReturns as usize),
        ("DisableThreadLibraryCalls", DisableThreadLibraryCalls as usize),
        ("GetEnvironmentVariableW", GetEnvironmentVariableW as usize),
        ("GetEnvironmentVariableA", GetEnvironmentVariableA as usize),
        // Step-11d exception/console shims for the Rust panic-report path.
        ("AddVectoredExceptionHandler", AddVectoredExceptionHandler as usize),
        ("RemoveVectoredExceptionHandler", RemoveVectoredExceptionHandler as usize),
        ("RaiseException", RaiseException as usize),
        ("GetProcAddress", GetProcAddress as usize),
        ("GetConsoleMode", GetConsoleMode as usize),
        ("SetConsoleMode", SetConsoleMode as usize),
        ("GetConsoleOutputCP", GetConsoleOutputCP as usize),
        ("SetThreadStackGuarantee", SetThreadStackGuarantee as usize),
        ("NtWriteFile", NtWriteFile as usize),
        // Step-11e timer/synch shims for Rust thread sleep + parking.
        ("CreateWaitableTimerExW", CreateWaitableTimerExW as usize),
        ("CreateWaitableTimerW", CreateWaitableTimerW as usize),
        ("SetWaitableTimer", SetWaitableTimer as usize),
        ("CancelWaitableTimer", CancelWaitableTimer as usize),
        ("WaitOnAddress", WaitOnAddress as usize),
        ("WakeByAddressSingle", WakeByAddressSingle as usize),
        ("WakeByAddressAll", WakeByAddressAll as usize),
        // Step-11f SEH search: `.pdata` reader plus Rtl* shims.
        ("RtlPcToFileHeader", RtlPcToFileHeader as usize),
        ("RtlLookupFunctionEntry", RtlLookupFunctionEntry as usize),
        ("RtlVirtualUnwind", RtlVirtualUnwind as usize),
        ("RtlAddFunctionTable", RtlAddFunctionTable as usize),
        ("RtlCaptureContext", RtlCaptureContext as usize),
        ("RtlUnwindEx", RtlUnwindEx as usize),
        ("RtlRestoreContext", RtlRestoreContext as usize),
        // Step-10c file APIs through the linked-in Linux VFS (kernel-family fallbacks).
        ("CreateFileW", CreateFileW as usize),
        ("CreateFileA", CreateFileA as usize),
        ("CreateFile2", CreateFile2 as usize),
        ("ReadFile", ReadFile as usize),
        ("WriteFile", WriteFile as usize),
        ("FlushFileBuffers", FlushFileBuffers as usize),
        ("GetFileSizeEx", GetFileSizeEx as usize),
        ("GetFileTime", GetFileTime as usize),
        ("SetFilePointerEx", SetFilePointerEx as usize),
        ("SetEndOfFile", SetEndOfFile as usize),
        ("GetFileInformationByHandle", GetFileInformationByHandle as usize),
        ("GetFileInformationByHandleEx", GetFileInformationByHandleEx as usize),
        ("NtReadFile", NtReadFile as usize),
        ("RtlNtStatusToDosError", RtlNtStatusToDosError as usize),
        ("GetCurrentDirectoryW", GetCurrentDirectoryW as usize),
        ("GetCurrentDirectoryA", GetCurrentDirectoryA as usize),
        ("GetFullPathNameW", GetFullPathNameW as usize),
        ("GetFullPathNameA", GetFullPathNameA as usize),
        ("DeleteFileW", DeleteFileW as usize),
        ("DeleteFileA", DeleteFileA as usize),
        ("SetFileInformationByHandle", SetFileInformationByHandle as usize),
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
        ("FindFirstFileExA", FindFirstFileExA as usize),
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
        ("FormatMessageW", FormatMessageW as usize),
        ("FormatMessageA", FormatMessageA as usize),
        ("MultiByteToWideChar", MultiByteToWideChar as usize),
        ("WideCharToMultiByte", WideCharToMultiByte as usize),
        ("GetUserDefaultGeoName", GetUserDefaultGeoName as usize),
        ("GetUserDefaultLocaleName", GetUserDefaultLocaleName as usize),
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
        assert_eq!(fit_affinity_mask(0x7F, 0), 0x7F);
        assert_eq!(fit_affinity_mask(0x7F, 0xF), 0xF);
        assert_eq!(fit_affinity_mask(CONSOLE_TITLE_AFFINITY_MASK, 0xF), 0xF);
        assert_eq!(fit_affinity_mask(CONSOLE_TITLE_SMT_AFFINITY_MASK, 0xF), 0xF);
        assert_eq!(fit_affinity_mask(1 << 6, 0xF), 1 << 2);
        assert_eq!(fit_affinity_mask(0x3000, 0xF), 0x3);
        assert_eq!(fit_affinity_mask(1 << 20, 0b1010_1010), 0b10);
        // Partial overlap preserves in-process bits and maps remaining console cores.
        assert_eq!(fit_affinity_mask(0b0111_1000, 0xF), 0xF);
        assert_eq!(fit_affinity_mask(0b0110_0010, 0xF), 0b0110);
    }

    #[test]
    fn affinity_null_and_zero_args_fail_cleanly() {
        unsafe {
            // Null handle fails even with a valid mask.
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadAffinityMask(0, 0xF), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            // Zero mask fails even on the current thread.
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadAffinityMask(current_thread_handle(), 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            let mut a: GroupAffinity = std::mem::zeroed();
            a.mask = 0xF;
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadGroupAffinity(0, &a, std::ptr::null_mut()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            set_error(ERROR_SUCCESS);
            assert_eq!(
                SetThreadGroupAffinity(current_thread_handle(), std::ptr::null(), std::ptr::null_mut()),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let zero_mask: GroupAffinity = std::mem::zeroed();
            set_error(ERROR_SUCCESS);
            assert_eq!(
                SetThreadGroupAffinity(current_thread_handle(), &zero_mask, std::ptr::null_mut()),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            // Threadpool mask: null pool or zero mask fails; a console mask succeeds.
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadpoolAffinityMask(std::ptr::null_mut(), 0x7F), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let dummy_pool = 0x1000usize as *mut c_void;
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadpoolAffinityMask(dummy_pool, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(SetThreadpoolAffinityMask(dummy_pool, 0x7F), 1);
        }
    }

    #[test]
    fn ideal_processor_bounds_and_fitting() {
        // Pure mapping: u32::MAX passes through, in-mask numbers stay, the rest wrap.
        assert_eq!(fit_ideal_processor(u32::MAX, 0xF), u32::MAX);
        assert_eq!(fit_ideal_processor(0, 0xF), 0);
        assert_eq!(fit_ideal_processor(3, 0xF), 3);
        assert_eq!(fit_ideal_processor(4, 0xF), 0);
        assert_eq!(fit_ideal_processor(5, 0xF), 1);
        assert_eq!(fit_ideal_processor(6, 0xF), 2);
        assert_eq!(fit_ideal_processor(14, 0xF), 2);
        assert_eq!(fit_ideal_processor(6, 0), 6);
        unsafe {
            // Null handle fails with the Windows sentinel; null ideal fails cleanly.
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadIdealProcessor(0, 0), u32::MAX);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            let ideal = ProcessorNumber { group: 0, number: 6, reserved: 0 };
            set_error(ERROR_SUCCESS);
            assert_eq!(
                SetThreadIdealProcessorEx(0, &ideal, std::ptr::null_mut()),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            set_error(ERROR_SUCCESS);
            assert_eq!(
                SetThreadIdealProcessorEx(
                    current_thread_handle(),
                    std::ptr::null(),
                    std::ptr::null_mut()
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Nonzero groups have no Linux equivalent and fail cleanly.
            let grouped = ProcessorNumber { group: 1, number: 0, reserved: 0 };
            set_error(ERROR_SUCCESS);
            assert_eq!(
                SetThreadIdealProcessorEx(
                    current_thread_handle(),
                    &grouped,
                    std::ptr::null_mut()
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // A console ideal processor is fitted, remembered, and reported back.
            let mut prev: ProcessorNumber = std::mem::zeroed();
            assert_eq!(
                SetThreadIdealProcessorEx(current_thread_handle(), &ideal, &mut prev),
                1
            );
            assert_eq!(prev.group, 0);
            assert_ne!(SetThreadIdealProcessor(current_thread_handle(), 5), u32::MAX);
            // Unknown handles fail cleanly.
            set_error(ERROR_SUCCESS);
            assert_eq!(SetThreadIdealProcessor(0xDEAD_BEEF, 3), u32::MAX);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            set_error(ERROR_SUCCESS);
            assert_eq!(
                SetThreadIdealProcessorEx(0xDEAD_BEEF, &ideal, std::ptr::null_mut()),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        }
    }

    #[test]
    fn thread_group_affinity_reports_live_masks() {
        unsafe {
            // Null out-param, NULL handle and unknown handles fail cleanly.
            set_error(ERROR_SUCCESS);
            assert_eq!(
                GetThreadGroupAffinity(current_thread_handle(), std::ptr::null_mut()),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let mut ga: GroupAffinity = std::mem::zeroed();
            set_error(ERROR_SUCCESS);
            assert_eq!(GetThreadGroupAffinity(0, &mut ga), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            set_error(ERROR_SUCCESS);
            assert_eq!(GetThreadGroupAffinity(0xDEAD_BEEF, &mut ga), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);

            // The current thread reports group 0 and the live host mask, overwriting
            // whatever the caller left in the out-param.
            ga.mask = 0xAA;
            ga.group = 7;
            ga.reserved = [1, 2, 3];
            assert_eq!(GetThreadGroupAffinity(current_thread_handle(), &mut ga), 1);
            assert_ne!(ga.mask, 0);
            assert_eq!(ga.group, 0);
            assert_eq!(ga.reserved, [0; 3]);
            assert_eq!(ga.mask, process_affinity());
            assert_eq!(GetLastError(), ERROR_SUCCESS);
        }
    }

    #[test]
    fn open_thread_returns_distinct_handles_for_live_threads() {
        unsafe {
            let me = current_tid();
            let a = OpenThread(0x1FFFFF, 0, me);
            assert_ne!(a, 0);
            assert_ne!(a, current_thread_handle());
            let b = OpenThread(0x1FFFFF, 0, me);
            assert_ne!(b, 0);
            assert_ne!(b, a);
            // Both handles address the same live thread state.
            assert_eq!(SetThreadPriority(a, THREAD_PRIORITY_ABOVE_NORMAL), 1);
            assert_eq!(GetThreadPriority(b), THREAD_PRIORITY_ABOVE_NORMAL);
            let mut ga: GroupAffinity = std::mem::zeroed();
            assert_eq!(GetThreadGroupAffinity(b, &mut ga), 1);
            assert_eq!(ga.mask, process_affinity());
            // Closing one opened handle leaves the other usable.
            assert_eq!(CloseHandle(a), 1);
            assert_eq!(GetThreadPriority(b), THREAD_PRIORITY_ABOVE_NORMAL);
            assert_eq!(CloseHandle(b), 1);
            assert_eq!(SetThreadPriority(current_thread_handle(), THREAD_PRIORITY_NORMAL), 1);

            // Bogus and zero thread ids fail with NULL, never a fake handle.
            set_error(ERROR_SUCCESS);
            assert_eq!(OpenThread(0x1FFFFF, 0, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            set_error(ERROR_SUCCESS);
            assert_eq!(OpenThread(0x1FFFFF, 0, 0x7FFF_FFF0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn mutex_recursion_and_release_ownership() {
        unsafe {
            // Unnamed mutex, not initially owned: the first wait acquires, a second wait
            // recurses, and only the owning thread can release.
            let m = CreateMutexA(std::ptr::null(), 0, std::ptr::null());
            assert_ne!(m, 0);
            assert_eq!(WaitForSingleObject(m, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(m, 0), WAIT_OBJECT_0); // recursive acquire
            let m2 = m;
            assert_eq!(
                std::thread::spawn(move || WaitForSingleObject(m2, 0)).join().unwrap(),
                WAIT_TIMEOUT
            );
            assert_eq!(ReleaseMutex(m), 1); // recursion 2 -> 1
            assert_eq!(ReleaseMutex(m), 1); // recursion 1 -> 0
            set_error(ERROR_SUCCESS);
            assert_eq!(ReleaseMutex(m), 0); // unowned: not the owner
            assert_eq!(GetLastError(), ERROR_NOT_OWNER);
            // Another thread can now acquire and release it.
            let m3 = m;
            let waiter = std::thread::spawn(move || {
                let r = WaitForSingleObject(m3, 2000);
                assert_eq!(r, WAIT_OBJECT_0);
                assert_eq!(ReleaseMutex(m3), 1);
                r
            });
            assert_eq!(waiter.join().unwrap(), WAIT_OBJECT_0);
            // Unknown handles fail cleanly.
            set_error(ERROR_SUCCESS);
            assert_eq!(ReleaseMutex(0xDEAD_BEEF), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            assert_eq!(CloseHandle(m), 1);
            assert_eq!(WaitForSingleObject(m, 0), WAIT_FAILED);
            assert_eq!(ReleaseMutex(m), 0);
        }
    }

    #[test]
    fn mutex_wait_blocks_until_the_owner_releases() {
        unsafe {
            // Initially owned mutex: a 5 s wait in another thread is released by main.
            let m = CreateMutexA(std::ptr::null(), 1, std::ptr::null());
            assert_ne!(m, 0);
            let m2 = m;
            let waiter = std::thread::spawn(move || {
                let r = WaitForSingleObject(m2, 5000);
                if r == WAIT_OBJECT_0 {
                    assert_eq!(ReleaseMutex(m2), 1);
                }
                r
            });
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(ReleaseMutex(m), 1);
            assert_eq!(waiter.join().unwrap(), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(m), 1);
        }
    }

    #[test]
    fn mutex_owner_death_reports_abandoned() {
        unsafe {
            let m = CreateMutexA(std::ptr::null(), 0, std::ptr::null());
            assert_ne!(m, 0);
            let m2 = m;
            std::thread::spawn(move || {
                assert_eq!(WaitForSingleObject(m2, 0), WAIT_OBJECT_0);
                // Exit still holding the mutex.
            })
            .join()
            .unwrap();
            // Windows reports abandonment and grants ownership to the waiter.
            assert_eq!(WaitForSingleObject(m, 0), WAIT_ABANDONED);
            assert_eq!(ReleaseMutex(m), 1);
            assert_eq!(CloseHandle(m), 1);
        }
    }

    #[test]
    fn initialize_slist_head_zeroes_the_16_byte_header() {
        let mut buf = [0xFFu8; 24];
        unsafe { InitializeSListHead(buf.as_mut_ptr() as *mut c_void) };
        assert_eq!(&buf[..16], &[0u8; 16]);
        assert_eq!(&buf[16..], &[0xFFu8; 8]); // guard bytes untouched
        unsafe { InitializeSListHead(std::ptr::null_mut()) }; // NULL is a no-op
    }

    #[test]
    fn set_process_priority_boost_accepts_the_current_process() {
        unsafe {
            // Linux has no dynamic priority boost; both requests are accepted hints.
            assert_eq!(SetProcessPriorityBoost(current_process_handle(), 1), 1);
            assert_eq!(SetProcessPriorityBoost(current_process_handle(), 0), 1);
            set_error(ERROR_SUCCESS);
            assert_eq!(SetProcessPriorityBoost(0xDEAD_BEEF, 1), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            set_error(ERROR_SUCCESS);
            assert_eq!(SetProcessPriorityBoost(0, 1), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        }
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
    fn placeholder_split_and_replacement_keep_the_va_reserved() {
        unsafe {
            let grain = GFX_GRAIN;
            // Retail order (`exe+0x1e86440`): one placeholder reservation, carved into
            // ranges with `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`, then each carved range
            // re-allocated at its own VA (`0x20406000` = `MEM_REPLACE_PLACEHOLDER |
            // MEM_RESERVE | large/physical pages`) and committed in place.
            let base = XMemVirtualAlloc(std::ptr::null(), 3 * grain, 0x42000, 7, PAGE_NOACCESS) as usize;
            assert_ne!(base, 0, "placeholder reservation refused");
            assert_eq!(perms_at(base).as_deref(), Some("---p"));

            assert_eq!(
                VirtualFree(base as *mut c_void, grain, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
                1,
                "split a placeholder without releasing its address space"
            );
            assert_eq!(
                VirtualFree((base + grain) as *mut c_void, grain, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
                1
            );
            assert_eq!(perms_at(base).as_deref(), Some("---p"));
            assert_eq!(perms_at(base + 2 * grain).as_deref(), Some("---p"));

            // A replacement matches exactly one carved range, at its original address.
            assert_eq!(VirtualAlloc(base as *const c_void, grain, 0x20406000, 0x1004) as usize, base);
            set_error(ERROR_SUCCESS);
            assert!(VirtualAlloc((base + grain) as *const c_void, 2 * grain, 0x20406000, 0x1004).is_null());
            assert_eq!(GetLastError(), ERROR_INVALID_ADDRESS);

            // Committing the replacement in place makes it writable; the rest stays reserved.
            assert_eq!(VirtualAlloc(base as *const c_void, grain, MEM_COMMIT, PAGE_READWRITE) as usize, base);
            *(base as *mut u32) = 0x1234_5678;
            assert_eq!(*(base as *const u32), 0x1234_5678);
            assert_eq!(perms_at(base).as_deref(), Some("rw-p"));
            assert_eq!(perms_at(base + grain).as_deref(), Some("---p"));
            assert_eq!(perms_at(base + 2 * grain).as_deref(), Some("---p"));

            // Back to a placeholder, coalesce the splits, release the backing reservation.
            assert_eq!(
                VirtualFree(base as *mut c_void, grain, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER),
                1,
                "return a replacement to a placeholder"
            );
            assert_eq!(
                VirtualFree(base as *mut c_void, 3 * grain, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS),
                1,
                "coalesce adjacent placeholders"
            );
            assert_eq!(VirtualFree(base as *mut c_void, 0, MEM_RELEASE), 1);
        }
    }

    /// The host mapping containing `addr`, from `/proc/self/maps`.
    fn mapping_size_at(addr: usize) -> Option<usize> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        for line in maps.lines() {
            let range = line.split_whitespace().next()?;
            let (a, b) = range.split_once('-')?;
            let (a, b) = (
                usize::from_str_radix(a, 16).ok()?,
                usize::from_str_radix(b, 16).ok()?,
            );
            if addr >= a && addr < b {
                return Some(b - a);
            }
        }
        None
    }

    #[test]
    fn create_thread_stack_has_a_host_minimum() {
        // Retail Step 50: the title asks for 0x8000 of stack, and the Vulkan loader
        // overflowed it inside `vkCreateInstance` on the guest render thread. Every
        // guest thread must get at least MIN_HOST_THREAD_STACK of host mapping,
        // whether the title names a small size or asks for the default (0).
        static STACK_BYTES: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "win64" fn probe(_param: *mut c_void) -> u32 {
            let local = 0u8;
            let size = mapping_size_at(&local as *const u8 as usize).unwrap_or(0);
            STACK_BYTES.store(size, Ordering::Release);
            0
        }
        unsafe {
            for requested in [0x8000usize, 0] {
                STACK_BYTES.store(0, Ordering::Release);
                let h = CreateThread(
                    std::ptr::null(),
                    requested,
                    probe as usize,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_ne!(h, 0);
                assert_eq!(unsafe { WaitForSingleObject(h, 5000) }, WAIT_OBJECT_0);
                let mapped = STACK_BYTES.load(Ordering::Acquire);
                assert!(
                    mapped >= MIN_HOST_THREAD_STACK,
                    "CreateThread(stack {requested:#x}) mapped {mapped:#x} of host stack, \
                     below the {MIN_HOST_THREAD_STACK:#x} minimum"
                );
                assert_eq!(unsafe { CloseHandle(h) }, 1);
            }
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
                characteristics: 0,
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
    fn system_info_reports_the_host_topology() {
        unsafe {
            // SYSTEM_INFO: AMD64, host page size, sane processor count.
            let mut info: SystemInfo = std::mem::zeroed();
            GetSystemInfo(&mut info);
            assert_eq!(
                (info.arch, info.page, info.allocation_granularity),
                (9, page_size() as u32, 0x1_0000)
            );
            assert!(info.num_processors >= 1);
            assert_eq!(info.num_processors, info.active_mask.count_ones());
            GetNativeSystemInfo(&mut info);
            assert_eq!(info.arch, 9);
            GetSystemInfo(std::ptr::null_mut()); // null is a no-op, not a fault.
            // Size probe: NULL buffer reports the required size with INSUFFICIENT_BUFFER.
            let mut need = 0u32;
            assert_eq!(GetLogicalProcessorInformation(std::ptr::null_mut(), &mut need), 0);
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            assert!(need >= 64 && need % 32 == 0);
            // Short buffer reports the same requirement.
            let mut half = vec![0u8; (need - 32) as usize];
            let mut half_len = half.len() as u32;
            assert_eq!(GetLogicalProcessorInformation(half.as_mut_ptr(), &mut half_len), 0);
            assert_eq!(half_len, need);
            // Full buffer: core entries plus one package entry; masks cover the affinity.
            let mut full = vec![0u8; need as usize];
            let mut full_len = need;
            assert_eq!(GetLogicalProcessorInformation(full.as_mut_ptr(), &mut full_len), 1);
            assert_eq!((full_len, GetLastError()), (need, ERROR_SUCCESS));
            let n = need as usize / 32;
            let mut cores = 0u32;
            let mut or_mask = 0usize;
            for i in 0..n {
                let base = i * 32;
                let mask = usize::from_le_bytes(full[base..base + 8].try_into().unwrap());
                let rel = u32::from_le_bytes(full[base + 8..base + 12].try_into().unwrap());
                or_mask |= mask;
                if rel == RELATION_PROCESSOR_CORE {
                    cores += 1;
                    assert_eq!(mask.count_ones(), 1);
                } else {
                    assert_eq!(rel, RELATION_PROCESSOR_PACKAGE);
                }
            }
            assert_eq!(cores as usize, n - 1);
            assert_eq!(or_mask, process_affinity());
            // Null length fails cleanly.
            assert_eq!(
                GetLogicalProcessorInformation(full.as_mut_ptr(), std::ptr::null_mut()),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
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
    fn precise_file_time_tracks_realtime_clock() {
        // Retail M1.1a: the auto-stub never wrote the out-param (3 calls before the
        // fault), so callers read stale stack. The precise variant must land the
        // current 100 ns FILETIME.
        let mut ft = FileTime { low: 0, high: 0 };
        unsafe { GetSystemTimePreciseAsFileTime(&mut ft) };
        let ticks = ((ft.high as u64) << 32) | ft.low as u64;
        assert_ne!(ticks, 0);
        let unix = (ticks as i128 / 10_000_000) - 11_644_473_600;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i128;
        assert!(unix > 1_577_836_800, "filetime {unix} is before 2020-01-01");
        assert!((unix - now).abs() <= 2, "filetime {unix} vs now {now}");
        // NULL is a safe no-op, and the name is wired into the export table.
        unsafe { GetSystemTimePreciseAsFileTime(std::ptr::null_mut()) };
        assert!(exports().iter().any(|(n, _)| *n == "GetSystemTimePreciseAsFileTime"));
    }

    // Resolve a COM export by name. The tests compile on a tree without the
    // implementation (no direct symbol references), so the pre-fix failure is a
    // clean "must be wired" RED instead of a compile error. Values come from
    // util/combaseapi and are duplicated here for the same reason.
    fn com_export(name: &str) -> usize {
        let addr = exports()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} must be wired"))
            .1;
        assert_ne!(addr, 0, "{name} must be implemented, not stub metadata");
        addr
    }

    #[test]
    fn com_entry_points_are_wired() {
        for name in ["CoInitializeEx", "CoUninitialize", "CoCreateInstance"] {
            com_export(name);
        }
    }

    #[test]
    fn com_activation_fails_closed_and_clears_the_out_pointer() {
        const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
        const REGDB_E_CLASSNOTAVAILABLE: i32 = 0x8004_0154u32 as i32;
        let create: unsafe extern "win64" fn(
            *const std::ffi::c_void,
            *mut std::ffi::c_void,
            u32,
            *const std::ffi::c_void,
            *mut *mut std::ffi::c_void,
        ) -> i32 = unsafe { std::mem::transmute(com_export("CoCreateInstance")) };
        // CLSID_MMDeviceEnumerator / IID_IMMDeviceEnumerator, as retail passes them.
        let clsid = [
            0x95u8, 0x03, 0xde, 0xbc, 0x2f, 0xe5, 0x7c, 0x46, 0x8e, 0x3d, 0xc4, 0x57, 0x92,
            0x91, 0x69, 0x2e,
        ];
        let iid = [
            0xd2u8, 0x64, 0x56, 0xa9, 0x14, 0x96, 0x35, 0x4f, 0xa7, 0x46, 0xde, 0x8d, 0xb6,
            0x36, 0x17, 0xe6,
        ];
        unsafe {
            let mut out = 1usize as *mut std::ffi::c_void;
            assert_eq!(
                create(
                    clsid.as_ptr() as *const std::ffi::c_void,
                    std::ptr::null_mut(),
                    0x17,
                    iid.as_ptr() as *const std::ffi::c_void,
                    &mut out
                ),
                REGDB_E_CLASSNOTAVAILABLE
            );
            assert!(out.is_null(), "a failed activation must clear *ppv");
            // A stale object must not survive any failure path.
            let mut out = 1usize as *mut std::ffi::c_void;
            assert_eq!(
                create(
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    0x17,
                    iid.as_ptr() as *const std::ffi::c_void,
                    &mut out
                ),
                E_INVALIDARG
            );
            assert!(out.is_null());
            assert_eq!(
                create(
                    clsid.as_ptr() as *const std::ffi::c_void,
                    std::ptr::null_mut(),
                    0x17,
                    std::ptr::null(),
                    &mut out
                ),
                E_INVALIDARG
            );
            assert!(out.is_null());
            // A NULL out pointer is invalid, not a crash.
            assert_eq!(
                create(
                    clsid.as_ptr() as *const std::ffi::c_void,
                    std::ptr::null_mut(),
                    0x17,
                    iid.as_ptr() as *const std::ffi::c_void,
                    std::ptr::null_mut()
                ),
                E_INVALIDARG
            );
        }
    }

    #[test]
    fn com_apartments_follow_the_windows_init_contract() {
        const S_OK: i32 = 0;
        const S_FALSE: i32 = 1;
        const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
        const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
        const COINIT_APARTMENTTHREADED: u32 = 0x2;
        // A fresh thread, so no other test's apartment state leaks in.
        std::thread::spawn(|| {
            let init: unsafe extern "win64" fn(*mut std::ffi::c_void, u32) -> i32 =
                unsafe { std::mem::transmute(com_export("CoInitializeEx")) };
            let uninit: unsafe extern "win64" fn() =
                unsafe { std::mem::transmute(com_export("CoUninitialize")) };
            unsafe {
                assert_eq!(init(std::ptr::null_mut(), 0x10), E_INVALIDARG);
                assert_eq!(init(std::ptr::null_mut(), 0), S_OK);
                assert_eq!(init(std::ptr::null_mut(), 0x4), S_FALSE);
                assert_eq!(
                    init(std::ptr::null_mut(), COINIT_APARTMENTTHREADED),
                    RPC_E_CHANGED_MODE
                );
                uninit();
                uninit();
                assert_eq!(init(std::ptr::null_mut(), COINIT_APARTMENTTHREADED), S_OK);
                uninit();
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn guid_creation_and_formatting_follow_the_win32_contract() {
        // Retail Step 80: the XGameSave failure path calls CoCreateGuid and
        // StringFromGUID2 (exe+0x55f3865/0x55f3883) and then builds a
        // std::wstring from the formatted buffer. Both were auto-stubs that
        // wrote nothing, so the GUID was all-zero and the string buffer kept
        // uninitialized stack.
        type CoCreateGuid = unsafe extern "win64" fn(*mut u8) -> i32;
        type StringFromGuid2 = unsafe extern "win64" fn(*const u8, *mut u16, i32) -> i32;
        let create: CoCreateGuid = unsafe { std::mem::transmute(com_export("CoCreateGuid")) };
        let format: StringFromGuid2 =
            unsafe { std::mem::transmute(com_export("StringFromGUID2")) };
        unsafe {
            const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
            assert_eq!(create(std::ptr::null_mut()), E_INVALIDARG);
            // A v4 GUID: version nibble 4, RFC 4122 variant, and two draws
            // from the OS entropy source differ.
            let mut a = [0u8; 16];
            let mut b = [0u8; 16];
            assert_eq!(create(a.as_mut_ptr()), 0);
            assert_eq!(create(b.as_mut_ptr()), 0);
            assert_ne!(a, b);
            assert_eq!(a[7] & 0xF0, 0x40, "version nibble");
            assert_eq!(a[8] & 0xC0, 0x80, "RFC 4122 variant");
            // The brace form, uppercase hex, fields in GUID byte order.
            let g: [u8; 16] = [
                0x8b, 0x2c, 0x0d, 0x01, 0x34, 0x12, 0x78, 0x56, 0xab, 0xcd, 0xef, 0x01, 0x23,
                0x45, 0x67, 0x89,
            ];
            let expect: Vec<u16> = "{010D2C8B-1234-5678-ABCD-EF0123456789}\0"
                .encode_utf16()
                .collect();
            assert_eq!(expect.len(), 39);
            let mut buf = [0xFFFFu16; 40];
            assert_eq!(format(g.as_ptr(), buf.as_mut_ptr(), 40), 39);
            assert_eq!(&buf[..39], &expect[..]);
            assert_eq!(buf[39], 0xFFFF, "nothing past the NUL");
            // Too small: 0 and no write.
            let mut small = [0xFFFFu16; 40];
            assert_eq!(format(g.as_ptr(), small.as_mut_ptr(), 38), 0);
            assert_eq!(small, [0xFFFFu16; 40]);
            // Exact fit puts the NUL in the last unit.
            let mut exact = [0xFFFFu16; 39];
            assert_eq!(format(g.as_ptr(), exact.as_mut_ptr(), 39), 39);
            assert_eq!(exact[38], 0);
            // Null / zero-length arguments are refused without writing.
            assert_eq!(format(std::ptr::null(), buf.as_mut_ptr(), 40), 0);
            assert_eq!(format(g.as_ptr(), std::ptr::null_mut(), 40), 0);
            let mut zero = [0xFFFFu16; 40];
            assert_eq!(format(g.as_ptr(), zero.as_mut_ptr(), 0), 0);
            assert_eq!(zero, [0xFFFFu16; 40]);
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

    #[test]
    fn thread_priority_tiers_map_to_win32_priorities() {
        // Pure mapping: standard constants and background flags pass through.
        assert_eq!(map_thread_priority(THREAD_PRIORITY_IDLE), THREAD_PRIORITY_IDLE);
        assert_eq!(map_thread_priority(THREAD_PRIORITY_LOWEST), THREAD_PRIORITY_LOWEST);
        assert_eq!(map_thread_priority(THREAD_PRIORITY_BELOW_NORMAL), THREAD_PRIORITY_BELOW_NORMAL);
        assert_eq!(map_thread_priority(THREAD_PRIORITY_NORMAL), THREAD_PRIORITY_NORMAL);
        assert_eq!(map_thread_priority(THREAD_PRIORITY_ABOVE_NORMAL), THREAD_PRIORITY_ABOVE_NORMAL);
        assert_eq!(map_thread_priority(THREAD_PRIORITY_HIGHEST), THREAD_PRIORITY_HIGHEST);
        assert_eq!(map_thread_priority(THREAD_PRIORITY_TIME_CRITICAL), THREAD_PRIORITY_TIME_CRITICAL);
        assert_eq!(map_thread_priority(THREAD_MODE_BACKGROUND_BEGIN), THREAD_MODE_BACKGROUND_BEGIN);
        assert_eq!(map_thread_priority(THREAD_MODE_BACKGROUND_END), THREAD_MODE_BACKGROUND_END);
        assert_eq!(map_thread_priority(-16), THREAD_PRIORITY_IDLE);
        assert_eq!(map_thread_priority(-100), THREAD_PRIORITY_IDLE);
        assert_eq!(map_thread_priority(-7), THREAD_PRIORITY_LOWEST);
        assert_eq!(map_thread_priority(-3), THREAD_PRIORITY_LOWEST);
        assert_eq!(map_thread_priority(3), THREAD_PRIORITY_HIGHEST);
        assert_eq!(map_thread_priority(6), THREAD_PRIORITY_HIGHEST);
        assert_eq!(map_thread_priority(7), THREAD_PRIORITY_TIME_CRITICAL);
        assert_eq!(map_thread_priority(31), THREAD_PRIORITY_TIME_CRITICAL);
        unsafe {
            // Per-handle storage keeps the mapped value, not the raw tier.
            let h = CreateThread(
                std::ptr::null(),
                0,
                thread_exit_zero as usize,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_ne!(h, 0);
            assert_eq!(SetThreadPriority(h, 3), 1);
            assert_eq!(GetThreadPriority(h), THREAD_PRIORITY_HIGHEST);
            assert_eq!(SetThreadPriority(h, 7), 1);
            assert_eq!(GetThreadPriority(h), THREAD_PRIORITY_TIME_CRITICAL);
            assert_eq!(SetThreadPriority(h, -7), 1);
            assert_eq!(GetThreadPriority(h), THREAD_PRIORITY_LOWEST);
            assert_eq!(SetThreadPriority(h, THREAD_PRIORITY_NORMAL), 1);
            assert_eq!(GetThreadPriority(h), THREAD_PRIORITY_NORMAL);
            // Current pseudo-handle stores the mapped tier too.
            assert_eq!(SetThreadPriority(current_thread_handle(), 5), 1);
            assert_eq!(GetThreadPriority(current_thread_handle()), THREAD_PRIORITY_HIGHEST);
            assert_eq!(SetThreadPriority(current_thread_handle(), THREAD_PRIORITY_NORMAL), 1);
            // Unknown handles fail cleanly with the error return.
            assert_eq!(GetThreadPriority(0xDEAD_BEEF), THREAD_PRIORITY_ERROR_RETURN);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            // Boost is remembered per handle; bad handles and null out-pointers fail.
            assert_eq!(SetThreadPriorityBoost(h, 1), 1);
            let mut disabled = 0;
            assert_eq!(GetThreadPriorityBoost(h, &mut disabled), 1);
            assert_eq!(disabled, 1);
            assert_eq!(SetThreadPriorityBoost(h, 0), 1);
            assert_eq!(GetThreadPriorityBoost(h, &mut disabled), 1);
            assert_eq!(disabled, 0);
            assert_eq!(SetThreadPriorityBoost(0xDEAD_BEEF, 1), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            assert_eq!(GetThreadPriorityBoost(0xDEAD_BEEF, &mut disabled), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            assert_eq!(GetThreadPriorityBoost(h, std::ptr::null_mut()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(WaitForSingleObject(h, 5000), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(h), 1);
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

    /// `ReadFile`/`WriteFile` honor the explicit `OVERLAPPED` offset on a
    /// synchronously-opened file and leave the handle positioned at
    /// `offset + transferred`, like the Windows contract. The retail pack reader
    /// issues its RPF9 header/table reads this way: ignoring the offset served
    /// the file position instead, which turned `appdata0.rpf`'s kind byte at
    /// logical 0x2e (0x09, class 4) into file 0x102e (0x0b, class 5) and
    /// aborted the boot in the class-5 pack-cache refusal.
    #[test]
    fn overlapped_file_io_uses_the_explicit_offset() {
        unsafe {
            let _ = ensure_test_vfs();
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const CREATE_ALWAYS: u32 = 2;
            const OPEN_EXISTING: u32 = 3;
            let name = format!(
                "G:\\data\\overlapped_{}_{}.bin",
                std::process::id(),
                line!()
            );

            let h = open_w(&name, GENERIC_WRITE, CREATE_ALWAYS);
            let mut done = 0u32;
            assert_eq!(
                WriteFile(
                    h,
                    b"0123456789abcdef".as_ptr() as *const c_void,
                    16,
                    &mut done,
                    std::ptr::null_mut()
                ),
                1
            );
            assert_eq!(CloseHandle(h), 1);

            let h = open_w(&name, GENERIC_READ, OPEN_EXISTING);
            // Park the handle away from the offset the OVERLAPPED asks for.
            let mut pos = 0u64;
            assert_eq!(SetFilePointerEx(h, 8, &mut pos, FILE_BEGIN), 1);
            let mut ov: Overlapped = std::mem::zeroed();
            let mut buf = [0xFFu8; 16];
            assert_eq!(
                ReadFile(
                    h,
                    buf.as_mut_ptr() as *mut c_void,
                    4,
                    &mut done,
                    &mut ov as *mut Overlapped as *mut c_void
                ),
                1
            );
            assert_eq!(done, 4);
            assert_eq!(&buf[..4], b"0123");
            assert_eq!(ov.internal_high, 4);
            // Synchronous completion leaves the handle at offset + transferred.
            assert_eq!(
                ReadFile(h, buf.as_mut_ptr() as *mut c_void, 4, &mut done, std::ptr::null_mut()),
                1
            );
            assert_eq!(&buf[..4], b"4567");
            // OffsetHigh participates in the 64-bit offset.
            let mut high: Overlapped = std::mem::zeroed();
            high.offset = 12;
            assert_eq!(
                ReadFile(
                    h,
                    buf.as_mut_ptr() as *mut c_void,
                    4,
                    &mut done,
                    &mut high as *mut Overlapped as *mut c_void
                ),
                1
            );
            assert_eq!(done, 4);
            assert_eq!(&buf[..4], b"cdef");
            assert_eq!(CloseHandle(h), 1);

            // The same rule applies to writes: only the requested bytes change.
            let mut ov: Overlapped = std::mem::zeroed();
            ov.offset = 4;
            let h = open_w(&name, GENERIC_WRITE, OPEN_EXISTING);
            let mut done = 0u32;
            assert_eq!(
                SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END),
                1
            );
            assert_eq!(
                WriteFile(
                    h,
                    b"XY".as_ptr() as *const c_void,
                    2,
                    &mut done,
                    &mut ov as *mut Overlapped as *mut c_void
                ),
                1
            );
            assert_eq!(done, 2);
            assert_eq!(ov.internal_high, 2);
            assert_eq!(CloseHandle(h), 1);

            let h = open_w(&name, GENERIC_READ, OPEN_EXISTING);
            assert_eq!(read_all(h), b"0123XY6789abcdef");
            assert_eq!(CloseHandle(h), 1);
            let del = crate::util::wide(&name);
            assert_eq!(DeleteFileW(del.as_ptr()), 1);
        }
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

            // Adversarial paths never open host files. An above-mount `..`
            // (upstream #1170) clamps at the mount root, so it names the
            // missing game file `outside.txt`, exactly like any other miss.
            for bad in ["Z:\\Windows\\notepad.exe"] {
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
            for bad in ["G:\\..\\outside.txt"] {
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
                assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND, "{bad}");
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
    fn delete_on_close_keeps_the_game_file_hidden_after_last_close() {
        // Upstream #1038 through the exported shims (Linux twin): a title opens
        // `G:\data\...` with FILE_FLAG_DELETE_ON_CLOSE, reads the original bytes
        // while open, opens a second handle, closes both — the original stays
        // hidden and the game folder stays byte-identical.
        let _ = ensure_test_vfs();
        let gv = crate::vfs::get().expect("global VFS initialised");
        let global_game = std::path::Path::new(gv.game_root());
        let probe = format!("doc_probe_{}.txt", std::process::id());
        let rel = format!("data\\{probe}");
        std::fs::create_dir_all(global_game.join("data")).ok();
        std::fs::write(global_game.join("data").join(&probe), b"original").ok();
        let before = tree_hash(global_game);
        unsafe {
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const DELETE: u32 = 0x0001_0000;
            const OPEN_EXISTING: u32 = 3;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            let open = |access: u32, flags: u32| {
                let w = crate::util::wide(&format!("G:\\data\\{probe}"));
                let h = CreateFileW(
                    w.as_ptr(),
                    access,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    flags,
                    0,
                );
                assert_ne!(h, INVALID_HANDLE_VALUE, "open failed err {}", GetLastError());
                h
            };
            // First handle carries DELETE_ON_CLOSE; the original bytes are readable.
            let h1 = open(GENERIC_READ | DELETE, FILE_FLAG_DELETE_ON_CLOSE);
            assert_eq!(read_all(h1), b"original");
            // Second handle opens the existing overlay entry.
            let h2 = open(GENERIC_WRITE, 0);
            assert_eq!(CloseHandle(h1), 1);
            assert_eq!(CloseHandle(h2), 1);
            // Both handles closed: the overlay copy is gone, the whiteout hides
            // the original, and reopening fails like a deleted file.
            assert!(gv.is_whiteout(&rel), "delete-on-close must hide the original");
            assert_eq!(gv.read_path_rel(&rel), None);
            let w = crate::util::wide(&format!("G:\\data\\{probe}"));
            assert_eq!(
                CreateFileW(w.as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, 0),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            assert_eq!(tree_hash(global_game), before, "game folder must stay byte-identical");
        }
        // Transient seed: remove it so later tests see a clean game root.
        let _ = std::fs::remove_file(global_game.join("data").join(&probe));
    }

    #[test]
    fn handle_delete_hides_the_game_file_after_close() {
        // Linux twin of upstream `delete_by_handle_hides_the_game_file` (#1054):
        // MSVC's `std::filesystem::remove` deletes through the handle
        // (`SetFileInformationByHandle(FileDisposition)`). The handle names the
        // overlay copy, so the whiteout is recorded at set time and the copy is
        // removed when the marking handle closes; a taken-back delete is
        // reversible. The game folder stays byte-identical throughout.
        let _ = ensure_test_vfs();
        let gv = crate::vfs::get().expect("global VFS initialised");
        let global_game = std::path::Path::new(gv.game_root());
        let pid = std::process::id();
        let probe = format!("hd_del_{pid}.txt");
        let probe2 = format!("hd_del_gone_{pid}.txt");
        std::fs::create_dir_all(global_game.join("data")).ok();
        std::fs::write(global_game.join("data").join(&probe), b"original").ok();
        std::fs::write(global_game.join("data").join(&probe2), b"bye").ok();
        unsafe {
            const DELETE: u32 = 0x0001_0000;
            const OPEN_EXISTING: u32 = 3;
            const GENERIC_READ: u32 = 0x8000_0000;
            const INFO: u32 = 4; // FILE_DISPOSITION_INFO
            const INFO_EX: u32 = 21; // FILE_DISPOSITION_INFO_EX
            let open_del = |name: &str| {
                let w = crate::util::wide(&format!("G:\\data\\{name}"));
                let h = CreateFileW(
                    w.as_ptr(),
                    DELETE | 0x80 | 0x100, // DELETE | READ_ATTRIBUTES | WRITE_ATTRIBUTES
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    0,
                );
                assert_ne!(h, INVALID_HANDLE_VALUE, "open {name} err {}", GetLastError());
                h
            };
            let set = |h: usize, class: u32, p: *const c_void, n: u32| {
                SetFileInformationByHandle(h, class, p, n)
            };

            // FileDispositionInfo { DeleteFile: TRUE }
            let rel = format!("data\\{probe}");
            let h = open_del(&probe);
            SetLastError(4242);
            assert_eq!(set(h, INFO, &1u8 as *const u8 as *const c_void, 1), 1);
            assert_eq!(GetLastError(), 4242, "a successful call leaves the last error alone");
            assert_eq!(CloseHandle(h), 1);
            assert!(gv.is_whiteout(&rel));
            assert!(gv.read_path_rel(&rel).is_none());
            let w = crate::util::wide(&format!("G:\\data\\{probe}"));
            assert_eq!(
                CreateFileW(w.as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, 0),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            assert_eq!(std::fs::read(global_game.join("data").join(&probe)).unwrap(), b"original");

            // FileDispositionInfoEx { DELETE | POSIX_SEMANTICS }: supported here,
            // so the game file is hidden the same way.
            let rel2 = format!("data\\{probe2}");
            let h = open_del(&probe2);
            assert_eq!(set(h, INFO_EX, &3u32 as *const u32 as *const c_void, 4), 1);
            assert_eq!(CloseHandle(h), 1);
            assert!(gv.is_whiteout(&rel2));
            assert!(gv.read_path_rel(&rel2).is_none());
            assert_eq!(std::fs::read(global_game.join("data").join(&probe2)).unwrap(), b"bye");
        }
        let _ = std::fs::remove_file(global_game.join("data").join(&probe));
        let _ = std::fs::remove_file(global_game.join("data").join(&probe2));
    }

    #[test]
    fn set_file_info_by_handle_passes_everything_else_through() {
        // Linux twin of upstream
        // `set_file_info_by_handle_passes_everything_else_through` (#1054):
        // directories are never hidden, a taken-back delete keeps the file, a
        // host file is deleted with no whiteout, and a bad handle fails with
        // ERROR_INVALID_HANDLE.
        let _ = ensure_test_vfs();
        let gv = crate::vfs::get().expect("global VFS initialised");
        let global_game = std::path::Path::new(gv.game_root());
        let pid = std::process::id();
        let dir_probe = format!("hd_pass_dir_{pid}");
        let inner = format!("{dir_probe}\\inner.txt");
        let file_probe = format!("hd_pass_{pid}.txt");
        let ext_probe = format!("hd_pass_ext_{pid}.txt");
        std::fs::create_dir_all(global_game.join(&dir_probe)).ok();
        std::fs::write(global_game.join(&dir_probe).join("inner.txt"), b"inner").ok();
        std::fs::write(global_game.join("data").join(&file_probe), b"original").ok();
        unsafe {
            const DELETE: u32 = 0x0001_0000;
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const OPEN_EXISTING: u32 = 3;
            const CREATE_ALWAYS: u32 = 2;
            const INFO: u32 = 4; // FILE_DISPOSITION_INFO
            let open_del = |path: &str| {
                let w = crate::util::wide(path);
                let h = CreateFileW(
                    w.as_ptr(),
                    DELETE | 0x80 | 0x100,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    0,
                );
                assert_ne!(h, INVALID_HANDLE_VALUE, "open {path} err {}", GetLastError());
                h
            };
            let set = |h: usize, class: u32, p: *const c_void, n: u32| {
                SetFileInformationByHandle(h, class, p, n)
            };
            let no_pass_whiteout = || {
                assert!(
                    gv.whiteouts().iter().all(|w| !w.contains("hd_pass")),
                    "{:?}",
                    gv.whiteouts()
                );
            };

            // A directory is not hidden: its game files stay visible.
            let h = open_del(&format!("G:\\{dir_probe}"));
            assert_eq!(set(h, INFO, &1u8 as *const u8 as *const c_void, 1), 1);
            assert_eq!(CloseHandle(h), 1);
            no_pass_whiteout();
            let w = crate::util::wide(&format!("G:\\{inner}"));
            let rh = CreateFileW(w.as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, 0);
            assert_ne!(rh, INVALID_HANDLE_VALUE);
            assert_eq!(read_all(rh), b"inner");
            assert_eq!(CloseHandle(rh), 1);

            // A delete that is taken back, and another class (FileBasicInfo,
            // all zero = no change): the file stays, no whiteout.
            let rel = format!("data\\{file_probe}");
            let h = open_del(&format!("G:\\data\\{file_probe}"));
            assert_eq!(set(h, INFO, &1u8 as *const u8 as *const c_void, 1), 1);
            assert_eq!(set(h, INFO, &0u8 as *const u8 as *const c_void, 1), 1);
            assert_eq!(set(h, 0, &[0u64; 5] as *const u64 as *const c_void, 40), 1);
            assert_eq!(CloseHandle(h), 1);
            assert!(!gv.is_whiteout(&rel));
            let w = crate::util::wide(&format!("G:\\data\\{file_probe}"));
            let rh = CreateFileW(w.as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, 0);
            assert_ne!(rh, INVALID_HANDLE_VALUE);
            assert_eq!(read_all(rh), b"original");
            assert_eq!(CloseHandle(rh), 1);
            no_pass_whiteout();

            // A file outside the overlay is deleted and leaves no whiteout.
            let w = crate::util::wide(&format!("T:\\{ext_probe}"));
            let eh = CreateFileW(w.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, 0, 0);
            assert_ne!(eh, INVALID_HANDLE_VALUE);
            assert_eq!(CloseHandle(eh), 1);
            let h = open_del(&format!("T:\\{ext_probe}"));
            assert_eq!(set(h, INFO, &1u8 as *const u8 as *const c_void, 1), 1);
            assert_eq!(CloseHandle(h), 1);
            if let crate::vfs::Target::Translated(p) = gv.classify(&format!("T:\\{ext_probe}")) {
                assert!(!std::path::Path::new(&p).exists());
            } else {
                panic!("T: must classify Translated");
            }
            no_pass_whiteout();

            // A failed call returns the invalid-handle error.
            assert_eq!(
                set(INVALID_HANDLE_VALUE, INFO, &1u8 as *const u8 as *const c_void, 1),
                0
            );
            assert_eq!(GetLastError(), 6); // ERROR_INVALID_HANDLE
        }
        let _ = std::fs::remove_file(global_game.join("data").join(&file_probe));
        let _ = std::fs::remove_file(global_game.join(&dir_probe).join("inner.txt"));
        let _ = std::fs::remove_dir(global_game.join(&dir_probe));
    }

    #[test]
    fn find_first_file_ex_a_lists_the_merged_game_folder() {
        // Linux twin of upstream #1264 (`hk_find_first_ex_a`): the ANSI Ex entry
        // must return the same merged overlay view as `FindFirstFileA`, for both
        // common info levels, and reject exotic level/op values.
        let _ = ensure_test_vfs();
        let gv = crate::vfs::get().expect("global VFS initialised");
        let global_game = std::path::Path::new(gv.game_root());
        let probe = format!("exa_probe_{}.txt", std::process::id());
        std::fs::create_dir_all(global_game.join("data")).ok();
        std::fs::write(global_game.join("data").join(&probe), b"exa").ok();
        unsafe fn collect_anchors(pat: *const u8, level: i32) -> Vec<String> {
            let mut data: FindDataW = std::mem::zeroed();
            let fh = FindFirstFileExA(
                pat,
                level,
                &mut data as *mut _ as *mut c_void,
                0,
                std::ptr::null(),
                0,
            );
            assert_ne!(
                fh,
                INVALID_HANDLE_VALUE,
                "ExA level {level} failed err {}",
                GetLastError()
            );
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
            names
        }
        unsafe {
            let pat = std::ffi::CString::new("G:\\data\\*").unwrap();
            for level in [0, 1] {
                let names = collect_anchors(pat.as_ptr() as *const u8, level);
                assert!(
                    names.iter().any(|n| n == &probe),
                    "level {level}: missing {probe} in {names:?}"
                );
            }
            // Exotic level/op values have no virtualised meaning: reject like ExW.
            let mut data: FindDataW = std::mem::zeroed();
            assert_eq!(
                FindFirstFileExA(
                    pat.as_ptr() as *const u8,
                    2,
                    &mut data as *mut _ as *mut c_void,
                    0,
                    std::ptr::null(),
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(
                FindFirstFileExA(
                    pat.as_ptr() as *const u8,
                    0,
                    &mut data as *mut _ as *mut c_void,
                    2,
                    std::ptr::null(),
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn create_directory_second_create_reports_already_exists() {
        // Reused data roots hit this: the title runs `create_dir_all("S:/Users/xcl/games")`
        // on every boot, so the 2nd boot's `CreateDirectoryW` on the existing PLS dir
        // must report Win32 ERROR_ALREADY_EXISTS (183), not the raw host EEXIST (17).
        use crate::vfs::{Target, ERROR_ALREADY_EXISTS};
        static PROBE_SEQ: AtomicU32 = AtomicU32::new(0);
        let _ = ensure_test_vfs();
        // `S:\` itself (the host `pls` dir) may not exist in the fixture yet; the
        // launcher creates it on a real boot, and `create_dir_all` in the title
        // creates each level in turn. Ensure the root so the single-level
        // `CreateDirectoryW` probe below tests EEXIST, not ENOENT.
        if let Target::Translated(root) = crate::vfs::get().unwrap().classify("S:\\") {
            std::fs::create_dir_all(&root).unwrap();
        }
        unsafe {
            let n = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = format!("S:\\eexist_probe_{}_{n}", std::process::id());
            let w = crate::util::wide(&path);
            assert_eq!(CreateDirectoryW(w.as_ptr(), std::ptr::null()), 1, "{path}");
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            let w2 = crate::util::wide(&path);
            assert_eq!(CreateDirectoryW(w2.as_ptr(), std::ptr::null()), 0, "{path}");
            assert_eq!(GetLastError(), ERROR_ALREADY_EXISTS, "{path}");
            let w3 = crate::util::wide(&path);
            assert_eq!(RemoveDirectoryW(w3.as_ptr()), 1, "{path}");
        }
    }

    #[test]
    fn createfile2_flags_require_a_readable_sized_structure() {
        assert_eq!(create_file_2_flags(std::ptr::null()), Some(0));
        let short = 4u32;
        assert_eq!(create_file_2_flags((&short as *const u32).cast()), None);
        assert_eq!(create_file_2_flags(1usize as *const c_void), None);
        let params = [12u32, 0x1234, 0x5678];
        assert_eq!(create_file_2_flags(params.as_ptr().cast()), Some(0x5678));
    }

    #[test]
    fn copy_and_move_reject_bad_sources_before_staging_the_overlay() {
        // Linux port of upstream #1779 (`copy_and_move_reject_bad_sources_...` in
        // `vfs_hooks.rs`, Windows-only): the live Linux path is `copy_impl` /
        // `move_impl` here, exercised through the `CopyFileW` / `MoveFileExW` shims
        // on the shared global VFS. All legs must fail without staging anything
        // into the overlay; game originals stay untouched.
        use crate::vfs::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
        let _ = ensure_test_vfs();
        let v = crate::vfs::get().expect("global VFS initialised");
        let game_root = std::path::Path::new(v.game_root()).to_path_buf();
        let tag = format!("badsrc_{}", std::process::id());
        let rel = |n: &str| format!("data\\{tag}\\{n}");
        let gpath = |n: &str| format!("G:\\data\\{tag}\\{n}");
        std::fs::create_dir_all(game_root.join("data").join(&tag).join("sub")).unwrap();
        std::fs::create_dir_all(game_root.join("data").join(&tag).join("dir")).unwrap();
        std::fs::write(game_root.join("data").join(&tag).join("orig.txt"), b"original").unwrap();
        std::fs::write(game_root.join("data").join(&tag).join("caf\u{e9}.txt"), b"original").unwrap();
        let overlay_untouched = || {
            assert!(
                !std::path::Path::new(&v.overlay_path(&rel("orig.txt"))).exists(),
                "self-copy staged an overlay file"
            );
            assert!(
                !std::path::Path::new(&v.overlay_path(&rel("sub"))).exists(),
                "a failed copy/move created overlay dirs"
            );
            assert!(
                !std::path::Path::new(&v.overlay_path(&rel(""))).exists(),
                "a failed copy/move created the overlay tag dir"
            );
        };
        unsafe {
            // Copying an un-overlaid game file onto itself (same path and
            // case-differing, including the accented pair) must fail.
            for (src, dst) in [
                (gpath("orig.txt"), gpath("orig.txt")),
                (gpath("orig.txt"), gpath("ORIG.TXT")),
                (gpath("caf\u{e9}.txt"), gpath("CAF\u{c9}.txt")),
            ] {
                let (s, d) = (crate::util::wide(&src), crate::util::wide(&dst));
                assert_eq!(CopyFileW(s.as_ptr(), d.as_ptr(), 0), 0, "self-copy {src} -> {dst} must fail");
                overlay_untouched();
            }
            assert_eq!(
                std::fs::read(game_root.join("data").join(&tag).join("orig.txt")).unwrap(),
                b"original"
            );

            // A directory source fails with ERROR_ACCESS_DENIED.
            let (s, d) = (
                crate::util::wide(&gpath("dir")),
                crate::util::wide(&gpath("sub\\d.txt")),
            );
            assert_eq!(CopyFileW(s.as_ptr(), d.as_ptr(), 0), 0);
            assert_eq!(GetLastError(), ERROR_ACCESS_DENIED);
            overlay_untouched();

            // A missing external source must not create overlay dirs for the destination.
            let missing = format!("T:\\{tag}_missing.txt");
            let (s, d) = (crate::util::wide(&missing), crate::util::wide(&gpath("sub\\new.txt")));
            assert_eq!(CopyFileW(s.as_ptr(), d.as_ptr(), 0), 0);
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            overlay_untouched();
            assert_eq!(MoveFileExW(s.as_ptr(), d.as_ptr(), 0), 0);
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            overlay_untouched();

            // An exact self-move is a safe no-op.
            let (s, d) = (crate::util::wide(&gpath("orig.txt")), crate::util::wide(&gpath("orig.txt")));
            assert_eq!(MoveFileExW(s.as_ptr(), d.as_ptr(), 0), 1);
            overlay_untouched();
        }
    }

    #[test]
    fn find_first_null_data_fails_instead_of_faulting() {
        // Linux port of upstream #1662's `merged_find_null_data_and_close_are_safe`
        // (`vfs_hooks.rs`, Windows-only): the live Linux path is `find_first_impl`
        // here. A NULL data pointer must fail with ERROR_INVALID_PARAMETER (87) +
        // INVALID_HANDLE_VALUE instead of dereferencing null. Pre-fix this test
        // faults the test process (SIGSEGV), which is the red signal.
        let _ = ensure_test_vfs();
        unsafe {
            let pat_w = crate::util::wide("G:\\data\\*");
            let pat_a = std::ffi::CString::new("G:\\data\\*").unwrap();

            SetLastError(0);
            assert_eq!(
                FindFirstFileW(pat_w.as_ptr(), std::ptr::null_mut()),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            SetLastError(0);
            assert_eq!(
                FindFirstFileA(
                    pat_a.as_ptr() as *const u8,
                    std::ptr::null_mut()
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            SetLastError(0);
            assert_eq!(
                FindFirstFileExW(
                    pat_w.as_ptr(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            SetLastError(0);
            assert_eq!(
                FindFirstFileExA(
                    pat_a.as_ptr() as *const u8,
                    0,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            // Invalid-handle Next/Close legs were already correct: pin them.
            let mut out: FindDataW = std::mem::zeroed();
            SetLastError(0);
            assert_eq!(FindNextFileW(usize::MAX - 7, &mut out), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            SetLastError(0);
            assert_eq!(FindNextFileA(usize::MAX - 7, &mut out), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            SetLastError(0);
            assert_eq!(FindClose(usize::MAX - 7), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            // A NULL Next on a merged (VFS-owned) handle also fails 87.
            let mut first: FindDataW = std::mem::zeroed();
            let fh = FindFirstFileW(pat_w.as_ptr(), &mut first);
            assert_ne!(fh, INVALID_HANDLE_VALUE);
            SetLastError(0);
            assert_eq!(FindNextFileW(fh, std::ptr::null_mut()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(FindClose(fh), 1);
        }
    }

    #[test]
    fn find_iteration_serializes_against_close() {
        // #1415-shape concurrency test: the Box-retaining intermediate shape from
        // upstream #1415 is superseded by #1662's HashMap design on both sides.
        // Linux already holds it (`FINDS: Mutex<HashMap<usize, FindState>>` at
        // kernelx_linux.rs:3521, no `Box::into_raw` on the Linux find path;
        // `FindNextFileW` keeps the `finds()` lock across lookup+increment and
        // `FindClose` removes under the same lock via `close_find_handle`).
        // This test proves the property: concurrent iteration delivers each
        // entry exactly once, and close-vs-reader never faults.
        let items: Vec<FindDataW> = (0..512u32)
            .map(|n| {
                let mut d: FindDataW = unsafe { std::mem::zeroed() };
                d.attrs = n;
                d
            })
            .collect();
        let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        finds().lock().unwrap().insert(h, FindState { items, pos: 0 });
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    let mut seen = Vec::new();
                    loop {
                        let mut d: FindDataW = unsafe { std::mem::zeroed() };
                        let r = unsafe { FindNextFileW(h, &mut d) };
                        if r == 0 {
                            break;
                        }
                        seen.push(d.attrs);
                    }
                    seen
                })
            })
            .collect();
        let mut all: Vec<u32> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (0..512u32).collect::<Vec<_>>());
        assert!(close_find_handle(h));
        // After close the handle is unknown: Next/Close fail 87, no fault.
        unsafe {
            let mut d: FindDataW = std::mem::zeroed();
            assert_eq!(FindNextFileW(h, &mut d), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(FindClose(h), 0);
        }

        for _ in 0..100 {
            let d: FindDataW = unsafe { std::mem::zeroed() };
            let h2 = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            finds()
                .lock()
                .unwrap()
                .insert(h2, FindState { items: vec![d], pos: 0 });
            let gate = Arc::new(Barrier::new(2));
            let other = Arc::clone(&gate);
            let reader = std::thread::spawn(move || {
                other.wait();
                let mut out: FindDataW = unsafe { std::mem::zeroed() };
                unsafe { FindNextFileW(h2, &mut out) }
            });
            gate.wait();
            assert!(close_find_handle(h2));
            let _ = reader.join().unwrap();
        }
    }

    #[test]
    fn find_first_on_mount_root_fails_like_native_drive_root() {
        // Linux port of upstream #761 (`find_first_on_mount_root_matches_native_drive_root`
        // in `vfs_hooks.rs`, Windows-only): a mount root has no entry of its own, so
        // `FindFirstFileW("G:\\")` fails with ERROR_FILE_NOT_FOUND just like native
        // `FindFirstFileW("C:\\")`. The upstream test probes a native drive via
        // `windows_sys`, which has no Linux meaning; this exercises the live Linux
        // path (`find_first_impl` -> `game_dir_entries`) on all four entries, and
        // pins that a root wildcard still lists the game contents.
        let _ = ensure_test_vfs();
        unsafe {
            let root_w = crate::util::wide("G:\\");
            let root_a = std::ffi::CString::new("G:\\").unwrap();
            let mut data: FindDataW = std::mem::zeroed();
            SetLastError(0);
            assert_eq!(
                FindFirstFileW(root_w.as_ptr(), &mut data),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            SetLastError(0);
            assert_eq!(
                FindFirstFileA(root_a.as_ptr() as *const u8, &mut data),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            SetLastError(0);
            assert_eq!(
                FindFirstFileExW(
                    root_w.as_ptr(),
                    0,
                    &mut data as *mut _ as *mut c_void,
                    0,
                    std::ptr::null(),
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);
            SetLastError(0);
            assert_eq!(
                FindFirstFileExA(
                    root_a.as_ptr() as *const u8,
                    0,
                    &mut data as *mut _ as *mut c_void,
                    0,
                    std::ptr::null(),
                    0
                ),
                INVALID_HANDLE_VALUE
            );
            assert_eq!(GetLastError(), ERROR_FILE_NOT_FOUND);

            // A root wildcard still lists the game folder contents.
            let star = crate::util::wide("G:\\*");
            let mut first: FindDataW = std::mem::zeroed();
            let fh = FindFirstFileW(star.as_ptr(), &mut first);
            assert_ne!(fh, INVALID_HANDLE_VALUE);
            let wname = |d: &FindDataW| {
                let n = d.name.iter().position(|&c| c == 0).unwrap_or(d.name.len());
                String::from_utf16_lossy(&d.name[..n])
            };
            let mut names = vec![wname(&first)];
            loop {
                let mut next: FindDataW = std::mem::zeroed();
                if FindNextFileW(fh, &mut next) == 0 {
                    break;
                }
                names.push(wname(&next));
            }
            assert_eq!(FindClose(fh), 1);
            assert!(
                names.iter().any(|n| n == "data"),
                "root wildcard must list game contents, got {names:?}"
            );
        }
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
    fn startup_info_and_narrow_cmdline_match_the_wide_line() {
        let _serial = CMDLINE_TEST_SERIAL.lock().unwrap();
        let prev = game_args();
        set_game_args(&[]);
        unsafe {
            // Narrow CRT line is the same text as the ANSI line.
            let a = GetCommandLineA();
            let narrow = _get_narrow_winmain_command_line();
            assert!(!narrow.is_null());
            assert_eq!(narrow, a);
            assert!(!crate::util::from_cstr_ptr(narrow).unwrap().is_empty());
            // Startup info: 104-byte struct, zeroed with real std handles.
            let mut si = std::mem::MaybeUninit::<StartupInfoW>::zeroed();
            GetStartupInfoW(si.as_mut_ptr());
            let si = si.assume_init();
            assert_eq!(si.cb, 104);
            assert_eq!(si.dw_flags, 0);
            assert_eq!(si.dw_x, 0);
            assert!(si.lp_desktop.is_null() && si.lp_title.is_null());
            assert_eq!(si.h_std_input, std_handle_value(STD_INPUT_HANDLE));
            assert_eq!(si.h_std_output, std_handle_value(STD_OUTPUT_HANDLE));
            assert_eq!(si.h_std_error, std_handle_value(STD_ERROR_HANDLE));
            // NULL is a no-op (never a fault), matching the stub's tolerance.
            GetStartupInfoW(std::ptr::null_mut());
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

    #[test]
    fn module_file_name_ex_and_base_name_answer_for_mapped_images() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        // psapi.dll routes to KERNELX, so the Ex/BaseName imports land on the twin.
        let reg = crate::hle::registry();
        assert_eq!(
            reg.hle_for("psapi.dll").map(|m| m.name),
            Some(crate::hle::KERNELX)
        );
        for name in [
            "GetModuleFileNameExW",
            "GetModuleFileNameExA",
            "K32GetModuleFileNameExW",
            "K32GetModuleFileNameExA",
            "GetModuleBaseNameW",
            "GetModuleBaseNameA",
            "K32GetModuleBaseNameW",
            "K32GetModuleBaseNameA",
        ] {
            assert!(reg.find_any(name).is_some(), "{name} must resolve");
        }

        let base: usize = 0x1_4000_0000;
        let fake_path = std::path::PathBuf::from("/xcl_test/xcl_modfile_test.dll");
        crate::modules::register_mapped(Arc::new(crate::modules::MappedModule {
            name: "xcl_modfile_test.dll".into(),
            path: fake_path.clone(),
            base,
            size: 0x10000,
            entry_rva: 0x1000,
            is_dll: true,
            exports: Default::default(),
            tls_callbacks: Mutex::new(Vec::new()),
            no_thread_calls: AtomicBool::new(false),
            initialized: AtomicBool::new(true),
            stack_reserve: 0,
        }));
        unsafe {
            let me = current_process_handle();
            let expected = fake_path.to_string_lossy().into_owned();

            // 1. Ex W + K32 alias on the mapped image.
            let mut buf_w = [0u16; 260];
            let len_w = K32GetModuleFileNameExW(me, base, buf_w.as_mut_ptr(), buf_w.len() as u32);
            assert!(len_w > 0);
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            assert_eq!(
                String::from_utf16_lossy(&buf_w[..len_w as usize]),
                expected
            );
            let mut buf_w2 = [0u16; 260];
            let len_w2 = GetModuleFileNameExW(me, base, buf_w2.as_mut_ptr(), buf_w2.len() as u32);
            assert_eq!(len_w, len_w2);
            assert_eq!(&buf_w[..len_w as usize], &buf_w2[..len_w2 as usize]);

            // 2. Ex A + K32 alias.
            let mut buf_a = [0u8; 260];
            let len_a = K32GetModuleFileNameExA(me, base, buf_a.as_mut_ptr(), buf_a.len() as u32);
            assert!(len_a > 0);
            assert_eq!(
                std::str::from_utf8(&buf_a[..len_a as usize]).unwrap(),
                expected
            );
            let mut buf_a2 = [0u8; 260];
            let len_a2 = GetModuleFileNameExA(me, base, buf_a2.as_mut_ptr(), buf_a2.len() as u32);
            assert_eq!(len_a, len_a2);
            assert_eq!(&buf_a[..len_a as usize], &buf_a2[..len_a2 as usize]);

            // 3. BaseName W + alias.
            let mut base_w = [0u16; 64];
            let blen_w = K32GetModuleBaseNameW(me, base, base_w.as_mut_ptr(), base_w.len() as u32);
            assert!(blen_w > 0);
            assert_eq!(
                String::from_utf16_lossy(&base_w[..blen_w as usize]),
                "xcl_modfile_test.dll"
            );
            let mut base_w2 = [0u16; 64];
            let blen_w2 = GetModuleBaseNameW(me, base, base_w2.as_mut_ptr(), base_w2.len() as u32);
            assert_eq!(blen_w, blen_w2);
            assert_eq!(&base_w[..blen_w as usize], &base_w2[..blen_w2 as usize]);

            // 4. BaseName A + alias.
            let mut base_a = [0u8; 64];
            let blen_a = K32GetModuleBaseNameA(me, base, base_a.as_mut_ptr(), base_a.len() as u32);
            assert!(blen_a > 0);
            assert_eq!(
                std::str::from_utf8(&base_a[..blen_a as usize]).unwrap(),
                "xcl_modfile_test.dll"
            );
            let mut base_a2 = [0u8; 64];
            let blen_a2 = GetModuleBaseNameA(me, base, base_a2.as_mut_ptr(), base_a2.len() as u32);
            assert_eq!(blen_a, blen_a2);
            assert_eq!(&base_a[..blen_a as usize], &base_a2[..blen_a2 as usize]);

            // 5. HLE pseudo-handles answer too.
            let hle = crate::modules::hle_handle("kernelx_ex_probe.dll");
            let mut hle_buf = [0u16; 260];
            let hle_len = K32GetModuleFileNameExW(me, hle, hle_buf.as_mut_ptr(), hle_buf.len() as u32);
            assert!(hle_len > 0);
            let mut hle_base = [0u16; 64];
            let hle_blen = K32GetModuleBaseNameW(me, hle, hle_base.as_mut_ptr(), hle_base.len() as u32);
            assert!(hle_blen > 0);
            assert_eq!(
                String::from_utf16_lossy(&hle_base[..hle_blen as usize]),
                "kernelx_ex_probe.dll"
            );

            // 6. Null buffer / zero size => ERROR_INVALID_PARAMETER.
            crate::util::set_last_error(0);
            assert_eq!(K32GetModuleFileNameExW(me, base, std::ptr::null_mut(), 260), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleFileNameExW(me, base, buf_w.as_mut_ptr(), 0),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleBaseNameW(me, base, std::ptr::null_mut(), 64),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleBaseNameW(me, base, base_w.as_mut_ptr(), 0),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            // 7. Foreign process + unknown module => ERROR_INVALID_HANDLE.
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleFileNameExW(0x1234, base, buf_w.as_mut_ptr(), 260),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleBaseNameW(0x1234, base, base_w.as_mut_ptr(), 64),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleFileNameExW(me, 0xDEAD_BEEF_1234, buf_w.as_mut_ptr(), 260),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
        }
        crate::modules::unregister_mapped(base);
    }

    #[test]
    fn module_information_answers_for_mapped_images() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        assert_eq!(
            std::mem::size_of::<ModuleInfo>(),
            24,
            "ModuleInfo must match Windows MODULEINFO (ptr,u32,pad,ptr)"
        );
        let reg = crate::hle::registry();
        assert!(reg.find_any("K32GetModuleInformation").is_some());
        assert!(reg.find_any("GetModuleInformation").is_some());

        let base: usize = 0x1_4100_0000;
        crate::modules::register_mapped(Arc::new(crate::modules::MappedModule {
            name: "xcl_modinfo_test.dll".into(),
            path: "/xcl_test/xcl_modinfo_test.dll".into(),
            base,
            size: 0x10000,
            entry_rva: 0x1230,
            is_dll: true,
            exports: Default::default(),
            tls_callbacks: Mutex::new(Vec::new()),
            no_thread_calls: AtomicBool::new(false),
            initialized: AtomicBool::new(true),
            stack_reserve: 0,
        }));
        unsafe {
            let me = current_process_handle();
            let cb = std::mem::size_of::<ModuleInfo>() as u32;
            let mut mi = ModuleInfo::default();
            assert_eq!(
                K32GetModuleInformation(me, base, &mut mi as *mut _ as *mut c_void, cb),
                1
            );
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            assert_eq!(mi.lpBaseOfDll as usize, base);
            assert_eq!(mi.SizeOfImage, 0x10000);
            assert_eq!(mi.EntryPoint as usize, base + 0x1230);

            // Alias answers the same.
            let mut mi2 = ModuleInfo::default();
            assert_eq!(
                GetModuleInformation(me, base, &mut mi2 as *mut _ as *mut c_void, cb),
                1
            );
            assert_eq!(mi2.lpBaseOfDll as usize, base);

            // Foreign process is not answered from the mapping.
            crate::util::set_last_error(0);
            let mut mi3 = ModuleInfo::default();
            assert_eq!(
                K32GetModuleInformation(0x1234, base, &mut mi3 as *mut _ as *mut c_void, cb),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);

            // Unknown module is not answered from the mapping.
            crate::util::set_last_error(0);
            let mut mi4 = ModuleInfo::default();
            assert_eq!(
                K32GetModuleInformation(me, 0xDEAD_BEEF_1234, &mut mi4 as *mut _ as *mut c_void, cb),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);

            // Too small a buffer + null info => ERROR_INSUFFICIENT_BUFFER.
            crate::util::set_last_error(0);
            let mut mi5 = ModuleInfo::default();
            assert_eq!(
                K32GetModuleInformation(me, base, &mut mi5 as *mut _ as *mut c_void, cb - 1),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            crate::util::set_last_error(0);
            assert_eq!(
                K32GetModuleInformation(me, base, std::ptr::null_mut(), cb),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
        }
        crate::modules::unregister_mapped(base);
    }

    #[test]
    fn format_message_system_string_allocate_and_truncate() {
        unsafe {
            // FROM_SYSTEM code message (Win32 2) ends like Windows with CRLF.
            let mut buf = [0u16; 128];
            let n = FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                std::ptr::null(),
                2,
                0,
                buf.as_mut_ptr(),
                128,
                std::ptr::null(),
            );
            assert!(n > 0);
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            let s = String::from_utf16_lossy(&buf[..n as usize]);
            assert!(s.contains("cannot find the file"), "{s}");
            assert!(s.ends_with("\r\n"), "{s:?}");
            // FROM_STRING with %1 inserts, argument-array of wide strings.
            let fmt = crate::util::wide("%1:%2!");
            let a1 = crate::util::wide("hello");
            let a2 = crate::util::wide("42");
            let arr = [a1.as_ptr() as usize, a2.as_ptr() as usize];
            let mut buf2 = [0u16; 64];
            let m = FormatMessageW(
                FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_ARGUMENT_ARRAY,
                fmt.as_ptr() as *const c_void,
                0,
                0,
                buf2.as_mut_ptr(),
                64,
                arr.as_ptr() as *const c_void,
            );
            assert!(m > 0);
            let s2 = String::from_utf16_lossy(&buf2[..m as usize]);
            assert_eq!(s2, "hello:42!");
            // Integer insert via !d!.
            let fmt3 = crate::util::wide("code %1!d!.");
            let arr3 = [87usize];
            let mut buf3 = [0u16; 64];
            let k = FormatMessageW(
                FORMAT_MESSAGE_FROM_STRING | FORMAT_MESSAGE_ARGUMENT_ARRAY,
                fmt3.as_ptr() as *const c_void,
                0,
                0,
                buf3.as_mut_ptr(),
                64,
                arr3.as_ptr() as *const c_void,
            );
            assert!(k > 0);
            let s3 = String::from_utf16_lossy(&buf3[..k as usize]);
            assert_eq!(s3, "code 87.");
            // ALLOCATE_BUFFER hands out a heap string the caller can read.
            let mut out: *mut u16 = std::ptr::null_mut();
            let a = FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM
                    | FORMAT_MESSAGE_IGNORE_INSERTS
                    | FORMAT_MESSAGE_ALLOCATE_BUFFER,
                std::ptr::null(),
                5,
                0,
                &mut out as *mut *mut u16 as *mut u16,
                0,
                std::ptr::null(),
            );
            assert!(a > 0);
            assert!(!out.is_null());
            let sa = String::from_utf16_lossy(std::slice::from_raw_parts(out, a as usize));
            assert!(sa.contains("denied"), "{sa}");
            assert_eq!(HeapFree(GetProcessHeap(), 0, out as *mut c_void), 1);
            // Truncation fails with ERROR_INSUFFICIENT_BUFFER.
            let mut tiny = [0u16; 4];
            assert_eq!(
                FormatMessageW(
                    FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                    std::ptr::null(),
                    2,
                    0,
                    tiny.as_mut_ptr(),
                    4,
                    std::ptr::null(),
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            // Unknown code falls back to text instead of failing.
            let mut ubuf = [0u16; 64];
            let u = FormatMessageW(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                std::ptr::null(),
                0x00BE_BEEF,
                0,
                ubuf.as_mut_ptr(),
                64,
                std::ptr::null(),
            );
            assert!(u > 0);
            let su = String::from_utf16_lossy(&ubuf[..u as usize]);
            assert!(su.contains("Unknown error"), "{su}");
            // NULL output without ALLOCATE_BUFFER is an error.
            assert_eq!(
                FormatMessageW(
                    FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                    std::ptr::null(),
                    2,
                    0,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // ANSI twin agrees on the same code.
            let mut abuf = [0u8; 128];
            let an = FormatMessageA(
                FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
                std::ptr::null(),
                2,
                0,
                abuf.as_mut_ptr(),
                128,
                std::ptr::null(),
            );
            assert!(an > 0);
            let sa2 = String::from_utf8_lossy(&abuf[..an as usize]);
            assert!(sa2.contains("cannot find the file"), "{sa2}");
        }
    }

    #[test]
    fn multibyte_and_widechar_convert_utf8_with_win32_contracts() {
        unsafe {
            // ASCII round-trips; query mode counts the terminator for -1.
            let src = b"D:\\LocalDumps\0";
            let need = MultiByteToWideChar(CP_UTF8, 0, src.as_ptr(), -1, std::ptr::null_mut(), 0);
            assert_eq!(need, 14); // 13 chars + NUL
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            let mut w = [0u16; 14];
            let n = MultiByteToWideChar(CP_UTF8, 0, src.as_ptr(), -1, w.as_mut_ptr(), 14);
            assert_eq!(n, 14);
            assert_eq!(w[13], 0);
            assert_eq!(String::from_utf16_lossy(&w[..13]), "D:\\LocalDumps");
            // The abort predicate sees an absolute path: first byte not NUL, contains ':'.
            assert_ne!(w[0], 0);
            // Explicit length: no implicit terminator.
            let need2 = MultiByteToWideChar(CP_UTF8, 0, src.as_ptr(), 2, std::ptr::null_mut(), 0);
            assert_eq!(need2, 2);
            let mut w2 = [0u16; 2];
            assert_eq!(MultiByteToWideChar(CP_UTF8, 0, src.as_ptr(), 2, w2.as_mut_ptr(), 2), 2);
            assert_eq!(String::from_utf16_lossy(&w2), "D:");
            // Multi-byte UTF-8 (emoji) becomes a surrogate pair.
            let snow = "a☃\0".as_bytes();
            let need3 = MultiByteToWideChar(CP_UTF8, 0, snow.as_ptr(), -1, std::ptr::null_mut(), 0);
            assert_eq!(need3, 3); // 'a' + U+2603 + NUL
            let mut w3 = [0u16; 3];
            assert_eq!(MultiByteToWideChar(CP_UTF8, 0, snow.as_ptr(), -1, w3.as_mut_ptr(), 3), 3);
            assert_eq!(w3[1], 0x2603);
            // CP_ACP is the same UTF-8 bytes on Linux.
            let mut wa = [0u16; 14];
            assert_eq!(MultiByteToWideChar(CP_ACP, 0, src.as_ptr(), -1, wa.as_mut_ptr(), 14), 14);
            assert_eq!(wa, w);
            // Invalid UTF-8: strict fails, lossy replaces with U+FFFD.
            let bad = [0x61u8, 0xFF, 0x00];
            assert_eq!(
                MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, bad.as_ptr(), -1, w.as_mut_ptr(), 14),
                0
            );
            assert_eq!(GetLastError(), ERROR_NO_UNICODE_TRANSLATION);
            let nl = MultiByteToWideChar(CP_UTF8, 0, bad.as_ptr(), -1, w.as_mut_ptr(), 14);
            assert_eq!(nl, 3); // 'a' + U+FFFD + NUL
            assert_eq!(w[1], 0xFFFD);
            // Small buffer fails without writing a partial result.
            let mut tiny = [0xBEEFu16; 4];
            assert_eq!(MultiByteToWideChar(CP_UTF8, 0, src.as_ptr(), -1, tiny.as_mut_ptr(), 4), 0);
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            // Bad code page / flags / pointers fail cleanly.
            assert_eq!(MultiByteToWideChar(CP_UTF7, 0, src.as_ptr(), -1, w.as_mut_ptr(), 14), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(MultiByteToWideChar(CP_UTF8, 0x40, src.as_ptr(), -1, w.as_mut_ptr(), 14), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_FLAGS);
            assert_eq!(MultiByteToWideChar(CP_UTF8, 0, std::ptr::null(), -1, w.as_mut_ptr(), 14), 0);
            assert_eq!(MultiByteToWideChar(CP_UTF8, 0, src.as_ptr(), 0, w.as_mut_ptr(), 14), 0);

            // Wide -> narrow mirrors the contract.
            let wsrc = crate::util::wide("D:\\LocalDumps");
            let mneed = WideCharToMultiByte(
                CP_UTF8, 0, wsrc.as_ptr(), -1, std::ptr::null_mut(), 0, std::ptr::null(), std::ptr::null_mut(),
            );
            assert_eq!(mneed, 14);
            let mut mb = [0u8; 14];
            let mn = WideCharToMultiByte(
                CP_UTF8, 0, wsrc.as_ptr(), -1, mb.as_mut_ptr(), 14, std::ptr::null(), std::ptr::null_mut(),
            );
            assert_eq!(mn, 14);
            assert_eq!(&mb[..13], b"D:\\LocalDumps");
            // Unpaired surrogate: strict fails, lossy maps to '?'.
            let lone = [0x44u16, 0xD800, 0x00];
            let mut mb2 = [0u8; 8];
            assert_eq!(
                WideCharToMultiByte(
                    CP_UTF8, WC_ERR_INVALID_CHARS, lone.as_ptr(), -1,
                    mb2.as_mut_ptr(), 8, std::ptr::null(), std::ptr::null_mut(),
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_NO_UNICODE_TRANSLATION);
            let ml = WideCharToMultiByte(
                CP_UTF8, 0, lone.as_ptr(), -1, mb2.as_mut_ptr(), 8, std::ptr::null(), std::ptr::null_mut(),
            );
            assert_eq!(ml, 3); // 'D' + '?' + NUL
            assert_eq!(&mb2[..3], b"D?\0");
            // UTF-8 targets must have null default-char slots.
            let dc = b"?\0";
            assert_eq!(
                WideCharToMultiByte(
                    CP_UTF8, 0, wsrc.as_ptr(), -1, mb.as_mut_ptr(), 14,
                    dc.as_ptr(), std::ptr::null_mut(),
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn fls_slots_are_per_thread_with_callbacks_on_exit() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        static LAST: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "win64" fn cb(v: usize) {
            LAST.store(v, Ordering::Release);
            CALLS.fetch_add(1, Ordering::AcqRel);
        }
        unsafe {
            let idx = FlsAlloc(cb as usize);
            assert_ne!(idx, FLS_OUT_OF_INDEXES);
            assert_eq!(FlsGetValue(idx), 0);
            assert_eq!(FlsSetValue(idx, 0x1234), 1);
            assert_eq!(FlsGetValue(idx), 0x1234);
            // A second thread sees its own (zero) value, not the creator's.
            let seen = crate::tls::spawn("fls-probe", move || FlsGetValue(idx)).unwrap().join().unwrap();
            assert_eq!(seen, 0);
            // Freeing runs the current thread's callback once.
            let before = CALLS.load(Ordering::Acquire);
            assert_eq!(FlsFree(idx), 1);
            assert_eq!(CALLS.load(Ordering::Acquire), before + 1);
            assert_eq!(LAST.load(Ordering::Acquire), 0x1234);
            // Double free and bad indexes fail cleanly, never abort.
            assert_eq!(FlsFree(idx), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(FlsGetValue(idx), 0);
            assert_eq!(FlsSetValue(idx, 1), 0);
            assert_eq!(FlsGetValue(999), 0);
            assert_eq!(FlsSetValue(999, 1), 0);
            // Exit path: a value left set on a dying thread fires its callback.
            CALLS.store(0, Ordering::Release);
            let idx2 = FlsAlloc(cb as usize);
            assert_ne!(idx2, FLS_OUT_OF_INDEXES);
            crate::tls::spawn("fls-exit", move || {
                FlsSetValue(idx2, 0xABCD);
                fls_thread_exit();
            })
            .unwrap()
            .join()
            .unwrap();
            assert_eq!(CALLS.load(Ordering::Acquire), 1);
            assert_eq!(LAST.load(Ordering::Acquire), 0xABCD);
            assert_eq!(FlsFree(idx2), 1);
        }
    }

    #[test]
    fn std_handles_write_to_the_host_fds() {
        unsafe {
            assert_eq!(GetStdHandle(STD_OUTPUT_HANDLE), std_handle_value(STD_OUTPUT_HANDLE));
            assert_eq!(GetStdHandle(STD_ERROR_HANDLE), std_handle_value(STD_ERROR_HANDLE));
            assert_eq!(GetStdHandle(STD_INPUT_HANDLE), std_handle_value(STD_INPUT_HANDLE));
            assert_eq!(GetStdHandle(0), 0);
            assert_eq!(SetStdHandle(STD_OUTPUT_HANDLE, std_handle_value(STD_OUTPUT_HANDLE)), 1);
            assert_eq!(SetStdHandle(STD_OUTPUT_HANDLE, 0x1234), 0);
            // Zero-length writes succeed and report 0 through both console handles.
            let mut done = 0xFFFF_FFFFu32;
            assert_eq!(
                WriteFile(
                    std_handle_value(STD_ERROR_HANDLE),
                    std::ptr::null(),
                    0,
                    &mut done,
                    std::ptr::null_mut()
                ),
                1
            );
            assert_eq!(done, 0);
            // Closing a std handle fails cleanly (the host fd stays open).
            assert_eq!(CloseHandle(std_handle_value(STD_OUTPUT_HANDLE)), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Fibers: never fibers on Linux; conversions fail cleanly.
            assert_eq!(IsThreadAFiber(), 0);
            assert_eq!(ConvertThreadToFiber(std::ptr::null()), 0);
            assert_eq!(ConvertFiberToThread(), 0);
            assert_eq!(CreateFiber(0, 0, std::ptr::null_mut()), 0);
            DeleteFiber(0);
            SwitchToFiber(0);
        }
    }

    #[test]
    fn module_handles_probe_without_loading() {
        unsafe {
            // NULL is the main image (or 0 when nothing is mapped in this test process).
            let _ = GetModuleHandleW(std::ptr::null());
            // HLE names hand out the stable pseudo handle.
            let a = GetModuleHandleW(crate::util::wide("kernelx.dll").as_ptr());
            assert_ne!(a, 0);
            assert_eq!(a, crate::modules::hle_handle("kernelx.dll"));
            let b = GetModuleHandleA(c"kernelx.dll".as_ptr() as *const u8);
            assert_eq!(b, a);
            // Unknown names miss without creating a handle.
            assert!(crate::modules::find_hle_handle("no_such_probe_xyz.dll").is_none());
            assert_eq!(GetModuleHandleW(crate::util::wide("no_such_probe_xyz.dll").as_ptr()), 0);
            assert_eq!(GetLastError(), ERROR_MOD_NOT_FOUND);
            assert!(crate::modules::find_hle_handle("no_such_probe_xyz.dll").is_none());
            // ExW without FROM_ADDRESS matches GetModuleHandle; FROM_ADDRESS resolves the
            // mapped image containing the address (or fails cleanly off-image).
            let mut out = 0usize;
            assert_eq!(
                GetModuleHandleExW(0, crate::util::wide("kernelx.dll").as_ptr(), &mut out),
                1
            );
            assert_eq!(out, a);
            let mut out2 = 0usize;
            assert_eq!(
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                    0x1234 as *const u16,
                    &mut out2
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_MOD_NOT_FOUND);
            assert!(GetModuleHandleExW(0, std::ptr::null(), std::ptr::null_mut()) == 0);
        }
    }

    #[test]
    fn module_handle_ex_rejects_invalid_flags() {
        unsafe {
            // Port of upstream #1235 flag-validation: unknown bits or PIN|UNCHANGED_REFCOUNT
            // fail with ERROR_INVALID_PARAMETER and leave *out null.
            const PIN: u32 = 0x1;
            const UNCHANGED_REFCOUNT: u32 = 0x2;
            let name = crate::util::wide("kernelx.dll");
            let mut out = 1usize;
            assert_eq!(
                GetModuleHandleExW(PIN | UNCHANGED_REFCOUNT, name.as_ptr(), &mut out),
                0
            );
            assert_eq!(out, 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let mut out2 = 1usize;
            assert_eq!(GetModuleHandleExW(0x8, name.as_ptr(), &mut out2), 0);
            assert_eq!(out2, 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Controls: default flags and FROM_ADDRESS plumbing still work.
            let mut ok = 0usize;
            assert_eq!(GetModuleHandleExW(0, name.as_ptr(), &mut ok), 1);
            assert_ne!(ok, 0);
            assert_eq!(GetLastError(), ERROR_SUCCESS);
        }
    }

    #[test]
    fn environment_variables_round_trip() {
        unsafe {
            std::env::set_var("XCL_FLS_PROBE_VAR", "hello-fls");
            let name = crate::util::wide("XCL_FLS_PROBE_VAR");
            // Size query (null buffer) returns the required size with NUL.
            let need = GetEnvironmentVariableW(name.as_ptr(), std::ptr::null_mut(), 0);
            assert_eq!(need, "hello-fls".len() as u32 + 1);
            let mut buf = [0u16; 64];
            let n = GetEnvironmentVariableW(name.as_ptr(), buf.as_mut_ptr(), 64);
            assert_eq!(n, "hello-fls".len() as u32);
            assert_eq!(
                String::from_utf16_lossy(&buf[..n as usize]),
                "hello-fls"
            );
            // Small buffer reports the required size with INSUFFICIENT_BUFFER.
            let mut tiny = [0u16; 4];
            let need2 = GetEnvironmentVariableW(name.as_ptr(), tiny.as_mut_ptr(), 4);
            assert_eq!(need2, "hello-fls".len() as u32 + 1);
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            // Missing names fail with ENVVAR_NOT_FOUND.
            let missing = crate::util::wide("XCL_DEFINITELY_MISSING_ENV_VAR_XYZ");
            assert_eq!(
                GetEnvironmentVariableW(missing.as_ptr(), buf.as_mut_ptr(), 64),
                0
            );
            assert_eq!(GetLastError(), ERROR_ENVVAR_NOT_FOUND);
            // ANSI form agrees.
            let aname = c"XCL_FLS_PROBE_VAR";
            let mut abuf = [0u8; 64];
            let m = GetEnvironmentVariableA(aname.as_ptr() as *const u8, abuf.as_mut_ptr(), 64);
            assert_eq!(m, "hello-fls".len() as u32);
            std::env::remove_var("XCL_FLS_PROBE_VAR");
        }
    }

    #[test]
    fn vectored_handlers_register_and_remove() {
        unsafe {
            // Null handlers fail cleanly, never hand out handle 0.
            assert_eq!(AddVectoredExceptionHandler(1, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let a = AddVectoredExceptionHandler(1, 0x1234);
            let b = AddVectoredExceptionHandler(0, 0x5678);
            assert_ne!(a, 0);
            assert_ne!(b, 0);
            assert_ne!(a, b);
            assert_eq!(RemoveVectoredExceptionHandler(a), 1);
            // Double remove and unknown handles fail cleanly.
            assert_eq!(RemoveVectoredExceptionHandler(a), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(RemoveVectoredExceptionHandler(0xDEAD_BEEF), 0);
            assert_eq!(RemoveVectoredExceptionHandler(b), 1);
            // Raising logs and returns (dispatch is later Phase C work).
            RaiseException(0xE06D_7363, 0, 0, std::ptr::null());
        }
    }

    #[test]
    fn get_proc_address_probes_hle_without_stubs() {
        unsafe {
            // Null names fail cleanly.
            assert_eq!(GetProcAddress(0x1234_5000, std::ptr::null()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // HLE pseudo handles resolve real HLE bodies, never stub thunks.
            let kx = crate::modules::hle_handle("kernelx.dll");
            let va = GetProcAddress(kx, c"VirtualAlloc".as_ptr() as *const u8);
            assert_ne!(va, 0);
            assert!(crate::stubs::info_of_stub(va).is_none());
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            // Unknown names miss with ERROR_PROC_NOT_FOUND so probing works.
            let miss = GetProcAddress(kx, c"XclDefinitelyMissingApi123".as_ptr() as *const u8);
            assert_eq!(miss, 0);
            assert_eq!(GetLastError(), ERROR_PROC_NOT_FOUND);
            // Unknown handles miss too.
            assert_eq!(
                GetProcAddress(0xDEAD_5000, c"VirtualAlloc".as_ptr() as *const u8),
                0
            );
            // Ordinal 0 (no ordinal exports here) misses cleanly.
            assert_eq!(GetProcAddress(kx, 0 as *const u8), 0);
        }
    }

    #[test]
    fn load_library_family_hands_out_known_handles() {
        unsafe {
            // HLE routes hand out the stable pseudo handle; A/W agree.
            let w = LoadLibraryW(crate::util::wide("kernelx.dll").as_ptr());
            assert_ne!(w, 0);
            assert_eq!(w, crate::modules::hle_handle("kernelx.dll"));
            assert_eq!(GetLastError(), ERROR_SUCCESS);
            let a = LoadLibraryA(c"kernelx.dll".as_ptr() as *const u8);
            assert_eq!(a, w);
            // Ex + packaged take the same path; flags are accepted, not rejected.
            assert_eq!(LoadLibraryExW(crate::util::wide("kernelx.dll").as_ptr(), 0, 0x22), w);
            assert_eq!(LoadLibraryExA(c"kernelx.dll".as_ptr() as *const u8, 0, 0), w);
            assert_eq!(LoadPackagedLibrary(crate::util::wide("kernelx.dll").as_ptr(), 0), w);
            // GetProcAddress through the loaded handle resolves HLE bodies.
            let va = GetProcAddress(w, c"VirtualAlloc".as_ptr() as *const u8);
            assert_ne!(va, 0);
            assert!(crate::stubs::info_of_stub(va).is_none());
            // NULL names fail cleanly; unknown DLLs miss with MOD_NOT_FOUND.
            assert_eq!(LoadLibraryW(std::ptr::null()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(LoadLibraryExW(std::ptr::null(), 0, 0), 0);
            assert_eq!(
                LoadLibraryW(crate::util::wide("xcl_definitely_missing_xyz.dll").as_ptr()),
                0
            );
            assert_eq!(GetLastError(), ERROR_MOD_NOT_FOUND);
            // FreeLibrary never unloads: known handles succeed, unknown fail.
            assert_eq!(FreeLibrary(w), 1);
            assert_eq!(FreeLibrary(0xDEAD_BEEF), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            FreeLibraryWhenCallbackReturns(0, w);
            // DisableThreadLibraryCalls records the opt-out on known handles.
            assert_eq!(DisableThreadLibraryCalls(w), 1);
            assert_eq!(DisableThreadLibraryCalls(0xDEAD_BEEF), 0);
        }
    }

    #[test]
    fn console_mode_follows_the_tty_and_stack_guarantee_holds() {
        unsafe {
            // Null mode buffers fail cleanly; unknown handles are not consoles.
            let mut mode = 0u32;
            assert_eq!(
                GetConsoleMode(std_handle_value(STD_ERROR_HANDLE), std::ptr::null_mut()),
                0
            );
            assert_eq!(GetConsoleMode(0x1234, &mut mode), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
            // Redirected std handles (this test's stderr is a pipe/file under cargo)
            // fail like Windows; on a real tty they succeed with a plausible mode.
            let r = GetConsoleMode(std_handle_value(STD_ERROR_HANDLE), &mut mode);
            if libc::isatty(2) == 1 {
                assert_eq!(r, 1);
                assert_ne!(mode, 0);
                assert_eq!(SetConsoleMode(std_handle_value(STD_ERROR_HANDLE), mode), 1);
            } else {
                assert_eq!(r, 0);
                assert_eq!(GetLastError(), ERROR_INVALID_HANDLE);
                assert_eq!(SetConsoleMode(std_handle_value(STD_ERROR_HANDLE), 7), 0);
            }
            assert_eq!(SetConsoleMode(0x1234, 7), 0);
            // UTF-8 so Rust takes the NtWriteFile path, not WriteConsoleW.
            assert_eq!(GetConsoleOutputCP(), 65001);
            // The guarantee call Rust's lang_start makes always succeeds.
            let mut size = 0x5000u32;
            assert_eq!(SetThreadStackGuarantee(&mut size), 1);
            assert_eq!(size, 0x5000);
            assert_eq!(SetThreadStackGuarantee(std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn nt_write_file_fills_the_io_status_block() {
        unsafe {
            let msg = b"xcl-ntwrite-probe";
            let mut iosb = IoStatusBlock { status: 0xDEAD, _pad: 0xBEEF, info: 0xCAFE };
            // Console stderr lands on host fd 2.
            let st = NtWriteFile(
                std_handle_value(STD_ERROR_HANDLE),
                0,
                0,
                0,
                &mut iosb,
                msg.as_ptr(),
                msg.len() as u32,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            assert_eq!(st, STATUS_SUCCESS);
            assert_eq!(iosb.status, STATUS_SUCCESS);
            assert_eq!(iosb.info, msg.len());
            // Zero-length writes succeed with zero information.
            let st = NtWriteFile(
                std_handle_value(STD_OUTPUT_HANDLE),
                0,
                0,
                0,
                &mut iosb,
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            assert_eq!(st, STATUS_SUCCESS);
            assert_eq!(iosb.info, 0);
            // Stdin and unknown handles are STATUS_INVALID_HANDLE, still filling the block.
            let st = NtWriteFile(
                std_handle_value(STD_INPUT_HANDLE),
                0,
                0,
                0,
                &mut iosb,
                msg.as_ptr(),
                msg.len() as u32,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            assert_eq!(st, STATUS_INVALID_HANDLE);
            assert_eq!(iosb.status, STATUS_INVALID_HANDLE);
            assert_eq!(iosb.info, 0);
            let st = NtWriteFile(
                0xDEAD_BEEF,
                0,
                0,
                0,
                &mut iosb,
                msg.as_ptr(),
                msg.len() as u32,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            assert_eq!(st, STATUS_INVALID_HANDLE);
        }
    }

    #[test]
    fn file_info_and_nt_read_match_read_file() {        unsafe {
            let _ = ensure_test_vfs();
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const OPEN_EXISTING: u32 = 3;
            const CREATE_ALWAYS: u32 = 2;
            // Seed a probe file under T: (title scratch, always writable).
            let name = format!("T:\\ntread_11l_{}.bin", std::process::id());
            let w = crate::util::wide(&name);
            let h = CreateFileW(
                w.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                CREATE_ALWAYS,
                0,
                0,
            );
            assert_ne!(h, INVALID_HANDLE_VALUE);
            let payload = b"hello-ntread-11l";
            let mut done = 0u32;
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
            // Reopen for reading: size, info, Ex, and NtReadFile agree.
            let h = open_w(&name, GENERIC_READ, OPEN_EXISTING);
            let mut len = 0u64;
            assert_eq!(GetFileSizeEx(h, &mut len), 1);
            assert_eq!(len, payload.len() as u64);
            let mut info: ByHandleFileInformation = std::mem::zeroed();
            assert_eq!(GetFileInformationByHandle(h, &mut info), 1);
            assert_eq!(
                ((info.size_high as u64) << 32 | info.size_low as u64),
                payload.len() as u64
            );
            assert_eq!(info.number_of_links, 1);
            let mut basic = [0u8; 40];
            assert_eq!(
                GetFileInformationByHandleEx(
                    h,
                    0,
                    basic.as_mut_ptr() as *mut c_void,
                    basic.len() as u32
                ),
                1
            );
            let mut standard = [0u8; 24];
            assert_eq!(
                GetFileInformationByHandleEx(
                    h,
                    1,
                    standard.as_mut_ptr() as *mut c_void,
                    standard.len() as u32
                ),
                1
            );
            let eof = i64::from_le_bytes(standard[8..16].try_into().unwrap());
            assert_eq!(eof, payload.len() as i64);
            // Small buffer fails cleanly.
            let mut tiny = [0u8; 8];
            assert_eq!(
                GetFileInformationByHandleEx(
                    h,
                    1,
                    tiny.as_mut_ptr() as *mut c_void,
                    tiny.len() as u32
                ),
                0
            );
            assert_eq!(GetLastError(), ERROR_INSUFFICIENT_BUFFER);
            // NtReadFile from the start matches ReadFile bytes.
            let mut buf = [0u8; 32];
            let mut iosb = IoStatusBlock { status: 0xDEAD, _pad: 0, info: 0 };
            let st = NtReadFile(
                h,
                0,
                0,
                0,
                &mut iosb,
                buf.as_mut_ptr(),
                payload.len() as u32,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            assert_eq!(st, STATUS_SUCCESS);
            assert_eq!(iosb.status, STATUS_SUCCESS);
            assert_eq!(iosb.info, payload.len());
            assert_eq!(&buf[..payload.len()], payload);
            // Explicit byte offset reads the tail.
            let off: i64 = 6;
            let mut tail = [0u8; 32];
            let mut iosb2 = IoStatusBlock { status: 0xDEAD, _pad: 0, info: 0 };
            let st = NtReadFile(
                h,
                0,
                0,
                0,
                &mut iosb2,
                tail.as_mut_ptr(),
                (payload.len() - 6) as u32,
                &off,
                std::ptr::null_mut(),
            );
            assert_eq!(st, STATUS_SUCCESS);
            assert_eq!(iosb2.info, payload.len() - 6);
            assert_eq!(&tail[..payload.len() - 6], &payload[6..]);
            // Unknown handles fail closed with STATUS_INVALID_HANDLE.
            let mut iosb3 = IoStatusBlock { status: 0, _pad: 0, info: 0xCAFE };
            assert_eq!(
                NtReadFile(
                    0xDEAD_BEEF,
                    0,
                    0,
                    0,
                    &mut iosb3,
                    buf.as_mut_ptr(),
                    4,
                    std::ptr::null(),
                    std::ptr::null_mut()
                ),
                STATUS_INVALID_HANDLE
            );
            assert_eq!(CloseHandle(h), 1);
            assert_eq!(GetFileInformationByHandle(0xDEAD_BEEF, &mut info), 0);
            // Status mapping covers the SUCCESS + INVALID_HANDLE round-trip.
            assert_eq!(RtlNtStatusToDosError(STATUS_SUCCESS), ERROR_SUCCESS);
            assert_eq!(RtlNtStatusToDosError(STATUS_INVALID_HANDLE), 6);
        }
    }

    #[test]
    fn get_file_time_reports_handle_timestamps() {
        unsafe {
            const GENERIC_READ: u32 = 0x8000_0000;
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const OPEN_EXISTING: u32 = 3;
            const CREATE_ALWAYS: u32 = 2;
            let _ = ensure_test_vfs();
            // Retail abort 0x2237320: the title calls GetFileTime(h,NULL,NULL,&ft)
            // from its 0x140F43800 wrapper; the write time must come from the
            // same VFS metadata the ByHandle helpers report. Seed a pid-unique
            // probe under T: (title scratch) like the NtReadFile test does, so
            // parallel tests never share-lock the same fixture.
            let name = format!("T:\\getfiletime_11m_{}.bin", std::process::id());
            let w = crate::util::wide(&name);
            let h = CreateFileW(
                w.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                CREATE_ALWAYS,
                0,
                0,
            );
            assert_ne!(h, INVALID_HANDLE_VALUE);
            assert_eq!(CloseHandle(h), 1);
            let h = open_w(&name, GENERIC_READ, OPEN_EXISTING);
            let mut c: FileTime = std::mem::zeroed();
            let mut a: FileTime = std::mem::zeroed();
            let mut w: FileTime = std::mem::zeroed();
            assert_eq!(GetFileTime(h, &mut c, &mut a, &mut w), 1);
            assert_ne!((w.high, w.low), (0, 0));
            // The game's wrapper pattern (creation/access NULL) reports the same write time.
            let mut w2: FileTime = std::mem::zeroed();
            assert_eq!(
                GetFileTime(h, std::ptr::null_mut(), std::ptr::null_mut(), &mut w2),
                1
            );
            assert_eq!((w2.high, w2.low), (w.high, w.low));
            assert_eq!(CloseHandle(h), 1);
            // Unknown handles fail closed, like the ByHandle neighbors.
            let mut w3: FileTime = std::mem::zeroed();
            assert_eq!(
                GetFileTime(0xDEAD_BEEF, std::ptr::null_mut(), std::ptr::null_mut(), &mut w3),
                0
            );
        }
    }

    #[test]
    fn current_directory_and_full_path_cover_console_paths() {
        unsafe {
            let _ = ensure_test_vfs();
            // Console-absolute paths pass through unchanged.
            for p in ["G:\\data\\hello.txt", "GAME:\\data\\sub\\nested.bin"] {
                let w = crate::util::wide(p);
                let mut buf = [0u16; 260];
                let n = GetFullPathNameW(w.as_ptr(), 260, buf.as_mut_ptr(), std::ptr::null_mut());
                assert_eq!(n as usize, p.encode_utf16().count(), "{p}");
                assert_eq!(String::from_utf16_lossy(&buf[..n as usize]), p);
                // Zero-length query returns the required size including NUL.
                let need = GetFullPathNameW(w.as_ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut());
                assert_eq!(need, n + 1);
                // Small buffers report the required size.
                let mut tiny = [0u16; 4];
                assert_eq!(
                    GetFullPathNameW(w.as_ptr(), 4, tiny.as_mut_ptr(), std::ptr::null_mut()),
                    need
                );
            }
            // Relative paths resolve against the game root as `G:\<rel>`.
            let rel = crate::util::wide("data\\hello.txt");
            let mut buf = [0u16; 260];
            let n = GetFullPathNameW(rel.as_ptr(), 260, buf.as_mut_ptr(), std::ptr::null_mut());
            assert!(n > 0);
            assert_eq!(
                String::from_utf16_lossy(&buf[..n as usize]),
                "G:\\data\\hello.txt"
            );
            // Current directory answers the game root.
            let mut cwd = [0u16; 260];
            let m = GetCurrentDirectoryW(260, cwd.as_mut_ptr());
            assert!(m > 0);
            assert!(!String::from_utf16_lossy(&cwd[..m as usize]).is_empty());
            assert_eq!(GetFullPathNameW(std::ptr::null(), 260, buf.as_mut_ptr(), std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn waitable_timers_fire_and_close() {
        unsafe {
            // Auto-reset (sync) timer: one-shot fires once, then disarms.
            let t = CreateWaitableTimerExW(std::ptr::null(), std::ptr::null(), 0, 0);
            assert_ne!(t, 0);
            // Null due fails cleanly.
            assert_eq!(SetWaitableTimer(t, std::ptr::null(), 0, 0, std::ptr::null_mut(), 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // Unknown handles fail cleanly too.
            assert_eq!(SetWaitableTimer(0xDEAD_BEEF, &0i64, 0, 0, std::ptr::null_mut(), 0), 0);
            assert_eq!(CancelWaitableTimer(0xDEAD_BEEF), 0);
            // 20 ms relative due: negative 100ns units.
            let due: i64 = -(20 * 10_000);
            assert_eq!(SetWaitableTimer(t, &due, 0, 0, std::ptr::null_mut(), 0), 1);
            assert_eq!(WaitForSingleObject(t, 5000), WAIT_OBJECT_0);
            // Auto-reset consumed the signal.
            assert_eq!(WaitForSingleObject(t, 0), WAIT_TIMEOUT);
            // Zero due fires immediately.
            let zero: i64 = 0;
            assert_eq!(SetWaitableTimer(t, &zero, 0, 0, std::ptr::null_mut(), 0), 1);
            assert_eq!(WaitForSingleObject(t, 1000), WAIT_OBJECT_0);
            // Cancel disarms: a 200 ms timer cancelled now never fires.
            let far: i64 = -(200 * 10_000);
            assert_eq!(SetWaitableTimer(t, &far, 0, 0, std::ptr::null_mut(), 0), 1);
            assert_eq!(CancelWaitableTimer(t), 1);
            assert_eq!(WaitForSingleObject(t, 50), WAIT_TIMEOUT);
            // Periodic timer fires repeatedly (manual-reset stays signaled).
            let m = CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_MANUAL_RESET,
                0,
            );
            assert_ne!(m, 0);
            let soon: i64 = -(10 * 10_000);
            assert_eq!(SetWaitableTimer(m, &soon, 10, 0, std::ptr::null_mut(), 0), 1);
            assert_eq!(WaitForSingleObject(m, 2000), WAIT_OBJECT_0);
            // Manual-reset stays signaled across waits.
            assert_eq!(WaitForSingleObject(m, 0), WAIT_OBJECT_0);
            // Legacy W spelling agrees.
            let w = CreateWaitableTimerW(std::ptr::null(), 0, std::ptr::null());
            assert_ne!(w, 0);
            assert_eq!(SetWaitableTimer(w, &zero, 0, 0, std::ptr::null_mut(), 0), 1);
            assert_eq!(WaitForSingleObject(w, 1000), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(t), 1);
            assert_eq!(CloseHandle(m), 1);
            assert_eq!(CloseHandle(w), 1);
            // Closed timers are gone from every wait path.
            assert_eq!(WaitForSingleObject(t, 0), WAIT_FAILED);
            assert_eq!(CancelWaitableTimer(t), 0);
        }
    }

    #[test]
    fn semaphores_create_wait_release_and_close() {
        unsafe {
            // Bad counts fail with NULL, never a handle.
            assert_eq!(CreateSemaphoreExW(std::ptr::null(), 0, 0, std::ptr::null(), 0, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(CreateSemaphoreExW(std::ptr::null(), 2, 1, std::ptr::null(), 0, 0), 0);
            assert_eq!(CreateSemaphoreExW(std::ptr::null(), -1, 1, std::ptr::null(), 0, 0), 0);
            // Zero initial: first wait times out, release wakes the next.
            let s = CreateSemaphoreExW(std::ptr::null(), 0, 2, std::ptr::null(), 0, 0);
            assert_ne!(s, 0);
            assert_eq!(WaitForSingleObject(s, 0), WAIT_TIMEOUT);
            let mut prev = -1i32;
            assert_eq!(ReleaseSemaphore(s, 1, &mut prev), 1);
            assert_eq!(prev, 0);
            assert_eq!(WaitForSingleObject(s, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(s, 0), WAIT_TIMEOUT);
            // Release of 2 wakes two counts; over-release fails without changing the count.
            assert_eq!(ReleaseSemaphore(s, 2, &mut prev), 1);
            assert_eq!(prev, 0);
            assert_eq!(ReleaseSemaphore(s, 1, std::ptr::null_mut()), 0);
            assert_eq!(GetLastError(), ERROR_TOO_MANY_POSTS);
            assert_eq!(WaitForSingleObject(s, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(s, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(s, 0), WAIT_TIMEOUT);
            // A releaser thread wakes a 5 s wait.
            let w = CreateSemaphoreW(std::ptr::null(), 0, 1, std::ptr::null());
            let setter = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                ReleaseSemaphore(w, 1, std::ptr::null_mut())
            });
            assert_eq!(WaitForSingleObject(w, 5000), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(w), 1);
            assert_eq!(CloseHandle(s), 1);
            // Multi-object: the semaphore wakes a wait-any alongside an event.
            let both = CreateSemaphoreExW(std::ptr::null(), 1, 1, std::ptr::null(), 0, 0);
            let ev = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            let pair = [both, ev];
            assert_eq!(WaitForMultipleObjects(2, pair.as_ptr(), 0, 0), WAIT_OBJECT_0);
            // Consumed: the event (still clear) is next only after a release.
            assert_eq!(ReleaseSemaphore(both, 1, std::ptr::null_mut()), 1);
            assert_eq!(WaitForMultipleObjects(2, pair.as_ptr(), 0, 0), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(both), 1);
            assert_eq!(CloseHandle(ev), 1);
            // Unknown handles fail instead of blocking forever.
            assert_eq!(ReleaseSemaphore(0xDEAD_BEEF, 1, std::ptr::null_mut()), 0);
            assert_eq!(WaitForSingleObject(0xDEAD_BEEF, 0), WAIT_FAILED);
            setter.join().unwrap();
            // Legacy W/A spellings agree.
            let a = CreateSemaphoreA(std::ptr::null(), 1, 1, std::ptr::null());
            assert_ne!(a, 0);
            assert_eq!(WaitForSingleObject(a, 0), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(a), 1);
            let e = CreateSemaphoreExA(std::ptr::null(), 0, 1, std::ptr::null(), 0, 0);
            assert_ne!(e, 0);
            assert_eq!(WaitForSingleObject(e, 10), WAIT_TIMEOUT);
            assert_eq!(CloseHandle(e), 1);
            // Closed semaphores are gone from every wait path.
            assert_eq!(WaitForSingleObject(both, 0), WAIT_FAILED);
            assert_eq!(ReleaseSemaphore(both, 1, std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn condition_variables_sleep_and_wake() {
        unsafe {
            // Nulls and an unowned CS fail cleanly, never block.
            let mut cv_slot = 0usize;
            let cv = &mut cv_slot as *mut usize;
            InitializeConditionVariable(cv);
            assert_eq!(cv_slot, 0);
            assert_eq!(SleepConditionVariableCS(std::ptr::null_mut(), std::ptr::null_mut(), 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let cs = Box::into_raw(Box::new(std::mem::zeroed::<CritSec>()));
            assert_eq!(InitializeCriticalSection(cs), 1);
            assert_eq!(SleepConditionVariableCS(cv, cs, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // A short timeout releases, lapses, and reacquires (still owned once).
            EnterCriticalSection(cs);
            assert_eq!(SleepConditionVariableCS(cv, cs, 20), 0);
            assert_eq!(GetLastError(), ERROR_TIMEOUT);
            LeaveCriticalSection(cs);
            assert_eq!(TryEnterCriticalSection(cs), 1);
            LeaveCriticalSection(cs);
            // A waiter blocked in Sleep wakes on WakeAll and leaves owning nothing.
            let cs_addr = cs as usize;
            let cv_addr = cv as usize;
            let woken = Arc::new(AtomicU32::new(0));
            let h = std::thread::spawn({
                let woken = woken.clone();
                move || unsafe {
                    let cs = cs_addr as *mut CritSec;
                    let cv = cv_addr as *mut usize;
                    EnterCriticalSection(cs);
                    let r = SleepConditionVariableCS(cv, cs, 5000);
                    if r == 1 {
                        woken.fetch_add(1, Ordering::SeqCst);
                    }
                    LeaveCriticalSection(cs);
                }
            });
            std::thread::sleep(Duration::from_millis(100));
            WakeAllConditionVariable(cv);
            h.join().unwrap();
            assert_eq!(woken.load(Ordering::SeqCst), 1);
            assert_eq!(TryEnterCriticalSection(cs), 1);
            LeaveCriticalSection(cs);
            // WakeSingle wakes exactly one of two waiters; the other times out.
            let mut cv2_slot = 0usize;
            let cv2 = &mut cv2_slot as *mut usize;
            InitializeConditionVariable(cv2);
            let cv2_addr = cv2 as usize;
            let r1 = Arc::new(AtomicU32::new(2));
            let r2 = Arc::new(AtomicU32::new(2));
            let waiter = |out: Arc<AtomicU32>| {
                std::thread::spawn(move || unsafe {
                    let cs = cs_addr as *mut CritSec;
                    let cv = cv2_addr as *mut usize;
                    EnterCriticalSection(cs);
                    let r = SleepConditionVariableCS(cv, cs, 800);
                    out.store(r as u32, Ordering::SeqCst);
                    LeaveCriticalSection(cs);
                })
            };
            let h1 = waiter(r1.clone());
            let h2 = waiter(r2.clone());
            std::thread::sleep(Duration::from_millis(150));
            WakeConditionVariable(cv2);
            h1.join().unwrap();
            h2.join().unwrap();
            let mut got = [r1.load(Ordering::SeqCst), r2.load(Ordering::SeqCst)];
            got.sort_unstable();
            assert_eq!(got, [0, 1]);
            // Wakes with no waiters (and nulls) are lost silently.
            WakeConditionVariable(cv2);
            WakeAllConditionVariable(cv2);
            WakeConditionVariable(std::ptr::null_mut());
            WakeAllConditionVariable(std::ptr::null_mut());
            assert_eq!(DeleteCriticalSection(cs), 1);
            let _ = Box::from_raw(cs);
        }
    }

    #[test]
    fn wait_on_address_wakes_and_times_out() {
        unsafe {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicU32, Ordering};
            // Shared cells: the address (usize) is Send; waiters read the bytes there.
            let cell = Arc::new(AtomicU32::new(0));
            let cell_addr = &*cell as *const AtomicU32 as usize;
            static WANT: u32 = 0;
            let want_addr = &WANT as *const u32 as usize;
            let want_ptr = want_addr as *const c_void;
            // Mismatched values return immediately without waiting.
            cell.store(1, Ordering::SeqCst);
            assert_eq!(WaitOnAddress(cell_addr as *const c_void, want_ptr, 4, 1000), 0);
            cell.store(0, Ordering::SeqCst);
            // A waker thread changes the value and wakes the waiter.
            let cell2 = cell.clone();
            let waker = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                cell2.store(7, Ordering::SeqCst);
                WakeByAddressAll(cell_addr as *const c_void);
            });
            assert_eq!(WaitOnAddress(cell_addr as *const c_void, want_ptr, 4, 5000), 1);
            waker.join().unwrap();
            // No change + short timeout: FALSE with ERROR_TIMEOUT.
            cell.store(0, Ordering::SeqCst);
            assert_eq!(WaitOnAddress(cell_addr as *const c_void, want_ptr, 4, 20), 0);
            assert_eq!(GetLastError(), ERROR_TIMEOUT);
            // WakeSingle wakes exactly one waiter.
            cell.store(0, Ordering::SeqCst);
            let h1 = std::thread::spawn(move || {
                WaitOnAddress(cell_addr as *const c_void, want_addr as *const c_void, 4, 5000)
            });
            // Second waiter on its own cell so the two waits do not share an address.
            let cell_b = Arc::new(AtomicU32::new(0));
            let cell_b_addr = &*cell_b as *const AtomicU32 as usize;
            let h2 = std::thread::spawn(move || {
                WaitOnAddress(cell_b_addr as *const c_void, want_addr as *const c_void, 4, 200)
            });
            std::thread::sleep(Duration::from_millis(30));
            WakeByAddressSingle(cell_addr as *const c_void);
            assert_eq!(h1.join().unwrap(), 1);
            // The waiter on the other address was not woken: it times out.
            assert_eq!(h2.join().unwrap(), 0);
            // Bad sizes/NULLs fail cleanly.
            assert_eq!(WaitOnAddress(std::ptr::null(), want_ptr, 4, 0), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(WaitOnAddress(cell_addr as *const c_void, want_ptr, 0, 0), 0);
            WakeByAddressSingle(std::ptr::null());
            WakeByAddressAll(std::ptr::null());
        }
    }

    #[test]
    fn capture_context_writes_plausible_rip_rsp() {
        unsafe {
            let mut ctx = vec![0u8; CONTEXT_SIZE].into_boxed_slice();
            let ptr = ctx.as_mut_ptr();
            RtlCaptureContext(ptr);
            let rip = ctx_u64(ptr as *const u8, CTX_RIP).unwrap_or(0);
            let rsp = ctx_u64(ptr as *const u8, CTX_RSP).unwrap_or(0);
            assert_ne!(rip, 0);
            assert_ne!(rsp, 0);
            assert_eq!(rsp & 0x7, 0);
            // RIP should be near this test function (same host module).
            let here = capture_context_writes_plausible_rip_rsp as usize;
            assert!(
                rip.abs_diff(here as u64) < 32 * 1024 * 1024,
                "RIP {rip:#x} far from test {here:#x}"
            );
            assert_eq!(
                std::ptr::read_unaligned(ptr.add(CTX_FLAGS_OFF) as *const u32),
                CONTEXT_FULL_AMD64
            );
        }
    }

    #[test]
    fn handler_data_va_points_past_handler() {
        let info = crate::pdata::UnwindInfo {
            version: 1,
            flags: crate::pdata::UNW_FLAG_EHANDLER,
            prolog_size: 4,
            frame_reg: 0,
            frame_offset_scaled: 0,
            codes: vec![crate::pdata::UnwindOp::AllocSmall { offset: 4, size: 8 }],
            handler_rva: Some(0x500),
            slots: 1,
        };
        let func = crate::pdata::RuntimeFunction {
            begin_rva: 0x1000,
            end_rva: 0x1100,
            unwind_rva: 0x2000,
        };
        // slots=1 -> data_slot=0? (1+1)&!1 = 0? Actually (1+1)&!1 = 2&!1 = 2.
        // data_off = 4 + 2*2 = 8; handler data = base + 0x2000 + 8 + 4.
        let va = handler_data_va(0x140_0000_0000, &func, &info);
        assert_eq!(va, 0x140_0000_0000 + 0x2000 + 8 + 4);
        // No handler -> 0.
        let mut no_handler = info.clone();
        no_handler.handler_rva = None;
        assert_eq!(handler_data_va(0x140_0000_0000, &func, &no_handler), 0);
    }

    #[test]
    fn veh_continue_search_does_not_claim() {
        // Empty VEH chain never claims ContinueExecution.
        veh().lock().unwrap().clear();
        let mut rec = ExceptionRecord {
            code: 0x2047_4343,
            flags: 0,
            record: 0,
            addr: 0,
            num_params: 0,
            _pad: 0,
            info: [0; 15],
        };
        let mut ctx = vec![0u8; CONTEXT_SIZE].into_boxed_slice();
        assert!(!call_veh_chain(
            &mut rec as *mut _,
            ctx.as_mut_ptr()
        ));
    }

    #[test]
    fn exit_process_ends_the_child_with_its_code() {
        // ExitProcess never returns, so it runs in an isolated child.
        const CHILD: &str = "XCL_KERNELX_EXIT_CHILD";
        const CODE: u32 = 0x2a;
        if std::env::var_os(CHILD).is_some() {
            unsafe { ExitProcess(CODE) };
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("hle::kernelx_linux::tests::exit_process_ends_the_child_with_its_code")
            .arg("--nocapture")
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(CODE as i32),
            "child must exit with the title code: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn terminate_process_rejects_foreign_handles() {
        unsafe {
            assert_eq!(TerminateProcess(0x1234_0000, 7), 0);
            assert_eq!(get_error(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn memstat_notes_each_title_commit_kind() {
        // Port of upstream #194's counters: every commit path notes its kind (gross,
        // frees not taken off). Growth assertions are lower bounds: other tests may
        // note concurrently on other threads.
        use crate::memstat::{GRAPHICS, HEAP, PHYSICAL, WINDOWS};
        unsafe {
            let w0 = crate::memstat::gross(WINDOWS);
            let p = VirtualAlloc(std::ptr::null(), 0x2000, 0x3000, 4);
            assert!(!p.is_null());
            assert!(crate::memstat::gross(WINDOWS) >= w0 + 0x2000, "VirtualAlloc commit notes WINDOWS");
            assert_eq!(VirtualFree(p, 0, 0x8000), 1);
            // A reserve without commit notes nothing (nothing is committed).
            let w1 = crate::memstat::gross(WINDOWS);
            let r = VirtualAlloc(std::ptr::null(), 0x2000, 0x2000, 4);
            assert!(!r.is_null());
            assert_eq!(crate::memstat::gross(WINDOWS), w1, "reserve-only notes nothing");
            assert_eq!(VirtualFree(r, 0, 0x8000), 1);

            let h0 = crate::memstat::gross(HEAP);
            let attrs = 8u64 << 24;
            let h = XMemAlloc(1000, attrs);
            assert!(!h.is_null());
            assert!(crate::memstat::gross(HEAP) >= h0 + 1000, "heap alloc notes HEAP");
            XMemFree(h, attrs);

            let g0 = crate::memstat::gross(GRAPHICS);
            let g = XMemAlloc(0x1_0000, 1u64 << 29);
            assert!(!g.is_null());
            assert!(crate::memstat::gross(GRAPHICS) >= g0 + 0x1_0000, "graphics alloc notes GRAPHICS");
            XMemFree(g, 1u64 << 29);

            let p0 = crate::memstat::gross(PHYSICAL);
            let (mut n, mut pages) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            assert!(crate::memstat::gross(PHYSICAL) >= p0 + 2 * 0x1_0000, "page homes note PHYSICAL");
            assert_eq!(XMemFreePhysicalPages(2, pages.as_mut_ptr()), 1);
        }
    }

    #[test]
    fn xmem_heap_graphics_and_physical_pages_behave() {
        // Mirrors the test-title checks: heap alignment + zeroing, graphics high-VA
        // with merge-reuse, XMemVirtualAlloc reserve/commit, and physical-page
        // map/move/unmap roundtrips. One test (no parallelism inside the XMem
        // slice) so the pool's first-fit addresses are deterministic.
        unsafe {
            // Heap: 256-byte alignment, zeroed.
            let attrs = 8u64 << 24;
            let p = XMemAlloc(1000, attrs);
            assert!(!p.is_null());
            assert_eq!(p as usize % 256, 0);
            assert!(std::slice::from_raw_parts(p as *const u8, 1000).iter().all(|&b| b == 0));
            XMemFree(p, attrs);

            // Graphics: high-VA, 64 KiB aligned, contiguous, reusable.
            let g_attrs = 1u64 << 29;
            let a = XMemAlloc(0x20000, g_attrs);
            let b = XMemAlloc(0x10000, g_attrs);
            assert!(!a.is_null() && !b.is_null());
            assert!((a as usize) >> 32 >= 4 && (b as usize) >> 32 >= 4);
            assert_eq!((a as usize) & 0xFFFF, 0);
            assert_eq!(b as usize, a as usize + 0x20000);
            *(b as *mut u64) = 0x1122_3344;
            assert_eq!(*(b as *const u64), 0x1122_3344);
            XMemFree(b, g_attrs);
            XMemFree(a, g_attrs);
            let c = XMemAlloc(0x30000, g_attrs);
            assert_eq!(c, a);
            assert!(std::slice::from_raw_parts(c as *const u8, 0x30000).iter().all(|&x| x == 0));
            XMemFree(c, g_attrs);
            // 4 MiB pages: 4 MiB aligned + zeroed.
            let big = XMemAlloc(0x1000, (1u64 << 29) | (2u64 << 14));
            assert!(!big.is_null() && big as usize % (4 << 20) == 0);
            assert_eq!(*(big as *const u64), 0);
            XMemFree(big, (1u64 << 29) | (2u64 << 14));

            // XMemVirtualAlloc: graphics commits high, plain commits low, both free.
            let (commit, rw) = (0x3000u32, 4u32);
            let g = XMemVirtualAlloc(std::ptr::null(), 0x1_0000, commit, 1u64 << 29, rw);
            let q = XMemVirtualAlloc(std::ptr::null(), 0x1_0000, commit, 0, rw);
            assert!(!g.is_null() && !q.is_null());
            assert!((g as usize) >> 32 >= 4);
            *(g as *mut u32) = 7;
            *(q as *mut u32) = 7;
            assert_eq!(VirtualFree(g, 0, 0x8000), 1);
            assert_eq!(VirtualFree(q, 0, 0x8000), 1);

            // Physical pages: allocate 4, map fresh, write second page, free.
            let page = 0x1_0000usize;
            let (mut n, mut pages) = (4usize, [0u64; 4]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            assert_eq!(n, 4);
            let mp = XMemMapPhysicalPages(std::ptr::null_mut(), 4, pages.as_mut_ptr()) as *mut u32;
            assert!(!mp.is_null() && mp as usize >> 32 >= 4);
            *mp.add(page / 4) = 7;
            assert_eq!(XMemFreePhysicalPages(4, pages.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(mp as *mut c_void, 0, 0x8000), 1);

            // Reserved VA keeps page contents across move/unmap/remap.
            let va = XMemVirtualAlloc(std::ptr::null(), 4 * page, 0x2000, 1u64 << 29, rw) as usize;
            assert_ne!(va, 0);
            assert_eq!(va % page, 0);
            let (mut m, mut pp) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut m, pp.as_mut_ptr()), 1);
            let mut ba = [pp[1], pp[0]];
            assert_eq!(
                XMemMapPhysicalPages((va + page) as *mut c_void, 2, ba.as_mut_ptr() as *mut u64)
                    as usize,
                va + page
            );
            *((va + page) as *mut u32) = 0xB;
            *((va + 2 * page) as *mut u32) = 0xA;
            let mut one = [pp[0]];
            assert_eq!(
                XMemMapPhysicalPages(va as *mut c_void, 1, one.as_mut_ptr() as *mut u64) as usize,
                va
            );
            assert_eq!(*(va as *const u32), 0xA);
            assert_eq!(
                XMemMapPhysicalPages(va as *mut c_void, 1, std::ptr::null_mut()) as usize,
                va
            );
            assert_eq!(
                XMemMapPhysicalPages((va + 3 * page) as *mut c_void, 1, one.as_mut_ptr() as *mut u64)
                    as usize,
                va + 3 * page
            );
            assert_eq!(*((va + 3 * page) as *const u32), 0xA);
            assert_eq!(*((va + page) as *const u32), 0xB);
            assert_eq!(XMemFreePhysicalPages(2, pp.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(va as *mut c_void, 0, 0x8000), 1);
        }
    }

    #[test]
    fn contiguous_home_requires_one_allocation() {
        let mut blocks = std::collections::BTreeMap::new();
        blocks.insert(10, PhysicalBlock::new(3, 0x1_0000_0000));
        // separate allocation whose page numbers immediately follow, with a non-adjacent home
        blocks.insert(13, PhysicalBlock::new(2, 0x2_0000_0000));
        // separate allocation whose home happens to be adjacent (still a distinct reservation)
        blocks.insert(15, PhysicalBlock::new(1, 0x2_0002_0000));

        let p = |i| page_number(i);
        assert_eq!(contiguous_home_in(&blocks, &[p(10), p(11), p(12)]), Some(0x1_0000_0000));
        assert_eq!(contiguous_home_in(&blocks, &[p(11), p(12)]), Some(0x1_0000_0000 + PHYS_PAGE));
        assert_eq!(contiguous_home_in(&blocks, &[p(13), p(14)]), Some(0x2_0000_0000));
        // spanning two allocations: refused, whether or not the homes are adjacent
        assert_eq!(contiguous_home_in(&blocks, &[p(12), p(13)]), None);
        assert_eq!(contiguous_home_in(&blocks, &[p(14), p(15)]), None);
        // non-consecutive, zero, empty, or overflow
        assert_eq!(contiguous_home_in(&blocks, &[p(10), p(12)]), None);
        assert_eq!(contiguous_home_in(&blocks, &[0, p(10)]), None);
        assert_eq!(contiguous_home_in(&blocks, &[]), None);
        assert_eq!(contiguous_home_in(&blocks, &[usize::MAX, 0]), None);
    }

    #[test]
    fn cross_allocation_pages_map_split_and_unmap_preserves_neighbors() {
        // (a) consecutive pages of one allocation map as one block: the NULL-address
        // path returns a single contiguous span holding every page's contents.
        // (b) consecutive page numbers from two separate allocations never merge into
        // one block over unrelated memory: each page maps with its own contents.
        // (c) unmapping or freeing one address preserves a neighbor's mapping.
        // Only own pages and VAs are asserted, so other tests may run alongside.
        unsafe {
            let page = 0x1_0000usize;
            let (mut n, mut pages) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            assert_eq!(contiguous_home(&[pages[0] as usize, pages[1] as usize]), page_home(pages[0] as usize));
            let mp = XMemMapPhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()) as *mut u32;
            assert!(!mp.is_null());
            *mp.add(page / 4 * 0) = 0xA0;
            *mp.add(page / 4 * 1) = 0xA1;
            assert_eq!(XMemMapPhysicalPages(mp as *mut c_void, 2, std::ptr::null_mut()) as usize, mp as usize);
            let mp2 = XMemMapPhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()) as *mut u32;
            assert!(!mp2.is_null());
            assert_eq!(*mp2.add(page / 4 * 0), 0xA0);
            assert_eq!(*mp2.add(page / 4 * 1), 0xA1);
            assert_eq!(XMemFreePhysicalPages(2, pages.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(mp as *mut c_void, 0, 0x8000), 1);
            assert_eq!(VirtualFree(mp2 as *mut c_void, 0, 0x8000), 1);

            // Two back-to-back allocations; their numbers are consecutive unless another
            // test allocated between the two calls (both layouts are asserted correctly).
            let (mut na, mut pa) = (2usize, [0u64; 2]);
            let (mut nb, mut pb) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut na, pa.as_mut_ptr()), 1);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut nb, pb.as_mut_ptr()), 1);
            let all = [pa[0] as usize, pa[1] as usize, pb[0] as usize, pb[1] as usize];
            let consecutive =
                all[1] == all[0] + FRAMES && all[2] == all[1] + FRAMES && all[3] == all[2] + FRAMES;
            if consecutive {
                assert_eq!(contiguous_home(&all), None, "two allocations never share one home");
            }
            assert!(write_physical_pages(&[all[0]], 0, &0xB0u32.to_ne_bytes()));
            assert!(write_physical_pages(&[all[1]], 0, &0xB1u32.to_ne_bytes()));
            assert!(write_physical_pages(&[all[2]], 0, &0xB2u32.to_ne_bytes()));
            assert!(write_physical_pages(&[all[3]], 0, &0xB3u32.to_ne_bytes()));
            let mut quad = [pa[0], pa[1], pb[0], pb[1]];
            let mq = XMemMapPhysicalPages(std::ptr::null_mut(), 4, quad.as_mut_ptr()) as *mut u32;
            assert!(!mq.is_null());
            for i in 0..4 {
                assert_eq!(*mq.add(page / 4 * i), 0xB0 + i as u32, "page {i} keeps its own contents");
            }
            assert_eq!(XMemFreePhysicalPages(2, pa.as_mut_ptr()), 1);
            assert_eq!(XMemFreePhysicalPages(2, pb.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(mq as *mut c_void, 0, 0x8000), 1);

            // Neighbor mappings survive unmap and free of an adjacent address.
            let (mut n1, mut p1) = (1usize, [0u64; 1]);
            let (mut n2, mut p2) = (1usize, [0u64; 1]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n1, p1.as_mut_ptr()), 1);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n2, p2.as_mut_ptr()), 1);
            let m1 = XMemMapPhysicalPages(std::ptr::null_mut(), 1, p1.as_mut_ptr()) as *mut u32;
            let m2 = XMemMapPhysicalPages(std::ptr::null_mut(), 1, p2.as_mut_ptr()) as *mut u32;
            assert!(!m1.is_null() && !m2.is_null());
            *m1 = 0xC1;
            *m2 = 0xC2;
            assert_eq!(XMemMapPhysicalPages(m1 as *mut c_void, 1, std::ptr::null_mut()) as usize, m1 as usize);
            assert_eq!(*m2, 0xC2, "unmapping one address leaves the neighbor mapped");
            let m1b = XMemMapPhysicalPages(std::ptr::null_mut(), 1, p1.as_mut_ptr()) as *mut u32;
            assert_eq!(*m1b, 0xC1, "unmapped data went back to its own home");
            assert_eq!(XMemFreePhysicalPages(1, p1.as_mut_ptr()), 1);
            assert_eq!(*m2, 0xC2, "freeing one page leaves the neighbor's mapping alone");
            let gone = XMemMapPhysicalPages(std::ptr::null_mut(), 1, p1.as_mut_ptr());
            assert!(gone.is_null(), "a freed page no longer resolves");
            assert_eq!(XMemMapPhysicalPages(m2 as *mut c_void, 1, std::ptr::null_mut()) as usize, m2 as usize);
            assert_eq!(XMemFreePhysicalPages(1, p2.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(m1 as *mut c_void, 0, 0x8000), 1);
            assert_eq!(VirtualFree(m2 as *mut c_void, 0, 0x8000), 1);
            assert_eq!(VirtualFree(m1b as *mut c_void, 0, 0x8000), 1);
        }
    }

    #[test]
    fn physical_pages_map_into_a_plain_reservation() {
        // `XMemVirtualAlloc` with memory_type 0 reserves ordinary `VirtualAlloc`
        // memory (the Windows twin does the same); `XMemMapPhysicalPages` must
        // accept that VA, not only graphics-pool ranges. Retail reserves a
        // 6 GiB stream window this way and int3s when the map is refused.
        unsafe {
            let (mut n, mut pages) = (1usize, [0u64; 1]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            assert!(write_physical_pages(&[pages[0] as usize], 0, &0x42u32.to_ne_bytes()));
            // mmap only guarantees 4 KiB; take a 64 KiB-aligned address inside
            // the reservation (the title's reservation is already aligned).
            let raw = VirtualAlloc(std::ptr::null(), 0x30000, 0x2000, 4) as usize;
            assert_ne!(raw, 0);
            let at = ((raw + 0xffff) & !0xffff) as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, 1, pages.as_mut_ptr()) as usize, at as usize);
            assert_eq!(*(at as *const u32), 0x42);
            // Unmap (null page) copies the contents home and decommits the VA.
            assert_eq!(XMemMapPhysicalPages(at, 1, std::ptr::null_mut()) as usize, at as usize);
            assert_eq!(VirtualFree(raw as *mut c_void, 0, 0x8000), 1);
            assert_eq!(XMemFreePhysicalPages(1, pages.as_mut_ptr()), 1);
        }
    }

    #[test]
    fn map_pages_gap_overrun_is_refused() {
        // Upstream #1635, gap half: a range holding a page number from no live
        // allocation is refused with ERROR_INVALID_PARAMETER however it is
        // mapped (NULL address or an explicit graphics VA). Only own pages and
        // VAs are asserted, so other tests may run alongside.
        unsafe {
            let (mut n, mut pages) = (1usize, [0u64; 1]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            let live = pages[0] as usize;
            // A page number far past every live block: never allocated, so it
            // has no home whatever other tests allocate concurrently.
            let dead = live + FRAMES * 1024;
            assert!(page_home(dead).is_none());

            let mut overrun = [pages[0], dead as u64];
            set_error(0);
            assert!(XMemMapPhysicalPages(std::ptr::null_mut(), 2, overrun.as_mut_ptr()).is_null());
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);

            // An explicit graphics VA refuses the same range before touching memory.
            let at = XMemMapPhysicalPages(std::ptr::null_mut(), 1, pages.as_mut_ptr());
            assert!(!at.is_null());
            set_error(0);
            assert_eq!(
                XMemMapPhysicalPages(at, 2, overrun.as_mut_ptr()),
                std::ptr::null_mut()
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            // The failed call left the live mapping alone.
            assert_eq!(XMemMapPhysicalPages(at, 1, pages.as_mut_ptr()) as usize, at as usize);

            assert_eq!(XMemFreePhysicalPages(1, pages.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(at, 0, 0x8000), 1);
        }
    }

    #[test]
    fn cross_allocation_live_span_splits_with_per_page_contents() {
        // Upstream #1635, live-span half, DELIBERATE divergence: consecutive page
        // numbers spanning two live allocations refuse upstream
        // (ERROR_INVALID_PARAMETER) but split-map here — the NULL-address fast
        // path only fires for one live block (`contiguous_home`), everything else
        // falls through to the per-page loop, so no cross-block home is ever
        // returned over unrelated memory. No `page_block` refactor: the split is
        // the fix, pinned for both the NULL and the explicit-VA paths.
        unsafe {
            let page = 0x1_0000usize;
            let (mut na, mut pa) = (1usize, [0u64; 1]);
            let (mut nb, mut pb) = (1usize, [0u64; 1]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut na, pa.as_mut_ptr()), 1);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut nb, pb.as_mut_ptr()), 1);
            let (a, b) = (pa[0] as usize, pb[0] as usize);
            if b != a + FRAMES {
                // Another test allocated between the two calls: not one
                // consecutive span (the quad case in
                // `cross_allocation_pages_map_split_and_unmap_preserves_neighbors`
                // covers the non-consecutive layout instead).
                assert_eq!(XMemFreePhysicalPages(1, pa.as_mut_ptr()), 1);
                assert_eq!(XMemFreePhysicalPages(1, pb.as_mut_ptr()), 1);
                return;
            }
            assert_eq!(contiguous_home(&[a, b]), None, "two allocations never share one home");
            assert!(write_physical_pages(&[a], 0, &0xD0u32.to_ne_bytes()));
            assert!(write_physical_pages(&[b], 0, &0xD1u32.to_ne_bytes()));

            // NULL address: succeeds, each page keeps its own contents.
            let mut pair = [pa[0], pb[0]];
            let m = XMemMapPhysicalPages(std::ptr::null_mut(), 2, pair.as_mut_ptr()) as *mut u32;
            assert!(!m.is_null());
            assert_eq!(*m.add(page / 4 * 0), 0xD0);
            assert_eq!(*m.add(page / 4 * 1), 0xD1);

            // Explicit address: the same span re-maps per page, contents intact.
            let mut pair2 = [pa[0], pb[0]];
            assert_eq!(
                XMemMapPhysicalPages(m as *mut c_void, 2, pair2.as_mut_ptr()) as usize,
                m as usize
            );
            assert_eq!(*m.add(page / 4 * 0), 0xD0);
            assert_eq!(*m.add(page / 4 * 1), 0xD1);

            assert_eq!(XMemFreePhysicalPages(1, pa.as_mut_ptr()), 1);
            assert_eq!(XMemFreePhysicalPages(1, pb.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(m as *mut c_void, 0, 0x8000), 1);
        }
    }

    /// Whether an away-mapped physical page is recorded at `at` (test-only view
    /// of the AWAY table; only own addresses are asserted).
    fn away_mapped(at: usize) -> bool {
        away().lock().unwrap().as_ref().is_some_and(|st| st.at.contains_key(&at))
    }

    #[test]
    fn virtual_free_unmaps_away_pages_across_gfx_reservations() {
        // Upstream #2544, Linux twin: VirtualFree on a graphics reservation used
        // to leave stale address→page entries (release forgot every mapping past
        // the exact base; decommit forgot them all). Partial MEM_DECOMMIT unmaps
        // the covered pages, saving their bytes home; MEM_RELEASE clears the
        // rest. Plain (non-graphics) frees need no twin: every away mapping
        // lives in the graphics pool, so those paths cannot overlap one.
        unsafe {
            let (mut n, mut pages) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            let [p0, p1] = [pages[0] as usize, pages[1] as usize];
            let r = XMemMapPhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()) as usize;
            assert_ne!(r, 0);
            assert!(away_mapped(r) && away_mapped(r + PHYS_PAGE));
            *(r as *mut u32) = 0x5555_0001;
            // Partial MEM_DECOMMIT unmaps the first page (its bytes went home);
            // the second page stays mapped.
            assert_eq!(VirtualFree(r as *mut c_void, PHYS_PAGE, MEM_DECOMMIT), 1);
            assert!(!away_mapped(r) && away_mapped(r + PHYS_PAGE));
            // Full MEM_RELEASE clears the remaining mapping: no stale entries.
            assert_eq!(VirtualFree(r as *mut c_void, 0, MEM_RELEASE), 1);
            assert!(!away_mapped(r) && !away_mapped(r + PHYS_PAGE));
            // Remap into a fresh reservation: p0 kept its bytes, untouched p1 reads 0.
            let r2 = XMemMapPhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()) as usize;
            assert_ne!(r2, 0);
            assert_eq!((*(r2 as *const u32), *((r2 + PHYS_PAGE) as *const u32)), (0x5555_0001, 0));
            assert_eq!(VirtualFree(r2 as *mut c_void, 0, MEM_RELEASE), 1);
            assert!(!away_mapped(r2) && !away_mapped(r2 + PHYS_PAGE));
            assert_eq!(XMemFreePhysicalPages(2, pages.as_mut_ptr()), 1);
            // The page numbers resolve nowhere once freed.
            assert!(page_home(p0).is_none() && page_home(p1).is_none());
        }
    }

    #[test]
    fn remap_at_current_mapping_preserves_contents() {
        // Linux twin of upstream #1143's
        // `mapping_page_at_its_own_home_in_mixed_call_keeps_it_committed`. The
        // upstream `home == at` arm has no Linux counterpart: homes are private
        // `malloc` memory while mapping targets always come from the graphics
        // pool, so a home address can never equal an explicit VA (the
        // `gfx_contains` gate rejects anything else). Re-mapping a page at the
        // VA it already occupies therefore goes through the ordinary move path
        // and stays AWAY-recorded by design — no copy-onto-itself decommit, no
        // lost contents. Only own pages and VAs are asserted.
        unsafe {
            let page = 0x1_0000usize;
            let (mut n2, mut blk) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n2, blk.as_mut_ptr()), 1);
            let (mut n1, mut other) = (1usize, [0u64; 1]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n1, other.as_mut_ptr()), 1);
            let (b0, o) = (blk[0] as usize, other[0] as usize);

            let h0 = XMemMapPhysicalPages(std::ptr::null_mut(), 1, blk.as_mut_ptr()) as usize;
            let h_other =
                XMemMapPhysicalPages(std::ptr::null_mut(), 1, other.as_mut_ptr()) as usize;
            assert_ne!(h0, 0);
            assert_ne!(h_other, 0);
            *(h0 as *mut u32) = 0x1122_3344;
            *(h_other as *mut u32) = 0x5566_7788;

            // Map [blk[0], other] at h0: blk[0] already occupies h0, other moves
            // to h0 + PHYS_PAGE. Both keep their contents.
            let mut mixed = [blk[0], other[0]];
            assert_eq!(
                XMemMapPhysicalPages(h0 as *mut c_void, 2, mixed.as_mut_ptr()) as usize,
                h0
            );
            assert_eq!(*(h0 as *const u32), 0x1122_3344);
            assert_eq!(*((h0 + page) as *const u32), 0x5566_7788);
            // Linux bookkeeping (differs from upstream on purpose): the mapping
            // stays AWAY-recorded instead of becoming home-resident.
            let guard = away().lock().unwrap();
            let recorded = guard
                .as_ref()
                .is_some_and(|st| st.page.get(&b0) == Some(&h0) && st.page.get(&o) == Some(&(h0 + page)));
            drop(guard);
            assert!(recorded, "re-mapped pages stay AWAY-recorded on Linux");

            assert_eq!(XMemFreePhysicalPages(2, blk.as_mut_ptr()), 1);
            assert_eq!(XMemFreePhysicalPages(1, other.as_mut_ptr()), 1);
            assert_eq!(VirtualFree(h0 as *mut c_void, 0, 0x8000), 1);
            assert_eq!(VirtualFree(h_other as *mut c_void, 0, 0x8000), 1);
        }
    }

    #[test]
    fn scatter_without_pages_unmaps_each_address() {
        // Linux twin of upstream #495's `scatter_without_pages_unmaps`: a NULL
        // page array means page 0 (unmap) for every address — a retail title
        // unmaps this way while streaming and stopped on its own `int3` when
        // the call was refused with ERROR_INVALID_PARAMETER. Only own pages
        // and VAs are asserted, so other tests may run alongside.
        unsafe {
            let page = PHYS_PAGE;
            let (mut n, mut pages) = (2usize, [0u64; 2]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            let mp = XMemMapPhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()) as *mut u32;
            assert!(!mp.is_null());
            *mp.add(page / 4 * 0) = 7;
            *mp.add(page / 4 * 1) = 9;
            let addrs = [mp as *mut c_void, ((mp as usize) + page) as *mut c_void];
            assert_eq!(
                XMemMapPhysicalPagesScatter(addrs.as_ptr(), 2, std::ptr::null_mut()),
                1,
                "no page array: unmapped, not refused"
            );
            // The pages kept what was written: mapped again, they read it.
            assert_eq!(
                XMemMapPhysicalPagesScatter(addrs.as_ptr(), 2, pages.as_mut_ptr() as *mut u64),
                1
            );
            assert_eq!((*mp.add(page / 4 * 0), *mp.add(page / 4 * 1)), (7, 9));
            assert_eq!(
                XMemMapPhysicalPagesScatter(addrs.as_ptr(), 2, std::ptr::null_mut()),
                1
            );
            // No addresses: the same answer with and without a page array.
            let mut none = [0u64; 1];
            assert_eq!(
                XMemMapPhysicalPagesScatter(addrs.as_ptr(), 0, std::ptr::null_mut()),
                XMemMapPhysicalPagesScatter(
                    addrs.as_ptr(),
                    0,
                    none.as_mut_ptr() as *mut u64
                )
            );
            assert_eq!(XMemFreePhysicalPages(2, pages.as_mut_ptr() as *mut u64), 1);
            assert_eq!(VirtualFree(mp as *mut c_void, 0, 0x8000), 1);
        }
    }

    #[test]
    fn xmem_virtual_query_writes_the_whole_flag_field_and_gpu_visibility() {
        // Linux twin of upstream #2048's
        // `xmem_virtual_query_writes_the_whole_flag_field_and_gpu_visibility`:
        // the field at +48 is a 16-byte flags field, not just the low u32.
        // Bit 0 marks committed readable memory (CPU cached), bit 1 marks
        // GPU-accessible UMA / sparse-VRAM memory. The tail (+52..+64) is
        // written as zero, never left as the caller's buffer contents. Only
        // own addresses are asserted, so other tests may run alongside.
        unsafe {
            let proc_ = current_process_handle();
            let x = 5u32;
            let mut info = [0xCCu8; 64];
            assert_eq!(
                XMemVirtualQuery(proc_, &x as *const u32 as *const c_void, info.as_mut_ptr()),
                1
            );
            let flags0 = u32::from_ne_bytes(info[48..52].try_into().unwrap());
            assert_eq!(flags0 & 3, 1, "first flags were {flags0:#x}");
            assert_eq!(flags0 & 2, 0, "plain stack memory is not GPU-visible");
            assert!(
                info[52..64].iter().all(|&b| b == 0),
                "flag tail was not written: {:?}",
                &info[48..64]
            );

            // A reserved page has no CPU-readable flag either, but the tail is still written.
            let r = VirtualAlloc(std::ptr::null(), 0x1_0000, MEM_RESERVE, PAGE_NOACCESS);
            assert!(!r.is_null());
            let mut info = [0xCCu8; 64];
            assert_eq!(XMemVirtualQuery(proc_, r, info.as_mut_ptr()), 1);
            assert_eq!(u32::from_ne_bytes(info[48..52].try_into().unwrap()) & 3, 0);
            assert!(info[52..64].iter().all(|&b| b == 0));
            assert_eq!(VirtualFree(r, 0, MEM_RELEASE), 1);

            // Coherent graphics-pool memory is visible to both the CPU and the
            // GPU (Linux counterpart of upstream's coherent-UMA leg: the
            // graphics pool is the title's GPU-addressable reservation set;
            // `gpu::uma` is `cfg(windows)`-only so there is no UMA twin to
            // allocate from here). Unmapped-but-reserved graphics memory keeps
            // the GPU bit without the CPU bit (upstream's sparse-VRAM shape).
            let (mut mn, mut mpgs) = (1usize, [0u64; 1]);
            assert_eq!(XMemAllocatePhysicalPages(0, &mut mn, mpgs.as_mut_ptr()), 1);
            let gx = XMemMapPhysicalPages(std::ptr::null_mut(), 1, mpgs.as_mut_ptr());
            assert!(!gx.is_null());
            let mut info = [0xCCu8; 64];
            assert_eq!(XMemVirtualQuery(proc_, gx, info.as_mut_ptr()), 1);
            let flags = u32::from_ne_bytes(info[48..52].try_into().unwrap());
            assert_eq!(flags & 3, 3, "graphics-pool memory must be CPU- and GPU-visible: {flags:#x}");
            assert!(info[52..64].iter().all(|&b| b == 0));
            // Unmap (decommit, still a graphics reservation): GPU bit stays, CPU bit clears.
            let gaddrs = [gx];
            assert_eq!(
                XMemMapPhysicalPagesScatter(gaddrs.as_ptr(), 1, std::ptr::null_mut()),
                1
            );
            let mut info = [0xCCu8; 64];
            assert_eq!(XMemVirtualQuery(proc_, gx, info.as_mut_ptr()), 1);
            let flags = u32::from_ne_bytes(info[48..52].try_into().unwrap());
            assert_eq!(flags & 3, 2, "unmapped graphics reservation must stay GPU-visible: {flags:#x}");
            assert!(info[52..64].iter().all(|&b| b == 0));
            assert_eq!(XMemFreePhysicalPages(1, mpgs.as_mut_ptr() as *mut u64), 1);
            assert_eq!(VirtualFree(gx, 0, MEM_RELEASE), 1);
        }
    }

    #[test]
    fn physical_page_liveness_crosses_bitmap_words() {
        let boundary = PhysicalBlock::WORD_BITS;
        let mut block = PhysicalBlock::new(boundary + 1, 0);
        assert!(block.free_page(boundary - 1));
        assert!(block.is_live(boundary));
        assert!(block.free_page(boundary));
        assert!(!block.free_page(boundary));
        assert!(!block.is_live(boundary - 1));
        assert!(!block.free_page(boundary + 1));
        assert!(block.is_live(0));
        assert_eq!(block.live, boundary - 1);
    }

    #[test]
    fn repeated_physical_page_release_preserves_live_siblings() {
        // Linux twin of upstream #608's `repeated_physical_page_release_*`:
        // no UMA gate (Linux homes are `calloc` + the graphics pool, always
        // available), same double-free-then-mixed-free shape through the
        // Linux `AllocateTitlePhysicalPages` / `FreeTitlePhysicalPages` /
        // `MapTitlePhysicalPages` entry points. Only own pages and VAs are
        // asserted, so other tests may run alongside.
        unsafe {
            let mut n = 3usize;
            let mut pages = [0usize; 3];
            assert_eq!(AllocateTitlePhysicalPages(0, 0, &mut n, pages.as_mut_ptr()), 1);
            let [mut first, second, mut sibling] = pages;
            assert!(write_physical_pages(&[sibling], 0, &[0x7d]));
            assert!(write_physical_pages(&[sibling], PHYS_PAGE - 1, &[0x4a]));

            let mut duplicate = [first, first];
            assert_eq!(FreeTitlePhysicalPages(0, 2, duplicate.as_mut_ptr()), 1);
            let mut bytes = [0u8; 2];
            assert!(!read_physical_pages(&[first], 0, &mut bytes));
            assert!(!write_physical_pages(&[first], 0, &[0]));
            assert!(MapTitlePhysicalPages(std::ptr::null_mut(), 1, 0, 0, &mut first).is_null());
            assert!(page_home(second).is_some());

            let mut mixed = [first, second];
            assert_eq!(FreeTitlePhysicalPages(0, 2, mixed.as_mut_ptr()), 1);
            assert!(page_home(second).is_none());
            assert!(read_physical_pages(&[sibling, sibling], PHYS_PAGE - 1, &mut bytes));
            assert_eq!(bytes, [0x4a, 0x7d]);
            let home = MapTitlePhysicalPages(std::ptr::null_mut(), 1, 0, 0, &mut sibling);
            assert!(!home.is_null());
            assert_eq!(MapTitlePhysicalPages(home, 1, 0, 0, &mut sibling), home);

            assert_eq!(FreeTitlePhysicalPages(0, 1, &mut sibling), 1);
            assert!(page_home(sibling).is_none());
            assert_eq!(VirtualFree(home, 0, 0x8000), 1);
        }
    }

    #[test]
    fn user_default_geo_name_reports_us_two_letter_name() {
        // Retail M1: the title calls `GetUserDefaultGeoName` just before the
        // exe+0x2237320 abort; the generic stub returned 0 with an empty
        // buffer and the NULL locale string fed the stage-2 object chain.
        unsafe {
            // Size query (null buffer or zero length) reports room for "US"+NUL.
            assert_eq!(GetUserDefaultGeoName(std::ptr::null_mut(), 0), 3);
            let mut probe = [0xCCCCu16; 1];
            assert_eq!(GetUserDefaultGeoName(probe.as_mut_ptr(), 0), 3);
            // Fill writes L"US"+NUL and returns chars written excluding NUL.
            let mut buf = [0xCCCCu16; 4];
            assert_eq!(GetUserDefaultGeoName(buf.as_mut_ptr(), buf.len() as i32), 2);
            assert_eq!(&buf[..3], &['U' as u16, 'S' as u16, 0]);
            // Wired into the KERNELX export table so kernel32/api-set imports resolve.
            assert!(exports().iter().any(|(n, _)| *n == "GetUserDefaultGeoName"));
        }
    }

    #[test]
    fn user_default_locale_name_fills_en_us_and_reports_the_null() {
        // Retail Step 38: the language/config init calls
        // GetUserDefaultLocaleName(buf, 0x55) through
        // api-ms-win-core-localization-l1-2-0.dll; the auto-stub returned 0 and left
        // the buffer empty, so the title saw no locale.
        unsafe {
            let mut buf = [0xCCCCu16; 0x55];
            let n = GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32);
            assert_eq!(n, 6, "chars including NUL");
            assert_eq!(&buf[..6], &[b'e' as u16, b'n' as u16, b'-' as u16, b'U' as u16, b'S' as u16, 0]);
            // Too small / invalid arguments fail with 0 instead of a truncated name.
            let mut small = [0xCCCCu16; 3];
            assert_eq!(GetUserDefaultLocaleName(small.as_mut_ptr(), small.len() as i32), 0);
            assert_eq!(&small, &[0xCCCC, 0xCCCC, 0xCCCC]);
            assert_eq!(GetUserDefaultLocaleName(std::ptr::null_mut(), 0x55), 0);
            assert_eq!(GetUserDefaultLocaleName(buf.as_mut_ptr(), 0), 0);
            assert!(exports().iter().any(|(n, _)| *n == "GetUserDefaultLocaleName"));
        }
    }

    #[test]
    fn failed_physical_page_read_or_write_leaves_earlier_bytes_untouched() {
        // Linux twin of upstream #1781's
        // `failed_physical_page_read_or_write_does_not_commit_earlier_sparse_homes`:
        // Linux homes are always-committed `calloc` memory — there is no
        // `home_again` / `sparse_pages` lazy-commit leg, so the Windows
        // partial-commit class (commit-then-fail inside the resolve loop)
        // cannot occur: `resolve_physical_pages` returns `None` before either
        // copy loop runs. Pin that a trailing unmapped page fails the call
        // and leaves the earlier page's bytes exactly as before.
        unsafe {
            let mut n = 1usize;
            let mut pages = [0usize; 1];
            assert_eq!(AllocateTitlePhysicalPages(0, 0, &mut n, pages.as_mut_ptr()), 1);
            let page = pages[0];
            // Page 0 is never a live allocation (`page_index(0)` is `None`),
            // so it plays the trailing-unmapped leg of the upstream test.
            assert!(write_physical_pages(&[page], 0, &vec![0xA5u8; PHYS_PAGE]));
            assert!(!write_physical_pages(&[page, 0], 0, &vec![0x5Au8; PHYS_PAGE + 1]));
            let mut check = vec![0u8; PHYS_PAGE];
            assert!(read_physical_pages(&[page], 0, &mut check));
            assert_eq!(check, vec![0xA5u8; PHYS_PAGE], "a failed write leaves earlier bytes untouched");
            let mut dst = vec![0xCCu8; PHYS_PAGE + 1];
            assert!(!read_physical_pages(&[page, 0], 0, &mut dst));
            assert_eq!(dst, vec![0xCCu8; PHYS_PAGE + 1], "a failed read leaves the destination untouched");
            assert_eq!(FreeTitlePhysicalPages(0, 1, pages.as_mut_ptr()), 1);
        }
    }

    #[test]
    fn critical_section_ex_and_pointer_cookies() {
        unsafe {
            let mut cs: CritSec = std::mem::zeroed();
            assert_eq!(InitializeCriticalSectionEx(&mut cs, 0x400, 1), 1);
            EnterCriticalSection(&mut cs);
            LeaveCriticalSection(&mut cs);
            assert_eq!(DeleteCriticalSection(&mut cs), 1);
            let p = &mut cs as *mut CritSec as *mut c_void;
            assert_eq!(EncodePointer(p), p);
            assert_eq!(DecodePointer(p), p);
            assert_eq!(EncodePointer(std::ptr::null_mut()), std::ptr::null_mut());
        }
    }
}
