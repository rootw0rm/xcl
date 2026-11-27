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
//!
//! Slot-routing queue (M1 step-88 inventory, K-D3D-06 order; callers in
//! `tests/testtitle/src/gpu.rs`, which reaches console slots by raw vtable
//! index — the title has zero static d3d12 imports). Routed today: device
//! #44/#47/#48/#52/#56/#73/#74/#75, heap #9, list #9 (Close) / #11, queue
//! #10 (ExecuteCommandLists: null asserts the record, Vulkan submits), null + Vulkan-list
//! #60/#61/#69/#70/#71/#72/#89/#93/#146/#148, null + Vulkan-device AS SRV
//! #18, Vulkan-device frame clock
//! #78–#81 (Linux twin + ancillary routing) and Vulkan-queue #30 `PresentX`
//! (real swapchain present, 1d(ii)); a Vulkan device also answers the DXGI
//! device IIDs with an adapter/output chain (GetAdapter/GetDesc/EnumOutputs,
//! 1d follow-up). Null stays default behind
//! `XCL_VULKAN_BACKEND`: unrouted slots keep logging stubs. No fake-success:
//! a routed slot presents a real frame or stays an honest logging stub.
//
//! Step-146 note (2026-11-27): re-verified at `46f7a172` — queue #30 and
//! device #78–#81 are still logging stubs (no `v[78..81]` / queue `v[30]`
//! assignment below), and the retail abort path still issues zero d3d12
//! calls (`SIGSEGV game_scarlett_master.exe+0x2237320`, 20 `called_stubs`
//! with no d3d12 entry, `xcl.log` with no d3d12/PresentX/frame line, xgr
//! exactly XAsync/XNetworking/XUser). Routing this slice moves nothing until
//! the game advances past the abort, so it stays unrouted. Linux PresentX
//! additionally needs a `frame_linux` twin (`hle/d3d12x/frame.rs` is
//! Windows-only: `windows_sys` handles + waitable-timer clock thread) plus
//! windowed present wiring — queued for when the frontier reaches a console
//! slot call, not before.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::hle::vulkan_linux::{Backend, Selection, VulkanState, acquire, select_cached};
use crate::util::{E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, HRESULT, S_FALSE, S_OK};

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
    /// A queue from a Vulkan device (1d): same vtable as [`Kind::CommandQueue`]
    /// plus the routed queue #30 `PresentX` (1d(ii)); the null device keeps
    /// plain queues with the logging stub (no default change).
    VulkanQueue,
    CommandAllocator,
    CommandList,
    /// A command list from a Vulkan device (step 17): the graphics recording
    /// slots stay logging stubs like [`Kind::CommandList`], and the console
    /// memory methods (#60/#61/#69–#72/#89/#93) run for real on host memory —
    /// shared with the plain null lists (#3: the title reads back through
    /// them on both backends). The object's `value` holds the creator
    /// device object.
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
    /// A console IDXGIDevice wrapper (1d follow-up): answers the DXGI device
    /// IIDs for a Vulkan device; its `value` holds the device object. The
    /// null device keeps answering queries with itself (no change unset).
    DxgiDevice,
    /// The adapter behind a [`Kind::DxgiDevice`]: GetDesc with the device's
    /// LUID (plus the Vulkan name when one backs it).
    DxgiAdapter,
    /// A display output behind a [`Kind::DxgiAdapter`]: output 0 exists so
    /// the title's `EnumOutputs(0)` succeeds; the desc is unattached.
    DxgiOutput,
}

