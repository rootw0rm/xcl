//! The GPU part of a replay: a device of its own, the capture's memory windows, textures,
//! views, samplers and targets made again, the call, and what it wrote read back.
//!
//! Addresses are the capture run's: each memory window is a buffer that holds the captured
//! ranges at their offsets (a reserved buffer with only the tiles they touch when a range lies
//! far in), the `io` words go to u0 as they were, and views sit at the heap indices the T# of
//! the tables name. A texture is in the state the call uses it in (target, depth, UAV, shader
//! resource), not in the one the title had it in.

use std::ffi::c_void;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, D3D_PRIMITIVE_TOPOLOGY};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

use super::{heap_size, Opts, Program};
use crate::gpu::recomp::pso::{self, Stage, StaticSampler};
use crate::gpu::texel_image;
use crate::hle::d3d12x::capture::manifest::{unhex, Call, Manifest, Texture};

/// The device the runtime would take (`XCL_ADAPTER`, else the first in high-performance
/// order), through the Agility SDK when there is one, as `gpu::recomp::runner` makes its own.
pub(super) unsafe fn device() -> Result<(ID3D12Device, String), String> {
    use windows::Win32::Graphics::Dxgi::*;
    let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(|x| format!("CreateDXGIFactory2: {x}"))?;
    let sel = crate::gpu::adapter::Selector::from_env_or("auto");
    let (adapter, desc) = crate::gpu::adapter::candidates(&factory, &sel).into_iter().next().ok_or_else(|| crate::gpu::adapter::no_match(&factory, &sel))?;
    let dev = crate::gpu::agility::create_device(&adapter, D3D_FEATURE_LEVEL_12_0).map_err(|x| format!("D3D12 device: {x}"))?;
    Ok((dev, crate::gpu::adapter::name(&desc)))
}

fn buffer_desc(size: u64, uav: bool) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
        ..Default::default()
    }
}

unsafe fn committed(dev: &ID3D12Device, heap: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource, String> {
    let mut r: Option<ID3D12Resource> = None;
    dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() }, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut r).map_err(|x| format!("CreateCommittedResource ({}x{} format {}): {x}", desc.Width, desc.Height, desc.Format.0))?;
    r.ok_or_else(|| "CreateCommittedResource".into())
}

/// An upload buffer holding `bytes`.
unsafe fn upload(dev: &ID3D12Device, bytes: &[u8]) -> Result<ID3D12Resource, String> {
    let b = committed(dev, D3D12_HEAP_TYPE_UPLOAD, &buffer_desc(bytes.len() as u64, false), D3D12_RESOURCE_STATE_GENERIC_READ)?;
    let mut p: *mut c_void = std::ptr::null_mut();
    b.Map(0, None, Some(&mut p)).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    b.Unmap(0, None);
    Ok(b)
}

unsafe fn transition(r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER { pResource: std::mem::transmute_copy(r), Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, StateBefore: before, StateAfter: after }),
        },
    }
}

fn texture_desc(t: &Texture) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION(t.dimension),
        Alignment: 0,
        Width: t.width,
        Height: t.height,
        DepthOrArraySize: t.depth_or_array,
        MipLevels: t.mips,
        Format: DXGI_FORMAT(t.format),
        SampleDesc: DXGI_SAMPLE_DESC { Count: t.samples.max(1), Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAGS(t.flags & (D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 | D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0)),
    }
}

/// A view description from its bytes in the manifest (None: the default view).
unsafe fn view_desc<T>(hex: &Option<String>) -> Option<T> {
    let b = unhex(hex.as_ref()?).ok()?;
    (b.len() == std::mem::size_of::<T>()).then(|| std::ptr::read_unaligned(b.as_ptr() as *const T))
}

struct Run<'a> {
    dev: &'a ID3D12Device,
    queue: &'a ID3D12CommandQueue,
    alloc: &'a ID3D12CommandAllocator,
    list: &'a ID3D12GraphicsCommandList,
    fence: &'a ID3D12Fence,
    value: u64,
}

enum Ended {
    Done(Duration),
    Removed(Duration, String),
    TimedOut(Duration),
}

