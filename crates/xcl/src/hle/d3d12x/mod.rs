//! d3d12_xs.dll – D3D12.X.
//!
//! Backends:
//! * `real` (default): genuine PC D3D12 objects from the GPU driver with extended
//!   vtables (PC methods go straight to the driver, console-only slots are guarded and
//!   logged), shader capture, fallback pipeline states, presentation window.
//! * `null`: stand-in objects that render nothing (`graphics = "null"` in xcl.toml or
//!   when no D3D12 device can be created).

#![allow(non_snake_case)]

mod alloc_pool;
pub mod bind;
pub mod capture;
pub mod console_heap;
pub mod console_list;
pub mod device_x;
pub mod dxgi_device;
mod cpu_writes;
pub mod dred;
mod fence_x;
pub mod frame;
pub mod guard;
mod late_release;
pub mod layout;
pub mod list_x;
pub mod null;
pub mod present;
pub mod pso_async;
pub mod real;
mod shader_cache_session;
pub mod sparse;
mod standin;
pub mod title_list;
pub mod title_textures;
mod uma_driver;
pub mod vram;

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::util::{HRESULT, S_OK};

/// data export: titles pass `&IID_ID3D12Device` to D3D12XboxCreateDevice / QueryInterface
static IID_ID3D12_DEVICE: windows::core::GUID = windows::core::GUID::from_u128(0x189819f1_1db6_4b57_be54_1821339b85f7);

pub fn exports() -> Vec<(&'static str, usize)> {
    let mut v = exports_fns();
    v.push(("IID_ID3D12Device", &IID_ID3D12_DEVICE as *const _ as usize));
    v
}

fn exports_fns() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        D3D12CreateDevice,
        D3D12XboxCreateDevice,
        D3D12GetDebugInterface,
        D3D12SerializeRootSignature,
        D3D12SerializeVersionedRootSignature,
        D3D12CreateRootSignatureDeserializer,
        D3D12CreateVersionedRootSignatureDeserializer,
        D3D12EnableExperimentalFeatures,
        D3D12GetInterface,
        D3D10CreateBlob,
        D3D12XboxSetProcessDebugFlags,
        D3D12ConfigureVirtualMemoryX,
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Real,
    Null,
}

/// Decided once: the real backend unless disabled or the driver refuses.
static BACKEND: OnceLock<Backend> = OnceLock::new();

fn backend_with_remap(allow_driver_remap: bool) -> Backend {
    *BACKEND.get_or_init(|| {
        let cfg = crate::state::config();
        if cfg.graphics.eq_ignore_ascii_case("null") {
            crate::xlog_info!("D3D12.X: backend 'null' (from the config)");
            return Backend::Null;
        }
        match real::init_with_remap(allow_driver_remap) {
            Ok(desc) => {
                crate::xlog_info!("D3D12.X: backend 'real' – {desc}");
                Backend::Real
            }
            Err(e) => {
                crate::xlog_warn!("D3D12.X: could not create a D3D12 device ({e}) – switching to the fallback device");
                Backend::Null
            }
        }
    })
}

pub fn backend() -> Backend {
    backend_with_remap(false)
}

pub(crate) fn backend_before_guest() -> Backend {
    backend_with_remap(true)
}

/// Call a function of the system d3d12.dll by name.
unsafe fn sys(name: &str) -> Option<usize> {
    crate::resolver::system_proc("d3d12.dll", &crate::pe::ImportName::Name { hint: 0, name: name.into() })
}

macro_rules! forward {
    ($name:literal, $ty:ty, $null:expr, ($($a:ident),*)) => {{
        if backend() == Backend::Real {
            if let Some(f) = sys($name) {
                let f: $ty = std::mem::transmute(f);
                return f($($a),*);
            }
        }
        $null($($a),*)
    }};
}

pub unsafe extern "system" fn D3D12CreateDevice(adapter: *mut c_void, fl: u32, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if backend() == Backend::Real {
        return real::create_device(riid, ppv);
    }
    null::D3D12CreateDevice(adapter, fl, riid, ppv)
}

