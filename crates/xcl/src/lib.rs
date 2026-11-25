//! XCL — Xbox Series X|S → Windows compatibility layer.
//!
//! See `docs/specs/2026-11-20-xcl-design.md` for the design.
//!
//! The host runtime is split by platform: Windows keeps the full loader/HLE path, while Linux
//! gains the Phase-C modules incrementally and returns clean "not supported on Linux yet
//! (Phase …)" errors for the rest. The offline tools (inspect, disasm, shaders, rpf) and the
//! RDNA2 front-end are platform-neutral.

// Function → address casts are the whole point of an import resolver.
#![allow(function_casts_as_integer)]

#[macro_use]
pub mod log;
pub mod manifest;
pub mod config;
pub mod disasm;
#[cfg(windows)]
pub mod eh;
#[cfg(target_os = "linux")]
#[path = "eh_linux.rs"]
pub mod eh;
pub mod gpu;
#[cfg(windows)]
pub mod hle;
#[cfg(not(windows))]
#[path = "hle_linux.rs"]
pub mod hle;
pub mod hle_routes;
#[cfg(windows)]
pub mod hooks;
#[cfg(windows)]
pub mod host;
#[cfg(not(windows))]
#[path = "host_linux.rs"]
pub mod host;
pub mod insn;
pub mod inspect;
#[cfg(windows)]
pub mod launcher;
#[cfg(not(windows))]
#[path = "launcher_linux.rs"]
pub mod launcher;
#[cfg(windows)]
pub mod loader;
#[cfg(not(windows))]
#[path = "loader_linux.rs"]
pub mod loader;
pub mod modules;
pub mod pdata;
pub mod pe;
pub mod platform;
#[cfg(windows)]
pub mod preflight;
pub mod report;
#[cfg(windows)]
pub mod resolver;
#[cfg(not(windows))]
#[path = "resolver_linux.rs"]
pub mod resolver;
pub mod rpf;
mod rpf_probe;
pub mod shaders;
#[cfg(test)]
mod source_hash;
pub mod state;
#[cfg(windows)]
pub mod stubs;
#[cfg(not(windows))]
#[path = "stubs_linux.rs"]
pub mod stubs;
#[cfg(windows)]
pub mod tls;
#[cfg(not(windows))]
#[path = "tls_linux.rs"]
pub mod tls;
pub mod util;
pub mod vfs;
#[cfg(windows)]
pub mod vfs_hooks;
#[cfg(not(windows))]
#[path = "vfs_hooks_linux.rs"]
pub mod vfs_hooks_linux;
#[cfg(not(windows))]
pub use vfs_hooks_linux as vfs_hooks;