impl Run<'_> {
    /// The list closed, submitted and waited for; reset for the next recording.
    unsafe fn submit(&mut self, timeout: Duration) -> Result<Ended, String> {
        self.list.Close().map_err(|x| format!("Close: {x}"))?;
        let t = Instant::now();
        self.queue.ExecuteCommandLists(&[Some(self.list.cast().map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?)]);
        self.value += 1;
        self.queue.Signal(self.fence, self.value).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
        let ended = loop {
            let done = self.fence.GetCompletedValue();
            if done == u64::MAX || self.dev.GetDeviceRemovedReason().is_err() {
                break Ended::Removed(t.elapsed(), self.dev.GetDeviceRemovedReason().err().map_or("fence lost".into(), |x| format!("{:#010x} {}", x.code().0 as u32, x.message())));
            }
            if done >= self.value {
                break Ended::Done(t.elapsed());
            }
            if t.elapsed() > timeout {
                break Ended::TimedOut(t.elapsed());
            }
            if t.elapsed() > Duration::from_millis(20) {
                std::thread::sleep(Duration::from_millis(1));
            } else {
                std::hint::spin_loop();
            }
        };
        if matches!(ended, Ended::Done(_)) {
            self.alloc.Reset().map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
            self.list.Reset(self.alloc, None).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
        }
        Ok(ended)
    }

    /// `submit` for the replay's own set-up and read-backs: anything but done is an error.
    unsafe fn finish(&mut self, what: &str) -> Result<(), String> {
        match self.submit(Duration::from_secs(60))? {
            Ended::Done(_) => Ok(()),
            Ended::Removed(_, why) => Err(format!("{what}: device removed ({why})")),
            Ended::TimedOut(t) => Err(format!("{what}: not finished after {} ms", t.as_millis())),
        }
    }
}

