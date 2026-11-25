//! Linux D3D12.X slice (Phase C step 11r): headless null device.
//!
//! Port of `hle/d3d12x/null.rs` without `windows`/`windows-sys`: the same
//! vtable guesses (PC D3D12 order, 256 slots, logging stubs for unknown slots)
//! with `extern "win64"` shims, `libc` backing memory and `kernelx_linux`
//! events for fence completion. `D3D12CreateDevice`/`D3D12XboxCreateDevice`
//! hand the static null device; queue signal completes fences instantly.
//!
//! Objects accept every call, log it, and hand back plausible outputs (child objects,
//! descriptor handles, completed fences, mappable memory) so a title can get past
//! graphics initialisation and keep loading data. Nothing is rendered. Used when the
//! real backend is disabled (`graphics = "null"`) or no D3D12 device can be created.
//!
//! Vtable layout guess: public PC D3D12 order (DirectX-Headers). Every slot we do not
//! implement is a logging stub named "<Interface>::#<slot>", so reports from real titles
//! show exactly where the console layout differs.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::hle::vulkan_linux::{Backend, Selection, VulkanState, acquire, select_cached};
use crate::util::{E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, HRESULT, S_FALSE, S_OK};

const VTBL_SLOTS: usize = 256;
const DLL: &str = "d3d12_xs.dll";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Device,
    /// A console device backed by a real Vulkan device (step 17): same vtable
    /// shape as [`Kind::Device`] today (every method delegates to the null
    /// implementation), but the object's `value` holds the
    /// [`VulkanState`] pointer and future slices override creation slots here
    /// without touching the headless default.
    VulkanDevice,
    CommandQueue,
    CommandAllocator,
    CommandList,
    /// A command list from a Vulkan device (step 17): the graphics recording
    /// slots stay logging stubs like [`Kind::CommandList`], but the console
    /// memory methods (#60/#61/#69–#72/#89/#93) run for real on host memory.
    /// The object's `value` holds the creator device object.
    VulkanList,
    Fence,
    DescriptorHeap,
    Resource,
    /// A console `CreatePlacedResourceX` resource: its GPU VA is the title's own
    /// memory (host-accessible), not a fake bump-allocator address.
    PlacedResource,
    Heap,
    PipelineState,
    RootSignature,
    QueryHeap,
    CommandSignature,
    PipelineLibrary,
    StateObject,
    Blob,
}

impl Kind {
    fn iface(self) -> &'static str {
        match self {
            Kind::Device | Kind::VulkanDevice => "ID3D12Device",
            Kind::CommandQueue => "ID3D12CommandQueue",
            Kind::CommandAllocator => "ID3D12CommandAllocator",
            Kind::CommandList | Kind::VulkanList => "ID3D12GraphicsCommandList",
            Kind::Fence => "ID3D12Fence",
            Kind::DescriptorHeap => "ID3D12DescriptorHeap",
            Kind::Resource | Kind::PlacedResource => "ID3D12Resource",
            Kind::Heap => "ID3D12Heap",
            Kind::PipelineState => "ID3D12PipelineState",
            Kind::RootSignature => "ID3D12RootSignature",
            Kind::QueryHeap => "ID3D12QueryHeap",
            Kind::CommandSignature => "ID3D12CommandSignature",
            Kind::PipelineLibrary => "ID3D12PipelineLibrary",
            Kind::StateObject => "ID3D12StateObject",
            Kind::Blob => "ID3DBlob",
        }
    }
}

#[repr(C)]
pub struct NullObj {
    vtbl: *const usize,
    refs: AtomicU32,
    kind: Kind,
    /// Fence value / descriptor heap base / resource GPU VA.
    value: AtomicU64,
    /// Resource backing memory or blob data.
    mem: Mutex<(usize, usize)>,
    /// Raw descriptor (resource desc 56 B, heap desc 16 B, …).
    desc: [u8; 64],
}

unsafe impl Sync for NullObj {}
unsafe impl Send for NullObj {}

static CREATED: AtomicU64 = AtomicU64::new(0);

fn vtable(kind: Kind) -> *const usize {
    static TABLES: OnceLock<Mutex<Vec<(Kind, usize)>>> = OnceLock::new();
    let t = TABLES.get_or_init(|| Mutex::new(Vec::new()));
    let mut g = t.lock().unwrap();
    if let Some(&(_, p)) = g.iter().find(|(k, _)| *k == kind) {
        return p as *const usize;
    }
    let mut v: Vec<usize> = (0..VTBL_SLOTS)
        .map(|i| crate::stubs::make_stub(DLL, &format!("{}::#{i}", kind.iface()), 0))
        .collect();
    v[0] = QueryInterface as usize;
    v[1] = AddRef as usize;
    v[2] = Release as usize;
    if kind == Kind::Blob {
        v[3] = Blob_GetBufferPointer as usize;
        v[4] = Blob_GetBufferSize as usize;
    } else {
        // ID3D12Object
        v[3] = Object_GetPrivateData as usize;
        v[4] = Object_Ok3 as usize; // SetPrivateData
        v[5] = Object_Ok2 as usize; // SetPrivateDataInterface
        v[6] = Object_Ok1 as usize; // SetName
        if kind != Kind::Device {
            v[7] = DeviceChild_GetDevice as usize;
        }
    }
    match kind {
        Kind::Device | Kind::VulkanDevice => {
            v[7] = Device_GetNodeCount as usize;
            v[8] = Device_CreateCommandQueue as usize;
            v[9] = Device_CreateCommandAllocator as usize;
            v[10] = Device_CreateGraphicsPipelineState as usize;
            v[11] = Device_CreateComputePipelineState as usize;
            v[12] = Device_CreateCommandList as usize;
            v[13] = Device_CheckFeatureSupport as usize;
            v[14] = Device_CreateDescriptorHeap as usize;
            v[15] = Device_GetDescriptorHandleIncrementSize as usize;
            v[16] = Device_CreateRootSignature as usize;
            v[25] = Device_GetResourceAllocationInfo as usize;
            v[26] = Device_GetCustomHeapProperties as usize;
            v[27] = Device_CreateCommittedResource as usize;
            v[28] = Device_CreateHeap as usize;
            v[29] = Device_CreatePlacedResource as usize;
            v[30] = Device_CreateReservedResource as usize;
            v[31] = Ret_E_NOTIMPL as usize; // CreateSharedHandle
            v[34] = Ret_S_OK as usize; // MakeResident
            v[35] = Ret_S_OK as usize; // Evict
            v[36] = Device_CreateFence as usize;
            v[37] = Ret_S_OK as usize; // GetDeviceRemovedReason
            v[38] = Device_GetCopyableFootprints as usize;
            v[39] = Device_CreateQueryHeap as usize;
            v[40] = Ret_S_OK as usize; // SetStablePowerState
            v[41] = Device_CreateCommandSignature as usize;
            v[43] = Device_GetAdapterLuid as usize;
            v[44] = Device_CreatePlacedResourceX as usize; // console placed title memory
            v[45] = Device_SetEventOnMultipleFenceCompletion as usize;
            v[46] = Ret_S_OK as usize; // SetResidencyPriority
            v[47] = Ret_S_OK as usize; // console extension #47 (unmodelled, S_OK)
            v[48] = Device_CreateCommandQueueX as usize; // console queue (DMA)
            v[51] = Device_CreateCommandList1 as usize;
            v[52] = Ret_S_OK as usize; // console SetDebugFlagsX
            v[53] = Device_CreateCommittedResource1 as usize;
            v[54] = Device_CreateHeap1 as usize;
            v[55] = Device_CreateDerivedPipelineStateX as usize; // console derived PSO
            v[56] = Device_SerializePipelineStateX as usize; // E_ACCESSDENIED like the console
            v[60] = Device_CreateGraphicsPipelineStateX as usize; // console PSO
            v[62] = Device_CreatePipelineState4 as usize; // CreateStateObject (Device5)
            v[73] = Device_CreateCommandListX as usize; // console list (DMA)
            v[74] = Device_RegisterPagePoolX as usize;
            v[75] = Device_UnregisterPagePoolX as usize;
        }
        Kind::CommandQueue => {
            v[10] = Ret_Void as usize; // ExecuteCommandLists
            v[14] = Queue_Signal as usize;
            v[15] = Ret_S_OK as usize; // Wait
            v[16] = Queue_GetTimestampFrequency as usize;
            v[17] = Queue_GetClockCalibration as usize;
            v[18] = Hidden_ZeroDesc16 as usize; // GetDesc
        }
        Kind::CommandAllocator => v[8] = Ret_S_OK as usize, // Reset
        Kind::CommandList | Kind::VulkanList => {
            v[8] = Ret_Zero as usize; // GetType → DIRECT
            v[9] = Ret_S_OK as usize; // Close
            v[10] = Ret_S_OK as usize; // Reset
            v[11] = Dma_CopyBufferRegion as usize; // console DMA CopyBufferRegion
        }
        Kind::Fence => {
            v[8] = Fence_GetCompletedValue as usize;
            v[9] = Fence_SetEventOnCompletion as usize;
            v[10] = Fence_Signal as usize;
        }
        Kind::DescriptorHeap => {
            v[8] = Heap_GetDesc16 as usize;
            v[9] = Heap_GetHandleStart as usize; // CPU
            v[10] = Heap_GetHandleStart as usize; // GPU
        }
        Kind::Resource | Kind::PlacedResource => {
            v[8] = Resource_Map as usize;
            v[9] = Ret_Void as usize; // Unmap
            v[10] = Resource_GetDesc as usize;
            v[11] = Resource_GetGPUVirtualAddress as usize;
            v[12] = Ret_S_OK as usize; // WriteToSubresource
            v[13] = Ret_S_OK as usize; // ReadFromSubresource
            v[14] = Resource_GetHeapProperties as usize;
        }
        Kind::Heap => v[8] = Hidden_ZeroDesc40 as usize,
        Kind::PipelineState => v[8] = Ret_E_NOTIMPL as usize, // GetCachedBlob
        _ => {}
    }
    // Step-17 Vulkan overrides: creation slots on the Vulkan device hand out
    // Vulkan lists (the null device keeps the plain ones — no default change),
    // and Vulkan lists implement the console memory methods for real.
    if kind == Kind::VulkanDevice {
        v[12] = Vulkan_CreateCommandList as usize;
        v[73] = Vulkan_CreateCommandListX as usize;
    }
    if kind == Kind::VulkanList {
        v[7] = VulkanList_GetDevice as usize;
        v[60] = VulList_CopyMemory as usize;
        v[61] = VulList_Fill32 as usize;
        v[69] = VulList_Write32Bottom as usize;
        v[70] = VulList_TimestampBottom as usize;
        v[71] = VulList_Wait32 as usize;
        v[72] = VulList_Wait64 as usize;
        v[89] = VulList_Write64Top as usize;
        v[93] = VulList_Kickoff as usize;
    }
    let leaked: &'static [usize] = Box::leak(v.into_boxed_slice());
    g.push((kind, leaked.as_ptr() as usize));
    leaked.as_ptr()
}

