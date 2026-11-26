#![cfg(windows)]
// Windows-only integration surface (real D3D12 driver objects / console-slot contracts); the Linux port gets its own e2e when the Vulkan backend covers this surface (Phase D).
//! Robustness checks for `ID3D12Device::CreateGraphicsPipelineState` and
//! `ID3D12Device::CreateComputePipelineState` (`d3d12x::real`), with no game data.
//!
//! Runs on any D3D12 adapter (AMD, NVIDIA, Intel, or WARP) with zero vendor-specific skips:
//! 1. Console graphics PSO with null `SemanticName` does not crash `own_graphics`.
//! 2. Unreadable `pInputElementDescs`, overflowing `NumElements`, unreadable `SemanticName`,
//!    and cross-page unterminated `SemanticName` return `E_INVALIDARG` cleanly.
//! 3. Unreadable shader bytecodes, `CachedPSO`, `StreamOutput` pointers, and unreadable
//!    top-level desc pointers on graphics and compute PSO hooks return `E_INVALIDARG` cleanly.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{GUID, HRESULT, Interface, PCSTR};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE};

use xcl::gpu::recomp::runner;
use xcl::hle::d3d12x::real;

fn device() -> Option<ID3D12Device> {
    static DEV: OnceLock<Option<ID3D12Device>> = OnceLock::new();
    DEV.get_or_init(|| unsafe {
        if real::init().is_err() {
            runner::skip_test("d3d12x::real::init failed");
            return None;
        }
        let raw = real::device_raw();
        ID3D12Device::from_raw_borrowed(&raw).cloned()
    })
    .clone()
}

/// Minimal synthetic console shader DXBC container recognized by `pso::is_console`
/// (`kind = 1` for `"VS"`, `kind = 5` for `"CS"`), with no game data.
fn synthetic_console_shader(kind: u32) -> Vec<u8> {
    let dxil_size = 32u32;
    let total_size = 36u32 + 8 + dxil_size;
    let mut b = Vec::with_capacity(total_size as usize);
    b.extend_from_slice(b"DXBC");
    b.extend_from_slice(&[0u8; 16]);
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&total_size.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&36u32.to_le_bytes());
    b.extend_from_slice(b"DXIL");
    b.extend_from_slice(&dxil_size.to_le_bytes());
    let pv = (kind << 16) | 0x65;
    b.extend_from_slice(&pv.to_le_bytes());
    b.extend_from_slice(&[0u8; 20]);
    b.extend_from_slice(b"XDAT");
    b.extend_from_slice(&0u32.to_le_bytes());
    assert!(xcl::gpu::recomp::pso::is_console(&b));
    b
}

fn bytecode(b: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE { pShaderBytecode: b.as_ptr() as *const c_void, BytecodeLength: b.len() }
}

#[test]
fn graphics_pso_null_semantic_name_does_not_crash() {
    let Some(dev) = device() else { return };
    let vs = synthetic_console_shader(1);
    let elem = D3D12_INPUT_ELEMENT_DESC::default();
    assert!(elem.SemanticName.0.is_null());

    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    desc.VS = bytecode(&vs);
    desc.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: &elem, NumElements: 1 };
    desc.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    desc.SampleMask = u32::MAX;
    desc.SampleDesc.Count = 1;

    // Must not dereference NULL in `own_graphics` (`CStr::from_ptr(null)`).
    let _ = unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&desc) };
}

#[test]
fn graphics_pso_unreadable_input_layout_and_semantic_name_return_invalidarg() {
    let Some(dev) = device() else { return };
    let vs = synthetic_console_shader(1);

    let mut base = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    base.VS = bytecode(&vs);
    base.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    base.SampleMask = u32::MAX;
    base.SampleDesc.Count = 1;

    // 1. Unreadable pInputElementDescs pointer
    let mut d1 = base.clone();
    d1.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: 0x1000 as *const D3D12_INPUT_ELEMENT_DESC, NumElements: 1 };
    let err = unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&d1) }.expect_err("unreadable InputLayout must fail");
    assert_eq!(err.code(), E_INVALIDARG);

    // 2. Overflowing / out-of-bounds NumElements
    let valid_elem = D3D12_INPUT_ELEMENT_DESC {
        SemanticName: PCSTR(b"POSITION\0".as_ptr()),
        ..Default::default()
    };
    let mut d2 = base.clone();
    d2.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: &valid_elem, NumElements: u32::MAX };
    let err = unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&d2) }.expect_err("huge NumElements must fail");
    assert_eq!(err.code(), E_INVALIDARG);

    // 3. Unreadable SemanticName pointer
    let bad_name_elem = D3D12_INPUT_ELEMENT_DESC {
        SemanticName: PCSTR(0x1000 as *const u8),
        ..Default::default()
    };
    let mut d3 = base.clone();
    d3.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: &bad_name_elem, NumElements: 1 };
    let err = unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&d3) }.expect_err("unreadable SemanticName must fail");
    assert_eq!(err.code(), E_INVALIDARG);

    // 4. Unterminated SemanticName reaching into an uncommitted guard page
    unsafe {
        let base_mem = VirtualAlloc(std::ptr::null(), 0x2000, MEM_RESERVE, PAGE_NOACCESS);
        assert!(!base_mem.is_null(), "VirtualAlloc MEM_RESERVE failed");
        let page0 = VirtualAlloc(base_mem, 0x1000, MEM_COMMIT, PAGE_READWRITE) as *mut u8;
        assert!(!page0.is_null(), "VirtualAlloc MEM_COMMIT failed");
        std::ptr::write_bytes(page0.add(0xFF0), b'A', 16);

        let guard_elem = D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(page0.add(0xFF0)),
            ..Default::default()
        };
        let mut d4 = base;
        d4.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: &guard_elem, NumElements: 1 };
        let res = dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&d4);
        VirtualFree(base_mem, 0, MEM_RELEASE);
        assert_eq!(res.expect_err("unterminated SemanticName must fail").code(), E_INVALIDARG);
    }
}