/// The capture `m` of `dir` run with `programs`; what it wrote goes to `out`. The exit code
/// (0, or 4 when the device was removed after the call finished); the process ends from here
/// with 3 or 4 when the call times out or takes the device with it.
pub(super) unsafe fn run(dir: &Path, out: &Path, m: &Manifest, o: &Opts, programs: &[Program], samplers: &[StaticSampler]) -> Result<i32, String> {
    if o.debug {
        match crate::gpu::agility::configuration_interface::<ID3D12Debug>(&CLSID_D3D12Debug) {
            Ok(d) => {
                d.EnableDebugLayer();
                println!("REPLAY the D3D12 debug layer is on");
            }
            Err(x) => println!("REPLAY WARNING: no D3D12 debug layer on this system ({x})"),
        }
    }
    let (dev, adapter) = device()?;
    println!("REPLAY device: {adapter}");
    let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    let mut run = Run { dev: &dev, queue: &queue, alloc: &alloc, list: &list, fence: &fence, value: 0 };
    let mut keep: Vec<ID3D12Resource> = Vec::new();

    // the pipeline
    let rs = pso::abi_root_signature_with(&dev, m.graphics, samplers).map_err(|x| format!("root signature with the static samplers {samplers:x?}: {x}"))?;
    let dxil = |stage: Stage| programs.iter().find(|p| p.stage == stage).map(|p| D3D12_SHADER_BYTECODE { pShaderBytecode: p.dxil.as_ptr() as *const c_void, BytecodeLength: p.dxil.len() });
    let t_pso = Instant::now();
    let mut rtv_formats = [DXGI_FORMAT(0); 8];
    let mut dsv_format = DXGI_FORMAT(0);
    let pipeline: ID3D12PipelineState = if m.graphics {
        let p = m.pso.as_ref().ok_or("the capture has no pipeline description")?;
        let bytes = unhex(&p.desc).map_err(|e| format!("the pipeline description: {e}"))?;
        if bytes.len() != std::mem::size_of::<D3D12_GRAPHICS_PIPELINE_STATE_DESC>() {
            return Err("the pipeline description has another size than this build's".into());
        }
        let mut d: D3D12_GRAPHICS_PIPELINE_STATE_DESC = std::ptr::read_unaligned(bytes.as_ptr() as *const _);
        let names: Vec<std::ffi::CString> = p.input.iter().map(|x| std::ffi::CString::new(x.name.as_str()).unwrap_or_default()).collect();
        let elems: Vec<D3D12_INPUT_ELEMENT_DESC> = p
            .input
            .iter()
            .zip(&names)
            .map(|(x, n)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(n.as_ptr() as *const u8),
                SemanticIndex: x.index,
                Format: DXGI_FORMAT(x.format),
                InputSlot: x.slot,
                AlignedByteOffset: x.offset,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(x.class),
                InstanceDataStepRate: x.step,
            })
            .collect();
        // every pointer of the description is the capture run's: all are set here
        std::ptr::write(&mut d.pRootSignature, std::mem::transmute_copy(&rs));
        d.VS = dxil(Stage::Vertex).ok_or("no vertex shader")?;
        d.PS = dxil(Stage::Pixel).unwrap_or_default();
        d.DS = Default::default();
        d.HS = Default::default();
        d.GS = Default::default();
        d.StreamOutput = Default::default();
        d.InputLayout = D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: if elems.is_empty() { std::ptr::null() } else { elems.as_ptr() }, NumElements: elems.len() as u32 };
        d.CachedPSO = Default::default();
        if p.rect_list_gs {
            println!("REPLAY WARNING: a rect list pipeline: its GS is not made, each rect is drawn as one triangle");
        }
        rtv_formats = d.RTVFormats;
        dsv_format = d.DSVFormat;
        println!(
            "REPLAY pipeline: {} render targets {:?}, DSV format {}, samples {}, depth (enable {}, write {}, func {}), stencil {}, blend0 enable {}, cull {}, topology type {}",
            d.NumRenderTargets,
            &d.RTVFormats[..d.NumRenderTargets.min(8) as usize].iter().map(|f| f.0).collect::<Vec<_>>(),
            d.DSVFormat.0,
            d.SampleDesc.Count,
            d.DepthStencilState.DepthEnable.as_bool(),
            d.DepthStencilState.DepthWriteMask.0,
            d.DepthStencilState.DepthFunc.0,
            d.DepthStencilState.StencilEnable.as_bool(),
            d.BlendState.RenderTarget[0].BlendEnable.as_bool(),
            d.RasterizerState.CullMode.0,
            d.PrimitiveTopologyType.0
        );
        dev.CreateGraphicsPipelineState(&d).map_err(|x| format!("CreateGraphicsPipelineState: {x}"))?
    } else {
        let d = D3D12_COMPUTE_PIPELINE_STATE_DESC { pRootSignature: std::mem::transmute_copy(&rs), CS: dxil(Stage::Compute).ok_or("no compute shader")?, ..Default::default() };
        dev.CreateComputePipelineState(&d).map_err(|x| format!("CreateComputePipelineState: {x}"))?
    };
    println!("REPLAY pipeline made in {} ms ({})", t_pso.elapsed().as_millis(), programs.iter().map(|p| format!("{} {} B", p.name, p.dxil.len())).collect::<Vec<_>>().join(", "));

    // the memory windows: buffers that hold the captured ranges at their offsets
    const TILE: u64 = 0x1_0000;
    let memory = if m.memory.is_empty() { Vec::new() } else { std::fs::read(dir.join(&m.memory_file)).map_err(|x| format!("{}: {x}", m.memory_file))? };
    let staged = if memory.is_empty() { None } else { Some(upload(&dev, &memory)?) };
    let mut windows: Vec<Option<ID3D12Resource>> = Vec::new();
    for k in 0..m.uma.windows.len().max(1) {
        let mine: Vec<_> = m.memory.iter().filter(|r| r.window as usize == k).collect();
        let Some(end) = mine.iter().map(|r| r.offset + r.len).max() else {
            windows.push(None);
            continue;
        };
        let size = end.div_ceil(TILE) * TILE;
        let buffer = if size <= 256 << 20 {
            committed(&dev, D3D12_HEAP_TYPE_DEFAULT, &buffer_desc(size, true), D3D12_RESOURCE_STATE_COMMON)?
        } else {
            // a reserved buffer with the tiles the ranges touch
            let mut r: Option<ID3D12Resource> = None;
            dev.CreateReservedResource(&buffer_desc(size, true), D3D12_RESOURCE_STATE_COMMON, None, &mut r).map_err(|x| format!("CreateReservedResource (window {k}, {} MB): {x}", size >> 20))?;
            let r = r.ok_or("CreateReservedResource")?;
            let mut tiles: Vec<u32> = mine.iter().flat_map(|r| (r.offset / TILE..(r.offset + r.len).div_ceil(TILE)).map(|t| t as u32)).collect();
            tiles.sort_unstable();
            tiles.dedup();
            let mut heap: Option<ID3D12Heap> = None;
            dev.CreateHeap(&D3D12_HEAP_DESC { SizeInBytes: tiles.len() as u64 * TILE, Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, Alignment: 0, Flags: D3D12_HEAP_FLAG_DENY_RT_DS_TEXTURES | D3D12_HEAP_FLAG_DENY_NON_RT_DS_TEXTURES }, &mut heap)
                .map_err(|x| format!("CreateHeap ({} tiles): {x}", tiles.len()))?;
            let heap = heap.ok_or("CreateHeap")?;
            let (mut coords, mut regions, mut flags, mut offsets, mut counts) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut i = 0;
            while i < tiles.len() {
                let mut n = 1;
                while i + n < tiles.len() && tiles[i + n] == tiles[i] + n as u32 {
                    n += 1;
                }
                coords.push(D3D12_TILED_RESOURCE_COORDINATE { X: tiles[i], ..Default::default() });
                regions.push(D3D12_TILE_REGION_SIZE { NumTiles: n as u32, UseBox: false.into(), ..Default::default() });
                flags.push(D3D12_TILE_RANGE_FLAG_NONE);
                offsets.push(i as u32);
                counts.push(n as u32);
                i += n;
            }
            queue.UpdateTileMappings(&r, coords.len() as u32, Some(coords.as_ptr()), Some(regions.as_ptr()), &heap, flags.len() as u32, Some(flags.as_ptr()), Some(offsets.as_ptr()), Some(counts.as_ptr()), D3D12_TILE_MAPPING_FLAG_NONE);
            std::mem::forget(heap);
            println!("REPLAY window {k}: a reserved buffer of {} MB with {} tiles mapped", size >> 20, tiles.len());
            r
        };
        list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
        for r in &mine {
            list.CopyBufferRegion(&buffer, r.offset, staged.as_ref().ok_or("no memory file")?, r.file_offset, r.len);
        }
        list.ResourceBarrier(&[transition(&buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
        windows.push(Some(buffer));
    }
    // u0 io (the SGPRs as the capture run had them: the same addresses), u1 res
    const RES_AT: u64 = 4096;
    let abi = committed(&dev, D3D12_HEAP_TYPE_DEFAULT, &buffer_desc(TILE, true), D3D12_RESOURCE_STATE_COMMON)?;
    let mut io: Vec<u8> = m.root.io.iter().flat_map(|w| w.to_le_bytes()).collect();
    io.resize(4 * crate::gpu::recomp::KERNEL_SGPRS, 0);
    let io_staged = upload(&dev, &io)?;
    list.ResourceBarrier(&[transition(&abi, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
    list.CopyBufferRegion(&abi, 0, &io_staged, 0, io.len() as u64);
    list.ResourceBarrier(&[transition(&abi, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);

    // the textures, in the state the call uses them in
    let is_target = |id: usize| m.targets.iter().any(|t| t.texture == Some(id));
    let is_depth = |id: usize| m.depth.as_ref().is_some_and(|t| t.texture == Some(id));
    let has_uav = |id: usize| m.views.iter().any(|v| v.uav && v.texture == id);
    let state = |t: &Texture| {
        if is_target(t.id) {
            D3D12_RESOURCE_STATE_RENDER_TARGET
        } else if is_depth(t.id) {
            D3D12_RESOURCE_STATE_DEPTH_WRITE
        } else if has_uav(t.id) && t.flags & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0 {
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS
        } else if t.flags & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0 {
            D3D12_RESOURCE_STATE_DEPTH_READ | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
        } else {
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
        }
    };
    let mut textures: Vec<ID3D12Resource> = Vec::new();
    for t in &m.textures {
        let d = texture_desc(t);
        let data = match &t.file {
            Some(f) => Some(std::fs::read(dir.join(f)).map_err(|x| format!("{f}: {x}"))?),
            None => None,
        };
        let r = committed(&dev, D3D12_HEAP_TYPE_DEFAULT, &d, if data.is_some() { D3D12_RESOURCE_STATE_COPY_DEST } else { state(t) })?;
        if let Some(data) = data {
            let mut fps = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); t.subresources as usize];
            let mut total = 0u64;
            dev.GetCopyableFootprints(&d, 0, t.subresources, 0, Some(fps.as_mut_ptr()), None, None, Some(&mut total));
            if total != data.len() as u64 {
                return Err(format!("texture {}: its file has {} bytes, this device lays it out in {total}", t.id, data.len()));
            }
            let staged = upload(&dev, &data)?;
            for (k, fp) in fps.iter().enumerate() {
                let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&r), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: k as u32 } };
                let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&staged), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *fp } };
                list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            }
            list.ResourceBarrier(&[transition(&r, D3D12_RESOURCE_STATE_COPY_DEST, state(t))]);
            keep.push(staged);
        } else {
            println!("REPLAY WARNING: texture {} ({:?}) has no contents in the capture: it starts empty", t.id, t.uses);
        }
        textures.push(r);
    }
    run.finish("uploads")?;
    keep.clear();
    drop(staged);
    drop(io_staged);

    // the views, at the capture's heap indices
    let n_heap = heap_size(m);
    let heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: n_heap, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 }).map_err(|x| format!("a view heap of {n_heap}: {x}"))?;
    let inc = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
    let h0 = heap.GetCPUDescriptorHandleForHeapStart().ptr;
    let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: h0 + i as usize * inc };
    // a slot's image without a view in the capture: null views of its kind
    for s in m.slots.iter().filter(|s| s.kind == "image") {
        let (srv_dim, uav_dim) = match s.words[3] >> 28 {
            8 => (D3D12_SRV_DIMENSION_TEXTURE1D, D3D12_UAV_DIMENSION_TEXTURE1D),
            10 => (D3D12_SRV_DIMENSION_TEXTURE3D, D3D12_UAV_DIMENSION_TEXTURE3D),
            11 => (D3D12_SRV_DIMENSION_TEXTURECUBEARRAY, D3D12_UAV_DIMENSION_TEXTURE2DARRAY),
            12 => (D3D12_SRV_DIMENSION_TEXTURE1DARRAY, D3D12_UAV_DIMENSION_TEXTURE1DARRAY),
            13 => (D3D12_SRV_DIMENSION_TEXTURE2DARRAY, D3D12_UAV_DIMENSION_TEXTURE2DARRAY),
            14 => (D3D12_SRV_DIMENSION_TEXTURE2DMS, D3D12_UAV_DIMENSION_TEXTURE2D),
            15 => (D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY, D3D12_UAV_DIMENSION_TEXTURE2DARRAY),
            _ => (D3D12_SRV_DIMENSION_TEXTURE2D, D3D12_UAV_DIMENSION_TEXTURE2D),
        };
        let srv = D3D12_SHADER_RESOURCE_VIEW_DESC { Format: DXGI_FORMAT(28), ViewDimension: srv_dim, Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, ..Default::default() };
        dev.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&srv), at(s.words[0]));
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC { Format: DXGI_FORMAT(42), ViewDimension: uav_dim, ..Default::default() };
        dev.CreateUnorderedAccessView(None::<&ID3D12Resource>, None::<&ID3D12Resource>, Some(&uav), at(s.words[0] + 1));
    }
    for v in &m.views {
        let r = textures.get(v.texture).ok_or("a view of an unknown texture")?;
        if v.uav {
            let d: Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> = view_desc(&v.desc);
            dev.CreateUnorderedAccessView(r, None::<&ID3D12Resource>, d.as_ref().map(|d| d as *const _), at(v.index));
        } else {
            let d: Option<D3D12_SHADER_RESOURCE_VIEW_DESC> = view_desc(&v.desc);
            dev.CreateShaderResourceView(r, d.as_ref().map(|d| d as *const _), at(v.index));
        }
    }
    let unviewed = m.slots.iter().filter(|s| s.kind == "image" && !m.views.iter().any(|v| v.index == s.words[0] || v.index == s.words[0] + 1)).count();
    if unviewed > 0 {
        println!("REPLAY note: {unviewed} image slot(s) of the tables have no view in the capture (null views)");
    }

    // the sampler heap: the title's samplers at their indices, a plain one everywhere else
    const SAMPLERS: u32 = 2048;
    let sampler_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, NumDescriptors: SAMPLERS, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 }).map_err(|x| format!("a sampler heap: {x}"))?;
    let s_inc = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER) as usize;
    let s0 = sampler_heap.GetCPUDescriptorHandleForHeapStart().ptr;
    let plain = D3D12_SAMPLER_DESC { Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR, AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP, AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP, AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP, MaxLOD: f32::MAX, MaxAnisotropy: 1, ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER, ..Default::default() };
    for k in 0..SAMPLERS {
        dev.CreateSampler(&plain, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: s0 + k as usize * s_inc });
    }
    for s in m.samplers.iter().filter(|s| s.index < SAMPLERS) {
        if let Some(d) = view_desc::<D3D12_SAMPLER_DESC>(&Some(s.desc.clone())) {
            dev.CreateSampler(&d, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: s0 + s.index as usize * s_inc });
        }
    }

    // the size of what is drawn into
    let (width, height) = m
        .targets
        .iter()
        .chain(m.depth.iter())
        .find_map(|t| t.texture.map(|id| (m.textures[id].width as u32, m.textures[id].height)))
        .or_else(|| m.state.viewports.first().map(|v| ((v[0] + v[2]).ceil() as u32, (v[1] + v[3]).ceil() as u32)))
        .unwrap_or((64, 64));
    // --loops: the counters (`count_loops`) and the image of the most passes per pixel
    let loops = if o.loops && m.graphics {
        let b = committed(&dev, D3D12_HEAP_TYPE_DEFAULT, &buffer_desc(256, true), D3D12_RESOURCE_STATE_COMMON)?;
        let td = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: width as u64, Height: height, DepthOrArraySize: 1, MipLevels: 1, Format: DXGI_FORMAT(42), SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, ..Default::default() };
        let t = committed(&dev, D3D12_HEAP_TYPE_DEFAULT, &td, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
        let raw = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT(39),
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Buffer: D3D12_BUFFER_UAV { FirstElement: 0, NumElements: 64, StructureByteStride: 0, CounterOffsetInBytes: 0, Flags: D3D12_BUFFER_UAV_FLAG_RAW } },
        };
        dev.CreateUnorderedAccessView(&b, None::<&ID3D12Resource>, Some(&raw), at(n_heap - 2));
        dev.CreateUnorderedAccessView(&t, None::<&ID3D12Resource>, None, at(n_heap - 1));
        // (zeroed: UAV clears need more set-up than a copy of zeros)
        let zero = upload(&dev, &[0u8; 256])?;
        list.ResourceBarrier(&[transition(&b, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
        list.CopyBufferRegion(&b, 0, &zero, 0, 256);
        list.ResourceBarrier(&[transition(&b, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
        keep.push(zero);
        Some((b, t, td))
    } else {
        None
    };

    // the targets
    let n_rt = m.targets.len();
    let rtv_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: n_rt.max(1) as u32, ..Default::default() }).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    let dsv_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV, NumDescriptors: 1, ..Default::default() }).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
    let rtv_inc = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
    let rtv0 = rtv_heap.GetCPUDescriptorHandleForHeapStart().ptr;
    // (name, resource, description, the format it is seen in, its state)
    let mut outputs: Vec<(String, ID3D12Resource, D3D12_RESOURCE_DESC, i32, D3D12_RESOURCE_STATES)> = Vec::new();
    let mut rtvs = Vec::new();
    for (k, t) in m.targets.iter().enumerate() {
        let view: Option<D3D12_RENDER_TARGET_VIEW_DESC> = view_desc(&t.desc);
        let (r, d) = match t.texture {
            Some(id) => (textures[id].clone(), texture_desc(&m.textures[id])),
            None => {
                // a target XCL did not know the resource of: an empty one in the pipeline's format
                let d = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: width as u64, Height: height, DepthOrArraySize: 1, MipLevels: 1, Format: rtv_formats[k.min(7)], SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, ..Default::default() };
                println!("REPLAY WARNING: render target {k} is not in the capture: an empty {width}x{height} one of format {}", d.Format.0);
                (committed(&dev, D3D12_HEAP_TYPE_DEFAULT, &d, D3D12_RESOURCE_STATE_RENDER_TARGET)?, d)
            }
        };
        let h = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: rtv0 + k * rtv_inc };
        let view = if t.texture.is_some() { view } else { None };
        dev.CreateRenderTargetView(&r, view.as_ref().map(|d| d as *const _), h);
        rtvs.push(h);
        let seen = view.map(|v| v.Format.0).filter(|&f| f != 0).or(Some(rtv_formats[k.min(7)].0).filter(|&f| f != 0)).unwrap_or(d.Format.0);
        outputs.push((format!("rt{k}"), r, d, seen, D3D12_RESOURCE_STATE_RENDER_TARGET));
    }
    let dsv = match m.depth.as_ref().and_then(|t| Some((t, t.texture?))) {
        Some((t, id)) => {
            let view: Option<D3D12_DEPTH_STENCIL_VIEW_DESC> = view_desc(&t.desc);
            let h = dsv_heap.GetCPUDescriptorHandleForHeapStart();
            dev.CreateDepthStencilView(&textures[id], view.as_ref().map(|d| d as *const _), h);
            outputs.push(("depth".into(), textures[id].clone(), texture_desc(&m.textures[id]), dsv_format.0, D3D12_RESOURCE_STATE_DEPTH_WRITE));
            Some(h)
        }
        None => {
            if m.depth.is_some() {
                println!("REPLAY WARNING: the depth buffer is not in the capture: drawn without one");
            }
            None
        }
    };
    for t in m.textures.iter().filter(|t| !is_target(t.id) && !is_depth(t.id) && state(t) == D3D12_RESOURCE_STATE_UNORDERED_ACCESS) {
        let seen = m.views.iter().filter(|v| v.uav && v.texture == t.id).find_map(|v| view_desc::<D3D12_UNORDERED_ACCESS_VIEW_DESC>(&v.desc)).map(|v| v.Format.0).filter(|&f| f != 0).unwrap_or(t.format);
        outputs.push((format!("uav_tex{}", t.id), textures[t.id].clone(), texture_desc(t), seen, D3D12_RESOURCE_STATE_UNORDERED_ACCESS));
    }

    // the call
    let query: ID3D12QueryHeap = {
        let mut q: Option<ID3D12QueryHeap> = None;
        dev.CreateQueryHeap(&D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2, NodeMask: 0 }, &mut q).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
        q.ok_or("CreateQueryHeap")?
    };
    let times = committed(&dev, D3D12_HEAP_TYPE_READBACK, &buffer_desc(256, false), D3D12_RESOURCE_STATE_COPY_DEST)?;
    let frequency = queue.GetTimestampFrequency().unwrap_or(1).max(1);
    let first = windows.iter().flatten().next().map_or(abi.GetGPUVirtualAddress(), |w| w.GetGPUVirtualAddress());
    let base_of = |k: usize| windows.get(k).and_then(|w| w.as_ref()).map(|w| w.GetGPUVirtualAddress());
    let pc_va = |address: u64| -> Option<u64> {
        let k = m.uma.windows.iter().position(|&(b, s)| address >= b && address < b + s)?;
        Some(base_of(k)? + (address - m.uma.windows[k].0))
    };
    let call = m.call.ok_or("the capture has no call arguments")?;
    let mut code = 0;
    for pass in 0..o.repeat.max(1) {
        list.SetDescriptorHeaps(&[Some(heap.clone()), Some(sampler_heap.clone())]);
        let roots = [abi.GetGPUVirtualAddress(), abi.GetGPUVirtualAddress() + RES_AT].into_iter().chain((0..4).map(|k| base_of(k).unwrap_or(first)));
        if m.graphics {
            list.SetGraphicsRootSignature(&rs);
            for (k, v) in roots.enumerate() {
                list.SetGraphicsRootUnorderedAccessView(k as u32, v);
            }
            list.SetPipelineState(&pipeline);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(m.state.topology.unwrap_or(4) as i32));
            for v in &m.vertex_buffers {
                match pc_va(v.address) {
                    Some(va) => list.IASetVertexBuffers(v.slot, Some(&[D3D12_VERTEX_BUFFER_VIEW { BufferLocation: va, SizeInBytes: v.size, StrideInBytes: v.stride }])),
                    None if pass == 0 => println!("REPLAY WARNING: vertex buffer {} is not in the captured memory: not bound", v.slot),
                    None => {}
                }
            }
            if let Some(v) = &m.index_buffer {
                match pc_va(v.address) {
                    Some(va) => list.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW { BufferLocation: va, SizeInBytes: v.size, Format: DXGI_FORMAT(v.format) })),
                    None if pass == 0 => println!("REPLAY WARNING: the index buffer is not in the captured memory: not bound"),
                    None => {}
                }
            }
            let viewports: Vec<D3D12_VIEWPORT> = m.state.viewports.iter().map(|v| D3D12_VIEWPORT { TopLeftX: v[0], TopLeftY: v[1], Width: v[2], Height: v[3], MinDepth: v[4], MaxDepth: v[5] }).collect();
            let scissors: Vec<windows::Win32::Foundation::RECT> = m.state.scissors.iter().map(|r| windows::Win32::Foundation::RECT { left: r[0], top: r[1], right: r[2], bottom: r[3] }).collect();
            if viewports.is_empty() {
                list.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: width as f32, Height: height as f32, MinDepth: 0.0, MaxDepth: 1.0 }]);
            } else {
                list.RSSetViewports(&viewports);
            }
            if scissors.is_empty() {
                list.RSSetScissorRects(&[windows::Win32::Foundation::RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 }]);
            } else {
                list.RSSetScissorRects(&scissors);
            }
            if let Some(f) = &m.state.blend_factor {
                list.OMSetBlendFactor(Some(f));
            }
            if let Some(s) = m.state.stencil_ref {
                list.OMSetStencilRef(s);
            }
            list.OMSetRenderTargets(rtvs.len() as u32, if rtvs.is_empty() { None } else { Some(rtvs.as_ptr()) }, false, dsv.as_ref().map(|d| d as *const _));
        } else {
            list.SetComputeRootSignature(&rs);
            for (k, v) in roots.enumerate() {
                list.SetComputeRootUnorderedAccessView(k as u32, v);
            }
            list.SetPipelineState(&pipeline);
        }
        list.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0);
        match call {
            Call::Draw { vertices, instances, first_vertex, first_instance } => list.DrawInstanced(vertices, instances, first_vertex, first_instance),
            Call::DrawIndexed { indices, instances, first_index, base_vertex, first_instance } => list.DrawIndexedInstanced(indices, instances, first_index, base_vertex, first_instance),
            Call::Dispatch { x, y, z } => list.Dispatch(x, y, z),
            Call::Indirect { .. } => return Err("an ExecuteIndirect: its arguments are not in the capture".into()),
        }
        list.EndQuery(&query, D3D12_QUERY_TYPE_TIMESTAMP, 1);
        list.ResolveQueryData(&query, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &times, 0);
        println!("REPLAY submitted: {call:?} (pass {})", pass + 1);
        let _ = std::io::stdout().flush();
        match run.submit(o.timeout)? {
            Ended::Done(wall) => {
                let mut p: *mut c_void = std::ptr::null_mut();
                times.Map(0, None, Some(&mut p)).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
                let t = std::slice::from_raw_parts(p as *const u64, 2).to_vec();
                times.Unmap(0, None);
                println!("REPLAY finished: GPU time {:.3} ms, {:.3} ms from submission to done", t[1].wrapping_sub(t[0]) as f64 * 1000.0 / frequency as f64, wall.as_secs_f64() * 1000.0);
            }
            Ended::Removed(t, why) => {
                println!("REPLAY RESULT: device removed after {} ms ({why})", t.as_millis());
                let _ = std::io::stdout().flush();
                std::process::exit(4);
            }
            Ended::TimedOut(t) => {
                println!("REPLAY RESULT: timed out after {} ms (the call was still running)", t.as_millis());
                let _ = std::io::stdout().flush();
                std::process::exit(3);
            }
        }
        if dev.GetDeviceRemovedReason().is_err() {
            code = 4;
            break;
        }
    }

    // what the call wrote
    let mut read = Vec::new();
    for (name, r, d, seen, state) in &outputs {
        if d.SampleDesc.Count > 1 {
            println!("REPLAY {name}: multisampled, not read back");
            continue;
        }
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let (mut rows, mut row_bytes, mut total) = (0u32, 0u64, 0u64);
        dev.GetCopyableFootprints(d, 0, 1, 0, Some(&mut fp), Some(&mut rows), Some(&mut row_bytes), Some(&mut total));
        let rb = committed(&dev, D3D12_HEAP_TYPE_READBACK, &buffer_desc(total, false), D3D12_RESOURCE_STATE_COPY_DEST)?;
        list.ResourceBarrier(&[transition(r, *state, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(r), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        read.push((name.clone(), rb, fp, rows, row_bytes, total, *seen));
    }
    let loop_read = match &loops {
        Some((b, t, td)) => {
            let rb = committed(&dev, D3D12_HEAP_TYPE_READBACK, &buffer_desc(256, false), D3D12_RESOURCE_STATE_COPY_DEST)?;
            list.ResourceBarrier(&[transition(b, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE), transition(t, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyBufferRegion(&rb, 0, b, 0, 256);
            let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let (mut rows, mut row_bytes, mut total) = (0u32, 0u64, 0u64);
            dev.GetCopyableFootprints(td, 0, 1, 0, Some(&mut fp), Some(&mut rows), Some(&mut row_bytes), Some(&mut total));
            let image = committed(&dev, D3D12_HEAP_TYPE_READBACK, &buffer_desc(total, false), D3D12_RESOURCE_STATE_COPY_DEST)?;
            let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&image), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
            let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(t), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            read.push(("loops".into(), image, fp, rows, row_bytes, total, 42));
            Some(rb)
        }
        None => None,
    };
    run.finish("read-back")?;
    let mapped = |rb: &ID3D12Resource, n: usize| -> Result<Vec<u8>, String> {
        let mut p: *mut c_void = std::ptr::null_mut();
        rb.Map(0, None, Some(&mut p)).map_err(|x| format!("{x} (replay/gpu.rs:{})", line!()))?;
        let v = std::slice::from_raw_parts(p as *const u8, n).to_vec();
        rb.Unmap(0, None);
        Ok(v)
    };
    if let Some(rb) = loop_read {
        let b = mapped(&rb, 256)?;
        let w = |k: usize| u32::from_le_bytes(b[4 * k..4 * k + 4].try_into().unwrap());
        let (sum, lanes, most) = (w(0) as u64 | (w(1) as u64) << 32, w(2), w(3));
        println!("REPLAY loops: {lanes} pixel shader lanes ran, {sum} loop passes in all, mean {:.2} per lane, most {most} in one lane (a lane's budget is 65536 a loop)", if lanes > 0 { sum as f64 / lanes as f64 } else { 0.0 });
        let each: Vec<String> = (0..16).filter(|&k| w(5 + 2 * k) != 0).map(|k| format!("#{k}: {} passes, most {}", w(4 + 2 * k), w(5 + 2 * k))).collect();
        println!("REPLAY loops, each (in the order of the HLSL): {}", each.join("; "));
        let debug: Vec<String> = (0..8).filter(|&j| w(38 + 3 * j) != 0).map(|j| format!("xcl_d{j}: most {:#x}, OR {:#x}, not 0 in {} lanes", w(36 + 3 * j), w(37 + 3 * j), w(38 + 3 * j))).collect();
        if !debug.is_empty() {
            println!("REPLAY debug words: {}", debug.join("; "));
        }
    }
    for (name, rb, fp, rows, row_bytes, total, seen) in read {
        let data = mapped(&rb, total as usize)?;
        // rows without their padding
        let (w, h, depth) = (fp.Footprint.Width as usize, rows as usize, fp.Footprint.Depth.max(1) as usize);
        let mut tight = Vec::with_capacity(row_bytes as usize * h * depth);
        for y in 0..h * depth {
            let at = y * fp.Footprint.RowPitch as usize;
            tight.extend_from_slice(&data[at..at + row_bytes as usize]);
        }
        let raw = out.join(format!("{name}.bin"));
        std::fs::write(&raw, &tight).map_err(|x| x.to_string())?;
        let hash = crate::gpu::shader::fnv64(&tight);
        match texel_image::image(seen, w, h, depth, row_bytes as usize, &tight) {
            Some(img) => {
                let png = out.join(format!("{name}.png"));
                std::fs::write(&png, texel_image::encode_png(&img.rgb, img.width, img.height)).map_err(|x| x.to_string())?;
                println!("REPLAY {name}: {w}x{h}x{depth} format {seen}, fnv64 {hash:016x}, {} of {} texels not zero, channel min {:?} max {:?} → {}", img.nonzero, w * h * depth, img.min, img.max, png.display());
            }
            None => println!("REPLAY {name}: {w}x{h}x{depth} format {seen} (no PNG for this format), fnv64 {hash:016x} → {}", raw.display()),
        }
    }
    if o.debug {
        if let Ok(q) = dev.cast::<ID3D12InfoQueue>() {
            let n = q.GetNumStoredMessages();
            println!("REPLAY debug layer: {n} message(s)");
            for i in 0..n.min(40) {
                let mut len = 0usize;
                let _ = q.GetMessage(i, None, &mut len);
                let mut buf = vec![0u64; len.div_ceil(8) + 1];
                if q.GetMessage(i, Some(buf.as_mut_ptr() as *mut D3D12_MESSAGE), &mut len).is_ok() {
                    let msg = &*(buf.as_ptr() as *const D3D12_MESSAGE);
                    println!("REPLAY debug layer: {}", String::from_utf8_lossy(std::slice::from_raw_parts(msg.pDescription, msg.DescriptionByteLength.saturating_sub(1))));
                }
            }
        }
    }
    println!("REPLAY RESULT: done, output in {}", out.display());
    Ok(code)
}