fn new_obj(kind: Kind) -> *mut NullObj {
    let n = CREATED.fetch_add(1, Ordering::Relaxed);
    if n < 64 || n % 10_000 == 0 {
        crate::xlog_debug!("D3D12.X null: new {} (#{n})", kind.iface());
    }
    Box::into_raw(Box::new(NullObj {
        vtbl: vtable(kind),
        refs: AtomicU32::new(1),
        kind,
        value: AtomicU64::new(0),
        mem: Mutex::new((0, 0)),
        desc: [0; 64],
    }))
}

unsafe fn out_obj(ppv: *mut *mut c_void, kind: Kind) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    *ppv = new_obj(kind) as *mut c_void;
    S_OK
}

fn device() -> *mut NullObj {
    static DEV: OnceLock<usize> = OnceLock::new();
    *DEV.get_or_init(|| new_obj(Kind::Device) as usize) as *mut NullObj
}

/// The process Vulkan device object (step 17): a singleton like [`device()`),
/// created only when the backend selection requests Vulkan and a real device
/// exists. The object's `value` holds the [`VulkanState`] pointer; `0` when no
/// Vulkan device is available (the caller falls back to [`device()`]).
fn vulkan_device(state: &'static VulkanState) -> *mut NullObj {
    static VDEV: OnceLock<usize> = OnceLock::new();
    *VDEV.get_or_init(|| {
        let o = new_obj(Kind::VulkanDevice);
        unsafe {
            (*o).value.store(state as *const VulkanState as u64, Ordering::Relaxed);
        }
        o as usize
    }) as *mut NullObj
}

// ---------------------------------------------------------------------------------------
// common slots
// ---------------------------------------------------------------------------------------

unsafe extern "win64" fn QueryInterface(this: *mut NullObj, _riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    (*this).refs.fetch_add(1, Ordering::AcqRel);
    *ppv = this as *mut c_void;
    S_OK
}
unsafe extern "win64" fn AddRef(this: *mut NullObj) -> u32 {
    (*this).refs.fetch_add(1, Ordering::AcqRel) + 1
}
unsafe extern "win64" fn Release(this: *mut NullObj) -> u32 {
    // Objects are intentionally leaked: a null device must never free memory a title
    // may still reference through a mis-guessed vtable slot.
    (*this).refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1)
}
unsafe extern "win64" fn Object_GetPrivateData(_t: *mut NullObj, _g: *const c_void, size: *mut u32, _d: *mut c_void) -> HRESULT {
    if !size.is_null() {
        *size = 0;
    }
    0x887A_0002u32 as i32 // DXGI_ERROR_NOT_FOUND
}
unsafe extern "win64" fn Object_Ok3(_t: *mut NullObj, _a: usize, _b: usize, _c: usize) -> HRESULT {
    S_OK
}
unsafe extern "win64" fn Object_Ok2(_t: *mut NullObj, _a: usize, _b: usize) -> HRESULT {
    S_OK
}
unsafe extern "win64" fn Object_Ok1(_t: *mut NullObj, _a: usize) -> HRESULT {
    S_OK
}
unsafe extern "win64" fn DeviceChild_GetDevice(_t: *mut NullObj, _riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    *ppv = device() as *mut c_void;
    S_OK
}
unsafe extern "win64" fn Ret_S_OK(_t: *mut NullObj) -> HRESULT {
    S_OK
}
unsafe extern "win64" fn Ret_E_NOTIMPL(_t: *mut NullObj) -> HRESULT {
    E_NOTIMPL
}
unsafe extern "win64" fn Ret_Zero(_t: *mut NullObj) -> usize {
    0
}
unsafe extern "win64" fn Ret_Void(_t: *mut NullObj) {}
/// Methods returning a 16-byte struct via hidden pointer (MSVC member-function ABI).
unsafe extern "win64" fn Hidden_ZeroDesc16(_t: *mut NullObj, ret: *mut u8) -> *mut u8 {
    if !ret.is_null() {
        std::ptr::write_bytes(ret, 0, 16);
    }
    ret
}
unsafe extern "win64" fn Hidden_ZeroDesc40(_t: *mut NullObj, ret: *mut u8) -> *mut u8 {
    if !ret.is_null() {
        std::ptr::write_bytes(ret, 0, 40);
    }
    ret
}

// ---------------------------------------------------------------------------------------
// device
// ---------------------------------------------------------------------------------------

unsafe extern "win64" fn Device_GetNodeCount(_t: *mut NullObj) -> u32 {
    1
}
unsafe extern "win64" fn Device_CreateCommandQueue(_t: *mut NullObj, _d: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::CommandQueue)
}
unsafe extern "win64" fn Device_CreateCommandAllocator(_t: *mut NullObj, _ty: u32, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::CommandAllocator)
}
unsafe extern "win64" fn Device_CreatePipelineState4(_t: *mut NullObj, _d: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::PipelineState)
}

/// D3D12_SHADER_BYTECODE { const void* pShaderBytecode; SIZE_T BytecodeLength; }
unsafe fn capture_bytecode(stage: &str, at: *const u8) {
    let p = *(at as *const usize);
    let n = *(at.add(8) as *const usize);
    crate::gpu::shader::capture(stage, p, n);
}

