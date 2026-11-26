//! Linux auto-stubs (Phase C step 10a): same thunk shape as Windows, `mmap` backing.
//!
//! Each stub is a tiny x64 thunk (Win64 ABI, as the game calls it):
//! ```text
//! sub  rsp, 0x28
//! mov  rcx, <StubInfo*>
//! mov  rdx, [rsp+0x28]        ; return address = caller
//! mov  rax, <stub_report>
//! call rax
//! add  rsp, 0x28
//! ret                         ; rax = configured return value
//! ```
//! The thunk is entered with the Windows x64 calling convention (the game is a
//! Windows PE), so it sets RCX/RDX itself before calling `stub_report`. The
//! reporter is therefore `extern "win64"` on Linux (on Windows `extern "system"`
//! is the same ABI). x86_64 keeps the instruction cache coherent with data writes,
//! so no `FlushInstructionCache` twin is needed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

use crate::util::parse_ret;

pub struct StubInfo {
    pub dll: String,
    pub name: String,
    pub ret: u64,
    pub calls: AtomicU64,
    pub first_caller: AtomicUsize,
    /// Optional `fn(&StubInfo, caller)` invoked on every call (e.g. present heuristics).
    pub hook: AtomicUsize,
}

pub type StubHook = fn(&StubInfo, usize);

static ALL: Mutex<Vec<&'static StubInfo>> = Mutex::new(Vec::new());
static ARENA: Mutex<(usize, usize)> = Mutex::new((0, 0)); // (next, end)
static BY_KEY: OnceLock<Mutex<HashMap<String, (usize, &'static StubInfo)>>> = OnceLock::new();
/// (first_ord, last_ord, first_r8, first_r9, last_r8, last_r9) of every thunk-called stub,
/// keyed by info address. The ordinal pair travels with the args so a caller that grabs its
/// ordinal before another but enters the table after it still records its args as first/last.
static ARGS: OnceLock<Mutex<HashMap<usize, (u64, u64, u64, u64, u64, u64)>>> = OnceLock::new();

/// Test-only injection point: blocks the ordinal-holding caller between fetch_add and the
/// side-table update so a two-thread fixture can force the first caller into the table second.
/// Only the caller whose info matches the armed key waits; others proceed. Consumed (taken)
/// on use; None in release builds.
#[cfg(test)]
static BEFORE_TABLE_UPDATE: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>, usize)>> =
    Mutex::new(None);

#[cfg(test)]
fn arm_before_table_update(key: usize) -> (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>) {
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let (arrived, arrive_rx) = std::sync::mpsc::channel();
    *BEFORE_TABLE_UPDATE.lock().unwrap() = Some((arrived, resume_rx, key));
    (resume_tx, arrive_rx)
}

#[cfg(test)]
fn pause_before_table_update(info: &StubInfo) {
    let mut guard = BEFORE_TABLE_UPDATE.lock().unwrap();
    if let Some((_, _, armed_key)) = guard.as_ref() {
        if (info as *const StubInfo as usize) == *armed_key {
            let Some((arrived, resume, _)) = guard.take() else { return };
            drop(guard);
            let _ = arrived.send(());
            let _ = resume.recv();
        }
    }
}

const THUNK_SIZE: usize = 48;
const ARENA_CHUNK: usize = 64 * 1024;

/// Count a call, log the first one, run the hook.
pub fn record_call(info: &StubInfo, caller: usize) {
    let n = info.calls.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        info.first_caller.store(caller, Ordering::Relaxed);
        crate::xlog_warn!(
            "[STUB] {}!{} first called from {} → returning {:#x}",
            info.dll,
            info.name,
            crate::modules::describe_address(caller),
            info.ret
        );
    }
    let h = info.hook.load(Ordering::Acquire);
    if h != 0 {
        let f: StubHook = unsafe { std::mem::transmute(h) };
        f(info, caller);
    }
}

// Win64 ABI: the thunk passes `info` in RCX and the caller in RDX; R8/R9 pass
// through untouched from the guest's Win64 convention, so they are the real
// 3rd/4th stub args on both ABIs.
unsafe extern "win64" fn stub_report(info: *const StubInfo, caller: usize, r8: u64, r9: u64) -> u64 {
    let info = &*info;
    record_call_with_args(info, caller, r8, r9);
    info.ret
}

