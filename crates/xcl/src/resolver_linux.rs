//! Linux import resolver (Phase C step 10a): same routes as Windows, stubs not nulls.
//!
//! Routing (`hle::registry`) is identical to Windows. The Linux differences:
//! * no `LoadLibrary`/`GetProcAddress` — the system leg always misses;
//! * an HLE export address `0` is `Stub` metadata, never a callable address;
//! * unresolved imports get a safe executable auto-stub thunk (`stubs::make_stub`)
//!   whose address is written into the IAT.
//!
//! Real HLE implementations and game-DLL auto-loading land in later slices
//! (steps 10b–10d); this slice only guarantees a logged stub, never a null call.
// LINUX-TODO: Phase C steps 10b–10d — real HLE bodies, kernel-family libc fallbacks,
// game-DLL `load_game_dll` + DllMain/TLS install, `XCL_TRACE_IMPORTS` tracer.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde::Serialize;

use crate::hle::{registry, HleModule, Route};
use crate::modules;
use crate::pe::{ExportTarget, ImportName};
use crate::stubs;
use crate::util::dll_key;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "detail")]
pub enum Source {
    /// Implemented by the HLE module the DLL routes to.
    Hle(String),
    /// Implemented by a different HLE module (cross lookup).
    HleOther(String),
    /// Real Windows DLL.
    System(String),
    /// DLL from the game folder, mapped by XCL.
    GameDll(String),
    Stub,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportRecord {
    pub importer: String,
    pub dll: String,
    pub function: String,
    pub delay: bool,
    pub source: Source,
}

static RECORDS: Mutex<Vec<ImportRecord>> = Mutex::new(Vec::new());
static SEARCH_DIRS: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

pub fn set_search_dirs(dirs: Vec<PathBuf>) {
    *SEARCH_DIRS.write().unwrap() = dirs;
}

pub fn records() -> Vec<ImportRecord> {
    RECORDS.lock().map(|v| v.clone()).unwrap_or_default()
}

fn fname(what: &ImportName) -> String {
    what.to_string()
}

/// No system modules on Linux: `LoadLibrary` has no twin here.
pub fn system_module(_dll: &str) -> Option<usize> {
    None
}

pub fn system_proc_in(_h: usize, _what: &ImportName) -> Option<usize> {
    None
}

pub fn system_proc(_dll: &str, _what: &ImportName) -> Option<usize> {
    None
}

fn hle_name_for<'a>(m: &'a HleModule, dll: &str, what: &'a ImportName) -> Option<&'a str> {
    match what {
        ImportName::Name { name, .. } => Some(name.as_str()),
        ImportName::Ordinal(o) => m
            .ordinals
            .get(o)
            .copied()
            .or_else(|| stubs::db().ordinal_name(dll, *o)),
    }
}

/// HLE lookup. An export address `0` is Stub metadata: it never counts as implemented.
fn resolve_hle(m: &HleModule, key: &str, what: &ImportName) -> Option<(usize, Source)> {
    let fallback_what = if let Some(name) = hle_name_for(m, key, what) {
        if let Some(&a) = m.exports.get(name) {
            if a != 0 {
                return Some((a, Source::Hle(m.name.to_string())));
            }
            // Address 0: fall through to the auto-stub below.
        }
        if let Some((other, a)) = registry().find_any(name) {
            if a != 0 {
                return Some((a, Source::HleOther(other.name.to_string())));
            }
        }
        Some(ImportName::Name { hint: 0, name: name.to_string() })
    } else {
        None
    };
    let query = fallback_what.as_ref().unwrap_or(what);
    let ordinal_only = fallback_what.is_none();
    for &c in m.fallback {
        let c = if c == crate::hle::SELF {
            if key == m.name {
                continue;
            }
            key
        } else {
            c
        };
        if ordinal_only && c != key {
            continue;
        }
        if let Some(a) = system_proc(c, query) {
            return Some((a, Source::System(c.to_string())));
        }
    }
    None
}

/// Find a DLL shipped with the game (case-insensitive file name match).
pub fn find_game_dll(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() && p.is_file() {
        return Some(p.to_path_buf());
    }
    let key = dll_key(name);
    for dir in SEARCH_DIRS.read().ok()?.iter() {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().to_ascii_lowercase() == key && e.path().is_file() {
                    return Some(e.path());
                }
            }
        }
    }
    None
}