#[test]
fn graphics_and_compute_pso_unreadable_shader_and_desc_pointers_return_invalidarg() {
    let Some(dev) = device() else { return };
    let bad_bc = D3D12_SHADER_BYTECODE { pShaderBytecode: 0x1000 as *const c_void, BytecodeLength: 64 };
    let bad_cache = D3D12_CACHED_PIPELINE_STATE { pCachedBlob: 0x1000 as *const c_void, CachedBlobSizeInBytes: 64 };

    // Graphics: unreadable VS, PS, CachedPSO, StreamOutput pointers
    let mut g_vs = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    g_vs.VS = bad_bc;
    assert_eq!(unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&g_vs) }.expect_err("unreadable VS").code(), E_INVALIDARG);

    let mut g_ps = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    g_ps.PS = bad_bc;
    assert_eq!(unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&g_ps) }.expect_err("unreadable PS").code(), E_INVALIDARG);

    let mut g_cache = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    g_cache.CachedPSO = bad_cache;
    assert_eq!(unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&g_cache) }.expect_err("unreadable graphics CachedPSO").code(), E_INVALIDARG);

    let mut g_so_decl = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    g_so_decl.StreamOutput.pSODeclaration = 0x1000 as *const D3D12_SO_DECLARATION_ENTRY;
    g_so_decl.StreamOutput.NumEntries = 1;
    assert_eq!(unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&g_so_decl) }.expect_err("unreadable pSODeclaration").code(), E_INVALIDARG);

    let mut g_so_strides = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    g_so_strides.StreamOutput.pBufferStrides = 0x1000 as *const u32;
    g_so_strides.StreamOutput.NumStrides = 1;
    assert_eq!(unsafe { dev.CreateGraphicsPipelineState::<ID3D12PipelineState>(&g_so_strides) }.expect_err("unreadable pBufferStrides").code(), E_INVALIDARG);

    // Compute: unreadable CS and CachedPSO
    let mut c_cs = D3D12_COMPUTE_PIPELINE_STATE_DESC::default();
    c_cs.CS = bad_bc;
    assert_eq!(unsafe { dev.CreateComputePipelineState::<ID3D12PipelineState>(&c_cs) }.expect_err("unreadable CS").code(), E_INVALIDARG);

    let mut c_cache = D3D12_COMPUTE_PIPELINE_STATE_DESC::default();
    c_cache.CachedPSO = bad_cache;
    assert_eq!(unsafe { dev.CreateComputePipelineState::<ID3D12PipelineState>(&c_cache) }.expect_err("unreadable compute CachedPSO").code(), E_INVALIDARG);

    // Raw COM vtable calls with unreadable top-level desc pointer (slots 10 & 11 of ID3D12Device)
    unsafe {
        type PsoFn = unsafe extern "system" fn(*mut c_void, *const c_void, *const GUID, *mut *mut c_void) -> HRESULT;
        let raw = real::device_raw();
        let vtbl = *(raw as *const *const usize);
        let create_gfx: PsoFn = std::mem::transmute(*vtbl.add(10));
        let create_cmp: PsoFn = std::mem::transmute(*vtbl.add(11));
        let iid = ID3D12PipelineState::IID;

        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(create_gfx(raw, 0x1000 as *const c_void, &iid, &mut out), E_INVALIDARG);
        assert!(out.is_null());

        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(create_cmp(raw, 0x1000 as *const c_void, &iid, &mut out), E_INVALIDARG);
        assert!(out.is_null());
    }
}
