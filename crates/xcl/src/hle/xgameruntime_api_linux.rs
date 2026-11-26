//! Linux GDK-thunk protocol slice: `InitializeApiImpl(Ex/Ex2)` + `QueryApiImpl`.
//!
//! The Windows `hle/xgameruntime/api.rs` stays the implementation source of truth
//! (GDK `QueryApiImpl(clsid, iid)` hands each API family out as a COM-style vtable
//! built from `gdk_table`, the extracted public-PC-GDK slot map). This module is the
//! Linux twin: same GUIDs, same slot layouts, same XCL-owned objects — but no PC
//! runtime leg (no `System32\xgameruntime.dll` on Linux), so every class XCL does
//! not implement answers `REGDB_E_CLASSNOTREG` and every unimplemented slot logs
//! once and answers `E_NOTIMPL`, both visible in `report.json` (`ApiStats`) and
//! `xcl.log`. The entry points are real `win64` code addresses so the game's
//! `GetProcAddress(xgameruntime.dll, …)` probes and static imports resolve as HLE.

#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

#[path = "xgameruntime/gdk_table.rs"]
mod gdk_table;

use crate::hle::{xgameruntime_linux as xr, xtaskqueue_linux as xq, xuser_linux as xu};
use crate::util::{write_cstr_out, E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER, HRESULT, S_OK};
use xu::XAsyncBlock;

// ---------------------------------------------------------------------------------------
// identities
// ---------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