/// Count a call, log the first one, run the hook. `r8`/`r9` are the 3rd/4th Win64 argument
/// registers of the stub call (recorded in the side table keyed by `info`).
pub fn record_call_with_args(info: &StubInfo, caller: usize, r8: u64, r9: u64) {
    let n = info.calls.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    pause_before_table_update(info);
    // Hold the ARGS mutex across the table update so the args recorded first/last always
    // match the ordinal implied by their label. The first-call log and hook stay outside the mutex.
    {
        let key = info as *const StubInfo as usize;
        let mut g = ARGS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
        let entry = g.entry(key).or_insert((n, n, r8, r9, r8, r9));
        if n < entry.0 {
            // an earlier ordinal arrived late: this is the first call
            entry.0 = n;
            entry.2 = r8;
            entry.3 = r9;
        }
        if n > entry.1 {
            // a later ordinal arrived (last)
            entry.1 = n;
            entry.4 = r8;
            entry.5 = r9;
        }
    }
    if n == 0 {
        info.first_caller.store(caller, Ordering::Relaxed);
        crate::xlog_warn!(
            "[STUB] {}!{} first called from {} → returning {:#x}",
            info.dll,
            info.name,
            crate::modules::describe_address(caller),
            info.ret
        );
    }
    let h = info.hook.load(Ordering::Acquire);
    if h != 0 {
        let f: StubHook = unsafe { std::mem::transmute(h) };
        f(info, caller);
    }
}

/// (first_r8, first_r9, last_r8, last_r9) recorded for `info`, if any call went through
/// a thunk (or `record_call_with_args`).
pub fn stub_args(info: &'static StubInfo) -> Option<(u64, u64, u64, u64)> {
    let key = info as *const StubInfo as usize;
    let g = ARGS.get()?.lock().ok()?;
    let (_, _, fr8, fr9, lr8, lr9) = g.get(&key).copied()?;
    Some((fr8, fr9, lr8, lr9))
}

/// A StubInfo that is not backed by a stub thunk (used by D3D12.X slot guards).
pub fn new_info(dll: &str, name: &str, ret: u64) -> &'static StubInfo {
    let info: &'static StubInfo = Box::leak(Box::new(StubInfo {
        dll: dll.to_string(),
        name: name.to_string(),
        ret,
        calls: AtomicU64::new(0),
        first_caller: AtomicUsize::new(0),
        hook: AtomicUsize::new(0),
    }));
    ALL.lock().unwrap().push(info);
    info
}

/// Allocate `size` bytes of executable memory for generated thunks.
pub fn alloc_code(size: usize) -> *mut u8 {
    let size = (size + 15) & !15;
    let mut a = ARENA.lock().unwrap();
    if a.0 == 0 || a.0 + size > a.1 {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ARENA_CHUNK,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        } as usize;
        assert!(p != 0 && p != libc::MAP_FAILED as usize, "mmap for stub arena failed");
        *a = (p, p + ARENA_CHUNK);
    }
    let p = a.0;
    a.0 += size;
    p as *mut u8
}

fn alloc_thunk() -> *mut u8 {
    alloc_code(THUNK_SIZE)
}

pub fn info_of_stub(addr: usize) -> Option<&'static StubInfo> {
    let map = BY_KEY.get()?;
    let g = map.lock().ok()?;
    g.values().find(|&&(a, _)| a == addr).map(|&(_, info)| info)
}

/// Create (or reuse) a stub for `dll!name`. Returns the callable address.
pub fn make_stub(dll: &str, name: &str, ret: u64) -> usize {
    make_stub_info(dll, name, ret).0
}

/// Like `make_stub`, also returning the StubInfo (to attach a hook).
pub fn make_stub_info(dll: &str, name: &str, ret: u64) -> (usize, &'static StubInfo) {
    let key = format!("{}!{}", dll.to_ascii_lowercase(), name);
    let map = BY_KEY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = map.lock().unwrap();
    if let Some(&entry) = g.get(&key) {
        return entry;
    }
    let info: &'static StubInfo = Box::leak(Box::new(StubInfo {
        dll: dll.to_string(),
        name: name.to_string(),
        ret,
        calls: AtomicU64::new(0),
        first_caller: AtomicUsize::new(0),
        hook: AtomicUsize::new(0),
    }));
    let p = alloc_thunk();
    let mut code: Vec<u8> = Vec::with_capacity(THUNK_SIZE);
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    code.extend_from_slice(&(info as *const StubInfo as u64).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x54, 0x24, 0x28]); // mov rdx, [rsp+0x28]
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&(stub_report as usize as u64).to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    code.push(0xC3); // ret
    debug_assert!(code.len() <= THUNK_SIZE);
    // SAFETY: `p` is freshly allocated with at least `THUNK_SIZE >= code.len()` bytes of RWX memory.
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), p, code.len());
    }
    ALL.lock().unwrap().push(info);
    let entry = (p as usize, info);
    g.insert(key, entry);
    entry
}

