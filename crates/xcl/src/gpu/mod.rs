//! GPU side of XCL: console shader capture and the RDNA2 microcode toolchain
//! (decoder / disassembler – the front-end of the shader recompiler).

// D3D12/DXGI runtime pieces: Windows-only until the Vulkan backend (Phase D).
#[cfg(windows)]
pub mod adapter;
#[cfg(windows)]
pub mod agility;
pub mod bvh;
pub mod desc;
pub mod dxbc;
pub mod dxil;
pub mod effects;
#[cfg(windows)]
pub(crate) mod fence;
pub mod pack;
pub mod rdna2;
pub mod recomp;
mod rdna2_spec;
mod rdna2_xbox;
pub mod shader;
#[cfg(windows)]
pub mod texture;
pub mod tiling;
mod tiling_tables;
#[cfg(windows)]
pub mod uma;
// LINUX-TODO Phase D: full UMA port (D3D12 heap). The emitter only needs the
// window-count bounds, which are console ABI — keep in sync with `uma.rs`.
#[cfg(not(windows))]
pub mod uma {
    pub const MAX_WINDOWS: usize = 4;
    pub const MAX_HEAP_WINDOWS: usize = 16;
}
pub mod xbox;

pub use rdna2_spec::ARCH as RDNA2_ARCH;
