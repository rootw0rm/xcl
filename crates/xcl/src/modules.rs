//! Process-wide table of modules XCL knows about:
//! * images mapped by the XCL loader (game exe + game-shipped DLLs),
//! * pseudo handles for HLE modules (`kernelx.dll` & co. have no real image).
//!
//! Address → module lookups used from exception dispatch (`RtlPcToFileHeader` hook) go
//! through a lock-free fixed array so they can never deadlock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::pe::OwnedExports;

pub struct MappedModule {
    /// Lower-case file name, e.g. "game.exe".
    pub name: String,
    pub path: PathBuf,
    pub base: usize,
    pub size: usize,
    pub entry_rva: u32,
    pub is_dll: bool,
    pub exports: OwnedExports,
    pub tls_callbacks: Mutex<Vec<usize>>,
    /// DisableThreadLibraryCalls was called.
    pub no_thread_calls: AtomicBool,
    /// DllMain(PROCESS_ATTACH) has completed.
    pub initialized: AtomicBool,
    pub stack_reserve: u64,
}

impl MappedModule {
    pub fn contains(&self, addr: usize) -> bool {
        addr.checked_sub(self.base).is_some_and(|off| off < self.size)
    }
}

static MAPPED: RwLock<Vec<Arc<MappedModule>>> = RwLock::new(Vec::new());
static MAIN: RwLock<Option<Arc<MappedModule>>> = RwLock::new(None);

const MAX_RANGES: usize = 512;

#[repr(C)]
struct RangeEntry {
    base: AtomicUsize,
    size: AtomicUsize,
}

struct RangeTable {
    count: AtomicUsize,
    min_addr: AtomicUsize,
    max_inclusive: AtomicUsize,
    overflow_warned: AtomicBool,
    entries: [RangeEntry; MAX_RANGES],
}

impl RangeTable {
    const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            min_addr: AtomicUsize::new(usize::MAX),
            max_inclusive: AtomicUsize::new(0),
            overflow_warned: AtomicBool::new(false),
            entries: [const { RangeEntry { base: AtomicUsize::new(0), size: AtomicUsize::new(0) } }; MAX_RANGES],
        }
    }

    fn insert(&self, base: usize, size: usize) -> bool {
        if base == 0 || size == 0 {
            return false;
        }
        let n = self.count.load(Ordering::Relaxed);
        if n >= MAX_RANGES {
            return !self.overflow_warned.swap(true, Ordering::Relaxed);
        }
        self.entries[n].size.store(size, Ordering::Relaxed);
        self.entries[n].base.store(base, Ordering::Relaxed);
        self.min_addr.fetch_min(base, Ordering::Relaxed);
        self.max_inclusive.fetch_max(base.saturating_add(size - 1), Ordering::Relaxed);
        self.count.store(n + 1, Ordering::Release);
        false
    }

    fn remove(&self, base: usize) {
        if base == 0 {
            return;
        }
        let n = self.count.load(Ordering::Relaxed);
        for entry in &self.entries[..n] {
            if entry.base.load(Ordering::Relaxed) == base {
                entry.base.store(0, Ordering::Release);
            }
        }
    }

    fn lookup(&self, addr: usize) -> Option<usize> {
        self.lookup_range(addr).map(|(base, _)| base)
    }

    /// Lock-free: (base, size) of the range containing `addr` (`None` when the
    /// address is in no tracked range). `lookup` is this without the size.
    fn lookup_range(&self, addr: usize) -> Option<(usize, usize)> {
        let n = self.count.load(Ordering::Acquire);
        if n == 0 {
            return None;
        }
        let lo = self.min_addr.load(Ordering::Relaxed);
        let hi = self.max_inclusive.load(Ordering::Relaxed);
        if addr < lo || addr > hi {
            return None;
        }
        for entry in &self.entries[..n] {
            let b = entry.base.load(Ordering::Acquire);
            if b != 0 && addr >= b {
                let s = entry.size.load(Ordering::Relaxed);
                if addr - b < s {
                    return Some((b, s));
                }
            }
        }
        None
    }
}