impl Guid {
    pub const fn from_u128(v: u128) -> Guid {
        Guid((v >> 96) as u32, (v >> 80) as u16, (v >> 64) as u16, (v as u64).to_be_bytes())
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let d = &self.3;
        write!(f, "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}", self.0, self.1, self.2, d[0], d[1], d[2..].iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
}

const fn g(v: u128) -> Guid {
    Guid::from_u128(v)
}

pub const XUSER: Guid = g(0x01acd177_91f9_4763_a38e_ccbb55ce32e0);
/// interface of XUserGetGamertag (with a gamertag component): its own vtable
#[cfg_attr(not(test), allow(dead_code))]
const XUSER_GAMERTAG_IID: Guid = g(0xcef4fac0_7676_4a94_a119_4c43f9eb5b74);
/// older console GDK XUser IID (same base slot layout)
const XUSER_V2_IID: Guid = g(0x1bf2f8c5_d507_4e52_bb05_f726d0e71161);
const XUSER_DEVICE: Guid = g(0x7d824997_10dc_45ab_86b7_2737767c0bf1);
pub const XGAMESAVE: Guid = g(0x704c3f58_e629_4cc2_b197_30511b996fe2);
/// the interface the game's libHttpClient asks for (GDK table: 0b9d556e)
#[cfg_attr(not(test), allow(dead_code))]
const XPACKAGE_OLD_IID: Guid = g(0xe2a4734b_2f4a_456d_aa8f_d065e04fb209);
/// console GDK 19041 XPackage v1 IID: XPackageEnumeratePackages is at slot 28
const XPACKAGE_V1_IID: Guid = g(0xbe3f3e9f_c4b3_424d_9a95_d1af060559a5);
/// console GDK 19041/22000 XPackage v2 IID: EnumeratePackages @ 35, RegisterPackageInstalled @ 36
const XPACKAGE_V2_IID: Guid = g(0xf92d8712_2b27_4d8a_bf01_11a6f8e3eb42);
pub const XPACKAGE: Guid = g(0xaf406016_e850_4aa8_a88d_2f3dcb9dac7e);
pub const XPLS: Guid = g(0xf4faf4d4_2d04_4fce_b3e0_474a713a3e84);
const XPLS_V1_IID: Guid = g(0xd29411df_0794_4553_8b27_95fc02d0f75d);
pub const XGAME: Guid = g(0x973a344e_24bf_4d0f_8457_56c534892b29);
const XGAME_V1_IID: Guid = g(0x50849859_0ad8_4f81_80e4_5bc78626f852);
pub const XSYSTEM: Guid = g(0xe349bd1a_fc20_4e40_b99c_4178cc6b409f);
const XSYSTEM_V2_IID: Guid = g(0x67ce4bfc_b1d1_4ac7_bc3a_cb9219a97a85);
/// console GDK XSystem v3 IID: XSystemGetAppSpecificDeviceId is at slot 4 (not slot 5)
const XSYSTEM_V3_IID: Guid = g(0x6fd71f09_7513_49f0_89bc_bfaf5df6f852);
const XANALYTICS: Guid = g(0xb884675d_b738_4a9c_815d_9a9a1e0c6c9b);
const XFEATURE: Guid = g(0x8836fe87_edb9_4fe3_8dad_05f0d2cd5b40);
const XDISPLAY: Guid = g(0x1b339674_328d_4283_a200_3171f18d3639);
const XGAMEEVENT: Guid = g(0xbbfbdcc7_bfe7_409b_a5ca_edf054960b4d);
pub const XNETWORKING: Guid = g(0x37e56907_2f10_41e8_b72f_36edb185331a);
pub const XERROR: Guid = g(0x8ca467f7_22e8_4096_8456_bb8aa13f79d8);
pub const XGAMEUI: Guid = g(0xdfcd4649_4ff8_4043_ba07_35d607df98b0);
const XSTORE: Guid = g(0x0dd112ac_7c24_448c_b92b_3960fb5bd30c);
const XAPPBROADCAST: Guid = g(0x186d5592_a72d_45fb_9560_11aed0e6647a);
const XAPPCAPTURE: Guid = g(0xa4f1aee2_2f41_4485_b008_a7c26d52ac27);
/// XAsync + XTaskQueue + XThread (iid = clsid)
pub const XASYNC: Guid = g(0x073b7dcb_1fcf_4030_94be_e3c9eb623428);

/// classes that reach Xbox Live / the Store or take XCL's user handles: not available
const REFUSED: [(Guid, &str); 3] = [(XSTORE, "XStore"), (XAPPBROADCAST, "XAppBroadcast"), (XAPPCAPTURE, "XAppCapture")];

/// rows of the GDK table: (function, class, interface, slot)
fn table() -> impl Iterator<Item = (&'static str, Guid, Guid, usize)> {
    gdk_table::API.iter().map(|&(n, c, i, s)| (n, g(c), g(i), s as usize))
}

/// a class's name for the log: the prefix of its first function in the GDK table
fn family(c: &Guid) -> String {
    let Some((n, ..)) = table().find(|r| r.1 == *c) else { return "?".into() };
    let end = n.char_indices().skip(1).filter(|(_, ch)| ch.is_ascii_uppercase()).nth(1).map_or(n.len(), |(k, _)| k);
    n[..end].to_string()
}

const REGDB_E_CLASSNOTREG: HRESULT = 0x8004_0154_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;

// ---------------------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------------------

static INIT_COUNT: AtomicU32 = AtomicU32::new(0);

pub unsafe extern "win64" fn InitializeApiImplEx2(gdk: u64, os: u64, flags: u64, _options: *mut c_void) -> HRESULT {
    if INIT_COUNT.fetch_add(1, Ordering::AcqRel) == 0 {
        crate::xlog_info!("XGameRuntime: InitializeApiImplEx2({gdk:#x}, {os:#x}, flags {flags:#x}) – title goes through the GDK thunks (XCL objects, no PC runtime)");
    }
    S_OK
}

pub unsafe extern "win64" fn InitializeApiImplEx(gdk: u64, os: u64, flags: u64) -> HRESULT {
    crate::xlog_info!("XGameRuntime: InitializeApiImplEx({gdk:#x}, {os:#x}, flags {flags:#x})");
    InitializeApiImplEx2(gdk, os, flags, std::ptr::null_mut())
}

pub unsafe extern "win64" fn InitializeApiImpl(gdk: u64, os: u64) -> HRESULT {
    crate::xlog_info!("XGameRuntime: InitializeApiImpl({gdk:#x}, {os:#x})");
    InitializeApiImplEx2(gdk << 32 | (os & 0xFFFF_FFFF), 0, 0, std::ptr::null_mut())
}

pub unsafe extern "win64" fn UninitializeApiImpl() {
    // saturating: titles may uninitialize without (or more often than) initializing
    if INIT_COUNT.load(Ordering::Acquire) > 0 {
        INIT_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
}

pub unsafe extern "win64" fn QueryApiImpl(clsid: *const Guid, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || iid.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let (c, i) = (*clsid, *iid);
    if let Some((_, name)) = REFUSED.iter().find(|r| r.0 == c) {
        note(c, i, format!("{name}: unavailable (offline)"));
        return REGDB_E_CLASSNOTREG;
    }
    if let Some(o) = own(&c, &i) {
        let known = o.iface.iids.contains(&i) || i == c;
        note(c, i, format!("{}: XCL{}", o.iface.name, if known { "" } else { " (interface version not in the GDK table: the main vtable)" }));
        *out = o as *const Obj as *mut c_void;
        return S_OK;
    }
    note(c, i, format!("{}: missing", family(&c)));
    REGDB_E_CLASSNOTREG
}

// ---------------------------------------------------------------------------------------
// XCL's API objects
// ---------------------------------------------------------------------------------------

/// One vtable of an XCL class.
struct Iface {
    name: &'static str,
    clsid: Guid,
    /// interface ids this vtable answers
    iids: Vec<Guid>,
    /// the class's main vtable: also for interface ids the GDK table does not know
    main: bool,
    /// slot → function (GDK table)
    names: Vec<Option<&'static str>>,
    /// (slot, function): the API function itself, without `this` (a shim drops it)
    slots: Vec<(usize, usize)>,
}

/// A class XCL answers for: its functions by GDK name.
struct Class {
    name: &'static str,
    clsid: Guid,
    impls: Vec<(&'static str, usize)>,
}

/// Functions that return bool (an unimplemented one answers false).
fn returns_bool(name: &str) -> bool {
    let rest = name.trim_start_matches('X');
    let verb = rest.char_indices().skip(1).find(|(_, ch)| ch.is_ascii_uppercase()).map_or("", |(k, _)| &rest[k..]);
    verb.starts_with("Is") || verb.contains("Unregister") || name == "XPackageUpdateInstallationMonitor" || name == "XPackageUninstallPackage"
}

#[repr(C)]
struct Obj {
    vtbl: *const usize,
    iface: Iface,
    /// slot → API function the shim calls
    targets: [usize; SLOTS],
}

unsafe impl Sync for Obj {}
unsafe impl Send for Obj {}

const SLOTS: usize = 96;

const IID_IUNKNOWN: Guid = g(0x0000_0000_0000_0000_c000_0000_0000_0046);

unsafe extern "win64" fn query_interface(this: *const Obj, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    let f = &(*this).iface;
    if *iid == IID_IUNKNOWN {
        // COM identity rule: every interface on one class must return the same canonical
        // IUnknown pointer (the class's main object, or `this` for standalone fallback objects).
        let canonical = objects().iter().copied()
            .find(|o| o.iface.clsid == f.clsid && o.iface.main)
            .map_or(this, |o| o as *const Obj);
        *out = canonical as *mut c_void;
        return S_OK;
    }
    if *iid == f.clsid || f.iids.contains(&*iid) {
        *out = this as *mut c_void;
        return S_OK;
    }
    if let Some(other) = own(&f.clsid, &*iid) {
        if other.iface.iids.contains(&*iid) || (f.main && std::ptr::eq(other, this)) {
            *out = other as *const Obj as *mut c_void;
            return S_OK;
        }
    }
    *out = std::ptr::null_mut();
    // a title asserting on this (a newer interface version of the class?) ends without a trace otherwise
    crate::xlog_warn!("XGameRuntime: {} QueryInterface({}) refused (E_NOINTERFACE) – an interface version XCL's GDK table does not know?", f.name, *iid);
    E_NOINTERFACE
}

unsafe extern "win64" fn add_ref(_this: *const Obj) -> u32 {
    1
}

static UNIMPLEMENTED: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

unsafe fn unknown_slot(this: *const Obj, slot: usize) -> usize {
    let f = &(*this).iface;
    let key = match f.names.get(slot).copied().flatten() {
        Some(n) => n.to_string(),
        None => format!("{}#{slot}", f.name),
    };
    let first = {
        let mut g = UNIMPLEMENTED.lock().unwrap();
        let n = g.entry(key.clone()).or_insert(0);
        *n += 1;
        *n == 1
    };
    if first {
        crate::xlog_warn!("XGameRuntime: {key} – not supported (E_NOTIMPL)");
    }
    if f.names.get(slot).copied().flatten().is_some_and(returns_bool) {
        0
    } else {
        E_NOTIMPL as u32 as usize
    }
}

/// Slot `slot` of an XCL object: its API function with the caller's arguments after `this`.
unsafe fn call_target(this: *const Obj, slot: usize, a: [usize; 9]) -> usize {
    let f: unsafe extern "win64" fn(usize, usize, usize, usize, usize, usize, usize, usize, usize) -> usize = std::mem::transmute((*this).targets[slot]);
    f(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8])
}

type Fwd = unsafe extern "win64" fn(*const Obj, usize, usize, usize, usize, usize, usize, usize, usize, usize) -> usize;

macro_rules! per_slot {
    ($($n:literal)*) => {
        const SHIMS: [Fwd; SLOTS] = [$({
            unsafe extern "win64" fn f(this: *const Obj, a: usize, b: usize, c: usize, d: usize, e: usize, g: usize, h: usize, i: usize, j: usize) -> usize {
                call_target(this, $n, [a, b, c, d, e, g, h, i, j])
            }
            f
        }),*];
        const STUBS: [unsafe extern "win64" fn(*const Obj) -> usize; SLOTS] = [$({
            unsafe extern "win64" fn f(this: *const Obj) -> usize {
                unknown_slot(this, $n)
            }
            f
        }),*];
    };
}
per_slot!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
    40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79
    80 81 82 83 84 85 86 87 88 89 90 91 92 93 94 95);

fn object(iface: Iface) -> &'static Obj {
    let mut v: Vec<usize> = (0..SLOTS).map(|k| STUBS[k] as usize).collect();
    v[0] = query_interface as usize;
    v[1] = add_ref as usize;
    v[2] = add_ref as usize;
    let mut targets = [0usize; SLOTS];
    for &(k, f) in &iface.slots {
        v[k] = SHIMS[k] as usize;
        targets[k] = f;
    }
    let vtbl = Box::leak(v.into_boxed_slice()).as_ptr();
    Box::leak(Box::new(Obj { vtbl, iface, targets }))
}

/// The vtables of a class from the GDK table: interface ids whose slots do not collide share
/// one (the one holding the class id itself, or the first, is the main one).
fn layout(c: Class) -> Vec<Iface> {
    let mut by_iid: BTreeMap<Guid, Vec<(usize, &'static str)>> = BTreeMap::new();
    for (n, cl, i, slot) in table() {
        if cl == c.clsid && slot < SLOTS {
            by_iid.entry(i).or_default().push((slot, n));
        }
    }
    let mut order: Vec<Guid> = by_iid.keys().copied().collect();
    order.sort_by_key(|i| (*i != c.clsid, by_iid[i].iter().map(|r| r.0).min()));
    let mut groups: Vec<(Vec<Guid>, Vec<Option<&'static str>>)> = Vec::new();
    for i in order {
        let rows = &by_iid[&i];
        let free = groups.iter().position(|(_, names)| rows.iter().all(|&(s, _)| names[s].is_none()));
        let k = free.unwrap_or_else(|| {
            groups.push((Vec::new(), vec![None; SLOTS]));
            groups.len() - 1
        });
        groups[k].0.push(i);
        for &(s, n) in rows {
            groups[k].1[s] = Some(n);
        }
    }
    groups
        .into_iter()
        .enumerate()
        .map(|(k, (iids, names))| {
            let slot_of = |f: &str| names.iter().position(|n| *n == Some(f));
            Iface {
                name: c.name,
                clsid: c.clsid,
                main: k == 0,
                slots: c.impls.iter().filter_map(|&(f, p)| Some((slot_of(f)?, p))).collect(),
                iids,
                names,
            }
        })
        .collect()
}

fn classes() -> Vec<Class> {
    vec![
        Class { name: "XUser", clsid: XUSER, impls: xuser() },
        Class { name: "XUser (devices)", clsid: XUSER_DEVICE, impls: xuser_devices() },
        Class { name: "XGameSave", clsid: XGAMESAVE, impls: xgamesave() },
        Class { name: "XPersistentLocalStorage", clsid: XPLS, impls: xpls() },
        Class { name: "XPackage", clsid: XPACKAGE, impls: xpackage() },
        Class { name: "XGame", clsid: XGAME, impls: xgame() },
        Class { name: "XSystem", clsid: XSYSTEM, impls: xsystem() },
        Class { name: "XSystemAnalytics", clsid: XANALYTICS, impls: named![XSystemGetAnalyticsInfo] },
        Class { name: "XGameRuntimeFeature", clsid: XFEATURE, impls: vec![("XGameRuntimeIsFeatureAvailable", xr::XGameRuntimeIsFeatureAvailable as usize)] },
        Class {
            name: "XDisplay",
            clsid: XDISPLAY,
            impls: vec![("XLaunchUri", launch_uri as usize), ("XDisplayAcquireTimeoutDeferral", display_deferral as usize), ("XDisplayCloseTimeoutDeferralHandle", nothing as usize)],
        },
        // telemetry: offline, accepted and dropped
        Class { name: "XGameEvent", clsid: XGAMEEVENT, impls: vec![("XGameEventWrite", ok as usize)] },
        Class { name: "XNetworking", clsid: XNETWORKING, impls: xnetworking() },
        Class { name: "XError", clsid: XERROR, impls: xerror() },
        // dialogs that need Xbox Live or an account: each Async completes at once with
        // E_NOTIMPL and each Result returns that status (logged, in the report); the
        // message, error and text dialogs, which take no user, stay unimplemented stubs
        Class { name: "XGameUi", clsid: XGAMEUI, impls: xgameui_refused() },
        Class { name: "XAsync", clsid: XASYNC, impls: xasync_impls() },
    ]
}

/// Build an explicit legacy console interface vtable from `(slot, function_name)` rows.
fn legacy_iface(
    name: &'static str,
    clsid: Guid,
    iid: Guid,
    rows: &[(usize, &'static str)],
    impls: &[(&'static str, usize)],
) -> Iface {
    let mut names = vec![None; SLOTS];
    let mut slots = Vec::new();
    for &(slot, fname) in rows {
        if slot < SLOTS {
            names[slot] = Some(fname);
            if let Some(&(_, ptr)) = impls.iter().find(|&&(n, _)| n == fname) {
                slots.push((slot, ptr));
            }
        }
    }
    Iface { name, clsid, iids: vec![iid], main: false, names, slots }
}

/// Older console GDK (19041 / 2020) interface versions whose vtable slot indices differ
/// from the PC GDK 261000 table.
fn legacy_ifaces() -> Vec<Iface> {
    let pkg = xpackage();
    let sys = xsystem();
    let pkg_v1 = [
        (3, "XPackageGetCurrentProcessPackageIdentifier"),
        (4, "XPackageIsPackagedProcess"),
        (5, "XPackageCreateInstallationMonitor"),
        (6, "XPackageCloseInstallationMonitorHandle"),
        (7, "XPackageGetInstallationProgress"),
        (8, "XPackageUpdateInstallationMonitor"),
        (9, "XPackageRegisterInstallationProgressChanged"),
        (10, "XPackageUnregisterInstallationProgressChanged"),
        (11, "XPackageGetUserLocale"),
        (12, "XPackageFindChunkAvailability"),
        (13, "XPackageEnumerateChunkAvailability"),
        (14, "XPackageChangeChunkInstallOrder"),
        (16, "XPackageInstallChunksAsync"),
        (17, "XPackageInstallChunksResult"),
        (28, "XPackageEnumeratePackages"),
    ];
    let pkg_v2 = [
        (3, "XPackageGetCurrentProcessPackageIdentifier"),
        (4, "XPackageIsPackagedProcess"),
        (5, "XPackageCreateInstallationMonitor"),
        (6, "XPackageCloseInstallationMonitorHandle"),
        (7, "XPackageGetInstallationProgress"),
        (8, "XPackageUpdateInstallationMonitor"),
        (9, "XPackageRegisterInstallationProgressChanged"),
        (10, "XPackageUnregisterInstallationProgressChanged"),
        (11, "XPackageGetUserLocale"),
        (18, "XPackageEstimateDownloadSize"),
        (22, "XPackageUnregisterPackageInstalled"),
        (24, "XPackageGetMountPathSize"),
        (25, "XPackageGetMountPath"),
        (26, "XPackageCloseMountHandle"),
        (30, "XPackageGetWriteStats"),
        (33, "XPackageEnumerateFeatures"),
        (35, "XPackageEnumeratePackages"),
        (36, "XPackageRegisterPackageInstalled"),
    ];
    let sys_v3 = [(4, "XSystemGetAppSpecificDeviceId")];
    vec![
        legacy_iface("XPackage", XPACKAGE, XPACKAGE_V1_IID, &pkg_v1, &pkg),
        legacy_iface("XPackage", XPACKAGE, XPACKAGE_V2_IID, &pkg_v2, &pkg),
        legacy_iface("XSystem", XSYSTEM, XSYSTEM_V3_IID, &sys_v3, &sys),
    ]
}

fn objects() -> &'static [&'static Obj] {
    static O: OnceLock<Vec<&'static Obj>> = OnceLock::new();
    O.get_or_init(|| {
        let mut ifaces: Vec<Iface> = classes().into_iter().flat_map(layout).collect();
        // Register older compatible console IIDs whose slot layouts match the main vtable.
        for f in &mut ifaces {
            if !f.main {
                continue;
            }
            match f.clsid {
                c if c == XUSER => f.iids.push(XUSER_V2_IID),
                c if c == XSYSTEM => f.iids.push(XSYSTEM_V2_IID),
                c if c == XGAME => f.iids.push(XGAME_V1_IID),
                c if c == XPLS => f.iids.push(XPLS_V1_IID),
                c if c == XNETWORKING => f.iids.push(XNETWORKING),
                _ => {}
            }
        }
        ifaces.extend(legacy_ifaces());
        ifaces.into_iter().map(object).collect()
    })
}

/// XCL's object for (clsid, iid): the vtable holding that interface, else the class's main one.
fn own(c: &Guid, i: &Guid) -> Option<&'static Obj> {
    let all = objects();
    all.iter().copied().find(|o| o.iface.clsid == *c && o.iface.iids.contains(i)).or_else(|| all.iter().copied().find(|o| o.iface.clsid == *c && o.iface.main))
}

/// `(name, function)` pairs for functions named like their GDK function
macro_rules! named {
    ($($f:ident),* $(,)?) => {
        vec![$( (stringify!($f), $f as usize) ),*]
    };
}
use named;

fn xuser() -> Vec<(&'static str, usize)> {
    let mut v = vec![
        ("XUserDuplicateHandle", xu::XUserDuplicateHandle as usize),
        ("XUserCloseHandle", xu::XUserCloseHandle as usize),
        ("XUserCompare", xu::XUserCompare as usize),
        ("XUserGetMaxUsers", xu::XUserGetMaxUsers as usize),
        ("XUserAddAsync", xu::XUserAddAsync as usize),
        ("XUserAddResult", xu::XUserAddResult as usize),
        ("XUserGetLocalId", xu::XUserGetLocalId as usize),
        ("XUserGetId", xu::XUserGetId as usize),
        ("XUserGetIsGuest", xu::XUserGetIsGuest as usize),
        ("XUserGetState", xu::XUserGetState as usize),
        ("XUserGetGamertag", xu::XUserGetGamertag as usize),
        ("XUserGetAgeGroup", xu::XUserGetAgeGroup as usize),
        ("XUserCheckPrivilege", XUserCheckPrivilege as usize),
        ("XUserRegisterForChangeEvent", xu::XUserRegisterForChangeEvent as usize),
        ("XUserUnregisterForChangeEvent", xu::XUserUnregisterForChangeEvent as usize),
        ("XUserGetTokenAndSignatureAsync", xu::XUserGetTokenAndSignatureAsync as usize),
        ("XUserGetTokenAndSignatureResultSize", xu::XUserGetTokenAndSignatureResultSize as usize),
        ("XUserGetTokenAndSignatureResult", xu::XUserGetTokenAndSignatureResult as usize),
        ("XUserGetTokenAndSignatureUtf16Async", xu::XUserGetTokenAndSignatureUtf16Async as usize),
        ("XUserGetTokenAndSignatureUtf16ResultSize", xu::XUserGetTokenAndSignatureUtf16ResultSize as usize),
        ("XUserGetTokenAndSignatureUtf16Result", xu::XUserGetTokenAndSignatureUtf16Result as usize),
    ];
    v.extend([
        ("XUserFindUserByLocalId", xu::XUserFindUserByLocalId as usize),
        ("XUserFindUserById", xu::XUserFindUserById as usize),
        ("XUserResolvePrivilegeWithUiAsync", resolve_privilege_async as usize),
        ("XUserResolvePrivilegeWithUiResult", empty_result as usize),
        ("XUserResolveIssueWithUiAsync", resolve_issue_async as usize),
        ("XUserResolveIssueWithUiResult", empty_result as usize),
        ("XUserResolveIssueWithUiUtf16Async", resolve_issue_async as usize),
        ("XUserResolveIssueWithUiUtf16Result", empty_result as usize),
        ("XUserGetSignOutDeferral", sign_out_deferral as usize),
        ("XUserCloseSignOutDeferralHandle", nothing as usize),
        ("XUserAddByIdWithUiAsync", add_by_id_async as usize),
        ("XUserAddByIdWithUiResult", xu::XUserAddResult as usize),
        ("XUserIsStoreUser", no as usize),
        ("XUserPlatformRemoteConnectSetEventHandlers", ok as usize),
        ("XUserPlatformRemoteConnectCancelPrompt", ok as usize),
        ("XUserPlatformSpopPromptSetEventHandlers", ok as usize),
        ("XUserPlatformSpopPromptComplete", ok as usize),
        ("XUserIsSignOutPresent", no as usize),
        ("XUserSignOutAsync", sign_out_async as usize),
        ("XUserSignOutResult", empty_result as usize),
    ]);
    v
}

/// device association: every controller belongs to the single local signed-in user
fn xuser_devices() -> Vec<(&'static str, usize)> {
    vec![
        ("XUserRegisterForDeviceAssociationChanged", xu::XUserRegisterForDeviceAssociationChanged as usize),
        ("XUserUnregisterForDeviceAssociationChanged", xu::XUserUnregisterForDeviceAssociationChanged as usize),
        ("XUserRegisterForDefaultAudioEndpointUtf16Changed", register_token4 as usize),
        ("XUserUnregisterForDefaultAudioEndpointUtf16Changed", yes as usize),
        ("XUserFindForDevice", xu::XUserFindForDevice as usize),
        ("XUserFindControllerForUserWithUiAsync", xu::XUserFindControllerForUserWithUiAsync as usize),
        ("XUserFindControllerForUserWithUiResult", xu::XUserFindControllerForUserWithUiResult as usize),
    ]
}

fn xgamesave() -> Vec<(&'static str, usize)> {
    vec![
        ("XGameSaveFilesGetFolderWithUiAsync", xu::XGameSaveFilesGetFolderWithUiAsync as usize),
        ("XGameSaveFilesGetFolderWithUiResult", xu::XGameSaveFilesGetFolderWithUiResult as usize),
        ("XGameSaveFilesGetRemainingQuota", files_remaining_quota as usize),
    ]
}

fn xpls() -> Vec<(&'static str, usize)> {
    let mut v = vec![
        ("XPersistentLocalStorageGetPathSize", xu::XPersistentLocalStorageGetPathSize as usize),
        ("XPersistentLocalStorageGetPath", xu::XPersistentLocalStorageGetPath as usize),
        ("XPersistentLocalStorageGetSpaceInfo", xu::XPersistentLocalStorageGetSpaceInfo as usize),
        ("XPersistentLocalStorageMountForPackage", XPersistentLocalStorageMountForPackage as usize),
    ];
    // no prompt to show: the space asked for is granted
    v.extend([("XPersistentLocalStoragePromptUserForSpaceAsync", prompt_space_async as usize), ("XPersistentLocalStoragePromptUserForSpaceResult", empty_result as usize)]);
    v
}

fn xpackage() -> Vec<(&'static str, usize)> {
    let mut v = vec![
        ("XPackageGetCurrentProcessPackageIdentifier", xr::XPackageGetCurrentProcessPackageIdentifier as usize),
        ("XPackageGetIdentifier", xr::XPackageGetIdentifier as usize),
        ("XPackageIsPackagedProcess", xr::XPackageIsPackagedProcess as usize),
        ("XPackageCreateInstallationMonitor", XPackageCreateInstallationMonitor as usize),
        ("XPackageGetInstallationProgress", XPackageGetInstallationProgress as usize),
        ("XPackageRegisterInstallationProgressChanged", XPackageRegisterInstallationProgressChanged as usize),
        ("XPackageUnregisterInstallationProgressChanged", XPackageUnregisterInstallationProgressChanged as usize),
        ("XPackageFindChunkAvailability", XPackageFindChunkAvailability as usize),
        ("XPackageEstimateDownloadSize", XPackageEstimateDownloadSize as usize),
        ("XPackageGetMountPathSize", XPackageGetMountPathSize as usize),
        ("XPackageGetMountPath", XPackageGetMountPath as usize),
        ("XPackageCloseMountHandle", XPackageCloseMountHandle as usize),
        ("XPackageGetWriteStats", XPackageGetWriteStats as usize),
        ("XPackageGetPackageKind", XPackageGetPackageKind as usize),
        ("XPackageEnumeratePackages", enumerate_packages as usize),
    ];
    v.extend([
        ("XPackageCloseInstallationMonitorHandle", nothing as usize),
        ("XPackageUpdateInstallationMonitor", no as usize),
        ("XPackageEnumerateChunkAvailability", ok as usize),
        ("XPackageChangeChunkInstallOrder", ok as usize),
        ("XPackageInstallChunks", install_chunks as usize),
        ("XPackageInstallChunksAsync", install_chunks_async as usize),
        ("XPackageInstallChunksResult", install_chunks_result as usize),
        ("XPackageUninstallChunks", ok as usize),
        ("XPackageRegisterPackageInstalled", register_token4 as usize),
        ("XPackageUnregisterPackageInstalled", yes as usize),
        ("XPackageEnumerateFeatures", ok as usize),
        ("XPackageUninstallPackage", no as usize),
        ("XPackageMountWithUiAsync", mount_with_ui_async as usize),
        ("XPackageMountWithUiResult", mount_with_ui_result as usize),
    ]);
    v
}

fn xgame() -> Vec<(&'static str, usize)> {
    named![XGameGetXboxTitleId, XLaunchNewGame, XLaunchRestartOnCrash]
}

fn xsystem() -> Vec<(&'static str, usize)> {
    let mut v = vec![
        ("XSystemGetConsoleId", xr::XSystemGetConsoleId as usize),
        ("XSystemGetXboxLiveSandboxId", xr::XSystemGetXboxLiveSandboxId as usize),
        ("XSystemGetAppSpecificDeviceId", XSystemGetAppSpecificDeviceId as usize),
    ];
    v.extend([("XSystemHandleTrack", ok as usize), ("XSystemIsHandleValid", yes as usize), ("XSystemAllowFullDownloadBandwidth", nothing as usize)]);
    v
}

fn xnetworking() -> Vec<(&'static str, usize)> {
    vec![
        ("XNetworkingQueryPreferredLocalUdpMultiplayerPort", xq::XNetworkingQueryPreferredLocalUdpMultiplayerPort as usize),
        ("XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged", xq::XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged as usize),
        ("XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged", xq::XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged as usize),
        ("XNetworkingGetConnectivityHint", xq::XNetworkingGetConnectivityHint as usize),
        ("XNetworkingRegisterConnectivityHintChanged", xq::XNetworkingRegisterConnectivityHintChanged as usize),
        ("XNetworkingUnregisterConnectivityHintChanged", xq::XNetworkingUnregisterConnectivityHintChanged as usize),
    ]
}

fn xerror() -> Vec<(&'static str, usize)> {
    vec![
        ("XErrorReport", XErrorReport as usize),
        ("XErrorSetCallback", error_set_callback as usize),
        ("XErrorSetOptions", error_set_options as usize),
    ]
}

fn xasync_impls() -> Vec<(&'static str, usize)> {
    vec![
        ("XAsyncGetStatus", xu::XAsyncGetStatus as usize),
        ("XAsyncGetResultSize", xu::XAsyncGetResultSize as usize),
        ("XAsyncCancel", xu::XAsyncCancel as usize),
        ("XAsyncRun", xu::XAsyncRun as usize),
        ("XAsyncBegin", xu::XAsyncBegin as usize),
        ("XAsyncSchedule", xu::XAsyncSchedule as usize),
        ("XAsyncComplete", xu::XAsyncComplete as usize),
        ("XAsyncGetResult", xu::XAsyncGetResult as usize),
        ("XTaskQueueCreate", xq::XTaskQueueCreate as usize),
        ("XTaskQueueCreateComposite", xq::XTaskQueueCreateComposite as usize),
        ("XTaskQueueGetPort", xq::XTaskQueueGetPort as usize),
        ("XTaskQueueDuplicateHandle", xq::XTaskQueueDuplicateHandle as usize),
        ("XTaskQueueDispatch", xq::XTaskQueueDispatch as usize),
        ("XTaskQueueCloseHandle", xq::XTaskQueueCloseHandle as usize),
        ("XTaskQueueSubmitCallback", xq::XTaskQueueSubmitCallback as usize),
        ("XTaskQueueSubmitDelayedCallback", xq::XTaskQueueSubmitDelayedCallback as usize),
        ("XTaskQueueRegisterWaiter", xq::XTaskQueueRegisterWaiter as usize),
        ("XTaskQueueUnregisterWaiter", xq::XTaskQueueUnregisterWaiter as usize),
        ("XTaskQueueTerminate", xq::XTaskQueueTerminate as usize),
        ("XTaskQueueRegisterMonitor", xq::XTaskQueueRegisterMonitor as usize),
        ("XTaskQueueUnregisterMonitor", xq::XTaskQueueUnregisterMonitor as usize),
        ("XTaskQueueGetCurrentProcessTaskQueue", xq::XTaskQueueGetCurrentProcessTaskQueue as usize),
        ("XTaskQueueSetCurrentProcessTaskQueue", xq::XTaskQueueSetCurrentProcessTaskQueue as usize),
        ("XThreadSetTimeSensitive", XThreadSetTimeSensitive as usize),
        ("XThreadVerifyNotTimeSensitive", XThreadVerifyNotTimeSensitive as usize),
        ("XThreadAssertNotTimeSensitive", XThreadAssertNotTimeSensitive as usize),
        ("XThreadIsTimeSensitive", XThreadIsTimeSensitive as usize),
    ]
}

// ---------------------------------------------------------------------------------------
// functions only the thunk path has
// ---------------------------------------------------------------------------------------

unsafe extern "win64" fn nothing(_a: usize) {}

unsafe extern "win64" fn no(_a: usize) -> bool {
    false
}

unsafe extern "win64" fn yes() -> bool {
    true
}

unsafe extern "win64" fn ok() -> HRESULT {
    S_OK
}

const E_GAMEUSER_DEFERRAL_NOT_AVAILABLE: HRESULT = 0x8924_5103_u32 as i32;
const E_GAMEUSER_USER_NOT_FOUND: HRESULT = 0x8924_5104_u32 as i32;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Register*(queue, context, callback, token): a token, nothing will be called
unsafe extern "win64" fn register_token4(_q: usize, _ctx: usize, _cb: usize, token: *mut u64) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    *token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    S_OK
}

static EMPTY_ID: u8 = 0;

/// an async call that completes at once with no result
unsafe fn completed(b: *mut XAsyncBlock) -> HRESULT {
    xu::begin_with(b, &EMPTY_ID as *const u8 as usize, || Ok(Vec::new()))
}

/// the Result of such a call: its status
unsafe extern "win64" fn empty_result(b: *mut XAsyncBlock) -> HRESULT {
    xu::XAsyncGetStatus(b, false)
}

unsafe extern "win64" fn resolve_privilege_async(_user: usize, _options: u32, _privilege: u32, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "win64" fn resolve_issue_async(_user: usize, _url: usize, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

/// An XGameUi dialog XCL cannot show (the Xbox Live and account UI): the async call completes
/// at once with E_NOTIMPL, so the title gets an answer instead of waiting for a dialog, and
/// its Result function returns that status.
unsafe fn ui_refused(name: &'static str, b: *mut XAsyncBlock) -> HRESULT {
    crate::xlog_warn!("XGameRuntime: {name} – no UI offline (completes with E_NOTIMPL)");
    xu::begin_with(b, &EMPTY_ID as *const u8 as usize, || Err(E_NOTIMPL))
}

macro_rules! ui_refused_fns {
    ($($f:ident => $name:literal),* $(,)?) => {
        $(unsafe extern "win64" fn $f(b: *mut XAsyncBlock) -> HRESULT {
            ui_refused($name, b)
        })*
    };
}
ui_refused_fns! {
    ui_achievements_async => "XGameUiShowAchievementsAsync",
    ui_player_picker_async => "XGameUiShowPlayerPickerAsync",
    ui_profile_card_async => "XGameUiShowPlayerProfileCardAsync",
    ui_send_invite_async => "XGameUiShowSendGameInviteAsync",
    ui_state_share_async => "XGameUiShowStateShareAsync",
    ui_activity_invite_async => "XGameUiShowMultiplayerActivityGameInviteAsync",
    ui_web_auth_async => "XGameUiShowWebAuthenticationAsync",
    ui_web_auth_options_async => "XGameUiShowWebAuthenticationWithOptionsAsync",
}

/// The XGameUi dialogs XCL refuses: each Async completes with E_NOTIMPL, each Result returns
/// that status (the buffers after the XAsyncBlock are left untouched).
fn xgameui_refused() -> Vec<(&'static str, usize)> {
    vec![
        ("XGameUiShowAchievementsAsync", ui_achievements_async as usize),
        ("XGameUiShowAchievementsResult", empty_result as usize),
        ("XGameUiShowPlayerPickerAsync", ui_player_picker_async as usize),
        ("XGameUiShowPlayerPickerResultCount", empty_result as usize),
        ("XGameUiShowPlayerPickerResult", empty_result as usize),
        ("XGameUiShowPlayerProfileCardAsync", ui_profile_card_async as usize),
        ("XGameUiShowPlayerProfileCardResult", empty_result as usize),
        ("XGameUiShowSendGameInviteAsync", ui_send_invite_async as usize),
        ("XGameUiShowSendGameInviteResult", empty_result as usize),
        ("XGameUiShowStateShareAsync", ui_state_share_async as usize),
        ("XGameUiShowStateShareResult", empty_result as usize),
        ("XGameUiShowMultiplayerActivityGameInviteAsync", ui_activity_invite_async as usize),
        ("XGameUiShowMultiplayerActivityGameInviteResult", empty_result as usize),
        ("XGameUiShowWebAuthenticationAsync", ui_web_auth_async as usize),
        ("XGameUiShowWebAuthenticationWithOptionsAsync", ui_web_auth_options_async as usize),
        ("XGameUiShowWebAuthenticationResultSize", empty_result as usize),
        ("XGameUiShowWebAuthenticationResult", empty_result as usize),
    ]
}

unsafe extern "win64" fn sign_out_async(_user: usize, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "win64" fn prompt_space_async(_bytes: u64, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "win64" fn sign_out_deferral(out: *mut usize) -> HRESULT {
    if !out.is_null() {
        *out = 0;
    }
    E_GAMEUSER_DEFERRAL_NOT_AVAILABLE
}

/// XUserAddByIdWithUiAsync: the local user by either of its ids (XUserAddByIdWithUiResult =
/// XUserAddResult)
unsafe extern "win64" fn add_by_id_async(id: u64, b: *mut XAsyncBlock) -> HRESULT {
    let mut h = std::ptr::null_mut();
    let hr = xu::XUserFindUserByLocalId(id, &mut h);
    let hr = if hr >= 0 { hr } else { xu::XUserFindUserById(id, &mut h) };
    if hr < 0 {
        return E_GAMEUSER_USER_NOT_FOUND;
    }
    xu::XUserCloseHandle(h);
    xu::XUserAddAsync(0, b)
}

/// Opening links on the title's behalf would act on the PC: refused.
unsafe extern "win64" fn launch_uri(_user: usize, uri: *const u8) -> HRESULT {
    crate::xlog_warn!("XLaunchUri({}) – refused", crate::util::from_cstr_ptr(uri).unwrap_or_default());
    E_NOTIMPL
}

/// nothing to defer on a PC
unsafe extern "win64" fn display_deferral(out: *mut usize) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 1;
    S_OK
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

#[repr(C)]
pub struct XSystemAnalyticsInfo {
    pub os_version: XVersion,
    pub hosting_os_version: XVersion,
    pub family: [u8; 64],
    pub form: [u8; 64],
}

fn fixed64(s: &str) -> [u8; 64] {
    let mut a = [0u8; 64];
    let b = s.as_bytes();
    a[..b.len().min(63)].copy_from_slice(&b[..b.len().min(63)]);
    a
}

/// What a console reports: the Xbox OS build, family "Windows.Xbox".
pub unsafe extern "win64" fn XSystemGetAnalyticsInfo() -> XSystemAnalyticsInfo {
    let v = XVersion { major: 10, minor: 0, build: 25398, revision: 0 };
    let form = if crate::state::config().is_series_s() { "Xbox Series S" } else { "Xbox Series X" };
    XSystemAnalyticsInfo { os_version: v, hosting_os_version: v, family: fixed64("Windows.Xbox"), form: fixed64(form) }
}

fn package_identifier() -> String {
    let c = crate::state::config();
    if !c.package_identifier.is_empty() {
        return c.package_identifier.clone();
    }
    let title = crate::state::boot().map(|b| b.title.clone()).unwrap_or_else(|| "XCL.Title".into());
    format!("{title}_1.0.0.0_x64__xcl")
}

fn game_root() -> String {
    crate::state::boot().map(|b| b.game_root.to_string_lossy().into_owned()).unwrap_or_else(|| ".".into())
}

#[cfg(test)]
pub(super) static LIVE_MOUNTS: AtomicUsize = AtomicUsize::new(0);

struct Mount {
    path: String,
}

pub unsafe extern "win64" fn XPackageMount(id: *const u8, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let id = crate::util::from_cstr_ptr(id).unwrap_or_default();
    if !id.is_empty() && !id.eq_ignore_ascii_case(&package_identifier()) {
        crate::xlog_warn!("XPackageMount({id}) – unknown package (DLC?)");
        return crate::util::E_NOT_FOUND;
    }
    #[cfg(test)]
    LIVE_MOUNTS.fetch_add(1, Ordering::SeqCst);
    *out = Box::into_raw(Box::new(Mount { path: game_root() })) as *mut c_void;
    S_OK
}

pub unsafe extern "win64" fn XPackageGetMountPathSize(m: *mut c_void, size: *mut usize) -> HRESULT {
    if m.is_null() || size.is_null() {
        return E_INVALIDARG;
    }
    let mount = &*(m as *const Mount);
    *size = mount.path.len() + 1;
    S_OK
}

pub unsafe extern "win64" fn XPackageGetMountPath(m: *mut c_void, size: usize, buf: *mut u8) -> HRESULT {
    if m.is_null() {
        return E_INVALIDARG;
    }
    write_cstr_out(&(*(m as *const Mount)).path, buf, size, std::ptr::null_mut())
}

pub unsafe extern "win64" fn XPackageCloseMountHandle(m: *mut c_void) {
    if !m.is_null() {
        #[cfg(test)]
        LIVE_MOUNTS.fetch_sub(1, Ordering::SeqCst);
        drop(Box::from_raw(m as *mut Mount));
    }
}

/// `XPersistentLocalStorageMountForPackage(packageIdentifier, mountHandle)`: the running
/// game's PLS as an XPackage mount handle (path through XPackageGetMountPath(Size), closed
/// with XPackageCloseMountHandle). Any other package has no PLS here:
/// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND), what the GDK reference gives for a title whose
/// PLS does not exist.
pub unsafe extern "win64" fn XPersistentLocalStorageMountForPackage(id: *const u8, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let Some(id) = crate::util::from_cstr_ptr(id).filter(|s| !s.is_empty()) else { return E_INVALIDARG };
    if !id.eq_ignore_ascii_case(&package_identifier()) {
        crate::xlog_warn!("XPersistentLocalStorageMountForPackage({id}) – no such package here");
        return crate::util::hresult_from_win32(2); // ERROR_FILE_NOT_FOUND
    }
    #[cfg(test)]
    LIVE_MOUNTS.fetch_add(1, Ordering::SeqCst);
    *out = Box::into_raw(Box::new(Mount { path: xu::pls_path() })) as *mut c_void;
    S_OK
}

/// every chunk is installed (XPackageChunkAvailability::Ready = 0)
unsafe extern "win64" fn XPackageFindChunkAvailability(_id: usize, _count: u32, _sel: usize, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0;
    S_OK
}

unsafe extern "win64" fn XPackageEstimateDownloadSize(_id: usize, _count: u32, _sel: usize, size: *mut u64, confirm: *mut bool) -> HRESULT {
    if !size.is_null() {
        *size = 0;
    }
    if !confirm.is_null() {
        *confirm = false;
    }
    S_OK
}

/// XPackageWriteStats { interval, budget, elapsed, bytesWritten }: nothing measured
unsafe extern "win64" fn XPackageGetWriteStats(out: *mut [u64; 4]) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = [0; 4];
    S_OK
}

/// XPackageKind::Game
unsafe extern "win64" fn XPackageGetPackageKind(_id: usize, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0;
    S_OK
}

pub const XSYSTEM_APP_SPECIFIC_DEVICE_ID_BYTES: usize = 45;

/// An app-specific device ID derived deterministically from the console ID and title ID.
/// GDK requirement: buffer size must be at least 45 bytes (44 hex chars + NUL).
unsafe extern "win64" fn XSystemGetAppSpecificDeviceId(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    use std::hash::{Hash, Hasher};
    if !used.is_null() {
        *used = XSYSTEM_APP_SPECIFIC_DEVICE_ID_BYTES;
    }
    if buf.is_null() {
        return E_INVALIDARG;
    }
    if size < XSYSTEM_APP_SPECIFIC_DEVICE_ID_BYTES {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    let mut h3 = std::collections::hash_map::DefaultHasher::new();
    (crate::state::config().console_id.as_str(), crate::state::config().title_id.as_str(), 1u32).hash(&mut h1);
    (crate::state::config().console_id.as_str(), crate::state::config().title_id.as_str(), 2u32).hash(&mut h2);
    (crate::state::config().console_id.as_str(), crate::state::config().title_id.as_str(), 3u32).hash(&mut h3);
    let s = format!("{:016X}{:016X}{:012X}", h1.finish(), h2.finish(), h3.finish() & 0x0000_FFFF_FFFF_FFFF);
    debug_assert_eq!(s.len(), 44);
    std::ptr::copy_nonoverlapping(s.as_ptr(), buf, 44);
    *buf.add(44) = 0;
    S_OK
}

/// The local user has every privilege (offline: no Xbox Live to deny one).
unsafe extern "win64" fn XUserCheckPrivilege(user: *mut c_void, _options: u32, _privilege: u32, has: *mut bool, reasons: *mut u32) -> HRESULT {
    if !xu::valid_user(user) || has.is_null() {
        return E_INVALIDARG;
    }
    *has = true;
    if !reasons.is_null() {
        *reasons = 0;
    }
    S_OK
}

unsafe extern "win64" fn files_remaining_quota(_user: *mut c_void, _config: *const u8, quota: *mut i64) -> HRESULT {
    if quota.is_null() {
        return E_INVALIDARG;
    }
    *quota = 256 << 20;
    S_OK
}

/// The installation monitor of a fully installed package (XCL runs from an unpacked folder).
static MONITOR: u64 = 0x4D4F_4E49;

unsafe extern "win64" fn XPackageCreateInstallationMonitor(_id: *const u8, _count: u32, _sel: *const c_void, _interval: u32, _queue: *mut c_void, out: *mut *const u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = &MONITOR;
    S_OK
}

/// XPackageInstallationProgress { totalBytes, installedBytes, launchBytes, launchable,
/// completed }: everything installed.
unsafe extern "win64" fn XPackageGetInstallationProgress(_monitor: *const u64, p: *mut u8) {
    if p.is_null() {
        return;
    }
    for k in 0..3 {
        std::ptr::write_unaligned(p.add(8 * k) as *mut u64, 1);
    }
    *p.add(24) = 1;
    *p.add(25) = 1;
}

unsafe extern "win64" fn XPackageRegisterInstallationProgressChanged(_monitor: *const u64, _ctx: *mut c_void, _cb: usize, token: *mut u64) -> HRESULT {
    if !token.is_null() {
        *token = 1;
    }
    S_OK
}

unsafe extern "win64" fn XPackageUnregisterInstallationProgressChanged(_monitor: *const u64, _token: u64, _wait: bool) -> bool {
    true
}

/// `<TitleId>` of the game's MicrosoftGame.Config (xcl.toml `title_id` overrides).
pub unsafe extern "win64" fn XGameGetXboxTitleId(out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let config = crate::state::config();
    *out = config.title_id_value().unwrap_or(0);
    if *out == 0 {
        if config.title_id.trim().is_empty() {
            crate::xlog_warn!("XGameGetXboxTitleId: no TitleId in MicrosoftGame.Config / xcl.toml → 0");
        } else {
            crate::xlog_warn!("XGameGetXboxTitleId: TitleId {:?} (MicrosoftGame.Config / xcl.toml) is not a nonzero 32-bit hex title id → 0", config.title_id);
        }
    }
    S_OK
}

unsafe extern "win64" fn XLaunchNewGame(exe: *const u8, args: *const u8, _user: *mut c_void) {
    crate::xlog_warn!(
        "XLaunchNewGame({}, {}) – ignored (XCL runs a single title)",
        crate::util::from_cstr_ptr(exe).unwrap_or_default(),
        crate::util::from_cstr_ptr(args).unwrap_or_default()
    );
}

unsafe extern "win64" fn XLaunchRestartOnCrash(_args: *const u8, _reserved: u32) -> HRESULT {
    S_OK
}

/// XPackageInstallChunks(id, count, selectors, interval, suppress, queue, monitor): installed
unsafe extern "win64" fn install_chunks(id: *const u8, count: u32, sel: *const c_void, interval: u32, _suppress: bool, queue: *mut c_void, out: *mut *const u64) -> HRESULT {
    XPackageCreateInstallationMonitor(id, count, sel, interval, queue, out)
}

static INSTALL_ID: u8 = 0;
static MOUNT_ID: u8 = 0;

unsafe extern "win64" fn install_chunks_async(_id: usize, _count: u32, _sel: usize, _interval: u32, _suppress: bool, b: *mut XAsyncBlock) -> HRESULT {
    xu::begin_with(b, &INSTALL_ID as *const u8 as usize, || Ok((&MONITOR as *const u64 as usize).to_le_bytes().to_vec()))
}

unsafe extern "win64" fn install_chunks_result(b: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0;
    xu::XAsyncGetResult(b, &INSTALL_ID as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut())
}

unsafe extern "win64" fn mount_with_ui_async(id: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    let owned_id = crate::util::from_cstr_ptr(id).map(|s| {
        let mut bytes = s.into_bytes();
        bytes.push(0);
        bytes
    });
    xu::begin_with(b, &MOUNT_ID as *const u8 as usize, move || {
        let mut m: *mut c_void = std::ptr::null_mut();
        let ptr = owned_id.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
        let hr = unsafe { XPackageMount(ptr, &mut m) };
        if hr < 0 {
            return Err(hr);
        }
        Ok((m as usize).to_le_bytes().to_vec())
    })
}

unsafe extern "win64" fn mount_with_ui_result(b: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0;
    xu::XAsyncGetResult(b, &MOUNT_ID as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut())
}

/// XPackageDetails (XPackage.h)
#[repr(C)]
struct PackageDetails {
    package_identifier: *const u8,
    version: XVersion,
    kind: u32,
    display_name: *const u8,
    description: *const u8,
    publisher: *const u8,
    store_id: *const u8,
    installing: bool,
    index: u32,
    count: u32,
    age_restricted: bool,
    title_id: *const u8,
}

/// What XPackageDetails says about the running game, from its MicrosoftGame.Config:
/// `<Identity Version>` (1.0.0.0 when not given, as the schema defaults it), `<StoreId>` and
/// `<ShellVisuals>`'s DefaultDisplayName / PublisherDisplayName / Description. A value that
/// names a localized string (`ms-resource:`) is resolved from the package's resources.pri on
/// a console; XCL does not read that file, so such a display name falls back to the
/// identity name (`title`) and the other strings to empty.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GameDetails {
    version: XVersion,
    display_name: String,
    publisher: String,
    description: String,
    store_id: String,
}

/// "a.b.c.d" (missing parts 0) → XVersion; None for anything else.
fn parse_version(v: &str) -> Option<XVersion> {
    let mut p = [0u16; 4];
    let parts: Vec<&str> = v.trim().split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    for (k, s) in parts.iter().enumerate() {
        p[k] = s.trim().parse().ok()?;
    }
    Some(XVersion { major: p[0], minor: p[1], build: p[2], revision: p[3] })
}

fn game_details(m: Option<&crate::manifest::Manifest>, title: &str) -> GameDetails {
    let text = |v: Option<&String>| v.filter(|s| !s.starts_with("ms-resource:")).cloned();
    GameDetails {
        version: m.and_then(|m| m.version.as_deref()).and_then(parse_version).unwrap_or(XVersion { major: 1, minor: 0, build: 0, revision: 0 }),
        display_name: text(m.and_then(|m| m.display_name.as_ref())).unwrap_or_else(|| title.to_string()),
        publisher: text(m.and_then(|m| m.publisher_display_name.as_ref())).unwrap_or_default(),
        description: text(m.and_then(|m| m.description.as_ref())).unwrap_or_default(),
        store_id: m.and_then(|m| m.store_id.clone()).unwrap_or_default(),
    }
}

fn current_game_details() -> &'static GameDetails {
    static D: OnceLock<GameDetails> = OnceLock::new();
    D.get_or_init(|| {
        let boot = crate::state::boot();
        let m = boot.and_then(|b| crate::manifest::load(&b.game_root));
        game_details(m.as_ref(), &boot.map(|b| b.title.clone()).unwrap_or_default())
    })
}

/// XPackageEnumeratePackages(kind, scope, context, callback): the game itself (no DLC), in
/// every scope – XCL runs one unpacked package, with no hub or related products installed.
unsafe extern "win64" fn enumerate_packages(kind: u32, _scope: u32, ctx: *mut c_void, cb: Option<unsafe extern "win64" fn(*mut c_void, *const PackageDetails) -> bool>) -> HRESULT {
    let Some(cb) = cb else { return E_INVALIDARG };
    if kind != 0 {
        return S_OK;
    }
    let c = |s: &str| std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
    let g = current_game_details();
    let id = c(&package_identifier());
    let title = c(&g.display_name);
    let publisher = c(&g.publisher);
    let description = c(&g.description);
    let store_id = c(&g.store_id);
    // the number XGameGetXboxTitleId gives, as 8 hex digits; none where it gives 0
    let title_id = c(&crate::state::config().title_id_value().map(|v| format!("{v:08X}")).unwrap_or_default());
    let d = PackageDetails {
        package_identifier: id.as_ptr() as *const u8,
        version: g.version,
        kind: 0,
        display_name: title.as_ptr() as *const u8,
        description: description.as_ptr() as *const u8,
        publisher: publisher.as_ptr() as *const u8,
        store_id: store_id.as_ptr() as *const u8,
        installing: false,
        index: 0,
        count: 1,
        age_restricted: false,
        title_id: title_id.as_ptr() as *const u8,
    };
    cb(ctx, &d);
    S_OK
}

static ERROR_CB: Mutex<Option<(usize, usize)>> = Mutex::new(None);
static ERROR_OPTIONS: AtomicUsize = AtomicUsize::new(0);

pub unsafe extern "win64" fn XErrorReport(hr: HRESULT, message: *const u8) {
    let text = crate::util::from_cstr_ptr(message).unwrap_or_default();
    crate::xlog_warn!("XErrorReport {:#x}: {text}", hr as u32);
    // Copy and release `ERROR_CB` before invoking the guest callback so re-entrant calls to
    // `XErrorSetCallback` or `XErrorReport` do not deadlock.
    let entry = *ERROR_CB.lock().unwrap();
    if let Some((cb, ctx)) = entry {
        let f: unsafe extern "win64" fn(HRESULT, *const u8, *mut c_void) -> bool = std::mem::transmute(cb);
        let _ = f(hr, message, ctx as *mut c_void);
    }
}

unsafe extern "win64" fn error_set_callback(cb: usize, ctx: *mut c_void) {
    *ERROR_CB.lock().unwrap() = (cb != 0).then_some((cb, ctx as usize));
}

unsafe extern "win64" fn error_set_options(options: u32, _reserved: usize) {
    // Store requested options without triggering DebugBreak in-process.
    ERROR_OPTIONS.store(options as usize, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------------------
// XThread (the async object's thread quarter)
// ---------------------------------------------------------------------------------------

thread_local! {
    static TIME_SENSITIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub unsafe extern "win64" fn XThreadSetTimeSensitive(v: bool) -> HRESULT {
    TIME_SENSITIVE.with(|c| c.set(v));
    S_OK
}

pub unsafe extern "win64" fn XThreadIsTimeSensitive() -> bool {
    TIME_SENSITIVE.with(|c| c.get())
}

pub unsafe extern "win64" fn XThreadVerifyNotTimeSensitive() -> HRESULT {
    if TIME_SENSITIVE.with(|c| c.get()) {
        crate::xlog_warn!("XThreadVerifyNotTimeSensitive called on a time-sensitive thread");
    }
    S_OK
}

pub unsafe extern "win64" fn XThreadAssertNotTimeSensitive() {
    if TIME_SENSITIVE.with(|c| c.get()) {
        crate::xlog_warn!("XThreadAssertNotTimeSensitive called on a time-sensitive thread");
    }
}

// ---------------------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------------------

static SEEN: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn note(c: Guid, i: Guid, what: String) {
    let key = format!("{c}/{i}");
    let mut g = SEEN.lock().unwrap();
    if !g.contains_key(&key) {
        crate::xlog_info!("XGameRuntime: QueryApiImpl {key} → {what}");
        g.insert(key, what);
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiStats {
    /// "clsid/iid" → who answered (XCL, unavailable, missing)
    pub apis: BTreeMap<String, String>,
    /// XCL objects' slots the title called that XCL does not implement: "Iface#slot" → calls
    pub unimplemented: BTreeMap<String, u64>,
}

pub fn stats() -> ApiStats {
    ApiStats { apis: SEEN.lock().unwrap().clone(), unimplemented: UNIMPLEMENTED.lock().unwrap().clone() }
}

/// Implemented exports with real code addresses. Every other name in `XGAMERUNTIME`
/// stays `Stub` (metadata `0`) until its slice lands.
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("InitializeApiImpl", InitializeApiImpl as usize),
        ("InitializeApiImplEx", InitializeApiImplEx as usize),
        ("InitializeApiImplEx2", InitializeApiImplEx2 as usize),
        ("UninitializeApiImpl", UninitializeApiImpl as usize),
        ("QueryApiImpl", QueryApiImpl as usize),
        ("XErrorReport", XErrorReport as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn query(c: Guid, i: Guid) -> (HRESULT, *mut c_void) {
        let mut o = std::ptr::null_mut();
        (QueryApiImpl(&c, &i, &mut o), o)
    }

    unsafe fn slot<T>(o: *mut c_void, k: usize) -> T {
        std::mem::transmute_copy(&*(*(o as *const *const usize)).add(k))
    }

    fn slot_in_table(name: &str) -> (Guid, Guid, usize) {
        table().find(|r| r.0 == name).map(|r| (r.1, r.2, r.3)).unwrap_or_else(|| panic!("{name} not in the GDK table"))
    }

    #[test]
    fn api_init_is_idempotent_s_ok() {
        unsafe {
            assert_eq!(InitializeApiImpl(0x1, 0x2), S_OK);
            assert_eq!(InitializeApiImplEx(0x1, 0x2, 0), S_OK);
            assert_eq!(InitializeApiImplEx2(0x6336_0000_2712, 0x65f4_0000_1edd, 2, std::ptr::null_mut()), S_OK);
            assert_eq!(InitializeApiImplEx2(0x6336_0000_2712, 0x65f4_0000_1edd, 2, std::ptr::null_mut()), S_OK);
            UninitializeApiImpl();
            UninitializeApiImpl();
            // saturating: uninitializing without initializing must not underflow
            UninitializeApiImpl();
            assert_eq!(InitializeApiImplEx2(0, 0, 0, std::ptr::null_mut()), S_OK);
            UninitializeApiImpl();
        }
    }

    #[test]
    fn query_rejects_nulls() {
        unsafe {
            let mut o = std::ptr::null_mut();
            assert_eq!(QueryApiImpl(std::ptr::null(), &XUSER, &mut o), E_INVALIDARG);
            assert_eq!(QueryApiImpl(&XUSER, std::ptr::null(), &mut o), E_INVALIDARG);
            assert_eq!(QueryApiImpl(&XUSER, &XUSER, std::ptr::null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn refused_and_unknown_classes_are_not_served() {
        unsafe {
            // the Store reaches Xbox Live: unavailable offline, never a fake object
            assert_eq!(query(XSTORE, XSTORE).0, REGDB_E_CLASSNOTREG);
            // an unknown class: missing, never a fake object
            let missing = g(0xdeadbeef_0000_4000_8000_0123456789ab);
            let (hr, o) = query(missing, missing);
            assert_eq!(hr, REGDB_E_CLASSNOTREG);
            assert!(o.is_null());
            assert!(stats().apis.values().any(|v| v.contains("missing")), "{:?}", stats().apis);
        }
    }

    /// Every wired function is in its class in the GDK table, and the slots the thunks
    /// call match the table.
    #[test]
    fn every_impl_is_in_its_gdk_class() {
        for c in classes() {
            for (f, _) in &c.impls {
                assert_eq!(slot_in_table(f).0, c.clsid, "{f} is not a {} function", c.name);
            }
        }
        for (f, slot) in [
            ("XUserAddAsync", 7),
            ("XUserGetState", 14),
            ("XUserCheckPrivilege", 20),
            ("XUserUnregisterForChangeEvent", 34),
            ("XUserIsStoreUser", 42),
            ("XPackageEnumeratePackages", 39),
            ("XPersistentLocalStorageGetPath", 4),
            ("XSystemGetAppSpecificDeviceId", 5),
            ("XGameGetXboxTitleId", 3),
            ("XAsyncBegin", 7),
            ("XTaskQueueCreate", 12),
        ] {
            assert_eq!(slot_in_table(f).2, slot, "{f}");
        }
        // slots XCL does not wire yet still belong to a known class (they become
        // logged E_NOTIMPL stubs, not silent gaps)
        assert_eq!(slot_in_table("XPackageGetUserLocale").0, XPACKAGE);
        assert_eq!(slot_in_table("XGameSaveCreateContainer").0, XGAMESAVE);
        let main = own(&XUSER, &XUSER).unwrap();
        assert!(!main.iface.iids.is_empty() && main.iface.main);
        assert!(own(&XSTORE, &XSTORE).is_none());
        assert!(returns_bool("XUserIsStoreUser") && returns_bool("XPackageUnregisterPackageInstalled") && !returns_bool("XUserGetState"));
    }

    #[test]
    fn xuser_main_vtable_serves_the_offline_user() {
        unsafe {
            let (hr, u) = query(XUSER, XUSER);
            assert_eq!(hr, S_OK);
            assert!(!u.is_null());
            // XUserGetMaxUsers through the vtable: 1 offline user
            let get_max: unsafe extern "win64" fn(*mut c_void, *mut u32) -> HRESULT = slot(u, slot_in_table("XUserGetMaxUsers").2);
            let mut max = 0u32;
            assert_eq!(get_max(u, &mut max), S_OK);
            assert_eq!(max, 1);
            // XUserCheckPrivilege through the vtable: the local user has every privilege
            let check: unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32, *mut bool, *mut u32) -> HRESULT =
                slot(u, slot_in_table("XUserCheckPrivilege").2);
            let (mut has, mut reasons) = (false, 0u32);
            assert_eq!(check(u, xu::user_ptr(), 0, 220, &mut has, &mut reasons), S_OK);
            assert!(has);
            // the gamertag interface has its own vtable; an unknown later XUser
            // interface falls back to the main one
            let (_, gt) = query(XUSER, XUSER_GAMERTAG_IID);
            assert!(!std::ptr::eq(gt as *const Obj, u as *const Obj));
            let (_, later) = query(XUSER, g(0x26f3c674_a2fe_44fa_b6c4_a323bc94ff53));
            assert!(std::ptr::eq(later as *const Obj, u as *const Obj));
        }
    }

    #[test]
    fn query_interface_accepts_iunknown_and_sibling_interfaces() {
        unsafe {
            type Qi = unsafe extern "win64" fn(*const Obj, *const Guid, *mut *mut c_void) -> HRESULT;
            let qi = |o: *mut c_void, iid: &Guid| {
                let f: Qi = slot(o, 0);
                let mut p = std::ptr::null_mut();
                (f(o as *const Obj, iid, &mut p), p)
            };
            // QI(IUnknown) from two different interfaces: one canonical pointer (the main object).
            let (_, u) = query(XUSER, XUSER);
            let (_, gt) = query(XUSER, XUSER_GAMERTAG_IID);
            assert!(!std::ptr::eq(gt as *const Obj, u as *const Obj));
            let (hru, iu) = qi(u, &IID_IUNKNOWN);
            let (hrg, ig) = qi(gt, &IID_IUNKNOWN);
            assert_eq!((hru, hrg), (S_OK, S_OK));
            assert!(!iu.is_null());
            assert!(std::ptr::eq(iu as *const Obj, ig as *const Obj));
            assert!(std::ptr::eq(iu as *const Obj, u as *const Obj));
            // sibling QI succeeds: main reaches the gamertag vtable, the alias reaches main's V2.
            let (hrs, s) = qi(u, &XUSER_GAMERTAG_IID);
            assert_eq!(hrs, S_OK);
            assert!(std::ptr::eq(s as *const Obj, gt as *const Obj));
            let (hrv, v) = qi(gt, &XUSER_V2_IID);
            assert_eq!(hrv, S_OK);
            assert!(std::ptr::eq(v as *const Obj, u as *const Obj));
            // legacy console vtables share the class identity too.
            let p1 = own(&XPACKAGE, &XPACKAGE_V1_IID).expect("XPackage v1");
            let (_, pm) = query(XPACKAGE, XPACKAGE);
            let (hrp, ip) = qi(p1 as *const Obj as *mut c_void, &IID_IUNKNOWN);
            assert_eq!(hrp, S_OK);
            assert!(std::ptr::eq(ip as *const Obj, pm as *const Obj));
            // failures null the out-pointer; nulls are rejected.
            // (a non-main alias stays isolated: an iid it does not hold is refused, not
            // routed to the main object.)
            let mut bad = 0x1 as *mut c_void;
            let f: Qi = slot(gt, 0);
            assert_eq!(f(gt as *const Obj, &g(0xdeadbeef_0000_4000_8000_0123456789ab), &mut bad), E_NOINTERFACE);
            assert!(bad.is_null());
            assert_eq!(f(gt as *const Obj, std::ptr::null(), &mut bad), E_INVALIDARG);
            assert_eq!(f(gt as *const Obj, &XUSER, std::ptr::null_mut()), E_INVALIDARG);
        }
    }

    #[test]
    fn query_api_impl_isolates_forwarding_alias() {
        // No PC runtime on Linux: unknown interface versions share the immutable main
        // vtable, so there is no per-alias forwarding state to poison — every alias keeps
        // the one class identity through QI(IUnknown), and an alias answers only what it holds.
        unsafe {
            type Qi = unsafe extern "win64" fn(*const Obj, *const Guid, *mut *mut c_void) -> HRESULT;
            let qi = |o: *mut c_void, iid: &Guid| {
                let f: Qi = slot(o, 0);
                let mut p = std::ptr::null_mut();
                (f(o as *const Obj, iid, &mut p), p)
            };
            let unknown_a = g(0x11111111_2222_4333_8444_555555555555);
            let unknown_b = g(0x66666666_7777_4888_8999_aaaaaaaaaaaa);
            let (_, o1) = query(XGAMEUI, unknown_a);
            let (_, o2) = query(XGAMEUI, unknown_b);
            let (_, main) = query(XGAMEUI, XGAMEUI);
            assert!(!o1.is_null());
            assert!(std::ptr::eq(o1 as *const Obj, main as *const Obj));
            assert!(std::ptr::eq(o2 as *const Obj, main as *const Obj));
            let (_, iu1) = qi(o1, &IID_IUNKNOWN);
            let (_, iu2) = qi(o2, &IID_IUNKNOWN);
            assert!(std::ptr::eq(iu1 as *const Obj, iu2 as *const Obj));
            assert!(std::ptr::eq(iu1 as *const Obj, main as *const Obj));
            // the alias answers its own class version, but a non-main alias stays isolated
            // from iids it does not hold (refused + nulled, not routed to main).
            let (hrs, s) = qi(o1, &XGAMEUI);
            assert_eq!(hrs, S_OK);
            assert!(std::ptr::eq(s as *const Obj, o1 as *const Obj));
            let (_, gt) = query(XUSER, XUSER_GAMERTAG_IID);
            let mut p = 0x1 as *mut c_void;
            let gtf: Qi = slot(gt, 0);
            assert_eq!(gtf(gt as *const Obj, &XGAMEUI, &mut p), E_NOINTERFACE);
            assert!(p.is_null());
        }
    }

    #[test]
    fn xgame_title_id_and_xerror_through_vtable() {
        unsafe {
            let (hr, gm) = query(XGAME, XGAME);
            assert_eq!(hr, S_OK);
            let get_id: unsafe extern "win64" fn(*mut c_void, *mut u32) -> HRESULT = slot(gm, slot_in_table("XGameGetXboxTitleId").2);
            let mut id = 0u32;
            assert_eq!(get_id(gm, &mut id), S_OK);
            let want = u32::from_str_radix(crate::state::config().title_id.trim_start_matches("0x"), 16).unwrap_or(0);
            assert_eq!(id, want);

            // XError vtable: the callback runs, options stick, no DebugBreak
            static ERR_SEEN: AtomicU32 = AtomicU32::new(0);
            unsafe extern "win64" fn on_error(hr: HRESULT, _msg: *const u8, _ctx: *mut c_void) -> bool {
                ERR_SEEN.store(hr as u32, Ordering::SeqCst);
                true
            }
            let (hr, err) = query(XERROR, XERROR);
            assert_eq!(hr, S_OK);
            let report: unsafe extern "win64" fn(*mut c_void, HRESULT, *const u8) = slot(err, slot_in_table("XErrorReport").2);
            let set_cb: unsafe extern "win64" fn(*mut c_void, usize, *mut c_void) = slot(err, slot_in_table("XErrorSetCallback").2);
            let set_opt: unsafe extern "win64" fn(*mut c_void, u32, usize) = slot(err, slot_in_table("XErrorSetOptions").2);
            set_opt(err, 3, 0);
            set_cb(err, on_error as usize, std::ptr::null_mut());
            report(err, crate::util::E_FAIL, c"unit error".as_ptr() as *const u8);
            assert_eq!(ERR_SEEN.load(Ordering::SeqCst), crate::util::E_FAIL as u32);
            set_cb(err, 0, std::ptr::null_mut());
        }
    }

    /// XPackageDetails of the running game come from its MicrosoftGame.Config; localized
    /// (`ms-resource:`) strings XCL cannot resolve fall back instead of reaching the title.
    #[test]
    fn package_details_come_from_the_game_config() {
        let m = crate::manifest::parse(
            r#"<Game><Identity Name="41336Pub.Game" Publisher="CN=X" Version="1.6.2.21"/><StoreId>9NZTKMP36L8J</StoreId>
<ShellVisuals DefaultDisplayName="Example Game" PublisherDisplayName="Example Publisher" Description="ms-resource:ApplicationDescription"/></Game>"#,
        );
        let d = game_details(Some(&m), "41336Pub.Game");
        assert_eq!(d.version, XVersion { major: 1, minor: 6, build: 2, revision: 21 });
        assert_eq!((d.display_name.as_str(), d.publisher.as_str(), d.description.as_str(), d.store_id.as_str()), ("Example Game", "Example Publisher", "", "9NZTKMP36L8J"));
        let loc = crate::manifest::parse(r#"<Game><ShellVisuals DefaultDisplayName="ms-resource:ApplicationDisplayName"/></Game>"#);
        assert_eq!(game_details(Some(&loc), "41336Pub.Game").display_name, "41336Pub.Game");
        let none = game_details(None, "T");
        assert_eq!((none.version, none.display_name.as_str(), none.store_id.as_str()), (XVersion { major: 1, minor: 0, build: 0, revision: 0 }, "T", ""));
        assert_eq!(parse_version("2.0"), Some(XVersion { major: 2, minor: 0, build: 0, revision: 0 }));
        assert_eq!(parse_version("1.2.3.70000"), None);
        assert_eq!(parse_version("1.2.3.4.5"), None);
        // the single title-id reading: the number XGameGetXboxTitleId gives, as 8 hex digits
        assert_eq!(crate::config::parse_title_id("0x1a2b3c").map(|v| format!("{v:08X}")).as_deref(), Some("001A2B3C"));
        assert_eq!(crate::config::parse_title_id("1234ABCD").map(|v| format!("{v:08X}")).as_deref(), Some("1234ABCD"));
        assert_eq!(crate::config::parse_title_id("0Xffffffff").map(|v| format!("{v:08X}")).as_deref(), Some("FFFFFFFF"));
        assert_eq!(std::mem::size_of::<PackageDetails>(), 80);
    }

    static REFUSED_CB: AtomicU32 = AtomicU32::new(0);
    unsafe extern "win64" fn refused_cb(_b: *mut XAsyncBlock) {
        REFUSED_CB.fetch_add(1, Ordering::SeqCst);
    }

    /// The refused XGameUi dialogs, called through the vtable slots a title's thunks use, on
    /// XCL's own queues (no PC runtime): every Async entry point returns S_OK, completes with
    /// E_NOTIMPL and delivers its completion callback exactly once; its Result entry points
    /// (called with the call's own block) return that status and leave the caller's output
    /// buffers as they were; a Result on a block that was never started is E_INVALIDARG.
    #[test]
    fn refused_xgameui_dialogs_complete_with_e_notimpl_and_leave_outputs_alone() {
        // No ASYNC_TEST_LOCK on the Linux twin (cf. step-48): each async test drives
        // its own XAsyncBlock, so no cross-test serialisation is needed (no use_pc:
        // there is no PC runtime on Linux).
        unsafe {
            let ui = own(&XGAMEUI, &g(0xeaf669df_5542_4590_99a3_8dc061f837cc)).expect("XGameUi object") as *const Obj as *mut c_void;
            let at = |name: &str| slot_in_table(name).2;
            // (async, then the call's other arguments as zeroes; each handler reads only the block)
            type Begin = unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, usize, usize, usize, usize, usize, usize) -> HRESULT;
            let run = |name: &str, args: usize| -> Box<XAsyncBlock> {
                let begin: Begin = slot(ui, at(name));
                let mut b: Box<XAsyncBlock> = Box::new(std::mem::zeroed());
                b.callback = refused_cb as usize;
                let before = REFUSED_CB.load(Ordering::SeqCst);
                assert_eq!(begin(ui, &mut *b, args, args, args, args, args, args), S_OK, "{name}");
                for _ in 0..400 {
                    if REFUSED_CB.load(Ordering::SeqCst) > before {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                assert_eq!(REFUSED_CB.load(Ordering::SeqCst), before + 1, "{name}: one completion callback");
                assert_eq!(xu::XAsyncGetStatus(&mut *b, false), E_NOTIMPL, "{name}: status");
                b
            };

            // Async entry points, called with zero for every argument after the block: the
            // handlers read only the block, so no pointer argument is ever dereferenced
            let mut blocks = Vec::new();
            for name in [
                "XGameUiShowAchievementsAsync",
                "XGameUiShowPlayerPickerAsync",
                "XGameUiShowPlayerProfileCardAsync",
                "XGameUiShowSendGameInviteAsync",
                "XGameUiShowStateShareAsync",
                "XGameUiShowMultiplayerActivityGameInviteAsync",
                "XGameUiShowWebAuthenticationAsync",
                "XGameUiShowWebAuthenticationWithOptionsAsync",
            ] {
                blocks.push((name, run(name, 0)));
            }
            let ptrs: Vec<(&str, *mut XAsyncBlock)> = blocks.iter_mut().map(|(n, b)| (*n, &mut **b as *mut XAsyncBlock)).collect();
            let block = |n: &str| ptrs.iter().find(|p| p.0 == n).unwrap().1;

            // Result entry points that take only the block
            for (result, of) in [
                ("XGameUiShowAchievementsResult", "XGameUiShowAchievementsAsync"),
                ("XGameUiShowPlayerProfileCardResult", "XGameUiShowPlayerProfileCardAsync"),
                ("XGameUiShowSendGameInviteResult", "XGameUiShowSendGameInviteAsync"),
                ("XGameUiShowStateShareResult", "XGameUiShowStateShareAsync"),
                ("XGameUiShowMultiplayerActivityGameInviteResult", "XGameUiShowMultiplayerActivityGameInviteAsync"),
            ] {
                let f: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HRESULT = slot(ui, at(result));
                let k = block(of);
                assert_eq!(f(ui, k), E_NOTIMPL, "{result}");
                assert_eq!(f(ui, k), E_NOTIMPL, "{result}: a second call answers the same");
            }

            // Result entry points with output buffers: sentinels stay as they were
            const S32: u32 = 0xDEAD_BEEF;
            const S64: u64 = 0xDEAD_BEEF_CAFE_F00D;
            let mut count = S32;
            let mut players = [S64; 3];
            let mut used32 = S32;
            let mut size = S64 as usize;
            let mut buf = [0xA5u8; 16];
            let mut ptr = 0x1234usize as *mut c_void;
            let mut used = S64 as usize;

            let pick = block("XGameUiShowPlayerPickerAsync");
            let f: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut u32) -> HRESULT = slot(ui, at("XGameUiShowPlayerPickerResultCount"));
            assert_eq!(f(ui, pick, &mut count), E_NOTIMPL);
            let f: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, u32, *mut u64, *mut u32) -> HRESULT = slot(ui, at("XGameUiShowPlayerPickerResult"));
            assert_eq!(f(ui, pick, 3, players.as_mut_ptr(), &mut used32), E_NOTIMPL);

            let web = block("XGameUiShowWebAuthenticationAsync");
            let f: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut usize) -> HRESULT = slot(ui, at("XGameUiShowWebAuthenticationResultSize"));
            assert_eq!(f(ui, web, &mut size), E_NOTIMPL);
            let f: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, usize, *mut u8, *mut *mut c_void, *mut usize) -> HRESULT =
                slot(ui, at("XGameUiShowWebAuthenticationResult"));
            assert_eq!(f(ui, web, buf.len(), buf.as_mut_ptr(), &mut ptr, &mut used), E_NOTIMPL);
            // the WithOptions call is read by the same Result functions
            let web2 = block("XGameUiShowWebAuthenticationWithOptionsAsync");
            assert_eq!(f(ui, web2, buf.len(), buf.as_mut_ptr(), &mut ptr, &mut used), E_NOTIMPL);

            assert_eq!((count, used32, size as u64, used as u64, ptr as usize), (S32, S32, S64, S64, 0x1234));
            assert_eq!(players, [S64; 3]);
            assert_eq!(buf, [0xA5; 16]);

            // a block that was never started is the caller's mistake, not a refused dialog
            let mut fresh: XAsyncBlock = std::mem::zeroed();
            let f: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut u32) -> HRESULT = slot(ui, at("XGameUiShowPlayerPickerResultCount"));
            assert_eq!(f(ui, &mut fresh, &mut count), E_INVALIDARG);
            assert_eq!(count, S32);
            // and a null block fails the call itself
            let begin: Begin = slot(ui, at("XGameUiShowStateShareAsync"));
            assert_eq!(begin(ui, std::ptr::null_mut(), 0, 0, 0, 0, 0, 0), E_INVALIDARG);
        }
    }

    #[test]
    fn legacy_console_iids_map_to_their_own_slots() {
        // SLOTS must cover the full GDK table (highest slot is 83).
        let max_slot = table().map(|r| r.3).max().unwrap_or(0);
        assert!(max_slot < SLOTS, "SLOTS={SLOTS} truncates slot {max_slot}");

        let bound = |o: &Obj, slot: usize, name: &str| {
            assert_eq!(o.iface.names[slot], Some(name), "{} slot {slot}", o.iface.name);
            assert_ne!(o.targets[slot], 0, "{} slot {slot} ({name}) has no function", o.iface.name);
            o.targets[slot]
        };
        let pm = own(&XPACKAGE, &XPACKAGE).expect("XPackage main");
        let main_fn = |name: &str| bound(pm, slot_in_table(name).2, name);

        // XPackage v1 (be3f3e9f): XPackageEnumeratePackages is at slot 28.
        let p1 = own(&XPACKAGE, &XPACKAGE_V1_IID).expect("XPackage v1");
        assert!(!p1.iface.main);
        assert_eq!(bound(p1, 28, "XPackageEnumeratePackages"), main_fn("XPackageEnumeratePackages"));

        // XPackage v2 (f92d8712): EnumeratePackages @ 35, RegisterPackageInstalled @ 36,
        // callable through the vtable as a title calls it.
        let p2 = own(&XPACKAGE, &XPACKAGE_V2_IID).expect("XPackage v2");
        assert!(!p2.iface.main);
        assert_eq!(bound(p2, 35, "XPackageEnumeratePackages"), main_fn("XPackageEnumeratePackages"));
        assert_eq!(bound(p2, 36, "XPackageRegisterPackageInstalled"), main_fn("XPackageRegisterPackageInstalled"));
        let register = |o: &Obj, token: *mut u64| unsafe {
            slot::<Fwd>(o as *const Obj as *mut c_void, 36)(o, 0, 0, 0, token as usize, 0, 0, 0, 0, 0) as u32 as HRESULT
        };
        let (mut t1, mut t2) = (0u64, 0u64);
        assert_eq!(register(p2, &mut t1), S_OK);
        assert_eq!(register(p2, &mut t2), S_OK);
        assert!(t1 != 0 && t2 > t1, "tokens {t1}, {t2}");
        assert_eq!(register(p2, std::ptr::null_mut()), E_INVALIDARG);

        // XPackage main: XPackageGetIdentifier at slot 27.
        bound(pm, 27, "XPackageGetIdentifier");
        // an XPackage interface id the table does not know gets the main vtable
        let old = own(&XPACKAGE, &XPACKAGE_OLD_IID).expect("XPackage old iid");
        assert!(old.iface.main);

        // XSystem v3 (6fd71f09): XSystemGetAppSpecificDeviceId is at slot 4 (not SandboxId).
        let s3 = own(&XSYSTEM, &XSYSTEM_V3_IID).expect("XSystem v3");
        assert!(!s3.iface.main);
        let sm = own(&XSYSTEM, &XSYSTEM).expect("XSystem main");
        bound(sm, 4, "XSystemGetXboxLiveSandboxId");
        assert_eq!(bound(s3, 4, "XSystemGetAppSpecificDeviceId"), bound(sm, 5, "XSystemGetAppSpecificDeviceId"));
    }

    #[test]
    fn unimplemented_slots_report_e_notimpl() {
        // No ASYNC_TEST_LOCK on the Linux twin (cf. step-48): direct win64 calls only.
        unsafe {
            let (hr, ui) = query(XGAMEUI, XGAMEUI);
            assert_eq!(hr, S_OK);
            // a refused XGameUi dialog completes at once with E_NOTIMPL; its Result returns that status
            let ach: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, usize, u32) -> HRESULT = slot(ui, slot_in_table("XGameUiShowAchievementsAsync").2);
            let ach_r: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock) -> HRESULT = slot(ui, slot_in_table("XGameUiShowAchievementsResult").2);
            let mut ab: XAsyncBlock = std::mem::zeroed();
            assert_eq!(ach(ui, &mut ab, 0, 0), S_OK);
            assert_eq!(xu::XAsyncGetStatus(&mut ab, true), E_NOTIMPL);
            assert_eq!(ach_r(ui, &mut ab), E_NOTIMPL);
            let web: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, usize, *const u8, *const u8, u32) -> HRESULT =
                slot(ui, slot_in_table("XGameUiShowWebAuthenticationWithOptionsAsync").2);
            let mut wb: XAsyncBlock = std::mem::zeroed();
            assert_eq!(web(ui, &mut wb, 0, c"https://example.invalid/".as_ptr() as *const u8, c"https://example.invalid/done".as_ptr() as *const u8, 0), S_OK);
            assert_eq!(xu::XAsyncGetStatus(&mut wb, true), E_NOTIMPL);
            // a slot XCL does not implement at all still answers E_NOTIMPL and is reported
            let slot_k = slot_in_table("XGameUiShowMessageDialogAsync").2;
            let show: unsafe extern "win64" fn(*mut c_void) -> HRESULT = slot(ui, slot_k);
            assert_eq!(show(ui), E_NOTIMPL);
            assert!(stats().unimplemented.contains_key("XGameUiShowMessageDialogAsync"), "{:?}", stats().unimplemented);
            // the refused dialogs are implemented, not unknown slots
            assert!(!stats().unimplemented.contains_key("XGameUiShowAchievementsAsync"), "{:?}", stats().unimplemented);
        }
    }

    #[test]
    fn get_proc_address_resolves_the_thunk_protocol() {
        // the game's GetProcAddress probes (and its static imports) must land on real
        // code, never NULL and never an auto-stub thunk
        let h = crate::modules::hle_handle(crate::hle::XGAMERUNTIME);
        for name in ["InitializeApiImpl", "InitializeApiImplEx", "InitializeApiImplEx2", "UninitializeApiImpl", "QueryApiImpl", "XErrorReport"] {
            let what = crate::pe::ImportName::Name { hint: 0, name: name.to_string() };
            let addr = crate::resolver::get_proc_address(h, &what);
            assert!(addr.is_some(), "{name} must resolve");
            let addr = addr.unwrap();
            assert_ne!(addr, 0, "{name} must be real code");
            assert!(crate::stubs::info_of_stub(addr).is_none(), "{name} must not be an auto-stub");
        }
    }

    #[test]
    fn mount_with_ui_async_does_not_leak_mount_on_invalid_or_aborted_call() {
        // No ASYNC_TEST_LOCK on the Linux twin (cf. step-48): each async test drives
        // its own XAsyncBlock / task queue, so no cross-test serialisation is needed.
        unsafe {
            let before = LIVE_MOUNTS.load(Ordering::SeqCst);

            // Null XAsyncBlock -> E_INVALIDARG without leaking a Mount handle.
            assert_eq!(mount_with_ui_async(std::ptr::null(), std::ptr::null_mut()), E_INVALIDARG);
            assert_eq!(LIVE_MOUNTS.load(Ordering::SeqCst), before, "Mount leaked on null XAsyncBlock");

            // Terminated task queue -> E_ABORT without leaking a Mount handle.
            let mut q = std::ptr::null_mut();
            assert_eq!(xq::XTaskQueueCreate(xq::MODE_THREAD_POOL, xq::MODE_THREAD_POOL, &mut q), S_OK);
            assert_eq!(xq::XTaskQueueTerminate(q, true, std::ptr::null_mut(), 0), S_OK);
            let mut b_aborted: XAsyncBlock = std::mem::zeroed();
            b_aborted.queue = q;
            assert_eq!(mount_with_ui_async(std::ptr::null(), &mut b_aborted), S_OK);
            assert_eq!(xu::XAsyncGetStatus(&mut b_aborted, true), crate::util::E_ABORT);
            assert_eq!(LIVE_MOUNTS.load(Ordering::SeqCst), before, "Mount leaked on terminated queue");
            xq::XTaskQueueCloseHandle(q);

            // Control: normal async mount succeeds and closes cleanly.
            let mut b_ok: XAsyncBlock = std::mem::zeroed();
            assert_eq!(mount_with_ui_async(std::ptr::null(), &mut b_ok), S_OK);
            assert_eq!(xu::XAsyncGetStatus(&mut b_ok, true), S_OK);
            let mut handle = 0usize;
            assert_eq!(mount_with_ui_result(&mut b_ok, &mut handle), S_OK);
            assert_ne!(handle, 0);
            XPackageCloseMountHandle(handle as *mut c_void);
            assert_eq!(LIVE_MOUNTS.load(Ordering::SeqCst), before);
        }
    }

    #[test]
    fn package_result_functions_null_outptrs_on_failure() {
        unsafe {
            let mut mount_ptr = 0xDEADBEEF as *mut c_void;
            let bad_id = b"unknown_package_id_12345\0";
            assert_eq!(XPackageMount(bad_id.as_ptr(), &mut mount_ptr), crate::util::E_NOT_FOUND);
            assert_eq!(mount_ptr, std::ptr::null_mut());

            let mut install_out = 0xDEADBEEFusize;
            let mut b_bad: XAsyncBlock = std::mem::zeroed();
            assert_eq!(install_chunks_result(&mut b_bad, &mut install_out), E_INVALIDARG);
            assert_eq!(install_out, 0);

            let mut mount_out = 0xDEADBEEFusize;
            assert_eq!(mount_with_ui_result(&mut b_bad, &mut mount_out), E_INVALIDARG);
            assert_eq!(mount_out, 0);
        }
    }

    #[test]
    fn pls_mounts_for_the_running_package_only() {
        // No ASYNC_TEST_LOCK on the Linux twin (cf. step-48): direct win64 calls only.
        unsafe {
            let before = LIVE_MOUNTS.load(Ordering::SeqCst);
            let mut m: *mut c_void = 1 as *mut c_void;
            assert_eq!(XPersistentLocalStorageMountForPackage(b"x\0".as_ptr(), std::ptr::null_mut()), E_INVALIDARG);
            assert_eq!(XPersistentLocalStorageMountForPackage(std::ptr::null(), &mut m), E_INVALIDARG);
            assert!(m.is_null());
            assert_eq!(XPersistentLocalStorageMountForPackage(b"\0".as_ptr(), &mut m), E_INVALIDARG);
            // another title's package: no PLS here
            let other = b"Some.Other.Title_1.0.0.0_x64__8wekyb3d8bbwe\0";
            assert_eq!(XPersistentLocalStorageMountForPackage(other.as_ptr(), &mut m), crate::util::hresult_from_win32(2));
            assert!(m.is_null());
            // the running package, in any case: the same path XPersistentLocalStorageGetPath gives
            let id = format!("{}\0", package_identifier().to_ascii_lowercase());
            assert_eq!(XPersistentLocalStorageMountForPackage(id.as_ptr(), &mut m), S_OK);
            assert!(!m.is_null());
            assert_eq!(LIVE_MOUNTS.load(Ordering::SeqCst), before + 1);
            let mut size = 0usize;
            assert_eq!(XPackageGetMountPathSize(m, &mut size), S_OK);
            let mut buf = vec![0u8; size];
            assert_eq!(XPackageGetMountPath(m, size, buf.as_mut_ptr()), S_OK);
            let got = std::ffi::CStr::from_bytes_until_nul(&buf).unwrap().to_str().unwrap();
            let mut want_size = 0usize;
            assert_eq!(xu::XPersistentLocalStorageGetPathSize(&mut want_size), S_OK);
            let mut want = vec![0u8; want_size];
            assert_eq!(xu::XPersistentLocalStorageGetPath(want_size, want.as_mut_ptr(), std::ptr::null_mut()), S_OK);
            assert_eq!(got, std::ffi::CStr::from_bytes_until_nul(&want).unwrap().to_str().unwrap());
            XPackageCloseMountHandle(m);
            assert_eq!(LIVE_MOUNTS.load(Ordering::SeqCst), before);
        }
    }

    #[test]
    fn system_app_specific_device_id_is_44_hex_chars() {
        // GDK requirement: the buffer must be at least 45 bytes (44 hex chars + NUL).
        unsafe {
            let mut used: usize = 0;
            // null buffer
            assert_eq!(XSystemGetAppSpecificDeviceId(0, std::ptr::null_mut(), &mut used), E_INVALIDARG);
            assert_eq!(used, XSYSTEM_APP_SPECIFIC_DEVICE_ID_BYTES);

            // undersized buffer (< 45)
            let mut small = [0u8; 44];
            used = 0;
            assert_eq!(XSystemGetAppSpecificDeviceId(small.len(), small.as_mut_ptr(), &mut used), E_NOT_SUFFICIENT_BUFFER);
            assert_eq!(used, XSYSTEM_APP_SPECIFIC_DEVICE_ID_BYTES);

            // valid buffer (>= 45)
            let mut buf = [0u8; 45];
            used = 0;
            assert_eq!(XSystemGetAppSpecificDeviceId(buf.len(), buf.as_mut_ptr(), &mut used), S_OK);
            assert_eq!(used, XSYSTEM_APP_SPECIFIC_DEVICE_ID_BYTES);
            assert_eq!(buf[44], 0);
            let s = std::str::from_utf8(&buf[..44]).expect("valid utf-8");
            assert_eq!(s.len(), 44);
            assert!(s.chars().all(|c| c.is_ascii_hexdigit()), "app specific device id must be hex digits: {s}");
        }
    }

    #[test]
    fn user_check_privilege_validates_the_user() {
        unsafe {
            let mut has = false;
            let mut reasons = 0xcccc_cccc;

            // null user handle
            assert_eq!(XUserCheckPrivilege(std::ptr::null_mut(), 0, 0, &mut has, &mut reasons), E_INVALIDARG);

            // invalid user handle
            let dummy = 0x1234usize as *mut c_void;
            assert_eq!(XUserCheckPrivilege(dummy, 0, 0, &mut has, &mut reasons), E_INVALIDARG);

            // valid user but null 'has' pointer
            assert_eq!(XUserCheckPrivilege(xu::user_ptr(), 0, 0, std::ptr::null_mut(), &mut reasons), E_INVALIDARG);

            // valid user and valid pointers
            assert_eq!(XUserCheckPrivilege(xu::user_ptr(), 0, 0, &mut has, &mut reasons), S_OK);
            assert!(has);
            assert_eq!(reasons, 0);

            // valid user with null reasons pointer (allowed by the GDK spec)
            has = false;
            assert_eq!(XUserCheckPrivilege(xu::user_ptr(), 0, 0, &mut has, std::ptr::null_mut()), S_OK);
            assert!(has);
        }
    }

    /// Linux mirror of upstream #1571 (`tests/gdk_table.rs`, Windows-only):
    /// the twin embeds the same generated table via
    /// `#[path = "xgameruntime/gdk_table.rs"]`, so the same regeneration
    /// hazards (duplicate rows, bad sort, out-of-range slots, empty names)
    /// must fail loudly here too, on Linux.
    const GDK_EXPECTED_ROWS: usize = 396;
    const GDK_EXPECTED_CLASSES: usize = 24;
    const GDK_EXPECTED_INTERFACES: usize = 34;
    const GDK_MAX_SLOT: u8 = 83;

    fn gdk_hex32(v: u128) -> String {
        format!("{v:032x}")
    }

    #[test]
    fn gdk_table_rows_are_sorted_by_class_interface_slot() {
        let keys: Vec<(String, String, u8)> = super::gdk_table::API
            .iter()
            .map(|&(_, c, i, s)| (gdk_hex32(c), gdk_hex32(i), s))
            .collect();
        let mut expected = keys.clone();
        expected.sort_by(|a, b| (a.0.as_str(), a.1.as_str(), a.2).cmp(&(b.0.as_str(), b.1.as_str(), b.2)));
        assert_eq!(keys, expected, "twin gdk_table::API is not sorted by (class, interface, slot)");
    }

    #[test]
    fn gdk_table_function_names_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for &(name, _, _, _) in super::gdk_table::API {
            assert!(!name.is_empty(), "empty function name");
            assert!(seen.insert(name), "duplicate function name {name}");
        }
    }

    #[test]
    fn gdk_table_has_no_duplicate_class_interface_slot_triples() {
        let mut seen = std::collections::HashSet::new();
        for &(_, class, interface, slot) in super::gdk_table::API {
            assert!(seen.insert((class, interface, slot)), "duplicate (class, interface, slot) {class:#x} {interface:#x} {slot}");
        }
    }

    #[test]
    fn gdk_table_row_and_id_counts_are_stable() {
        assert_eq!(super::gdk_table::API.len(), GDK_EXPECTED_ROWS, "expected {GDK_EXPECTED_ROWS} rows");
        let classes: std::collections::HashSet<u128> =
            super::gdk_table::API.iter().map(|&(_, c, _, _)| c).collect();
        let interfaces: std::collections::HashSet<u128> =
            super::gdk_table::API.iter().map(|&(_, _, i, _)| i).collect();
        assert_eq!(classes.len(), GDK_EXPECTED_CLASSES, "distinct classes");
        assert_eq!(interfaces.len(), GDK_EXPECTED_INTERFACES, "distinct interfaces");
    }

    #[test]
    fn gdk_table_slots_are_within_u8_and_the_known_range() {
        for &(_, _, _, slot) in super::gdk_table::API {
            assert!(slot <= GDK_MAX_SLOT, "slot {slot} exceeds known maximum {GDK_MAX_SLOT}");
        }
        // the twin's vtable builder must cover the full table (highest slot is 83)
        let max_slot = super::gdk_table::API.iter().map(|&(_, _, _, s)| s).max().unwrap_or(0);
        assert!((max_slot as usize) < SLOTS, "SLOTS={SLOTS} truncates slot {max_slot}");
    }

    #[test]
    fn gdk_table_classes_and_interfaces_are_non_zero() {
        for &(_, class, interface, _) in super::gdk_table::API {
            assert!(class != 0, "zero class id");
            assert!(interface != 0, "zero interface id");
        }
    }

    #[test]
    fn gdk_table_source_format_is_32_lowercase_hex_chars() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/hle/xgameruntime/gdk_table.rs");
        let src = std::fs::read_to_string(&path).unwrap();
        let mut rows_seen = 0usize;
        for line in src.lines() {
            let line = line.trim();
            if !line.starts_with("(\"X") {
                continue;
            }
            rows_seen += 1;
            let inner = line.strip_prefix('(').and_then(|l| l.strip_suffix(',')).and_then(|l| l.strip_suffix(')'))
                .unwrap_or_else(|| panic!("unparseable row line: {line}"));
            let mut parts = inner.split(',');
            parts.next().unwrap();
            let class = parts.next().unwrap().trim();
            let interface = parts.next().unwrap().trim();
            let slot = parts.next().unwrap().trim();
            assert!(class.starts_with("0x") && class.len() == 34 && class[2..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "bad class id {class}");
            assert!(interface.starts_with("0x") && interface.len() == 34 && interface[2..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "bad interface id {interface}");
            assert!(slot.chars().all(|c| c.is_ascii_digit()), "bad slot {slot}");
        }
        assert_eq!(rows_seen, GDK_EXPECTED_ROWS, "source row count drifted from {GDK_EXPECTED_ROWS}");
    }
}