/// D3D12_GRAPHICS_PIPELINE_STATE_DESC: pRootSignature @0, VS @8, PS @24, DS @40, HS @56, GS @72.
pub unsafe fn capture_graphics_desc(desc: *const u8) {
    if !crate::gpu::shader::readable(desc as usize, 88) {
        return;
    }
    for (stage, off) in [("vs", 8), ("ps", 24), ("ds", 40), ("hs", 56), ("gs", 72)] {
        capture_bytecode(stage, desc.add(off));
    }
}

/// D3D12_COMPUTE_PIPELINE_STATE_DESC: pRootSignature @0, CS @8.
pub unsafe fn capture_compute_desc(desc: *const u8) {
    if crate::gpu::shader::readable(desc as usize, 24) {
        capture_bytecode("cs", desc.add(8));
    }
}

/// (payload size, payload alignment) of D3D12_PIPELINE_STATE_SUBOBJECT_TYPE values.
pub fn subobject_payload(ty: u32) -> Option<(usize, usize)> {
    Some(match ty {
        0 => (8, 8),                          // ROOT_SIGNATURE
        1..=6 | 24 | 25 => (16, 8),           // VS PS DS HS GS CS AS MS
        7 => (32, 8),                         // STREAM_OUTPUT
        8 => (328, 4),                        // BLEND
        9 | 13 | 14 | 16 | 18 | 20 => (4, 4), // SAMPLE_MASK, IB_STRIP_CUT, TOPOLOGY, DS_FORMAT, NODE_MASK, FLAGS
        10 | 27 => (44, 4),                   // RASTERIZER, RASTERIZER1
        11 => (52, 4),                        // DEPTH_STENCIL
        12 => (16, 8),                        // INPUT_LAYOUT
        15 => (36, 4),                        // RENDER_TARGET_FORMATS
        17 => (8, 4),                         // SAMPLE_DESC
        19 => (16, 8),                        // CACHED_PSO
        21 => (56, 4),                        // DEPTH_STENCIL1
        22 => (24, 8),                        // VIEW_INSTANCING
        26 => (60, 4),                        // DEPTH_STENCIL2
        28 => (40, 4),                        // RASTERIZER2
        _ => return None,
    })
}

/// D3D12_PIPELINE_STATE_STREAM_DESC { SIZE_T SizeInBytes; void* pPipelineStateSubobjectStream; }
pub unsafe fn capture_stream_desc(desc: *const u8) {
    if !crate::gpu::shader::readable(desc as usize, 16) {
        return;
    }
    let size = *(desc as *const usize);
    let base = *(desc.add(8) as *const usize);
    if !crate::gpu::shader::readable(base, size) {
        return;
    }
    let mut off = 0usize;
    while off + 8 <= size {
        let ty = *((base + off) as *const u32);
        let Some((psize, palign)) = subobject_payload(ty) else {
            crate::xlog_debug!("[SHADER] PSO stream: unknown subobject type {ty} – stopping the parse");
            break;
        };
        let poff = if palign == 8 { 8 } else { 4 };
        let stage = match ty {
            1 => Some("vs"),
            2 => Some("ps"),
            3 => Some("ds"),
            4 => Some("hs"),
            5 => Some("gs"),
            6 => Some("cs"),
            24 => Some("as"),
            25 => Some("ms"),
            _ => None,
        };
        if let Some(s) = stage {
            capture_bytecode(s, (base + off + poff) as *const u8);
        }
        off += (poff + psize + 7) & !7;
    }
}

unsafe extern "win64" fn Device_CreateGraphicsPipelineState(_t: *mut NullObj, d: *const u8, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    capture_graphics_desc(d);
    out_obj(ppv, Kind::PipelineState)
}
unsafe extern "win64" fn Device_CreateComputePipelineState(_t: *mut NullObj, d: *const u8, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    capture_compute_desc(d);
    out_obj(ppv, Kind::PipelineState)
}
unsafe extern "win64" fn Device_CreateCommandList(_t: *mut NullObj, _m: u32, _ty: u32, _a: *mut c_void, _p: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::CommandList)
}
unsafe extern "win64" fn Device_CreateCommandList1(_t: *mut NullObj, _m: u32, _ty: u32, _f: u32, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::CommandList)
}
unsafe extern "win64" fn Device_CheckFeatureSupport(_t: *mut NullObj, feature: u32, data: *mut c_void, size: u32) -> HRESULT {
    crate::xlog_debug!("D3D12.X null: CheckFeatureSupport({feature})");
    // D3D12_FEATURE_D3D12_OPTIONS5 = 27: RaytracingTier sits at +8
    if feature == 27 && !data.is_null() && size >= 12 {
        let tier = crate::state::config().raytracing_tier_override().unwrap_or(0);
        *((data as *mut u8).add(8) as *mut u32) = tier;
    }
    S_OK
}
unsafe extern "win64" fn Device_CreateDescriptorHeap(_t: *mut NullObj, desc: *const u8, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::DescriptorHeap);
    if !desc.is_null() {
        std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 16);
        let n = *(desc.add(4) as *const u32) as usize;
        // unique, never-dereferenced-by-us address range for the handles
        let mem = vec![0u8; (n.max(1) * 32).min(64 << 20)].leak();
        (*o).value.store(mem.as_ptr() as u64, Ordering::Relaxed);
    }
    *ppv = o as *mut c_void;
    S_OK
}
unsafe extern "win64" fn Device_GetDescriptorHandleIncrementSize(_t: *mut NullObj, _ty: u32) -> u32 {
    32
}
unsafe extern "win64" fn Device_CreateRootSignature(_t: *mut NullObj, _m: u32, b: *const c_void, l: usize, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    crate::gpu::shader::capture("rootsig", b as usize, l);
    out_obj(ppv, Kind::RootSignature)
}

/// D3D12_RESOURCE_DESC: Dimension u32 @0, Alignment u64 @8, Width u64 @16, Height u32 @24,
/// DepthOrArraySize u16 @28, MipLevels u16 @30, Format u32 @32 … (56 bytes).
unsafe fn approx_size(desc: *const u8) -> usize {
    if desc.is_null() {
        return 0x1000;
    }
    let dim = *(desc as *const u32);
    let width = *(desc.add(16) as *const u64) as usize;
    if dim == 1 {
        return width.max(1);
    }
    let height = (*(desc.add(24) as *const u32)).max(1) as usize;
    let depth = (*(desc.add(28) as *const u16)).max(1) as usize;
    (width.max(1) * height * depth * 4 * 4 / 3).min(256 << 20)
}

unsafe fn new_resource(desc: *const u8, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::Resource);
    if !desc.is_null() {
        std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 56);
    }
    let size = approx_size(desc);
    *(*o).mem.lock().unwrap() = (0, size);
    // unique fake GPU VA (64 KiB aligned bump allocator)
    static NEXT_VA: AtomicU64 = AtomicU64::new(0x1_0000_0000);
    let va = NEXT_VA.fetch_add(((size as u64) + 0xFFFF) & !0xFFFF, Ordering::Relaxed);
    (*o).value.store(va, Ordering::Relaxed);
    *ppv = o as *mut c_void;
    S_OK
}

