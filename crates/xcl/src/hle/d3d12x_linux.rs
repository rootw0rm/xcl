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
//! #44/#47/#48/#52/#56/#73/#74/#75, heap #9, list #9 (Close) / #11 / #12 /
//! #13 (draw counting) / #23–#24 / #28 (BeginQuery) / #29 (EndQuery) / #30
//! (ResolveQueryData) / #32 (SetDescriptorHeaps) / #34 (root signature) /
//! #36 (root table) / #44 (root SRV) / #47 (index buffer) / #48 (vertex
//! buffers) / #50 (render targets) / #52 (ClearRenderTargetView) / #81
//! (DrawIndexedX), queue
//! #10 (ExecuteCommandLists: null asserts the record, Vulkan submits), null + Vulkan-list
//! #60/#61/#69/#70/#71/#72/#89/#93/#146/#148, null + Vulkan-device AS SRV
//! #18, Vulkan-device frame clock
//! #78–#81 (Linux twin + ancillary routing) and Vulkan-queue #30 `PresentX`
//! (real swapchain present, 1d(ii)); both devices answer the DXGI device
//! IIDs with an adapter/output chain (GetAdapter/GetDesc/EnumOutputs, 1d
//! follow-up; the null device since step 40). Null stays default behind
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

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

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
    Blob,
    /// A console IDXGIDevice wrapper (1d follow-up): answers the DXGI device
    /// IIDs for a Vulkan or null device; its `value` holds the device object.
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

static VTABLES: OnceLock<Mutex<Vec<(Kind, usize)>>> = OnceLock::new();

// ---------------------------------------------------------------------------------------
// console command-list layout (Step 68)
// ---------------------------------------------------------------------------------------

/// The title runtime numbers the command list's console methods off the PC
/// order by two (`hle/d3d12x/real.rs`'s `QUEUE_SHIFTED`): its Signal / Wait
/// moved on the queue, and on the list `CopyMemoryX` / `FillMemoryWith32BitValueX`
/// / the write-and-wait / timestamp family / `KickoffX` / `RSSetDepthBiasX`
/// sit two slots later. Which numbering a title uses cannot be told from
/// device creation; its calls tell: queue #16/#17 with one of its fences,
/// device #108/#127/#133 (console-only) or list #139 (the newer Reset) only
/// exist in the newer runtime. The flag is read at each slot call, so lists
/// created before the runtime identifies itself still route correctly; the
/// older meaning applies while it is unset (the Windows fallback).
static NEWER_LIST_LAYOUT: AtomicBool = AtomicBool::new(false);

/// Newer command-list slot → the older layout's slot of the same method.
/// Byte-for-byte the Windows twin's `hle/d3d12x/real.rs::NEWER_LIST_SLOTS`
/// (that module does not compile on Linux); a drift between the twins is a
/// routing bug.
const NEWER_LIST_SLOTS: [(usize, usize); 22] = [
    (61, 59),
    (62, 60),
    (63, 61),
    (64, 62),
    (65, 63),
    (70, 68),
    (71, 69),
    (72, 70),
    (73, 71),
    (74, 72),
    (80, 78),
    (83, 81),
    (85, 83),
    (86, 84),
    (87, 85),
    (90, 88),
    (91, 89),
    (92, 90),
    (95, 93),
    (96, 94),
    (129, 127),
    (139, 10),
];

fn note_newer_list_layout() {
    if !NEWER_LIST_LAYOUT.swap(true, Ordering::AcqRel) {
        crate::xlog_info!(
            "D3D12.X: the title's runtime uses the newer console command-list layout \
             (queue #16/#17 fence, device #108/#127/#133 or list #139)"
        );
    }
}

/// The Windows twin's `hle/d3d12x/real.rs::console_remap(Kind::CommandList)`
/// base block (that module does not compile on Linux): the console list keeps
/// slots 0–27 and 56–59 in PC order, moves the PC query methods 52–55 to
/// 28–31, and the PC methods 28–51 four slots further (console 32–55). `None`
/// for a slot outside the block. The test pins every pair; a drift between
/// the twins is a routing bug.
fn console_list_base_slot(console: usize) -> Option<usize> {
    match console {
        28..=31 => Some(console + 24),
        32..=55 => Some(console - 4),
        _ => None,
    }
}

/// The console base slot's PC method when the null list implements it, by PC
/// slot. The handler goes at the **console** slot: the title calls the console
/// numbering, so the PC index would reach another method's handler (or the
/// wrong stub). A PC method without a handler here leaves the console slot the
/// honest logging stub. Console #28–31 (the query methods) are routed
/// separately below.
fn console_base_handler(pc: usize) -> Option<usize> {
    Some(match pc {
        28 => List_SetDescriptorHeapsX as usize, // no heaps to bind: 2-arg ack
        30 => List_SetGraphicsRootSignature as usize, // S2 root sig (store-only)
        32 => List_SetGraphicsRootDescriptorTable as usize,
        40 => List_SetGraphicsRootShaderResourceView as usize,
        43 => List_IASetIndexBuffer as usize, // S2 index buffer (store-only)
        44 => List_IASetVertexBuffers as usize,
        46 => List_OMSetRenderTargets as usize, // S2 render targets (store-only)
        48 => List_ClearRenderTargetView as usize,
        _ => return None,
    })
}

/// The logging stub the vtable would have at `slot` without a console handler.
fn list_stub(slot: usize) -> usize {
    crate::stubs::make_stub(DLL, &format!("{}::#{slot}", Kind::CommandList.iface()), 0)
}

/// A command-list method the older layout has at `slot`. `#10` is its Reset
/// (the null backend acknowledges it); everything else is a logging stub.
fn legacy_list_handler(slot: usize) -> usize {
    match slot {
        10 => Ret_S_OK as usize, // Reset
        60 => VulList_CopyMemory as usize,
        61 => VulList_Fill32 as usize,
        68 => List_OMSetDepthBounds as usize,
        69 => VulList_Write32Bottom as usize,
        70 => VulList_TimestampBottom as usize,
        71 => VulList_Wait32 as usize,
        72 => VulList_Wait64 as usize,
        81 => List_NoteDraw as usize,
        89 => VulList_Write64Top as usize,
        93 => VulList_Kickoff as usize,
        94 => List_RSSetDepthBiasX as usize,
        _ => list_stub(slot),
    }
}

fn legacy_list_implemented(slot: usize) -> bool {
    matches!(slot, 10 | 60 | 61 | 68 | 69 | 70 | 71 | 72 | 81 | 89 | 93 | 94)
}

/// The method the newer runtime has at a newer-layout slot: the older
/// layout's method at `older_slot` moved there, except for the two
/// console-only methods that have no older numbering of their own
/// (`ExecuteIndirect` at #59, the newer Reset at #139 → older #10).
fn newer_list_handler(older_slot: usize) -> usize {
    match older_slot {
        59 => VulList_ExecuteIndirect as usize,
        10 => List_ResetX as usize,
        s => legacy_list_handler(s),
    }
}

/// `(older-layout target, newer-layout target)` of a command-list slot whose
/// meaning depends on the title runtime's numbering; `None` keeps the plain
/// logging stub (neither layout has a handler there on this backend).
fn list_slot_targets(slot: usize) -> Option<(usize, usize)> {
    let &(_, older_slot) = NEWER_LIST_SLOTS.iter().find(|&&(newer, _)| newer == slot)?;
    let newer_implemented = matches!(older_slot, 10 | 59) || legacy_list_implemented(older_slot);
    if !legacy_list_implemented(slot) && !newer_implemented {
        return None;
    }
    Some((legacy_list_handler(slot), newer_list_handler(older_slot)))
}

/// Newer-only entry point Reset #139: a call to this slot is itself proof of
/// the newer layout, so latch before running the console's three-argument
/// Reset.
unsafe extern "win64" fn VulList_Slot139(this: *mut NullObj, alloc: *mut c_void, pso: *mut c_void, flags: u64) -> HRESULT {
    note_newer_list_layout();
    List_ResetX(this, alloc, pso, flags)
}

/// A slot thunk that picks its target at each call from the layout byte,
/// leaving every argument register — rcx/rdx/r8/r9, xmm0–15 and the caller's
/// stack arguments — untouched. It clobbers only the two volatile registers
/// `rax`/`r10`:
///
/// ```text
/// mov rax, flag ; cmp byte [rax], 0
/// mov rax, older ; mov r10, newer ; cmovne rax, r10 ; jmp rax
/// ```
///
/// No caller classification: every call to a null-backend vtable slot comes
/// from the title.
fn layout_thunk(older: usize, newer: usize, flag: &'static AtomicBool) -> usize {
    let mut c: Vec<u8> = Vec::with_capacity(48);
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    c.extend_from_slice(&(flag as *const AtomicBool as u64).to_le_bytes());
    c.extend_from_slice(&[0x80, 0x38, 0x00]); // cmp byte [rax], 0
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64 (older target)
    c.extend_from_slice(&(older as u64).to_le_bytes());
    c.extend_from_slice(&[0x49, 0xBA]); // mov r10, imm64 (newer target)
    c.extend_from_slice(&(newer as u64).to_le_bytes());
    c.extend_from_slice(&[0x49, 0x0F, 0x45, 0xC2]); // cmovne rax, r10 (r10 in r/m, REX.B)
    c.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
    let p = crate::stubs::alloc_code(c.len());
    unsafe {
        std::ptr::copy_nonoverlapping(c.as_ptr(), p, c.len());
    }
    p as usize
}