pub unsafe extern "system" fn D3D12XboxCreateDevice(adapter: *mut c_void, params: *const c_void, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    // D3D12XBOX_CREATE_DEVICE_PARAMETERS: the first word is the SDK version the title was
    // built with (the console's vtable layouts follow it)
    if !params.is_null() && crate::gpu::shader::readable(params as usize, 32) {
        let w: Vec<u32> = (0..8).map(|k| (params as *const u32).add(k).read_unaligned()).collect();
        crate::xlog_info!("D3D12XboxCreateDevice: parameters {w:x?} (SDK version {})", w[0]);
    }
    if backend() == Backend::Real {
        return real::create_device(riid, ppv);
    }
    null::D3D12XboxCreateDevice(adapter, params, riid, ppv)
}

pub unsafe extern "system" fn D3D12GetDebugInterface(riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!("D3D12GetDebugInterface", unsafe extern "system" fn(*const c_void, *mut *mut c_void) -> HRESULT, null::D3D12GetDebugInterface, (riid, ppv))
}

// console-only root signature flags (seen: 0x3000000 from XboxBvhBuilder.dll) – Windows
// rejects unknown bits; flags sit at +32 in 40-byte 1.0 desc and +40 in 48-byte versioned desc
const PC_FLAGS: u32 = 0xFFF;

unsafe fn serialize_root_sig<const QWORDS: usize>(
    name: &str, d: *const c_void, flags_off: usize, err: *mut *mut c_void,
    meta: impl FnOnce(*const u8) -> (u32, u32, u32, u32),
    call: impl FnOnce(*const c_void, *mut *mut c_void) -> HRESULT,
) -> HRESULT {
    let mut own_err: *mut c_void = std::ptr::null_mut();
    let e = if err.is_null() { &mut own_err as *mut _ } else { *err = std::ptr::null_mut(); err };
    let mut copy = [0u64; QWORDS];
    let mut d = d;
    if !d.is_null() && (d as *const u8).add(flags_off).cast::<u32>().read_unaligned() & !PC_FLAGS != 0 {
        let dst = copy.as_mut_ptr().cast::<u8>();
        std::ptr::copy_nonoverlapping(d.cast::<u8>(), dst, QWORDS * 8);
        let f = dst.add(flags_off).cast::<u32>();
        crate::xlog_info!("{name}: console flags {:#x} dropped", *f & !PC_FLAGS);
        *f &= PC_FLAGS;
        d = dst.cast::<c_void>();
    }
    let hr = call(d, e);
    if hr < 0 && !d.is_null() {
        let (ver, params, samplers, flags) = meta(d.cast::<u8>());
        crate::xlog_warn!("{name}: {hr:#x} (version {ver}, parameters {params}, samplers {samplers}, flags {flags:#x}): {}", blob_text(*e));
    }
    if err.is_null() && !own_err.is_null() {
        blob_release(own_err);
    }
    hr
}