unsafe extern "win64" fn Device_GetResourceAllocationInfo(_t: *mut NullObj, ret: *mut u64, _mask: u32, n: u32, descs: *const u8) -> *mut u64 {
    let mut total = 0usize;
    for i in 0..n as usize {
        total += (approx_size(descs.add(i * 56)) + 0xFFFF) & !0xFFFF;
    }
    if !ret.is_null() {
        *ret = total as u64;
        *ret.add(1) = 0x10000;
    }
    ret
}
unsafe extern "win64" fn Device_GetCustomHeapProperties(_t: *mut NullObj, ret: *mut u8, _m: u32, ty: u32) -> *mut u8 {
    if !ret.is_null() {
        std::ptr::write_bytes(ret, 0, 20);
        *(ret as *mut u32) = 4; // CUSTOM
        *(ret.add(4) as *mut u32) = if ty == 2 || ty == 3 { 3 } else { 1 }; // CPU page property
        *(ret.add(8) as *mut u32) = 1; // memory pool L0
    }
    ret
}
unsafe extern "win64" fn Device_CreateCommittedResource(_t: *mut NullObj, _hp: *const c_void, _hf: u32, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(desc, ppv)
}
unsafe extern "win64" fn Device_CreateCommittedResource1(_t: *mut NullObj, _hp: *const c_void, _hf: u32, desc: *const u8, _st: u32, _cv: *const c_void, _ps: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(desc, ppv)
}
unsafe extern "win64" fn Device_CreateHeap(_t: *mut NullObj, _d: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::Heap)
}
unsafe extern "win64" fn Device_CreateHeap1(_t: *mut NullObj, _d: *const c_void, _s: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::Heap)
}
unsafe extern "win64" fn Device_CreatePlacedResource(_t: *mut NullObj, _h: *mut c_void, _o: u64, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(desc, ppv)
}
unsafe extern "win64" fn Device_CreateReservedResource(_t: *mut NullObj, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(desc, ppv)
}
unsafe extern "win64" fn Device_CreateFence(_t: *mut NullObj, initial: u64, _f: u32, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::Fence);
    (*o).value.store(initial, Ordering::Relaxed);
    *ppv = o as *mut c_void;
    S_OK
}
unsafe extern "win64" fn Device_GetCopyableFootprints(
    _t: *mut NullObj,
    desc: *const u8,
    _first: u32,
    num: u32,
    base: u64,
    layouts: *mut u8,
    num_rows: *mut u32,
    row_sizes: *mut u64,
    total: *mut u64,
) {
    let size = approx_size(desc) as u64;
    let width = if desc.is_null() { 1 } else { (*(desc.add(16) as *const u64)).max(1) };
    let height = if desc.is_null() { 1 } else { (*(desc.add(24) as *const u32)).max(1) };
    let format = if desc.is_null() { 0 } else { *(desc.add(32) as *const u32) };
    let row = ((width * 4 + 255) & !255) as u32;
    for i in 0..num as usize {
        if !layouts.is_null() {
            let l = layouts.add(i * 32);
            *(l as *mut u64) = base;
            *(l.add(8) as *mut u32) = format;
            *(l.add(12) as *mut u32) = width as u32;
            *(l.add(16) as *mut u32) = height;
            *(l.add(20) as *mut u32) = 1;
            *(l.add(24) as *mut u32) = row;
        }
        if !num_rows.is_null() {
            *num_rows.add(i) = height;
        }
        if !row_sizes.is_null() {
            *row_sizes.add(i) = width * 4;
        }
    }
    if !total.is_null() {
        *total = size.max(row as u64 * height as u64);
    }
}
unsafe extern "win64" fn Device_CreateQueryHeap(_t: *mut NullObj, _d: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::QueryHeap)
}
unsafe extern "win64" fn Device_CreateCommandSignature(_t: *mut NullObj, _d: *const c_void, _rs: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    out_obj(ppv, Kind::CommandSignature)
}
unsafe extern "win64" fn Device_GetAdapterLuid(_t: *mut NullObj, ret: *mut u64) -> *mut u64 {
    if !ret.is_null() {
        *ret = 0x5843_4C00;
    }
    ret
}
unsafe extern "win64" fn Device_SetEventOnMultipleFenceCompletion(_t: *mut NullObj, _f: *const c_void, _v: *const u64, _n: u32, _flags: u32, ev: *mut c_void) -> HRESULT {
    if !ev.is_null() {
        crate::hle::kernelx_linux::SetEvent(ev as usize);
    }
    S_OK
}

// ---------------------------------------------------------------------------------------
// console device extensions (Phase C step 11s): the title's own memory, DMA queue/list,
// derived/console PSOs, page pools. Slots collide with PC Device1/2 methods the null
// device never served the title (no PipelineLibrary/stream use); the title is console.
// ---------------------------------------------------------------------------------------

/// `CreatePlacedResourceX(va, desc, state, clear, riid, out)`: the resource IS the
/// title's memory at `va` – record the desc and report `va` back, no host backing.
unsafe extern "win64" fn Device_CreatePlacedResourceX(
    _t: *mut NullObj,
    va: u64,
    desc: *const u8,
    _state: u32,
    _clear: *const c_void,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || va == 0 {
        return E_INVALIDARG;
    }
    let o = new_obj(Kind::PlacedResource);
    if !desc.is_null() {
        std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 56);
    }
    (*o).value.store(va, Ordering::Relaxed);
    *ppv = o as *mut c_void;
    S_OK
}

/// `CreateCommandQueueX(desc[4], riid, out)`: `desc[0]` is the queue type (0x10 DMA).
unsafe extern "win64" fn Device_CreateCommandQueueX(
    _t: *mut NullObj,
    _desc: *const u32,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    out_obj(ppv, Kind::CommandQueue)
}

/// `CreateCommandListX(desc[4], allocator, state, riid, out)`.
unsafe extern "win64" fn Device_CreateCommandListX(
    _t: *mut NullObj,
    _desc: *const u32,
    _alloc: *mut c_void,
    _state: *mut c_void,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    out_obj(ppv, Kind::CommandList)
}

/// `CreateGraphicsPipelineStateX(desc, flags, cache, riid, out)`: capture and accept.
unsafe extern "win64" fn Device_CreateGraphicsPipelineStateX(
    _t: *mut NullObj,
    d: *const u8,
    _flags: u32,
    _cache: *const c_void,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    capture_graphics_desc(d);
    out_obj(ppv, Kind::PipelineState)
}

/// `CreateDerivedPipelineStateX(base, count, patch, riid, out)`: `patch[0]` selects the
/// derivation (2 = blend state). Unknown derivations fail like the console.
unsafe extern "win64" fn Device_CreateDerivedPipelineStateX(
    _t: *mut NullObj,
    _base: *mut c_void,
    _count: u32,
    patch: *const u8,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if patch.is_null() {
        return E_INVALIDARG;
    }
    match *patch {
        2 => out_obj(ppv, Kind::PipelineState),
        _ => E_INVALIDARG,
    }
}

/// `SerializePipelineStateX(what)`: the console refuses with `E_ACCESSDENIED`.
unsafe extern "win64" fn Device_SerializePipelineStateX(
    _t: *mut NullObj,
    _what: *mut c_void,
) -> HRESULT {
    0x8007_0005u32 as i32 // E_ACCESSDENIED
}

static PAGE_POOL_NEXT: AtomicU32 = AtomicU32::new(1);
static PAGE_POOLS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// `RegisterPagePoolX(base, count, out)`: hand a monotonic pool id.
unsafe extern "win64" fn Device_RegisterPagePoolX(
    _t: *mut NullObj,
    _base: u64,
    _count: u32,
    out: *mut u32,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let id = PAGE_POOL_NEXT.fetch_add(1, Ordering::Relaxed);
    PAGE_POOLS.lock().unwrap().push(id);
    *out = id;
    S_OK
}

/// `UnregisterPagePoolX(pool)`: `S_OK` once, `E_FAIL` when the pool is unknown.
unsafe extern "win64" fn Device_UnregisterPagePoolX(_t: *mut NullObj, pool: u64) -> HRESULT {
    let mut g = PAGE_POOLS.lock().unwrap();
    match g.iter().position(|&p| p as u64 == pool) {
        Some(i) => {
            g.remove(i);
            S_OK
        }
        None => E_FAIL,
    }
}

/// Ensure a resource has host backing and hand its pointer plus size.
unsafe fn resource_backing(o: *mut NullObj) -> (*mut u8, usize) {
    let mut g = (*o).mem.lock().unwrap();
    if g.0 == 0 {
        let n = g.1.max(1);
        let p = libc::malloc(n) as usize;
        if p != 0 {
            std::ptr::write_bytes(p as *mut u8, 0, n);
        }
        g.0 = p;
    }
    (g.0 as *mut u8, g.1)
}

