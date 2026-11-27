//! Linux import resolver (Phase C steps 10a–10d): same routes as Windows, stubs not nulls.
//!
//! Routing (`hle::registry`) is identical to Windows. The Linux differences:
//! * no `LoadLibrary`/`GetProcAddress` — the system leg always misses;
//! * an HLE export address `0` is `Stub` metadata, never a callable address;
//! * unresolved imports get a safe executable auto-stub thunk (`stubs::make_stub`)
//!   whose address is written into the IAT.
//!
//! Steps 10b–10c contribute real HLE bodies; step 10d auto-loads unmapped game DLLs
//! through `loader::load_game_dll` (DllMain/TLS install) before falling back to a stub.
// LINUX-TODO: Phase C step 10e+ — `XCL_TRACE_IMPORTS` tracer; static IAT fill for the
//! main image still waits on the launcher path.

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
    // Step 10d: auto-load an unmapped game DLL through the game-DLL load path
    // (DllMain/TLS install), then resolve against it. A load failure stays a safe
    // stub, never a null.
    if let Some(path) = find_game_dll(key) {
        if let Ok(m) = crate::loader::load_game_dll(&path) {
            if let Some(a) = resolve_export_of_mapped_inner(&m, what, depth, allow_stub) {
                return Some((a, Source::GameDll(key.to_string())));
            }
        }
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
    // Step 10d: load the game DLL through the game-DLL load path. A load failure
    // returns None so optional-API probing still works.
    if let Some(path) = find_game_dll(name) {
        return crate::loader::load_game_dll(&path).ok().map(|m| m.base);
    }
    None
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
        // to a callable auto-stub thunk, never null. Step-10c file APIs are HLE now, so
        // they are covered by `kernelx_core_imports_resolve_to_hle` instead.
        // Steps 11r/12f/12g made d3d12_xs/mfplat/xcrdapi HLE, so the stub probe uses
        // acphal (still Stub).
        for (dll, sym) in [
            ("acphal.dll", "AcpHalCreate"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert_eq!(src, Source::Stub, "{dll}!{sym}");
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            let info = stubs::info_of_stub(addr).expect("stub registered");
            assert!(info.dll.eq_ignore_ascii_case(dll), "{dll}!{sym}");
        }
    }

    #[test]
    fn kernelx_core_imports_resolve_to_hle() {
        // Steps 10b–10c: memory/thread/sync/time/file imports are real HLE, callable directly.
        for (dll, sym) in [
            ("kernel32.dll", "VirtualAlloc"),
            ("api-ms-win-core-memory-l1-1-0.dll", "VirtualProtect"),
            ("api-ms-win-core-processthreads-l1-1-0.dll", "CreateThread"),
            ("kernel32.dll", "ExitProcess"),
            ("kernel32.dll", "TerminateProcess"),
            ("api-ms-win-core-synch-l1-1-0.dll", "WaitForSingleObject"),
            ("api-ms-win-core-synch-l1-1-0.dll", "CreateSemaphoreExW"),
            ("api-ms-win-core-synch-l1-1-0.dll", "CreateSemaphoreW"),
            ("api-ms-win-core-synch-l1-1-0.dll", "ReleaseSemaphore"),
            ("api-ms-win-core-synch-l1-1-0.dll", "InitializeConditionVariable"),
            ("api-ms-win-core-synch-l1-1-0.dll", "SleepConditionVariableCS"),
            ("api-ms-win-core-synch-l1-1-0.dll", "WakeConditionVariable"),
            ("api-ms-win-core-synch-l1-1-0.dll", "WakeAllConditionVariable"),
            ("api-ms-win-core-sysinfo-l1-1-0.dll", "GetSystemInfo"),
            ("api-ms-win-core-sysinfo-l1-1-0.dll", "GetNativeSystemInfo"),
            ("api-ms-win-core-sysinfo-l1-1-0.dll", "GetLogicalProcessorInformation"),
            ("api-ms-win-core-synch-l1-1-0.dll", "EnterCriticalSection"),
            ("api-ms-win-core-profile-l1-1-0.dll", "QueryPerformanceCounter"),
            ("kernel32.dll", "CreateFileW"),
            ("api-ms-win-core-file-l1-1-0.dll", "ReadFile"),
            ("api-ms-win-core-file-l1-1-0.dll", "WriteFile"),
            ("api-ms-win-core-file-l1-1-0.dll", "DeleteFileW"),
            ("api-ms-win-core-file-l1-2-0.dll", "GetDiskFreeSpaceExW"),
            // Step 11b startup: command line, process heap and module file names.
            ("kernel32.dll", "GetCommandLineW"),
            ("kernel32.dll", "GetProcessHeap"),
            ("kernel32.dll", "HeapAlloc"),
            ("kernel32.dll", "HeapFree"),
            ("kernel32.dll", "HeapReAlloc"),
            ("kernel32.dll", "GetModuleFileNameW"),
            // Retail abort 0x2237179: CRT entry reads startup info + narrow cmdline.
            ("kernel32.dll", "GetStartupInfoW"),
            ("kernel32.dll", "_get_narrow_winmain_command_line"),
            ("api-ms-win-core-processthreads-l1-1-0.dll", "GetStartupInfoW"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_get_narrow_winmain_command_line"),
            // Step 11c startup neighbors: FLS, std handles, module handles, environment.
            ("kernel32.dll", "FlsAlloc"),
            ("kernel32.dll", "FlsSetValue"),
            ("kernel32.dll", "FlsGetValue"),
            ("kernel32.dll", "IsThreadAFiber"),
            ("kernel32.dll", "GetStdHandle"),
            ("kernel32.dll", "GetModuleHandleA"),
            ("kernel32.dll", "GetModuleHandleExW"),
            ("kernel32.dll", "GetEnvironmentVariableW"),
            // Step 11d panic-report path: exception, console, loader probes.
            ("kernel32.dll", "AddVectoredExceptionHandler"),
            ("kernel32.dll", "RemoveVectoredExceptionHandler"),
            ("kernel32.dll", "RaiseException"),
            ("kernel32.dll", "GetProcAddress"),
            ("kernel32.dll", "GetConsoleMode"),
            ("kernel32.dll", "SetConsoleMode"),
            ("kernel32.dll", "GetConsoleOutputCP"),
            ("kernel32.dll", "SetThreadStackGuarantee"),
            ("ntdll.dll", "NtWriteFile"),
            // Step 11e timer/synch: Rust sleep + parking.
            ("kernel32.dll", "CreateWaitableTimerExW"),
            ("kernel32.dll", "SetWaitableTimer"),
            ("api-ms-win-core-synch-l1-2-0.dll", "WaitOnAddress"),
            // Step 11f SEH search: `.pdata` reader plus Rtl* shims.
            ("kernel32.dll", "RtlPcToFileHeader"),
            ("kernel32.dll", "RtlLookupFunctionEntry"),
            ("kernel32.dll", "RtlVirtualUnwind"),
            ("kernel32.dll", "RtlAddFunctionTable"),
            ("kernel32.dll", "RtlCaptureContext"),
            ("kernel32.dll", "RtlUnwindEx"),
            ("kernel32.dll", "RtlRestoreContext"),
            // Step 11k module loading: dynamic LoadLibrary family.
            ("kernel32.dll", "LoadLibraryW"),
            ("kernel32.dll", "LoadLibraryA"),
            ("kernel32.dll", "LoadLibraryExW"),
            ("kernel32.dll", "LoadLibraryExA"),
            ("kernel32.dll", "LoadPackagedLibrary"),
            ("kernel32.dll", "FreeLibrary"),
            ("kernel32.dll", "DisableThreadLibraryCalls"),
            // Step-11l file reads past stubs (Rust std + CRT through the VFS).
            ("kernel32.dll", "GetFileInformationByHandle"),
            ("kernel32.dll", "GetFileInformationByHandleEx"),
            // Retail abort 0x2237320: the title reads handle timestamps.
            ("kernel32.dll", "GetFileTime"),
            ("api-ms-win-core-file-l1-1-0.dll", "GetFileTime"),
            ("ntdll.dll", "NtReadFile"),
            ("ntdll.dll", "RtlNtStatusToDosError"),
            ("kernel32.dll", "GetCurrentDirectoryW"),
            ("kernel32.dll", "GetFullPathNameW"),
            // Step 11j XMem: heap, graphics UMA pool and physical pages.
            ("kernelx.dll", "XMemAlloc"),
            ("kernelx.dll", "XMemAllocDefault"),
            ("kernelx.dll", "XMemFree"),
            ("kernelx.dll", "XMemVirtualAlloc"),
            ("kernelx.dll", "XMemAllocatePhysicalPages"),
            ("kernelx.dll", "XMemMapPhysicalPages"),
            ("kernelx.dll", "XMemFreePhysicalPages"),
            ("xmem.dll", "XMemAllocatePhysicalPages"),
            // Step 11o LSA probe backing advapi32legacy dynamic lookups.
            ("advapi32legacy.dll", "LsaOpenPolicy"),
            // Step 12e message formatting: the title-called kernel32 stub.
            ("kernel32.dll", "FormatMessageW"),
            ("kernel32.dll", "FormatMessageA"),
            ("api-ms-win-core-localization-l1-2-0.dll", "FormatMessageW"),
            // Retail abort 0x2237179: narrow<->wide dump-path conversion (UTF-8 on Linux).
            ("kernel32.dll", "MultiByteToWideChar"),
            ("kernel32.dll", "WideCharToMultiByte"),
            ("api-ms-win-core-string-l1-1-0.dll", "MultiByteToWideChar"),
            ("api-ms-win-core-string-l1-1-0.dll", "WideCharToMultiByte"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // The implementation is directly callable with the Windows x64 ABI.
        let (addr, _) = resolve_inner(
            &dll_key("api-ms-win-core-profile-l1-1-0.dll"),
            &name("QueryPerformanceFrequency"),
            0,
        );
        let qpf: unsafe extern "win64" fn(*mut u64) -> i32 = unsafe { std::mem::transmute(addr) };
        let mut freq = 0u64;
        assert_eq!(unsafe { qpf(&mut freq) }, 1);
        assert_eq!(freq, 10_000_000);
    }

    #[test]
    fn crt_startup_imports_resolve_to_hle() {
        // Step 11: CRT contracts have no routes but resolve through the cross-HLE
        // name lookup to the ucrtbase module, callable directly (never a stub).
        for (dll, sym) in [
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__p__fmode"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__p__commode"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argc"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argv"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_errno"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__acrt_iob_func"),
            ("api-ms-win-crt-heap-l1-1-0.dll", "malloc"),
            ("api-ms-win-crt-environment-l1-1-0.dll", "getenv"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "setvbuf"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "fopen"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "fread"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "fclose"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "fflush"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__stdio_common_vfprintf"),
            // Retail abort 0x2237179: the dump-path table (`%s%s`, `%llu`, ...) is
            // formatted through the secure wide common routine (and its twins).
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__stdio_common_vswprintf_s"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__stdio_common_vswprintf"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__stdio_common_vsprintf_s"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "__stdio_common_vsprintf"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strcmp"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strncmp"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strcpy"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strncpy"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strcat"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strncat"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcscpy"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcsncpy"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcscat"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcsncat"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wmemcpy"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wmemmove"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strstr"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strrchr"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strcspn"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strtok_s"),
            ("api-ms-win-crt-string-l1-1-0.dll", "_strnicmp"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strdup"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcschr"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcsrchr"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcsstr"),
            ("api-ms-win-crt-string-l1-1-0.dll", "_wcsdup"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcsdup"),
            ("api-ms-win-crt-convert-l1-1-0.dll", "mbrtowc"),
            ("api-ms-win-crt-convert-l1-1-0.dll", "wcrtomb"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_execute_onexit_table"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcscmp"),
            ("api-ms-win-crt-string-l1-1-0.dll", "wcsncmp"),
            ("api-ms-win-crt-string-l1-1-0.dll", "strchr"),
            ("api-ms-win-crt-string-l1-1-0.dll", "memchr"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_purecall"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_get_purecall_handler"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_invoke_watson"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "terminate"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_get_terminate"),
            ("api-ms-win-crt-heap-l1-1-0.dll", "_get_invalid_parameter_handler"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "__CxxFrameHandler3"),
            // M1.1a time/RNG/CRT-mode slice: real implementations, not auto-stubs.
            ("api-ms-win-crt-time-l1-1-0.dll", "_time64"),
            ("api-ms-win-crt-stdio-l1-1-0.dll", "_set_fmode"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_register_thread_local_exe_atexit_callback"),
            ("ucrtbase.dll", "memcpy"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::HleOther(_)),
                "{dll}!{sym} must be cross-HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(stubs::info_of_stub(addr).is_none(), "{dll}!{sym} must not be a stub thunk");
        }
        // The pointer shim is directly callable with the Windows x64 ABI.
        let (addr, _) =
            resolve_inner(&dll_key("api-ms-win-crt-stdio-l1-1-0.dll"), &name("__p__fmode"), 0);
        let p_fmode: unsafe extern "win64" fn() -> *mut i32 = unsafe { std::mem::transmute(addr) };
        let p = unsafe { p_fmode() };
        assert!(!p.is_null());
    }

    #[test]
    fn bundled_crt_dlls_forward_to_ucrt_hle() {
        // Phase C frontier: the game ships vcruntime140/msvcp140/concrt140/vcomp140/
        // vccorlib140. Their imports must resolve to `crt_linux`, never to bundled
        // GameDll code or logging stubs (which returned NULL for memcpy and faulted
        // msvcp140 DllMain at +0x2efd).
        let reg = registry();
        for dll in [
            "vcruntime140.dll",
            "msvcp140.dll",
            "concrt140.dll",
            "vcomp140.dll",
            "vccorlib140.dll",
        ] {
            assert_eq!(
                reg.hle_for(dll).map(|m| m.name),
                Some(crate::hle::UCRT),
                "{dll} must route to ucrtbase"
            );
        }
        // Routed CRT primitives are real HLE, callable directly (never a stub).
        for (dll, sym) in [
            ("vcruntime140.dll", "memcpy"),
            ("vcruntime140.dll", "memset"),
            ("vcruntime140.dll", "malloc"),
            ("msvcp140.dll", "memcpy"),
            ("msvcp140.dll", "memmove"),
            ("concrt140.dll", "malloc"),
            ("vcomp140.dll", "memset"),
            ("vccorlib140.dll", "free"),
            ("vcruntime140.dll", "__stdio_common_vfprintf"),
            ("msvcp140.dll", "__stdio_common_vfprintf"),
            ("msvcp140.dll", "strcmp"),
            ("vcruntime140.dll", "strncmp"),
            ("msvcp140.dll", "strcpy"),
            ("vcruntime140.dll", "strncpy"),
            ("msvcp140.dll", "strcat"),
            ("vcruntime140.dll", "strncat"),
            ("msvcp140.dll", "wcscpy"),
            ("vcruntime140.dll", "wcscat"),
            ("msvcp140.dll", "wmemcpy"),
            ("vcruntime140.dll", "wmemmove"),
            ("msvcp140.dll", "strstr"),
            ("vcruntime140.dll", "strrchr"),
            ("msvcp140.dll", "strdup"),
            ("vcruntime140.dll", "_strdup"),
            ("msvcp140.dll", "wcschr"),
            ("vcruntime140.dll", "wcsstr"),
            ("msvcp140.dll", "_wcsdup"),
            ("vcruntime140.dll", "wcsdup"),
            ("msvcp140.dll", "mbrtowc"),
            ("vcruntime140.dll", "wcrtomb"),
            ("concrt140.dll", "_execute_onexit_table"),
            ("msvcp140.dll", "wcscmp"),
            ("vcruntime140.dll", "strchr"),
            ("concrt140.dll", "memchr"),
            ("msvcp140.dll", "??2@YAPAXI@Z"),
            ("vcruntime140.dll", "??2@YAPEAX_K@Z"),
            ("msvcp140.dll", "??_U@YAPAXI@Z"),
            ("vcruntime140.dll", "??_U@YAPEAX_K@Z"),
            ("msvcp140.dll", "??3@YAXPAX@Z"),
            ("vcruntime140.dll", "??3@YAXPEAX@Z"),
            ("msvcp140.dll", "??_V@YAXPAX@Z"),
            ("vcruntime140.dll", "??_V@YAXPEAX@Z"),
            ("vcruntime140.dll", "??3@YAXPEAX_K@Z"),
            ("msvcp140.dll", "_callnewh"),
            ("vcruntime140.dll", "_set_new_handler"),
            ("concrt140.dll", "_query_new_handler"),
            ("vcruntime140.dll", "_purecall"),
            ("msvcp140.dll", "_set_purecall_handler"),
            ("concrt140.dll", "_get_purecall_handler"),
            ("vcruntime140.dll", "_invoke_watson"),
            ("msvcp140.dll", "_invalid_parameter"),
            ("vcruntime140.dll", "_invalid_parameter_noinfo"),
            ("concrt140.dll", "_get_invalid_parameter_handler"),
            ("msvcp140.dll", "terminate"),
            ("vcruntime140.dll", "_set_terminate"),
            ("concrt140.dll", "_get_terminate"),
            ("vcomp140.dll", "_set_unexpected"),
            ("vccorlib140.dll", "_get_unexpected"),
            ("msvcp140.dll", "_CxxThrowException"),
            ("vcruntime140.dll", "__CxxFrameHandler3"),
            ("msvcp140.dll", "__CxxFrameHandler4"),
            // M1.1a: `std::_Random_device` is real OS entropy, not the constant stub.
            ("msvcp140.dll", "?_Random_device@std@@YAIXZ"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(ref m) if m == crate::hle::UCRT),
                "{dll}!{sym} must be Hle(ucrtbase), got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(stubs::info_of_stub(addr).is_none(), "{dll}!{sym} must not be a stub thunk");
        }
        // The routed memcpy is directly callable with the Windows x64 ABI.
        let (addr, _) = resolve_inner(&dll_key("vcruntime140.dll"), &name("memcpy"), 0);
        let memcpy: unsafe extern "win64" fn(*mut u8, *const u8, usize) -> *mut u8 =
            unsafe { std::mem::transmute(addr) };
        unsafe {
            let mut dst = [0u8; 8];
            let src = *b"ABCDEFGH";
            assert_eq!(memcpy(dst.as_mut_ptr(), src.as_ptr(), 8), dst.as_mut_ptr());
            assert_eq!(&dst, &src);
        }
        // The routed strcpy/strcat are directly callable with the Windows x64 ABI
        // (return `dst`, never the NULL stub).
        let (addr, _) = resolve_inner(&dll_key("msvcp140.dll"), &name("strcpy"), 0);
        let strcpy: unsafe extern "win64" fn(*mut u8, *const u8) -> *mut u8 =
            unsafe { std::mem::transmute(addr) };
        let (addr, _) = resolve_inner(&dll_key("vcruntime140.dll"), &name("wmemmove"), 0);
        let wmemmove: unsafe extern "win64" fn(*mut u16, *const u16, usize) -> *mut u16 =
            unsafe { std::mem::transmute(addr) };
        unsafe {
            let mut dst = [0u8; 8];
            assert_eq!(strcpy(dst.as_mut_ptr(), b"hi\0".as_ptr()), dst.as_mut_ptr());
            assert_eq!(&dst[..3], b"hi\0");
            let mut w = [1u16, 2, 3, 4, 0, 0];
            assert_eq!(wmemmove(w.as_mut_ptr().add(1), w.as_ptr(), 4), w.as_mut_ptr().add(1));
            assert_eq!(&w[..5], &[1, 1, 2, 3, 4]);
        }
        // The routed operator new/delete is directly callable with the Windows x64 ABI.
        let (addr, _) = resolve_inner(&dll_key("msvcp140.dll"), &name("??2@YAPEAX_K@Z"), 0);
        let op_new: unsafe extern "win64" fn(u64) -> *mut u8 = unsafe { std::mem::transmute(addr) };
        let (addr, _) = resolve_inner(&dll_key("msvcp140.dll"), &name("??3@YAXPEAX@Z"), 0);
        let op_del: unsafe extern "win64" fn(*mut u8) = unsafe { std::mem::transmute(addr) };
        unsafe {
            let p = op_new(32);
            assert!(!p.is_null());
            *p = 0xAB;
            assert_eq!(*p, 0xAB);
            op_del(p);
            op_del(std::ptr::null_mut());
        }
        // The routed frame personality is directly callable and declines (1).
        let (addr, _) = resolve_inner(&dll_key("vcruntime140.dll"), &name("__CxxFrameHandler3"), 0);
        let fh: unsafe extern "win64" fn() -> i32 = unsafe { std::mem::transmute(addr) };
        assert_eq!(unsafe { fh() }, 1);
        // Even when a bundled file exists on disk, the HLE route wins over GameDll.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("xcl-crt-fwd-{nonce}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vcruntime140.dll"), b"not-a-pe").unwrap();
        let old = super::SEARCH_DIRS.read().unwrap().clone();
        super::set_search_dirs(vec![dir.clone()]);
        let (addr, src) = resolve_inner(&dll_key("vcruntime140.dll"), &name("memcpy"), 0);
        assert!(matches!(src, Source::Hle(_)), "bundled file must not shadow HLE, got {src:?}");
        assert!(stubs::info_of_stub(addr).is_none());
        // LoadLibrary hands out the HLE pseudo-handle, and GetProcAddress resolves it.
        let h = load_library("vcruntime140.dll").expect("bundled CRT HLE handle");
        assert!(crate::modules::hle_name_by_handle(h).is_some());
        assert!(get_proc_address(h, &name("memcpy")).is_some());
        super::set_search_dirs(old);
        let _ = std::fs::remove_dir_all(&dir);
        // Unknown C++ names stay stubs (return 0), never GameDll.
        let (addr, src) = resolve_inner(&dll_key("msvcp140.dll"), &name("LinuxProbeMissingCxx"), 0);
        assert_eq!(src, Source::Stub);
        assert_ne!(addr, 0);
    }

    #[test]
    fn xgr_identity_imports_resolve_to_hle() {
        // Step 11i: console identity + package imports are real HLE, callable directly.
        for (dll, sym) in [
            ("xgameruntime.dll", "XGameRuntimeInitialize"),
            ("xgameruntime.dll", "XSystemGetDeviceType"),
            (
                "xgameruntime.dll",
                "XPackageGetCurrentProcessPackageIdentifier",
            ),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // The device-type query is directly callable with the Windows x64 ABI.
        let (addr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XSystemGetDeviceType"), 0);
        let f: unsafe extern "win64" fn() -> u32 = unsafe { std::mem::transmute(addr) };
        let got = unsafe { f() };
        assert!(got == 6 || got == 7, "device type {got}");
    }

    #[test]
    fn minuserx_window_imports_resolve_to_hle() {
        // Step 11m: headless console window. `minuserx.dll` + ntuser API sets are
        // routed HLE; `user32.dll`/`dwmapi.dll` stay routeless and resolve via
        // the cross-HLE name lookup.
        for (dll, sym) in [
            ("minuserx.dll", "CreateWindowExA"),
            ("minuserx.dll", "CreateWindowExW"),
            (
                "ext-ms-win-rtcore-ntuser-window-l1-1-0.dll",
                "CreateWindowExW",
            ),
            ("user32.dll", "RegisterClassW"),
            ("user32.dll", "GetWindowLongW"),
            ("user32.dll", "GetClientRect"),
            ("user32.dll", "DefWindowProcW"),
            ("user32.dll", "DestroyWindow"),
            ("dwmapi.dll", "DwmGetWindowAttribute"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
    }

    #[test]
    fn gameinput_imports_resolve_to_hle() {
        // Step 11n: headless IGameInput. `gameinput.dll` is routed HLE.
        for (dll, sym) in [("gameinput.dll", "GameInputCreate")] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Timestamps advance through the COM vtable slot 3, as the title reads them.
        let (addr, _) = resolve_inner(&dll_key("gameinput.dll"), &name("GameInputCreate"), 0);
        let create: unsafe extern "win64" fn(*mut *mut std::ffi::c_void) -> i32 =
            unsafe { std::mem::transmute(addr) };
        let mut gi: *mut std::ffi::c_void = std::ptr::null_mut();
        assert_eq!(unsafe { create(&mut gi) }, 0);
        assert!(!gi.is_null());
        let vt = unsafe { *(gi as *const *const usize) };
        let ts: unsafe extern "win64" fn(*mut std::ffi::c_void) -> u64 =
            unsafe { std::mem::transmute(*vt.add(3)) };
        let a = unsafe { ts(gi) };
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = unsafe { ts(gi) };
        assert!(a > 0 && b > a, "timestamps {a} {b}");
    }

    #[test]
    fn mfplat_imports_resolve_to_hle() {
        // Step 12f: headless MFPlat manager. `mfplat.dll` is routed HLE.
        for (dll, sym) in [
            ("mfplat.dll", "MFStartup"),
            ("mfplat.dll", "MFShutdown"),
            ("mfplat.dll", "MFCreateDXGIDeviceManager"),
            ("mfplat.dll", "MFResetDXGIDeviceManagerX"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Direct ABI: create hands out a manager whose title slots work.
        let (addr, _) = resolve_inner(&dll_key("mfplat.dll"), &name("MFCreateDXGIDeviceManager"), 0);
        let create: unsafe extern "win64" fn(*mut u32, *mut *mut std::ffi::c_void) -> i32 =
            unsafe { std::mem::transmute(addr) };
        let (raddr, _) = resolve_inner(&dll_key("mfplat.dll"), &name("MFResetDXGIDeviceManagerX"), 0);
        let reset: unsafe extern "win64" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, u32) -> i32 =
            unsafe { std::mem::transmute(raddr) };
        unsafe {
            let mut token: u32 = 0;
            let mut mgr: *mut std::ffi::c_void = std::ptr::null_mut();
            assert_eq!(create(&mut token, &mut mgr), 0);
            assert_ne!(token, 0);
            assert!(!mgr.is_null());
            let fake = 0x2000 as *mut std::ffi::c_void;
            assert_eq!(reset(mgr, fake, token.wrapping_add(1)), 0x8007_0057u32 as i32);
            assert_eq!(reset(mgr, fake, token), 0);
        }
    }

    #[test]
    fn dstorage_imports_resolve_to_hle() {
        // Step 12g: DirectStorage file-read subset. `xcrdapi.dll` is routed HLE.
        for (dll, sym) in [("xcrdapi.dll", "DStorageGetFactory")] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Direct ABI: factory QI accepts the factory IID, refuses unknown IIDs.
        let (addr, _) = resolve_inner(&dll_key("xcrdapi.dll"), &name("DStorageGetFactory"), 0);
        let get: unsafe extern "win64" fn(*const u8, *mut *mut std::ffi::c_void) -> i32 =
            unsafe { std::mem::transmute(addr) };
        unsafe {
            let mut f: *mut std::ffi::c_void = std::ptr::null_mut();
            // IID_FACTORY bytes (little-endian fields, as the title passes them).
            let iid: [u8; 16] = [
                0xac, 0x54, 0x3b, 0xfb, 0x82, 0xbf, 0x3a, 0x46, 0xb7, 0x77, 0x73, 0x19, 0x58, 0x37,
                0x86, 0x4d,
            ];
            assert_eq!(get(iid.as_ptr(), &mut f), 0);
            assert!(!f.is_null());
        }
    }

    #[test]
    fn xtaskqueue_imports_resolve_to_hle() {
        // Step 11o: manual/delayed/pool dispatch + composite + networking + LSA probe.
        for (dll, sym) in [
            ("xgameruntime.dll", "XTaskQueueCreate"),
            ("xgameruntime.dll", "XTaskQueueCreateComposite"),
            ("xgameruntime.dll", "XTaskQueueGetPort"),
            ("xgameruntime.dll", "XTaskQueueSubmitCallback"),
            ("xgameruntime.dll", "XTaskQueueSubmitDelayedCallback"),
            ("xgameruntime.dll", "XTaskQueueDispatch"),
            ("xgameruntime.dll", "XTaskQueueTerminate"),
            ("xgameruntime.dll", "XTaskQueueCloseHandle"),
            ("xgameruntime.dll", "XNetworkingQueryPreferredLocalUdpMultiplayerPort"),
            ("xgameruntime.dll", "XNetworkingGetConnectivityHint"),
            ("advapi32legacy.dll", "LsaOpenPolicy"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Direct ABI: manual queue dispatches, delayed fires via timeout, pool runs async.
        let (addr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XTaskQueueCreate"), 0);
        let create: unsafe extern "win64" fn(u32, u32, *mut *mut std::ffi::c_void) -> i32 =
            unsafe { std::mem::transmute(addr) };
        let (daddr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XTaskQueueDispatch"), 0);
        let dispatch: unsafe extern "win64" fn(*mut std::ffi::c_void, u32, u32) -> bool =
            unsafe { std::mem::transmute(daddr) };
        let (saddr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XTaskQueueSubmitCallback"), 0);
        let submit: unsafe extern "win64" fn(*mut std::ffi::c_void, u32, *mut std::ffi::c_void, usize) -> i32 =
            unsafe { std::mem::transmute(saddr) };
        let (taddr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XTaskQueueTerminate"), 0);
        let terminate: unsafe extern "win64" fn(*mut std::ffi::c_void, bool, *mut std::ffi::c_void, usize) -> i32 =
            unsafe { std::mem::transmute(taddr) };
        let (caddr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XTaskQueueCloseHandle"), 0);
        let close: unsafe extern "win64" fn(*mut std::ffi::c_void) =
            unsafe { std::mem::transmute(caddr) };
        unsafe extern "win64" fn bump(ctx: *mut std::ffi::c_void, canceled: bool) {
            assert!(!canceled);
            let hits = &*(ctx as *const std::sync::atomic::AtomicU32);
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let hits = std::sync::atomic::AtomicU32::new(0);
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(create(0, 0, &mut q), 0);
            assert_eq!(submit(q, 0, &hits as *const _ as *mut _, bump as usize), 0);
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(dispatch(q, 0, 0));
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(terminate(q, true, std::ptr::null_mut(), 0), 0);
            close(q);
        }
        // LSA probe resolves through the kernelx HLE handle for GetProcAddress checks.
        let h = load_library("advapi32legacy.dll").expect("advapi32legacy HLE handle");
        assert!(get_proc_address(h, &name("LsaOpenPolicy")).is_some());
    }

    #[test]
    fn xuser_imports_resolve_to_hle() {
        // Step 11p: XAsync engine + offline user + save-folder/PLS storage.
        for (dll, sym) in [
            ("xgameruntime.dll", "XAsyncBegin"),
            ("xgameruntime.dll", "XAsyncGetStatus"),
            ("xgameruntime.dll", "XAsyncGetResult"),
            ("xgameruntime.dll", "XAsyncRun"),
            ("xgameruntime.dll", "XUserAddAsync"),
            ("xgameruntime.dll", "XUserAddResult"),
            ("xgameruntime.dll", "XUserGetGamertag"),
            ("xgameruntime.dll", "XUserFindForDevice"),
            ("xgameruntime.dll", "XUserGetTokenAndSignatureAsync"),
            ("xgameruntime.dll", "XGameSaveFilesGetFolderWithUiAsync"),
            ("xgameruntime.dll", "XGameSaveFilesGetFolderWithUiResult"),
            ("xgameruntime.dll", "XPersistentLocalStorageGetPath"),
            ("xgameruntime.dll", "XPersistentLocalStorageGetSpaceInfo"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Direct ABI: silent sign-in completes, gamertag matches config.
        let (addr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XUserAddAsync"), 0);
        let add: unsafe extern "win64" fn(u32, *mut u8) -> i32 = unsafe { std::mem::transmute(addr) };
        let (saddr, _) = resolve_inner(&dll_key("xgameruntime.dll"), &name("XAsyncGetStatus"), 0);
        let status: unsafe extern "win64" fn(*mut u8, bool) -> i32 = unsafe { std::mem::transmute(saddr) };
        unsafe {
            let mut b = [0u8; 48];
            assert_eq!(add(1, b.as_mut_ptr()), 0);
            assert_eq!(status(b.as_mut_ptr(), true), 0);
        }
    }

    #[test]
    fn xaudio2_imports_resolve_to_hle() {
        // Step 11q: headless XAudio2 engine. `xaudio2_9.dll` is routed HLE.
        for (dll, sym) in [
            ("xaudio2_9.dll", "XAudio2Create"),
            ("xaudio2_9.dll", "XAudio2CreateWithVersionInfo"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Direct ABI: engine hands out a mastering voice.
        let (addr, _) = resolve_inner(&dll_key("xaudio2_9.dll"), &name("XAudio2Create"), 0);
        let create: unsafe extern "win64" fn(*mut *mut std::ffi::c_void, u32, u32) -> i32 =
            unsafe { std::mem::transmute(addr) };
        unsafe {
            let mut eng: *mut std::ffi::c_void = std::ptr::null_mut();
            assert_eq!(create(&mut eng, 0x41, 0x3c), 0);
            assert!(!eng.is_null());
            let vt = *(eng as *const *const usize);
            let create_master: unsafe extern "win64" fn(
                *mut std::ffi::c_void,
                *mut *mut std::ffi::c_void,
                u32,
                u32,
                u32,
                *const u16,
                *const std::ffi::c_void,
                u32,
            ) -> i32 = std::mem::transmute(*vt.add(7));
            let mut master: *mut std::ffi::c_void = std::ptr::null_mut();
            assert_eq!(
                create_master(eng, &mut master, 2, 48_000, 0x4, std::ptr::null(), std::ptr::null(), 6),
                0
            );
            assert!(!master.is_null());
        }
    }

    #[test]
    fn d3d12x_imports_resolve_to_hle() {
        // Step 11r: headless D3D12.X null device. `d3d12_xs.dll` is routed HLE.
        for (dll, sym) in [
            ("d3d12_xs.dll", "D3D12CreateDevice"),
            ("d3d12_xs.dll", "D3D12XboxCreateDevice"),
            ("d3d12_xs.dll", "D3D12SerializeRootSignature"),
            ("d3d12_xs.dll", "D3D10CreateBlob"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // Direct ABI: device hands out queue + fence, signal completes instantly.
        let (addr, _) = resolve_inner(&dll_key("d3d12_xs.dll"), &name("D3D12CreateDevice"), 0);
        let create: unsafe extern "win64" fn(
            *mut std::ffi::c_void,
            u32,
            *const std::ffi::c_void,
            *mut *mut std::ffi::c_void,
        ) -> i32 = unsafe { std::mem::transmute(addr) };
        unsafe {
            let mut dev: *mut std::ffi::c_void = std::ptr::null_mut();
            assert_eq!(create(std::ptr::null_mut(), 0xC000, std::ptr::null(), &mut dev), 0);
            assert!(!dev.is_null());
            let vt = *(dev as *const *const usize);
            let create_queue: unsafe extern "win64" fn(
                *mut std::ffi::c_void,
                *const std::ffi::c_void,
                *const std::ffi::c_void,
                *mut *mut std::ffi::c_void,
            ) -> i32 = std::mem::transmute(*vt.add(8));
            let create_fence: unsafe extern "win64" fn(
                *mut std::ffi::c_void,
                u64,
                u32,
                *const std::ffi::c_void,
                *mut *mut std::ffi::c_void,
            ) -> i32 = std::mem::transmute(*vt.add(36));
            let mut q: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut f: *mut std::ffi::c_void = std::ptr::null_mut();
            assert_eq!(create_queue(dev, std::ptr::null(), std::ptr::null(), &mut q), 0);
            assert_eq!(create_fence(dev, 5, 0, std::ptr::null(), &mut f), 0);
            assert!(!q.is_null() && !f.is_null());
            let qvt = *(q as *const *const usize);
            let signal: unsafe extern "win64" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, u64) -> i32 =
                std::mem::transmute(*qvt.add(14));
            assert_eq!(signal(q, f, 9), 0);
            let fvt = *(f as *const *const usize);
            let completed: unsafe extern "win64" fn(*mut std::ffi::c_void) -> u64 =
                std::mem::transmute(*fvt.add(8));
            assert_eq!(completed(f), 9);
        }
    }

    #[test]
    fn xg_d3dcompile_imports_resolve_to_hle() {
        // Step 11s: XG texture computer (routed HLE) + headless D3DCompile
        // (routeless `d3dcompiler_47.dll`, resolved via the cross-HLE lookup).
        for (dll, sym) in [
            ("xg_xs.dll", "XGCreateTextureComputer"),
            ("xg_xs.dll", "XGSuggestSwizzleMode"),
            ("d3dcompiler_47.dll", "D3DCompile"),
        ] {
            let (addr, src) = resolve_inner(&dll_key(dll), &name(sym), 0);
            assert!(
                matches!(src, Source::Hle(_) | Source::HleOther(_)),
                "{dll}!{sym} must be HLE, got {src:?}"
            );
            assert_ne!(addr, 0, "{dll}!{sym} must be callable");
            assert!(
                stubs::info_of_stub(addr).is_none(),
                "{dll}!{sym} must not be a stub thunk"
            );
        }
        // The fake compiler echoes its source into a blob.
        let (addr, _) = resolve_inner(&dll_key("d3dcompiler_47.dll"), &name("D3DCompile"), 0);
        type Compile = unsafe extern "win64" fn(
            *const u8,
            usize,
            *const u8,
            *const std::ffi::c_void,
            *const std::ffi::c_void,
            *const u8,
            *const u8,
            u32,
            u32,
            *mut *mut std::ffi::c_void,
            *mut *mut std::ffi::c_void,
        ) -> i32;
        let compile: Compile = unsafe { std::mem::transmute(addr) };
        unsafe {
            let src = b"float4 main() : SV_Target { return 1; }";
            let mut code = std::ptr::null_mut();
            assert_eq!(
                compile(
                    src.as_ptr(),
                    src.len(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    b"main\0".as_ptr(),
                    b"ps_5_0\0".as_ptr(),
                    0,
                    0,
                    &mut code,
                    std::ptr::null_mut(),
                ),
                0
            );
            assert!(!code.is_null());
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
        // Step-11k: LoadLibraryW is now implemented HLE, directly callable.
        let ll = get_proc_address(h, &name("LoadLibraryW")).expect("LoadLibraryW HLE");
        assert!(stubs::info_of_stub(ll).is_none());
        // Implemented HLE bodies resolve to callable addresses through the same path.
        let va = get_proc_address(h, &name("VirtualAlloc")).expect("VirtualAlloc HLE");
        assert!(stubs::info_of_stub(va).is_none());
        let xa = get_proc_address(h, &name("XMemAlloc")).expect("XMemAlloc HLE");
        assert!(stubs::info_of_stub(xa).is_none());
        // Unknown handles have no lookup.
        assert_eq!(get_proc_address(0x1234_5000, &name("XMemAlloc")), None);
        // Stub routes still hand out a pseudo handle.
        assert!(load_library("mystery.dll").is_none_or(|h| h != 0));
    }

    #[test]
    fn classify_never_claims_hle_on_linux() {
        // File APIs are HLE on Linux now (step 10c), as is the step-11 CRT slice
        // (resolved from any api-ms-win-crt-* contract via the cross-HLE lookup);
        // unimplemented HLE names stay Stub.
        assert!(matches!(
            classify("kernel32.dll", &name("CreateFileW")),
            Source::Hle(_) | Source::HleOther(_)
        ));
        assert!(matches!(
            classify("d3d12_xs.dll", &name("D3D12XboxCreateDevice")),
            Source::Hle(_) | Source::HleOther(_)
        ));
        assert!(matches!(
            classify("ucrtbase.dll", &name("malloc")),
            Source::HleOther(_)
        ));
        assert!(matches!(
            classify("api-ms-win-crt-stdio-l1-1-0.dll", &name("__p__fmode")),
            Source::HleOther(_)
        ));
        assert!(matches!(
            classify("ucrtbase.dll", &name("getenv")),
            Source::HleOther(_)
        ));
        assert!(matches!(
            classify("ntdll.dll", &name("NtWriteFile")),
            Source::HleOther(_)
        ));
        assert_eq!(classify("ucrtbase.dll", &name("LinuxProbeMissingCrt")), Source::Stub);
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
