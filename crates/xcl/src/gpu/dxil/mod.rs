//! Direct DXIL emission: console GPU code → DXIL containers without HLSL or
//! DXC. Design and roadmap: `docs/specs/2026-11-24-direct-dxil.md`.
//!
//! Layers, bottom up:
//! * [`bitstream`] — LLVM bitstream writer (unabbreviated records only);
//! * [`module`] — an LLVM 3.7 module (types, constants, one entry function,
//!   metadata) and its bitcode;
//! * [`shader`] — DXIL on top: `dx.op` intrinsics, resource handles,
//!   entry-point metadata, shader flags;
//! * [`container`] — the DXBC container (`SFI0`, signatures, `PSV0`,
//!   `DXIL`) and its digest, so no SDK DLL is needed at run time;
//! * [`validate`] — `dxil.dll` validator and DXC disassembler as test oracles.

pub mod bitstream;
pub mod cfg;
mod compact;
pub mod container;
pub mod coverage;
pub mod graphics;
pub mod kernel;
mod lanes;
pub mod lower;
mod lower_geom;
mod lower_gfx;
pub(crate) mod lower_image;
mod lower_mem;
mod lower_round;
mod lower_valu;
mod lower_wide;
mod lower_window;
pub mod module;
pub mod ops;
pub mod ops_core;
mod prelude;
pub mod reload;
pub mod shader;
pub mod ssa;
#[cfg(windows)]
pub mod validate;

/// The validator version our metadata declares (`dx.valver`). It stays 1.8 rather than
/// 1.7: the PSV0 part carries `PSVRuntimeInfo3` (the entry name), which validator 1.8
/// introduced, so a 1.7 validator would reject the container anyway.
pub const VALVER: (u32, u32) = (1, 8);

// LINUX-TODO Phase D: GPU/DXC-backed translation test; runner/dxc are
// Windows-only until the SPIR-V backend lands.
#[cfg(all(test, windows))]
mod compact_tests;
#[cfg(all(test, windows))]
mod core_tests;
#[cfg(all(test, windows))]
mod tests;
#[cfg(all(test, windows))]
mod ops_tests;
#[cfg(test)]
mod bitreader_tests;
#[cfg(test)]
mod robustness_tests;
// LINUX-TODO Phase D: GPU/DXC-backed translation tests; validate/runner/dxc
// are Windows-only until the SPIR-V backend lands.
#[cfg(all(test, windows))]
mod fuzz_tests;
#[cfg(all(test, windows))]
mod msaa_tests;
#[cfg(all(test, windows))]
mod round_tests;
#[cfg(all(test, windows))]
mod geometry_tests;