pub fn all_stubs() -> Vec<&'static StubInfo> {
    ALL.lock().map(|v| v.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Stub database: default return values + ordinal names
// ---------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct RawDb {
    #[serde(default)]
    module_defaults: HashMap<String, String>,
    #[serde(default)]
    functions: HashMap<String, String>,
    #[serde(default)]
    ordinals: HashMap<String, HashMap<String, String>>,
}

pub struct StubDb {
    module_defaults: HashMap<String, u64>,
    functions: HashMap<String, u64>,
    ordinals: HashMap<String, HashMap<u16, String>>,
}

static DB: OnceLock<StubDb> = OnceLock::new();

const BUILTIN_DB: &str = include_str!("../data/stubdb.toml");

impl StubDb {
    fn parse(text: &str, overrides: &std::collections::BTreeMap<String, String>) -> StubDb {
        let raw: RawDb = toml::from_str(text).unwrap_or_default();
        let mut functions: HashMap<String, u64> = raw
            .functions
            .iter()
            .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), parse_ret(v)?)))
            .collect();
        for (k, v) in overrides {
            if let Some(r) = parse_ret(v) {
                functions.insert(k.to_ascii_lowercase(), r);
            }
        }
        StubDb {
            module_defaults: raw
                .module_defaults
                .iter()
                .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), parse_ret(v)?)))
                .collect(),
            functions,
            ordinals: raw
                .ordinals
                .into_iter()
                .map(|(dll, m)| {
                    (
                        dll.to_ascii_lowercase(),
                        m.into_iter().filter_map(|(o, n)| Some((o.parse().ok()?, n))).collect(),
                    )
                })
                .collect(),
        }
    }

    pub fn return_for(&self, dll: &str, name: &str) -> u64 {
        self.return_for_with_fallback(dll, None, name)
    }

    /// Look up the stub return value for `dll!name`, falling back to `fallback_dll` (e.g. a
    /// canonical HLE module name when `dll` is a routed alias) if `dll` has no explicit override
    /// or module default.
    pub fn return_for_with_fallback(&self, dll: &str, fallback_dll: Option<&str>, name: &str) -> u64 {
        let dll = dll.to_ascii_lowercase();
        let fallback = fallback_dll.map(str::to_ascii_lowercase);
        let n = name.to_ascii_lowercase();
        if let Some(&r) = self.functions.get(&format!("{dll}!{n}")) {
            return r;
        }
        if let Some(fb) = fallback.as_deref().filter(|&fb| fb != dll) {
            if let Some(&r) = self.functions.get(&format!("{fb}!{n}")) {
                return r;
            }
        }
        if let Some(&r) = self.functions.get(&format!("*!{n}")) {
            return r;
        }
        if let Some(&r) = self.module_defaults.get(&dll) {
            return r;
        }
        if let Some(fb) = fallback.as_deref() {
            if let Some(&r) = self.module_defaults.get(fb) {
                return r;
            }
        }
        0
    }

    pub fn ordinal_name(&self, dll: &str, ordinal: u16) -> Option<&str> {
        self.ordinals.get(&dll.to_ascii_lowercase())?.get(&ordinal).map(String::as_str)
    }
}

pub fn init_db(overrides: &std::collections::BTreeMap<String, String>) {
    let _ = DB.set(StubDb::parse(BUILTIN_DB, overrides));
}

pub fn db() -> &'static StubDb {
    DB.get_or_init(|| StubDb::parse(BUILTIN_DB, &Default::default()))
}