/// Console DMA `CopyBufferRegion(dst, dst_off, src, src_off, bytes)`: an immediate host
/// `memcpy`. Placed resources read from the title's memory (their VA); committed ones
/// from their Map backing. Enough for the title's DMA round-trip check; the queue's
/// `ExecuteCommandLists` stays a no-op because the copy already happened.
unsafe extern "win64" fn Dma_CopyBufferRegion(
    _t: *mut NullObj,
    dst: *mut NullObj,
    dst_off: u64,
    src: *mut NullObj,
    src_off: u64,
    bytes: u64,
) {
    if dst.is_null() || src.is_null() || bytes == 0 {
        return;
    }
    let n = bytes as usize;
    let (dp, dn) = resource_backing(dst);
    if dp.is_null() || dst_off as usize + n > dn {
        return;
    }
    if (*src).kind == Kind::PlacedResource {
        let va = (*src).value.load(Ordering::Relaxed);
        if va == 0 {
            return;
        }
        std::ptr::copy_nonoverlapping((va + src_off) as *const u8, dp.add(dst_off as usize), n);
    } else {
        let (sp, sn) = resource_backing(src);
        if sp.is_null() || src_off as usize + n > sn {
            return;
        }
        std::ptr::copy_nonoverlapping(sp.add(src_off as usize), dp.add(dst_off as usize), n);
    }
}

// ---------------------------------------------------------------------------------------
// Vulkan lists (step 17): console command-list memory methods on title memory.
// The title's graphics memory is host memory (XMemAlloc), so writes, fills,
// copies and the same-list write-then-wait apply immediately and synchronously;
// waits poll the cell until the comparison passes (another thread or the GPU
// would write it on the console) with a 5 s fail-closed deadline like the
// fence waits, and KickoffX has nothing left to flush. Null addresses return
// silently instead of faulting.
// ---------------------------------------------------------------------------------------

/// Create a Vulkan command list on `creator` (slot 12 / 73): like the null one,
/// but the object's `value` remembers the creator device for `GetDevice`.
unsafe fn new_vulkan_list(creator: *mut NullObj, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::VulkanList);
    (*o).value.store(creator as u64, Ordering::Relaxed);
    *ppv = o as *mut c_void;
    S_OK
}

unsafe extern "win64" fn Vulkan_CreateCommandList(
    this: *mut NullObj,
    _mask: u32,
    _ty: u32,
    _alloc: *mut c_void,
    _pso: *mut c_void,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    new_vulkan_list(this, ppv)
}

unsafe extern "win64" fn Vulkan_CreateCommandListX(
    this: *mut NullObj,
    _desc: *const u32,
    _alloc: *mut c_void,
    _state: *mut c_void,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    new_vulkan_list(this, ppv)
}

unsafe extern "win64" fn VulkanList_GetDevice(this: *mut NullObj, _riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    // the creator device (stored at creation); the null device when detached
    let d = (*this).value.load(Ordering::Relaxed) as *mut c_void;
    *ppv = if d.is_null() { device() as *mut c_void } else { d };
    S_OK
}

/// `Write32BitValueBottomOfPipeX(va, value, _, _)`: an immediate host write.
unsafe extern "win64" fn VulList_Write32Bottom(_t: *mut NullObj, va: u64, val: u32, _a: u32, _b: u32) {
    if va == 0 {
        return;
    }
    *(va as *mut u32) = val;
}

/// `Write64BitValueTopOfPipeX(va, value, _)`: an immediate host write.
unsafe extern "win64" fn VulList_Write64Top(_t: *mut NullObj, va: u64, val: u64, _a: u32) {
    if va == 0 {
        return;
    }
    *(va as *mut u64) = val;
}

/// `Write64BitValueBottomOfPipeX(va, _, _, timestamp)`: the title passes it a
/// zero value and reads back a GPU timestamp — the monotonic clock in ns.
unsafe extern "win64" fn VulList_TimestampBottom(_t: *mut NullObj, va: u64, _a: u64, _b: u32, _c: u32) {
    if va == 0 {
        return;
    }
    let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
    let ns = if libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()) == 0 {
        let ts = ts.assume_init();
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    } else {
        1
    };
    *(va as *mut u64) = ns.max(1);
}

/// `FillMemoryWith32BitValueX(va, bytes, value)`: `bytes` (a multiple of 4 in
/// every title call) of the u32 pattern, guard zones untouched.
unsafe extern "win64" fn VulList_Fill32(_t: *mut NullObj, va: u64, bytes: u64, val: u32) {
    if va == 0 || bytes == 0 {
        return;
    }
    let words = (bytes as usize) / 4;
    for i in 0..words {
        *((va as *mut u32).add(i)) = val;
    }
}

/// `CopyMemoryToMemoryX(dst, src, bytes, _)`: an immediate host `memcpy`.
unsafe extern "win64" fn VulList_CopyMemory(_t: *mut NullObj, dst: u64, src: u64, bytes: u64, _f: u32) {
    if dst == 0 || src == 0 || bytes == 0 {
        return;
    }
    std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes as usize);
}

/// `D3D12_COMPARISON_FUNC` codes the title's waits use (3 = EQUAL,
/// 7 = GREATER_EQUAL); anything else falls back to equality.
fn wait_passes(func: u32, cur: u64, val: u64) -> bool {
    match func {
        1 => false, // NEVER
        2 => cur < val,
        3 => cur == val,
        4 => cur <= val,
        5 => cur > val,
        6 => cur != val,
        7 => cur >= val,
        8 => true, // ALWAYS
        _ => cur == val,
    }
}

/// Poll `*va` (volatile: written concurrently by another thread) until the
/// masked comparison passes, or the deadline expires — then return anyway so
/// a stuck cell can never hang the title past a GPU-timeout-style wait.
unsafe fn poll_until(va: u64, func: u32, val: u64, mask: u64, bits: u32) {
    if va == 0 || func == 8 {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let cur = if bits == 32 {
            std::ptr::read_volatile(va as *const u32) as u64
        } else {
            std::ptr::read_volatile(va as *const u64)
        };
        if wait_passes(func, cur & mask, val & mask) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            crate::xlog_debug!("D3D12.X Vulkan list: wait at {va:#x} timed out (func {func})");
            return;
        }
        std::thread::yield_now();
    }
}

/// `Wait32BitValueX(va, func, value, mask, _)`.
unsafe extern "win64" fn VulList_Wait32(_t: *mut NullObj, va: u64, func: u32, val: u32, mask: u32, _tmo: u32) {
    poll_until(va, func, val as u64, mask as u64, 32);
}

/// `Wait64BitValueX(va, func, value, mask, _)`.
unsafe extern "win64" fn VulList_Wait64(_t: *mut NullObj, va: u64, func: u32, val: u64, mask: u64, _tmo: u32) {
    poll_until(va, func, val, mask, 64);
}

/// `KickoffX()`: flushes recorded work to the GPU — every method above already
/// applied synchronously, so there is nothing queued to flush.
unsafe extern "win64" fn VulList_Kickoff(_t: *mut NullObj) {}

// ---------------------------------------------------------------------------------------
// queue / fence / heap / resource / blob
// ---------------------------------------------------------------------------------------