pub fn resolve_export_of_mapped(m: &modules::MappedModule, what: &ImportName, depth: u32) -> Option<usize> {
    resolve_export_of_mapped_inner(m, what, depth, false)
}

fn resolve_export_of_mapped_inner(
    m: &modules::MappedModule,
    what: &ImportName,
    depth: u32,
    allow_stub: bool,
) -> Option<usize> {
    let t = match what {
        ImportName::Name { name, .. } => m.exports.by_name.get(name)?,
        ImportName::Ordinal(o) => m.exports.by_ordinal.get(&(*o as u32))?,
    };
    match t {
        ExportTarget::Rva(r) => Some(m.base + *r as usize),
        ExportTarget::Forwarder(f) => {
            if depth > 8 {
                return None;
            }
            let (dll, func) = f.rsplit_once('.')?;
            if dll.is_empty() || func.is_empty() {
                return None;
            }
            let what = match func.strip_prefix('#') {
                Some(n) => ImportName::Ordinal(n.parse().ok()?),
                None => ImportName::Name { hint: 0, name: func.to_string() },
            };
            let target_key = if dll.to_ascii_lowercase().ends_with(".dll") {
                dll_key(dll)
            } else {
                dll_key(&format!("{dll}.dll"))
            };
            resolve_inner_opt(&target_key, &what, depth + 1, allow_stub).map(|(a, _)| a)
        }
    }
}

fn stub_for(key: &str, fallback_dll: Option<&str>, what: &ImportName) -> (usize, Source) {
    let name = fname(what);
    let ret = stubs::db().return_for_with_fallback(key, fallback_dll, &name);
    (stubs::make_stub(key, &name, ret), Source::Stub)
}

fn resolve_inner(key: &str, what: &ImportName, depth: u32) -> (usize, Source) {
    resolve_inner_opt(key, what, depth, true).expect("allow_stub=true always resolves")
}

fn resolve_inner_opt(key: &str, what: &ImportName, depth: u32, allow_stub: bool) -> Option<(usize, Source)> {
    let reg = registry();
    match reg.route(key) {
        Some(Route::Stub) => {
            return allow_stub.then(|| {
                modules::hle_handle(key);
                stub_for(key, None, what)
            });
        }
        Some(Route::System) => {
            return match system_proc(key, what) {
                Some(a) => Some((a, Source::System(key.to_string()))),
                None => allow_stub.then(|| stub_for(key, None, what)),
            }
        }
        Some(Route::Hle(i)) => {
            let m = &reg.modules[*i];
            let fallback = (m.name != key).then_some(m.name);
            return resolve_hle(m, key, what).or_else(|| allow_stub.then(|| stub_for(key, fallback, what)));
        }
        None => {}
    }
    // A game DLL already mapped by the loader.
    if let Some(m) = modules::find_mapped_by_name(key) {
        if let Some(a) = resolve_export_of_mapped_inner(&m, what, depth, allow_stub) {
            return Some((a, Source::GameDll(key.to_string())));
        }
        return allow_stub.then(|| stub_for(key, None, what));
    }
    // LINUX-TODO: Phase C step 10d — auto-load a game DLL from `find_game_dll` through
    // the game-DLL load path (DllMain/TLS install). Until then an unmapped game DLL is
    // still a safe stub, never a null.
    if find_game_dll(key).is_some() {
        return allow_stub.then(|| stub_for(key, None, what));
    }
    // No system DLLs on Linux.
    if let ImportName::Name { name, .. } = what {
        if let Some((m, a)) = reg.find_any(name) {
            if a != 0 {
                return Some((a, Source::HleOther(m.name.to_string())));
            }
        }
    }
    allow_stub.then(|| stub_for(key, None, what))
}