fn vtable(kind: Kind) -> *const usize {
    let t = VTABLES.get_or_init(|| Mutex::new(Vec::new()));
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
            // console CopyDescriptorsSimple: the slot bytes in the title's heap
            v[24] = Device_CopyDescriptorsSimple as usize;
            // console CreateShaderResourceViewAt: the staging writer for #24
            v[70] = Device_CreateShaderResourceViewAt as usize;
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
            v[57] = Device_DeserializePipelineStateX as usize; // console pipeline cache: E_NOTIMPL
            v[60] = Device_CreateGraphicsPipelineStateX as usize; // console PSO
            // console compute PSO (XboxBvhBuilder): (desc, a, b, riid, out) — PC #62 = CreateStateObject
            v[62] = Device_CreateComputePipelineStateX as usize;
            v[67] = Device_DeserializePipelineStateX as usize; // console pipeline cache: E_NOTIMPL
            v[86] = Device_CreatePipelineStateX as usize; // console CreatePipelineState (stream desc)
            v[73] = Device_CreateCommandListX as usize; // console list (DMA)
            v[74] = Device_RegisterPagePoolX as usize;
            v[75] = Device_UnregisterPagePoolX as usize;
            v[108] = Device_GetResourceAllocationInfo2 as usize; // console allocation info (desc1)
            v[127] = Device_CreateCommittedResourceLayoutX as usize; // console committed resource (barrier layout)
            v[133] = Device_CreatePlacedResourceLayoutX as usize; // console placed title memory (barrier layout)
        }
        Kind::CommandQueue | Kind::VulkanQueue => {
            v[10] = Queue_ExecuteCommandLists as usize;
            v[14] = Queue_Signal as usize;
            v[15] = Ret_S_OK as usize; // Wait
            // #16/#17 are Signal/Wait on the newer console layout and the PC
            // order's GetTimestampFrequency/GetClockCalibration on the older
            // one; the fence registry tells the call shapes apart.
            v[16] = Queue_Slot16 as usize;
            v[17] = Queue_Slot17 as usize;
            // The title's runtime uses the newer console queue layout: #18 is
            // GetTimestampFrequency (HRESULT + u64 out), #19 GetClockCalibration,
            // #20 GetDesc. Retail call: exe+0x2484180.
            v[18] = Queue_GetTimestampFrequency as usize;
            v[19] = Queue_GetClockCalibration as usize;
            v[20] = Hidden_ZeroDesc16 as usize;
            v[32] = Queue_PresentX as usize; // newer layout PresentX
            v[35] = Queue_ExecuteCommandListsX as usize; // newer layout ExecuteCommandListsX
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
            v[12] = Fence_CompletedValueAddress as usize; // console completed value address
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
        // Console base layout (Step 70, the twin's `real.rs::console_remap`):
        // the PC methods 28–51 sit four console slots further. A PC method
        // without a handler leaves its console slot the logging stub.
        for console in 28..=55 {
            if let Some(handler) = console_list_base_slot(console).and_then(console_base_handler) {
                v[console] = handler;
            }
        }
        v[23] = List_OMSetBlendFactor as usize; // PC order keeps #23/#24
        v[24] = List_OMSetStencilRef as usize;
        // PC CopyBufferRegion (slot 15): the same immediate host memcpy as the
        // console DMA one — the single queue is already coherent, so the copy
        // lands at record time and ExecuteCommandLists stays a no-op.
        v[15] = Dma_CopyBufferRegion as usize;
        // M2 title-mem/streaming leg: PC CopyTextureRegion (slot 16) detiles a
        // placed title-memory texture into the readback buffer, same timing.
        v[16] = List_CopyTextureRegion as usize;
        // M2 draw S5: PC DrawInstanced (12) / DrawIndexedInstanced (13) count
        // draws for the #28/#29 occlusion queries; console slots #28 (Begin),
        // #29 (End), #30 (Resolve) and DrawIndexedX #81 feed the same counter.
        v[12] = List_NoteDraw as usize;
        v[13] = List_NoteDraw as usize;
        v[28] = List_BeginQuery as usize;
        v[29] = List_EndQuery as usize;
        v[30] = List_ResolveQueryData as usize;
        v[81] = List_NoteDraw as usize;
        // Console command-list methods the title calls on both layouts: the
        // older numbering at these slots. The slots a newer runtime moves are
        // chosen at each call below.
        v[60] = VulList_CopyMemory as usize;
        v[68] = List_OMSetDepthBounds as usize;
        v[69] = VulList_Write32Bottom as usize;
        v[89] = VulList_Write64Top as usize;
        v[93] = VulList_Kickoff as usize;
        v[94] = List_RSSetDepthBiasX as usize; // S6 dynamic depth bias
        for slot in 0..VTBL_SLOTS {
            if let Some((older, newer)) = list_slot_targets(slot) {
                // Reset #139 is a newer-only entry point: calling it proves
                // the layout, so it latches then runs Reset.
                v[slot] = if slot == 139 { VulList_Slot139 as usize } else { layout_thunk(older, newer, &NEWER_LIST_LAYOUT) };
            }
        }
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

/// Split the first two arguments of a method whose struct result travels
/// through a hidden pointer. The retail title calls the C way — `(ret, this,
/// …)` (`exe+0x247870d`: `rcx` = result buffer, `rdx` = device) — while
/// windows-rs (the test title) calls `(this, ret, …)`. Like the Windows twin's
/// `this_and_ret`, whichever argument is one of our objects is `this`; the
/// other is the result buffer. Writing to the wrong one overwrites the
/// `NullObj` header (`vtbl`/`refs`/`kind`) with the struct.
unsafe fn this_and_ret(a: *mut c_void, b: *mut c_void) -> (*mut NullObj, *mut u8) {
    // Unaligned: a 20-byte struct's buffer is 4-byte aligned.
    let ours = |p: *mut c_void| {
        !p.is_null()
            && VTABLES.get().is_some_and(|t| {
                let vt = (p as *const usize).read_unaligned();
                t.lock().unwrap().iter().any(|&(_, v)| v == vt)
            })
    };
    if ours(b) && !ours(a) {
        (b as *mut NullObj, a as *mut u8)
    } else {
        (a as *mut NullObj, b as *mut u8)
    }
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

/// Fences the title created. The newer console queue layout puts `Signal(fence, value)`
/// and `Wait(fence, value)` at #16/#17, where the PC order has `GetTimestampFrequency` /
/// `GetClockCalibration` (out pointers). Windows tells the shapes apart with the same
/// registry (`real.rs::is_fence`) so a timing out-parameter is never dereferenced as a
/// fence and a fence is never written through as a u64 out slot.
static FENCES: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn note_fence(o: *mut NullObj) {
    FENCES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(o as usize);
}

fn is_title_fence(p: *mut c_void) -> bool {
    !p.is_null() && FENCES.lock().unwrap_or_else(|e| e.into_inner()).contains(&(p as usize))
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
    // Both devices are also IDXGIDevices (the Windows twin's `dxgi_device`):
    // DXGI queries get an adapter-chain wrapper; every other interface keeps
    // the old answer (self). Retail asks the null device at `0x142481cd5`.
    if !this.is_null() && matches!((*this).kind, Kind::Device | Kind::VulkanDevice) && dxgi_device_iid(riid) {
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
/// Console device #62 `CreateComputePipelineState(desc, a, b, riid, out)`: two
/// arguments more than the PC method (XboxBvhBuilder passes NULL, NULL), so the
/// out pointer is the sixth argument and must never be taken from `r9`.
unsafe extern "win64" fn Device_CreateComputePipelineStateX(
    _t: *mut NullObj,
    d: *const u8,
    a: usize,
    b: usize,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if a != 0 || b != 0 {
        crate::xlog_warn!("CreateComputePipelineState (console, slot 62): unknown parameters {a:#x} {b:#x} ignored");
    }
    capture_compute_desc(d);
    out_obj(ppv, Kind::PipelineState)
}

/// Console device #57/#67 `Deserialize{Graphics,Compute}PipelineStateX(state,
/// riid, pipeline)`: the serialised form is the console driver's, so the
/// Windows twin returns `E_NOTIMPL` with a null out pointer and the title
/// creates the pipeline from its desc. A plain stub returned `S_OK` and left
/// the title's out pointer untouched (585 + 42 calls in the first frame run).
unsafe extern "win64" fn Device_DeserializePipelineStateX(
    _t: *mut NullObj,
    _state: *const u8,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    static WARNED: AtomicU32 = AtomicU32::new(0);
    if WARNED.swap(1, Ordering::Relaxed) == 0 {
        crate::xlog_warn!("D3D12.X: DeserializePipelineStateX – console pipeline caches are not supported");
    }
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    E_NOTIMPL
}

/// Console device #86 `CreatePipelineState(stream desc, riid, pipeline)`: the
/// null device hands back a pipeline object (Windows builds the PC pipeline
/// from the same stream desc).
unsafe extern "win64" fn Device_CreatePipelineStateX(
    _t: *mut NullObj,
    _desc: *const u8,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
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

/// Draw-relevant PSO bytes stashed at creation. `rt0_mask` is the RT0 color
/// write mask (see [`ReplayDraw`]); `static_bias` is the rasterizer's baked
/// depth bias triple (bias, clamp, slope).
#[derive(Clone)]
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
    pub rt0_mask: u8,
    pub static_bias: [f32; 3],
}

impl Default for PsoRecord {
    /// A record that never saw creation bytes binds everything (full mask,
    /// no bias) — the S4/S5 unit shapes.
    fn default() -> Self {
        Self {
            vs_ptr: 0,
            vs_len: 0,
            ps_ptr: 0,
            ps_len: 0,
            blend: Vec::new(),
            raster: Vec::new(),
            num_rtv: 0,
            rtv_formats: [0; 8],
            dsv_format: 0,
            rt0_mask: 0xF,
            static_bias: [0.0; 3],
        }
    }
}

/// A `CopyTextureRegion` from a committed render target stashed at record
/// time: `Execute` fills it from the S4 offscreen pixels (record time is too
/// early — the pixels only exist after the submit). Offsets/pitches come
/// straight from the destination footprint.
#[derive(Clone, Default)]
pub struct PendingCopy {
    pub dst: usize,
    pub offset: u64,
    pub pitch: u32,
    pub height: u32,
}

/// One counted draw's replay state (M2 draw S6): the list state snapshotted
/// at `List_NoteDraw` so Execute replays each draw group with its own
/// viewport/scissor, bound-PSO blend mask, and effective depth bias instead
/// of one full-target draw. Query banking stays per draw call at record time
/// (S5); the groups only drive pixels.
#[derive(Clone, Default)]
pub struct DrawOp {
    pub viewport: [f32; 6],
    pub has_viewport: bool,
    pub scissor: [i32; 4],
    pub has_scissor: bool,
    pub pso: usize,
    pub rt0_mask: u8,
    pub topology: u32,
    pub depth_test: bool,
    pub depth_bias: f32,
    pub depth_slope: f32,
    pub depth_clamp: f32,
}

/// List-bound draw state: the PSO bound at SetPipelineState plus its record,
/// with the list-bound assembly S2 stores (root sig, render targets, index
/// buffer, viewports/scissors, topology), the S4 Close mark, the S5 draw
/// counter + query snapshots/counts + pending committed-RT copies, and the S6
/// per-draw groups + bias state (dynamic #94 override over the bound PSO's
/// static triple). Store-only; S3 interprets, S4 executes, S5 resolves/reads back.
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
    pub draws: u64,
    pub query_start: [Option<u64>; 8],
    pub query_counts: [u64; 8],
    pub pending_copies: Vec<PendingCopy>,
    pub ops: Vec<DrawOp>,
    pub dyn_bias: Option<[f32; 3]>,
    pub static_bias: [f32; 3],
    /// Console `OMSetDepthBounds(min, max)` (#68, or newer #70): store-only,
    /// like the null backend's other raster state.
    pub depth_bounds: Option<[f32; 2]>,
    /// Console list #23 `OMSetBlendFactor`: the bound factor (`None` after a
    /// null argument, i.e. the D3D12 default 1, 1, 1, 1).
    pub blend_factor: Option<[f32; 4]>,
    /// Console list #24 `OMSetStencilRef`.
    pub stencil_ref: Option<u32>,
    /// Console list #36 `SetGraphicsRootDescriptorTable` (PC #32): the last
    /// `(root parameter, base descriptor)`.
    pub root_table: Option<(u32, u64)>,
    /// Console list #44 `SetGraphicsRootShaderResourceView` (PC #40): the
    /// last `(root parameter, buffer location)`.
    pub root_srv: Option<(u32, u64)>,
    /// Console list #48 `IASetVertexBuffers` (PC #44): the first slot and the
    /// raw 16-byte `D3D12_VERTEX_BUFFER_VIEW`s that follow it.
    pub vb_start: u32,
    pub vb_views: Vec<u8>,
    /// Console list #52 `ClearRenderTargetView` (PC #48): the last
    /// `(rtv handle, colour, rect count)`. The null backend has no target to
    /// clear.
    pub clear_rtv: Option<(u64, [f32; 4], u32)>,
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

/// Copy one PSO record's VS bytes out (title graphics memory is host
/// memory; garbage lengths fail closed to the triangle fallback instead of
/// faulting). S4 uses this per group so each draw replays its own PSO's VS.
fn pso_vs_bytes(rec: &PsoRecord) -> Vec<u8> {
    if rec.vs_ptr != 0 && rec.vs_len != 0 && rec.vs_len < (16 << 20) {
        unsafe { std::slice::from_raw_parts(rec.vs_ptr as *const u8, rec.vs_len).to_vec() }
    } else {
        Vec::new()
    }
}

/// S4: per-group VS bytes for Execute — a group drawn under a different PSO
/// carries that PSO's bytes; same-PSO and unknown-PSO groups inherit the
/// list-level bytes (empty).
fn group_own_vs_bytes(op_pso: usize, list_pso: usize) -> Vec<u8> {
    if op_pso == 0 || op_pso == list_pso {
        return Vec::new();
    }
    pso_record_for(op_pso).map(|rec| pso_vs_bytes(&rec)).unwrap_or_default()
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
    // RT0 write mask: byte 36 of RT0 in the blend desc (blend + 8 + 36).
    r.rt0_mask = if r.blend.len() > 44 { r.blend[44] } else { 0xF };
    // Rasterizer static bias triple: DepthBias i32 @12, DepthBiasClamp f32
    // @16, SlopeScaledDepthBias f32 @20 of D3D12_RASTERIZER_DESC.
    if r.raster.len() >= 24 {
        r.static_bias = [
            i32::from_le_bytes([r.raster[12], r.raster[13], r.raster[14], r.raster[15]]) as f32,
            f32::from_le_bytes([r.raster[16], r.raster[17], r.raster[18], r.raster[19]]),
            f32::from_le_bytes([r.raster[20], r.raster[21], r.raster[22], r.raster[23]]),
        ];
    }
    pso_store().lock().unwrap().insert(obj as usize, r);
}

/// List slot 25 `SetPipelineState(pso)`: bind the PSO's record to this list,
/// preserving any S2 state already stored (order-independent). A fresh PSO
/// brings its own static bias and clears any dynamic #94 override.
unsafe extern "win64" fn List_SetPipelineState(this: *mut NullObj, pso: *mut c_void) {
    if this.is_null() {
        return;
    }
    let info = pso_store().lock().unwrap().get(&(pso as usize)).cloned().unwrap_or_default();
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    e.pso = pso as usize;
    e.static_bias = info.static_bias;
    e.dyn_bias = None;
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

/// Console list #23 `OMSetBlendFactor(factor[4])`: stash the factor. The PC
/// index is the same (0–27 keep the PC order); a null pointer clears it.
unsafe extern "win64" fn List_OMSetBlendFactor(this: *mut NullObj, factor: *const f32) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    e.blend_factor = if factor.is_null() {
        None
    } else {
        Some([*factor, *factor.add(1), *factor.add(2), *factor.add(3)])
    };
}

/// Console list #24 `OMSetStencilRef(ref)`: stash the reference value.
unsafe extern "win64" fn List_OMSetStencilRef(this: *mut NullObj, value: u32) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().stencil_ref = Some(value);
}

/// Console list #36 `SetGraphicsRootDescriptorTable(param, base)` (PC #32):
/// the console list numbers the PC methods 28–51 four slots further, so the
/// title's table call arrives here, not at the PC index. Store-only.
unsafe extern "win64" fn List_SetGraphicsRootDescriptorTable(this: *mut NullObj, param: u32, base: u64) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().root_table = Some((param, base));
}

/// Console list #44 `SetGraphicsRootShaderResourceView(param, location)`
/// (PC #40): store the root descriptor address.
unsafe extern "win64" fn List_SetGraphicsRootShaderResourceView(this: *mut NullObj, param: u32, location: u64) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().root_srv = Some((param, location));
}

/// Console list #48 `IASetVertexBuffers(start, count, views)` (PC #44): stash
/// the raw 16-byte views after the first slot. A null array clears them.
unsafe extern "win64" fn List_IASetVertexBuffers(this: *mut NullObj, start: u32, count: u32, views: *const u8) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    e.vb_start = start;
    e.vb_views.clear();
    if views.is_null() {
        return;
    }
    let bytes = (count.min(32) as usize) * 16;
    e.vb_views.extend_from_slice(std::slice::from_raw_parts(views, bytes));
}