impl Kind {
    fn iface(self) -> &'static str {
        match self {
            Kind::Device | Kind::VulkanDevice => "ID3D12Device",
            Kind::CommandQueue | Kind::VulkanQueue => "ID3D12CommandQueue",
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
            Kind::DxgiDevice => "IDXGIDevice",
            Kind::DxgiAdapter => "IDXGIAdapter",
            Kind::DxgiOutput => "IDXGIOutput",
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
        Kind::CommandQueue | Kind::VulkanQueue => {
            v[10] = Queue_ExecuteCommandLists as usize;
            v[14] = Queue_Signal as usize;
            v[15] = Ret_S_OK as usize; // Wait
            v[16] = Queue_GetTimestampFrequency as usize;
            v[17] = Queue_GetClockCalibration as usize;
            v[18] = Hidden_ZeroDesc16 as usize; // GetDesc
        }
        Kind::CommandAllocator => v[8] = Ret_S_OK as usize, // Reset
        Kind::CommandList | Kind::VulkanList => {
            v[8] = Ret_Zero as usize; // GetType → DIRECT
            v[9] = List_Close as usize;
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
        Kind::DxgiDevice => {
            // IDXGIDevice: the title calls QI + GetAdapter; the rest stay stubs.
            v[0] = Dxgi_QI as usize;
            v[6] = Ret_E_NOTIMPL as usize; // GetParent
            v[7] = Dxgi_GetAdapter as usize;
        }
        Kind::DxgiAdapter => {
            // IDXGIAdapter: GetDesc serves the title's LUID check, EnumOutputs
            // serves output 0 (a real object, not the DeviceChild default).
            v[0] = Dxgi_QI as usize;
            v[6] = Ret_E_NOTIMPL as usize; // GetParent
            v[7] = Adapter_EnumOutputs as usize;
            v[8] = Adapter_GetDesc as usize;
        }
        Kind::DxgiOutput => {
            // IDXGIOutput: the title only needs the object to exist; GetDesc
            // reports it unattached.
            v[0] = Dxgi_QI as usize;
            v[6] = Ret_E_NOTIMPL as usize; // GetParent
            v[7] = Output_GetDesc as usize;
        }
        _ => {}
    }
    // Step-17 Vulkan overrides: creation slots on the Vulkan device hand out
    // Vulkan lists (the null device keeps the plain ones — no default change),
    // and Vulkan lists implement the console memory methods for real.
    if kind == Kind::VulkanDevice {
        v[12] = Vulkan_CreateCommandList as usize;
        v[13] = Vulkan_CheckFeatureSupport as usize;
        v[18] = Vulkan_CreateShaderResourceView as usize;
        v[73] = Vulkan_CreateCommandListX as usize;
        v[78] = Vulkan_SetFrameIntervalX as usize; // 1d(i) frame clock
        v[79] = Vulkan_ScheduleFrameEventX as usize;
        v[80] = Vulkan_WaitFrameEventX as usize;
        v[81] = Vulkan_GetFrameStatisticsX as usize;
    }
    if kind == Kind::VulkanList {
        v[7] = VulkanList_GetDevice as usize;
    }
    // #3 Execute leg: the console memory methods run for real on host
    // memory on the plain null lists too (the same immediate semantics as
    // the Vulkan lists — the title's graphics memory is host memory, so a
    // write the list applies is visible to the CPU at once on both
    // backends). GetDevice above stays Vulkan-only; the barrier (#146) and
    // upload (#148) slots run on the plain null lists too (M2 raytracing
    // slice: host upload memory + a coherent no-op barrier need no GPU).
    if kind == Kind::VulkanList || kind == Kind::CommandList {
        v[25] = List_SetPipelineState as usize; // S1 record bind (store-only)
        v[20] = List_IASetPrimitiveTopology as usize; // S2 topology (PC-side, store-only)
        v[21] = List_RSSetViewports as usize; // S2 viewports (PC-side, store-only)
        v[22] = List_RSSetScissorRects as usize; // S2 scissors (PC-side, store-only)
        v[34] = List_SetGraphicsRootSignature as usize; // S2 root sig (store-only)
        v[47] = List_IASetIndexBuffer as usize; // S2 index buffer (store-only)
        v[50] = List_OMSetRenderTargets as usize; // S2 render targets (store-only)
        // PC CopyBufferRegion (slot 15): the same immediate host memcpy as the
        // console DMA one — the single queue is already coherent, so the copy
        // lands at record time and ExecuteCommandLists stays a no-op.
        v[15] = Dma_CopyBufferRegion as usize;
        // M2 title-mem/streaming leg: PC CopyTextureRegion (slot 16) detiles a
        // placed title-memory texture into the readback buffer, same timing.
        v[16] = List_CopyTextureRegion as usize;
        v[60] = VulList_CopyMemory as usize;
        v[61] = VulList_Fill32 as usize;
        v[69] = VulList_Write32Bottom as usize;
        v[70] = VulList_TimestampBottom as usize;
        v[71] = VulList_Wait32 as usize;
        v[72] = VulList_Wait64 as usize;
        v[89] = VulList_Write64Top as usize;
        v[93] = VulList_Kickoff as usize;
    }
    if kind == Kind::VulkanList || kind == Kind::CommandList {
        v[146] = Ret_Void as usize; // UAV barrier (single queue: already coherent)
        v[148] = VulList_AllocUpload as usize;
    }
    // M2 raytracing slice: the null device writes the same RTAS view words
    // as the Vulkan one (a pure host-memory write into the heap slot, no GPU
    // needed) so the title's `raytracing_surface` leg 2 passes by default.
    if kind == Kind::Device {
        v[18] = Vulkan_CreateShaderResourceView as usize;
    }
    if kind == Kind::VulkanQueue {
        v[30] = Queue_PresentX as usize; // 1d(ii) real present
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
// DXGI identity: the GUID bytes the title queries (windows-crate values,
// little-endian fields), matched without the `windows` crate.
// ---------------------------------------------------------------------------------------

/// `54ec77fa-1377-44e6-8c32-88fd5f44c84c` … `05008617-fbfd-4051-a790-144884b4f6a9`.
const IID_DXGI_OBJECT: [u8; 16] = [0xb8, 0x2f, 0xc2, 0xae, 0xf3, 0x76, 0x39, 0x46, 0x9b, 0xe0, 0x28, 0xeb, 0x43, 0xa6, 0x7a, 0x2e];
const IID_DXGI_DEVICE: [u8; 16] = [0xfa, 0x77, 0xec, 0x54, 0x77, 0x13, 0xe6, 0x44, 0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c];
const IID_DXGI_DEVICE1: [u8; 16] = [0x0f, 0x97, 0xdb, 0x77, 0x76, 0x62, 0xba, 0x48, 0xba, 0x28, 0x07, 0x01, 0x43, 0xb4, 0x39, 0x2c];
const IID_DXGI_DEVICE2: [u8; 16] = [0x17, 0x86, 0x00, 0x05, 0xfd, 0xfb, 0x51, 0x40, 0xa7, 0x90, 0x14, 0x48, 0x84, 0xb4, 0xf6, 0xa9];
/// `2411e7e1-12ac-4ccf-bd14-9798e8534dc0` / `29038f61-3839-4626-91fd-086879011a05`.
const IID_DXGI_ADAPTER: [u8; 16] = [0xe1, 0xe7, 0x11, 0x24, 0xac, 0x12, 0xcf, 0x4c, 0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0];
const IID_DXGI_ADAPTER1: [u8; 16] = [0x61, 0x8f, 0x03, 0x29, 0x39, 0x38, 0x26, 0x46, 0x91, 0xfd, 0x08, 0x68, 0x79, 0x01, 0x1a, 0x05];
/// `ae02eedb-c735-4690-8d52-5a8dc20213aa`.
const IID_DXGI_OUTPUT: [u8; 16] = [0xdb, 0xee, 0x02, 0xae, 0x35, 0xc7, 0x90, 0x46, 0x8d, 0x52, 0x5a, 0x8d, 0xc2, 0x02, 0x13, 0xaa];
/// `00000000-0000-0000-c000-000000000046` / `189819f1-1db6-4b57-be54-1821339b85f7`.
const IID_UNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46];
const IID_D3D12DEVICE: [u8; 16] = [0xf1, 0x19, 0x98, 0x18, 0xb6, 0x1d, 0x57, 0x4b, 0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7];

fn iid_is(riid: *const c_void, want: &[u8; 16]) -> bool {
    !riid.is_null() && unsafe { std::ptr::read_unaligned(riid as *const [u8; 16]) } == *want
}

fn dxgi_device_iid(riid: *const c_void) -> bool {
    iid_is(riid, &IID_DXGI_OBJECT) || iid_is(riid, &IID_DXGI_DEVICE) || iid_is(riid, &IID_DXGI_DEVICE1) || iid_is(riid, &IID_DXGI_DEVICE2)
}

// ---------------------------------------------------------------------------------------
// common slots
// ---------------------------------------------------------------------------------------

unsafe extern "win64" fn QueryInterface(this: *mut NullObj, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    // A Vulkan device is also an IDXGIDevice (the Windows twin's `dxgi_device`):
    // DXGI queries get an adapter-chain wrapper; every other interface keeps
    // the old answer. The null device is untouched (no change while unset).
    if !this.is_null() && (*this).kind == Kind::VulkanDevice && dxgi_device_iid(riid) {
        let o = new_obj(Kind::DxgiDevice);
        (*o).value.store(this as u64, Ordering::Relaxed);
        (*this).refs.fetch_add(1, Ordering::AcqRel);
        *ppv = o as *mut c_void;
        return S_OK;
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
unsafe extern "win64" fn Device_CreateCommandQueue(this: *mut NullObj, _d: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    // 1d(i): a Vulkan device's queues route PresentX (1d(ii)); null keeps stubs.
    let vk = !this.is_null() && (*this).kind == Kind::VulkanDevice;
    out_obj(ppv, if vk { Kind::VulkanQueue } else { Kind::CommandQueue })
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

// ---------------------------------------------------------------------------------------
// M2 draw S1: PSO record (store-only). The draw-relevant PSO bytes — VS/PS,
// RTV formats/DSV format, rasterizer + blend — are stashed at creation
// (#60/#55 + PC10, read with the PC graphics-desc layout like
// `capture_graphics_desc` above) and bound to the list at SetPipelineState
// (#25). No draw, no validation, no execute: S3 (pipeline) interprets the
// bytes, S4 executes. `translate_vs_spirv` (`gpu/recomp/spirv.rs`) is the
// future VS consumer.
// ---------------------------------------------------------------------------------------

/// PC D3D12_GRAPHICS_PIPELINE_STATE_DESC draw-field offsets (x64).
pub const GFX_BLEND_OFF: usize = 120;
pub const GFX_BLEND_LEN: usize = 328;
pub const GFX_RASTER_OFF: usize = 452;
pub const GFX_RASTER_LEN: usize = 44;
pub const GFX_NUMRT_OFF: usize = 572;
pub const GFX_RTV_OFF: usize = 576;
pub const GFX_DSV_OFF: usize = 608;
pub const GFX_DESC_MIN: usize = 644;

/// Draw-relevant PSO bytes stashed at creation.
#[derive(Clone, Default)]
pub struct PsoRecord {
    pub vs_ptr: usize,
    pub vs_len: usize,
    pub ps_ptr: usize,
    pub ps_len: usize,
    pub blend: Vec<u8>,
    pub raster: Vec<u8>,
    pub num_rtv: u32,
    pub rtv_formats: [u32; 8],
    pub dsv_format: u32,
}

/// List-bound draw state: the PSO bound at SetPipelineState plus its record,
/// with the list-bound assembly S2 stores (root sig, render targets, index
/// buffer, viewports/scissors, topology) and the S4 Close mark. Store-only;
/// S3 interprets, S4 executes.
#[derive(Clone, Default)]
pub struct ListDrawRecord {
    pub pso: usize,
    pub pso_info: PsoRecord,
    pub root_sig: usize,
    pub rtv_count: u32,
    pub rtv_handles: [usize; 8],
    pub dsv_handle: usize,
    pub has_dsv: bool,
    pub index_va: u64,
    pub index_size: u32,
    pub index_format: u32,
    pub has_index: bool,
    pub topology: u32,
    pub viewports: Vec<u8>,
    pub scissors: Vec<u8>,
    pub closed: bool,
}

fn pso_store() -> &'static Mutex<HashMap<usize, PsoRecord>> {
    static S: OnceLock<Mutex<HashMap<usize, PsoRecord>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn list_store() -> &'static Mutex<HashMap<usize, ListDrawRecord>> {
    static S: OnceLock<Mutex<HashMap<usize, ListDrawRecord>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn pso_record_for(pso: usize) -> Option<PsoRecord> {
    pso_store().lock().unwrap().get(&pso).cloned()
}

pub fn list_record_for(list: usize) -> Option<ListDrawRecord> {
    list_store().lock().unwrap().get(&list).cloned()
}

/// Stash the PSO's draw bytes for `obj` (store-only; keeps today's capture).
pub unsafe fn record_graphics_pso(obj: *mut NullObj, desc: *const u8) {
    capture_graphics_desc(desc);
    if desc.is_null() {
        return;
    }
    // Null-shim trust, like `Device_CreateDescriptorHeap` above: the title
    // hands host pointers, so read them directly. (`shader::readable` is a
    // no-op false on Linux — gating on it would store an empty record on
    // the one platform S3 needs bytes on. A null desc stores nothing.)
    let mut r = PsoRecord::default();
    r.vs_ptr = *(desc.add(8) as *const usize);
    r.vs_len = *(desc.add(16) as *const usize);
    r.ps_ptr = *(desc.add(24) as *const usize);
    r.ps_len = *(desc.add(32) as *const usize);
    r.blend = std::slice::from_raw_parts(desc.add(GFX_BLEND_OFF), GFX_BLEND_LEN).to_vec();
    r.raster = std::slice::from_raw_parts(desc.add(GFX_RASTER_OFF), GFX_RASTER_LEN).to_vec();
    r.num_rtv = *(desc.add(GFX_NUMRT_OFF) as *const u32);
    for (i, f) in r.rtv_formats.iter_mut().enumerate() {
        *f = *(desc.add(GFX_RTV_OFF + 4 * i) as *const u32);
    }
    r.dsv_format = *(desc.add(GFX_DSV_OFF) as *const u32);
    pso_store().lock().unwrap().insert(obj as usize, r);
}

/// List slot 25 `SetPipelineState(pso)`: bind the PSO's record to this list,
/// preserving any S2 state already stored (order-independent).
unsafe extern "win64" fn List_SetPipelineState(this: *mut NullObj, pso: *mut c_void) {
    if this.is_null() {
        return;
    }
    let info = pso_store().lock().unwrap().get(&(pso as usize)).cloned().unwrap_or_default();
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    e.pso = pso as usize;
    e.pso_info = info;
}

/// List slot 34 `SetGraphicsRootSignature(rs)`: stash the bound root signature.
unsafe extern "win64" fn List_SetGraphicsRootSignature(this: *mut NullObj, rs: *mut c_void) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().root_sig = rs as usize;
}

/// List slot 50 `OMSetRenderTargets(n, rtvs, single, dsv)`: stash the target
/// handles (CPU-descriptor-sized words; capped at 8 RTVs).
unsafe extern "win64" fn List_OMSetRenderTargets(
    this: *mut NullObj,
    n: u32,
    rtvs: *const usize,
    _single: u32,
    dsv: *const usize,
) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    e.rtv_count = n.min(8);
    e.rtv_handles = [0; 8];
    if !rtvs.is_null() {
        for (i, h) in e.rtv_handles.iter_mut().enumerate().take(e.rtv_count as usize) {
            *h = *rtvs.add(i);
        }
    }
    if dsv.is_null() {
        e.has_dsv = false;
        e.dsv_handle = 0;
    } else {
        e.has_dsv = true;
        e.dsv_handle = *dsv;
    }
}

/// List slot 47 `IASetIndexBuffer(view)`: stash the 16-byte view
/// (BufferLocation u64, SizeInBytes u32, Format u32); null clears.
unsafe extern "win64" fn List_IASetIndexBuffer(this: *mut NullObj, view: *const u8) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    if view.is_null() {
        e.has_index = false;
        e.index_va = 0;
        e.index_size = 0;
        e.index_format = 0;
        return;
    }
    e.has_index = true;
    e.index_va = *(view as *const u64);
    e.index_size = *(view.add(8) as *const u32);
    e.index_format = *(view.add(12) as *const u32);
}

/// PC list slot 20 `IASetPrimitiveTopology(topology)`: stash the topology.
unsafe extern "win64" fn List_IASetPrimitiveTopology(this: *mut NullObj, topo: u32) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().topology = topo;
}

/// PC list slot 21 `RSSetViewports(n, vp)`: stash the raw 24-byte viewports.
unsafe extern "win64" fn List_RSSetViewports(this: *mut NullObj, n: u32, vp: *const u8) {
    if this.is_null() || vp.is_null() {
        return;
    }
    let bytes = (n.min(16) as usize) * 24;
    let v = std::slice::from_raw_parts(vp, bytes).to_vec();
    list_store().lock().unwrap().entry(this as usize).or_default().viewports = v;
}

/// PC list slot 22 `RSSetScissorRects(n, rects)`: stash the raw 16-byte rects.
unsafe extern "win64" fn List_RSSetScissorRects(this: *mut NullObj, n: u32, rects: *const u8) {
    if this.is_null() || rects.is_null() {
        return;
    }
    let bytes = (n.min(16) as usize) * 16;
    let v = std::slice::from_raw_parts(rects, bytes).to_vec();
    list_store().lock().unwrap().entry(this as usize).or_default().scissors = v;
}

// ---------------------------------------------------------------------------------------
// M2 draw S4: Close/Execute flush. List slot 9 `Close` marks the list; queue
// slot 10 `ExecuteCommandLists` flushes each listed record — null queues
// assert the record exists and stop there (no pixels), Vulkan queues run the
// S3 pipeline build and then a real offscreen submit whose pixels wait for
// the S5 readback. A list with no bound PSO is skipped with a debug line
// (fail-closed); missing-Vulkan (`"no ..."`) errors skip the same way.
// ---------------------------------------------------------------------------------------

static EXECUTE_FLUSHES: AtomicU64 = AtomicU64::new(0);

fn last_pixels_store() -> &'static Mutex<Option<crate::gpu::recomp::vulkan_graphics::OffscreenPixels>> {
    static S: OnceLock<Mutex<Option<crate::gpu::recomp::vulkan_graphics::OffscreenPixels>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// S4 flushes so far (null + Vulkan): how many listed records Execute saw.
pub fn execute_flush_count() -> u64 {
    EXECUTE_FLUSHES.load(Ordering::Relaxed)
}

/// Pixels from the last Vulkan Execute submit (`None` until one lands; S5
/// reads these back). Null Executes never touch this.
pub fn last_execute_pixels() -> Option<crate::gpu::recomp::vulkan_graphics::OffscreenPixels> {
    last_pixels_store().lock().unwrap().clone()
}

/// List slot 9 `Close`: mark the list closed (S4 Execute flushes it).
unsafe extern "win64" fn List_Close(this: *mut NullObj) -> HRESULT {
    if !this.is_null() {
        list_store().lock().unwrap().entry(this as usize).or_default().closed = true;
    }
    S_OK
}

/// Queue slot 10 `ExecuteCommandLists(count, lists)`: flush each listed
/// S1–S3 record; Vulkan queues submit for real pixels.
unsafe extern "win64" fn Queue_ExecuteCommandLists(this: *mut NullObj, count: u32, lists: *const *mut NullObj) {
    if this.is_null() || lists.is_null() || count == 0 {
        return;
    }
    let vk = (*this).kind == Kind::VulkanQueue;
    for i in 0..(count.min(64) as usize) {
        let l = *lists.add(i);
        if l.is_null() {
            continue;
        }
        let r = list_store().lock().unwrap().get(&(l as usize)).cloned();
        let Some(r) = r else {
            crate::xlog_debug!("D3D12.X Execute: list {:#x} holds no record — skipping", l as usize);
            continue;
        };
        if r.pso == 0 {
            crate::xlog_debug!("D3D12.X Execute: list {:#x} binds no PSO — skipping", l as usize);
            continue;
        }
        EXECUTE_FLUSHES.fetch_add(1, Ordering::Relaxed);
        if !vk {
            // null lists assert the record, no pixels.
            continue;
        }
        // Copy the VS bytes out from under the store lock (the title's
        // graphics memory is host memory; cap garbage lengths at the
        // triangle fallback instead of faulting).
        let bytes = if r.pso_info.vs_ptr != 0 && r.pso_info.vs_len < (16 << 20) && r.pso_info.vs_len != 0 {
            std::slice::from_raw_parts(r.pso_info.vs_ptr as *const u8, r.pso_info.vs_len).to_vec()
        } else {
            Vec::new()
        };
        match crate::gpu::recomp::vulkan_graphics::build_record_pipeline(&bytes, r.pso_info.num_rtv) {
            Ok(sum) => crate::xlog_debug!(
                "D3D12.X Execute: record pipeline built (vs {} words, fallback {})",
                sum.vs_words,
                sum.vs_fallback
            ),
            Err(e) if e.starts_with("no ") => {
                crate::xlog_debug!("D3D12.X Execute: pipeline skipped ({e})");
                continue;
            }
            Err(e) => {
                crate::xlog_warn!("D3D12.X Execute: pipeline failed ({e})");
                continue;
            }
        }
        match crate::gpu::recomp::vulkan_graphics::execute_record_draw(&bytes) {
            Ok((px, fallback)) => {
                crate::xlog_debug!("D3D12.X Execute: offscreen submit (fallback {fallback})");
                *last_pixels_store().lock().unwrap() = Some(px);
            }
            Err(e) if e.starts_with("no ") => crate::xlog_debug!("D3D12.X Execute: submit skipped ({e})"),
            Err(e) => crate::xlog_warn!("D3D12.X Execute: submit failed ({e})"),
        }
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
    let hr = out_obj(ppv, Kind::PipelineState);
    if hr == S_OK && !ppv.is_null() && !(*ppv).is_null() {
        record_graphics_pso(*ppv as *mut NullObj, d);
    }
    hr
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
        if size > 12 {
            // oversized probe padding: zero it (4 KiB sanity cap — the title
            // passes 16; a garbage size must not memset the address space)
            let tail = ((size - 12) as usize).min(4096);
            std::ptr::write_bytes((data as *mut u8).add(12), 0, tail);
        }
    }
    S_OK
}

/// Console descriptor heaps live in unified-memory window 0 (the retail game's
/// first placeholder begins with its view heap, and shader tables address them
/// with 32 bits): the handle range comes from the high-VA graphics pool's next
/// window-0 range, committed host memory (views write words into the slots) —
/// never plain host memory below the windows. Falls back to plain memory when
/// the pool has no window-0 room, so the heap still works.
unsafe extern "win64" fn Device_CreateDescriptorHeap(_t: *mut NullObj, desc: *const u8, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::DescriptorHeap);
    if !desc.is_null() {
        std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 16);
        let n = *(desc.add(4) as *const u32) as usize;
        let bytes = (n.max(1) * 32).min(64 << 20);
        // 64 KiB floor is the pool grain; the title reads slots back, so commit.
        let start = crate::hle::kernelx_linux::gfx_alloc_window0(bytes, 0x1_0000, true).unwrap_or(0);
        (*o).value.store(
            if start != 0 {
                start as u64
            } else {
                // exhaustion fallback: unique, never-dereferenced-by-us address range
                vec![0u8; bytes].leak().as_ptr() as u64
            },
            Ordering::Relaxed,
        );
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

/// Committed-resource backing in the high-VA graphics pool (real committable host
/// memory, so `Map` hands out the VA itself): small `READBACK` buffers go to
/// window 3 (the coherent room) while committed textures avoid window 3 so they
/// never eat that room — past it both fall back to the old fake-VA path, like
/// the Windows slice past `COHERENT_KEPT` / `COHERENT_READBACK_MAX`. Buffers of
/// any other heap type keep the old path (their legs already pass on it).
unsafe fn committed_backing(hp: *const c_void, desc: *const u8, size: usize) -> Option<usize> {
    if size == 0 {
        return None;
    }
    // D3D12_HEAP_PROPERTIES: Type u32 @0 (DEFAULT=1, UPLOAD=2, READBACK=3).
    let heap_ty = if hp.is_null() { 0 } else { (hp as *const u32).read_unaligned() };
    // D3D12_RESOURCE_DESC: Dimension u32 @0 (BUFFER=1).
    let dim = if desc.is_null() { 0 } else { *(desc as *const u32) };
    if heap_ty == 3 && size <= (64 << 20) {
        crate::hle::kernelx_linux::gfx_alloc_window3(size, 0x1_0000, true)
    } else if dim != 1 {
        crate::hle::kernelx_linux::gfx_alloc_avoid_window3(size, 0x1_0000, true)
    } else {
        None
    }
}

unsafe fn new_resource(hp: *const c_void, desc: *const u8, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::Resource);
    if !desc.is_null() {
        std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 56);
    }
    let size = approx_size(desc);
    if let Some(base) = committed_backing(hp, desc, size) {
        // pool-backed: VA == Map address (the console's unified memory)
        *(*o).mem.lock().unwrap() = (base, size);
        (*o).value.store(base as u64, Ordering::Relaxed);
        *ppv = o as *mut c_void;
        return S_OK;
    }
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
unsafe extern "win64" fn Device_CreateCommittedResource(_t: *mut NullObj, hp: *const c_void, _hf: u32, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(hp, desc, ppv)
}
unsafe extern "win64" fn Device_CreateCommittedResource1(_t: *mut NullObj, hp: *const c_void, _hf: u32, desc: *const u8, _st: u32, _cv: *const c_void, _ps: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(hp, desc, ppv)
}
/// Back a console buffer heap with the high-VA graphics pool: the pool address is a
/// real committable host address in window 0, so placed buffers on it get VA == Map
/// address (the console's unified memory) with title-side offsets aliasing. Returns 0
/// when the pool is exhausted — the heap still works, placed resources just fall back
/// to the old fake-VA path and lose aliasing.
unsafe fn heap_pool_backing(desc: *const c_void) -> u64 {
    if desc.is_null() {
        return 0;
    }
    // D3D12_HEAP_DESC: SizeInBytes u64 @0, Alignment u64 @32 (repr(C), unaligned reads).
    let d = desc as *const u8;
    let size = (d as *const u64).read_unaligned() as usize;
    let align = (d.add(32) as *const u64).read_unaligned() as usize;
    if size == 0 {
        return 0;
    }
    crate::hle::kernelx_linux::gfx_alloc(size, align, true).unwrap_or(0) as u64
}
unsafe extern "win64" fn Device_CreateHeap(_t: *mut NullObj, d: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::Heap);
    if !d.is_null() {
        std::ptr::copy_nonoverlapping(d as *const u8, (*o).desc.as_mut_ptr(), 16);
    }
    (*o).value.store(heap_pool_backing(d), Ordering::Relaxed);
    *ppv = o as *mut c_void;
    S_OK
}
unsafe extern "win64" fn Device_CreateHeap1(_t: *mut NullObj, d: *const c_void, _s: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::Heap);
    if !d.is_null() {
        std::ptr::copy_nonoverlapping(d as *const u8, (*o).desc.as_mut_ptr(), 16);
    }
    (*o).value.store(heap_pool_backing(d), Ordering::Relaxed);
    *ppv = o as *mut c_void;
    S_OK
}
unsafe extern "win64" fn Device_CreatePlacedResource(_t: *mut NullObj, h: *mut c_void, off: u64, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    if !h.is_null() && !desc.is_null() && (*(h as *mut NullObj)).kind == Kind::Heap {
        let base = (*(h as *mut NullObj)).value.load(Ordering::Relaxed);
        if base != 0 {
            // heap-backed: VA == host address, so Map and aliasing offsets agree
            let o = new_obj(Kind::Resource);
            std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 56);
            let size = approx_size(desc);
            *(*o).mem.lock().unwrap() = (base.wrapping_add(off) as usize, size);
            (*o).value.store(base.wrapping_add(off), Ordering::Relaxed);
            *ppv = o as *mut c_void;
            return S_OK;
        }
    }
    new_resource(std::ptr::null(), desc, ppv)
}
unsafe extern "win64" fn Device_CreateReservedResource(_t: *mut NullObj, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(std::ptr::null(), desc, ppv)
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
    first: u32,
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
    // M2 title-mem leg: a 2D texture's subresources (mip + slice × mips) each
    // get their own offset and their mip dims, so per-subresource copies land
    // in disjoint rows. Other descs keep the old shape.
    let dim = if desc.is_null() { 0 } else { *(desc as *const u32) };
    if dim == 3 && !desc.is_null() {
        let mips = ((*(desc.add(30) as *const u16)) as u32).max(1);
        let mut off = base;
        for i in 0..num as usize {
            let mip = (first as usize + i) as u32 % mips;
            let w = (width >> mip).max(1);
            let h = (height >> mip).max(1);
            let rp = ((w * 4 + 255) & !255) as u32;
            if !layouts.is_null() {
                let l = layouts.add(i * 32);
                *(l as *mut u64) = off;
                *(l.add(8) as *mut u32) = format;
                *(l.add(12) as *mut u32) = w as u32;
                *(l.add(16) as *mut u32) = h as u32;
                *(l.add(20) as *mut u32) = 1;
                *(l.add(24) as *mut u32) = rp;
            }
            if !num_rows.is_null() {
                *num_rows.add(i) = h as u32;
            }
            if !row_sizes.is_null() {
                *row_sizes.add(i) = w * 4;
            }
            off += rp as u64 * h as u64;
        }
        if !total.is_null() {
            *total = off - base;
        }
        return;
    }
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
// DXGI adapter/output chain (1d follow-up): device → IDXGIDevice1 →
// GetAdapter → GetDesc (LUID == GetAdapterLuid) → EnumOutputs, then the
// routed #78–#81 / #30 slots. Vulkan devices only; the null device keeps
// the old QueryInterface answer.
// ---------------------------------------------------------------------------------------

/// Shared QueryInterface for both wrapper kinds (plus IUnknown/IDXGIObject);
/// the device wrapper also hands back its D3D12 device, preserving identity.
/// Anything else is E_NOINTERFACE, never the device.
unsafe extern "win64" fn Dxgi_QI(this: *mut NullObj, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    *ppv = std::ptr::null_mut();
    if this.is_null() {
        return E_INVALIDARG;
    }
    let here = match (*this).kind {
        Kind::DxgiDevice => dxgi_device_iid(riid),
        Kind::DxgiAdapter => iid_is(riid, &IID_DXGI_ADAPTER) || iid_is(riid, &IID_DXGI_ADAPTER1),
        Kind::DxgiOutput => iid_is(riid, &IID_DXGI_OUTPUT),
        _ => false,
    };
    if here || iid_is(riid, &IID_UNKNOWN) || iid_is(riid, &IID_DXGI_OBJECT) {
        (*this).refs.fetch_add(1, Ordering::AcqRel);
        *ppv = this as *mut c_void;
        return S_OK;
    }
    if (*this).kind == Kind::DxgiDevice && iid_is(riid, &IID_D3D12DEVICE) {
        let dev = (*this).value.load(Ordering::Relaxed) as *mut NullObj;
        if !dev.is_null() {
            (*dev).refs.fetch_add(1, Ordering::AcqRel);
            *ppv = dev as *mut c_void;
            return S_OK;
        }
    }
    E_NOINTERFACE
}

/// IDXGIDevice slot 7: the device's adapter (one reference, the caller's);
/// the name is the Vulkan adapter's when one backs the device.
unsafe extern "win64" fn Dxgi_GetAdapter(this: *mut NullObj, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    let o = new_obj(Kind::DxgiAdapter);
    let mut name: &[u8] = b"XCL Vulkan adapter";
    if !this.is_null() {
        let dev = (*this).value.load(Ordering::Relaxed) as *const NullObj;
        if !dev.is_null() && (*dev).kind == Kind::VulkanDevice {
            let st = (*dev).value.load(Ordering::Relaxed) as *const VulkanState;
            if !st.is_null() && !(&(*st).name).is_empty() {
                name = &(*st).name.as_bytes();
            }
        }
    }
    let n = name.len().min(63);
    (&mut (*o).desc)[..n].copy_from_slice(&name[..n]);
    *ppv = o as *mut c_void;
    S_OK
}

// DXGI adapter chain, part 2 (1d follow-up): EnumOutputs serves output 0 as
// a real object (part 1 left the DeviceChild default, a bogus pointer the
// title only survived because SetFrameIntervalX ignores it).
/// IDXGIAdapter slot 7: output 0 exists; anything else is NOT_FOUND (a
/// headless box still serves the object — the title tolerates either, and
/// SetFrameIntervalX takes the output either way).
unsafe extern "win64" fn Adapter_EnumOutputs(_t: *mut NullObj, output: u32, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    if output != 0 {
        *ppv = std::ptr::null_mut();
        return 0x887A_0002u32 as i32; // DXGI_ERROR_NOT_FOUND
    }
    out_obj(ppv, Kind::DxgiOutput)
}

/// IDXGIAdapter slot 8: DXGI_ADAPTER_DESC (304 B) with the device's LUID —
/// the one field the title checks — plus the baked name and plausible memory.
unsafe extern "win64" fn Adapter_GetDesc(this: *mut NullObj, ret: *mut u8) -> HRESULT {
    if ret.is_null() {
        return E_POINTER;
    }
    std::ptr::write_bytes(ret, 0, 304);
    if !this.is_null() {
        // Description[128] WCHAR: UTF-16LE, NUL-terminated, from the baked name.
        let raw = &(*this).desc;
        let n = raw.iter().position(|&b| b == 0).unwrap_or(raw.len()).min(127);
        for (i, &b) in raw[..n].iter().enumerate() {
            (ret as *mut u16).add(i).write_unaligned(b as u16);
        }
    }
    // VendorId/DeviceId/SubSysId/Revision stay 0 (unknown); memory is
    // plausible — the title only checks the LUID below.
    (ret.add(272) as *mut u64).write_unaligned(4u64 << 30);
    (ret.add(288) as *mut u64).write_unaligned(8u64 << 30);
    (ret.add(296) as *mut u32).write_unaligned(0x5843_4C00); // == Device_GetAdapterLuid
    S_OK
}

/// IDXGIOutput slot 7: DXGI_OUTPUT_DESC (96 B), zeroed — unattached, like a
/// headless box.
unsafe extern "win64" fn Output_GetDesc(_t: *mut NullObj, ret: *mut u8) -> HRESULT {
    if ret.is_null() {
        return E_POINTER;
    }
    std::ptr::write_bytes(ret, 0, 96);
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
    this: *mut NullObj,
    _desc: *const u32,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let vk = !this.is_null() && (*this).kind == Kind::VulkanDevice;
    out_obj(ppv, if vk { Kind::VulkanQueue } else { Kind::CommandQueue })
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
    let hr = out_obj(ppv, Kind::PipelineState);
    if hr == S_OK && !ppv.is_null() && !(*ppv).is_null() {
        record_graphics_pso(*ppv as *mut NullObj, d);
    }
    hr
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
        // blend derivation carries the base record; S3 applies the patch bytes.
        2 if !_base.is_null() => {
            let hr = out_obj(ppv, Kind::PipelineState);
            if hr == S_OK && !ppv.is_null() && !(*ppv).is_null() {
                let base = pso_store().lock().unwrap().get(&(_base as usize)).cloned().unwrap_or_default();
                pso_store().lock().unwrap().insert(*ppv as usize, base);
            }
            hr
        }
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

/// Frame clock slots on the Vulkan device (1d(i)): thin adapters over the
/// Linux twin (`d3d12x_frame_linux`) with the title's ancillary handles routed
/// at schedule time. Signatures mirror the title's raw vtable calls
/// (`present_x_frame`), return codes the Windows twin's.

// device #78 SetFrameIntervalX(output, length µs, periodicity, flags)
unsafe extern "win64" fn Vulkan_SetFrameIntervalX(_t: *mut NullObj, _out: *mut c_void, len_us: u32, _per: u32, _fl: u32) -> HRESULT {
    crate::hle::d3d12x_frame_linux::set_frame_interval(len_us)
}

// device #79 ScheduleFrameEventX(type, offset µs, ancillary, flags)
unsafe extern "win64" fn Vulkan_ScheduleFrameEventX(_t: *mut NullObj, ty: u32, off_us: u32, block: *const u8, flags: u32) -> HRESULT {
    // UNSCHEDULE = 1, like the Windows twin
    let hr = crate::hle::d3d12x_frame_linux::register_ancillary(ty, block, flags & 1 != 0);
    if hr != S_OK {
        return hr;
    }
    crate::hle::d3d12x_frame_linux::schedule_frame_event(ty, off_us, flags)
}

// device #80 WaitFrameEventX(type, timeout ms, ancillary, flags, token in/out)
unsafe extern "win64" fn Vulkan_WaitFrameEventX(_t: *mut NullObj, ty: u32, timeout_ms: u32, _block: *const u8, flags: u32, token: *mut u64) -> HRESULT {
    // the title may pass no token storage; the wait still paces the frame
    let mut t = if token.is_null() { 0 } else { *token };
    let hr = crate::hle::d3d12x_frame_linux::wait_frame_event(ty, timeout_ms, flags, &mut t);
    if !token.is_null() {
        *token = t;
    }
    hr
}

// device #81 GetFrameStatisticsX(token, type, count in/out, statistics)
unsafe extern "win64" fn Vulkan_GetFrameStatisticsX(_t: *mut NullObj, token: u64, ty: u32, count: *mut u32, stats: *mut u8) -> HRESULT {
    if count.is_null() || stats.is_null() {
        return E_INVALIDARG;
    }
    match crate::hle::d3d12x_frame_linux::frame_statistics(token) {
        Ok((origin, flip)) => {
            *count = 1;
            (stats as *mut u32).write_unaligned(ty);
            (stats.add(0x10) as *mut i64).write_unaligned(origin);
            (stats.add(0x18) as *mut i64).write_unaligned(flip);
            S_OK
        }
        Err(e) => {
            *count = 0;
            e
        }
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

/// An XG texture computer for the console's 0x38-byte desc, or `None` when the
/// layout is not modelled. The table address is fixed: cache it.
fn xg_computer(desc: &[u8; 0x38]) -> Option<*mut c_void> {
    static ADDR: OnceLock<usize> = OnceLock::new();
    let addr = *ADDR.get_or_init(|| {
        crate::hle::xg_linux::exports()
            .into_iter()
            .find(|(n, _)| *n == "XGCreateTextureComputer")
            .map(|(_, a)| a)
            .unwrap_or(0)
    });
    if addr == 0 {
        return None;
    }
    type Create = unsafe extern "win64" fn(*const u8, *mut *mut c_void) -> HRESULT;
    let mut c = std::ptr::null_mut();
    if unsafe { std::mem::transmute::<usize, Create>(addr)(desc.as_ptr(), &mut c) } < 0 || c.is_null() {
        None
    } else {
        Some(c)
    }
}

/// PC `CopyTextureRegion(dst, x, y, z, src, box)` on a command list: detile the
/// subresource out of a placed title-memory texture into the destination
/// buffer's footprint at record time (the single queue is already coherent, so
/// the copy lands at once and `ExecuteCommandLists` stays a no-op — the same
/// immediate semantics as the DMA `CopyBufferRegion`). Only the title's
/// texture-readback shape is modelled: a whole-subresource (`box` null) RGBA8
/// copy from a `PlacedResource` with no D3D12 flags into a buffer. Anything
/// else (committed render targets, boxes, other formats) stays a silent no-op
/// like the old stub.
unsafe extern "win64" fn List_CopyTextureRegion(
    _t: *mut NullObj,
    dst: *const u8,
    x: u32,
    y: u32,
    _z: u32,
    src: *const u8,
    bx: *const u8,
) {
    if dst.is_null() || src.is_null() || !bx.is_null() {
        return;
    }
    // D3D12_TEXTURE_COPY_LOCATION: pResource @0, Type u32 @8 (destination
    // PLACED_FOOTPRINT = 1, source SUBRESOURCE_INDEX = 0), union @16: the
    // footprint (Offset u64 @16, Format u32 @24, Height u32 @32, RowPitch u32
    // @40) or the subresource index u32 @16.
    let dob = (dst as *const *mut NullObj).read_unaligned();
    let sob = (src as *const *mut NullObj).read_unaligned();
    if dob.is_null() || sob.is_null() {
        return;
    }
    if (dst.add(8) as *const u32).read_unaligned() != 1
        || (src.add(8) as *const u32).read_unaligned() != 0
    {
        return;
    }
    if (*sob).kind != Kind::PlacedResource {
        return;
    }
    if (dst.add(24) as *const u32).read_unaligned() != 28 {
        return;
    }
    let sub = (src.add(16) as *const u32).read_unaligned();
    let va = (*sob).value.load(Ordering::Relaxed);
    if va == 0 {
        return;
    }
    // the placed texture's desc (56 B recorded at creation) as the console's
    // 0x38-byte XG desc (dim, width, height, array, mips, format, samples,
    // layout — D3D12 flags stay out: a flagged texture lays out differently).
    let sd = &(*sob).desc;
    if (sd.as_ptr() as *const u32).read_unaligned() != 3
        || (sd.as_ptr().add(48) as *const u32).read_unaligned() != 0
    {
        return;
    }
    let mut xd = [0u8; 0x38];
    xd[0..4].copy_from_slice(&sd[0..4]);
    xd[0x10..0x18].copy_from_slice(&sd[16..24]);
    xd[0x18..0x1C].copy_from_slice(&sd[24..28]);
    xd[0x1C..0x1E].copy_from_slice(&sd[28..30]);
    xd[0x1E..0x20].copy_from_slice(&sd[30..32]);
    xd[0x20..0x24].copy_from_slice(&sd[32..36]);
    xd[0x24..0x28].copy_from_slice(&sd[36..40]);
    xd[0x2C..0x30].copy_from_slice(&sd[44..48]);
    let Some(xg) = xg_computer(&xd) else {
        return;
    };
    type Release = unsafe extern "win64" fn(*mut c_void) -> u32;
    let release: Release = std::mem::transmute(*(*(xg as *const *const usize)).add(1));
    // the computer's CopyFromSubresource detiles into linear rows (its slot 9):
    // (computer, linear dst, plane, sub, tiled src, row pitch, slice pitch, 0).
    type From = unsafe extern "win64" fn(*mut c_void, *mut u8, u32, u32, usize, usize, usize, usize) -> HRESULT;
    let from: From = std::mem::transmute(*(*(xg as *const *const usize)).add(9));
    let rp = (dst.add(40) as *const u32).read_unaligned() as u64;
    let h = (dst.add(32) as *const u32).read_unaligned() as u64;
    let off = (dst.add(16) as *const u64).read_unaligned();
    let (dp, dn) = resource_backing(dob);
    let start = off.saturating_add(y as u64 * rp).saturating_add(x as u64 * 4);
    let end = start.saturating_add(rp.saturating_mul(h));
    if dp.is_null() || end as usize > dn {
        release(xg);
        return;
    }
    from(xg, dp.add(start as usize), 0, sub, va as usize, rp as usize, rp.saturating_mul(h) as usize, 0);
    release(xg);
}

// ---------------------------------------------------------------------------------------
// Vulkan device overrides (step 17): the raytracing surface the title probes.
// Only slot 13 (feature support) and slot 18 (RTAS views) differ from the null
// device; everything else still delegates.
// ---------------------------------------------------------------------------------------

/// `CheckFeatureSupport` on a Vulkan device: the null behavior (raytracing
/// tier at +8 of `D3D12_FEATURE_D3D12_OPTIONS5`) plus zeroing the tail past
/// the 12-byte struct — the console runtime zeroes it, and the title probes
/// with a 16-byte struct filled with `0xAA` to check exactly that.
unsafe extern "win64" fn Vulkan_CheckFeatureSupport(_t: *mut NullObj, feature: u32, data: *mut c_void, size: u32) -> HRESULT {
    crate::xlog_debug!("D3D12.X Vulkan: CheckFeatureSupport({feature})");
    if feature == 27 && !data.is_null() && size >= 12 {
        let tier = crate::state::config().raytracing_tier_override().unwrap_or(0);
        ((data as *mut u8).add(8) as *mut u32).write_unaligned(tier);
        if size > 12 {
            // oversized probe padding: zero it (4 KiB sanity cap — the title
            // passes 16; a garbage size must not memset the address space)
            let tail = ((size - 12) as usize).min(4096);
            std::ptr::write_bytes((data as *mut u8).add(12), 0, tail);
        }
    }
    S_OK
}

/// `CreateShaderResourceView(res, desc, dest)` on a Vulkan or null device: a
/// raytracing-acceleration-structure view (dimension 11) writes the RDNA2 BVH
/// descriptor words into the heap slot `dest` points at —
/// `[(va >> 8), ((va >> 40) & 0xFF) | (1 << 31), u32::MAX, 0]` — the exact
/// words the title reads back. A pure host-memory write, so the null device
/// shares it (no GPU needed). Other view dimensions stay unmodelled (left
/// untouched, like the null stub) until a caller needs them.
unsafe extern "win64" fn Vulkan_CreateShaderResourceView(_t: *mut NullObj, _res: *mut c_void, desc: *const u8, dest: usize) {
    const RTAS_DIM: u32 = 11;
    if desc.is_null() || dest == 0 {
        return;
    }
    // D3D12_SHADER_RESOURCE_VIEW_DESC: Format u32 @0, ViewDimension u32 @4,
    // Shader4ComponentMapping u32 @8, union @16 (8-aligned: 4 bytes of
    // padding after the three u32s; RTAS: Location u64 @16).
    // Unaligned reads: the descriptor comes from console code as bytes.
    if (desc.add(4) as *const u32).read_unaligned() != RTAS_DIM {
        return;
    }
    let va = (desc.add(16) as *const u64).read_unaligned();
    let words = [(va >> 8) as u32, (((va >> 40) as u32) & 0xFF) | (1 << 31), u32::MAX, 0];
    // byte copy: the heap slot is host memory without alignment promises
    std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, dest as *mut u8, 16);
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

/// `AllocateUploadMemoryX(size, alignment)` (slot 148): host upload memory —
/// the title only needs it aligned and writable (it writes a tag word and
/// reads it back). Zeroed, intentionally leaked like every null-device block;
/// the pointer/size are remembered in the list's `mem` cell.
unsafe extern "win64" fn VulList_AllocUpload(this: *mut NullObj, size: usize, align: usize) -> *mut u8 {
    if this.is_null() || size == 0 {
        return std::ptr::null_mut();
    }
    let align = align.max(std::mem::align_of::<usize>()).next_power_of_two();
    let Ok(layout) = std::alloc::Layout::from_size_align(size, align) else {
        return std::ptr::null_mut();
    };
    let p = std::alloc::alloc_zeroed(layout);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    *(*this).mem.lock().unwrap() = (p as usize, size);
    p
}

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

/// Host RGBA bytes of a back-buffer resource (R8G8B8A8 2D texture only):
/// committed resources from their Map backing, placed ones straight from
/// title memory. Unmodelled shapes return `None` and the caller presents
/// nothing. Linux trusts the pointers (no `/proc/self/maps` walk yet —
/// `gpu::shader::readable` is windows-only), like every other Linux slot.
unsafe fn read_rgba(o: *mut NullObj) -> Option<(u32, u32, Vec<u8>)> {
    // D3D12_RESOURCE_DESC: Dimension u32 @0, Width u64 @16, Height u32 @24,
    // Format u32 @32.
    let d = &(*o).desc;
    if d[0] != 3 || u32::from_le_bytes(d[32..36].try_into().ok()?) != 28 {
        return None; // not a 2D R8G8B8A8_UNORM texture
    }
    let (w, h) = (
        u64::from_le_bytes(d[16..24].try_into().ok()?) as u32,
        u32::from_le_bytes(d[24..28].try_into().ok()?),
    );
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let n = (w as usize) * (h as usize) * 4;
    let src = if (*o).kind == Kind::PlacedResource {
        (*o).value.load(Ordering::Relaxed) as usize as *const u8
    } else {
        let (p, len) = resource_backing(o);
        if p.is_null() || len < n {
            return None;
        }
        p as *const u8
    };
    if src.is_null() {
        return None;
    }
    let mut v = vec![0u8; n];
    std::ptr::copy_nonoverlapping(src, v.as_mut_ptr(), n);
    Some((w, h, v))
}

/// Queue #30 `PresentX` on a Vulkan queue (1d(ii)): plane 0's resource is the
/// frame — upload its host bytes through the real swapchain present, then
/// record the flip for #81. Headless (`"no ..."`) still records the flip and
/// returns `S_OK`: a console present never fails for the title.
unsafe extern "win64" fn Queue_PresentX(_t: *mut NullObj, plane_count: u32, planes: *const u8, _params: *const u8) -> HRESULT {
    if plane_count != 0 && !planes.is_null() {
        let token = (planes as *const u64).read_unaligned();
        // plane 0: { u64 token @0; u32 count @8; resources @0x10 }
        let mut rgba = None;
        if (planes.add(8) as *const u32).read_unaligned() > 0 {
            let list = (planes.add(0x10) as *const usize).read_unaligned();
            if list != 0 {
                let res = (list as *const usize).read_unaligned() as *mut NullObj;
                if !res.is_null() && ((*res).kind == Kind::Resource || (*res).kind == Kind::PlacedResource) {
                    rgba = read_rgba(res);
                }
            }
        }
        match rgba {
            Some((w, h, px)) => {
                match crate::gpu::recomp::vulkan_present::present_rgba_image(w, h, &px) {
                    Ok(()) => crate::xlog_info!("D3D12.X Vulkan: presented {w}x{h} (token {token:#x})"),
                    Err(e) if e.starts_with("no ") => {
                        crate::xlog_debug!("D3D12.X Vulkan: present skipped ({e})")
                    }
                    Err(e) => crate::xlog_warn!("D3D12.X Vulkan: present failed ({e})"),
                }
            }
            None => crate::xlog_debug!("D3D12.X Vulkan: PresentX with no readable plane-0 RGBA (token {token:#x})"),
        }
        crate::hle::d3d12x_frame_linux::mark_flipped(token);
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

/// (tiles of title memory with video memory, pool heaps holding them) for the memory
/// line (port of upstream #194's `sparse::stats()` shape): `None` – the null backend
/// has no sparse windows, so the memory line omits the segment, exactly as upstream
/// omits it with no sparse windows.
pub fn stats() -> Option<(usize, usize)> {
    None
}

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

    /// M2 desc-heaps leg: the title's 8 GiB+ graphics reservation is accepted, and
    /// console descriptor heaps made after it still get window-0 starts outside it.
    /// Fails pre-slice twice: the reservation is refused (the pool is exactly
    /// 8 GiB) and heap starts are plain host memory below window 0.
    #[test]
    fn descriptor_heaps_stay_in_window_0_after_a_large_graphics_reservation() {
        unsafe {
            let big = 0x2_0041_0000usize;
            let r = crate::hle::kernelx_linux::XMemVirtualAlloc(
                std::ptr::null(),
                big,
                0x2000,
                1u64 << 29,
                4,
            );
            assert!(!r.is_null(), "graphics reservation of {big:#x} bytes refused");
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create_heap: Fn4 = std::mem::transmute(slot(dev, 14));
            for (ty, n) in [(0u32, 442_368u32), (1u32, 512u32)] {
                let desc = [ty, n, 1, 0]; // shader visible
                let mut h = std::ptr::null_mut();
                assert_eq!(create_heap(dev, desc.as_ptr() as *const c_void, std::ptr::null(), &mut h), S_OK);
                let start: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(h, 9));
                let p = start(h);
                assert!(p >> 32 >= 4 && (p >> 32) & 3 == 0, "heap of {n} slots is not in window 0: {p:#x}");
                assert!(p < r as u64 || p >= r as u64 + big as u64, "heap at {p:#x} inside the reservation {r:?}");
            }
            assert_eq!(crate::hle::kernelx_linux::VirtualFree(r, 0, 0x8000), 1);
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
    fn null_list_shares_the_console_memory_methods() {
        // #3 Execute leg: the null device's plain lists run the same
        // immediate host-memory methods as the Vulkan lists (writes, fill,
        // copy, a timestamp, waits, kickoff) — the title's
        // `console_list_methods` reads back through these slots on both
        // backends. Fails pre-route (slot 69 is a logging stub: no write).
        unsafe {
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut ndev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut ndev), S_OK);
            type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: MkList = std::mem::transmute(slot(ndev, 12));
            let mut l = std::ptr::null_mut();
            assert_eq!(mk(ndev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            assert_eq!((*(l as *mut NullObj)).kind, Kind::CommandList);
            let mut mem = vec![0u8; 0x400];
            let base = mem.as_mut_ptr() as u64;
            let r32 = |o: usize| (mem.as_ptr().add(o) as *const u32).read();
            let r64 = |o: usize| (mem.as_ptr().add(o) as *const u64).read();
            type W32 = unsafe extern "win64" fn(*mut c_void, u64, u32, u32, u32);
            let w32: W32 = std::mem::transmute(slot(l, 69));
            w32(l, base, 0x1234_5678, 0, 0);
            assert_eq!(r32(0), 0x1234_5678);
            type W64 = unsafe extern "win64" fn(*mut c_void, u64, u64, u32);
            let w64: W64 = std::mem::transmute(slot(l, 89));
            w64(l, base + 8, 0x1122_3344_5566_7788, 0);
            assert_eq!(r64(8), 0x1122_3344_5566_7788);
            type Ts = unsafe extern "win64" fn(*mut c_void, u64, u64, u32, u32);
            let ts: Ts = std::mem::transmute(slot(l, 70));
            ts(l, base + 0x10, 0, 0, 1);
            assert_ne!(r64(0x10), 0);
            type Fill = unsafe extern "win64" fn(*mut c_void, u64, u64, u32);
            let fill: Fill = std::mem::transmute(slot(l, 61));
            fill(l, base + 0x100, 0x100, 0xABCD_EF01);
            assert!((0..64).all(|i| r32(0x100 + 4 * i) == 0xABCD_EF01));
            assert_eq!(r32(0x200), 0);
            type Copy = unsafe extern "win64" fn(*mut c_void, u64, u64, u64, u32);
            let copy: Copy = std::mem::transmute(slot(l, 60));
            for i in 0..64 {
                *mem.as_mut_ptr().add(0x300 + i) = i as u8 ^ 0x5A;
            }
            copy(l, base + 0x200, base + 0x300, 64, 0);
            assert!((0..64).all(|i| *mem.as_ptr().add(0x200 + i) == i as u8 ^ 0x5A));
            // pre-satisfied waits return at once; kickoff is a silent no-op
            type Wait = unsafe extern "win64" fn(*mut c_void, u64, u32, u32, u32, u32);
            let wait: Wait = std::mem::transmute(slot(l, 71));
            wait(l, base, 3, 0x1234_5678, u32::MAX, 0);
            type Kick = unsafe extern "win64" fn(*mut c_void);
            let kick: Kick = std::mem::transmute(slot(l, 93));
            kick(l);
            // the copy-source pattern fill clobbered offset 0; the write64
            // cell and the copied region survived it
            assert_eq!((mem.as_ptr().add(8) as *const u64).read(), 0x1122_3344_5566_7788);
            assert!((0..64).all(|i| *mem.as_ptr().add(0x200 + i) == i as u8 ^ 0x5A));
        }
    }

    #[test]
    fn heap_backed_placed_buffers_share_va_and_map_and_copy() {
        // M2 VA-vs-Map leg: a heap comes from the high-VA graphics pool, so
        // placed buffers on it report VA == Map address with title-side
        // aliasing offsets, and PC CopyBufferRegion (slot 15) moves bytes at
        // record time. Fails pre-slice three ways: each placed resource gets
        // a fresh fake VA, Map hands out detached memory, slot 15 is a stub.
        unsafe {
            let dev = device() as *mut c_void;
            // D3D12_HEAP_DESC (48 B): SizeInBytes u64 @0, Type u32 @8,
            // Alignment u64 @32.
            let mut hdesc = [0u8; 48];
            (hdesc.as_mut_ptr() as *mut u64).write_unaligned(0x4_0000);
            (hdesc.as_mut_ptr().add(8) as *mut u32).write_unaligned(1);
            (hdesc.as_mut_ptr().add(32) as *mut u64).write_unaligned(0x1_0000);
            type MkHeap = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk_heap: MkHeap = std::mem::transmute(slot(dev, 28));
            let mut h = std::ptr::null_mut();
            assert_eq!(mk_heap(dev, hdesc.as_ptr() as *const c_void, std::ptr::null(), &mut h), S_OK);
            // D3D12_RESOURCE_DESC (56 B): BUFFER (dim 1) + Width @16.
            let buf_desc = |bytes: u64| {
                let mut d = [0u8; 56];
                (d.as_mut_ptr() as *mut u32).write_unaligned(1);
                (d.as_mut_ptr().add(16) as *mut u64).write_unaligned(bytes);
                d
            };
            type MkPlaced = unsafe extern "win64" fn(*mut c_void, *mut c_void, u64, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk_placed: MkPlaced = std::mem::transmute(slot(dev, 29));
            let (da, db) = (buf_desc(0x1_0000), buf_desc(0x1000));
            let (mut a, mut b) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(mk_placed(dev, h, 0, da.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut a), S_OK);
            assert_eq!(mk_placed(dev, h, 0x200, db.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut b), S_OK);
            type Map = unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let map: Map = std::mem::transmute(slot(a, 8));
            type Va = unsafe extern "win64" fn(*mut c_void) -> u64;
            let va: Va = std::mem::transmute(slot(a, 11));
            let (mut pa, mut pb) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(map(a, 0, std::ptr::null(), &mut pa), S_OK);
            assert_eq!(map(b, 0, std::ptr::null(), &mut pb), S_OK);
            assert!(!pa.is_null() && !pb.is_null());
            assert_eq!(va(a), pa as u64, "placed VA != Map address");
            assert_eq!(va(b), pa as u64 + 0x200, "aliased VA mismatch");
            assert_eq!(pb as u64, pa as u64 + 0x200, "aliased Map mismatch");
            assert_ne!((va(a) >> 32) & 3, 3, "heap pool must avoid window 3");
            // PC CopyBufferRegion (slot 15) into a committed buffer, at once.
            type MkCommitted = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk_committed: MkCommitted = std::mem::transmute(slot(dev, 27));
            let dc = buf_desc(0x1000);
            let mut rb = std::ptr::null_mut();
            assert_eq!(mk_committed(dev, std::ptr::null(), 0, dc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            (pa as *mut u32).write(0x1122_3344);
            type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk_list: MkList = std::mem::transmute(slot(dev, 12));
            let mut l = std::ptr::null_mut();
            assert_eq!(mk_list(dev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type CopyBuf = unsafe extern "win64" fn(*mut c_void, *mut c_void, u64, *mut c_void, u64, u64);
            let copy: CopyBuf = std::mem::transmute(slot(l, 15));
            copy(l, rb, 0, a, 0, 4);
            let mut prb = std::ptr::null_mut();
            assert_eq!(map(rb, 0, std::ptr::null(), &mut prb), S_OK);
            assert_eq!((prb as *const u32).read(), 0x1122_3344, "slot-15 copy did not land");
        }
    }

    #[test]
    fn committed_textures_leave_room_for_window3_readback() {
        // M2 committed-tex leg (title `committed_textures_leave_room_for_readback`):
        // committed textures avoid window 3, leaving the coherent room for
        // readback, and a readback buffer made afterwards gets a window-3 VA
        // with Map handing out the VA itself. Fails pre-slice: every
        // committed resource gets a fake bump-allocator VA outside window 3.
        unsafe {
            let dev = device() as *mut c_void;
            type MkCommitted = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: MkCommitted = std::mem::transmute(slot(dev, 27));
            // D3D12_RESOURCE_DESC (56 B): dim @0, Width u64 @16, Height u32 @24,
            // Format u32 @32.
            let tex_desc = |side: u64| {
                let mut d = [0u8; 56];
                (d.as_mut_ptr() as *mut u32).write_unaligned(3); // TEXTURE2D
                (d.as_mut_ptr().add(16) as *mut u64).write_unaligned(side);
                (d.as_mut_ptr().add(24) as *mut u32).write_unaligned(side as u32);
                (d.as_mut_ptr().add(32) as *mut u32).write_unaligned(28); // R8G8B8A8_UNORM
                d
            };
            let buf_desc = |bytes: u64| {
                let mut d = [0u8; 56];
                (d.as_mut_ptr() as *mut u32).write_unaligned(1); // BUFFER
                (d.as_mut_ptr().add(16) as *mut u64).write_unaligned(bytes);
                d
            };
            // D3D12_HEAP_PROPERTIES (20 B): Type u32 @0 (DEFAULT=1, READBACK=3).
            let props = |ty: u32| {
                let mut p = [0u8; 20];
                (p.as_mut_ptr() as *mut u32).write_unaligned(ty);
                p
            };
            let (def, rb_props) = (props(1), props(3));
            let td = tex_desc(512);
            let mut kept = Vec::new();
            for _ in 0..8 {
                let mut t = std::ptr::null_mut();
                assert_eq!(mk(dev, def.as_ptr() as *const c_void, 0, td.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut t), S_OK);
                assert!(!t.is_null());
                kept.push(t);
            }
            // a render target with a console-only flag bit is still created
            let rt = tex_desc(2048);
            let mut r = std::ptr::null_mut();
            assert_eq!(mk(dev, def.as_ptr() as *const c_void, 0, rt.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut r), S_OK);
            assert!(!r.is_null());
            kept.push(r);
            type Va = unsafe extern "win64" fn(*mut c_void) -> u64;
            let va: Va = std::mem::transmute(slot(kept[0], 11));
            for t in &kept {
                assert_ne!((va(*t) >> 32) & 3, 3, "committed texture in window 3: {:#x}", va(*t));
            }
            let bd = buf_desc(1 << 20);
            let mut rb = std::ptr::null_mut();
            assert_eq!(mk(dev, rb_props.as_ptr() as *const c_void, 0, bd.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            assert!(!rb.is_null());
            assert_eq!((va(rb) >> 32) & 3, 3, "readback buffer after the textures is not in window 3: {:#x}", va(rb));
            type Map = unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let map: Map = std::mem::transmute(slot(rb, 8));
            let mut p = std::ptr::null_mut();
            assert_eq!(map(rb, 0, std::ptr::null(), &mut p), S_OK);
            assert_eq!(p as u64, va(rb), "readback Map != VA");
        }
    }

    /// M2 title-mem leg (title `title_texture`): `GetCopyableFootprints` hands
    /// each subresource its own offset and mip dims, and PC `CopyTextureRegion`
    /// (list slot 16) detiles the placed title memory into the readback buffer
    /// — all four subs read back, and a rewritten sub re-reads its new texels.
    /// Fails pre-slice: every footprint shares offset 0 and slot 16 is a stub.
    #[test]
    fn title_memory_texture_copy_detiles_all_subresources() {
        unsafe {
            // XG desc (0x38 B): the title's 80x40 array-2 mip-2 64KB_S_X shape.
            let mut xd = [0u8; 0x38];
            (xd.as_mut_ptr() as *mut u32).write_unaligned(3);
            (xd.as_mut_ptr().add(0x10) as *mut u64).write_unaligned(80);
            (xd.as_mut_ptr().add(0x18) as *mut u32).write_unaligned(40);
            (xd.as_mut_ptr().add(0x1C) as *mut u16).write_unaligned(2);
            (xd.as_mut_ptr().add(0x1E) as *mut u16).write_unaligned(2);
            (xd.as_mut_ptr().add(0x20) as *mut u32).write_unaligned(28);
            (xd.as_mut_ptr().add(0x2C) as *mut u32).write_unaligned(0x119);
            let create_addr = crate::hle::xg_linux::exports()
                .into_iter()
                .find(|(n, _)| *n == "XGCreateTextureComputer")
                .map(|(_, a)| a)
                .unwrap();
            type CreateXg = unsafe extern "win64" fn(*const u8, *mut *mut c_void) -> HRESULT;
            let create: CreateXg = std::mem::transmute(create_addr);
            let mut xg = std::ptr::null_mut();
            assert!(create(xd.as_ptr(), &mut xg) >= 0 && !xg.is_null());
            type Size = unsafe extern "win64" fn(*mut c_void) -> u64;
            let size: Size = std::mem::transmute(slot(xg, 3));
            let mut title = vec![0u8; size(xg) as usize];
            type Into = unsafe extern "win64" fn(*mut c_void, *mut u8, u32, u32, *const u8, u32, u32) -> HRESULT;
            let into: Into = std::mem::transmute(slot(xg, 8));
            let dims = |mip: u32| ((80u32 >> mip).max(1), (40u32 >> mip).max(1));
            let texels = |sub: u32, salt: u8| -> Vec<u8> {
                let (w, h) = dims(sub % 2);
                (0..w * h * 4).map(|i| (i as u8).wrapping_mul(7).wrapping_add(sub as u8 * 31).wrapping_add(salt)).collect()
            };
            for sub in 0..4u32 {
                let (w, h) = dims(sub % 2);
                let t = texels(sub, 0);
                assert_eq!(into(xg, title.as_mut_ptr(), 0, sub, t.as_ptr(), w * 4, w * h * 4), 0, "into sub {sub}");
            }
            // device side (u64-backed: the HLE reads descs with aligned loads)
            let dev = device() as *mut c_void;
            let mut desc = [0u64; 7];
            let db = desc.as_mut_ptr() as *mut u8;
            (db as *mut u32).write_unaligned(3);
            (db.add(16) as *mut u64).write_unaligned(80);
            (db.add(24) as *mut u32).write_unaligned(40);
            (db.add(28) as *mut u16).write_unaligned(2);
            (db.add(30) as *mut u16).write_unaligned(2);
            (db.add(32) as *mut u32).write_unaligned(28);
            (db.add(44) as *mut u32).write_unaligned(0x119);
            type PlacedX = unsafe extern "win64" fn(*mut c_void, u64, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let placed: PlacedX = std::mem::transmute(slot(dev, 44));
            let mut raw = std::ptr::null_mut();
            assert_eq!(placed(dev, title.as_ptr() as u64, db as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut raw), S_OK);
            type Fp = unsafe extern "win64" fn(*mut c_void, *const u8, u32, u32, u64, *mut u8, *mut u32, *mut u64, *mut u64);
            let fp: Fp = std::mem::transmute(slot(dev, 38));
            let mut fps = [0u64; 16];
            let mut total = 0u64;
            fp(dev, db as *const u8, 0, 4, 0, fps.as_mut_ptr() as *mut u8, std::ptr::null_mut(), std::ptr::null_mut(), &mut total);
            let fpsb = std::slice::from_raw_parts(fps.as_ptr() as *const u8, 128);
            // each subresource gets its own offset and its mip dims
            let mut offs = [0u64; 4];
            for (i, b) in fpsb.chunks(32).enumerate().take(4) {
                offs[i] = u64::from_le_bytes(b[0..8].try_into().unwrap());
                let w = u32::from_le_bytes(b[12..16].try_into().unwrap());
                let h = u32::from_le_bytes(b[16..20].try_into().unwrap());
                let (ew, eh) = dims(i as u32 % 2);
                assert_eq!((w, h), (ew, eh), "fp[{i}] dims");
            }
            assert!(offs[0] < offs[1] && offs[1] < offs[2] && offs[2] < offs[3], "footprint offsets overlap: {offs:#x?}");
            // readback buffer + PC list; CopyTextureRegion is list slot 16
            let mut buf = [0u64; 7];
            let bb = buf.as_mut_ptr() as *mut u8;
            (bb as *mut u32).write_unaligned(1);
            (bb.add(16) as *mut u64).write_unaligned(total);
            let mut hp = [0u64; 3];
            let hb = hp.as_mut_ptr() as *mut u8;
            (hb as *mut u32).write_unaligned(3);
            type Mk = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: Mk = std::mem::transmute(slot(dev, 27));
            let mut rb = std::ptr::null_mut();
            assert_eq!(mk(dev, hb as *const c_void, 0, bb as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkl: MkList = std::mem::transmute(slot(dev, 12));
            let mut list = std::ptr::null_mut();
            assert_eq!(mkl(dev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut list), S_OK);
            type Copy = unsafe extern "win64" fn(*mut c_void, *const u8, u32, u32, u32, *const u8, *const u8);
            let copy: Copy = std::mem::transmute(slot(list, 16));
            type Map = unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let map: Map = std::mem::transmute(slot(rb, 8));
            let read_back = |salt: u8| {
                for (i, f) in fpsb.chunks(32).enumerate().take(4) {
                    let mut dst = [0u8; 48];
                    (dst.as_mut_ptr() as *mut usize).write_unaligned(rb as usize);
                    (dst.as_mut_ptr().add(8) as *mut u32).write_unaligned(1);
                    dst[16..48].copy_from_slice(&f[..32]);
                    let mut src = [0u8; 24];
                    (src.as_mut_ptr() as *mut usize).write_unaligned(raw as usize);
                    (src.as_mut_ptr().add(8) as *mut u32).write_unaligned(0);
                    (src.as_mut_ptr().add(16) as *mut u32).write_unaligned(i as u32);
                    copy(list, dst.as_ptr(), 0, 0, 0, src.as_ptr(), std::ptr::null());
                }
                let mut p = std::ptr::null_mut();
                assert_eq!(map(rb, 0, std::ptr::null(), &mut p), S_OK);
                for sub in 0..4u32 {
                    let (w, h) = dims(sub % 2);
                    let exp = texels(sub, if sub == 2 { salt } else { 0 });
                    let b = &fpsb[sub as usize * 32..];
                    let off = u64::from_le_bytes(b[0..8].try_into().unwrap()) as usize;
                    let rp = u32::from_le_bytes(b[24..28].try_into().unwrap()) as usize;
                    for y in 0..h as usize {
                        let got = std::slice::from_raw_parts((p as *const u8).add(off + y * rp), w as usize * 4);
                        assert_eq!(got, &exp[y * w as usize * 4..(y + 1) * w as usize * 4], "sub {sub} row {y}");
                    }
                }
            };
            read_back(0);
            // a rewritten subresource re-reads with the new texels
            let t2 = texels(2, 99);
            assert_eq!(into(xg, title.as_mut_ptr(), 0, 2, t2.as_ptr(), 80 * 4, 80 * 40 * 4), 0);
            read_back(99);
            type Release = unsafe extern "win64" fn(*mut c_void) -> u32;
            let release: Release = std::mem::transmute(slot(xg, 1));
            release(xg);
        }
    }

    /// M2 streaming leg (title `streamed_texture`): a texture placed at a
    /// reservation copies zeros while nothing is mapped there, then copies its
    /// texels once they are written — the copy runs at record time against the
    /// placed VA. Fails pre-slice: slot 16 is a stub, so the texels never land.
    #[test]
    fn streamed_texture_copy_sees_texels_written_after_placement() {
        unsafe {
            // XG desc (0x38 B): the title's 64x64 4KB_S streaming shape.
            let mut xd = [0u8; 0x38];
            (xd.as_mut_ptr() as *mut u32).write_unaligned(3);
            (xd.as_mut_ptr().add(0x10) as *mut u64).write_unaligned(64);
            (xd.as_mut_ptr().add(0x18) as *mut u32).write_unaligned(64);
            (xd.as_mut_ptr().add(0x1C) as *mut u16).write_unaligned(1);
            (xd.as_mut_ptr().add(0x1E) as *mut u16).write_unaligned(1);
            (xd.as_mut_ptr().add(0x20) as *mut u32).write_unaligned(28);
            (xd.as_mut_ptr().add(0x2C) as *mut u32).write_unaligned(0x105);
            let create_addr = crate::hle::xg_linux::exports()
                .into_iter()
                .find(|(n, _)| *n == "XGCreateTextureComputer")
                .map(|(_, a)| a)
                .unwrap();
            type CreateXg = unsafe extern "win64" fn(*const u8, *mut *mut c_void) -> HRESULT;
            let create: CreateXg = std::mem::transmute(create_addr);
            let mut xg = std::ptr::null_mut();
            assert!(create(xd.as_ptr(), &mut xg) >= 0 && !xg.is_null());
            type Size = unsafe extern "win64" fn(*mut c_void) -> u64;
            let size: Size = std::mem::transmute(slot(xg, 3));
            // the reservation: address space with nothing mapped yet
            let mut title = vec![0u8; size(xg) as usize];
            let dev = device() as *mut c_void;
            let mut desc = [0u64; 7];
            let db = desc.as_mut_ptr() as *mut u8;
            (db as *mut u32).write_unaligned(3);
            (db.add(16) as *mut u64).write_unaligned(64);
            (db.add(24) as *mut u32).write_unaligned(64);
            (db.add(28) as *mut u16).write_unaligned(1);
            (db.add(30) as *mut u16).write_unaligned(1);
            (db.add(32) as *mut u32).write_unaligned(28);
            (db.add(44) as *mut u32).write_unaligned(0x105);
            type PlacedX = unsafe extern "win64" fn(*mut c_void, u64, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let placed: PlacedX = std::mem::transmute(slot(dev, 44));
            let mut raw = std::ptr::null_mut();
            assert_eq!(placed(dev, title.as_ptr() as u64, db as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut raw), S_OK);
            type Fp = unsafe extern "win64" fn(*mut c_void, *const u8, u32, u32, u64, *mut u8, *mut u32, *mut u64, *mut u64);
            let fp: Fp = std::mem::transmute(slot(dev, 38));
            let mut fpb = [0u64; 4];
            let mut total = 0u64;
            fp(dev, db as *const u8, 0, 1, 0, fpb.as_mut_ptr() as *mut u8, std::ptr::null_mut(), std::ptr::null_mut(), &mut total);
            let mut buf = [0u64; 7];
            let bbp = buf.as_mut_ptr() as *mut u8;
            (bbp as *mut u32).write_unaligned(1);
            (bbp.add(16) as *mut u64).write_unaligned(total);
            let mut hp = [0u64; 3];
            let hbp = hp.as_mut_ptr() as *mut u8;
            (hbp as *mut u32).write_unaligned(3);
            type Mk = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: Mk = std::mem::transmute(slot(dev, 27));
            let mut rb = std::ptr::null_mut();
            assert_eq!(mk(dev, hbp as *const c_void, 0, bbp as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkl: MkList = std::mem::transmute(slot(dev, 12));
            let mut list = std::ptr::null_mut();
            assert_eq!(mkl(dev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut list), S_OK);
            type Copy = unsafe extern "win64" fn(*mut c_void, *const u8, u32, u32, u32, *const u8, *const u8);
            let copy: Copy = std::mem::transmute(slot(list, 16));
            type Map = unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let map: Map = std::mem::transmute(slot(rb, 8));
            let fpbytes = std::slice::from_raw_parts(fpb.as_ptr() as *const u8, 32);
            let mut dst = [0u8; 48];
            (dst.as_mut_ptr() as *mut usize).write_unaligned(rb as usize);
            (dst.as_mut_ptr().add(8) as *mut u32).write_unaligned(1);
            dst[16..48].copy_from_slice(&fpbytes[..32]);
            let mut src = [0u8; 24];
            (src.as_mut_ptr() as *mut usize).write_unaligned(raw as usize);
            (src.as_mut_ptr().add(8) as *mut u32).write_unaligned(0);
            (src.as_mut_ptr().add(16) as *mut u32).write_unaligned(0);
            // nothing mapped yet: the copy reads zeros
            copy(list, dst.as_ptr(), 0, 0, 0, src.as_ptr(), std::ptr::null());
            let mut p = std::ptr::null_mut();
            assert_eq!(map(rb, 0, std::ptr::null(), &mut p), S_OK);
            let off = u64::from_le_bytes(fpbytes[0..8].try_into().unwrap()) as usize;
            let rp = u32::from_le_bytes(fpbytes[24..28].try_into().unwrap()) as usize;
            for y in 0..64usize {
                assert!(std::slice::from_raw_parts((p as *const u8).add(off + y * rp), 256).iter().all(|&b| b == 0), "row {y} not zero before map-in");
            }
            // pages mapped in and texels written: the next copy sees them
            let texels: Vec<u8> = (0..64 * 64 * 4).map(|i: u32| (i as u8).wrapping_mul(13) ^ 0x5A).collect();
            type Into = unsafe extern "win64" fn(*mut c_void, *mut u8, u32, u32, *const u8, u32, u32) -> HRESULT;
            let into: Into = std::mem::transmute(slot(xg, 8));
            assert_eq!(into(xg, title.as_mut_ptr(), 0, 0, texels.as_ptr(), 256, 256 * 64), 0);
            copy(list, dst.as_ptr(), 0, 0, 0, src.as_ptr(), std::ptr::null());
            assert_eq!(map(rb, 0, std::ptr::null(), &mut p), S_OK);
            for y in 0..64usize {
                assert_eq!(std::slice::from_raw_parts((p as *const u8).add(off + y * rp), 256), &texels[y * 256..(y + 1) * 256], "row {y}");
            }
            type Release = unsafe extern "win64" fn(*mut c_void) -> u32;
            let release: Release = std::mem::transmute(slot(xg, 1));
            release(xg);
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

    #[test]
    fn null_check_feature_support_zeroes_options5_tail() {
        // #10 XS slice: the null device's OPTIONS5 path must zero the tail
        // past the 12-byte struct like the Vulkan one does — the title
        // probes with a 16-byte struct filled with `0xAA`.
        unsafe {
            let this = std::ptr::null_mut();
            let mut wide = [0xAAu8; 16];
            assert_eq!(
                Device_CheckFeatureSupport(this, 27, wide.as_mut_ptr() as *mut c_void, 16),
                S_OK
            );
            assert_eq!(&wide[12..16], &[0, 0, 0, 0]);
            // other features / short structs pass through untouched
            let mut narrow = [0xAAu8; 8];
            assert_eq!(
                Device_CheckFeatureSupport(this, 7, narrow.as_mut_ptr() as *mut c_void, 8),
                S_OK
            );
            assert_eq!(&narrow, &[0xAA; 8]);
        }
    }

    #[test]
    fn null_device_serves_the_raytracing_surface() {
        // M2 next slice (failing-first): the null device's AS SRV view +
        // null list #146/#148 serve the title's `raytracing_surface` legs
        // 2-3 with null-consistent host-memory values (the same words the
        // Vulkan path writes — no GPU needed).
        unsafe {
            let dev = device() as *mut c_void;
            type MkHeap = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkheap: MkHeap = std::mem::transmute(slot(dev, 14));
            let mut heap = std::ptr::null_mut();
            let hdesc = [6u32, 2, 1, 0]; // CBV_SRV_UAV, 2 slots, visible
            assert_eq!(mkheap(dev, hdesc.as_ptr() as *const c_void, std::ptr::null(), &mut heap), S_OK);
            type Start = unsafe extern "win64" fn(*mut c_void) -> usize;
            let start: Start = std::mem::transmute(slot(heap, 9));
            let cpu = start(heap);
            type Srv = unsafe extern "win64" fn(*mut c_void, *mut c_void, *const u8, usize);
            let srv: Srv = std::mem::transmute(slot(dev, 18));
            let va = 0x1_2345_6780u64;
            let mut desc = [0u8; 24];
            desc[4..8].copy_from_slice(&11u32.to_le_bytes());
            desc[16..24].copy_from_slice(&va.to_le_bytes());
            srv(dev, std::ptr::null_mut(), desc.as_ptr(), cpu);
            assert_eq!(
                std::slice::from_raw_parts(cpu as *const u32, 4),
                &[(va >> 8) as u32, (((va >> 40) as u32) & 0xFF) | (1 << 31), u32::MAX, 0]
            );
            // default title list: #148 upload memory + #146 barrier
            type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: MkList = std::mem::transmute(slot(dev, 12));
            let mut l = std::ptr::null_mut();
            assert_eq!(mk(dev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type AllocUp = unsafe extern "win64" fn(*mut c_void, usize, usize) -> *mut u8;
            let alloc_up: AllocUp = std::mem::transmute(slot(l, 148));
            let up = alloc_up(l, 256, 256);
            assert!(!up.is_null() && (up as usize).is_multiple_of(256));
            *(up as *mut u32) = 0x5A5A_1234;
            assert_eq!(*(up as *const u32), 0x5A5A_1234);
            type Barrier = unsafe extern "win64" fn(*mut c_void, u32, *const c_void);
            let barrier: Barrier = std::mem::transmute(slot(l, 146));
            barrier(l, 1, std::ptr::null());
        }
    }

    #[test]
    fn vulkan_raytracing_helpers_are_pure_and_safe() {
        // Step-17 raytracing slice, pure logic (no device needed): the
        // oversized OPTIONS5 probe gets its tail zeroed, an RTAS view writes
        // the descriptor words the title asserts, other views stay untouched,
        // nulls never fault, and upload memory comes back aligned + zeroed.
        unsafe {
            let this = std::ptr::null_mut();
            // OPTIONS5 tail: 16-byte 0xAA probe, tier at +8, zeros at 12..16
            let mut wide = [0xAAu8; 16];
            assert_eq!(
                Vulkan_CheckFeatureSupport(this, 27, wide.as_mut_ptr() as *mut c_void, 16),
                S_OK
            );
            assert_eq!(&wide[12..16], &[0, 0, 0, 0]);
            // other features / short structs pass through untouched
            let mut narrow = [0xAAu8; 8];
            assert_eq!(
                Vulkan_CheckFeatureSupport(this, 7, narrow.as_mut_ptr() as *mut c_void, 8),
                S_OK
            );
            assert_eq!(&narrow, &[0xAA; 8]);
            // RTAS view descriptor words
            let va = 0x5_6789_0ABC_DEF0u64;
            let mut desc = [0u8; 24];
            desc[4..8].copy_from_slice(&11u32.to_le_bytes());
            desc[16..24].copy_from_slice(&va.to_le_bytes());
            let mut slot = [0u32; 4];
            Vulkan_CreateShaderResourceView(this, std::ptr::null_mut(), desc.as_ptr(), slot.as_mut_ptr() as usize);
            assert_eq!(
                slot,
                [(va >> 8) as u32, (((va >> 40) as u32) & 0xFF) | (1 << 31), u32::MAX, 0]
            );
            // another dimension leaves the slot alone; nulls never fault
            desc[4..8].copy_from_slice(&4u32.to_le_bytes());
            slot = [0xDEAD_BEEF; 4];
            Vulkan_CreateShaderResourceView(this, std::ptr::null_mut(), desc.as_ptr(), slot.as_mut_ptr() as usize);
            assert_eq!(slot, [0xDEAD_BEEF; 4]);
            Vulkan_CreateShaderResourceView(this, std::ptr::null_mut(), std::ptr::null(), slot.as_mut_ptr() as usize);
            Vulkan_CreateShaderResourceView(this, std::ptr::null_mut(), desc.as_ptr(), 0);
            // upload memory: aligned, zeroed, remembered in the list object
            let o = new_obj(Kind::VulkanList);
            let p = VulList_AllocUpload(o, 256, 256);
            assert!(!p.is_null() && (p as usize).is_multiple_of(256));
            assert_eq!(std::slice::from_raw_parts(p, 256), &[0u8; 256]);
            assert_eq!(*(*o).mem.lock().unwrap(), (p as usize, 256));
            assert!(VulList_AllocUpload(o, 0, 256).is_null());
        }
    }

    #[test]
    fn vulkan_device_serves_the_raytracing_surface() {
        // Step-17 wiring on a real Vulkan device: CheckFeatureSupport zeroes
        // the oversized OPTIONS5 tail, the AS SRV lands in the heap slot, and
        // the list's AllocateUploadMemory + UAV barrier behave — the title's
        // `raytracing_surface` asserts, end to end. Skips without a device.
        unsafe {
            match acquire() {
                Some(st) => {
                    let sel = Selection { backend: Backend::Vulkan, lane: None, reason: "test".into() };
                    let mut dev = std::ptr::null_mut();
                    assert_eq!(create_device_for(&sel, Some(st), &mut dev), S_OK);
                    type Check = unsafe extern "win64" fn(*mut c_void, u32, *mut c_void, u32) -> HRESULT;
                    let check: Check = std::mem::transmute(slot(dev, 13));
                    let mut wide = [0xAAu8; 16];
                    assert_eq!(check(dev, 27, wide.as_mut_ptr() as *mut c_void, 16), S_OK);
                    assert_eq!(&wide[12..16], &[0, 0, 0, 0]);
                    // SRV heap + RTAS view (slot 18 takes the handle by value)
                    type MkHeap = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
                    let mkheap: MkHeap = std::mem::transmute(slot(dev, 14));
                    let mut heap = std::ptr::null_mut();
                    let hdesc = [6u32, 2, 1, 0]; // CBV_SRV_UAV, 2 slots, visible
                    assert_eq!(mkheap(dev, hdesc.as_ptr() as *const c_void, std::ptr::null(), &mut heap), S_OK);
                    type Start = unsafe extern "win64" fn(*mut c_void) -> usize;
                    let start: Start = std::mem::transmute(slot(heap, 9));
                    let cpu = start(heap);
                    type Srv = unsafe extern "win64" fn(*mut c_void, *mut c_void, *const u8, usize);
                    let srv: Srv = std::mem::transmute(slot(dev, 18));
                    let va = 0x1_2345_6780u64;
                    let mut desc = [0u8; 24];
                    desc[4..8].copy_from_slice(&11u32.to_le_bytes());
                    desc[16..24].copy_from_slice(&va.to_le_bytes());
                    srv(dev, std::ptr::null_mut(), desc.as_ptr(), cpu);
                    assert_eq!(
                        std::slice::from_raw_parts(cpu as *const u32, 4),
                        &[(va >> 8) as u32, (((va >> 40) as u32) & 0xFF) | (1 << 31), u32::MAX, 0]
                    );
                    // list upload memory + barrier (slots 148 / 146)
                    type MkList = unsafe extern "win64" fn(*mut c_void, u32, u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
                    let mk: MkList = std::mem::transmute(slot(dev, 12));
                    let mut l = std::ptr::null_mut();
                    assert_eq!(mk(dev, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
                    type AllocUp = unsafe extern "win64" fn(*mut c_void, usize, usize) -> *mut u8;
                    let alloc_up: AllocUp = std::mem::transmute(slot(l, 148));
                    let up = alloc_up(l, 256, 256);
                    assert!(!up.is_null() && (up as usize).is_multiple_of(256));
                    *(up as *mut u32) = 0x5A5A_1234;
                    assert_eq!(*(up as *const u32), 0x5A5A_1234);
                    type Barrier = unsafe extern "win64" fn(*mut c_void, u32, *const c_void);
                    let barrier: Barrier = std::mem::transmute(slot(l, 146));
                    barrier(l, 1, std::ptr::null());
                }
                None => crate::gpu::recomp::vulkan_runner::skip_test("no Vulkan"),
            }
        }
    }

    #[test]
    fn vulkan_frame_clock_and_presentx_route_behind_the_backend() {
        // 1d failing-first: device #78-81 frame clock + queue #30 PresentX route
        // on Vulkan objects; the null device keeps logging stubs (no default change).
        // No real GPU needed: the slots never touch the VulkanState pointer.
        // The clock is one per process: take turns with the twin's own tests.
        let _serial = crate::hle::d3d12x_frame_linux::CLOCK_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut ndev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut ndev), S_OK);
            for i in [78usize, 79, 80, 81] {
                let name = crate::stubs::info_of_stub(slot(ndev, i)).map(|s| s.name.as_str()).unwrap_or("");
                assert_eq!(name, format!("ID3D12Device::#{i}"), "null slot {i} stays a stub");
            }
            let vd = new_obj(Kind::VulkanDevice);
            let vdp = vd as *mut c_void;
            type SetIv = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32, u32) -> HRESULT;
            let set: SetIv = std::mem::transmute(slot(vdp, 78));
            assert_eq!(set(vdp, std::ptr::null_mut(), 16667, 1, 0), S_FALSE);
            type Sched = unsafe extern "win64" fn(*mut c_void, u32, u32, *const u8, u32) -> HRESULT;
            let sched: Sched = std::mem::transmute(slot(vdp, 79));
            let hr = sched(vdp, 0, 0, std::ptr::null(), 0);
            assert!(hr == S_FALSE || hr == 0x1000_0000, "{hr:#x}");
            type Wait = unsafe extern "win64" fn(*mut c_void, u32, u32, *const u8, u32, *mut u64) -> HRESULT;
            let wait: Wait = std::mem::transmute(slot(vdp, 80));
            let mut token = 0u64;
            let t0 = std::time::Instant::now();
            let mut hr = S_FALSE;
            while t0.elapsed() < std::time::Duration::from_secs(5) {
                hr = wait(vdp, 0, 200, std::ptr::null(), 0, &mut token);
                if hr == S_OK {
                    break;
                }
                let _ = sched(vdp, 0, 0, std::ptr::null(), 0);
            }
            assert_eq!(hr, S_OK);
            assert_ne!(token, 0);
            // ancillary routing: a title event registered at schedule fires with type 1
            let ev = crate::hle::kernelx_linux::CreateEventA(std::ptr::null(), 0, 0, std::ptr::null());
            assert_ne!(ev, 0);
            let list = [ev];
            let mut signaled = -1i32;
            let mut block = [0u8; 0x18];
            block[..4].copy_from_slice(&1u32.to_le_bytes());
            block[8..16].copy_from_slice(&(list.as_ptr() as usize).to_le_bytes());
            block[16..24].copy_from_slice(&(&mut signaled as *mut i32 as usize).to_le_bytes());
            let hr = sched(vdp, 1, 0, block.as_ptr(), 0);
            assert!(hr == S_FALSE || hr == 0x1000_0000, "{hr:#x}");
            let mut t1 = 0u64;
            assert_eq!(wait(vdp, 1, 500, std::ptr::null(), 0, &mut t1), S_OK);
            assert_eq!(crate::hle::kernelx_linux::WaitForSingleObject(ev, 0), 0);
            let _ = sched(vdp, 1, 0, std::ptr::null(), 1); // unschedule
            // queue #30 PresentX (1d(ii)): a queue from the Vulkan device carries
            // it, the frame's bytes present (or skip headless), #81 names the flip
            type MkQ = unsafe extern "win64" fn(*mut c_void, *const u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkq: MkQ = std::mem::transmute(slot(vdp, 48));
            let mut q = std::ptr::null_mut();
            assert_eq!(mkq(vdp, [0x10u32, 0, 0, 0].as_ptr(), std::ptr::null(), &mut q), S_OK);
            assert_eq!((*(q as *mut NullObj)).kind, Kind::VulkanQueue);
            // 4x4 RGBA back buffer with host bytes
            let mut desc = [0u8; 56];
            desc[0] = 3; // TEXTURE2D
            desc[16..24].copy_from_slice(&4u64.to_le_bytes());
            desc[24] = 4;
            desc[32..36].copy_from_slice(&28u32.to_le_bytes()); // R8G8B8A8_UNORM
            type MkRes = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkr: MkRes = std::mem::transmute(slot(vdp, 27));
            let mut rb = std::ptr::null_mut();
            assert_eq!(mkr(vdp, std::ptr::null(), 0, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            let map: unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT = std::mem::transmute(slot(rb, 8));
            let mut p = std::ptr::null_mut();
            assert_eq!(map(rb, 0, std::ptr::null(), &mut p), S_OK);
            std::slice::from_raw_parts_mut(p as *mut u8, 64).fill(0x7F);
            let resources = [rb];
            let mut plane = [0u8; 0x18];
            plane[..8].copy_from_slice(&token.to_le_bytes());
            plane[8..12].copy_from_slice(&1u32.to_le_bytes());
            plane[0x10..0x18].copy_from_slice(&(resources.as_ptr() as u64).to_le_bytes());
            type Present = unsafe extern "win64" fn(*mut c_void, u32, *const u8, *const u8) -> HRESULT;
            let present: Present = std::mem::transmute(slot(q, 30));
            assert_eq!(present(q, 1, plane.as_ptr(), std::ptr::null()), S_OK);
            // ... and the flip shows up in #81 statistics
            type Stats = unsafe extern "win64" fn(*mut c_void, u64, u32, *mut u32, *mut u8) -> HRESULT;
            let stats: Stats = std::mem::transmute(slot(vdp, 81));
            let (mut n, mut st) = (1u32, [0u8; 0x40]);
            assert_eq!(stats(vdp, token, 0, &mut n, st.as_mut_ptr()), S_OK);
            assert_eq!(n, 1);
            let (origin, flip) = (
                i64::from_le_bytes(st[0x10..0x18].try_into().unwrap()),
                i64::from_le_bytes(st[0x18..0x20].try_into().unwrap()),
            );
            assert!(origin > 0 && origin <= flip);
            assert_eq!(set(vdp, std::ptr::null_mut(), 0, 0, 0), S_FALSE);
        }
    }

    #[test]
    fn vulkan_dxgi_chain_serves_adapter_and_output() {
        // 1d follow-up: a Vulkan device answers the DXGI device IIDs with a
        // wrapper whose GetAdapter/GetDesc/EnumOutputs chain works (adapter
        // LUID == device LUID, D3D12 identity round-trips, output 0 exists);
        // the null device keeps answering with itself (no change unset).
        unsafe {
            type Qi = unsafe extern "win64" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut ndev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut ndev), S_OK);
            let nqi: Qi = std::mem::transmute(slot(ndev, 0));
            let mut p = std::ptr::null_mut();
            assert_eq!(nqi(ndev, IID_DXGI_DEVICE1.as_ptr() as *const c_void, &mut p), S_OK);
            assert_eq!(p, ndev, "null QI keeps handing back the device itself");
            // Vulkan device: DXGI queries get a wrapper, not the device.
            let vd = new_obj(Kind::VulkanDevice) as *mut c_void;
            let vqi: Qi = std::mem::transmute(slot(vd, 0));
            let mut dx = std::ptr::null_mut();
            assert_eq!(vqi(vd, IID_DXGI_DEVICE1.as_ptr() as *const c_void, &mut dx), S_OK);
            assert_eq!((*(dx as *mut NullObj)).kind, Kind::DxgiDevice);
            // the D3D12 identity survives the round trip the title asserts
            let dqi: Qi = std::mem::transmute(slot(dx, 0));
            let mut back = std::ptr::null_mut();
            assert_eq!(dqi(dx, IID_D3D12DEVICE.as_ptr() as *const c_void, &mut back), S_OK);
            assert_eq!(back, vd);
            // GetAdapter (slot 7) hands out an adapter ...
            type GetAd = unsafe extern "win64" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
            let get_ad: GetAd = std::mem::transmute(slot(dx, 7));
            let mut ad = std::ptr::null_mut();
            assert_eq!(get_ad(dx, &mut ad), S_OK);
            assert_eq!((*(ad as *mut NullObj)).kind, Kind::DxgiAdapter);
            // ... whose GetDesc (slot 8) carries the device's LUID
            type AdDesc = unsafe extern "win64" fn(*mut c_void, *mut u8) -> HRESULT;
            let get_desc: AdDesc = std::mem::transmute(slot(ad, 8));
            let mut desc = [0u8; 304];
            assert_eq!(get_desc(ad, desc.as_mut_ptr()), S_OK);
            assert_ne!(u16::from_le_bytes([desc[0], desc[1]]), 0, "description is named");
            let mut luid = 0u64;
            Device_GetAdapterLuid(vd as *mut NullObj, &mut luid);
            assert_eq!(&desc[296..304], &luid.to_le_bytes(), "adapter LUID == device LUID");
            // ... and EnumOutputs (slot 7) serves a real output 0, nothing else
            type EnumOut = unsafe extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
            let outputs: EnumOut = std::mem::transmute(slot(ad, 7));
            let mut o = std::ptr::null_mut();
            assert_eq!(outputs(ad, 0, &mut o), S_OK);
            assert_eq!((*(o as *mut NullObj)).kind, Kind::DxgiOutput);
            let mut o1 = std::ptr::null_mut();
            assert_eq!(outputs(ad, 1, &mut o1), 0x887A_0002u32 as i32);
            assert!(o1.is_null());
            // unknown interfaces stay unknown, not the device
            let aqi: Qi = std::mem::transmute(slot(ad, 0));
            let mut nope = std::ptr::null_mut();
            assert_eq!(aqi(ad, [0u8; 16].as_ptr() as *const c_void, &mut nope), E_NOINTERFACE);
            assert!(nope.is_null());
            // the chain lands on the routed slots, not logging stubs
            for i in [78usize, 79, 80, 81] {
                assert!(crate::stubs::info_of_stub(slot(vd, i)).is_none(), "slot {i} routes");
            }
        }
    }

    /// M2 draw S1 failing-first: the PSO's draw bytes (VS/PS/RTV/DSV +
    /// rasterizer/blend, from #60/#55 + PC10) land in the list-bound record
    /// at SetPipelineState, before any draw is issued. Fails pre-S1: no
    /// record store exists, so this does not compile, let alone pass.
    #[test]
    fn draw_s1_pso_record_populated_pre_draw() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            // shader bytecodes the desc points at (must stay readable memory)
            let vs = vec![0x11u8; 64];
            let ps = vec![0x22u8; 64];
            let mut desc = vec![0u8; super::GFX_DESC_MIN];
            desc[8..16].copy_from_slice(&(vs.as_ptr() as usize).to_le_bytes());
            desc[16..24].copy_from_slice(&(vs.len()).to_le_bytes());
            desc[24..32].copy_from_slice(&(ps.as_ptr() as usize).to_le_bytes());
            desc[32..40].copy_from_slice(&(ps.len()).to_le_bytes());
            for (i, b) in desc[super::GFX_BLEND_OFF..super::GFX_BLEND_OFF + super::GFX_BLEND_LEN].iter_mut().enumerate() {
                *b = (i & 0xFF) as u8;
            }
            desc[super::GFX_RASTER_OFF..super::GFX_RASTER_OFF + super::GFX_RASTER_LEN].fill(0x2A);
            desc[super::GFX_NUMRT_OFF..super::GFX_NUMRT_OFF + 4].copy_from_slice(&2u32.to_le_bytes());
            desc[super::GFX_RTV_OFF..super::GFX_RTV_OFF + 4].copy_from_slice(&28u32.to_le_bytes());
            desc[super::GFX_RTV_OFF + 4..super::GFX_RTV_OFF + 8].copy_from_slice(&28u32.to_le_bytes());
            desc[super::GFX_DSV_OFF..super::GFX_DSV_OFF + 4].copy_from_slice(&45u32.to_le_bytes());
            // console PSO (#60) records + accepts
            type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let pso_x: PsoX = std::mem::transmute(slot(dev, 60));
            let mut pso = std::ptr::null_mut();
            assert_eq!(pso_x(dev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
            let rec = super::pso_record_for(pso as usize).expect("S1 records the PSO at creation");
            assert_eq!((rec.vs_ptr, rec.vs_len), (vs.as_ptr() as usize, vs.len()));
            assert_eq!((rec.ps_ptr, rec.ps_len), (ps.as_ptr() as usize, ps.len()));
            assert_eq!(rec.blend.len(), super::GFX_BLEND_LEN);
            assert_eq!(rec.blend[7], 7);
            assert!(rec.raster.iter().all(|&b| b == 0x2A));
            assert_eq!((rec.num_rtv, rec.rtv_formats[0], rec.rtv_formats[1], rec.dsv_format), (2, 28, 28, 45));
            // PC PSO (#10) records too
            let pso_pc: Fn4 = std::mem::transmute(slot(dev, 10));
            let mut pso2 = std::ptr::null_mut();
            assert_eq!(pso_pc(dev, desc.as_ptr() as *const c_void, std::ptr::null(), &mut pso2), S_OK);
            assert!(super::pso_record_for(pso2 as usize).is_some(), "PC10 records as well");
            // derived blend PSO (#55) inherits the base record
            type DerivedX = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, *const u8, *const c_void, *mut *mut c_void) -> HRESULT;
            let derive: DerivedX = std::mem::transmute(slot(dev, 55));
            let mut d = std::ptr::null_mut();
            assert_eq!(derive(dev, pso, 1, [2u8].as_ptr(), std::ptr::null(), &mut d), S_OK);
            let drec = super::pso_record_for(d as usize).expect("derived PSO carries the base record");
            assert_eq!(drec.vs_ptr, vs.as_ptr() as usize);
            // list SetPipelineState (#25) binds the record pre-draw: no draw issued
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type SetPso = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            let set: SetPso = std::mem::transmute(slot(l, 25));
            set(l, pso);
            let bound = super::list_record_for(l as usize).expect("list record bound at SetPipelineState");
            assert_eq!(bound.pso, pso as usize);
            assert_eq!((bound.pso_info.vs_ptr, bound.pso_info.dsv_format), (vs.as_ptr() as usize, 45));
            assert_eq!(bound.pso_info.blend[7], 7);
        }
    }

    /// M2 draw S2 failing-first: list-bound state assembly — root sig (#34),
    /// render targets (#50), index buffer (#47), topology/viewports/scissors
    /// (PC 20/21/22) — lands in the list record with no draw issued. Fails
    /// pre-S2: the record has no S2 fields and the slots are unwired.
    #[test]
    fn draw_s2_list_state_recorded_pre_draw() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            // root signature first: SetPipelineState must not clobber it
            // (store-only: a dummy non-null handle suffices, no object needed)
            let rs = 0x1234_5000usize as *mut c_void;
            type SetRs = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            let set_rs: SetRs = std::mem::transmute(slot(l, 34));
            set_rs(l, rs);
            // PSO bind after root sig keeps both
            let mut desc = vec![0u8; super::GFX_DESC_MIN];
            type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let pso_x: PsoX = std::mem::transmute(slot(dev, 60));
            let mut pso = std::ptr::null_mut();
            assert_eq!(pso_x(dev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
            type SetPso = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            let set_pso: SetPso = std::mem::transmute(slot(l, 25));
            set_pso(l, pso);
            // render targets (#50): 2 RTVs + DSV
            let rtvs = [0x1000usize, 0x2000usize];
            let dsv = 0x3000usize;
            type OmRt = unsafe extern "win64" fn(*mut c_void, u32, *const usize, u32, *const usize);
            let om: OmRt = std::mem::transmute(slot(l, 50));
            om(l, 2, rtvs.as_ptr(), 0, &dsv);
            // index buffer (#47): va + size + format
            let mut view = [0u8; 16];
            view[0..8].copy_from_slice(&0x9_0000u64.to_le_bytes());
            view[8..12].copy_from_slice(&1024u32.to_le_bytes());
            view[12..16].copy_from_slice(&44u32.to_le_bytes());
            type Ib = unsafe extern "win64" fn(*mut c_void, *const u8);
            let ib: Ib = std::mem::transmute(slot(l, 47));
            ib(l, view.as_ptr());
            // PC-side: topology (20), one viewport (21), one scissor (22)
            type Topo = unsafe extern "win64" fn(*mut c_void, u32);
            let topo: Topo = std::mem::transmute(slot(l, 20));
            topo(l, 4);
            let mut vp = [0u8; 24];
            vp[8..12].copy_from_slice(&1280.0f32.to_le_bytes());
            vp[12..16].copy_from_slice(&720.0f32.to_le_bytes());
            type Vp = unsafe extern "win64" fn(*mut c_void, u32, *const u8);
            let set_vp: Vp = std::mem::transmute(slot(l, 21));
            set_vp(l, 1, vp.as_ptr());
            let mut sc = [0u8; 16];
            sc[8..12].copy_from_slice(&1280i32.to_le_bytes());
            sc[12..16].copy_from_slice(&720i32.to_le_bytes());
            type Sc = unsafe extern "win64" fn(*mut c_void, u32, *const u8);
            let set_sc: Sc = std::mem::transmute(slot(l, 22));
            set_sc(l, 1, sc.as_ptr());
            // no draw issued; every stored field reads back
            let r = super::list_record_for(l as usize).expect("S2 stores list state pre-draw");
            assert_eq!((r.root_sig, r.pso), (rs as usize, pso as usize));
            assert_eq!((r.rtv_count, r.rtv_handles[0], r.rtv_handles[1]), (2, 0x1000, 0x2000));
            assert!((r.has_dsv, r.dsv_handle) == (true, 0x3000));
            assert!((r.has_index, r.index_va, r.index_size, r.index_format) == (true, 0x9_0000, 1024, 44));
            assert_eq!(r.topology, 4);
            assert_eq!(r.viewports.len(), 24);
            assert_eq!(&r.viewports[8..16], &vp[8..16]);
            assert_eq!(r.scissors.len(), 16);
            assert_eq!(&r.scissors[8..16], &sc[8..16]);
        }
    }

    /// M2 draw S3 failing-first: graphics pipeline + render pass build from the
    /// stashed record's VS bytes (offscreen, no execute). Garbage (PC
    /// DXBC-class) bytes fall back to the title triangle VS; PS is always
    /// const-green. Fails pre-S3: the builder symbols are absent.
    #[test]
    fn draw_s3_record_pipeline_builds_pre_execute() {
        use crate::gpu::recomp::vulkan_graphics::{build_record_pipeline, green_fs_spv};
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            // non-blit VS bytes the backend rejects (kept alive past the build)
            let vs = vec![0xAAu8; 64];
            let ps = vec![0xBBu8; 64];
            let mut desc = vec![0u8; super::GFX_DESC_MIN];
            desc[8..16].copy_from_slice(&(vs.as_ptr() as usize).to_le_bytes());
            desc[16..24].copy_from_slice(&(vs.len()).to_le_bytes());
            desc[24..32].copy_from_slice(&(ps.as_ptr() as usize).to_le_bytes());
            desc[32..40].copy_from_slice(&(ps.len()).to_le_bytes());
            desc[super::GFX_NUMRT_OFF..super::GFX_NUMRT_OFF + 4].copy_from_slice(&1u32.to_le_bytes());
            desc[super::GFX_RTV_OFF..super::GFX_RTV_OFF + 4].copy_from_slice(&28u32.to_le_bytes());
            type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let pso_x: PsoX = std::mem::transmute(slot(dev, 60));
            let mut pso = std::ptr::null_mut();
            assert_eq!(pso_x(dev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type SetPso = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            let set: SetPso = std::mem::transmute(slot(l, 25));
            set(l, pso);
            let r = super::list_record_for(l as usize).expect("S3 builds from the stashed record");
            let bytes = std::slice::from_raw_parts(r.pso_info.vs_ptr as *const u8, r.pso_info.vs_len);
            let sum = match build_record_pipeline(bytes, r.pso_info.num_rtv) {
                Ok(s) => s,
                Err(e) if e.starts_with("no ") => {
                    crate::gpu::recomp::vulkan_runner::skip_test("no GPU");
                    return;
                }
                Err(e) => panic!("build_record_pipeline failed: {e}"),
            };
            assert!(sum.vs_fallback, "garbage VS bytes must take the triangle fallback");
            assert_eq!(sum.fs_words as usize, green_fs_spv().len(), "PS is always const-green");
            assert_eq!((sum.rtv_count, r.pso_info.num_rtv), (1, 1));
        }
    }

    /// M2 draw S4 failing-first: Close marks the list and ExecuteCommandLists
    /// flushes the S1–S3 record — null queues assert the record (no pixels),
    /// Vulkan queues run the S3 pipeline build plus a real offscreen submit
    /// (pixels stashed for the S5 readback). Fails pre-S4: queue #10 is a
    /// no-op, so the flush count never moves and no pixels land.
    #[test]
    fn draw_s4_execute_flushes_record_offscreen() {
        unsafe {
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut dev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut dev), S_OK);
            let vs = vec![0xAAu8; 64];
            let ps = vec![0xBBu8; 64];
            let mut desc = vec![0u8; super::GFX_DESC_MIN];
            desc[8..16].copy_from_slice(&(vs.as_ptr() as usize).to_le_bytes());
            desc[16..24].copy_from_slice(&(vs.len()).to_le_bytes());
            desc[24..32].copy_from_slice(&(ps.as_ptr() as usize).to_le_bytes());
            desc[32..40].copy_from_slice(&(ps.len()).to_le_bytes());
            desc[super::GFX_NUMRT_OFF..super::GFX_NUMRT_OFF + 4].copy_from_slice(&1u32.to_le_bytes());
            type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let pso_x: PsoX = std::mem::transmute(slot(dev, 60));
            let mut pso = std::ptr::null_mut();
            assert_eq!(pso_x(dev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
            type MkQueue = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkq: MkQueue = std::mem::transmute(slot(dev, 8));
            let mut q = std::ptr::null_mut();
            assert_eq!(mkq(dev, std::ptr::null(), std::ptr::null(), &mut q), S_OK);
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type SetPso = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            let set: SetPso = std::mem::transmute(slot(l, 25));
            set(l, pso);
            // Close (slot 9) marks the list closed.
            type Close = unsafe extern "win64" fn(*mut c_void) -> HRESULT;
            let close: Close = std::mem::transmute(slot(l, 9));
            assert_eq!(close(l), S_OK);
            assert!(super::list_record_for(l as usize).map(|r| r.closed).unwrap_or(false), "Close marks the list");
            // null Execute (slot 10): flushes the record, draws no pixels.
            let before = super::execute_flush_count();
            let had_px = super::last_execute_pixels().is_some();
            type Exec = unsafe extern "win64" fn(*mut c_void, u32, *const *mut c_void);
            let exec: Exec = std::mem::transmute(slot(q, 10));
            exec(q, 1, &l);
            assert_eq!(super::execute_flush_count(), before + 1, "null Execute flushes the record");
            assert_eq!(super::last_execute_pixels().is_some(), had_px, "null Execute draws no pixels");
            // Vulkan half: a real submit lands real pixels (S5 reads them back).
            match acquire() {
                None => crate::gpu::recomp::vulkan_runner::skip_test("no Vulkan"),
                Some(st) => {
                    let vulkan_sel = Selection { backend: Backend::Vulkan, lane: None, reason: "test".into() };
                    let mut vdev = std::ptr::null_mut();
                    assert_eq!(create_device_for(&vulkan_sel, Some(st), &mut vdev), S_OK);
                    let mkq: MkQueue = std::mem::transmute(slot(vdev, 8));
                    let mut vq = std::ptr::null_mut();
                    assert_eq!(mkq(vdev, std::ptr::null(), std::ptr::null(), &mut vq), S_OK);
                    assert_eq!((*(vq as *mut NullObj)).kind, Kind::VulkanQueue);
                    let list_x: ListX = std::mem::transmute(slot(vdev, 73));
                    let mut vl = std::ptr::null_mut();
                    assert_eq!(list_x(vdev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut vl), S_OK);
                    assert_eq!((*(vl as *mut NullObj)).kind, Kind::VulkanList);
                    let mut vpso = std::ptr::null_mut();
                    assert_eq!(pso_x(vdev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut vpso), S_OK);
                    let set: SetPso = std::mem::transmute(slot(vl, 25));
                    set(vl, vpso);
                    let close: Close = std::mem::transmute(slot(vl, 9));
                    assert_eq!(close(vl), S_OK);
                    let exec: Exec = std::mem::transmute(slot(vq, 10));
                    exec(vq, 1, &vl);
                    assert_eq!(super::execute_flush_count(), before + 2, "Vulkan Execute flushes too");
                    let px = super::last_execute_pixels().expect("Vulkan Execute submits real pixels");
                    assert_eq!((px.width, px.height), (64, 64));
                    assert_eq!(px.rgba.len(), 64 * 64 * 4);
                }
            }
        }
    }

    /// Calls `f(this)` (Win64) the way console code does, with a writable pointer left in rdx.    #[cfg(all(target_arch = "x86_64", windows))]
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