static RANGES: RangeTable = RangeTable::new();

pub fn register_mapped(m: Arc<MappedModule>) {
    let warn_overflow = {
        let mut g = MAPPED.write().unwrap();
        let warn = RANGES.insert(m.base, m.size);
        g.push(m);
        warn
    };
    if warn_overflow {
        crate::xlog_warn!("modules: lock-free range table full ({MAX_RANGES} entries); subsequent modules are not tracked in RANGES");
    }
}

/// The image at `base` failed to load: it is no longer found (by name, base, address), so a
/// later load maps it again – as Windows' loader unloads a DLL that failed.
pub fn unregister_mapped(base: usize) {
    let mut g = MAPPED.write().unwrap();
    g.retain(|m| m.base != base);
    RANGES.remove(base);
}

/// Lock-free: base of the mapped image containing `addr`.
pub fn image_base_for_addr(addr: usize) -> Option<usize> {
    RANGES.lookup(addr)
}

/// Lock-free: (base, size) of the mapped image containing `addr` (`None` when the
/// address is in no mapped image). Crash-time readers use this instead of the
/// table lock so a crash report never blocks behind the faulting thread.
pub fn image_range_for_addr(addr: usize) -> Option<(usize, usize)> {
    RANGES.lookup_range(addr)
}

pub fn mapped_modules() -> Vec<Arc<MappedModule>> {
    MAPPED.read().map(|v| v.clone()).unwrap_or_default()
}

pub fn find_mapped_by_name(name: &str) -> Option<Arc<MappedModule>> {
    let key = crate::util::dll_key(name);
    MAPPED.read().ok()?.iter().find(|m| m.name == key).cloned()
}

pub fn find_mapped_by_base(base: usize) -> Option<Arc<MappedModule>> {
    MAPPED.read().ok()?.iter().find(|m| m.base == base).cloned()
}

pub fn find_mapped_by_addr(addr: usize) -> Option<Arc<MappedModule>> {
    MAPPED.read().ok()?.iter().find(|m| m.contains(addr)).cloned()
}

pub fn set_main(m: Arc<MappedModule>) {
    *MAIN.write().unwrap() = Some(m);
}

pub fn main_module() -> Option<Arc<MappedModule>> {
    MAIN.read().ok()?.clone()
}

// ---------------------------------------------------------------------------------------
// HLE pseudo handles
// ---------------------------------------------------------------------------------------

static HLE_HANDLES: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Stable fake HMODULE for an HLE module: a committed page starting with "MZ".
pub fn hle_handle(module_name: &str) -> usize {
    let key = crate::util::dll_key(module_name);
    let mut g = HLE_HANDLES.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    if let Some(&h) = map.get(&key) {
        return h;
    }
    #[cfg(windows)]
    let p = unsafe {
        use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
        VirtualAlloc(std::ptr::null(), 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
    } as *mut u8;
    #[cfg(not(windows))]
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            0x1000,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        ) as *mut u8
    };
    assert!(p as isize > 0, "allocating a page for an HLE pseudo handle failed");
    unsafe {
        *p = b'M';
        *p.add(1) = b'Z';
    }
    map.insert(key, p as usize);
    p as usize
}

pub fn find_hle_handle(module_name: &str) -> Option<usize> {
    let key = crate::util::dll_key(module_name);
    let g = HLE_HANDLES.lock().ok()?;
    g.as_ref()?.get(&key).copied()
}

pub fn hle_name_by_handle(h: usize) -> Option<String> {
    let g = HLE_HANDLES.lock().ok()?;
    g.as_ref()?.iter().find(|(_, &v)| v == h).map(|(k, _)| k.clone())
}

/// `name+offset` for an address inside its module, else the bare address.
///
/// The addresses `describe_address` gets include raw stack words, which can be anything. Wine
/// may report a module for one with a handle above the address: the subtraction must not panic
/// there, or a crash report turns into a panic in the crash handler.
#[cfg_attr(not(windows), allow(dead_code))] // used by the Windows fallback and its test
fn module_offset(name: &str, base: usize, addr: usize) -> String {
    match addr.checked_sub(base) {
        Some(offset) => format!("{name}+{offset:#x}"),
        None => format!("{addr:#x}"),
    }
}