unsafe extern "win64" fn Queue_Signal(_t: *mut NullObj, fence: *mut NullObj, value: u64) -> HRESULT {
    if !fence.is_null() && (*fence).kind == Kind::Fence {
        (*fence).value.fetch_max(value, Ordering::AcqRel);
    }
    S_OK
}
unsafe extern "win64" fn Queue_GetTimestampFrequency(_t: *mut NullObj, f: *mut u64) -> HRESULT {
    if f.is_null() {
        return E_INVALIDARG;
    }
    *f = 10_000_000;
    S_OK
}
unsafe extern "win64" fn Queue_GetClockCalibration(_t: *mut NullObj, gpu: *mut u64, cpu: *mut u64) -> HRESULT {
    let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
    let t = if libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()) == 0 {
        let ts = ts.assume_init();
        (ts.tv_sec as u64) * 10_000_000 + (ts.tv_nsec as u64) / 100
    } else {
        0
    };
    if !gpu.is_null() {
        *gpu = t;
    }
    if !cpu.is_null() {
        *cpu = t;
    }
    S_OK
}
unsafe extern "win64" fn Fence_GetCompletedValue(this: *mut NullObj) -> u64 {
    (*this).value.load(Ordering::Acquire)
}
unsafe extern "win64" fn Fence_SetEventOnCompletion(this: *mut NullObj, value: u64, ev: *mut c_void) -> HRESULT {
    // the null GPU finishes everything instantly
    (*this).value.fetch_max(value, Ordering::AcqRel);
    if !ev.is_null() {
        crate::hle::kernelx_linux::SetEvent(ev as usize);
    }
    S_OK
}
unsafe extern "win64" fn Fence_Signal(this: *mut NullObj, value: u64) -> HRESULT {
    (*this).value.store(value, Ordering::Release);
    S_OK
}
unsafe extern "win64" fn Heap_GetDesc16(this: *mut NullObj, ret: *mut u8) -> *mut u8 {
    if !ret.is_null() {
        std::ptr::copy_nonoverlapping((*this).desc.as_ptr(), ret, 16);
    }
    ret
}
/// #9 / #10 GetCPU / GetGPUDescriptorHandleForHeapStart: console code takes the handle in rax
/// and passes only `this` (as `real::heap_cpu_start_x`); rdx holds whatever the caller left
/// there, so it must not be written through as the PC's hidden return pointer.
unsafe extern "win64" fn Heap_GetHandleStart(this: *mut NullObj) -> u64 {
    (*this).value.load(Ordering::Relaxed)
}
unsafe extern "win64" fn Resource_Map(this: *mut NullObj, _sub: u32, _range: *const c_void, data: *mut *mut c_void) -> HRESULT {
    let mut g = (*this).mem.lock().unwrap();
    if g.0 == 0 {
        let n = g.1.max(1);
        let p = libc::malloc(n) as usize;
        if p != 0 {
            std::ptr::write_bytes(p as *mut u8, 0, n);
        }
        g.0 = p;
    }
    if !data.is_null() {
        *data = g.0 as *mut c_void;
    }
    if g.0 == 0 {
        crate::util::E_OUTOFMEMORY
    } else {
        S_OK
    }
}
unsafe extern "win64" fn Resource_GetDesc(this: *mut NullObj, ret: *mut u8) -> *mut u8 {
    if !ret.is_null() {
        std::ptr::copy_nonoverlapping((*this).desc.as_ptr(), ret, 56);
    }
    ret
}
unsafe extern "win64" fn Resource_GetGPUVirtualAddress(this: *mut NullObj) -> u64 {
    (*this).value.load(Ordering::Relaxed)
}
unsafe extern "win64" fn Resource_GetHeapProperties(_t: *mut NullObj, props: *mut u8, flags: *mut u32) -> HRESULT {
    if !props.is_null() {
        std::ptr::write_bytes(props, 0, 20);
        *(props as *mut u32) = 1; // DEFAULT
    }
    if !flags.is_null() {
        *flags = 0;
    }
    S_OK
}
unsafe extern "win64" fn Blob_GetBufferPointer(this: *mut NullObj) -> *mut c_void {
    (*this).mem.lock().unwrap().0 as *mut c_void
}
unsafe extern "win64" fn Blob_GetBufferSize(this: *mut NullObj) -> usize {
    (*this).mem.lock().unwrap().1
}

pub(crate) unsafe fn new_blob(size: usize, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let o = new_obj(Kind::Blob);
    let buf = vec![0u8; size.max(1)].leak();
    *(*o).mem.lock().unwrap() = (buf.as_mut_ptr() as usize, size);
    *out = o as *mut c_void;
    S_OK
}

/// A blob pre-filled with `len` bytes from `src` (the headless `D3DCompile` echo).
pub(crate) unsafe fn new_blob_from(src: *const u8, len: usize, out: *mut *mut c_void) -> HRESULT {
    let hr = new_blob(len, out);
    if hr != S_OK {
        return hr;
    }
    let back = (*(*out as *mut NullObj)).mem.lock().unwrap().0 as *mut u8;
    if back.is_null() {
        return E_FAIL;
    }
    std::ptr::copy_nonoverlapping(src, back, len);
    S_OK
}

// ---------------------------------------------------------------------------------------
// exports
// ---------------------------------------------------------------------------------------

static ANNOUNCED: AtomicU32 = AtomicU32::new(0);

unsafe fn create_device(ppv: *mut *mut c_void) -> HRESULT {
    // Step-17: query the Vulkan backend selector (cached: one probe per
    // process, not one per device creation); the Vulkan request path hands out
    // the real-device object below, the default stays the null device.
    create_device_for(select_cached(), acquire(), ppv)
}

/// Device creation for an explicit selection + device state (pure wiring, for
/// tests): `Null` always hands out the null device; `Vulkan` hands out the
/// Vulkan device object when `state` is `Some`, and falls back to the null
/// device with a logged reason when it is `None`. Never a panic.
pub(crate) unsafe fn create_device_for(
    sel: &Selection,
    state: Option<&'static VulkanState>,
    ppv: *mut *mut c_void,
) -> HRESULT {
    crate::xlog_debug!("D3D12.X backend: {:?} ({})", sel.backend, sel.reason);
    if sel.backend == Backend::Vulkan {
        if let Some(st) = state {
            if ANNOUNCED.fetch_add(1, Ordering::Relaxed) == 0 {
                crate::xlog_warn!(
                    "D3D12.X: Vulkan device '{}' ({:?}) – console device calls route to the Vulkan backend",
                    st.name,
                    st.lane,
                );
            }
            if ppv.is_null() {
                return S_FALSE;
            }
            let d = vulkan_device(st);
            (*d).refs.fetch_add(1, Ordering::AcqRel);
            *ppv = d as *mut c_void;
            return S_OK;
        }
        crate::xlog_warn!("D3D12.X: Vulkan requested but no device – falling back to the null device");
    }
    if ANNOUNCED.fetch_add(1, Ordering::Relaxed) == 0 {
        crate::xlog_warn!("D3D12.X: creating a FALLBACK DEVICE (null device) – graphics are not rendered in milestone M1");
    }
    if ppv.is_null() {
        return S_FALSE;
    }
    let d = device();
    (*d).refs.fetch_add(1, Ordering::AcqRel);
    *ppv = d as *mut c_void;
    S_OK
}

pub unsafe extern "win64" fn D3D12CreateDevice(_adapter: *mut c_void, _fl: u32, _riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    create_device(ppv)
}
pub unsafe extern "win64" fn D3D12XboxCreateDevice(_adapter: *mut c_void, _params: *const c_void, _riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    create_device(ppv)
}
pub unsafe extern "win64" fn D3D12GetDebugInterface(_riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    E_NOINTERFACE
}
pub unsafe extern "win64" fn D3D12SerializeRootSignature(_d: *const c_void, _v: u32, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT {
    if !err.is_null() {
        *err = std::ptr::null_mut();
    }
    new_blob(16, blob)
}
pub unsafe extern "win64" fn D3D12SerializeVersionedRootSignature(_d: *const c_void, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT {
    if !err.is_null() {
        *err = std::ptr::null_mut();
    }
    new_blob(16, blob)
}
pub unsafe extern "win64" fn D3D12CreateRootSignatureDeserializer(_d: *const c_void, _l: usize, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    E_NOTIMPL
}
pub unsafe extern "win64" fn D3D12CreateVersionedRootSignatureDeserializer(_d: *const c_void, _l: usize, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    E_NOTIMPL
}
pub unsafe extern "win64" fn D3D12EnableExperimentalFeatures(_n: u32, _i: *const c_void, _c: *mut c_void, _s: *mut u32) -> HRESULT {
    E_NOINTERFACE
}
pub unsafe extern "win64" fn D3D12GetInterface(_clsid: *const c_void, _riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    E_NOINTERFACE
}
pub unsafe extern "win64" fn D3D10CreateBlob(size: usize, out: *mut *mut c_void) -> HRESULT {
    new_blob(size, out)
}
pub unsafe extern "win64" fn D3D12XboxSetProcessDebugFlags(_flags: u32) -> HRESULT {
    S_OK
}


pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("D3D12CreateDevice", D3D12CreateDevice as usize),
        ("D3D12XboxCreateDevice", D3D12XboxCreateDevice as usize),
        ("D3D12GetDebugInterface", D3D12GetDebugInterface as usize),
        ("D3D12SerializeRootSignature", D3D12SerializeRootSignature as usize),
        (
            "D3D12SerializeVersionedRootSignature",
            D3D12SerializeVersionedRootSignature as usize,
        ),
        (
            "D3D12CreateRootSignatureDeserializer",
            D3D12CreateRootSignatureDeserializer as usize,
        ),
        (
            "D3D12CreateVersionedRootSignatureDeserializer",
            D3D12CreateVersionedRootSignatureDeserializer as usize,
        ),
        (
            "D3D12EnableExperimentalFeatures",
            D3D12EnableExperimentalFeatures as usize,
        ),
        ("D3D12GetInterface", D3D12GetInterface as usize),
        ("D3D10CreateBlob", D3D10CreateBlob as usize),
        (
            "D3D12XboxSetProcessDebugFlags",
            D3D12XboxSetProcessDebugFlags as usize,
        ),
        (
            "D3D12ConfigureVirtualMemoryX",
            D3D12ConfigureVirtualMemoryX as usize,
        ),
    ]
}