pub unsafe extern "system" fn D3D12SerializeRootSignature(d: *const c_void, v: u32, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT {
    type Serialize = unsafe extern "system" fn(*const c_void, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
    serialize_root_sig::<5>("D3D12SerializeRootSignature", d, 32, err, |p| (v, p.cast::<u32>().read_unaligned(), p.add(16).cast::<u32>().read_unaligned(), p.add(32).cast::<u32>().read_unaligned()), |d, e| {
        match sys("D3D12SerializeRootSignature").filter(|_| backend() == Backend::Real) {
            Some(f) => std::mem::transmute::<_, Serialize>(f)(d, v, blob, e),
            None => null::D3D12SerializeRootSignature(d, v, blob, e),
        }
    })
}

pub unsafe extern "system" fn D3D12SerializeVersionedRootSignature(d: *const c_void, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT {
    type Serialize = unsafe extern "system" fn(*const c_void, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
    serialize_root_sig::<6>("D3D12SerializeVersionedRootSignature", d, 40, err, |p| (p.cast::<u32>().read_unaligned(), p.add(8).cast::<u32>().read_unaligned(), p.add(24).cast::<u32>().read_unaligned(), p.add(40).cast::<u32>().read_unaligned()), |d, e| {
        match sys("D3D12SerializeVersionedRootSignature").filter(|_| backend() == Backend::Real) {
            Some(f) => std::mem::transmute::<_, Serialize>(f)(d, blob, e),
            None => null::D3D12SerializeVersionedRootSignature(d, blob, e),
        }
    })
}

/// text of an ID3DBlob (error messages)
unsafe fn blob_text(b: *mut c_void) -> String {
    if b.is_null() {
        return String::new();
    }
    let vt = *(b as *const *const usize);
    let ptr: unsafe extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(*vt.add(3));
    let len: unsafe extern "system" fn(*mut c_void) -> usize = std::mem::transmute(*vt.add(4));
    String::from_utf8_lossy(std::slice::from_raw_parts(ptr(b), len(b))).trim_end_matches('\0').trim().to_string()
}

unsafe fn blob_release(b: *mut c_void) {
    let vt = *(b as *const *const usize);
    let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(2));
    release(b);
}

pub unsafe extern "system" fn D3D12CreateRootSignatureDeserializer(d: *const c_void, l: usize, r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12CreateRootSignatureDeserializer",
        unsafe extern "system" fn(*const c_void, usize, *const c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12CreateRootSignatureDeserializer,
        (d, l, r, ppv)
    )
}

pub unsafe extern "system" fn D3D12CreateVersionedRootSignatureDeserializer(d: *const c_void, l: usize, r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12CreateVersionedRootSignatureDeserializer",
        unsafe extern "system" fn(*const c_void, usize, *const c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12CreateVersionedRootSignatureDeserializer,
        (d, l, r, ppv)
    )
}

pub unsafe extern "system" fn D3D12EnableExperimentalFeatures(n: u32, i: *const c_void, c: *mut c_void, s: *mut u32) -> HRESULT {
    forward!(
        "D3D12EnableExperimentalFeatures",
        unsafe extern "system" fn(u32, *const c_void, *mut c_void, *mut u32) -> HRESULT,
        null::D3D12EnableExperimentalFeatures,
        (n, i, c, s)
    )
}

pub unsafe extern "system" fn D3D12GetInterface(clsid: *const c_void, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12GetInterface",
        unsafe extern "system" fn(*const c_void, *const c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12GetInterface,
        (clsid, riid, ppv)
    )
}

pub unsafe extern "system" fn D3D10CreateBlob(size: usize, out: *mut *mut c_void) -> HRESULT {
    null::D3D10CreateBlob(size, out)
}

pub unsafe extern "system" fn D3D12XboxSetProcessDebugFlags(_flags: u32) -> HRESULT {
    S_OK
}

/// How the console splits the title's GPU virtual address space (a 32-byte configuration;
/// the retail game's second dword is a count of 16 GiB units + 2). The PC's GPU addresses come from its
/// own device, so there is nothing to configure – but a title asserts (`int3`) on a failure.
pub unsafe extern "system" fn D3D12ConfigureVirtualMemoryX(config: *const c_void) -> HRESULT {
    if !config.is_null() && crate::gpu::shader::readable(config as usize, 32) {
        let w: Vec<u32> = (0..8).map(|k| (config as *const u32).add(k).read_unaligned()).collect();
        crate::xlog_info!("D3D12ConfigureVirtualMemoryX: {w:x?} – accepted (nothing to configure on the PC)");
    }
    S_OK
}

pub(crate) mod host_bvh;

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_ROOT_SIGNATURE_DESC, D3D12_ROOT_SIGNATURE_FLAGS, D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
        D3D12_ROOT_SIGNATURE_FLAG_DENY_PIXEL_SHADER_ROOT_ACCESS, D3D12_VERSIONED_ROOT_SIGNATURE_DESC,
        D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0, D3D_ROOT_SIGNATURE_VERSION_1_0, ID3D12RootSignatureDeserializer,
    };

    #[test]
    fn serialize_root_signature_strips_console_flags_and_preserves_pc_flags() {
        assert_eq!((std::mem::size_of::<D3D12_ROOT_SIGNATURE_DESC>(), std::mem::offset_of!(D3D12_ROOT_SIGNATURE_DESC, Flags)), (40, 32));
        assert_eq!((std::mem::size_of::<D3D12_VERSIONED_ROOT_SIGNATURE_DESC>(), std::mem::offset_of!(D3D12_VERSIONED_ROOT_SIGNATURE_DESC, Anonymous) + std::mem::offset_of!(D3D12_ROOT_SIGNATURE_DESC, Flags)), (48, 40));
        unsafe {
            let deser_flags = |blob_raw: *mut c_void| -> D3D12_ROOT_SIGNATURE_FLAGS {
                let blob = ID3DBlob::from_raw(blob_raw);
                let mut deser_raw = std::ptr::null_mut();
                assert_eq!(D3D12CreateRootSignatureDeserializer(blob.GetBufferPointer(), blob.GetBufferSize(), (&ID3D12RootSignatureDeserializer::IID as *const windows::core::GUID).cast(), &mut deser_raw), S_OK);
                (*ID3D12RootSignatureDeserializer::from_raw(deser_raw).GetRootSignatureDesc()).Flags
            };
            let pc_flags = D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT | D3D12_ROOT_SIGNATURE_FLAG_DENY_PIXEL_SHADER_ROOT_ACCESS;
            let raw_flags = D3D12_ROOT_SIGNATURE_FLAGS(pc_flags.0 | 0x0300_0000);
            let desc = D3D12_ROOT_SIGNATURE_DESC { Flags: raw_flags, ..Default::default() };
            let (mut blob_raw, mut vblob_raw) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(D3D12SerializeRootSignature(&desc as *const _ as *const c_void, D3D_ROOT_SIGNATURE_VERSION_1_0.0 as u32, &mut blob_raw, std::ptr::null_mut()), S_OK);
            assert_eq!(desc.Flags, raw_flags, "caller descriptor must not be mutated");
            assert_eq!(deser_flags(blob_raw), pc_flags);

            let vdesc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC { Version: D3D_ROOT_SIGNATURE_VERSION_1_0, Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 { Desc_1_0: desc } };
            assert_eq!(D3D12SerializeVersionedRootSignature(&vdesc as *const _ as *const c_void, &mut vblob_raw, std::ptr::null_mut()), S_OK);
            assert_eq!(deser_flags(vblob_raw), pc_flags);

            let (mut out, mut err_out) = (std::ptr::null_mut(), 1usize as *mut c_void);
            assert!(D3D12SerializeRootSignature(&desc as *const _ as *const c_void, 99, &mut out, std::ptr::null_mut()) < 0);
            assert!(D3D12SerializeRootSignature(&desc as *const _ as *const c_void, 99, &mut out, &mut err_out) < 0);
            assert_ne!(err_out, 1usize as *mut c_void, "caller error pointer must be initialized");
            // In-box Windows 10 d3d12.dll (without Agility SDK) leaves the error blob null/empty for unknown versions.
            if !err_out.is_null() {
                let text = blob_text(err_out);
                assert!(text.is_empty() || text.contains("version"), "unexpected error blob text: {text}");
                blob_release(err_out);
            }
            err_out = 1usize as *mut c_void;
            assert!(D3D12SerializeRootSignature(std::ptr::null(), D3D_ROOT_SIGNATURE_VERSION_1_0.0 as u32, &mut out, &mut err_out) < 0);
            assert!(err_out.is_null());

            // Runtime-independent error-blob forwarding and internal release checks.
            #[repr(C)]
            struct FakeBlob { vt: *const [usize; 5], releases: u32 }
            unsafe extern "system" fn fake_release(this: *mut c_void) -> u32 { (*(this as *mut FakeBlob)).releases += 1; 0 }
            unsafe extern "system" fn fake_ptr(_: *mut c_void) -> *const u8 { b"bad root sig\0".as_ptr() }
            unsafe extern "system" fn fake_len(_: *mut c_void) -> usize { 13 }
            let vt = [0, 0, fake_release as *const () as usize, fake_ptr as *const () as usize, fake_len as *const () as usize];
            let mut fake = FakeBlob { vt: &vt, releases: 0 };
            let fake_p = &mut fake as *mut FakeBlob as *mut c_void;
            err_out = 1usize as *mut c_void;
            assert_eq!(serialize_root_sig::<5>("test", &desc as *const _ as *const c_void, 32, &mut err_out, |_| (99, 0, 0, 0), |_, e| { assert!((*e).is_null()); *e = fake_p; -2147024809 }), -2147024809);
            assert_eq!((err_out, fake.releases), (fake_p, 0));
            assert_eq!(serialize_root_sig::<5>("test", &desc as *const _ as *const c_void, 32, std::ptr::null_mut(), |_| (99, 0, 0, 0), |_, e| { *e = fake_p; -2147024809 }), -2147024809);
            assert_eq!(fake.releases, 1);
        }
    }
}
