//! Platform-neutral HLE DLL routing table (imported DLL pattern → HLE module name).
//!
//! `hle::build` materialises this table into the Windows registry; the Linux `inspect`
//! path uses it without the HLE implementation modules (which stay `cfg(windows)` until
//! Phase C). Windows-only additions (config aliases, the optional `XCL_NET_OFFLINE`
//! routes) stay in `hle::build` after this table.

use crate::util::glob_match;

pub const KERNELX: &str = "kernelx.dll";
pub const XGAMERUNTIME: &str = "xgameruntime.dll";
pub const D3D12X: &str = "d3d12_xs.dll";
pub const XAUDIO2: &str = "xaudio2_9.dll";
pub const MFPLAT: &str = "mfplat.dll";
pub const DXCOMPILER: &str = "dxcompiler_xs.dll";
/// the console's texture layout library (`xg`)
pub const XG: &str = "xg_xs.dll";
/// the console's DirectStorage (`dstorage`)
pub const XCRDAPI: &str = "xcrdapi.dll";
/// the console's user32 subset (windows, the message loop): the PC user32
pub const MINUSERX: &str = "minuserx.dll";
/// the console's audio processor (APU memory, its channel, SHAPE contexts), decoder (xapu) and
/// DSP (chad)
pub const ACPHAL: &str = "acphal.dll";
/// controllers (`GameInputCreate`)
pub const GAMEINPUT: &str = "gameinput.dll";

/// DLLs whose *import* makes a title a console title rather than a regular Windows program.
/// `kernelx.dll` itself marks a title; DLLs merely routed to the kernel module (a PC program's
/// `kernel32.dll`) do not, so check for it separately instead of using `module_for`.
pub fn is_console_game_dll(dll_key: &str) -> bool {
    if dll_key == KERNELX {
        return true;
    }
    matches!(module_for(dll_key), Some(XGAMERUNTIME | D3D12X))
}

/// Ordered exactly like the Windows registry: the first matching pattern wins.
pub const ROUTES: [(&str, &str); 29] = [
    ("kernelx.dll", KERNELX),
    ("kernel32.dll", KERNELX),
    ("kernelbase.dll", KERNELX),
    ("advapi32.dll", KERNELX),
    ("kernel32legacy.dll", KERNELX),
    ("advapi32legacy.dll", KERNELX),
    ("minappmodelx.dll", KERNELX),
    ("api-ms-win-core-*", KERNELX),
    ("ext-ms-win-core-*", KERNELX),
    ("xgameruntime.dll", XGAMERUNTIME),
    ("xgameruntime*.dll", XGAMERUNTIME),
    ("xgameplatform.dll", XGAMERUNTIME),
    ("d3d12_xs.dll", D3D12X),
    ("d3d12_x.dll", D3D12X),
    ("d3d12x.dll", D3D12X),
    ("xaudio2_9.dll", XAUDIO2),
    ("xaudio2_9redist.dll", XAUDIO2),
    (MFPLAT, MFPLAT),
    (MINUSERX, MINUSERX),
    ("ext-ms-win-rtcore-ntuser-window-ansi-*", MINUSERX),
    ("ext-ms-win-rtcore-ntuser-window-l1-*", MINUSERX),
    (XCRDAPI, XCRDAPI),
    (XG, XG),
    ("acphal.dll", ACPHAL),
    ("xapu.dll", ACPHAL),
    ("chad.dll", ACPHAL),
    (GAMEINPUT, GAMEINPUT),
    ("dxcompiler_xs.dll", DXCOMPILER),
    ("dxcompiler_x.dll", DXCOMPILER),
];

/// The HLE module a DLL name routes to, if any.
pub fn module_for(dll_key: &str) -> Option<&'static str> {
    ROUTES.iter().find(|(pattern, _)| glob_match(pattern, dll_key)).map(|&(_, module)| module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_dlls_route_to_their_modules() {
        let cases = [
            ("kernelx.dll", KERNELX),
            ("kernel32.dll", KERNELX),
            ("api-ms-win-core-file-l1-1-0.dll", KERNELX),
            ("ext-ms-win-core-synch-l1-1-0.dll", KERNELX),
            ("xgameruntime.dll", XGAMERUNTIME),
            ("xgameruntime.thunks.dll", XGAMERUNTIME),
            ("xgameplatform.dll", XGAMERUNTIME),
            ("d3d12_xs.dll", D3D12X),
            ("d3d12_x.dll", D3D12X),
            ("d3d12x.dll", D3D12X),
            ("xaudio2_9.dll", XAUDIO2),
            ("xaudio2_9redist.dll", XAUDIO2),
            ("mfplat.dll", MFPLAT),
            ("minuserx.dll", MINUSERX),
            ("ext-ms-win-rtcore-ntuser-window-l1-1-0.dll", MINUSERX),
            ("xcrdapi.dll", XCRDAPI),
            ("xg_xs.dll", XG),
            ("acphal.dll", ACPHAL),
            ("xapu.dll", ACPHAL),
            ("chad.dll", ACPHAL),
            ("gameinput.dll", GAMEINPUT),
            ("dxcompiler_xs.dll", DXCOMPILER),
            ("dxcompiler_x.dll", DXCOMPILER),
        ];
        for (dll, module) in cases {
            assert_eq!(module_for(dll), Some(module), "{dll}");
        }
        assert_eq!(module_for("ucrtbase.dll"), None);
        assert_eq!(module_for("api-ms-win-crt-runtime-l1-1-0.dll"), None);
    }

    #[test]
    fn only_console_modules_mark_a_title() {
        assert!(is_console_game_dll("kernelx.dll"));
        assert!(is_console_game_dll("xgameruntime.thunks.dll"));
        assert!(is_console_game_dll("d3d12_x.dll"));
        // Routed to an HLE module, but also shipped with Windows PCs: not a title marker.
        assert!(!is_console_game_dll("kernel32.dll"));
        assert!(!is_console_game_dll("xaudio2_9.dll"));
        assert!(!is_console_game_dll("mfplat.dll"));
        assert!(!is_console_game_dll("ucrtbase.dll"));
    }
}
