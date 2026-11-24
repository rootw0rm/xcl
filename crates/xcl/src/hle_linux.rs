//! Linux HLE registry (Phase C steps 10a–10b): routing plus the kernelx core.
//!
//! The Windows `hle` module stays the implementation source of truth for the full title
//! path. On Linux this twin provides the same ordered routes (`hle_routes::ROUTES`).
//! Unimplemented exports keep metadata address `0` (meaning `Stub`); the runtime
//! resolver turns `0` into a safe executable auto-stub thunk. Step 10b implements the
//! kernelx memory/thread/sync/time slice in `hle/kernelx_linux.rs`, so those names
//! carry real code addresses here and resolve as `HLE`. No `LoadLibrary`/`GetProcAddress`
//! on Linux.
//!
//! Export-name lists below must match the `hle_exports![...]` invocations in the
//! corresponding Windows files (`hle/kernelx.rs`, `hle/xgameruntime/mod.rs`, …).
//! They are duplicated here until the implementation bodies move to `_windows.rs`
//! (DEC-002); `inspect` and the registry tests pin the counts.

#[path = "hle/kernelx_linux.rs"]
pub mod kernelx_linux;
#[path = "hle/crt_linux.rs"]
pub mod crt_linux;

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::config::Config;
use crate::util::glob_match;

pub use crate::hle_routes::{
    ACPHAL, D3D12X, DXCOMPILER, GAMEINPUT, KERNELX, MFPLAT, MINUSERX, XAUDIO2, XCRDAPI, XG,
    XGAMERUNTIME,
};

/// Canonical name of the Linux CRT module. It intentionally has no route: CRT imports
/// arrive as `api-ms-win-crt-*.dll` (and `ucrtbase.dll`) and resolve through the
/// cross-HLE `find_any` name lookup, so every CRT contract variant works without
/// route-table churn. `inspect` reports these as `HLE (ucrtbase.dll)`.
pub const UCRT: &str = "ucrtbase.dll";

/// Linux form: keep the name, store `0` as `Stub` metadata. The Windows form keeps
/// `$name as usize`. Only one of the two is compiled (see `lib.rs` `#[path]`).
#[macro_export]
macro_rules! hle_exports {
    ($($name:ident),* $(,)?) => {
        vec![$( (stringify!($name), 0usize) ),*]
    };
}

pub struct HleModule {
    /// Canonical DLL name, e.g. "kernelx.dll".
    pub name: &'static str,
    pub exports: HashMap<&'static str, usize>,
    pub ordinals: HashMap<u16, &'static str>,
    /// For names we do not implement: real Windows DLLs to try, in order. `"@self"` is
    /// the DLL name the title imported (when it is not our canonical name).
    pub fallback: &'static [&'static str],
}

pub const SELF: &str = "@self";

impl HleModule {
    fn new(name: &'static str, list: Vec<(&'static str, usize)>, fallback: &'static [&'static str]) -> Self {
        HleModule { name, exports: list.into_iter().collect(), ordinals: HashMap::new(), fallback }
    }

    pub fn aliases_are_system(&self) -> bool {
        self.fallback.contains(&SELF)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Hle(usize),
    System,
    Stub,
}

pub struct Registry {
    pub modules: Vec<HleModule>,
    routes: Vec<(String, Route)>,
}

static REG: OnceLock<Registry> = OnceLock::new();

