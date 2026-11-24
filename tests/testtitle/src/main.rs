//! XCL test title – a fake "console executable".
//!
//! Its import table names console DLLs (kernelx.dll, xgameruntime.dll, d3d12_xs.dll) and
//! a DLL shipped "with the game" (testgame_mw.dll). Plain Windows cannot start it; XCL
//! can. Every check prints [PASS]/[FAIL]; the exit code is the number of failures.
//!
//! The imports (raw-dylib) and the D3D12 types are Windows-only: the PE is cross-built
//! for a Windows target (Phase C), not for the Linux host.

#[cfg(windows)]
include!("title_body.rs");

#[cfg(not(windows))]
fn main() {
    eprintln!("xcl_testtitle is a Windows PE test title; cross-build it for a Windows target (Phase C).");
}