/// "game.exe+0x1234", "KERNELBASE.dll+0x5678" or "0x...".
pub fn describe_address(addr: usize) -> String {
    if let Some(b) = image_base_for_addr(addr) {
        if let Ok(g) = MAPPED.try_read() {
            if let Some(m) = g.iter().find(|m| m.base == b) {
                return format!("{}+{:#x}", m.name, addr - b);
            }
        }
        return format!("{b:#x}+{:#x}", addr - b);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{
            GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        let mut h = std::ptr::null_mut();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as *const u16,
            &mut h,
        ) != 0
        {
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(h, buf.as_mut_ptr(), 260) as usize;
            let full = String::from_utf16_lossy(&buf[..n]);
            let name = full.rsplit('\\').next().unwrap_or(&full).to_string();
            return module_offset(&name, h as usize, addr);
        }
    }
    // Linux: only images XCL itself mapped are named; there is no system loader lookup.
    format!("{addr:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_offset_survives_a_base_above_the_address() {
        // What Wine answered for a stack word while reporting a crash of the retail title:
        // the handle lay above the address (the subtraction panicked and the report was lost).
        assert_eq!(
            module_offset("kernelbase.dll", 0x7fff_0000_0000, 0x6f93_bbb0_e7dc),
            "0x6f93bbb0e7dc"
        );
        assert_eq!(module_offset("game.exe", 0x140000000, 0x140001234), "game.exe+0x1234");
        assert_eq!(module_offset("game.exe", 0x140000000, 0x140000000), "game.exe+0x0");
    }

    #[test]
    fn describe_address_takes_arbitrary_stack_words() {
        for addr in [0usize, 1, 0x1000_0000_4, 0x7fff_ffff_ffff, usize::MAX] {
            let _ = describe_address(addr);
        }
    }

    fn fake_module(name: &str, base: usize, size: usize) -> Arc<MappedModule> {
        Arc::new(MappedModule {
            name: name.to_string(),
            path: PathBuf::from(name),
            base,
            size,
            entry_rva: 0,
            is_dll: true,
            exports: OwnedExports::default(),
            tls_callbacks: Mutex::new(Vec::new()),
            no_thread_calls: AtomicBool::new(false),
            initialized: AtomicBool::new(true),
            stack_reserve: 0,
        })
    }

    #[test]
    fn contains_and_range_table_handle_overflow_and_envelope_boundaries() {
        let hi_base = usize::MAX - 0xFFF;
        let m = fake_module("high.dll", hi_base, 0x2000);
        assert!(m.contains(usize::MAX) && m.contains(hi_base) && !m.contains(hi_base - 1));

        let t = RangeTable::new();
        assert_eq!(t.lookup(0x1_4000_0000), None);
        assert!(!t.insert(0, 0x1000) && !t.insert(0x1_4000_0000, 0));
        assert_eq!((t.count.load(Ordering::Relaxed), t.min_addr.load(Ordering::Relaxed)), (0, usize::MAX));

        assert!(!t.insert(0x1_4000_0000, 0x1000));
        assert!(!t.insert(0x1_5000_0000, 0x2000));
        assert_eq!(t.lookup(0x1_3FFF_FFFF), None);
        assert_eq!(t.lookup(0x1_4000_0000), Some(0x1_4000_0000));
        assert_eq!(t.lookup(0x1_4000_0FFF), Some(0x1_4000_0000));
        assert_eq!(t.lookup(0x1_4000_1000), None);
        assert_eq!(t.lookup(0x1_5000_1FFF), Some(0x1_5000_0000));
        assert_eq!(t.lookup(0x1_5000_2000), None);

        t.remove(0);
        t.remove(0x1_4000_0000);
        assert_eq!((t.lookup(0x1_4000_0100), t.lookup(0x1_5000_0100)), (None, Some(0x1_5000_0000)));

        assert!(!t.insert(hi_base, 0x2000));
        assert_eq!((t.lookup(usize::MAX), t.lookup(hi_base), t.lookup(hi_base - 1)), (Some(hi_base), Some(hi_base), None));

        let full = RangeTable::new();
        for k in 0..MAX_RANGES { assert!(!full.insert(0x2_0000_0000 + k * 0x10000, 0x1000)); }
        assert!(full.insert(0x3_0000_0000, 0x1000));
        assert!(!full.insert(0x3_0001_0000, 0x1000));
        assert_eq!(full.count.load(Ordering::Relaxed), MAX_RANGES);
    }

    #[test]
    fn concurrent_register_and_unregister_leave_no_zombie_ranges() {
        struct UnregGuard(Vec<usize>);
        impl Drop for UnregGuard {
            fn drop(&mut self) { for &b in &self.0 { unregister_mapped(b); } }
        }
        let origin = 0x0000_6ABC_0000_0000usize;
        let bases: Vec<usize> = (0..13).map(|k| origin + k * 0x10_0000).collect();
        let _guard = UnregGuard(bases.clone());
        register_mapped(fake_module("sib.dll", bases[0], 0x4000));

        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..2).map(|_| {
            let (stop, b0, probes) = (Arc::clone(&stop), bases[0], bases.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    assert_eq!(image_base_for_addr(b0 + 0x100), Some(b0));
                    for &b in &probes[1..] { let _ = image_base_for_addr(b + 0x100); }
                    assert_eq!(image_base_for_addr(0x0000_7FFF_DEAD_0000), None);
                }
            })
        }).collect();
        let writers: Vec<_> = [1..=6, 7..=12].into_iter().map(|range| {
            let bases = bases.clone();
            std::thread::spawn(move || {
                for k in range {
                    register_mapped(fake_module(&format!("tmp{k}.dll"), bases[k], 0x2000));
                    assert_eq!(image_base_for_addr(bases[k] + 0x40), Some(bases[k]));
                    unregister_mapped(bases[k]);
                }
            })
        }).collect();
        for w in writers { w.join().unwrap(); }
        stop.store(true, Ordering::Relaxed);
        for r in readers { r.join().unwrap(); }
        assert_eq!(image_base_for_addr(bases[0] + 0x100), Some(bases[0]));
        for &b in &bases[1..] {
            assert_eq!(image_base_for_addr(b + 0x100), None);
            assert!(find_mapped_by_base(b).is_none());
        }

        if !cfg!(debug_assertions) {
            let (t, old_b, old_s) = (RangeTable::new(), [const { AtomicUsize::new(0) }; 32], [const { AtomicUsize::new(0) }; 32]);
            for k in 0..32usize {
                let b = 0x1_4000_0000 + k * 0x100_0000;
                t.insert(b, 0x80_0000);
                old_b[k].store(b, Ordering::Relaxed);
                old_s[k].store(0x80_0000, Ordering::Relaxed);
            }
            let probe = std::hint::black_box(0x7FF9_1234_5678usize);
            let n_iters = 200_000u32;
            let t0 = std::time::Instant::now();
            let mut old_hits = 0u32;
            for _ in 0..n_iters {
                let mut hit = false;
                for i in 0..32 {
                    let (b, s) = (old_b[i].load(Ordering::Acquire), old_s[i].load(Ordering::Acquire));
                    if b != 0 && probe >= b && probe < b.wrapping_add(s) { hit = true; break; }
                }
                old_hits += u32::from(std::hint::black_box(hit));
            }
            let old_ns = t0.elapsed().as_nanos() as f64 / n_iters as f64;
            let t1 = std::time::Instant::now();
            let mut new_hits = 0u32;
            for _ in 0..n_iters { new_hits += u32::from(std::hint::black_box(t.lookup(probe).is_some())); }
            let new_ns = t1.elapsed().as_nanos() as f64 / n_iters as f64;
            assert_eq!((old_hits, new_hits), (0, 0));
            println!("MODULES_BENCH_NS out_of_envelope_32: before={old_ns:.2} after={new_ns:.2}");
        }
    }
}