fn kernelx_exports() -> Vec<(&'static str, usize)> {
    // Step-10b names below carry real Linux addresses from `kernelx_linux`; the rest stay
    // `Stub` metadata (0) until steps 10c–10d and Phase D/E land. The stub list must match
    // the Windows `hle_exports![...]` in `hle/kernelx.rs`; `inspect` pins the totals.
    let mut v = crate::hle_exports![
        RegOpenKeyW, RegOpenKeyExW, RegQueryValueExW, RegCloseKey, VirtualAlloc, VirtualAllocEx,
        VirtualFree, VirtualFreeEx, VirtualProtect, VirtualProtectEx, XMemAlloc, XMemFree,
        XMemAllocDefault, XMemFreeDefault, XMemSetAllocationHooks, XMemVirtualQuery,
        XMemVirtualAlloc, XMemAllocatePhysicalPages, XMemMapPhysicalPages,
        XMemMapPhysicalPagesScatter, XMemFreePhysicalPages, XMemTransferMemory,
        XMemGetWorkingSetStatistics, TitleMemoryStatus, JobTitleMemoryStatus, ToolingMemoryStatus,
        AllocateTitlePhysicalPages, FreeTitlePhysicalPages, MapTitlePhysicalPages,
        RegisterAppStateChangeNotification, UnregisterAppStateChangeNotification,
        RegisterAppConstrainedChangeNotification, UnregisterAppConstrainedChangeNotification,
        SetUnhandledExceptionFilter, SetThreadName, GetThreadName, SetThreadpoolAffinityMask,
        SetThreadAffinityMask, SetThreadGroupAffinity, QueryProcessorSchedulingStatistics,
        GetConsoleType, OutputDebugStringA, OutputDebugStringW, GetSystemOSVersion, LoadLibraryW,
        LoadLibraryA, LoadLibraryExW, LoadLibraryExA, LoadPackagedLibrary, GetProcAddress,
        GetModuleHandleW, GetModuleHandleA, GetModuleHandleExW, GetModuleHandleExA,
        GetModuleFileNameW, GetModuleFileNameA, FreeLibrary, FreeLibraryWhenCallbackReturns,
        DisableThreadLibraryCalls,
    ];
    // Kernel-family fallbacks routed to KERNELX (api-ms-win-core-*) live outside the
    // Windows `hle_exports!` list; they are always real on Linux when implemented.
    for (name, addr) in kernelx_linux::exports() {
        if let Some(slot) = v.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = addr;
        } else {
            v.push((name, addr));
        }
    }
    v
}