#[cfg(test)]
#[path = "stubs_args_tests.rs"]
mod stubs_args_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_configured_value_and_counts_calls() {
        let addr = make_stub("unit.dll", "Answer", 42);
        assert_ne!(addr, 0);
        // The game calls with the Windows x64 ABI; exercise the thunk the same way.
        let f: unsafe extern "win64" fn(usize, usize, usize, usize, usize) -> u64 =
            unsafe { std::mem::transmute(addr) };
        assert_eq!(unsafe { f(1, 2, 3, 4, 5) }, 42);
        assert_eq!(unsafe { f(0, 0, 0, 0, 0) }, 42);
        let info = all_stubs().into_iter().find(|s| s.name == "Answer").unwrap();
        assert_eq!(info.calls.load(Ordering::Relaxed), 2);
        assert_ne!(info.first_caller.load(Ordering::Relaxed), 0);
        // same key → same thunk
        assert_eq!(make_stub("UNIT.dll", "Answer", 1), addr);
    }

    #[test]
    fn thunk_is_executable_and_unique_per_name() {
        let (a, _) = make_stub_info("unit.dll", "LinuxExecProbeA", 0);
        let (b, _) = make_stub_info("unit.dll", "LinuxExecProbeB", 0);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b);
        let f: unsafe extern "win64" fn() -> u64 = unsafe { std::mem::transmute(a) };
        assert_eq!(unsafe { f() }, 0);
        assert!(info_of_stub(a).is_some());
    }

    #[test]
    fn builtin_db_parses_with_overrides() {
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("kernelx.dll!Foo".to_string(), "7".to_string());
        ov.insert("xgameplatform.dll!PerAlias".to_string(), "0".to_string());
        ov.insert("xgameruntime.dll!CanonicalFn".to_string(), "1".to_string());
        let db = StubDb::parse(BUILTIN_DB, &ov);
        assert_eq!(db.return_for("kernelx.dll", "Foo"), 7);
        assert_eq!(db.return_for("xgameruntime.dll", "XSomethingUnknown"), 0x8000_4001);
        assert_eq!(db.return_for("kernelx.dll", "SomethingUnknown"), 0);
        assert_eq!(db.ordinal_name("d3d12_x.dll", 7), Some("D3D12CreateDevice"));
        // Routed HLE aliases inherit canonical module defaults while preserving per-alias overrides.
        assert_eq!(db.return_for_with_fallback("xgameplatform.dll", Some("xgameruntime.dll"), "XUnknown"), 0x8000_4001);
        assert_eq!(db.return_for_with_fallback("d3d12x.dll", Some("d3d12_xs.dll"), "D3D12Unknown"), 0x8000_4001);
        assert_eq!(db.return_for_with_fallback("xgameplatform.dll", Some("xgameruntime.dll"), "PerAlias"), 0);
        assert_eq!(db.return_for_with_fallback("xgameplatform.dll", Some("xgameruntime.dll"), "CanonicalFn"), 1);
        assert_eq!(db.return_for_with_fallback("xgameplatform.dll", Some("xgameruntime.dll"), "XGameUiShowErrorDialogAsync"), 0x8000_4004);
        assert_eq!(db.return_for_with_fallback("advapi32legacy.dll", Some("kernelx.dll"), "SomethingUnknown"), 0);
    }

    #[test]
    fn make_stub_info_is_race_free_under_concurrency() {
        use std::sync::{Arc, Barrier};

        const THREADS: usize = 8;
        const ITERS: usize = 50;

        for iter in 0..ITERS {
            let func = format!("ConcurrentRaceFn_{iter}");
            let barrier = Arc::new(Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let barrier = Arc::clone(&barrier);
                    let func = func.clone();
                    let dll = if t % 2 == 0 { "RaceStub.dll" } else { "racestub.dll" };
                    std::thread::spawn(move || {
                        barrier.wait();
                        let (addr, info) = make_stub_info(dll, &func, 0x8000_4001);
                        (addr, info as *const StubInfo as usize)
                    })
                })
                .collect();

            let results: Vec<(usize, usize)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let first = results[0];
            assert_ne!(first.0, 0);
            for &r in &results[1..] {
                assert_eq!(r, first, "iter {iter}: concurrent callers received different stubs");
            }
            assert_eq!(
                info_of_stub(first.0).map(|i| i as *const StubInfo as usize),
                Some(first.1)
            );
            let count = all_stubs()
                .into_iter()
                .filter(|s| s.dll.eq_ignore_ascii_case("racestub.dll") && s.name == func)
                .count();
            assert_eq!(count, 1, "iter {iter}: duplicate StubInfo pushed to ALL");
        }
    }
}