pub unsafe extern "win64" fn D3D12ConfigureVirtualMemoryX(
    _a: *mut c_void,
    _b: *mut c_void,
) -> HRESULT {
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    type Fn4 = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;

    unsafe fn slot(obj: *mut c_void, i: usize) -> usize {
        *(*(obj as *const *const usize)).add(i)
    }

    #[test]
    fn device_queue_fence_resource_roundtrip() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create_queue: Fn4 = std::mem::transmute(slot(dev, 8));
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(dev, std::ptr::null(), std::ptr::null(), &mut q), S_OK);
            // fence: CreateFence(initial=5)
            let create_fence: unsafe extern "win64" fn(*mut c_void, u64, u32, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 36));
            let mut f = std::ptr::null_mut();
            assert_eq!(create_fence(dev, 5, 0, std::ptr::null(), &mut f), S_OK);
            let signal: unsafe extern "win64" fn(*mut c_void, *mut c_void, u64) -> HRESULT = std::mem::transmute(slot(q, 14));
            assert_eq!(signal(q, f, 9), S_OK);
            let completed: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(f, 8));
            assert_eq!(completed(f), 9);
            // buffer resource: Map gives writable memory
            let mut desc = [0u8; 56];
            desc[0] = 1; // BUFFER
            desc[16..24].copy_from_slice(&4096u64.to_le_bytes());
            let create_res: unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 27));
            let mut r = std::ptr::null_mut();
            assert_eq!(create_res(dev, std::ptr::null(), 0, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut r), S_OK);
            let map: unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT = std::mem::transmute(slot(r, 8));
            let mut p = std::ptr::null_mut();
            assert_eq!(map(r, 0, std::ptr::null(), &mut p), S_OK);
            std::ptr::write_bytes(p as *mut u8, 0xAB, 4096);
            let gpu: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(r, 11));
            assert_ne!(gpu(r), 0);
            // unknown slot → logging stub returning 0
            let unknown: unsafe extern "win64" fn(*mut c_void) -> usize = std::mem::transmute(slot(dev, 200));
            assert_eq!(unknown(dev), 0);
            assert!(crate::stubs::all_stubs().iter().any(|s| s.name == "ID3D12Device::#200" && s.calls.load(Ordering::Relaxed) > 0));
        }
    }

    /// Console code calls descriptor heap #9 / #10 with only `this` and takes the start in rax;
    /// rdx is left over from the caller and must not be written (the test title's rdx pointed
    /// into its read-only .rdata: an access violation under Wine on the null device).
    #[test]
    fn heap_start_handles_come_back_in_rax() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let desc = [0u32, 16, 1, 0]; // CBV_SRV_UAV, 16 slots, shader visible
            let create_heap: Fn4 = std::mem::transmute(slot(dev, 14));
            let mut heaps = [std::ptr::null_mut(); 2];
            for h in &mut heaps {
                assert_eq!(create_heap(dev, desc.as_ptr() as *const c_void, std::ptr::null(), h), S_OK);
            }
            // the start is the heap's own handle range (CreateDescriptorHeap): not 0, one per heap
            let base = heaps.map(|h| (*(h as *mut NullObj)).value.load(Ordering::Relaxed));
            assert_ne!(base[0], 0);
            assert_ne!(base[0], base[1]);
            for (h, base) in heaps.into_iter().zip(base) {
                for i in [9, 10] {
                    #[cfg(all(target_arch = "x86_64", windows))]
                    {
                        let mut sentinel = 0x5A5A_5A5A_5A5A_5A5Au64;
                        let p = call_with_rdx(slot(h, i), h, &mut sentinel);
                        assert_eq!(sentinel, 0x5A5A_5A5A_5A5A_5A5A, "slot {i} wrote through rdx");
                        assert_eq!(p, base, "slot {i} with rdx set");
                    }
                    let start: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(h, i));
                    assert_eq!(start(h), base, "slot {i}");
                }
            }
        }
    }

    #[test]
    fn console_placed_dma_and_pools_roundtrip() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            // title memory the placed resource aliases
            let mut title = vec![0u32; 64];
            for (i, v) in title.iter_mut().enumerate() {
                *v = 0xC0DE_0000 | i as u32;
            }
            let mut desc = [0u8; 56];
            desc[0] = 1; // BUFFER
            desc[16..24].copy_from_slice(&256u64.to_le_bytes());
            type PlacedX = unsafe extern "win64" fn(*mut c_void, u64, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let placed: PlacedX = std::mem::transmute(slot(dev, 44));
            let mut raw = std::ptr::null_mut();
            assert_eq!(placed(dev, title.as_ptr() as u64, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut raw), S_OK);
            assert!(!raw.is_null());
            let va: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(raw, 11));
            assert_eq!(va(raw), title.as_ptr() as u64);
            // readback + DMA copy through the DMA queue/list slots
            type QueueX = unsafe extern "win64" fn(*mut c_void, *const u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let queue_x: QueueX = std::mem::transmute(slot(dev, 48));
            let mut q = std::ptr::null_mut();
            assert_eq!(queue_x(dev, [0x10u32, 0, 0, 0].as_ptr(), std::ptr::null(), &mut q), S_OK);
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            let create_rb: unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 27));
            let mut rb = std::ptr::null_mut();
            assert_eq!(create_rb(dev, std::ptr::null(), 0, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            type Copy = unsafe extern "win64" fn(*mut c_void, *mut c_void, u64, *mut c_void, u64, u64);
            let copy: Copy = std::mem::transmute(slot(l, 11));
            copy(l, rb, 0, raw, 0, 256);
            let map: unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT = std::mem::transmute(slot(rb, 8));
            let mut p = std::ptr::null_mut();
            assert_eq!(map(rb, 0, std::ptr::null(), &mut p), S_OK);
            assert_eq!(std::slice::from_raw_parts(p as *const u32, 64), &title[..]);
            // console PSO + derived PSO blend accept, unknown derivation fails
            type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let pso_x: PsoX = std::mem::transmute(slot(dev, 60));
            let mut pso = std::ptr::null_mut();
            assert_eq!(pso_x(dev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
            assert!(!pso.is_null());
            type DerivedX = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, *const u8, *const c_void, *mut *mut c_void) -> HRESULT;
            let derive: DerivedX = std::mem::transmute(slot(dev, 55));
            let mut d2 = std::ptr::null_mut();
            assert_eq!(derive(dev, pso, 1, [2u8].as_ptr(), std::ptr::null(), &mut d2), S_OK);
            assert!(!d2.is_null());
            assert!(derive(dev, pso, 1, [4u8].as_ptr(), std::ptr::null(), &mut d2) < 0);
            // serialize refuses, extension #47 stays S_OK, pools register once
            let ser: unsafe extern "win64" fn(*mut c_void, *mut c_void) -> HRESULT = std::mem::transmute(slot(dev, 56));
            assert_eq!(ser(dev, std::ptr::null_mut()), 0x8007_0005u32 as i32);
            let ext: unsafe extern "win64" fn(*mut c_void, usize, usize, usize) -> HRESULT = std::mem::transmute(slot(dev, 47));
            assert_eq!(ext(dev, 0, 0, 0), S_OK);
            let reg: unsafe extern "win64" fn(*mut c_void, u64, u32, *mut u32) -> HRESULT = std::mem::transmute(slot(dev, 74));
            let unreg: unsafe extern "win64" fn(*mut c_void, u64) -> HRESULT = std::mem::transmute(slot(dev, 75));
            let mut pool = u32::MAX;
            assert_eq!(reg(dev, 0x2_0000_0000, 16, &mut pool), S_OK);
            assert_ne!(pool, u32::MAX);
            assert_eq!(unreg(dev, pool as u64), S_OK);
            assert!(unreg(dev, pool as u64) < 0);
        }
    }

    #[test]
    fn vulkan_routing_hands_out_the_matching_device() {
        // Step-17 device slice: the selection decides which singleton the
        // title gets; a Vulkan request without a device falls back to null.
        // Pure wiring except the `Some` arm, which skips without hardware.
        unsafe {
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let vulkan_sel = Selection {
                backend: Backend::Vulkan,
                lane: None,
                reason: "test".into(),
            };
            // a null request never touches Vulkan state (present or not)
            for state in [None, acquire()] {
                let mut p = std::ptr::null_mut();
                assert_eq!(create_device_for(&null_sel, state, &mut p), S_OK);
                assert_eq!((*(p as *mut NullObj)).kind, Kind::Device);
            }
            // a Vulkan request without a device falls back to the null device
            let mut p = std::ptr::null_mut();
            assert_eq!(create_device_for(&vulkan_sel, None, &mut p), S_OK);
            assert_eq!((*(p as *mut NullObj)).kind, Kind::Device);
            // a Vulkan request with a device hands out the Vulkan object
            match acquire() {
                Some(st) => {
                    let mut p = std::ptr::null_mut();
                    assert_eq!(create_device_for(&vulkan_sel, Some(st), &mut p), S_OK);
                    assert_eq!((*(p as *mut NullObj)).kind, Kind::VulkanDevice);
                    assert_eq!(
                        (*(p as *mut NullObj)).value.load(Ordering::Relaxed),
                        st as *const _ as u64
                    );
                }
                None => crate::gpu::recomp::vulkan_runner::skip_test("no Vulkan"),
            }
            // a null out-pointer stays S_FALSE on every path
            assert_eq!(create_device_for(&null_sel, acquire(), std::ptr::null_mut()), S_FALSE);
            assert_eq!(create_device_for(&vulkan_sel, None, std::ptr::null_mut()), S_FALSE);
        }
    }

    #[test]
    fn vulkan_list_memory_methods_act_on_host_memory() {
        // Step-17 list slice, pure logic (no device needed): writes, fill,
        // copy and timestamp land in host memory; waits return on a satisfied
        // cell and on a cross-thread write like the title's; null addresses
        // never fault. Mirrors the title's `console_list_methods` asserts.
        unsafe {
            let this = std::ptr::null_mut();
            let mut mem = vec![0u8; 0x1000];
            let base = mem.as_mut_ptr() as u64;
            let r32 = |o: usize| (mem.as_ptr().add(o) as *const u32).read();
            let r64 = |o: usize| (mem.as_ptr().add(o) as *const u64).read();
            VulList_Write32Bottom(this, base, 0x1234_5678, 0, 0);
            assert_eq!(r32(0), 0x1234_5678);
            VulList_Write64Top(this, base + 8, 0x1122_3344_5566_7788, 0);
            assert_eq!(r64(8), 0x1122_3344_5566_7788);
            VulList_TimestampBottom(this, base + 0x10, 0, 0, 1);
            assert_ne!(r64(0x10), 0);
            VulList_Fill32(this, base + 0x100, 0x100, 0xABCD_EF01);
            assert!((0..64).all(|i| r32(0x100 + 4 * i) == 0xABCD_EF01));
            assert_eq!(r32(0x200), 0);
            for i in 0..64 {
                *mem.as_mut_ptr().add(i) = i as u8 ^ 0x5A;
            }
            VulList_CopyMemory(this, base + 0x200, base, 64, 0);
            assert!((0..64).all(|i| *mem.as_ptr().add(0x200 + i) == i as u8 ^ 0x5A));
            assert_eq!(*mem.as_ptr().add(0x240), 0);
            // pre-satisfied waits return at once
            VulList_Wait32(this, base, 3, 0x1234_5678, u32::MAX, 0);
            VulList_Wait64(this, base + 8, 7, 0x1122_3344_5566_7788, u64::MAX, 0);
            // cross-thread masked EQUAL wait resolves like the title's
            let cell = base + 0x300;
            *(cell as *mut u32) = 0;
            let writer = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(15));
                *(cell as *mut u32) = 0xDEAD_00AA;
            });
            let t0 = std::time::Instant::now();
            VulList_Wait32(this, cell, 3, 0xAA, 0xFF, 0);
            assert!(t0.elapsed() < std::time::Duration::from_secs(5));
            writer.join().unwrap();
            assert_eq!(*(cell as *const u32), 0xDEAD_00AA);
            // kickoff is a silent no-op; null addresses never fault
            VulList_Kickoff(this);
            VulList_Write32Bottom(this, 0, 1, 0, 0);
            VulList_Write64Top(this, 0, 1, 0);
            VulList_TimestampBottom(this, 0, 0, 0, 1);
            VulList_Fill32(this, 0, 64, 1);
            VulList_CopyMemory(this, 0, base, 64, 0);
            VulList_CopyMemory(this, base, 0, 64, 0);
            VulList_Wait32(this, 0, 3, 1, u32::MAX, 0);
            VulList_Wait64(this, 0, 7, 1, u64::MAX, 0);
        }
    }

    #[test]
    fn vulkan_device_creates_vulkan_lists() {
        // Step-17 wiring: a Vulkan device's slot 12 (PC CreateCommandList)
        // and slot 73 (CreateCommandListX) hand out Vulkan lists that remember
        // their device; the null device keeps plain lists. Skips without a
        // Vulkan device; the null half always runs.
        unsafe {
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut ndev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut ndev), S_OK);
            type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: MkList = std::mem::transmute(slot(ndev, 12));
            let mut l = std::ptr::null_mut();
            assert_eq!(mk(ndev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            assert_eq!((*(l as *mut NullObj)).kind, Kind::CommandList);
            match acquire() {
                Some(st) => {
                    let vulkan_sel = Selection { backend: Backend::Vulkan, lane: None, reason: "test".into() };
                    let mut dev = std::ptr::null_mut();
                    assert_eq!(create_device_for(&vulkan_sel, Some(st), &mut dev), S_OK);
                    assert_eq!((*(dev as *mut NullObj)).kind, Kind::VulkanDevice);
                    // PC slot 12
                    let mk: MkList = std::mem::transmute(slot(dev, 12));
                    let mut l = std::ptr::null_mut();
                    assert_eq!(mk(dev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
                    assert_eq!((*(l as *mut NullObj)).kind, Kind::VulkanList);
                    // GetDevice (slot 7) hands back the Vulkan device
                    type GetDev = unsafe extern "win64" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
                    let get: GetDev = std::mem::transmute(slot(l, 7));
                    let mut d = std::ptr::null_mut();
                    assert_eq!(get(l, std::ptr::null(), &mut d), S_OK);
                    assert_eq!(d, dev);
                    // console slot 73 (CreateCommandListX)
                    type MkListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
                    let mkx: MkListX = std::mem::transmute(slot(dev, 73));
                    let mut lx = std::ptr::null_mut();
                    let desc = [0x10u32, 0, 1, 0];
                    assert_eq!(mkx(dev, desc.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut lx), S_OK);
                    assert_eq!((*(lx as *mut NullObj)).kind, Kind::VulkanList);
                    assert_eq!((*(lx as *mut NullObj)).value.load(Ordering::Relaxed), dev as u64);
                }
                None => crate::gpu::recomp::vulkan_runner::skip_test("no Vulkan"),
            }
        }
    }

    /// Calls `f(this)` (Win64) the way console code does, with a writable pointer left in rdx.
    #[cfg(all(target_arch = "x86_64", windows))]
    unsafe fn call_with_rdx(f: usize, this: *mut c_void, rdx: *mut u64) -> u64 {
        let ret: u64;
        // rsp is 16-aligned on entry to the asm block: 32 B of shadow space keep it so at the call
        core::arch::asm!(
            "sub rsp, 32",
            "call {f}",
            "add rsp, 32",
            f = in(reg) f,
            in("rcx") this,
            in("rdx") rdx,
            lateout("rax") ret,
            clobber_abi("system"),
        );
        ret
    }
}