fn xgameruntime_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        InitializeApiImpl, InitializeApiImplEx, InitializeApiImplEx2, UninitializeApiImpl,
        QueryApiImpl, XErrorReport, XGameRuntimeInitialize, XGameRuntimeInitializeWithOptions,
        XGameRuntimeUninitialize, XGameRuntimeIsFeatureAvailable, XTaskQueueCreate,
        XTaskQueueCreateComposite, XTaskQueueGetPort, XTaskQueueDuplicateHandle,
        XTaskQueueCloseHandle, XTaskQueueSubmitCallback, XTaskQueueSubmitDelayedCallback,
        XTaskQueueDispatch, XTaskQueueIsEmpty, XTaskQueueTerminate,
        XTaskQueueGetCurrentProcessTaskQueue, XTaskQueueSetCurrentProcessTaskQueue,
        XTaskQueueRegisterWaiter, XTaskQueueUnregisterWaiter, XTaskQueueRegisterMonitor,
        XTaskQueueUnregisterMonitor, XAsyncBegin, XAsyncSchedule, XAsyncComplete, XAsyncGetStatus,
        XAsyncGetResultSize, XAsyncGetResult, XAsyncCancel, XAsyncRun, XThreadSetTimeSensitive,
        XThreadVerifyNotTimeSensitive, XThreadAssertNotTimeSensitive, XThreadIsTimeSensitive,
        XNetworkingQueryPreferredLocalUdpMultiplayerPort,
        XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync,
        XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult,
        XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged,
        XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged,
        XNetworkingQuerySecurityInformationForUrlAsync,
        XNetworkingQuerySecurityInformationForUrlAsyncResultSize,
        XNetworkingQuerySecurityInformationForUrlAsyncResult,
        XNetworkingQuerySecurityInformationForUrlUtf16Async,
        XNetworkingQuerySecurityInformationForUrlUtf16AsyncResultSize,
        XNetworkingQuerySecurityInformationForUrlUtf16AsyncResult,
        XNetworkingVerifyServerCertificate, XNetworkingGetConnectivityHint,
        XNetworkingRegisterConnectivityHintChanged, XNetworkingUnregisterConnectivityHintChanged,
        XSystemGetDeviceType, XSystemGetConsoleId, XSystemGetAnalyticsInfo,
        XSystemGetXboxLiveSandboxId, XPackageIsPackagedProcess,
        XPackageGetCurrentProcessPackageIdentifier, XPackageGetIdentifier, XPackageMount,
        XPackageGetMountPathSize, XPackageGetMountPath, XPackageCloseMountHandle,
        XPackageEnumeratePackages, XPersistentLocalStorageGetPathSize,
        XPersistentLocalStorageGetPath, XPersistentLocalStorageGetSpaceInfo,
        XGameSaveFilesGetFolderWithUiAsync, XGameSaveFilesGetFolderWithUiResult,
        XGameSaveInitializeProvider, XGameSaveInitializeProviderAsync,
        XGameSaveInitializeProviderResult, XGameSaveCloseProvider, XGameSaveGetRemainingQuota,
        XGameSaveGetRemainingQuotaAsync, XGameSaveGetRemainingQuotaResult,
        XGameSaveDeleteContainer, XGameSaveDeleteContainerAsync, XGameSaveDeleteContainerResult,
        XGameSaveCreateContainer, XGameSaveCloseContainer, XGameSaveGetContainerInfo,
        XGameSaveEnumerateContainerInfo, XGameSaveEnumerateContainerInfoByName,
        XGameSaveEnumerateBlobInfo, XGameSaveEnumerateBlobInfoByName, XGameSaveReadBlobData,
        XGameSaveReadBlobDataAsync, XGameSaveReadBlobDataResult, XGameSaveCreateUpdate,
        XGameSaveCloseUpdate, XGameSaveSubmitBlobWrite, XGameSaveSubmitBlobDelete,
        XGameSaveSubmitUpdate, XGameSaveSubmitUpdateAsync, XGameSaveSubmitUpdateResult,
        XUserAddAsync, XUserAddResult, XUserDuplicateHandle, XUserCloseHandle, XUserCompare,
        XUserGetMaxUsers, XUserGetId, XUserGetLocalId, XUserFindUserByLocalId, XUserFindUserById,
        XUserGetIsGuest, XUserGetState, XUserGetGamertag, XUserGetAgeGroup,
        XUserRegisterForChangeEvent, XUserUnregisterForChangeEvent,
        XUserGetTokenAndSignatureAsync, XUserGetTokenAndSignatureResultSize,
        XUserGetTokenAndSignatureResult, XUserGetTokenAndSignatureUtf16Async,
        XUserGetTokenAndSignatureUtf16ResultSize, XUserGetTokenAndSignatureUtf16Result,
        XUserFindForDevice, XUserFindControllerForUserWithUiAsync,
        XUserFindControllerForUserWithUiResult, XClosedCaptionGetProperties,
        XClosedCaptionSetEnabled, XHighContrastGetMode, XStoreCreateContext,
        XStoreCloseContextHandle,
    ]
}

fn d3d12x_exports() -> Vec<(&'static str, usize)> {
    let mut v = crate::hle_exports![
        D3D12CreateDevice, D3D12XboxCreateDevice, D3D12GetDebugInterface,
        D3D12SerializeRootSignature, D3D12SerializeVersionedRootSignature,
        D3D12CreateRootSignatureDeserializer, D3D12CreateVersionedRootSignatureDeserializer,
        D3D12EnableExperimentalFeatures, D3D12GetInterface, D3D10CreateBlob,
        D3D12XboxSetProcessDebugFlags, D3D12ConfigureVirtualMemoryX,
    ];
    // Data export on Windows (`&IID_ID3D12Device`); metadata `0` on Linux.
    v.push(("IID_ID3D12Device", 0));
    v
}

fn xaudio2_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![XAudio2Create, XAudio2CreateWithVersionInfo]
}

fn mfplat_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![MFResetDXGIDeviceManagerX]
}

fn dxcompiler_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![DxcCreateInstance, DxcCreateInstance2]
}

fn minuserx_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![CreateWindowExA, RegisterClassExA, CreateWindowExW]
}

fn xcrdapi_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![DStorageGetFactory]
}

fn xg_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![XGCreateTextureComputer, XGSuggestSwizzleMode]
}

fn acphal_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        XDspConnectWithMaximumStreamLimit, XDspDisconnect, XDspActivate, XDspDeactivate,
        XDspSubmitCommand, ApuCreateHeap, ApuAlloc, ApuFree, ApuMapVirtualAddress,
        ApuMapApuAddress, ApuIsVirtualAddressValid, ApuHeapGetState, AcpHalCreate,
        AcpHalAllocateShapeContexts, AcpHalReleaseShapeContexts, XApuConnect, XApuDisconnect,
        XApuEnqueueCommand, XApuDequeueResult, XApuGetResultCount, XApuGetUtilization,
    ]
}

