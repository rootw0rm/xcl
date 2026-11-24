//! Linux XG slice (Phase C step 11s): win64 texture computer on the shared core.
//!
//! `hle/xg.rs` is platform-independent (only `gpu::tiling` + `util` + `stubs`) and
//! now uses `extern "win64"` throughout (identical to `"system"` on x86_64 Windows,
//! correct for game calls on Linux), so this module re-exports the real addresses
//! the same way `gameinput_linux`/`minuserx_linux` do for their tables.

#![allow(non_snake_case)]

#[path = "xg.rs"]
mod core;

pub use core::{element, optimal_mode, Texture, LAYOUT_BYTES};

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        (
            "XGCreateTextureComputer",
            core::XGCreateTextureComputer as usize,
        ),
        ("XGSuggestSwizzleMode", core::XGSuggestSwizzleMode as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computer_roundtrips_texels() {
        // 80x40 RGBA8 2-mip 2-slice texture, the title's `title_texture` shape.
        let mut d = [0u8; 0x38];
        d[0] = 3; // TEXTURE2D
        d[0x10..0x18].copy_from_slice(&80u64.to_le_bytes());
        d[0x18..0x1C].copy_from_slice(&40u32.to_le_bytes());
        d[0x1C..0x1E].copy_from_slice(&2u16.to_le_bytes());
        d[0x1E..0x20].copy_from_slice(&2u16.to_le_bytes());
        d[0x20..0x24].copy_from_slice(&28u32.to_le_bytes()); // R8G8B8A8_UNORM
        d[0x28] = 1;
        d[0x2C..0x30].copy_from_slice(&0x119u32.to_le_bytes()); // 64KB_S_X
        unsafe {
            let mut xg = std::ptr::null_mut();
            assert_eq!(core::XGCreateTextureComputer(d.as_ptr(), &mut xg), 0);
            assert!(!xg.is_null());
            type Size = unsafe extern "win64" fn(*mut std::ffi::c_void) -> u64;
            let vtbl = *(xg as *const *const usize);
            let size: Size = std::mem::transmute(*vtbl.add(3));
            let n = size(xg) as usize;
            assert!(n > 0);
            let tex = Texture::new(&d).unwrap();
            assert_eq!(tex.size() as usize, n);
            type Into = unsafe extern "win64" fn(
                *mut std::ffi::c_void,
                *mut u8,
                u32,
                u32,
                *const u8,
                u32,
                u32,
            ) -> i32;
            let into: Into = std::mem::transmute(*vtbl.add(8));
            let mut mem = vec![0u8; n];
            let row = vec![7u8; 80 * 4];
            assert_eq!(into(xg, mem.as_mut_ptr(), 0, 0, row.as_ptr(), 80 * 4, 80 * 4 * 40), 0);
            type Release = unsafe extern "win64" fn(*mut std::ffi::c_void) -> u32;
            let release: Release = std::mem::transmute(*vtbl.add(1));
            assert_eq!(release(xg), 0);
        }
    }
}