/// Resolve one static/delay import and record the outcome for the report.
pub fn resolve_import(dll: &str, what: &ImportName, importer: &str, delay: bool) -> usize {
    let key = dll_key(dll);
    let (addr, source) = resolve_inner(&key, what, 0);
    debug_assert_ne!(addr, 0, "{key}!{what}: resolver must never return null");
    crate::xlog_debug!("import {importer}: {key}!{what} → {source:?}");
    RECORDS.lock().unwrap().push(ImportRecord {
        importer: importer.to_string(),
        dll: key,
        function: fname(what),
        delay,
        source,
    });
    addr
}

/// Dry-run classification used by `xcl inspect` (does not map game DLLs).
pub fn classify(dll: &str, what: &ImportName) -> Source {
    let key = dll_key(dll);
    let reg = registry();
    match reg.route(&key) {
        Some(Route::Stub) => Source::Stub,
        Some(Route::System) => {
            if system_proc(&key, what).is_some() {
                Source::System(key)
            } else {
                Source::Stub
            }
        }
        Some(Route::Hle(i)) => resolve_hle(&reg.modules[*i], &key, what).map(|x| x.1).unwrap_or(Source::Stub),
        None => {
            if find_game_dll(&key).is_some() {
                return Source::GameDll(key);
            }
            if system_proc(&key, what).is_some() {
                return Source::System(key);
            }
            if let ImportName::Name { name, .. } = what {
                if let Some((m, a)) = reg.find_any(name) {
                    if a != 0 {
                        return Source::HleOther(m.name.to_string());
                    }
                }
            }
            Source::Stub
        }
    }
}

// ---------------------------------------------------------------------------------------
// Runtime LoadLibrary / GetProcAddress semantics (used by kernelx shims)
// ---------------------------------------------------------------------------------------

/// LoadLibrary as seen by the game. Returns an HMODULE-like value or None.
pub fn load_library(name: &str) -> Option<usize> {
    let key = dll_key(name);
    let reg = registry();
    match reg.route(&key) {
        Some(Route::Hle(i)) => {
            let m = &reg.modules[*i];
            // Kernel-family aliases hand out the real module on Windows; on Linux they
            // are pseudo handles like every other HLE module.
            return Some(modules::hle_handle(m.name));
        }
        Some(Route::Stub) => return Some(modules::hle_handle(&key)),
        Some(Route::System) => return system_module(name),
        None => {}
    }
    if let Some(m) = modules::find_mapped_by_name(&key) {
        return Some(m.base);
    }
    // LINUX-TODO: Phase C step 10d — load the game DLL from the VFS/game folder here.
    find_game_dll(name).and(system_module(name))
}