fn gameinput_exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![GameInputCreate]
}

/// Step-11 CRT startup slice: every name carries a real Linux address from
/// `crt_linux` (never `0`/stub metadata).
fn crt_exports() -> Vec<(&'static str, usize)> {
    crt_linux::exports()
}

fn build(config: &Config) -> Registry {
    let modules = vec![
        HleModule::new(KERNELX, kernelx_exports(), &[SELF, "kernelbase.dll", "kernel32.dll", "advapi32.dll", "sechost.dll"]),
        HleModule::new(XGAMERUNTIME, xgameruntime_exports(), &[]),
        HleModule::new(D3D12X, d3d12x_exports(), &[]),
        HleModule::new(XAUDIO2, xaudio2_exports(), &["xaudio2_9.dll"]),
        HleModule::new(MFPLAT, mfplat_exports(), &["mfplat.dll"]),
        HleModule::new(DXCOMPILER, dxcompiler_exports(), &[]),
        HleModule::new(MINUSERX, minuserx_exports(), &["user32.dll"]),
        HleModule::new(XCRDAPI, xcrdapi_exports(), &[]),
        HleModule::new(XG, xg_exports(), &[]),
        HleModule::new(ACPHAL, acphal_exports(), &[]),
        HleModule::new(GAMEINPUT, gameinput_exports(), &[]),
        HleModule::new(UCRT, crt_exports(), &[]),
    ];
    let idx = |n: &str| modules.iter().position(|m| m.name == n).unwrap();
    let mut routes: Vec<(String, Route)> = Vec::new();
    for (pat, target) in &config.aliases {
        let t = target.trim().to_ascii_lowercase();
        let route = match t.as_str() {
            "@system" => Some(Route::System),
            "@stub" => Some(Route::Stub),
            other => modules
                .iter()
                .position(|m| m.name == crate::util::dll_key(other))
                .map(Route::Hle),
        };
        match route {
            Some(r) => routes.push((pat.to_ascii_lowercase(), r)),
            None => crate::xlog_warn!("aliases: unknown target '{target}' for '{pat}' – skipping"),
        }
    }
    // Same ordered table as Windows: the first matching pattern wins.
    for &(pattern, module) in &crate::hle_routes::ROUTES {
        routes.push((pattern.into(), Route::Hle(idx(module))));
    }
    // LINUX-TODO: Phase C — `XCL_NETWORK=0` offline routes (`netoff`) once its Linux
    // implementation lands; Windows adds winhttp/ws2_32 routes here.
    Registry { modules, routes }
}

pub fn init(config: &Config) {
    let _ = REG.set(build(config));
}

pub fn registry() -> &'static Registry {
    REG.get_or_init(|| build(&Config::default()))
}

impl Registry {
    pub fn route(&self, dll_key: &str) -> Option<&Route> {
        self.routes.iter().find(|(p, _)| glob_match(p, dll_key)).map(|(_, r)| r)
    }

    pub fn module_by_name(&self, name: &str) -> Option<&HleModule> {
        let k = crate::util::dll_key(name);
        self.modules.iter().find(|m| m.name == k)
    }

    pub fn hle_for(&self, dll_key: &str) -> Option<&HleModule> {
        match self.route(dll_key)? {
            Route::Hle(i) => self.modules.get(*i),
            _ => None,
        }
    }