/// Console list #52 `ClearRenderTargetView(rtv, colour, nrects, rects)`
/// (PC #48): store-only acknowledgement — the null backend has no target to
/// clear.
unsafe extern "win64" fn List_ClearRenderTargetView(this: *mut NullObj, rtv: u64, color: *const f32, n: u32, _rects: *const c_void) {
    if this.is_null() || color.is_null() {
        return;
    }
    let c = [*color, *color.add(1), *color.add(2), *color.add(3)];
    list_store().lock().unwrap().entry(this as usize).or_default().clear_rtv = Some((rtv, c, n));
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
// assert the record exists and stop there (no pixels), Vulkan queues replay
// the S6 per-draw groups (viewports, blend variants, depth bias) through one
// offscreen submit whose pixels wait for the S5 readback. A list with no
// bound PSO is skipped with a debug line (fail-closed); missing-Vulkan
// (`"no ..."`) errors skip the same way.
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
            // null lists assert the record, no pixels: drop any stashed
            // committed-RT copies unwritten (the readback stays zero).
            let mut ls = list_store().lock().unwrap();
            if let Some(e) = ls.get_mut(&(l as usize)) {
                e.pending_copies.clear();
            }
            continue;
        }
        // Copy the VS bytes out from under the store lock (the title's
        // graphics memory is host memory; cap garbage lengths at the
        // triangle fallback instead of faulting).
        let bytes = pso_vs_bytes(&r.pso_info);
        match crate::gpu::recomp::vulkan_graphics::build_record_pipeline(&bytes, r.pso_info.num_rtv) {
            Ok(sum) => crate::xlog_debug!(
                "D3D12.X Execute: record pipeline built (vs {} words, fallback {}: {})",
                sum.vs_words,
                sum.vs_fallback,
                sum.vs_detail
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
        // S6: replay each counted draw group (viewport, blend mask, depth
        // bias) in list order; an empty group list replays the S4 single draw.
        // S4: a group drawn under a different PSO carries that PSO's VS
        // bytes (empty inherits the list bytes above).
        let groups: Vec<crate::gpu::recomp::vulkan_graphics::ReplayDraw> = r
            .ops
            .iter()
            .map(|o| crate::gpu::recomp::vulkan_graphics::ReplayDraw {
                viewport: o.viewport,
                scissor: o.has_scissor.then_some(o.scissor),
                rt0_mask: o.rt0_mask,
                depth_test: o.depth_test,
                depth_bias: o.depth_bias,
                depth_slope: o.depth_slope,
                depth_clamp: o.depth_clamp,
                vs_bytes: group_own_vs_bytes(o.pso, r.pso),
            })
            .collect();
        match crate::gpu::recomp::vulkan_graphics::execute_draw_ops(&bytes, &groups) {
            Ok((px, fallback)) => {
                let own = groups.iter().filter(|g| !g.vs_bytes.is_empty()).count();
                crate::xlog_debug!("D3D12.X Execute: offscreen submit ({} groups, {own} own-VS, fallback {fallback})", groups.len());
                *last_pixels_store().lock().unwrap() = Some(px.clone());
                // S5 readback: fill the list's stashed committed-RT copies
                // from the submit's pixels, then drop them.
                let mut ls = list_store().lock().unwrap();
                if let Some(e) = ls.get_mut(&(l as usize)) {
                    for c in e.pending_copies.drain(..) {
                        blit_execute_pixels(&px, &c);
                    }
                }
            }
            Err(e) if e.starts_with("no ") => crate::xlog_debug!("D3D12.X Execute: submit skipped ({e})"),
            Err(e) => crate::xlog_warn!("D3D12.X Execute: submit failed ({e})"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// M2 draw S5: query-resolve + readback. List slots #12/#13 (PC draws) and #81
// (console DrawIndexedX) count draws; #28 (BeginQuery) snapshots the counter
// and #29 (EndQuery) banks `(draws - snapshot) * SAMPLES_PER_DRAW` for its
// index, so the title's nested occlusion shape resolves to outer = 2N,
// inner = N, single = N. #30 (ResolveQueryData) writes the banked counts as
// u64s into the results buffer's committed backing (unresolved indices read
// 0, fail-closed). A CopyTextureRegion from a committed 2D RGBA8 render
// target stashes a pending copy at record time; the S4 Execute flush above
// fills it from the submit's pixels. Counting is API-level (both backends);
// pixels land on Vulkan only — null readbacks stay zero.
// ---------------------------------------------------------------------------------------

/// Samples one counted draw contributes to a resolved occlusion query.
pub const SAMPLES_PER_DRAW: u64 = 1024;

/// Slots #12/#13/#81: one more draw on this list (extra call args ignored).
/// Snapshots one S6 replay group: the current viewport/scissor (or
/// full-target defaults), bound-PSO blend mask, and effective depth bias.
unsafe extern "win64" fn List_NoteDraw(this: *mut NullObj) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    e.draws += 1;
    let mut op = DrawOp {
        viewport: [0.0, 0.0, 64.0, 64.0, 0.0, 1.0],
        scissor: [0, 0, 64, 64],
        pso: e.pso,
        rt0_mask: e.pso_info.rt0_mask,
        topology: e.topology,
        depth_test: e.has_dsv,
        ..DrawOp::default()
    };
    if e.viewports.len() >= 24 {
        let f = |o: usize| f32::from_le_bytes([e.viewports[o], e.viewports[o + 1], e.viewports[o + 2], e.viewports[o + 3]]);
        op.viewport = [f(0), f(4), f(8), f(12), f(16), f(20)];
        op.has_viewport = true;
    }
    if e.scissors.len() >= 16 {
        let i = |o: usize| i32::from_le_bytes([e.scissors[o], e.scissors[o + 1], e.scissors[o + 2], e.scissors[o + 3]]);
        op.scissor = [i(0), i(4), i(8), i(12)];
        op.has_scissor = true;
    }
    let b = e.dyn_bias.unwrap_or(e.static_bias);
    (op.depth_bias, op.depth_slope, op.depth_clamp) = (b[0], b[1], b[2]);
    e.ops.push(op);
}

/// List slot 94 `RSSetDepthBiasX(bias, slope, clamp)`: stash the dynamic
/// depth-bias override; the next SetPipelineState clears it (a fresh PSO
/// brings its own static bias).
unsafe extern "win64" fn List_RSSetDepthBiasX(this: *mut NullObj, bias: f32, slope: f32, clamp: f32) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().dyn_bias = Some([bias, slope, clamp]);
}

/// List slot 28 `BeginQuery(heap, type, index)`: snapshot the draw counter.
unsafe extern "win64" fn List_BeginQuery(this: *mut NullObj, _heap: *mut c_void, _ty: u32, index: u32) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    let draws = e.draws;
    e.query_start[(index & 7) as usize] = Some(draws);
}

/// List slot 29 `EndQuery(heap, type, index)`: bank the samples drawn since
/// the matching Begin (an unmatched End counts from zero, fail-closed).
unsafe extern "win64" fn List_EndQuery(this: *mut NullObj, _heap: *mut c_void, _ty: u32, index: u32) {
    if this.is_null() {
        return;
    }
    let mut ls = list_store().lock().unwrap();
    let e = ls.entry(this as usize).or_default();
    let i = (index & 7) as usize;
    let start = e.query_start[i].unwrap_or(0);
    e.query_counts[i] = e.draws.saturating_sub(start).saturating_mul(SAMPLES_PER_DRAW);
}

/// List slot 30 `ResolveQueryData(heap, type, start, count, dst, dst_off)`:
/// write the banked counts as u64s into the destination's committed backing.
unsafe extern "win64" fn List_ResolveQueryData(
    this: *mut NullObj,
    _heap: *mut c_void,
    _ty: u32,
    start: u32,
    count: u32,
    dst: *mut NullObj,
    dst_off: u64,
) {
    if this.is_null() || dst.is_null() {
        return;
    }
    let counts = list_store().lock().unwrap().get(&(this as usize)).map(|r| r.query_counts).unwrap_or([0; 8]);
    let (dp, dn) = resource_backing(dst);
    if dp.is_null() {
        return;
    }
    for i in 0..count.min(64) {
        let idx = start.wrapping_add(i);
        let v = if (idx as usize) < 8 { counts[idx as usize] } else { 0 };
        let off = dst_off.wrapping_add(i as u64 * 8) as usize;
        if off + 8 <= dn {
            (dp.add(off) as *mut u64).write_unaligned(v);
        }
    }
}

/// Fill one stashed committed-RT copy from a submit's pixels: row-by-row into
/// the destination footprint (pitch-aware, clamped to both shapes).
unsafe fn blit_execute_pixels(px: &crate::gpu::recomp::vulkan_graphics::OffscreenPixels, c: &PendingCopy) {
    if c.dst == 0 || px.rgba.is_empty() {
        return;
    }
    let o = c.dst as *mut NullObj;
    if (*o).kind != Kind::Resource && (*o).kind != Kind::PlacedResource {
        return;
    }
    let (dp, dn) = resource_backing(o);
    if dp.is_null() || c.pitch == 0 || c.height == 0 {
        return;
    }
    let rows = (px.height as usize).min(c.height as usize);
    let cols = (px.width as usize * 4).min(c.pitch as usize);
    let src_pitch = px.width as usize * 4;
    for y in 0..rows {
        let off = c.offset as usize + y * c.pitch as usize;
        if off + cols > dn || (y + 1) * src_pitch > px.rgba.len() {
            break;
        }
        std::ptr::copy_nonoverlapping(px.rgba.as_ptr().add(y * src_pitch), dp.add(off), cols);
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
/// with 32 bits): the handle range comes from the title's allocation hook when it
/// gives one — the retail game asserts the start is the base of its own arena — and
/// otherwise from the high-VA graphics pool's next window-0 range, committed host
/// memory (views write words into the slots) — never plain host memory below the
/// windows. Falls back to plain memory when the pool has no window-0 room, so the
/// heap still works.
unsafe extern "win64" fn Device_CreateDescriptorHeap(_t: *mut NullObj, desc: *const u8, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let o = new_obj(Kind::DescriptorHeap);
    if !desc.is_null() {
        std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 16);
        let n = *(desc.add(4) as *const u32) as usize;
        // The title's allocator sees the requested size; only the pool fallback is capped.
        let bytes = n.max(1) * 32;
        let start = crate::hle::kernelx_linux::title_descriptor_heap_memory(bytes)
            .or_else(|| {
                // 64 KiB floor is the pool grain; the title reads slots back, so commit.
                crate::hle::kernelx_linux::gfx_alloc_window0(bytes.min(64 << 20), 0x1_0000, true)
            })
            .unwrap_or(0);
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
/// Console device #24 `CopyDescriptorsSimple(n, dst, src, type)`: retail stages
/// a descriptor with #70 and copies it into its binding heap here
/// (`exe+0x4ecc87e`, n = 1, both handles in the shader-visible heap; 113 calls
/// in one fresh-root run). A Linux handle is a host address inside a heap from
/// [`Device_CreateDescriptorHeap`] and every slot is 32 bytes
/// ([`Device_GetDescriptorHandleIncrementSize`]), so the copy is the slot bytes.
/// A range outside the graphics pool (or a length overflow) is refused, never
/// written.
unsafe extern "win64" fn Device_CopyDescriptorsSimple(_t: *mut NullObj, n: u32, dst: u64, src: u64, _ty: u32) -> HRESULT {
    let Some(bytes) = (n as usize).checked_mul(32).filter(|b| *b != 0) else {
        return E_INVALIDARG;
    };
    let (dst, src) = (dst as usize, src as usize);
    if !crate::hle::kernelx_linux::gfx_contains_range(dst, bytes) || !crate::hle::kernelx_linux::gfx_contains_range(src, bytes) {
        static N: AtomicU32 = AtomicU32::new(0);
        if N.fetch_add(1, Ordering::Relaxed) < 8 {
            crate::xlog_warn!("D3D12.X: CopyDescriptorsSimple({n}) outside the descriptor heaps: {src:#x} -> {dst:#x}");
        }
        return E_INVALIDARG;
    }
    std::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes);
    S_OK
}
/// Console device #70 `CreateShaderResourceViewAt(resource_desc, view, cpu)`:
/// the writer half of retail's raw-memory descriptor staging (`exe+0x4ecc81b`,
/// right before the #24 copy). The view is the console layout: Format u32 @0,
/// ViewDimension u32 @4, Shader4ComponentMapping u32 @8 (0x1688 = identity),
/// the dimension union @0x10 and the GPU address u64 @0x28 — the layout the
/// twin's RTAS handler already reads Location from, not the PC
/// `D3D12_SHADER_RESOURCE_VIEW_DESC` (union @8). The handler writes the
/// console descriptor into the 32-byte slot at `cpu`: a V# for BUFFER, a T#
/// with BASE_ADDRESS for dimensions 2–10, the BVH words for a ray-tracing
/// view. `cpu` must be a graphics-pool address (a title heap slot), else
/// nothing is written; the GPU address is encoded, never dereferenced.
unsafe extern "win64" fn Device_CreateShaderResourceViewAt(_t: *mut NullObj, rdesc: *const u8, view: *const u8, cpu: u64) {
    use crate::gpu::desc::{self, ImageType};
    const SLOT: usize = 32;
    if view.is_null() || cpu == 0 || !crate::hle::kernelx_linux::gfx_contains_range(cpu as usize, SLOT) {
        return;
    }
    let u32_at = |off: usize| (view.add(off) as *const u32).read_unaligned();
    let dim = u32_at(4);
    let va = (view.add(0x28) as *const u64).read_unaligned();
    if va == 0 {
        static N: AtomicU32 = AtomicU32::new(0);
        if N.fetch_add(1, Ordering::Relaxed) < 8 {
            crate::xlog_warn!("D3D12.X: CreateShaderResourceViewAt(#{dim}) with a null GPU address: slot {cpu:#x}");
        }
        return;
    }
    let mut words = [0u32; 8];
    match dim {
        1 => {
            let first = (view.add(0x10) as *const u64).read_unaligned();
            let n = u32_at(0x18);
            let stride = u32_at(0x1c);
            let raw = u32_at(0x20) & 1 != 0;
            let fmt = desc::gfx10_format_num(u32_at(0)).unwrap_or(0);
            let (stride, elem) = if raw {
                (0, 4)
            } else if stride > 0 {
                (stride, stride)
            } else {
                let e = desc::format_bytes(fmt);
                (e, e)
            };
            let Some(address) = first.checked_mul(elem as u64).and_then(|off| va.checked_add(off)) else {
                return;
            };
            let records = if stride == 0 { n.saturating_mul(4) } else { n };
            words[..4].copy_from_slice(&desc::buffer_words(address, stride, records, fmt));
        }
        2..=10 => {
            if rdesc.is_null() {
                return;
            }
            let view_format = u32_at(0);
            let mapping = u32_at(8);
            let res_format = (rdesc.add(32) as *const u32).read_unaligned();
            let fmt = desc::gfx10_format_num(if view_format == 0 { res_format } else { view_format }).unwrap_or(0);
            let width = (rdesc.add(16) as *const u64).read_unaligned() as u32;
            let height = (rdesc.add(24) as *const u32).read_unaligned();
            let array = (rdesc.add(28) as *const u16).read_unaligned() as u32;
            let mips = (rdesc.add(30) as *const u16).read_unaligned() as u32;
            let samples = (rdesc.add(36) as *const u32).read_unaligned().max(1);
            let (ty, most, levels, first_slice, depth) = match dim {
                2 => (ImageType::D1, u32_at(0x10), u32_at(0x14), 0, 1),
                3 => (ImageType::D1Array, u32_at(0x10), u32_at(0x14), u32_at(0x18), u32_at(0x1c).max(1)),
                4 => (ImageType::D2, u32_at(0x10), u32_at(0x14), 0, 1),
                5 => (ImageType::D2Array, u32_at(0x10), u32_at(0x14), u32_at(0x18), u32_at(0x1c).max(1)),
                6 => (ImageType::D2Msaa, 0, 1, 0, 1),
                7 => (ImageType::D2MsaaArray, 0, 1, u32_at(0x10), u32_at(0x14).max(1)),
                8 => (ImageType::D3, u32_at(0x10), u32_at(0x14), 0, array.max(1)),
                9 => (ImageType::Cube, u32_at(0x10), u32_at(0x14), 0, array.max(6)),
                // TEXTURECUBEARRAY: NumCubes faces, starting at First2DArrayFace
                10 => (ImageType::Cube, u32_at(0x10), u32_at(0x14), u32_at(0x18), u32_at(0x1c).max(1).saturating_mul(6)),
                _ => return,
            };
            let (base_level, last_level, max_mip) = if matches!(ty, ImageType::D2Msaa | ImageType::D2MsaaArray) {
                (0, samples.trailing_zeros().min(4), 0)
            } else {
                let res_max = mips.max(1) - 1;
                let base = most.min(res_max).min(15);
                let last = if levels == 0 || levels == u32::MAX {
                    res_max
                } else {
                    base.saturating_add(levels.saturating_sub(1)).min(res_max)
                }
                .min(15);
                (base, last, res_max.min(15))
            };
            let dst_sel = [0u32, 3, 6, 9].map(|shift| {
                let c = (mapping >> shift) & 7;
                if c < 4 {
                    c + 4
                } else {
                    c
                }
            });
            words = desc::console_image_words(va, fmt, ty, width, height, depth, base_level, last_level, max_mip, first_slice, dst_sel);
        }
        11 => words[..4].copy_from_slice(&desc::bvh_words(va, false)),
        _ => return,
    }
    std::ptr::write_bytes(cpu as *mut u8, 0, SLOT);
    std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, cpu as *mut u8, SLOT);
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

unsafe extern "win64" fn Device_GetResourceAllocationInfo(a: *mut c_void, b: *mut c_void, _mask: u32, n: u32, descs: *const u8) -> *mut u8 {
    let (_this, ret) = this_and_ret(a, b);
    let mut total = 0usize;
    for i in 0..n as usize {
        total += (approx_size(descs.add(i * 56)) + 0xFFFF) & !0xFFFF;
    }
    if !ret.is_null() {
        // D3D12_RESOURCE_ALLOCATION_INFO { SizeInBytes, Alignment } — the buffer
        // is only required to be 4-byte aligned, so never assume u64 alignment.
        (ret as *mut u64).write_unaligned(total as u64);
        (ret.add(8) as *mut u64).write_unaligned(0x10000);
    }
    ret
}
/// Console device #108 `GetResourceAllocationInfo2`: a hidden return buffer first
/// (the title's order: `(ret, device, mask, count, D3D12_RESOURCE_DESC1[], infos)`),
/// 0x48-byte descriptors, and an optional `{offset, alignment, size}` per
/// resource. The title asserts the total size's high word is zero
/// (`exe+0xabfe1b`), so the buffer is written even without a PC device.
unsafe extern "win64" fn Device_GetResourceAllocationInfo2(
    a: *mut c_void,
    b: *mut c_void,
    _mask: u32,
    n: u32,
    descs: *const u8,
    infos: *mut [u64; 3],
) -> *mut u8 {
    note_newer_list_layout(); // console-only device method (#108)
    let (_this, ret) = this_and_ret(a, b);
    let (mut size, mut align) = (0u64, 0u64);
    for k in 0..n as usize {
        let d = if descs.is_null() { std::ptr::null() } else { descs.add(k * 0x48) };
        let s = ((approx_size(d) as u64) + 0xFFFF) & !0xFFFF;
        let offset = size.next_multiple_of(0x10000);
        if !infos.is_null() {
            // D3D12_RESOURCE_ALLOCATION_INFO1 { Offset, Alignment, SizeInBytes }.
            infos.add(k).write_unaligned([offset, 0x10000, s]);
        }
        size = offset + s;
        align = 0x10000;
    }
    if n > 1 {
        size = size.next_multiple_of(align);
    }
    if !ret.is_null() {
        (ret as *mut u64).write_unaligned(size);
        (ret.add(8) as *mut u64).write_unaligned(align);
    }
    ret
}
unsafe extern "win64" fn Device_GetCustomHeapProperties(a: *mut c_void, b: *mut c_void, _m: u32, ty: u32) -> *mut u8 {
    let (_this, ret) = this_and_ret(a, b);
    if !ret.is_null() {
        std::ptr::write_bytes(ret, 0, 20);
        (ret as *mut u32).write_unaligned(4); // CUSTOM
        (ret.add(4) as *mut u32).write_unaligned(if ty == 2 || ty == 3 { 3 } else { 1 }); // CPU page property
        (ret.add(8) as *mut u32).write_unaligned(1); // memory pool L0
    }
    ret
}
unsafe extern "win64" fn Device_CreateCommittedResource(_t: *mut NullObj, hp: *const c_void, _hf: u32, desc: *const u8, _st: u32, _cv: *const c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(hp, desc, ppv)
}
unsafe extern "win64" fn Device_CreateCommittedResource1(_t: *mut NullObj, hp: *const c_void, _hf: u32, desc: *const u8, _st: u32, _cv: *const c_void, _ps: *mut c_void, _r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    new_resource(hp, desc, ppv)
}

/// Console slot 127 `CreateCommittedResourceX` with a barrier layout in the PC's
/// initial-state place (`real.rs`'s `dev_create_committed_resource_layout_x`).
///
/// A display target (`flags & ALLOW_DISPLAY`, console `D3D12_HEAP_FLAGS` value
/// `0x8`) gets its memory from the title's allocation hook, exactly as the
/// console runtime's committed-resource path does: the retail title registers
/// one display-target block and asserts that it is used up
/// (`exe+0x247ad10`/`+0x247af2e`, two 3840x2160 back buffers in a 64/82 MiB
/// region). The null device keeps those bytes as the resource's VA.
///
/// Every other committed resource keeps the plain [`new_resource`] path.
#[allow(clippy::too_many_arguments)]
unsafe extern "win64" fn Device_CreateCommittedResourceLayoutX(
    _t: *mut NullObj,
    props: *const c_void,
    flags: u32,
    desc: *const u8,
    _layout: u32,
    _clear: *const c_void,
    _protected: *mut c_void,
    _castable: u32,
    _formats: *const c_void,
    _riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    note_newer_list_layout(); // console-only device method (#127)
    const ALLOW_DISPLAY: u32 = 0x8;
    if flags & ALLOW_DISPLAY != 0 && !desc.is_null() && !ppv.is_null() {
        let x = (desc as *const [u8; 0x38]).read_unaligned();
        let size = crate::hle::xg_linux::Texture::new(&x).map_or(0, |t| t.size() as usize);
        if size > 0 {
            if let Some(p) = crate::hle::kernelx_linux::runtime_alloc(size, crate::hle::kernelx_linux::D3D_DISPLAY_TARGET) {
                let o = new_obj(Kind::Resource);
                std::ptr::copy_nonoverlapping(desc, (*o).desc.as_mut_ptr(), 56);
                *(*o).mem.lock().unwrap() = (p, size);
                (*o).value.store(p as u64, Ordering::Relaxed);
                *ppv = o as *mut c_void;
                static N: AtomicU32 = AtomicU32::new(0);
                if N.fetch_add(1, Ordering::Relaxed) < 8 {
                    let at = |o: usize| u32::from_le_bytes(x[o..o + 4].try_into().unwrap());
                    crate::xlog_info!(
                        "console display target {}x{} format {}: {size:#x} bytes from the title's memory at {p:#x}",
                        at(0x10),
                        at(0x18),
                        at(0x20)
                    );
                }
                return S_OK;
            }
        }
    }
    new_resource(props, desc, ppv)
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
    note_fence(o);
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
/// the name is the Vulkan adapter's when one backs the device, otherwise the
/// honest headless name.
unsafe extern "win64" fn Dxgi_GetAdapter(this: *mut NullObj, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    let o = new_obj(Kind::DxgiAdapter);
    let mut name: &[u8] = b"XCL console adapter";
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

/// device #133 `CreatePlacedResourceX` with a barrier layout (`D3D12_BARRIER_LAYOUT`)
/// where the initial state sits (`real.rs`'s `create_placed_resource_layout_x`).
///
/// The retail title creates its streamed textures through its renderer with this
/// console form and keeps the returned resource in the texture wrapper's `+0x20`;
/// that field is later read back through `ID3D12Resource::GetDesc` (slot 10), so a
/// stub that leaves the out pointer untouched makes the title call `[null+0x50]`
/// (`exe+0x501f808`). This is the same title memory as [`Device_CreatePlacedResourceX`]
/// (slot #44): the layout only replaces the initial state, which the null device
/// does not consume.
#[allow(clippy::too_many_arguments)]
unsafe extern "win64" fn Device_CreatePlacedResourceLayoutX(
    this: *mut NullObj,
    va: u64,
    desc: *const u8,
    _layout: u32,
    clear: *const c_void,
    riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    note_newer_list_layout(); // console-only device method (#133)
    Device_CreatePlacedResourceX(this, va, desc, 0, clear, riid, ppv)
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
                let mut base = pso_store().lock().unwrap().get(&(_base as usize)).cloned().unwrap_or_default();
                // the patch carries a full D3D12_BLEND_DESC at +4: RT0's
                // write mask sits at +4+44 (see record_graphics_pso).
                let patch_len = 0x14Cusize;
                let bytes = std::slice::from_raw_parts(patch, patch_len.min(4096));
                if bytes.len() > 4 + 44 {
                    base.rt0_mask = bytes[4 + 44];
                }
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
/// copy from a `PlacedResource` with no D3D12 flags detiles at record time;
/// a whole-subresource RGBA8 copy from a committed render target stashes a
/// pending copy that Execute fills from the S5 offscreen pixels. Anything
/// else (boxes, other formats) stays a silent no-op like the old stub.
unsafe extern "win64" fn List_CopyTextureRegion(
    this: *mut NullObj,
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
        // M2 draw S5: a committed 2D RGBA8 render target stashes a pending
        // copy for Execute (the pixels only exist after the S4 submit);
        // anything else stays a silent no-op like the old stub.
        if (*sob).kind == Kind::Resource && !this.is_null() {
            let sd = &(*sob).desc;
            let is_rt = (sd.as_ptr() as *const u32).read_unaligned() == 3
                && (sd.as_ptr().add(32) as *const u32).read_unaligned() == 28;
            if is_rt {
                let c = PendingCopy {
                    dst: dob as usize,
                    offset: (dst.add(16) as *const u64).read_unaligned(),
                    pitch: (dst.add(40) as *const u32).read_unaligned(),
                    height: (dst.add(32) as *const u32).read_unaligned(),
                };
                list_store().lock().unwrap().entry(this as usize).or_default().pending_copies.push(c);
            }
        }
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

/// `ExecuteIndirectX(signature, max_count, argument_buffer, offset, count_buffer, count_offset)`
/// (newer layout #61): the null backend has one coherent queue and records no
/// GPU work, so the indirect draw/dispatch is acknowledged without replay.
unsafe extern "win64" fn VulList_ExecuteIndirect(
    _t: *mut NullObj,
    _sig: *mut c_void,
    _max: u32,
    _args: *const c_void,
    _args_off: u64,
    _count: *const c_void,
    _count_off: u64,
) -> HRESULT {
    S_OK
}

/// `OMSetDepthBounds(min, max)` (older #68 / newer #70): store-only raster
/// state, like `RSSetDepthBiasX` — no rasteriser consumes it on the null
/// backend.
unsafe extern "win64" fn List_OMSetDepthBounds(this: *mut NullObj, min: f32, max: f32) {
    if this.is_null() {
        return;
    }
    list_store().lock().unwrap().entry(this as usize).or_default().depth_bounds = Some([min, max]);
}

/// Console #32 `SetDescriptorHeaps(count, heaps)`: the null backend owns no
/// descriptor heaps, so the call is acknowledged (the title only needs S_OK).
unsafe extern "win64" fn List_SetDescriptorHeapsX(_t: *mut NullObj, _n: u32, _heaps: *const usize) -> HRESULT {
    S_OK
}

/// Newer layout #139 `Reset(allocator, initial_pso, flags)` — the console's
/// three-argument Reset. On the null backend Reset only re-opens the list (the
/// PC #10 path acknowledges); the title passes a NULL pipeline on the lists it
/// takes from its pool.
unsafe extern "win64" fn List_ResetX(this: *mut NullObj, _alloc: *mut c_void, pso: *mut c_void, _flags: u64) -> HRESULT {
    if !this.is_null() {
        let mut ls = list_store().lock().unwrap();
        let e = ls.entry(this as usize).or_default();
        e.pso = pso as usize;
        e.closed = false;
    }
    S_OK
}

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
/// Console queue #16: the newer console layout shifts Signal from #14 to #16
/// (two console methods sit before it), so a title call there passes
/// `(fence, value)`; the PC order has `GetTimestampFrequency(out)` instead.
/// Retail `exe+0x2482608` signalled through this slot — the frequency write used
/// to land on the fence object's vtable and the title's later completed-value
/// poll called `0x989680` as a function pointer at `exe+0x4fb0aec`.
unsafe extern "win64" fn Queue_Slot16(this: *mut NullObj, a1: *mut c_void, a2: u64) -> HRESULT {
    if is_title_fence(a1) {
        note_newer_list_layout();
        Queue_Signal(this, a1 as *mut NullObj, a2)
    } else {
        Queue_GetTimestampFrequency(this, a1 as *mut u64)
    }
}

/// Console queue #17: `Wait(fence, value)` on the newer layout, the PC order's
/// `GetClockCalibration(gpu, cpu)` on the older one. The null GPU completes
/// everything instantly, so Wait is `S_OK`.
unsafe extern "win64" fn Queue_Slot17(this: *mut NullObj, a1: *mut c_void, a2: u64) -> HRESULT {
    if is_title_fence(a1) {
        note_newer_list_layout();
        S_OK
    } else {
        Queue_GetClockCalibration(this, a1 as *mut u64, a2 as usize as *mut u64)
    }
}

/// Console fence #12: the address of the fence's completed value, which the
/// title polls with `monitorx`/`mwaitx` (Windows `fence_x::completed_value_address`
/// gives each fence a separate cell kept equal to `GetCompletedValue`). The null
/// GPU completes instantly and every signal updates the fence's own `value`, so
/// that field is the cell: no follower thread and no second allocation.
unsafe extern "win64" fn Fence_CompletedValueAddress(this: *mut NullObj) -> *const u64 {
    (*this).value.as_ptr()
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

/// Console newer queue #35 `ExecuteCommandListsX(count, lists, value, flags)`;
/// the null device only needs the lists the title submitted (the extra value
/// word is a console wait token).
unsafe extern "win64" fn Queue_ExecuteCommandListsX(
    this: *mut NullObj,
    count: u32,
    lists: *const *mut NullObj,
    _value: u64,
    _flags: u32,
) {
    Queue_ExecuteCommandLists(this, count, lists)
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

    /// S4/S5 draw tests share process globals (`EXECUTE_FLUSHES`,
    /// `last_execute_pixels`): hold this across each test so their exact
    /// delta asserts can't interleave under `--test-threads=N`.
    fn draw_serial() -> &'static std::sync::Mutex<()> {
        static S: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        S.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// `TITLE_HOOKS` is process-global: every test that installs a synthetic
    /// title allocator holds this across its phases.
    fn title_hooks_serial() -> &'static std::sync::Mutex<()> {
        static S: std::sync::LazyLock<std::sync::Mutex<()>> = std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
        &S
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

    /// The retail title calls slots 26/25 the C way: `rcx` = result buffer,
    /// `rdx` = device (`exe+0x247870d`, `exe+0x14034413f`). Reading `rcx` as
    /// `this` wrote the struct over the `NullObj` header — gdb showed
    /// `[device] = 0x0000000100000004` and the next call faulting at
    /// `0x1000000d4`. windows-rs (the test title) passes `(this, ret, …)`, so
    /// both orders must fill the caller's buffer and leave the object alone.
    #[test]
    fn heap_properties_and_allocation_info_use_a_hidden_result_buffer() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let vtable_word = || (dev as *const usize).read();
            let before = vtable_word();

            // Each pair is (which argument, 0 = result buffer, 1 = device):
            // retail, then windows-rs.
            type HeapProps = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32) -> *mut u8;
            let heap_props: HeapProps = std::mem::transmute(slot(dev, 26));
            for buf_first in [true, false] {
                let mut props = [0u8; 20];
                let buf = props.as_mut_ptr() as *mut c_void;
                let (c, d) = if buf_first { (buf, dev) } else { (dev, buf) };
                assert_eq!(heap_props(c, d, 1, 2), props.as_mut_ptr(), "slot 26 returned the wrong buffer");
                assert_eq!(u32::from_le_bytes(props[0..4].try_into().unwrap()), 4, "CUSTOM");
                assert_eq!(u32::from_le_bytes(props[4..8].try_into().unwrap()), 3, "UPLOAD page property");
                assert_eq!(u32::from_le_bytes(props[8..12].try_into().unwrap()), 1, "memory pool L0");
                assert_eq!(vtable_word(), before, "slot 26 wrote over the device header");
                assert_eq!((*(dev as *mut NullObj)).kind, Kind::Device);
            }

            type AllocInfo = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32, *const u8) -> *mut u8;
            let alloc: AllocInfo = std::mem::transmute(slot(dev, 25));
            let mut desc = [0u8; 56];
            desc[0] = 1; // BUFFER
            desc[16..24].copy_from_slice(&0x1234u64.to_le_bytes());
            for buf_first in [true, false] {
                let mut info = [0u8; 16];
                let buf = info.as_mut_ptr() as *mut c_void;
                let (c, d) = if buf_first { (buf, dev) } else { (dev, buf) };
                assert_eq!(alloc(c, d, 1, 1, desc.as_ptr()), info.as_mut_ptr(), "slot 25 returned the wrong buffer");
                assert_eq!(u64::from_le_bytes(info[0..8].try_into().unwrap()), 0x1_0000);
                assert_eq!(u64::from_le_bytes(info[8..16].try_into().unwrap()), 0x1_0000);
                assert_eq!(vtable_word(), before, "slot 25 wrote over the device header");
                assert_eq!((*(dev as *mut NullObj)).kind, Kind::Device);
            }
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

    /// Step 42: the retail title's shader-visible descriptor heap must start in the
    /// memory the title's own allocation hook hands out (its arena base), not in a
    /// fresh graphics-pool range. A hook answer outside the pool is rejected and
    /// returned to the title, and the heap falls back to the window-0 range.
    #[test]
    fn descriptor_heap_memory_prefers_the_title_allocation_hook() {
        use std::sync::atomic::AtomicUsize;
        static TEST_BLOCK: AtomicUsize = AtomicUsize::new(0);
        static TEST_BYTES: AtomicUsize = AtomicUsize::new(0);
        static FREED: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "win64" fn alloc(size: usize, _attrs: u64) -> *mut c_void {
            let want = TEST_BYTES.load(Ordering::Relaxed);
            if want != 0 && size == want {
                TEST_BLOCK.load(Ordering::Relaxed) as *mut c_void
            } else {
                std::ptr::null_mut()
            }
        }
        unsafe extern "win64" fn free(p: *mut c_void, _attrs: u64) {
            FREED.store(p as usize, Ordering::Relaxed);
        }
        struct ClearHooks;
        impl Drop for ClearHooks {
            fn drop(&mut self) {
                unsafe { crate::hle::kernelx_linux::set_title_hooks_for_test(0, 0) };
            }
        }
        let _serial = title_hooks_serial().lock().unwrap();
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create_heap: Fn4 = std::mem::transmute(slot(dev, 14));
            let start_of = |h: *mut c_void| {
                let f: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(h, 9));
                f(h)
            };
            let desc = [0u32, 7, 1, 0]; // CBV_SRV_UAV, 7 slots, shader visible => 224 bytes

            // 1) the title's hook supplies the heap memory: the handle is exactly it
            let block = crate::hle::kernelx_linux::gfx_alloc_window0(7 * 32, 0x1_0000, true)
                .expect("graphics-pool block for the synthetic title allocator");
            TEST_BLOCK.store(block, Ordering::Relaxed);
            TEST_BYTES.store(7 * 32, Ordering::Relaxed);
            crate::hle::kernelx_linux::set_title_hooks_for_test(alloc as usize, free as usize);
            let _clear = ClearHooks;
            let mut h = std::ptr::null_mut();
            assert_eq!(create_heap(dev, desc.as_ptr() as *const c_void, std::ptr::null(), &mut h), S_OK);
            assert_eq!(start_of(h), block as u64, "the heap starts in the title's memory");

            // 2) a hook answer outside the pool is refused, freed, and the window-0
            //    fallback still gives a nonzero start that is not the title's answer
            let outside_buf = [0u8; 7 * 32];
            let outside = outside_buf.as_ptr() as usize;
            TEST_BLOCK.store(outside, Ordering::Relaxed);
            FREED.store(0, Ordering::Relaxed);
            let mut h2 = std::ptr::null_mut();
            assert_eq!(create_heap(dev, desc.as_ptr() as *const c_void, std::ptr::null(), &mut h2), S_OK);
            let fallback = start_of(h2);
            assert_eq!(FREED.load(Ordering::Relaxed), outside, "the refused title memory goes back");
            assert_ne!(fallback, 0);
            assert_ne!(fallback, outside as u64);
            assert!(fallback >> 32 >= 4 && (fallback >> 32) & 3 == 0, "fallback {fallback:#x} is not window 0");
        }
    }

    /// Step 43: a console display target (`#127`, `CreateCommittedResourceX`
    /// with a barrier layout, `ALLOW_DISPLAY = 0x8`) takes its memory from the
    /// title's allocation hook with `D3D_DISPLAY_TARGET`, which is the event
    /// the title's registered display-target region bumps on (the retail title
    /// asserts at `exe+0x247af2e` that the region is used up).
    ///
    /// RED before the slot is routed: the logging stub returns S_OK but writes
    /// no resource and never calls the title's allocator.
    #[test]
    fn display_target_memory_comes_from_the_title_allocation_hook() {
        let _layout = LayoutSerial::take();
        use std::sync::atomic::{AtomicU64, AtomicUsize};
        static TEST_BLOCK: AtomicUsize = AtomicUsize::new(0);
        static TEST_BYTES: AtomicUsize = AtomicUsize::new(0);
        static ALLOC_SIZE: AtomicUsize = AtomicUsize::new(0);
        static ALLOC_ATTRS: AtomicU64 = AtomicU64::new(0);
        unsafe extern "win64" fn alloc(size: usize, attrs: u64) -> *mut c_void {
            ALLOC_SIZE.store(size, Ordering::Relaxed);
            ALLOC_ATTRS.store(attrs, Ordering::Relaxed);
            if size == TEST_BYTES.load(Ordering::Relaxed) {
                TEST_BLOCK.load(Ordering::Relaxed) as *mut c_void
            } else {
                std::ptr::null_mut()
            }
        }
        unsafe extern "win64" fn free(_p: *mut c_void, _attrs: u64) {}
        struct ClearHooks;
        impl Drop for ClearHooks {
            fn drop(&mut self) {
                unsafe { crate::hle::kernelx_linux::set_title_hooks_for_test(0, 0) };
            }
        }
        let _serial = title_hooks_serial().lock().unwrap();
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            // console D3D12_RESOURCE_DESC of the retail back buffer: 3840x2160
            // R8G8B8A8_UNORM, 1 mip / 1 array slice, console flags +0x30.
            let mut d = [0u8; 0x38];
            d[0..4].copy_from_slice(&3u32.to_le_bytes()); // D3D12_RESOURCE_DIMENSION_TEXTURE2D
            d[0x10..0x18].copy_from_slice(&3840u64.to_le_bytes());
            d[0x18..0x1c].copy_from_slice(&2160u32.to_le_bytes());
            d[0x1c..0x1e].copy_from_slice(&1u16.to_le_bytes());
            d[0x1e..0x20].copy_from_slice(&1u16.to_le_bytes());
            d[0x20..0x24].copy_from_slice(&28u32.to_le_bytes()); // DXGI_FORMAT_R8G8B8A8_UNORM
            d[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
            d[0x30..0x34].copy_from_slice(&0x30_0005u32.to_le_bytes());
            let size = crate::hle::xg_linux::Texture::new(&d).expect("console 4K RGBA8 desc").size() as usize;
            assert!(size > 0);
            let block = crate::hle::kernelx_linux::gfx_alloc_window0(size, 0x1_0000, true)
                .expect("graphics-pool block for the synthetic title allocator");
            TEST_BYTES.store(size, Ordering::Relaxed);
            TEST_BLOCK.store(block, Ordering::Relaxed);
            crate::hle::kernelx_linux::set_title_hooks_for_test(alloc as usize, free as usize);
            let _clear = ClearHooks;
            type CreateLayout = unsafe extern "win64" fn(
                *mut c_void,
                *const c_void,
                u32,
                *const u8,
                u32,
                *const c_void,
                *mut c_void,
                u32,
                *const c_void,
                *const c_void,
                *mut *mut c_void,
            ) -> HRESULT;
            let create: CreateLayout = std::mem::transmute(slot(dev, 127));
            let props = [1u32, 0, 0, 1, 1]; // D3D12_HEAP_PROPERTIES DEFAULT
            let mut res: *mut c_void = std::ptr::null_mut();
            let hr = create(
                dev,
                props.as_ptr() as *const c_void,
                0x8, // console D3D12_HEAP_FLAG_ALLOW_DISPLAY
                d.as_ptr(),
                3, // barrier layout
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                &mut res,
            );
            assert_eq!(hr, S_OK);
            assert!(!res.is_null(), "the display target resource was not created");
            assert_eq!(ALLOC_ATTRS.load(Ordering::Relaxed), crate::hle::kernelx_linux::D3D_DISPLAY_TARGET);
            assert_eq!(ALLOC_SIZE.load(Ordering::Relaxed), size);
            let o = res as *mut NullObj;
            assert_eq!((*o).value.load(Ordering::Relaxed), block as u64, "the resource VA is the title's memory");
            assert_eq!(*(*o).mem.lock().unwrap(), (block, size));
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

    /// Retail's console device #24 `CopyDescriptorsSimple(n, dst, src, type)`:
    /// `exe+0x4ecc87e` copies the descriptor #70 just staged (n = 1) into the
    /// binding heap, 113 calls in one fresh-root run. On Linux a handle is a
    /// host address in a heap from `Device_CreateDescriptorHeap`, and every slot
    /// is 32 bytes (`GetDescriptorHandleIncrementSize`), so the call is a byte
    /// copy of the slot range. RED before the slot is routed: the logging stub
    /// answers S_OK and leaves the destination sentinel untouched.
    #[test]
    fn console_copy_descriptors_simple_copies_heap_slots() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let src = crate::hle::kernelx_linux::gfx_alloc_window0(3 * 32, 0x1_0000, true).expect("source heap range");
            let dst = crate::hle::kernelx_linux::gfx_alloc_window0(3 * 32, 0x1_0000, true).expect("destination heap range");
            let words = [0x1111_1111u32, 0x2222_2222, 0x3333_3333, 0x4444_4444, 0x5555_5555, 0x6666_6666, 0x7777_7777, 0x8888_8888];
            for (k, w) in words.iter().enumerate() {
                (src as *mut u32).add(k).write_unaligned(*w);
            }
            std::ptr::write_bytes(dst as *mut u8, 0x5A, 3 * 32);

            let copy: unsafe extern "win64" fn(*mut c_void, u32, u64, u64, u32) -> HRESULT =
                std::mem::transmute(slot(dev, 24));
            assert_eq!(copy(dev, 2, dst as u64, src as u64, 0), S_OK);
            let got: [u32; 8] = std::ptr::read_unaligned(dst as *const [u32; 8]);
            assert_eq!(got, words, "two 32-byte slots copied");
            assert_eq!((dst as *const u32).add(16).read_unaligned(), 0x5A5A_5A5A, "the slot after the copied range is untouched");

            // A destination outside the graphics pool is refused, not written.
            let local = [0x5A5A_5A5Au32; 8];
            assert_eq!(copy(dev, 1, local.as_ptr() as u64, src as u64, 0), E_INVALIDARG);
            assert_eq!(local, [0x5A5A_5A5Au32; 8], "a refused copy writes nothing");
        }
    }

    #[test]
    fn console_copy_descriptors_simple_is_routed() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            assert_eq!(slot(dev, 24), Device_CopyDescriptorsSimple as usize);
        }
    }

    /// One console `CreateShaderResourceViewAt` view: the console layout has
    /// Format u32 @0, ViewDimension u32 @4, Shader4ComponentMapping u32 @8
    /// (0x1688 = identity), the dimension union @0x10 and the GPU address @0x28.
    fn srv_at_view(format: u32, dim: u32, union: &[u8], va: u64) -> [u8; 0x30] {
        let mut v = [0u8; 0x30];
        v[0..4].copy_from_slice(&format.to_le_bytes());
        v[4..8].copy_from_slice(&dim.to_le_bytes());
        v[8..12].copy_from_slice(&0x1688u32.to_le_bytes());
        v[0x10..0x10 + union.len()].copy_from_slice(union);
        v[0x28..0x30].copy_from_slice(&va.to_le_bytes());
        v
    }

    /// Retail's console device #70 `CreateShaderResourceViewAt` on the
    /// raw-memory BUFFER staging path (`exe+0x4ecc81b`): the staging helper
    /// passes a BUFFER resource desc, a BUFFER view (FirstElement 16, 64
    /// elements, RAW) and a GPU address; the handler writes the console V# in
    /// the title's 32-byte slot. RED before routing: the logging stub leaves
    /// the sentinel.
    #[test]
    fn console_create_srv_at_writes_buffer_v_words() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let cpu = crate::hle::kernelx_linux::gfx_alloc_window0(32, 0x1_0000, true).expect("descriptor slot");
            std::ptr::write_bytes(cpu as *mut u8, 0x5A, 32);
            let mut rdesc = [0u8; 56];
            rdesc[0..4].copy_from_slice(&1u32.to_le_bytes()); // BUFFER
            rdesc[16..24].copy_from_slice(&256u64.to_le_bytes());
            let mut union = [0u8; 0x18];
            union[0..8].copy_from_slice(&16u64.to_le_bytes()); // FirstElement
            union[8..12].copy_from_slice(&64u32.to_le_bytes()); // NumElements
            union[12..16].copy_from_slice(&0u32.to_le_bytes()); // StructureByteStride
            union[16..20].copy_from_slice(&1u32.to_le_bytes()); // RAW
            let va = 0x10_2000_0000u64;
            let view = srv_at_view(41, 1, &union, va); // DXGI_FORMAT_R32_FLOAT
            type SrvAt = unsafe extern "win64" fn(*mut c_void, *const u8, *const u8, u64);
            let srv: SrvAt = std::mem::transmute(slot(dev, 70));
            srv(dev, rdesc.as_ptr(), view.as_ptr(), cpu as u64);

            let words: [u32; 8] = std::ptr::read_unaligned(cpu as *const [u32; 8]);
            let b = crate::gpu::desc::buffer(&[words[0], words[1], words[2], words[3]]);
            assert_eq!((b.address, b.stride, b.num_records, b.format), (va + 64, 0, 256, 22), "V# of the raw buffer");
            assert_eq!(b.dst_sel, [4, 5, 6, 7]);
            assert_eq!(&words[4..], &[0u32; 4], "the rest of the console slot is zeroed");
        }
    }

    /// The texture half: a 2D view of cached mips 1..3 of a 256×128, 8-mip
    /// RGBA8 resource. The console T# carries the GPU address, GFX10 format 56
    /// (R8G8B8A8_UNORM), base/last/max mip 1/3/7 and the identity DST_SEL.
    #[test]
    fn console_create_srv_at_writes_texture_t_sharp() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let cpu = crate::hle::kernelx_linux::gfx_alloc_window0(32, 0x1_0000, true).expect("descriptor slot");
            std::ptr::write_bytes(cpu as *mut u8, 0x5A, 32);
            let mut rdesc = [0u8; 56];
            rdesc[0..4].copy_from_slice(&3u32.to_le_bytes()); // TEXTURE2D
            rdesc[16..24].copy_from_slice(&256u64.to_le_bytes());
            rdesc[24..28].copy_from_slice(&128u32.to_le_bytes());
            rdesc[28..30].copy_from_slice(&1u16.to_le_bytes()); // DepthOrArraySize
            rdesc[30..32].copy_from_slice(&8u16.to_le_bytes()); // MipLevels
            rdesc[32..36].copy_from_slice(&28u32.to_le_bytes()); // R8G8B8A8_UNORM
            rdesc[36..40].copy_from_slice(&1u32.to_le_bytes()); // SampleDesc.Count
            let mut union = [0u8; 8];
            union[0..4].copy_from_slice(&1u32.to_le_bytes()); // MostDetailedMip
            union[4..8].copy_from_slice(&3u32.to_le_bytes()); // MipLevels
            let va = 0x10_4000_0000u64;
            let view = srv_at_view(28, 4, &union, va); // TEXTURE2D
            type SrvAt = unsafe extern "win64" fn(*mut c_void, *const u8, *const u8, u64);
            let srv: SrvAt = std::mem::transmute(slot(dev, 70));
            srv(dev, rdesc.as_ptr(), view.as_ptr(), cpu as u64);

            let words: [u32; 8] = std::ptr::read_unaligned(cpu as *const [u32; 8]);
            let i = crate::gpu::desc::image(&words).expect("a console T#");
            assert_eq!((i.address, i.format, i.width, i.height, i.depth), (va, 56, 256, 128, 1));
            assert_eq!((i.ty, i.base_level, i.last_level, i.max_mip), (crate::gpu::desc::ImageType::D2, 1, 3, 7));
            assert_eq!(i.dst_sel, [4, 5, 6, 7]);
        }
    }

    /// No write outside the graphics pool, and no descriptor for a null GPU
    /// address: a wild view must not put bytes into host memory.
    #[test]
    fn console_create_srv_at_refuses_bad_slots_and_null_addresses() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let mut rdesc = [0u8; 56];
            rdesc[0..4].copy_from_slice(&1u32.to_le_bytes());
            let mut union = [0u8; 0x18];
            union[8..12].copy_from_slice(&64u32.to_le_bytes());
            let mut view = srv_at_view(41, 1, &union, 0x10_2000_0000);
            type SrvAt = unsafe extern "win64" fn(*mut c_void, *const u8, *const u8, u64);
            let srv: SrvAt = std::mem::transmute(slot(dev, 70));

            let local = [0x5A5A_5A5Au32; 8];
            srv(dev, rdesc.as_ptr(), view.as_ptr(), local.as_ptr() as u64);
            assert_eq!(local, [0x5A5A_5A5Au32; 8], "a slot outside the pool is not written");

            let cpu = crate::hle::kernelx_linux::gfx_alloc_window0(32, 0x1_0000, true).expect("descriptor slot");
            std::ptr::write_bytes(cpu as *mut u8, 0x5A, 32);
            view[0x28..0x30].fill(0);
            srv(dev, rdesc.as_ptr(), view.as_ptr(), cpu as u64);
            let sentinel: [u32; 8] = std::ptr::read_unaligned(cpu as *const [u32; 8]);
            assert_eq!(sentinel, [0x5A5A_5A5Au32; 8], "a null GPU address writes no descriptor");
        }
    }

    #[test]
    fn console_create_srv_at_is_routed() {
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            assert_eq!(slot(dev, 70), Device_CreateShaderResourceViewAt as usize);
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
            // a real console blend patch is a full D3D12_BLEND_DESC at +4
            // (the S6 replay reads RT0's write mask from it — short probes
            // would over-read, so the patch must be well-formed).
            let mut patch = vec![0u8; 0x14C];
            patch[0] = 2;
            patch[4 + 44] = 0xF;
            let mut d2 = std::ptr::null_mut();
            assert_eq!(derive(dev, pso, 1, patch.as_ptr(), std::ptr::null(), &mut d2), S_OK);
            assert!(!d2.is_null());
            assert_eq!(super::pso_record_for(d2 as usize).map(|r| r.rt0_mask), Some(0xF));
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

    /// Console device slot 62 is `CreateComputePipelineState` with two extra
    /// arguments (desc, a, b, riid, out) — XboxBvhBuilder's call order — not the
    /// PC `CreateStateObject`. The out pointer is the sixth argument; before the
    /// console mapping `r9` (b) was read as `ppv`, so a null b gave S_FALSE and
    /// left the caller's out slot unwritten.
    #[test]
    fn console_compute_pso_slot62_writes_the_sixth_argument() {
        type ComputeX = unsafe extern "win64" fn(
            *mut c_void,
            *const u8,
            usize,
            usize,
            *const c_void,
            *mut *mut c_void,
        ) -> HRESULT;
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create: ComputeX = std::mem::transmute(slot(dev, 62));
            let desc = [0u8; 0x38]; // D3D12_COMPUTE_PIPELINE_STATE_DESC
            let mut out = std::ptr::null_mut();
            assert_eq!(create(dev, desc.as_ptr(), 0, 0, std::ptr::null(), &mut out), S_OK);
            assert!(!out.is_null(), "slot 62 did not write the sixth argument");
            assert_eq!((*(out as *mut NullObj)).kind, Kind::PipelineState);
            let mut out2 = std::ptr::null_mut();
            assert_eq!(create(dev, desc.as_ptr(), 1, 1, std::ptr::null(), &mut out2), S_OK);
            assert!(!out2.is_null(), "the two console extras shifted the out argument");
            assert_eq!((*(out2 as *mut NullObj)).kind, Kind::PipelineState);
        }
    }

    #[test]
    fn console_resource_allocation_info2_writes_offsets_and_total() {
        let _layout = LayoutSerial::take();
        // Console device #108 is GetResourceAllocationInfo2: a hidden return
        // buffer first, then the device, mask, count, an array of 0x48-byte
        // resource descs, and an optional {offset, alignment, size} per
        // resource. The title asserts the returned size's high word is zero.
        type Info2 = unsafe extern "win64" fn(
            *mut c_void,
            *mut c_void,
            u32,
            u32,
            *const u8,
            *mut [u64; 3],
        ) -> *mut c_void;
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let f: Info2 = std::mem::transmute(slot(dev, 108));
            let mut descs = [0u8; 0x48 * 2];
            (descs.as_mut_ptr() as *mut u32).write_unaligned(1); // DIMENSION_BUFFER
            (descs.as_mut_ptr().add(16) as *mut u64).write_unaligned(0x12345);
            (descs.as_mut_ptr().add(0x48) as *mut u32).write_unaligned(1);
            (descs.as_mut_ptr().add(0x48 + 16) as *mut u64).write_unaligned(0x20);
            let mut out = [u64::MAX; 2];
            let mut infos = [[u64::MAX; 3]; 2];
            let r = f(
                out.as_mut_ptr() as *mut c_void,
                dev,
                1,
                2,
                descs.as_ptr(),
                infos.as_mut_ptr(),
            );
            assert_eq!(r as usize, out.as_mut_ptr() as usize, "slot 108 must return the hidden buffer");
            assert_eq!(out[0], 0x30000, "total size of the two 64 KiB-rounded buffers");
            assert_eq!(out[1], 0x10000, "alignment");
            assert_eq!(infos, [[0, 0x10000, 0x20000], [0x20000, 0x10000, 0x10000]]);
        }
    }

    #[test]
    fn console_deserialize_pso_slots_return_e_notimpl_and_null_the_out() {
        // Windows' #57/#67 Deserialize{Graphics,Compute}PipelineStateX returns
        // E_NOTIMPL and nulls the out pointer: console pipeline caches are not
        // supported, and the title creates the pipeline from its desc instead.
        // A stub returning S_OK leaves a garbage out pointer the title uses.
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            for s in [57usize, 67] {
                let f: unsafe extern "win64" fn(*mut c_void, *const u8, *const c_void, *mut *mut c_void) -> HRESULT =
                    std::mem::transmute(slot(dev, s));
                let mut out = 0xdead_usize as *mut c_void;
                assert_eq!(f(dev, std::ptr::null(), std::ptr::null(), &mut out), E_NOTIMPL, "slot {s}");
                assert!(out.is_null(), "slot {s} must clear the out pointer");
            }
        }
    }

    #[test]
    fn console_create_pipeline_state_slot86_returns_a_pso() {
        // Windows' #86 CreatePipelineState(stream desc, riid, pipeline) creates
        // the PC pipeline; the null device hands back a PipelineState object.
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let f: unsafe extern "win64" fn(*mut c_void, *const u8, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 86));
            let desc = [0u8; 0x38];
            let mut out = std::ptr::null_mut();
            assert_eq!(f(dev, desc.as_ptr(), std::ptr::null(), &mut out), S_OK);
            assert!(!out.is_null());
            assert_eq!((*(out as *mut NullObj)).kind, Kind::PipelineState);
        }
    }

    /// Step 49: device #133 console `CreatePlacedResourceX(va, desc, layout,
    /// clear, riid, out)`. The retail title keeps the returned resource in its
    /// texture wrapper (`+0x20`) and reads it back with
    /// `ID3D12Resource::GetDesc` (slot 10); the logging stub returned `S_OK`
    /// with the out pointer untouched, so the title's `call [null+0x50]`
    /// faulted at `exe+0x501f808`.
    #[test]
    fn console_placed_resource_layout_slot133_writes_the_resource() {
        let _layout = LayoutSerial::take();
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let f: unsafe extern "win64" fn(*mut c_void, u64, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 133));
            // console D3D12_RESOURCE_DESC of the streamed texture (TEXTURE2D,
            // 256x256 RGBA8, console flags +0x30).
            let mut d = [0u8; 0x38];
            d[0..4].copy_from_slice(&3u32.to_le_bytes());
            d[0x10..0x18].copy_from_slice(&256u64.to_le_bytes());
            d[0x18..0x1c].copy_from_slice(&256u32.to_le_bytes());
            d[0x1c..0x1e].copy_from_slice(&1u16.to_le_bytes());
            d[0x1e..0x20].copy_from_slice(&1u16.to_le_bytes());
            d[0x20..0x24].copy_from_slice(&28u32.to_le_bytes());
            d[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
            d[0x30..0x34].copy_from_slice(&0x30_0005u32.to_le_bytes());
            let va = 0x1_2345_6780_u64;
            let mut out = std::ptr::null_mut();
            assert_eq!(f(dev, va, d.as_ptr(), 6, std::ptr::null(), std::ptr::null(), &mut out), S_OK);
            assert!(!out.is_null(), "slot 133 must write the resource");
            assert_eq!((*(out as *mut NullObj)).kind, Kind::PlacedResource);
            // the title's next call is ID3D12Resource::GetDesc on that object
            let gpu: unsafe extern "win64" fn(*mut c_void) -> u64 = std::mem::transmute(slot(out, 11));
            assert_eq!(gpu(out), va, "the placed resource reports the title's address");
            let desc: unsafe extern "win64" fn(*mut c_void, *mut u8) -> *mut u8 = std::mem::transmute(slot(out, 10));
            let mut got = [0u8; 0x38];
            desc(out, got.as_mut_ptr());
            assert_eq!(got, d, "GetDesc round-trips the console description");
        }
    }

    #[test]
    fn console_queue_timestamp_frequency_and_clock_calibration() {
        // The title's runtime uses the newer console queue layout: #18 is
        // GetTimestampFrequency (out pointer in rdx, HRESULT), not the PC
        // #18 GetDesc. exe+0x2484180 checks the HRESULT and treats the out
        // value as a frequency, then divides 1 by it.
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create_queue: Fn4 = std::mem::transmute(slot(dev, 8));
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(dev, std::ptr::null(), std::ptr::null(), &mut q), S_OK);
            let freq: unsafe extern "win64" fn(*mut c_void, *mut u64) -> HRESULT = std::mem::transmute(slot(q, 18));
            let mut hz = 0u64;
            assert_eq!(freq(q, &mut hz), S_OK);
            assert_eq!(hz, 10_000_000);
            let clock: unsafe extern "win64" fn(*mut c_void, *mut u64, *mut u64) -> HRESULT =
                std::mem::transmute(slot(q, 19));
            let (mut gpu, mut cpu) = (0u64, 0u64);
            assert_eq!(clock(q, &mut gpu, &mut cpu), S_OK);
            assert_ne!(gpu, 0);
            assert_eq!(gpu, cpu);
        }
    }

    #[test]
    fn console_queue_signal_slot16_does_not_overwrite_the_fence() {
        // The newer console queue layout puts Signal(fence, value) at #16 (the
        // PC order has GetTimestampFrequency(out) there). Retail calls it at
        // exe+0x2482608 with the fence from its CreateFence; the frequency
        // handler wrote 10 MHz over the fence object's vtable and the title's
        // deferred-free poll then called it as a function pointer
        // (`exe+0x4fb0aec`, fault `0x9896e0`) — Signal must only bump the
        // fence's value.
        let _g = layout_serial().lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create_queue: Fn4 = std::mem::transmute(slot(dev, 8));
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(dev, std::ptr::null(), std::ptr::null(), &mut q), S_OK);
            let create_fence: unsafe extern "win64" fn(*mut c_void, u64, u32, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 36));
            let mut fence = std::ptr::null_mut();
            assert_eq!(create_fence(dev, 2, 0, std::ptr::null(), &mut fence), S_OK);
            let obj = fence as *mut NullObj;
            let vtbl = *fence.cast::<usize>();

            let signal: unsafe extern "win64" fn(*mut c_void, *mut c_void, u64) -> HRESULT =
                std::mem::transmute(slot(q, 16));
            assert_eq!(signal(q, fence, 7), S_OK);
            assert_eq!((*obj).value.load(Ordering::Acquire), 7, "console #16 is Signal");
            assert_eq!(*fence.cast::<usize>(), vtbl, "Signal must not write the frequency over the fence");

            let wait: unsafe extern "win64" fn(*mut c_void, *mut c_void, u64) -> HRESULT =
                std::mem::transmute(slot(q, 17));
            assert_eq!(wait(q, fence, 9), S_OK);
            assert_eq!((*obj).value.load(Ordering::Acquire), 7, "console #17 is Wait");
            assert_eq!(*fence.cast::<usize>(), vtbl);

            // the PC-order call shape at the same slots still answers the timing queries
            let freq: unsafe extern "win64" fn(*mut c_void, *mut u64) -> HRESULT = std::mem::transmute(slot(q, 16));
            let mut hz = 0u64;
            assert_eq!(freq(q, &mut hz), S_OK);
            assert_eq!(hz, 10_000_000);
            let clock: unsafe extern "win64" fn(*mut c_void, *mut u64, *mut u64) -> HRESULT =
                std::mem::transmute(slot(q, 17));
            let (mut gpu, mut cpu) = (0u64, 0u64);
            assert_eq!(clock(q, &mut gpu, &mut cpu), S_OK);
            assert_ne!(gpu, 0);
            assert_eq!(gpu, cpu);
            assert_eq!(*fence.cast::<usize>(), vtbl);
        }
    }

    #[test]
    fn fence_slot12_is_the_completed_value_address() {
        // Console fence #12 is the address of the completed value the title
        // polls (`monitorx`/`mwaitx`; Windows `fence_x::completed_value_address`).
        // The null GPU completes instantly, so a signal advances the same cell.
        let _g = layout_serial().lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut dev = std::ptr::null_mut();
            assert_eq!(D3D12CreateDevice(std::ptr::null_mut(), 0xC100, std::ptr::null(), &mut dev), S_OK);
            let create_queue: Fn4 = std::mem::transmute(slot(dev, 8));
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(dev, std::ptr::null(), std::ptr::null(), &mut q), S_OK);
            let create_fence: unsafe extern "win64" fn(*mut c_void, u64, u32, *const c_void, *mut *mut c_void) -> HRESULT =
                std::mem::transmute(slot(dev, 36));
            let mut fence = std::ptr::null_mut();
            assert_eq!(create_fence(dev, 2, 0, std::ptr::null(), &mut fence), S_OK);

            let cell: unsafe extern "win64" fn(*mut c_void) -> *const u64 = std::mem::transmute(slot(fence, 12));
            let p = cell(fence);
            assert!(!p.is_null(), "fence #12 must expose the completed value address");
            assert_eq!(*p, 2);

            let signal: unsafe extern "win64" fn(*mut c_void, *mut c_void, u64) -> HRESULT =
                std::mem::transmute(slot(q, 16));
            assert_eq!(signal(q, fence, 7), S_OK);
            assert_eq!(*cell(fence), 7);
            let cpu_signal: unsafe extern "win64" fn(*mut c_void, u64) -> HRESULT = std::mem::transmute(slot(fence, 10));
            assert_eq!(cpu_signal(fence, 9), S_OK);
            assert_eq!(*cell(fence), 9);
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
        let _g = layout_serial().lock().unwrap_or_else(|e| e.into_inner());
        NEWER_LIST_LAYOUT.store(false, Ordering::Release); // the older numbering below
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
        // the null device now answers the same way (step 40), and both keep
        // answering self for every non-DXGI IID.
        unsafe {
            type Qi = unsafe extern "win64" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut ndev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut ndev), S_OK);
            let nqi: Qi = std::mem::transmute(slot(ndev, 0));
            let mut p = std::ptr::null_mut();
            assert_eq!(nqi(ndev, IID_DXGI_DEVICE1.as_ptr() as *const c_void, &mut p), S_OK);
            assert_eq!((*(p as *mut NullObj)).kind, Kind::DxgiDevice);
            let mut selfq = std::ptr::null_mut();
            assert_eq!(nqi(ndev, IID_D3D12DEVICE.as_ptr() as *const c_void, &mut selfq), S_OK);
            assert_eq!(selfq, ndev, "non-DXGI IIDs still get the device itself");
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

    /// Step 40: retail asks the null device for `IID_IDXGIDevice` at
    /// `0x142481cd5` and then calls slot 7 (`GetAdapter`) on the answer. The
    /// answer must be the `DxgiDevice` wrapper, not the device (whose slot 7
    /// is `GetNodeCount`), and the wrapper chain must reach a real adapter,
    /// its LUID desc and output 0 in that call order.
    #[test]
    fn null_device_dxgi_chain_serves_adapter_and_output() {
        unsafe {
            type Qi = unsafe extern "win64" fn(*mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut dev = std::ptr::null_mut();
            assert_eq!(create_device_for(&sel, None, &mut dev), S_OK);
            let dqi: Qi = std::mem::transmute(slot(dev, 0));
            // retail's query: IID_IDXGIDevice on the null device
            let mut dx = std::ptr::null_mut();
            assert_eq!(dqi(dev, IID_DXGI_DEVICE.as_ptr() as *const c_void, &mut dx), S_OK);
            assert_ne!(dx, dev, "DXGI QI must not hand back the device");
            assert_eq!((*(dx as *mut NullObj)).kind, Kind::DxgiDevice);
            assert_eq!(
                (*(dx as *mut NullObj)).value.load(Ordering::Relaxed),
                dev as u64,
                "wrapper keeps the device it came from"
            );
            // wrapper identity: DXGI1 answers itself, D3D12 round-trips
            let wqi: Qi = std::mem::transmute(slot(dx, 0));
            let mut obj = std::ptr::null_mut();
            assert_eq!(wqi(dx, IID_DXGI_DEVICE1.as_ptr() as *const c_void, &mut obj), S_OK);
            assert_eq!(obj, dx, "wrapper answers DXGI1 with itself");
            let mut back = std::ptr::null_mut();
            assert_eq!(wqi(dx, IID_D3D12DEVICE.as_ptr() as *const c_void, &mut back), S_OK);
            assert_eq!(back, dev, "D3D12 identity round-trips to the null device");
            // slot 7 on the wrapper is GetAdapter, not GetNodeCount
            type GetAd = unsafe extern "win64" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
            let get_ad: GetAd = std::mem::transmute(slot(dx, 7));
            let mut ad = std::ptr::null_mut();
            assert_eq!(get_ad(dx, &mut ad), S_OK);
            assert!(!ad.is_null());
            assert_eq!((*(ad as *mut NullObj)).kind, Kind::DxgiAdapter);
            // adapter desc: named, LUID == the device's GetAdapterLuid (slot 43)
            type AdDesc = unsafe extern "win64" fn(*mut c_void, *mut u8) -> HRESULT;
            let get_desc: AdDesc = std::mem::transmute(slot(ad, 8));
            let mut desc = [0u8; 304];
            assert_eq!(get_desc(ad, desc.as_mut_ptr()), S_OK);
            assert_ne!(u16::from_le_bytes([desc[0], desc[1]]), 0, "description is named");
            let mut luid = 0u64;
            Device_GetAdapterLuid(dev as *mut NullObj, &mut luid);
            assert_eq!(&desc[296..304], &luid.to_le_bytes(), "adapter LUID == device LUID");
            // EnumOutputs(0) is a real output; 1 is NOT_FOUND
            type EnumOut = unsafe extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT;
            let outputs: EnumOut = std::mem::transmute(slot(ad, 7));
            let mut o = std::ptr::null_mut();
            assert_eq!(outputs(ad, 0, &mut o), S_OK);
            assert_eq!((*(o as *mut NullObj)).kind, Kind::DxgiOutput);
            let mut o1 = std::ptr::null_mut();
            assert_eq!(outputs(ad, 1, &mut o1), 0x887A_0002u32 as i32);
            assert!(o1.is_null());
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
            desc[super::GFX_BLEND_OFF + 44] = 0xF; // RT0 full write mask
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
            let _serial = draw_serial().lock().unwrap();
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
            desc[super::GFX_BLEND_OFF + 44] = 0xF; // RT0 full write mask
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

    /// M2 draw S5 failing-first: list #28/#29 count draws and #30 writes the
    /// occlusion counts into the results buffer, and a CopyTextureRegion from
    /// a committed render target stashes a pending copy that Execute fills
    /// from the S4 offscreen pixels (Map then reads green centre + blue
    /// clear). Fails pre-S5: #28–#30 are stubs (counts stay 0) and the
    /// committed-RT copy is a silent no-op (readback stays zero).
    #[test]
    fn draw_s5_query_resolve_and_readback() {
        unsafe {
            let _serial = draw_serial().lock().unwrap();
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut dev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut dev), S_OK);
            type MkCommitted = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: MkCommitted = std::mem::transmute(slot(dev, 27));
            let buf_desc = |bytes: u64| {
                // 8-aligned backing: approx_size reads Width with an aligned u64 load.
                let mut s = [0u64; 7];
                (s.as_mut_ptr() as *mut u32).write_unaligned(1); // BUFFER
                (s.as_mut_ptr().byte_add(16) as *mut u64).write_unaligned(bytes);
                s
            };
            let mut props_s = [0u64; 3];
            (props_s.as_mut_ptr() as *mut u32).write_unaligned(3); // READBACK
            let props = props_s.as_ptr() as *const c_void;
            let bd = buf_desc(24);
            let mut results = std::ptr::null_mut();
            assert_eq!(mk(dev, props, 0, (&bd as *const u64) as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut results), S_OK);
            type MkQ = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkq: MkQ = std::mem::transmute(slot(dev, 39));
            let mut qh = std::ptr::null_mut();
            assert_eq!(mkq(dev, std::ptr::null(), std::ptr::null(), &mut qh), S_OK);
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type Query = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32);
            type Resolve = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32, u32, *mut c_void, u64);
            let (begin, end): (Query, Query) = (std::mem::transmute(slot(l, 28)), std::mem::transmute(slot(l, 29)));
            let resolve: Resolve = std::mem::transmute(slot(l, 30));
            type Draw = unsafe extern "win64" fn(*mut c_void, u32, u32, u32, u32);
            let draw: Draw = std::mem::transmute(slot(l, 12));
            // outer (0): two draws; single (2): one draw — like the title.
            begin(l, qh, 0, 0);
            draw(l, 3, 1, 0, 0);
            draw(l, 3, 1, 0, 0);
            end(l, qh, 0, 0);
            begin(l, qh, 0, 2);
            draw(l, 3, 1, 0, 0);
            end(l, qh, 0, 2);
            resolve(l, qh, 0, 0, 3, results, 0);
            type Map = unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let map: Map = std::mem::transmute(slot(results, 8));
            let mut p = std::ptr::null_mut();
            assert_eq!(map(results, 0, std::ptr::null(), &mut p), S_OK);
            let counts = std::slice::from_raw_parts(p as *const u64, 3).to_vec();
            assert_eq!(counts, [2048, 0, 1024], "query counts resolve from draw counts, got {counts:?}");
            // Vulkan half: a stashed committed-RT copy lands the S4 pixels.
            match acquire() {
                None => crate::gpu::recomp::vulkan_runner::skip_test("no Vulkan"),
                Some(st) => {
                    let vulkan_sel = Selection { backend: Backend::Vulkan, lane: None, reason: "test".into() };
                    let mut vdev = std::ptr::null_mut();
                    assert_eq!(create_device_for(&vulkan_sel, Some(st), &mut vdev), S_OK);
                    let mk: MkCommitted = std::mem::transmute(slot(vdev, 27));
                    let mut vresults = std::ptr::null_mut();
                    assert_eq!(mk(vdev, props, 0, (&bd as *const u64) as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut vresults), S_OK);
                    let mkq: MkQ = std::mem::transmute(slot(vdev, 39));
                    let mut vqh = std::ptr::null_mut();
                    assert_eq!(mkq(vdev, std::ptr::null(), std::ptr::null(), &mut vqh), S_OK);
                    // committed 64x64 RGBA8 render target as the copy source.
                    let mut td_s = [0u64; 7];
                    (td_s.as_mut_ptr() as *mut u32).write_unaligned(3);
                    (td_s.as_mut_ptr().byte_add(16) as *mut u64).write_unaligned(64);
                    (td_s.as_mut_ptr().byte_add(24) as *mut u32).write_unaligned(64);
                    (td_s.as_mut_ptr().byte_add(32) as *mut u32).write_unaligned(28);
                    let td = (&td_s as *const u64) as *const u8;
                    let mut def_s = [0u64; 3];
                    (def_s.as_mut_ptr() as *mut u32).write_unaligned(1); // DEFAULT
                    let def = def_s.as_ptr() as *const c_void;
                    let mut rt = std::ptr::null_mut();
                    assert_eq!(mk(vdev, def, 0, td, 0, std::ptr::null(), std::ptr::null(), &mut rt), S_OK);
                    let rbd = buf_desc(256 * 64);
                    let mut rb = std::ptr::null_mut();
                    assert_eq!(mk(vdev, props, 0, (&rbd as *const u64) as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
                    let vs = vec![0xAAu8; 64];
                    let ps = vec![0xBBu8; 64];
                    let mut desc = vec![0u8; super::GFX_DESC_MIN];
                    desc[8..16].copy_from_slice(&(vs.as_ptr() as usize).to_le_bytes());
                    desc[16..24].copy_from_slice(&(vs.len()).to_le_bytes());
                    desc[24..32].copy_from_slice(&(ps.as_ptr() as usize).to_le_bytes());
                    desc[32..40].copy_from_slice(&(ps.len()).to_le_bytes());
                    desc[super::GFX_NUMRT_OFF..super::GFX_NUMRT_OFF + 4].copy_from_slice(&1u32.to_le_bytes());
                    desc[super::GFX_BLEND_OFF + 44] = 0xF; // RT0 full write mask
                    type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
                    let pso_x: PsoX = std::mem::transmute(slot(vdev, 60));
                    let mut pso = std::ptr::null_mut();
                    assert_eq!(pso_x(vdev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
                    type MkQueue = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
                    let mkq2: MkQueue = std::mem::transmute(slot(vdev, 8));
                    let mut vq = std::ptr::null_mut();
                    assert_eq!(mkq2(vdev, std::ptr::null(), std::ptr::null(), &mut vq), S_OK);
                    let list_x: ListX = std::mem::transmute(slot(vdev, 73));
                    let mut vl = std::ptr::null_mut();
                    assert_eq!(list_x(vdev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut vl), S_OK);
                    type SetPso = unsafe extern "win64" fn(*mut c_void, *mut c_void);
                    let set: SetPso = std::mem::transmute(slot(vl, 25));
                    set(vl, pso);
                    let (begin, end): (Query, Query) = (std::mem::transmute(slot(vl, 28)), std::mem::transmute(slot(vl, 29)));
                    let resolve: Resolve = std::mem::transmute(slot(vl, 30));
                    let draw: Draw = std::mem::transmute(slot(vl, 12));
                    begin(vl, vqh, 0, 0);
                    draw(vl, 3, 1, 0, 0);
                    draw(vl, 3, 1, 0, 0);
                    end(vl, vqh, 0, 0);
                    resolve(vl, vqh, 0, 0, 1, vresults, 0);
                    // stash the committed-RT → readback copy (record time).
                    let mut dst_loc = [0u8; 48];
                    (dst_loc.as_mut_ptr() as *mut usize).write_unaligned(rb as usize);
                    (dst_loc.as_mut_ptr().add(8) as *mut u32).write_unaligned(1);
                    (dst_loc.as_mut_ptr().add(16) as *mut u64).write_unaligned(0);
                    (dst_loc.as_mut_ptr().add(24) as *mut u32).write_unaligned(28);
                    (dst_loc.as_mut_ptr().add(28) as *mut u32).write_unaligned(64);
                    (dst_loc.as_mut_ptr().add(32) as *mut u32).write_unaligned(64);
                    (dst_loc.as_mut_ptr().add(36) as *mut u32).write_unaligned(1);
                    (dst_loc.as_mut_ptr().add(40) as *mut u32).write_unaligned(256);
                    let mut src_loc = [0u8; 24];
                    (src_loc.as_mut_ptr() as *mut usize).write_unaligned(rt as usize);
                    (src_loc.as_mut_ptr().add(8) as *mut u32).write_unaligned(0);
                    (src_loc.as_mut_ptr().add(16) as *mut u32).write_unaligned(0);
                    type CopyTex = unsafe extern "win64" fn(*mut c_void, *const u8, u32, u32, u32, *const u8, *const u8);
                    let copy_tex: CopyTex = std::mem::transmute(slot(vl, 16));
                    copy_tex(vl, dst_loc.as_ptr(), 0, 0, 0, src_loc.as_ptr(), std::ptr::null());
                    type Close = unsafe extern "win64" fn(*mut c_void) -> HRESULT;
                    let close: Close = std::mem::transmute(slot(vl, 9));
                    assert_eq!(close(vl), S_OK);
                    type Exec = unsafe extern "win64" fn(*mut c_void, u32, *const *mut c_void);
                    let exec: Exec = std::mem::transmute(slot(vq, 10));
                    exec(vq, 1, &vl);
                    let map: Map = std::mem::transmute(slot(vresults, 8));
                    let mut p = std::ptr::null_mut();
                    assert_eq!(map(vresults, 0, std::ptr::null(), &mut p), S_OK);
                    assert_eq!((p as *const u64).read_unaligned(), 2048, "outer query counts its two draws");
                    let map_rb: Map = std::mem::transmute(slot(rb, 8));
                    let mut prb = std::ptr::null_mut();
                    assert_eq!(map_rb(rb, 0, std::ptr::null(), &mut prb), S_OK);
                    let at = |x: usize, y: usize| {
                        let o = y * 256 + x * 4;
                        let s = std::slice::from_raw_parts((prb as *const u8).add(o), 4);
                        [s[0], s[1], s[2], s[3]]
                    };
                    assert_eq!(at(32, 32), [0, 255, 0, 255], "centre pixel is the S4 offscreen green");
                    assert_eq!(at(1, 1), [0, 0, 255, 255], "corner pixel is the S4 offscreen clear");
                }
            }
        }
    }

    /// M2 draw S6 failing-first: per-draw state replay at execute time —
    /// viewports/quadrants, derived-PSO blend, and depth bias (#94). A list
    /// drawing the title's shape (TL-half with the green-only derived PSO,
    /// then full with the base PSO, then a TR indexed draw) reads back cyan
    /// TL / green centre+TR / blue corner; a depth list replaying the title's
    /// six bias steps reads back blue/green/green/blue/green/blue. Query
    /// banking stays per draw call (S5 shape) under the replay. Fails pre-S6:
    /// Execute runs one full-viewport full-mask draw with no depth, so the TL
    /// quadrant comes back green and the no-bias pixels come back green.
    #[test]
    fn draw_s6_per_draw_state_replay() {
        unsafe {
            let _serial = draw_serial().lock().unwrap();
            // Null half: occlusion banking is unchanged by the replay (one
            // banked group per draw call — the title's nested shape).
            let null_sel = Selection { backend: Backend::Null, lane: None, reason: "test".into() };
            let mut dev = std::ptr::null_mut();
            assert_eq!(create_device_for(&null_sel, None, &mut dev), S_OK);
            type MkCommitted = unsafe extern "win64" fn(*mut c_void, *const c_void, u32, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mk: MkCommitted = std::mem::transmute(slot(dev, 27));
            let buf_desc = |bytes: u64| {
                let mut s = [0u64; 7];
                (s.as_mut_ptr() as *mut u32).write_unaligned(1); // BUFFER
                (s.as_mut_ptr().byte_add(16) as *mut u64).write_unaligned(bytes);
                s
            };
            let mut props_s = [0u64; 3];
            (props_s.as_mut_ptr() as *mut u32).write_unaligned(3); // READBACK
            let props = props_s.as_ptr() as *const c_void;
            let bd = buf_desc(24);
            let mut results = std::ptr::null_mut();
            assert_eq!(mk(dev, props, 0, (&bd as *const u64) as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut results), S_OK);
            type MkQ = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkq: MkQ = std::mem::transmute(slot(dev, 39));
            let mut qh = std::ptr::null_mut();
            assert_eq!(mkq(dev, std::ptr::null(), std::ptr::null(), &mut qh), S_OK);
            type ListX = unsafe extern "win64" fn(*mut c_void, *const u32, *mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let list_x: ListX = std::mem::transmute(slot(dev, 73));
            let mut l = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l), S_OK);
            type Query = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32);
            type Resolve = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, u32, u32, *mut c_void, u64);
            let (begin, end): (Query, Query) = (std::mem::transmute(slot(l, 28)), std::mem::transmute(slot(l, 29)));
            let resolve: Resolve = std::mem::transmute(slot(l, 30));
            type Draw = unsafe extern "win64" fn(*mut c_void, u32, u32, u32, u32);
            let draw: Draw = std::mem::transmute(slot(l, 12));
            begin(l, qh, 0, 0);
            draw(l, 3, 1, 0, 0);
            draw(l, 3, 1, 0, 0);
            end(l, qh, 0, 0);
            resolve(l, qh, 0, 0, 1, results, 0);
            type Map = unsafe extern "win64" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT;
            let map: Map = std::mem::transmute(slot(results, 8));
            let mut p = std::ptr::null_mut();
            assert_eq!(map(results, 0, std::ptr::null(), &mut p), S_OK);
            assert_eq!((p as *const u64).read_unaligned(), 2048, "per-draw-group banking: two draws bank 2N");
            // White-box: each counted draw snapshots one replay group —
            // viewport, bound-PSO blend mask, and effective depth bias.
            type PsoX = unsafe extern "win64" fn(*mut c_void, *const u8, u32, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let pso_x: PsoX = std::mem::transmute(slot(dev, 60));
            let mut bdesc = vec![0u8; super::GFX_DESC_MIN];
            bdesc[super::GFX_BLEND_OFF + 44] = 0xF;
            let mut base = std::ptr::null_mut();
            assert_eq!(pso_x(dev, bdesc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut base), S_OK);
            type DerivedX = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32, *const u8, *const c_void, *mut *mut c_void) -> HRESULT;
            let derive: DerivedX = std::mem::transmute(slot(dev, 55));
            let mut patch = vec![0u8; 0x14C];
            patch[0] = 2;
            patch[4 + 44] = 0xA;
            let mut green_only = std::ptr::null_mut();
            assert_eq!(derive(dev, base, 1, patch.as_ptr(), std::ptr::null(), &mut green_only), S_OK);
            assert_eq!(super::pso_record_for(green_only as usize).map(|r| r.rt0_mask), Some(0xA));
            let mut sdesc = bdesc.clone();
            sdesc[super::GFX_RASTER_OFF + 12..super::GFX_RASTER_OFF + 16].copy_from_slice(&(-1000i32).to_le_bytes());
            let mut static_pso = std::ptr::null_mut();
            assert_eq!(pso_x(dev, sdesc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut static_pso), S_OK);
            assert_eq!(super::pso_record_for(static_pso as usize).map(|r| r.static_bias), Some([-1000.0, 0.0, 0.0]));
            let mut l2 = std::ptr::null_mut();
            assert_eq!(list_x(dev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut l2), S_OK);
            type SetPso = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            let set_pso: SetPso = std::mem::transmute(slot(l2, 25));
            type Vp = unsafe extern "win64" fn(*mut c_void, u32, *const u8);
            let set_vp: Vp = std::mem::transmute(slot(l2, 21));
            type SetDepthBiasX = unsafe extern "win64" fn(*mut c_void, f32, f32, f32);
            let set_bias: SetDepthBiasX = std::mem::transmute(slot(l2, 94));
            let vp = |x: f32, y: f32, w: f32, h: f32| {
                let mut v = [0u8; 24];
                v[0..4].copy_from_slice(&x.to_le_bytes());
                v[4..8].copy_from_slice(&y.to_le_bytes());
                v[8..12].copy_from_slice(&w.to_le_bytes());
                v[12..16].copy_from_slice(&h.to_le_bytes());
                v[16..20].copy_from_slice(&0f32.to_le_bytes());
                v[20..24].copy_from_slice(&1f32.to_le_bytes());
                v
            };
            let tl = vp(0.0, 0.0, 32.0, 32.0);
            set_pso(l2, base);
            set_vp(l2, 1, tl.as_ptr());
            draw(l2, 3, 1, 0, 0);
            let tr = vp(32.0, 0.0, 32.0, 32.0);
            set_pso(l2, green_only);
            set_vp(l2, 1, tr.as_ptr());
            draw(l2, 3, 1, 0, 0);
            set_bias(l2, -1000.5, 0.0, 0.0);
            draw(l2, 3, 1, 0, 0);
            set_pso(l2, static_pso);
            draw(l2, 3, 1, 0, 0);
            set_pso(l2, base);
            draw(l2, 3, 1, 0, 0);
            let ops = super::list_record_for(l2 as usize).map(|r| r.ops).unwrap_or_default();
            assert_eq!(ops.len(), 5, "one group per draw call");
            assert!((ops[0].viewport, ops[0].rt0_mask, ops[0].has_viewport) == ([0.0, 0.0, 32.0, 32.0, 0.0, 1.0], 0xF, true));
            assert!((ops[1].viewport[0], ops[1].rt0_mask) == (32.0, 0xA), "derived PSO mask rides the group");
            assert!(!ops[0].depth_test && (ops[0].depth_bias, ops[0].depth_slope, ops[0].depth_clamp) == (0.0, 0.0, 0.0));
            assert_eq!(ops[2].depth_bias, -1000.5, "dynamic #94 override");
            assert_eq!(ops[3].depth_bias, -1000.0, "fresh static-PSO bind clears the override");
            assert_eq!(ops[4].depth_bias, 0.0, "zero-PSO bind carries no bias");
            // Vulkan halves need a device; without one there is nothing to replay.
            let st = match acquire() {
                None => {
                    crate::gpu::recomp::vulkan_runner::skip_test("no Vulkan");
                    return;
                }
                Some(st) => st,
            };
            let vulkan_sel = Selection { backend: Backend::Vulkan, lane: None, reason: "test".into() };
            let mut vdev = std::ptr::null_mut();
            assert_eq!(create_device_for(&vulkan_sel, Some(st), &mut vdev), S_OK);
            let mk: MkCommitted = std::mem::transmute(slot(vdev, 27));
            // committed 64x64 RGBA8 render target as the copy source.
            let mut td_s = [0u64; 7];
            (td_s.as_mut_ptr() as *mut u32).write_unaligned(3);
            (td_s.as_mut_ptr().byte_add(16) as *mut u64).write_unaligned(64);
            (td_s.as_mut_ptr().byte_add(24) as *mut u32).write_unaligned(64);
            (td_s.as_mut_ptr().byte_add(32) as *mut u32).write_unaligned(28);
            let td = (&td_s as *const u64) as *const u8;
            let mut def_s = [0u64; 3];
            (def_s.as_mut_ptr() as *mut u32).write_unaligned(1); // DEFAULT
            let def = def_s.as_ptr() as *const c_void;
            let mut rt = std::ptr::null_mut();
            assert_eq!(mk(vdev, def, 0, td, 0, std::ptr::null(), std::ptr::null(), &mut rt), S_OK);
            let rbd = buf_desc(256 * 64);
            let mut rb = std::ptr::null_mut();
            assert_eq!(mk(vdev, props, 0, (&rbd as *const u64) as *const u8, 0, std::ptr::null(), std::ptr::null(), &mut rb), S_OK);
            // base PSO (full write mask) + green-only derived PSO (#55 blend patch).
            // (Slot type aliases come from the null half above; only the
            // bindings are re-made for the Vulkan device/list.)
            let vs = vec![0xAAu8; 64];
            let ps = vec![0xBBu8; 64];
            let mut desc = vec![0u8; super::GFX_DESC_MIN];
            desc[8..16].copy_from_slice(&(vs.as_ptr() as usize).to_le_bytes());
            desc[16..24].copy_from_slice(&(vs.len()).to_le_bytes());
            desc[24..32].copy_from_slice(&(ps.as_ptr() as usize).to_le_bytes());
            desc[32..40].copy_from_slice(&(ps.len()).to_le_bytes());
            desc[super::GFX_NUMRT_OFF..super::GFX_NUMRT_OFF + 4].copy_from_slice(&1u32.to_le_bytes());
            desc[super::GFX_BLEND_OFF + 44] = 0xF; // RT0 full write mask
            let pso_x: PsoX = std::mem::transmute(slot(vdev, 60));
            let mut pso = std::ptr::null_mut();
            assert_eq!(pso_x(vdev, desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut pso), S_OK);
            let derive: DerivedX = std::mem::transmute(slot(vdev, 55));
            let mut patch = vec![0u8; 0x14C];
            patch[0] = 2;
            patch[4 + 44] = 0xA; // derived RT0 mask: green + alpha only
            let mut derived = std::ptr::null_mut();
            assert_eq!(derive(vdev, pso, 1, patch.as_ptr(), std::ptr::null(), &mut derived), S_OK);
            type MkQueue = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> HRESULT;
            let mkq2: MkQueue = std::mem::transmute(slot(vdev, 8));
            let mut vq = std::ptr::null_mut();
            assert_eq!(mkq2(vdev, std::ptr::null(), std::ptr::null(), &mut vq), S_OK);
            let list_x: ListX = std::mem::transmute(slot(vdev, 73));
            let mut vl = std::ptr::null_mut();
            assert_eq!(list_x(vdev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut vl), S_OK);
            let set: SetPso = std::mem::transmute(slot(vl, 25));
            let set_vp: Vp = std::mem::transmute(slot(vl, 21));
            let vp = |x: f32, y: f32, w: f32, h: f32| {
                let mut v = [0u8; 24];
                v[0..4].copy_from_slice(&x.to_le_bytes());
                v[4..8].copy_from_slice(&y.to_le_bytes());
                v[8..12].copy_from_slice(&w.to_le_bytes());
                v[12..16].copy_from_slice(&h.to_le_bytes());
                v[16..20].copy_from_slice(&0f32.to_le_bytes());
                v[20..24].copy_from_slice(&1f32.to_le_bytes());
                v
            };
            let draw: Draw = std::mem::transmute(slot(vl, 12));
            type DrawIndexedX = unsafe extern "win64" fn(*mut c_void, u64, u32, u32, u32);
            let draw_idx: DrawIndexedX = std::mem::transmute(slot(vl, 81));
            // the title's shape: TL-half derived, full base, TR indexed.
            let tl = vp(0.0, 0.0, 32.0, 32.0);
            set(vl, derived);
            set_vp(vl, 1, tl.as_ptr());
            draw(vl, 3, 1, 0, 0);
            let full = vp(0.0, 0.0, 64.0, 64.0);
            set(vl, pso);
            set_vp(vl, 1, full.as_ptr());
            draw(vl, 3, 1, 0, 0);
            let tr = vp(32.0, 0.0, 32.0, 32.0);
            set_vp(vl, 1, tr.as_ptr());
            draw_idx(vl, 0, 0, 3, 0);
            let mut dst_loc = [0u8; 48];
            (dst_loc.as_mut_ptr() as *mut usize).write_unaligned(rb as usize);
            (dst_loc.as_mut_ptr().add(8) as *mut u32).write_unaligned(1);
            (dst_loc.as_mut_ptr().add(16) as *mut u64).write_unaligned(0);
            (dst_loc.as_mut_ptr().add(24) as *mut u32).write_unaligned(28);
            (dst_loc.as_mut_ptr().add(28) as *mut u32).write_unaligned(64);
            (dst_loc.as_mut_ptr().add(32) as *mut u32).write_unaligned(64);
            (dst_loc.as_mut_ptr().add(36) as *mut u32).write_unaligned(1);
            (dst_loc.as_mut_ptr().add(40) as *mut u32).write_unaligned(256);
            let mut src_loc = [0u8; 24];
            (src_loc.as_mut_ptr() as *mut usize).write_unaligned(rt as usize);
            (src_loc.as_mut_ptr().add(8) as *mut u32).write_unaligned(0);
            (src_loc.as_mut_ptr().add(16) as *mut u32).write_unaligned(0);
            type CopyTex = unsafe extern "win64" fn(*mut c_void, *const u8, u32, u32, u32, *const u8, *const u8);
            let copy_tex: CopyTex = std::mem::transmute(slot(vl, 16));
            copy_tex(vl, dst_loc.as_ptr(), 0, 0, 0, src_loc.as_ptr(), std::ptr::null());
            type Close = unsafe extern "win64" fn(*mut c_void) -> HRESULT;
            let close: Close = std::mem::transmute(slot(vl, 9));
            assert_eq!(close(vl), S_OK);
            type Exec = unsafe extern "win64" fn(*mut c_void, u32, *const *mut c_void);
            let exec: Exec = std::mem::transmute(slot(vq, 10));
            exec(vq, 1, &vl);
            let map_rb: Map = std::mem::transmute(slot(rb, 8));
            let mut prb = std::ptr::null_mut();
            assert_eq!(map_rb(rb, 0, std::ptr::null(), &mut prb), S_OK);
            let at = |x: usize, y: usize| {
                let o = y * 256 + x * 4;
                let s = std::slice::from_raw_parts((prb as *const u8).add(o), 4);
                [s[0], s[1], s[2], s[3]]
            };
            assert_eq!(at(16, 16), [0, 255, 255, 255], "TL quadrant is the derived green-only draw over blue");
            assert_eq!(at(32, 32), [0, 255, 0, 255], "centre is the full base draw");
            assert_eq!(at(48, 16), [0, 255, 0, 255], "TR quadrant is the indexed draw");
            assert_eq!(at(1, 1), [0, 0, 255, 255], "corner keeps the clear");
            // Depth half: the title's six bias steps (no-bias / -1000.5 /
            // persists-across-KickoffX / reset-to-0 / static -1000 / zero PSO).
            let mut vl2 = std::ptr::null_mut();
            assert_eq!(list_x(vdev, [0x10u32, 0, 1, 0].as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null(), &mut vl2), S_OK);
            let mut zero_desc = vec![0u8; super::GFX_DESC_MIN];
            zero_desc[8..16].copy_from_slice(&(vs.as_ptr() as usize).to_le_bytes());
            zero_desc[16..24].copy_from_slice(&(vs.len()).to_le_bytes());
            zero_desc[24..32].copy_from_slice(&(ps.as_ptr() as usize).to_le_bytes());
            zero_desc[32..40].copy_from_slice(&(ps.len()).to_le_bytes());
            zero_desc[super::GFX_BLEND_OFF + 44] = 0xF; // RT0 full write mask
            let mut zero_pso = std::ptr::null_mut();
            assert_eq!(pso_x(vdev, zero_desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut zero_pso), S_OK);
            let mut static_desc = zero_desc.clone();
            static_desc[super::GFX_RASTER_OFF + 12..super::GFX_RASTER_OFF + 16].copy_from_slice(&(-1000i32).to_le_bytes());
            let mut static_pso = std::ptr::null_mut();
            assert_eq!(pso_x(vdev, static_desc.as_ptr(), 0, std::ptr::null(), std::ptr::null(), &mut static_pso), S_OK);
            type OmRt = unsafe extern "win64" fn(*mut c_void, u32, *const usize, u32, *const usize);
            let om: OmRt = std::mem::transmute(slot(vl2, 50));
            let (rtv, dsv) = (0x1000usize, 0x3000usize);
            om(vl2, 1, &rtv, 0, &dsv);
            let set2: SetPso = std::mem::transmute(slot(vl2, 25));
            let set_vp2: Vp = std::mem::transmute(slot(vl2, 21));
            let draw2: Draw = std::mem::transmute(slot(vl2, 12));
            let set_bias: SetDepthBiasX = std::mem::transmute(slot(vl2, 94));
            type KickoffX = unsafe extern "win64" fn(*mut c_void);
            let kickoff: KickoffX = std::mem::transmute(slot(vl2, 93));
            let q1 = vp(0.0, 0.0, 32.0, 32.0);
            set2(vl2, zero_pso);
            set_vp2(vl2, 1, q1.as_ptr());
            draw2(vl2, 3, 1, 0, 0);
            let q2 = vp(32.0, 0.0, 32.0, 32.0);
            set_bias(vl2, -1000.5, 0.0, 0.0);
            set_vp2(vl2, 1, q2.as_ptr());
            draw2(vl2, 3, 1, 0, 0);
            kickoff(vl2);
            let q3 = vp(32.0, 32.0, 32.0, 32.0);
            set_vp2(vl2, 1, q3.as_ptr());
            draw2(vl2, 3, 1, 0, 0);
            let q4 = vp(16.0, 32.0, 16.0, 16.0);
            set_bias(vl2, 0.0, 0.0, 0.0);
            set_vp2(vl2, 1, q4.as_ptr());
            draw2(vl2, 3, 1, 0, 0);
            let q5 = vp(0.0, 32.0, 16.0, 32.0);
            set2(vl2, static_pso);
            set_vp2(vl2, 1, q5.as_ptr());
            draw2(vl2, 3, 1, 0, 0);
            let q6 = vp(0.0, 0.0, 16.0, 16.0);
            set2(vl2, zero_pso);
            set_vp2(vl2, 1, q6.as_ptr());
            draw2(vl2, 3, 1, 0, 0);
            copy_tex(vl2, dst_loc.as_ptr(), 0, 0, 0, src_loc.as_ptr(), std::ptr::null());
            let close2: Close = std::mem::transmute(slot(vl2, 9));
            assert_eq!(close2(vl2), S_OK);
            exec(vq, 1, &vl2);
            let mut prb2 = std::ptr::null_mut();
            assert_eq!(map_rb(rb, 0, std::ptr::null(), &mut prb2), S_OK);
            let at2 = |x: usize, y: usize| {
                let o = y * 256 + x * 4;
                let s = std::slice::from_raw_parts((prb2 as *const u8).add(o), 4);
                [s[0], s[1], s[2], s[3]]
            };
            assert_eq!(at2(20, 20), [0, 0, 255, 255], "no bias rejects at z == clear");
            assert_eq!(at2(48, 16), [0, 255, 0, 255], "dynamic bias passes");
            assert_eq!(at2(48, 48), [0, 255, 0, 255], "bias persists across KickoffX");
            assert_eq!(at2(24, 48), [0, 0, 255, 255], "reset bias rejects again");
            assert_eq!(at2(8, 48), [0, 255, 0, 255], "static PSO bias passes");
            assert_eq!(at2(8, 8), [0, 0, 255, 255], "zero PSO bias does not leak");
        }
    }

    #[test]
    fn draw_s4_per_group_vs_bytes_follow_bound_pso() {
        // S4 failing shape: a group drawn under a different PSO carries that
        // PSO's VS bytes; same-PSO and unknown-PSO groups inherit (empty).
        let a: &'static [u8] = Box::leak(vec![0xA1u8; 32].into_boxed_slice());
        let b: &'static [u8] = Box::leak(vec![0xB2u8; 48].into_boxed_slice());
        let (pa, pb, pl) = (0x5_4000_001usize, 0x5_4000_002usize, 0x5_4000_003usize);
        for (id, blob) in [(pa, a), (pb, b), (pl, a)] {
            super::pso_store().lock().unwrap().insert(
                id,
                super::PsoRecord { vs_ptr: blob.as_ptr() as usize, vs_len: blob.len(), ..Default::default() },
            );
        }
        assert_eq!(super::group_own_vs_bytes(pa, pl), a, "other PSO carries its own bytes");
        assert_eq!(super::group_own_vs_bytes(pb, pl), b);
        assert!(super::group_own_vs_bytes(pl, pl).is_empty(), "list PSO inherits");
        assert!(super::group_own_vs_bytes(0x5_4000_099, pl).is_empty(), "unknown PSO inherits");
    }

    /// The newer-layout command-list table must stay identical to the Windows
    /// twin's (`hle/d3d12x/real.rs::NEWER_LIST_SLOTS`, which does not compile
    /// on Linux, so the literal is pinned here).
    #[test]
    fn newer_list_slot_table_matches_the_windows_twin() {
        const EXPECTED: [(usize, usize); 22] = [
            (61, 59),
            (62, 60),
            (63, 61),
            (64, 62),
            (65, 63),
            (70, 68),
            (71, 69),
            (72, 70),
            (73, 71),
            (74, 72),
            (80, 78),
            (83, 81),
            (85, 83),
            (86, 84),
            (87, 85),
            (90, 88),
            (91, 89),
            (92, 90),
            (95, 93),
            (96, 94),
            (129, 127),
            (139, 10),
        ];
        assert_eq!(NEWER_LIST_SLOTS.len(), EXPECTED.len());
        for (linux, windows) in NEWER_LIST_SLOTS.iter().zip(EXPECTED) {
            assert_eq!(*linux, windows, "Linux/Windows newer-list slot tables drifted");
        }
        // The slots this backend implements on either side get a call-time
        // thunk; the rest stay logging stubs.
        let wired: Vec<usize> = (0..VTBL_SLOTS).filter(|&s| list_slot_targets(s).is_some()).collect();
        assert_eq!(wired, vec![61, 62, 63, 70, 71, 72, 73, 74, 83, 91, 95, 96, 139]);
    }

    /// `slot(list, 70)` is `OMSetDepthBounds` on the newer layout and
    /// `TimestampBottom` on the older one; `#72` is the timestamp on newer,
    /// `#74` `Wait64`; `#32` is always the console `SetDescriptorHeaps`;
    /// `#139` resets on newer and is a stub in the older table.
    #[test]
    fn newer_list_layout_selects_the_moved_methods() {
        let t = |s: usize| list_slot_targets(s).expect("slot has a layout split");
        assert_eq!(t(70).0, VulList_TimestampBottom as usize);
        assert_eq!(t(70).1, List_OMSetDepthBounds as usize);
        assert_ne!(t(70).1, VulList_TimestampBottom as usize);
        assert_eq!(t(71).0, VulList_Wait32 as usize);
        assert_eq!(t(71).1, VulList_Write32Bottom as usize);
        assert_eq!(t(72).0, VulList_Wait64 as usize);
        assert_eq!(t(72).1, VulList_TimestampBottom as usize);
        assert_eq!(t(74).0, list_stub(74));
        assert_eq!(t(74).1, VulList_Wait64 as usize);
        assert_eq!(t(61).0, VulList_Fill32 as usize);
        assert_eq!(t(61).1, VulList_ExecuteIndirect as usize);
        assert_eq!(t(62).1, VulList_CopyMemory as usize);
        assert_eq!(t(63).1, VulList_Fill32 as usize);
        assert_eq!(t(91).1, VulList_Write64Top as usize);
        assert_eq!(t(95).1, VulList_Kickoff as usize);
        assert_eq!(t(96).1, List_RSSetDepthBiasX as usize);
        assert_eq!(t(139).0, list_stub(139));
        assert_eq!(t(139).1, List_ResetX as usize);
        // Slots this backend implements on neither side keep the honest stub.
        assert_eq!(list_slot_targets(80), None); // newer #80 (older #78 DispatchX) not implemented
        assert_eq!(list_slot_targets(129), None);
        unsafe {
            let list = new_obj(Kind::CommandList) as *mut c_void;
            assert_eq!(slot(list, 32), List_SetDescriptorHeapsX as usize);
            assert_ne!(slot(list, 139), list_stub(139));
            assert_ne!(slot(list, 139), t(139).0);
            assert_ne!(slot(list, 70), t(70).0);
        }
    }

    fn layout_serial() -> &'static Mutex<()> {
        static S: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));
        &S
    }

    /// The console base block must stay identical to the Windows twin's
    /// `real.rs::console_remap(Kind::CommandList)` (the twin does not compile
    /// on Linux, so the literal is pinned here): console 28–31 are the PC
    /// query methods 52–55 and console 32–55 are PC 28–51.
    #[test]
    fn console_list_base_slots_match_the_windows_twin() {
        const EXPECTED: [(usize, usize); 28] = [
            (28, 52), (29, 53), (30, 54), (31, 55), (32, 28), (33, 29), (34, 30),
            (35, 31), (36, 32), (37, 33), (38, 34), (39, 35), (40, 36), (41, 37),
            (42, 38), (43, 39), (44, 40), (45, 41), (46, 42), (47, 43), (48, 44),
            (49, 45), (50, 46), (51, 47), (52, 48), (53, 49), (54, 50), (55, 51),
        ];
        for (console, pc) in EXPECTED {
            assert_eq!(console_list_base_slot(console), Some(pc), "console #{console} is PC #{pc}");
        }
        assert_eq!(console_list_base_slot(27), None);
        assert_eq!(console_list_base_slot(56), None);
        // Only the implemented PC methods get a handler; the retail-called
        // ones are the title's ClearRenderTargetView #52, IASetVertexBuffers
        // #48, root table #36, root SRV #44 plus the S2 stores.
        let wired: Vec<usize> = (28..=55).filter(|&c| console_list_base_slot(c).and_then(console_base_handler).is_some()).collect();
        assert_eq!(wired, vec![32, 34, 36, 44, 47, 48, 50, 52]);
    }

    /// End to end through the vtable: the retail title's console slots store
    /// their state, and a PC index in the shifted block is never another
    /// method's handler.
    #[test]
    fn console_base_slots_store_blend_stencil_roots_and_vertices() {
        unsafe {
            let list = new_obj(Kind::CommandList);
            let obj = list as *mut c_void;
            assert_eq!(slot(obj, 23), List_OMSetBlendFactor as usize);
            assert_eq!(slot(obj, 24), List_OMSetStencilRef as usize);
            assert_eq!(slot(obj, 36), List_SetGraphicsRootDescriptorTable as usize);
            assert_eq!(slot(obj, 44), List_SetGraphicsRootShaderResourceView as usize);
            assert_eq!(slot(obj, 48), List_IASetVertexBuffers as usize);
            assert_eq!(slot(obj, 52), List_ClearRenderTargetView as usize);

            type F2 = unsafe extern "win64" fn(*mut c_void, *const f32);
            type U32 = unsafe extern "win64" fn(*mut c_void, u32);
            type Root = unsafe extern "win64" fn(*mut c_void, u32, u64);
            type Vb = unsafe extern "win64" fn(*mut c_void, u32, u32, *const u8);
            type Clear = unsafe extern "win64" fn(*mut c_void, u64, *const f32, u32, *const c_void);

            let factor = [0.25f32, 0.5, 0.75, 1.0];
            let blend: F2 = std::mem::transmute(slot(obj, 23));
            blend(obj, factor.as_ptr());
            let stencil: U32 = std::mem::transmute(slot(obj, 24));
            stencil(obj, 7);
            let table: Root = std::mem::transmute(slot(obj, 36));
            table(obj, 3, 0x1234_5678_9abc);
            let srv: Root = std::mem::transmute(slot(obj, 44));
            srv(obj, 2, 0x1400_0000);
            let mut views = [0u8; 32];
            views[..8].copy_from_slice(&0x1_0000u64.to_le_bytes());
            views[8..12].copy_from_slice(&0x400u32.to_le_bytes());
            views[12..16].copy_from_slice(&32u32.to_le_bytes());
            let vb: Vb = std::mem::transmute(slot(obj, 48));
            vb(obj, 5, 2, views.as_ptr());
            let color = [0.1f32, 0.2, 0.3, 1.0];
            let clear: Clear = std::mem::transmute(slot(obj, 52));
            clear(obj, 0xab, color.as_ptr(), 0, std::ptr::null());

            let r = list_record_for(obj as usize).expect("the calls store a list record");
            assert_eq!(r.blend_factor, Some(factor));
            assert_eq!(r.stencil_ref, Some(7));
            assert_eq!(r.root_table, Some((3, 0x1234_5678_9abc)));
            assert_eq!(r.root_srv, Some((2, 0x1400_0000)));
            assert_eq!(r.vb_start, 5);
            assert_eq!(r.vb_views.as_slice(), &views[..32]);
            assert_eq!(r.clear_rtv, Some((0xab, color, 0)));
            // console #49 is PC #45 SOSetTargets, which this backend keeps a stub
            assert_eq!(slot(obj, 49), list_stub(49));
        }
    }

    /// Serializes a test whose calls latch the process-wide newer-list flag and
    /// clears the flag on drop, so a parallel test never observes a layout that
    /// a previous test's device/queue slot happened to prove.
    struct LayoutSerial {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl LayoutSerial {
        fn take() -> Self {
            Self { _guard: layout_serial().lock().unwrap_or_else(|e| e.into_inner()) }
        }
    }

    impl Drop for LayoutSerial {
        fn drop(&mut self) {
            NEWER_LIST_LAYOUT.store(false, Ordering::Release);
        }
    }

    /// End to end through the vtable: older #70 writes a timestamp;
    /// a newer-only Reset #139 call latches the layout; then #70 stores depth
    /// bounds, #72 is the timestamp, #74 Wait64 and #32 SetDescriptorHeaps.
    #[test]
    fn newer_list_layout_routes_depth_bounds_timestamp_and_reset() {
        let _g = layout_serial().lock().unwrap_or_else(|e| e.into_inner());
        NEWER_LIST_LAYOUT.store(false, Ordering::Release);
        unsafe {
            let list = new_obj(Kind::CommandList);
            let obj = list as *mut c_void;

            let mut stamp = 0u64;
            let ts70: unsafe extern "win64" fn(*mut c_void, u64, u64, u32, u32) = std::mem::transmute(slot(obj, 70));
            ts70(obj, &mut stamp as *mut u64 as u64, 0, 0, 0);
            assert!(stamp > 0, "older layout #70 is the timestamp");
            assert_eq!(list_record_for(list as usize).unwrap_or_default().depth_bounds, None);

            let reset: unsafe extern "win64" fn(*mut c_void, *mut c_void, *mut c_void, u64) -> HRESULT =
                std::mem::transmute(slot(obj, 139));
            assert_eq!(reset(obj, std::ptr::null_mut(), std::ptr::null_mut(), 0), S_OK);
            assert!(NEWER_LIST_LAYOUT.load(Ordering::Acquire), "#139 proves the newer layout");

            let dbounds: unsafe extern "win64" fn(*mut c_void, f32, f32) = std::mem::transmute(slot(obj, 70));
            dbounds(obj, 0.25, 0.75);
            assert_eq!(list_record_for(obj as usize).unwrap().depth_bounds, Some([0.25, 0.75]));
            assert!(stamp > 0, "depth bounds must not write the timestamp slot");

            stamp = 0;
            let ts72: unsafe extern "win64" fn(*mut c_void, u64, u64, u32, u32) = std::mem::transmute(slot(obj, 72));
            ts72(obj, &mut stamp as *mut u64 as u64, 0, 0, 0);
            assert!(stamp > 0, "newer layout #72 is the timestamp");
            assert_eq!(list_record_for(obj as usize).unwrap().depth_bounds, Some([0.25, 0.75]));

            let wait64: unsafe extern "win64" fn(*mut c_void, u64, u32, u64, u64, u32) =
                std::mem::transmute(slot(obj, 74));
            wait64(obj, &mut stamp as *mut u64 as u64, 8, 0, 0, 0); // ALWAYS: no polling

            let heaps: unsafe extern "win64" fn(*mut c_void, u32, *const usize) -> HRESULT =
                std::mem::transmute(slot(obj, 32));
            assert_eq!(heaps(obj, 1, std::ptr::null()), S_OK);
        }
        NEWER_LIST_LAYOUT.store(false, Ordering::Release);
    }

    /// The layout thunk reads its flag at each call and forwards every
    /// argument register, the floating-point ones and the stack arguments too
    /// (otherwise a newer title's `OMSetDepthBounds(min, max)` reaches the
    /// timestamp handler with `rdx == 1`).
    #[test]
    fn list_layout_thunk_reads_the_flag_at_each_call_and_preserves_arguments() {
        unsafe extern "win64" fn older(a: u64, b: u64, c: u64, d: u64, e: f32, f: f64) -> u64 {
            a + 10 * b + 100 * c + 1000 * d + (e * 10_000.0) as u64 + (f * 100_000.0) as u64
        }
        unsafe extern "win64" fn newer(a: u64, b: u64, c: u64, d: u64, e: f32, f: f64) -> u64 {
            older(a, b, c, d, e, f) + 7
        }
        static FLAG: AtomicBool = AtomicBool::new(false);
        let p = layout_thunk(older as usize, newer as usize, &FLAG);
        let f: unsafe extern "win64" fn(u64, u64, u64, u64, f32, f64) -> u64 = unsafe { std::mem::transmute(p) };
        unsafe {
            assert_eq!(f(1, 2, 3, 4, 5.0, 6.0), older(1, 2, 3, 4, 5.0, 6.0));
            FLAG.store(true, Ordering::Release);
            assert_eq!(f(1, 2, 3, 4, 5.0, 6.0), newer(1, 2, 3, 4, 5.0, 6.0));
            FLAG.store(false, Ordering::Release);
            assert_eq!(f(1, 2, 3, 4, 5.0, 6.0), older(1, 2, 3, 4, 5.0, 6.0));
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
