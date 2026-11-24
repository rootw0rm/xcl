//! Linux D3DCompile slice (Phase C step 11s): headless `d3dcompiler_47` shield.
//!
//! The null device never validates bytecode, so compiling is accepting the source
//! and handing back a blob holding it. Every title shader check (`vs_5_0`/`ps_5_0`
//! through `CreateGraphicsPipelineState`) then runs past the compiler instead of
//! failing on the `d3dcompiler_47.dll!D3DCompile` stub.

#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::util::{E_INVALIDARG, HRESULT, S_OK};

/// `HRESULT D3DCompile(src, len, name, defines, include, entry, target, flags1, flags2,
/// code, errors)`: echo the source into a blob. `S_OK` with `*code` set and
/// `*errors` nulled; `E_INVALIDARG` when the source or `code` is missing.
pub unsafe extern "win64" fn D3DCompile(
    src: *const u8,
    len: usize,
    _name: *const u8,
    _defines: *const c_void,
    _include: *const c_void,
    _entry: *const u8,
    _target: *const u8,
    _flags1: u32,
    _flags2: u32,
    code: *mut *mut c_void,
    errors: *mut *mut c_void,
) -> HRESULT {
    if !errors.is_null() {
        *errors = std::ptr::null_mut();
    }
    if src.is_null() || len == 0 || code.is_null() {
        return E_INVALIDARG;
    }
    let hr = crate::hle::d3d12x_linux::new_blob_from(src, len, code);
    if hr != S_OK {
        return hr;
    }
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![("D3DCompile", D3DCompile as usize)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_echoes_source_into_a_blob() {
        unsafe {
            let src = b"float4 main() : SV_Target { return 1; }";
            let mut code = std::ptr::null_mut();
            let mut errors = 0x1234 as *mut c_void;
            assert_eq!(
                D3DCompile(
                    src.as_ptr(),
                    src.len(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    b"main\0".as_ptr(),
                    b"ps_5_0\0".as_ptr(),
                    0,
                    0,
                    &mut code,
                    &mut errors,
                ),
                S_OK
            );
            assert!(!code.is_null() && errors.is_null());
            // the blob round-trips through its COM vtable (a corrupted header
            // would fault here, as the title's first PSO check did).
            let vt = unsafe { *(code as *const *const usize) };
            let get_ptr: unsafe extern "win64" fn(*mut std::ffi::c_void) -> *mut std::ffi::c_void =
                unsafe { std::mem::transmute(*vt.add(3)) };
            let get_size: unsafe extern "win64" fn(*mut std::ffi::c_void) -> usize =
                unsafe { std::mem::transmute(*vt.add(4)) };
            assert_eq!(unsafe { get_size(code) }, src.len());
            assert_eq!(
                unsafe { std::slice::from_raw_parts(get_ptr(code) as *const u8, src.len()) },
                src
            );
            let mut bad = std::ptr::null_mut();
            assert_eq!(
                D3DCompile(
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    0,
                    &mut bad,
                    std::ptr::null_mut(),
                ),
                E_INVALIDARG
            );
        }
    }
}