    pub fn find_any(&self, name: &str) -> Option<(&HleModule, usize)> {
        self.modules
            .iter()
            .find_map(|m| m.exports.get(name).map(|&a| (m, a)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_routes() {
        let r = build(&Config::default());
        let name = |k: &str| r.hle_for(k).map(|m| m.name);
        assert_eq!(name("kernelx.dll"), Some(KERNELX));
        assert_eq!(name("kernel32.dll"), Some(KERNELX));
        assert_eq!(name("api-ms-win-core-file-l1-1-0.dll"), Some(KERNELX));
        assert_eq!(name("xgameruntime.dll"), Some(XGAMERUNTIME));
        assert_eq!(name("d3d12_xs.dll"), Some(D3D12X));
        assert_eq!(name("xaudio2_9.dll"), Some(XAUDIO2));
        assert_eq!(name("mfplat.dll"), Some(MFPLAT));
        assert!(r.hle_for("kernel32.dll").unwrap().aliases_are_system());
        assert!(!r.hle_for("xaudio2_9.dll").unwrap().aliases_are_system());
        assert!(!r.hle_for("mfplat.dll").unwrap().aliases_are_system());
        assert_eq!(name("ucrtbase.dll"), None);
        assert_eq!(name("api-ms-win-crt-runtime-l1-1-0.dll"), None);
    }

    #[test]
    fn aliases_take_precedence() {
        let mut c = Config::default();
        c.aliases.insert("kernel32.dll".into(), "@system".into());
        c.aliases.insert("mystery*.dll".into(), "@stub".into());
        c.aliases.insert("xboxthing.dll".into(), "xgameruntime".into());
        let r = build(&c);
        assert_eq!(r.route("kernel32.dll"), Some(&Route::System));
        assert_eq!(r.route("mystery42.dll"), Some(&Route::Stub));
        assert_eq!(r.hle_for("xboxthing.dll").map(|m| m.name), Some(XGAMERUNTIME));
    }

    #[test]
    fn linux_exports_are_stub_metadata_except_kernelx_core() {
        let r = build(&Config::default());
        // Unimplemented names are metadata `0`, never a callable address. Steps 10b–10c
        // implement the kernelx memory/thread/sync/time/file slices and step 11 the CRT
        // startup slice, so those resolve as HLE.
        let mut total = 0;
        let mut implemented = 0;
        for m in &r.modules {
            assert!(!m.exports.is_empty(), "{} has no exports", m.name);
            for (&name, &addr) in &m.exports {
                total += 1;
                if (m.name == KERNELX || m.name == UCRT) && addr != 0 {
                    implemented += 1;
                    continue;
                }
                assert_eq!(addr, 0, "{}!{} must be Stub metadata", m.name, name);
            }
        }
        assert!(total > 200, "expected the Windows export-name tables, got {total}");
        assert!(implemented > 70, "expected the step-10b/10c kernelx core, got {implemented}");
        // Spot-check counts against the Windows tables (kernelx grows with the Linux
        // kernel-family fallbacks routed to it; ucrtbase is a Linux-only CRT module).
        let kx = r.module_by_name(KERNELX).unwrap();
        assert_eq!(kx.exports.len(), 59 + kernelx_linux::exports().iter().filter(|(n, _)| !kx_stub_name(n)).count(), "kernelx export count");
        assert_eq!(r.module_by_name(XGAMERUNTIME).unwrap().exports.len(), 127);
        assert_eq!(r.module_by_name(D3D12X).unwrap().exports.len(), 13);
        assert_eq!(r.module_by_name(UCRT).unwrap().exports.len(), crt_linux::exports().len());
        assert!(r.find_any("XMemAlloc").is_some());
        assert!(r.find_any("D3D12XboxCreateDevice").is_some());
        // Step-10b spot checks: real code, not stub metadata.
        for name in [
            "VirtualAlloc",
            "VirtualProtect",
            "CreateThread",
            "WaitForSingleObject",
            "QueryPerformanceCounter",
            "InitializeCriticalSection",
            "AcquireSRWLockExclusive",
            "CreateFileW",
            "ReadFile",
            "WriteFile",
            "DeleteFileW",
            "CreateDirectoryW",
            "MoveFileExW",
            "FindFirstFileW",
            "FindNextFileW",
            "GetFileAttributesW",
            "GetDiskFreeSpaceExW",
            // Step-11c startup neighbors: FLS, std handles, module handles, environment.
            "FlsAlloc",
            "FlsFree",
            "FlsGetValue",
            "FlsSetValue",
            "IsThreadAFiber",
            "GetStdHandle",
            "GetModuleHandleW",
            "GetModuleHandleA",
            "GetModuleHandleExW",
            "GetEnvironmentVariableW",
        ] {
            let (m, addr) = r.find_any(name).expect(name);
            assert_eq!(m.name, KERNELX);
            assert_ne!(addr, 0, "{name} must be implemented");
        }
        // Steps 10d stay stubs here (names present in the Windows export list).
        for name in ["LoadLibraryW", "XMemAlloc"] {
            let (m, addr) = r.find_any(name).expect(name);
            assert_eq!(m.name, KERNELX);
            assert_eq!(addr, 0, "{name} stays Stub until its slice lands");
        }
        // Step-11 CRT spot checks: real code, reachable from any api-ms-win-crt-*
        // contract through the cross-HLE lookup (no per-contract routes).
        for name in [
            "__p__fmode",
            "__p__commode",
            "__p___argc",
            "__p___argv",
            "__p__environ",
            "_errno",
            "__acrt_iob_func",
            "_initterm",
            "_initterm_e",
            "malloc",
            "free",
            "memcpy",
            "memset",
            "strlen",
        ] {
            let (m, addr) = r.find_any(name).expect(name);
            assert_eq!(m.name, UCRT);
            assert_ne!(addr, 0, "{name} must be implemented");
        }
        // CRT names stay routeless: contracts resolve via the name lookup above.
        assert_eq!(r.hle_for("ucrtbase.dll").map(|m| m.name), None);
        assert_eq!(r.hle_for("api-ms-win-crt-runtime-l1-1-0.dll").map(|m| m.name), None);
    }

    /// Windows `hle_exports![...]` names in `hle/kernelx.rs` (59). Kernel-family fallback
    /// names below are Linux additions and do not change the Windows count.
    fn kx_stub_name(name: &str) -> bool {
        matches!(
            name,
            "RegOpenKeyW"
                | "RegOpenKeyExW"
                | "RegQueryValueExW"
                | "RegCloseKey"
                | "VirtualAlloc"
                | "VirtualAllocEx"
                | "VirtualFree"
                | "VirtualFreeEx"
                | "VirtualProtect"
                | "VirtualProtectEx"
                | "XMemAlloc"
                | "XMemFree"
                | "XMemAllocDefault"
                | "XMemFreeDefault"
                | "XMemSetAllocationHooks"
                | "XMemVirtualQuery"
                | "XMemVirtualAlloc"
                | "XMemAllocatePhysicalPages"
                | "XMemMapPhysicalPages"
                | "XMemMapPhysicalPagesScatter"
                | "XMemFreePhysicalPages"
                | "XMemTransferMemory"
                | "XMemGetWorkingSetStatistics"
                | "TitleMemoryStatus"
                | "JobTitleMemoryStatus"
                | "ToolingMemoryStatus"
                | "AllocateTitlePhysicalPages"
                | "FreeTitlePhysicalPages"
                | "MapTitlePhysicalPages"
                | "RegisterAppStateChangeNotification"
                | "UnregisterAppStateChangeNotification"
                | "RegisterAppConstrainedChangeNotification"
                | "UnregisterAppConstrainedChangeNotification"
                | "SetUnhandledExceptionFilter"
                | "SetThreadName"
                | "GetThreadName"
                | "SetThreadpoolAffinityMask"
                | "SetThreadAffinityMask"
                | "SetThreadGroupAffinity"
                | "QueryProcessorSchedulingStatistics"
                | "GetConsoleType"
                | "OutputDebugStringA"
                | "OutputDebugStringW"
                | "GetSystemOSVersion"
                | "LoadLibraryW"
                | "LoadLibraryA"
                | "LoadLibraryExW"
                | "LoadLibraryExA"
                | "LoadPackagedLibrary"
                | "GetProcAddress"
                | "GetModuleHandleW"
                | "GetModuleHandleA"
                | "GetModuleHandleExW"
                | "GetModuleHandleExA"
                | "GetModuleFileNameW"
                | "GetModuleFileNameA"
                | "FreeLibrary"
                | "FreeLibraryWhenCallbackReturns"
                | "DisableThreadLibraryCalls"
        )
    }

    #[test]
    fn cross_module_lookup_finds_xmem() {
        let r = build(&Config::default());
        assert_eq!(r.find_any("XMemAlloc").map(|(m, _)| m.name), Some(KERNELX));
    }
}
