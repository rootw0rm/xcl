//! XCL — Xbox Series X|S → Windows compatibility layer.
//!
//! See `docs/specs/2026-11-20-xcl-design.md` for the design.
//!
//! The host runtime (loader, TLS, EH, HLE, VFS, hooks) is Windows-only for now; on Linux
//! those modules stay out of the build and the CLI returns clean "not supported on Linux
//! yet (Phase …)" errors instead. The offline tools (inspect, disasm, shaders, rpf) and
//! the RDNA2 front-end are platform-neutral.

// Function → address casts are the whole point of an import resolver.
#![allow(function_casts_as_integer)]

#[macro_use]
pub mod log;
pub mod manifest;
pub mod config;
pub mod disasm;
#[cfg(windows)]
pub mod eh;
pub mod gpu;
#[cfg(windows)]
pub mod hle;
pub mod hle_routes;
#[cfg(windows)]
pub mod hooks;
#[cfg(windows)]
pub mod host;
pub mod insn;
pub mod inspect;
#[cfg(windows)]
pub mod launcher;
#[cfg(windows)]
pub mod loader;
#[cfg(windows)]
pub mod modules;
pub mod pe;
pub mod platform;
#[cfg(windows)]
pub mod preflight;
#[cfg(windows)]
pub mod report;
#[cfg(windows)]
pub mod resolver;
pub mod rpf;
mod rpf_probe;
pub mod shaders;
pub mod state;
#[cfg(windows)]
pub mod stubs;
#[cfg(windows)]
pub mod tls;
pub mod util;
#[cfg(windows)]
pub mod vfs;
#[cfg(windows)]
pub mod vfs_hooks;