/// GetProcAddress as seen by the game. Missing functions return None (no stub) so
/// that optional-API probing works.
pub fn get_proc_address(h: usize, what: &ImportName) -> Option<usize> {
    let reg = registry();
    if let Some(name) = modules::hle_name_by_handle(h) {
        let r = match reg.module_by_name(&name).or_else(|| reg.hle_for(&name)) {
            Some(m) => resolve_hle(m, &name, what).map(|x| x.0),
            None => None,
        };
        if r.is_none() {
            crate::xlog_warn!("GetProcAddress({name}, {what}) → NULL (not implemented)");
        }
        return r;
    }
    if let Some(m) = modules::find_mapped_by_base(h) {
        return resolve_export_of_mapped(&m, what, 0);
    }
    // No real-module lookup on Linux.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> ImportName {
        ImportName::Name { hint: 0, name: s.into() }
    }

    #[test]
    fn hle_routes_are_shared_with_windows() {
        let reg = registry();
        let at = |k: &str| reg.hle_for(k).map(|m| m.name);
        assert_eq!(at("kernelx.dll"), Some(crate::hle::KERNELX));
        assert_eq!(at("kernel32.dll"), Some(crate::hle::KERNELX));
        assert_eq!(at("api-ms-win-core-file-l1-1-0.dll"), Some(crate::hle::KERNELX));
        assert_eq!(at("xgameruntime.dll"), Some(crate::hle::XGAMERUNTIME));
        assert_eq!(at("d3d12_xs.dll"), Some(crate::hle::D3D12X));
        assert_eq!(at("xaudio2_9.dll"), Some(crate::hle::XAUDIO2));
        assert_eq!(at("mfplat.dll"), Some(crate::hle::MFPLAT));
        assert_eq!(at("ucrtbase.dll"), None);
    }

    #[test]
    fn routed_imports_resolve_to_callable_stubs() {
        // Every import routed to an HLE module is Stub metadata (address 0) but resolves
        // to a callable auto-stub thunk, never null.
        for (dll, sym) in [
            ("kernel32.dll", "CreateFileW"),
            ("kernelx.dll", "XMemAlloc"),
            ("d3d12_xs.dll", "D3D12XboxCreateDevice"),
            ("xgameruntime.dll", "XGameRuntimeInitialize"),
            ("xaudio2_9.dll", "XAudio2Create"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert_eq!(src, Source::Stub, "{dll}!{sym}");
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            let info = stubs::info_of_stub(addr).expect("stub registered");
            assert!(info.dll.eq_ignore_ascii_case(dll), "{dll}!{sym}");
        }
    }

    #[test]
    fn stub_thunk_calls_log_and_return() {
        let (addr, src) = resolve_inner("kernelx.dll", &name("LinuxProbeThunk"), 0);
        assert_eq!(src, Source::Stub);
        assert_ne!(addr, 0);
        // Call through the thunk with the Windows x64 ABI, as mapped game code does.
        let f: unsafe extern "win64" fn() -> u64 = unsafe { std::mem::transmute(addr) };
        let ret = unsafe { f() };
        assert_eq!(ret, stubs::db().return_for("kernelx.dll", "LinuxProbeThunk"));
        let info = stubs::info_of_stub(addr).unwrap();
        assert_eq!(info.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn hle_alias_stubs_inherit_canonical_module_default() {
        let e_notimpl = crate::util::E_NOTIMPL as u32 as u64;
        for (dll, sym) in [
            ("xgameplatform.dll", "XUnimplementedAliasProbe"),
            ("xgameruntime.thunks.dll", "XUnimplementedThunkProbe"),
            ("d3d12x.dll", "D3D12UnimplementedAliasProbe"),
        ] {
            let (addr, src) = resolve_inner(dll, &name(sym), 0);
            assert_eq!(src, Source::Stub, "{dll}!{sym}");
            let info = stubs::info_of_stub(addr).expect("stub info registered");
            assert_eq!(info.ret, e_notimpl, "{dll}!{sym} should inherit E_NOTIMPL");
            assert!(info.dll.eq_ignore_ascii_case(dll));
            let f: unsafe extern "win64" fn() -> u64 = unsafe { std::mem::transmute(addr) };
            assert_eq!(unsafe { f() }, e_notimpl, "{dll}!{sym} thunk return");
        }
        let (addr, src) = resolve_inner("advapi32legacy.dll", &name("UnimplementedKernelAliasProbe"), 0);
        assert_eq!(src, Source::Stub);
        assert_eq!(stubs::info_of_stub(addr).unwrap().ret, 0);
    }

    #[test]
    fn ordinal_and_unknown_imports_are_stubs_never_system() {
        let ord1 = ImportName::Ordinal(1);
        let (addr, src) = resolve_inner("kernel32.dll", &ord1, 0);
        assert_eq!(src, Source::Stub);
        assert_ne!(addr, 0);
        // Unknown DLLs and cross-module names are stubs on Linux (no LoadLibrary).
        let (addr, src) = resolve_inner("mystery.dll", &name("Nope"), 0);
        assert_eq!(src, Source::Stub);
        assert_ne!(addr, 0);
        assert!(system_proc("kernel32.dll", &name("CreateFileW")).is_none());
    }

    #[test]
    fn load_library_and_get_proc_address_contract() {
        let h = load_library("kernelx.dll").expect("HLE pseudo handle");
        assert!(modules::hle_name_by_handle(h).is_some());
        // HLE exports are Stub metadata: GetProcAddress returns None (no stub synthesis).
        assert_eq!(get_proc_address(h, &name("XMemAlloc")), None);
        // Unknown handles have no lookup.
        assert_eq!(get_proc_address(0x1234_5000, &name("XMemAlloc")), None);
        // Stub routes still hand out a pseudo handle.
        assert!(load_library("mystery.dll").is_none_or(|h| h != 0));
    }

    #[test]
    fn classify_never_claims_hle_on_linux() {
        assert_eq!(classify("kernel32.dll", &name("CreateFileW")), Source::Stub);
        assert_eq!(classify("d3d12_xs.dll", &name("D3D12XboxCreateDevice")), Source::Stub);
        assert_eq!(classify("ucrtbase.dll", &name("malloc")), Source::Stub);
    }

    #[test]
    fn mapped_forwarders_preserve_get_proc_address_contract() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        // Reserve two disjoint mappings so test bases never collide with real images.
        let chunk = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                0x20000,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(chunk, libc::MAP_FAILED);
        let target_base = chunk as usize;
        let proxy_base = target_base + 0x10000;

        let mut target_exports = crate::pe::OwnedExports::default();
        target_exports.by_name.insert("RealExport".into(), ExportTarget::Rva(0x1234));
        let target_mod = Arc::new(modules::MappedModule {
            name: "dotted.fwdtarget.dll".into(),
            path: PathBuf::from("dotted.fwdtarget.dll"),
            base: target_base,
            size: 0x10000,
            entry_rva: 0,
            is_dll: true,
            exports: target_exports,
            tls_callbacks: Mutex::new(Vec::new()),
            no_thread_calls: AtomicBool::new(false),
            initialized: AtomicBool::new(true),
            stack_reserve: 0,
        });
        modules::register_mapped(target_mod);

        let mut proxy_exports = crate::pe::OwnedExports::default();
        proxy_exports
            .by_name
            .insert("MissingFwd".into(), ExportTarget::Forwarder("nonexistent_fwd_target.MissingApi".into()));
        proxy_exports.by_name.insert(
            "MissingHleFwd".into(),
            ExportTarget::Forwarder("xgameruntime.thunks.XMissingForwardedApi".into()),
        );
        proxy_exports
            .by_name
            .insert("DottedFwd".into(), ExportTarget::Forwarder("dotted.fwdtarget.RealExport".into()));
        let proxy_mod = Arc::new(modules::MappedModule {
            name: "fwd_proxy_unit.dll".into(),
            path: PathBuf::from("fwd_proxy_unit.dll"),
            base: proxy_base,
            size: 0x10000,
            entry_rva: 0,
            is_dll: true,
            exports: proxy_exports,
            tls_callbacks: Mutex::new(Vec::new()),
            no_thread_calls: AtomicBool::new(false),
            initialized: AtomicBool::new(true),
            stack_reserve: 0,
        });
        modules::register_mapped(Arc::clone(&proxy_mod));

        assert_eq!(get_proc_address(proxy_base, &name("MissingFwd")), None);
        assert_eq!(get_proc_address(proxy_base, &name("MissingHleFwd")), None);
        assert_eq!(get_proc_address(proxy_base, &name("DottedFwd")), Some(target_base + 0x1234));

        let (stub_addr, _) = resolve_inner("fwd_proxy_unit.dll", &name("MissingHleFwd"), 0);
        assert_eq!(
            stubs::info_of_stub(stub_addr).map(|i| i.ret),
            Some(crate::util::E_NOTIMPL as u32 as u64)
        );
        unsafe {
            libc::munmap(chunk, 0x20000);
        }
    }
}
