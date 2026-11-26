//! HLE ("high-level emulation") modules: our own implementations of console DLLs.
//!
//! A module is just a name → function address table. Routing decides which imported DLL
//! names go to which module (`kernelx.dll`, `kernel32.dll`, `api-ms-win-core-*` → kernelx;
//! `xgameruntime*.dll` → xgameruntime; `d3d12_xs.dll` → d3d12x …). Config aliases win.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::config::Config;
use crate::util::glob_match;

pub mod acphal;
pub mod bcpack;
pub mod bink;
pub mod d3d12_layout;
pub mod d3d12x;
pub mod dstorage;
pub mod dxcompiler;
pub mod gameinput;
mod input_rules;
pub mod kbm;
pub mod kernelx;
pub mod mfplat;
pub mod netoff;
pub mod opus;
pub mod playstation;
pub mod user;
pub mod xaudio2;
pub mod xma2;
pub mod xg;
pub mod xgameruntime;

/// Build an export list from functions whose Rust names equal the exported names.
#[macro_export]
macro_rules! hle_exports {
    ($($name:ident),* $(,)?) => {
        vec![$( (stringify!($name), $name as usize) ),*]
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

/// The real DLL a name we route here stands for.
pub const SELF: &str = "@self";

impl HleModule {
    fn new(name: &'static str, list: Vec<(&'static str, usize)>, fallback: &'static [&'static str]) -> Self {
        HleModule { name, exports: list.into_iter().collect(), ordinals: HashMap::new(), fallback }
    }

    /// Kernel family: the names routed here (kernel32.dll, api-ms-win-core-*) are real
    /// Windows DLLs, so LoadLibrary / GetModuleHandle hand out the genuine module.
    pub fn aliases_are_system(&self) -> bool {
        self.fallback.contains(&SELF)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Index into `Registry::modules`.
    Hle(usize),
    /// Load the real DLL with LoadLibrary.
    System,
    /// Every import becomes an auto-stub.
    Stub,
}

pub struct Registry {
    pub modules: Vec<HleModule>,
    routes: Vec<(String, Route)>,
}

static REG: OnceLock<Registry> = OnceLock::new();

pub use crate::hle_routes::{
    ACPHAL, D3D12X, DXCOMPILER, GAMEINPUT, KERNELX, MFPLAT, MINUSERX, XAUDIO2, XCRDAPI, XG,
    XGAMERUNTIME,
};

fn build(config: &Config) -> Registry {
    let mut modules = vec![
        HleModule::new(KERNELX, kernelx::exports(), &[SELF, "kernelbase.dll", "kernel32.dll", "advapi32.dll", "sechost.dll", "psapi.dll"]),
        HleModule::new(XGAMERUNTIME, xgameruntime::exports(), &[]),
        HleModule::new(D3D12X, d3d12x::exports(), &[]),
        HleModule::new(XAUDIO2, xaudio2::exports(), &["xaudio2_9.dll"]),
        HleModule::new(MFPLAT, mfplat::exports(), &["mfplat.dll"]),
        HleModule::new(DXCOMPILER, dxcompiler::exports(), &[]),
        HleModule::new(MINUSERX, user::exports(), &["user32.dll"]),
        HleModule::new(XCRDAPI, dstorage::exports(), &[]),
        HleModule::new(XG, xg::exports(), &[]),
        HleModule::new(ACPHAL, acphal::exports(), &[]),
        HleModule::new(GAMEINPUT, gameinput::exports(), &[]),
    ];
    let offline = netoff::enabled();
    if offline {
        // the system's own network functions too, for whatever reaches them (#305)
        netoff::install_system_hooks();
        let mut m = HleModule::new(netoff::NETOFF, netoff::exports(), &[SELF]);
        m.ordinals = netoff::ordinals().into_iter().collect();
        modules.push(m);
    }
    let idx = |n: &str| modules.iter().position(|m| m.name == n).unwrap();
    let mut routes: Vec<(String, Route)> = Vec::new();
    // user aliases first – they take precedence
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
    // Shared with the Linux `inspect` path: one ordered table, no second DLL list.
    for &(pattern, module) in &crate::hle_routes::ROUTES {
        routes.push((pattern.into(), Route::Hle(idx(module))));
    }
    if offline {
        let n = idx(netoff::NETOFF);
        for p in ["winhttp.dll", "ws2_32.dll"] {
            routes.push((p.into(), Route::Hle(n)));
        }
    }
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

    /// Which HLE module (if any) a DLL name routes to.
    pub fn hle_for(&self, dll_key: &str) -> Option<&HleModule> {
        match self.route(dll_key)? {
            Route::Hle(i) => self.modules.get(*i),
            _ => None,
        }
    }

    /// Cross-module lookup: functions whose hosting DLL on the console is unknown
    /// (e.g. XMem*, XThread*) are found wherever we implemented them.
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
    fn cross_module_lookup_finds_xmem() {
        let r = build(&Config::default());
        assert_eq!(r.find_any("XMemAlloc").map(|(m, _)| m.name), Some(KERNELX));
    }
}
