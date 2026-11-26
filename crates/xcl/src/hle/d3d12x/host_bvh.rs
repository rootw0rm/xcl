//! Opt-in shadow DXR builds from XboxBvhBuilder calls. The title keeps its console BVH.
//! This experimental capture retains at most 256 MiB of native buffers until process exit.
//! Geometry inputs are copied on the active recording list. TLAS capture requires readable
//! CPU instance descriptors. Query bindings are prepared for a later shader consumer.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop, MaybeUninit};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::modules::MappedModule;
use crate::pe::{ExportTarget, ImportName};
use super::{console_list, real, title_list};

const LIMIT: u64 = 256 << 20;
const MAX_DESCS: usize = 8192;
const BUILD: &str = "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z";
const REHYDRATE: &str = "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z";
const COPY: &str = "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z";

static BUILD_FN: AtomicUsize = AtomicUsize::new(0);
static REHYDRATE_FN: AtomicUsize = AtomicUsize::new(0);
static COPY_FN: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct BatchedDesc {
    build: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC,
    postbuild_count: u32,
    _padding: u32,
    postbuild: *const D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC,
    _tail: [u64; 2],
}

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_HOST_DXR").is_ok_and(|v| v == "1") || query_requested())
}

pub(crate) fn query_requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_HOST_RAYQUERY").is_ok_and(|v| v == "1"))
}

pub(crate) fn query_supported() -> bool {
    if !query_requested() { return false; }
    unsafe {
        let raw = real::device_raw();
        let Some(dev) = ID3D12Device::from_raw_borrowed(&raw) else { return false };
        dev.cast::<ID3D12Device5>().is_ok_and(|dev| supports_dxr(&dev).is_ok())
    }
}

fn replacement(name: &str, original: usize) -> Option<usize> {
    let (saved, hook) = match name {
        BUILD => (&BUILD_FN, build as usize),
        REHYDRATE => (&REHYDRATE_FN, rehydrate as usize),
        COPY => (&COPY_FN, copy as usize),
        _ => return None,
    };
    match saved.compare_exchange(0, original, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            crate::xlog_info!("Host DXR: observing {} calls", name.split('@').next().unwrap_or(name));
            Some(hook)
        }
        Err(existing) => (existing == original).then_some(hook),
    }
}

/// The same path handles static imports, delay imports and GetProcAddress.
pub(crate) fn intercept(module: &MappedModule, what: &ImportName, rva: u32, original: usize) -> usize {
    if module.name != "xboxbvhbuilder.dll" || !enabled() {
        return original;
    }
    let name = match what {
        ImportName::Name { name, .. } => Some(name.as_str()),
        ImportName::Ordinal(_) => module.exports.by_name.iter().find_map(|(name, target)| {
            matches!(target, ExportTarget::Rva(value) if *value == rva).then_some(name.as_str())
        }),
    };
    name.and_then(|name| replacement(name, original)).unwrap_or(original)
}

unsafe extern "system-unwind" fn build(this: *mut c_void, list: *mut c_void, count: u32, descs: *const BatchedDesc) {
    let original: unsafe extern "system-unwind" fn(*mut c_void, *mut c_void, u32, *const BatchedDesc) =
        std::mem::transmute(BUILD_FN.load(Ordering::Acquire));
    original(this, list, count, descs);
    observe(|| {
        for desc in read_items::<BatchedDesc>(descs as usize, count as usize)? {
            observe(|| record(list, &desc.build));
        }
        Ok(())
    });
}

unsafe extern "system-unwind" fn rehydrate(
    this: *mut c_void, list: *mut c_void, count: u32,
    descs: *const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC, sources: *const u64,
) -> i32 {
    let original: unsafe extern "system-unwind" fn(
        *mut c_void, *mut c_void, u32, *const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC, *const u64,
    ) -> i32 = std::mem::transmute(REHYDRATE_FN.load(Ordering::Acquire));
    let result = original(this, list, count, descs, sources);
    if result >= 0 {
        observe(|| {
            for desc in read_items::<D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC>(descs as usize, count as usize)? {
                forget(desc.DestAccelerationStructureData);
            }
            // Rehydration applies scale, translation and format transforms from its compressed
            // source header. Rebuilding the input geometry alone would produce a different BLAS.
            if count == 0 { Ok(()) } else { Err("compressed rehydration transforms are not captured".into()) }
        });
    }
    result
}

unsafe extern "system-unwind" fn copy(this: *mut c_void, list: *mut c_void, dest: u64, source: u64, mode: i32) {
    let original: unsafe extern "system-unwind" fn(*mut c_void, *mut c_void, u64, u64, i32) =
        std::mem::transmute(COPY_FN.load(Ordering::Acquire));
    original(this, list, dest, source, mode);
    observe(|| {
        let list = recording_list(list)?;
        let dev = device(&list)?;
        let mut states = STATES.lock().unwrap();
        let state = states.get_or_insert_with(HashMap::new).entry(dev.as_raw() as usize).or_default();
        record_copy_on(state, &dev, &list, dest, source, mode)?;
        Ok(())
    });
}

fn observe(run: impl FnOnce() -> Result<(), String>) {
    if let Err(error) = run() {
        static ERRORS: AtomicU64 = AtomicU64::new(0);
        let count = ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 8 || count.is_power_of_two() {
            crate::xlog_info!("Host DXR: shadow build skipped ({count}): {error}");
        }
    }
}

// All T at the call sites are POD descriptor records or integer words.
unsafe fn read_items<T: Copy>(address: usize, count: usize) -> Result<Vec<T>, String> {
    let bytes = count.checked_mul(size_of::<T>()).ok_or("descriptor size overflow")?;
    if count > MAX_DESCS || address.checked_add(bytes).is_none() {
        return Err("descriptor capture limit exceeded".into());
    }
    if bytes == 0 {
        return Ok(Vec::new());
    }
    let mut values = Vec::<MaybeUninit<T>>::new();
    values.try_reserve_exact(count).map_err(|_| "descriptor allocation failed")?;
    values.resize_with(count, MaybeUninit::uninit);
    let mut done = 0;
    if address == 0 || ReadProcessMemory(
        GetCurrentProcess(), address as *const c_void, values.as_mut_ptr().cast(), bytes, &mut done,
    ) == 0 || done != bytes {
        return Err("descriptor memory is unreadable".into());
    }
    let mut values = ManuallyDrop::new(values);
    Ok(Vec::from_raw_parts(values.as_mut_ptr().cast(), values.len(), values.capacity()))
}

unsafe fn native_vtable<T: Interface>(object: &T) -> &T::Vtable {
    if real::is_adopted(object.as_raw()) {
        let table = *(object.as_raw() as *const *const usize);
        &*(*table.sub(1) as *const T::Vtable)
    } else {
        object.vtable()
    }
}

unsafe fn recording_list(list: *mut c_void) -> Result<ID3D12GraphicsCommandList4, String> {
    if list.is_null() || !crate::gpu::shader::readable(list as usize, size_of::<usize>()) {
        return Err("command list is unavailable".into());
    }
    if console_list::pc_list(list).is_some() {
        return Err("command-buffer replay is not supported by shadow DXR".into());
    }
    let raw = title_list::recording(list).unwrap_or(list);
    ID3D12GraphicsCommandList::from_raw_borrowed(&raw).ok_or("invalid command list")?
        .cast().map_err(|e| format!("command list has no DXR interface: {e}"))
}

unsafe fn device(list: &ID3D12GraphicsCommandList4) -> Result<ID3D12Device5, String> {
    let mut dev: Option<ID3D12Device5> = None;
    list.GetDevice(&mut dev).map_err(|e| e.to_string())?;
    let dev = dev.ok_or("command list has no device")?;
    supports_dxr(&dev)?;
    Ok(dev)
}

unsafe fn supports_dxr(dev: &ID3D12Device5) -> Result<(), String> {
    let base = &*(native_vtable(dev) as *const _ as *const ID3D12Device_Vtbl);
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
    (base.CheckFeatureSupport)(dev.as_raw(), D3D12_FEATURE_D3D12_OPTIONS5, (&mut options as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS5).cast(), size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS5>() as u32)
        .ok().map_err(|e| e.to_string())?;
    if options.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0 {
        return Err("host does not support DXR 1.1".into());
    }
    Ok(())
}

#[derive(Clone)]
struct HostAs {
    resource: ID3D12Resource,
    bytes: u64,
    guest_bytes: u64,
    kind: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE,
    opaque: bool,
    serial: u64,
    owner: usize,
}

#[derive(Default)]
struct State {
    structures: HashMap<u64, HostAs>,
    // Buffers stay alive across command-list resets and submissions. The byte cap includes
    // scratch, input snapshots and old generations, including failed or unsubmitted builds.
    retained: Vec<ID3D12Resource>,
    bytes: u64,
    recorded: u64,
    serial: u64,
}

impl State {
    fn retain(&mut self, prepared: Prepared) {
        self.bytes += prepared.bytes;
        self.retained.extend(prepared.resources);
    }
}

static STATES: Mutex<Option<HashMap<usize, State>>> = Mutex::new(None);

/// A reset or released recording no longer proves producer-before-consumer ordering.
pub(super) fn forget_list(list: usize) {
    if let Some(states) = STATES.lock().unwrap().as_mut() {
        for state in states.values_mut() {
            let bottom_removed = state.structures.values().any(|s| s.owner == list
                && s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL);
            state.structures.retain(|_, s| s.owner != list
                && !(bottom_removed && s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL));
        }
    }
}

/// Freed or remapped console memory cannot keep naming a previously captured AS.
pub(crate) fn forget_range(base: usize, bytes: usize) {
    if bytes == 0 { return; }
    let end = base.saturating_add(bytes) as u64;
    if let Some(states) = STATES.lock().unwrap().as_mut() {
        for state in states.values_mut() {
            let mut bottom_removed = false;
            state.structures.retain(|&address, value| {
                let overlap = address < end && (base as u64) < address.saturating_add(value.guest_bytes);
                bottom_removed |= overlap && value.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL;
                !overlap
            });
            if bottom_removed { state.structures.retain(|_, s| s.kind != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL); }
        }
    }
}

pub(super) struct QueryBinding {
    pub console: u64,
    pub native: u64,
}

pub(super) unsafe fn query_binding(list: &ID3D12GraphicsCommandList) -> Option<QueryBinding> {
    if !query_requested() { return None; }
    let mut dev: Option<ID3D12Device5> = None;
    list.GetDevice(&mut dev).ok()?;
    let dev = dev?;
    let states = STATES.lock().unwrap();
    let state = states.as_ref()?.get(&(dev.as_raw() as usize))?;
    // Other addresses keep the original traversal; prefer the most recent compatible TLAS.
    let (&console, value) = state.structures.iter().filter(|(_, value)| {
        value.opaque && value.owner == list.as_raw() as usize
            && value.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL
    }).max_by_key(|(_, value)| value.serial)?;
    static LAST: AtomicU64 = AtomicU64::new(0);
    if LAST.swap(console, Ordering::Relaxed) != console {
        crate::xlog_info!("Host RayQuery: native TLAS available at console {console:#x}, generation {}", value.serial);
    }
    Some(QueryBinding { console, native: value.resource.GetGPUVirtualAddress() })
}

fn managed_span(address: u64, bytes: u64) -> bool {
    crate::gpu::uma::get().is_some_and(|u| {
        bytes != 0 && u.contains(address as usize)
            && address.checked_add(bytes - 1).is_some_and(|end| u.contains(end as usize))
    })
}

fn forget(dest: u64) {
    if let Some(states) = STATES.lock().unwrap().as_mut() {
        for state in states.values_mut() {
            if state.structures.remove(&dest).is_some_and(|s| s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL) {
                state.structures.retain(|_, value| value.kind != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL);
            }
        }
    }
}

struct Prepared {
    resources: Vec<ID3D12Resource>,
    copies: Vec<(ID3D12Resource, u64, ID3D12Resource, u64)>,
    bytes: u64,
    limit: u64,
}

impl Prepared {
    fn new(limit: u64) -> Self {
        Self { resources: Vec::new(), copies: Vec::new(), bytes: 0, limit }
    }

    unsafe fn buffer(&mut self, dev: &ID3D12Device5, bytes: u64, heap: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource, String> {
        let bytes = bytes.checked_add(255).map(|n| n & !255).filter(|&n| n > 0).ok_or("invalid native buffer size")?;
        let committed = bytes.checked_add(65535).map(|n| n & !65535).ok_or("native allocation size overflow")?;
        if committed > self.limit.saturating_sub(self.bytes) {
            return Err("256 MiB shadow DXR capture budget reached".into());
        }
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: bytes, Height: 1,
            DepthOrArraySize: 1, MipLevels: 1, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: if heap == D3D12_HEAP_TYPE_DEFAULT && state != D3D12_RESOURCE_STATE_COPY_DEST {
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
            } else { D3D12_RESOURCE_FLAG_NONE },
            ..Default::default()
        };
        let properties = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
        let base = &*(native_vtable(dev) as *const _ as *const ID3D12Device_Vtbl);
        let mut resource = None::<ID3D12Resource>;
        (base.CreateCommittedResource)(
            dev.as_raw(), &properties, D3D12_HEAP_FLAG_NONE, &desc, state, std::ptr::null(),
            &ID3D12Resource::IID, (&mut resource as *mut Option<ID3D12Resource>).cast(),
        ).ok().map_err(|e| e.to_string())?;
        let resource = resource.ok_or("native buffer creation returned null")?;
        self.bytes += committed;
        self.resources.push(resource.clone());
        Ok(resource)
    }

    unsafe fn input(
        &mut self, dev: &ID3D12Device5, address: u64, bytes: u64,
        source: &dyn Fn(u64, u64) -> Option<(ID3D12Resource, u64)>,
    ) -> Result<u64, String> {
        let (original, offset) = source(address, bytes)
            .ok_or_else(|| format!("geometry {address:#x} ({bytes} bytes) is outside a mapped UMA window"))?;
        let buffer = self.buffer(dev, bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST)?;
        let address = buffer.GetGPUVirtualAddress();
        self.copies.push((original, offset, buffer, bytes));
        Ok(address)
    }
}

fn vertex_bytes(format: DXGI_FORMAT) -> Option<u64> {
    match format {
        DXGI_FORMAT_R32G32B32_FLOAT => Some(12),
        DXGI_FORMAT_R32G32_FLOAT => Some(8),
        // DXR ignores A16, and packed positions may use a six-byte stride.
        DXGI_FORMAT_R16G16B16A16_FLOAT | DXGI_FORMAT_R16G16B16A16_SNORM |
        DXGI_FORMAT_R16G16B16A16_UNORM => Some(6),
        DXGI_FORMAT_R16G16_FLOAT | DXGI_FORMAT_R16G16_SNORM |
        DXGI_FORMAT_R16G16_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        _ => None,
    }
}

fn vertex_span(triangle: &D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC) -> Result<u64, String> {
    let element = vertex_bytes(triangle.VertexFormat).ok_or("unsupported DXR vertex format")?;
    u64::from(triangle.VertexCount).checked_sub(1)
        .and_then(|n| n.checked_mul(triangle.VertexBuffer.StrideInBytes))
        .and_then(|n| n.checked_add(element)).ok_or_else(|| "invalid DXR vertex span".into())
}

unsafe fn geometry(
    prepared: &mut Prepared, dev: &ID3D12Device5, inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    source: &dyn Fn(u64, u64) -> Option<(ID3D12Resource, u64)>,
) -> Result<Vec<D3D12_RAYTRACING_GEOMETRY_DESC>, String> {
    let mut geometries = match inputs.DescsLayout {
        D3D12_ELEMENTS_LAYOUT_ARRAY => read_items(inputs.Anonymous.pGeometryDescs as usize, inputs.NumDescs as usize)?,
        D3D12_ELEMENTS_LAYOUT_ARRAY_OF_POINTERS => {
            let pointers: Vec<usize> = read_items(inputs.Anonymous.ppGeometryDescs as usize, inputs.NumDescs as usize)?;
            pointers.into_iter().map(|p| read_items::<D3D12_RAYTRACING_GEOMETRY_DESC>(p, 1).map(|mut v| v.remove(0))).collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err("unsupported geometry descriptor layout".into()),
    };
    for geometry in &mut geometries {
        if geometry.Type != D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES {
            return Err("shadow DXR currently accepts triangle geometry".into());
        }
        let triangle = &mut geometry.Anonymous.Triangles;
        let bytes = vertex_span(triangle)?;
        triangle.VertexBuffer.StartAddress = prepared.input(dev, triangle.VertexBuffer.StartAddress, bytes, source)?;
        if triangle.IndexCount != 0 {
            let width = match triangle.IndexFormat {
                DXGI_FORMAT_R16_UINT => 2,
                DXGI_FORMAT_R32_UINT => 4,
                _ => return Err("unsupported DXR index format".into()),
            };
            triangle.IndexBuffer = prepared.input(dev, triangle.IndexBuffer, u64::from(triangle.IndexCount) * width, source)?;
        }
        if triangle.Transform3x4 != 0 {
            triangle.Transform3x4 = prepared.input(dev, triangle.Transform3x4, 48, source)?;
        }
    }
    Ok(geometries)
}

unsafe fn instances(
    prepared: &mut Prepared, dev: &ID3D12Device5, inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    structures: &HashMap<u64, HostAs>, owner: usize,
) -> Result<(u64, bool), String> {
    if inputs.DescsLayout != D3D12_ELEMENTS_LAYOUT_ARRAY {
        return Err("shadow TLAS requires a contiguous instance array".into());
    }
    let address = inputs.Anonymous.InstanceDescs as usize;
    let mut instances: Vec<[u8; 64]> = read_items(address, inputs.NumDescs as usize).or_else(|error| {
        let cpu = crate::hle::kernelx::mirrors::source(address).ok_or(error)?;
        read_items(cpu, inputs.NumDescs as usize)
    })?;
    let mut opaque = true;
    for instance in &mut instances {
        let console = u64::from_le_bytes(instance[56..64].try_into().unwrap());
        let native = structures.get(&console)
            .filter(|s| s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL)
            .ok_or("TLAS references a BLAS without a shadow build")?;
        if native.owner != owner {
            return Err("TLAS references a BLAS from another recording".into());
        }
        opaque &= native.opaque && native.owner == owner && instance[51] == 0xff && instance[55] == 0;
        // First consumer is verified for translation-only instances, without winding overrides.
        for row in 0..3 {
            for column in 0..4 {
                let at = (row * 4 + column) * 4;
                let value = f32::from_le_bytes(instance[at..at + 4].try_into().unwrap());
                opaque &= value.is_finite() && (column == 3 || value == if row == column { 1. } else { 0. });
            }
        }
        instance[56..64].copy_from_slice(&native.resource.GetGPUVirtualAddress().to_le_bytes());
    }
    let bytes = size_of::<[u8; 64]>() * instances.len();
    let upload = prepared.buffer(dev, bytes as u64, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
    let mut mapped = std::ptr::null_mut();
    upload.Map(0, None, Some(&mut mapped)).map_err(|e| e.to_string())?;
    std::ptr::copy_nonoverlapping(instances.as_ptr().cast::<u8>(), mapped.cast(), bytes);
    upload.Unmap(0, None);
    Ok((upload.GetGPUVirtualAddress(), opaque))
}

unsafe fn transition(resource: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 { Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
            pResource: std::mem::transmute_copy(resource), Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            StateBefore: before, StateAfter: after,
        }) },
        ..Default::default()
    }
}

unsafe fn uav_barrier(resource: Option<&ID3D12Resource>) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Anonymous: D3D12_RESOURCE_BARRIER_0 { UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
            pResource: ManuallyDrop::new(resource.map(|resource| std::mem::transmute_copy(resource))),
        }) },
        ..Default::default()
    }
}

unsafe fn barriers(list: &ID3D12GraphicsCommandList4, barriers: &[D3D12_RESOURCE_BARRIER]) {
    let base = &*(native_vtable(list) as *const _ as *const ID3D12GraphicsCommandList_Vtbl);
    (base.ResourceBarrier)(list.as_raw(), barriers.len() as u32, barriers.as_ptr());
}

unsafe fn record(list: *mut c_void, desc: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC) -> Result<(), String> {
    let list = recording_list(list)?;
    let dev = device(&list)?;
    let mut states = STATES.lock().unwrap();
    let state = states.get_or_insert_with(HashMap::new).entry(dev.as_raw() as usize).or_default();
    let source = |address: u64, bytes: u64| {
        let (raw, offset) = real::uma_window_buffer(address as usize, usize::try_from(bytes).ok()?)?;
        let raw = raw as *mut c_void;
        Some((ID3D12Resource::from_raw_borrowed(&raw)?.clone(), offset))
    };
    record_on(state, &dev, &list, desc, &source)?;
    state.recorded += 1;
    if state.recorded <= 8 || state.recorded.is_power_of_two() {
        crate::xlog_info!("Host DXR: {} shadow builds recorded, {} KiB retained", state.recorded, state.bytes >> 10);
    }
    Ok(())
}

unsafe fn record_on(
    state: &mut State, dev: &ID3D12Device5, list: &ID3D12GraphicsCommandList4,
    desc: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC,
    source: &dyn Fn(u64, u64) -> Option<(ID3D12Resource, u64)>,
) -> Result<HostAs, String> {
    let previous = state.structures.remove(&desc.DestAccelerationStructureData);
    if desc.Inputs.Type == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL
        || previous.is_some_and(|s| s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL) {
        state.structures.retain(|_, value| value.kind != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL);
    }
    if desc.DestAccelerationStructureData == 0 || desc.Inputs.NumDescs == 0 {
        return Err("empty acceleration structure".into());
    }
    let mut prepared = Prepared::new(LIMIT - state.bytes);
    let mut inputs = desc.Inputs;
    inputs.Flags &= !D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE;
    let geometries;
    let opaque;
    let guest_bytes;
    match inputs.Type {
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL => {
            geometries = geometry(&mut prepared, dev, &inputs, source)?;
            opaque = geometries.iter().all(|g| g.Flags.0 & D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE.0 != 0 && g.Anonymous.Triangles.Transform3x4 == 0);
            let triangles = geometries.iter().map(|g| {
                let t = g.Anonymous.Triangles;
                u64::from(if t.IndexCount == 0 { t.VertexCount } else { t.IndexCount }) / 3
            }).sum();
            guest_bytes = crate::gpu::bvh::prebuild_size(triangles, false);
            inputs.DescsLayout = D3D12_ELEMENTS_LAYOUT_ARRAY;
            inputs.Anonymous.pGeometryDescs = geometries.as_ptr();
        }
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL => {
            let (address, compatible) = instances(&mut prepared, dev, &inputs, &state.structures, list.as_raw() as usize)?;
            inputs.Anonymous.InstanceDescs = address;
            opaque = compatible;
            guest_bytes = crate::gpu::bvh::prebuild_size(inputs.NumDescs as u64, true);
        }
        _ => return Err("unsupported acceleration structure type".into()),
    }
    let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
    (native_vtable(dev).GetRaytracingAccelerationStructurePrebuildInfo)(dev.as_raw(), &inputs, &mut info);
    let result = prepared.buffer(dev, info.ResultDataMaxSizeInBytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE)?;
    let scratch = prepared.buffer(dev, info.ScratchDataSizeInBytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
    let copies = std::mem::take(&mut prepared.copies);
    // Retain every referenced buffer before recording the first command. Failed preparation
    // has emitted no commands and can release its allocations immediately.
    state.retain(prepared);
    barriers(list, &[uav_barrier(None)]);
    let base = &*(native_vtable(list) as *const _ as *const ID3D12GraphicsCommandList_Vtbl);
    for (original, offset, target, bytes) in copies {
        barriers(list, &[transition(&original, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        (base.CopyBufferRegion)(list.as_raw(), target.as_raw(), 0, original.as_raw(), offset, bytes);
        barriers(list, &[
            transition(&original, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON),
            transition(&target, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
        ]);
    }
    let native = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
        DestAccelerationStructureData: result.GetGPUVirtualAddress(), Inputs: inputs,
        ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(), ..Default::default()
    };
    (native_vtable(list).BuildRaytracingAccelerationStructure)(list.as_raw(), &native, 0, std::ptr::null());
    barriers(list, &[uav_barrier(Some(&result))]);
    // Only the UMA allocation/remapping hooks currently invalidate native query bindings.
    let opaque = opaque && managed_span(desc.DestAccelerationStructureData, guest_bytes);
    state.serial += 1;
    let built = HostAs { resource: result, bytes: info.ResultDataMaxSizeInBytes, guest_bytes, kind: inputs.Type, opaque, serial: state.serial, owner: list.as_raw() as usize };
    state.structures.insert(desc.DestAccelerationStructureData, built.clone());
    Ok(built)
}

unsafe fn record_copy_on(
    state: &mut State, dev: &ID3D12Device5, list: &ID3D12GraphicsCommandList4,
    dest: u64, source: u64, mode: i32,
) -> Result<HostAs, String> {
    let old = state.structures.get(&source).cloned();
    let previous = state.structures.remove(&dest);
    if previous.is_some_and(|s| s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL)
        || old.as_ref().is_some_and(|s| s.kind == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL) {
        state.structures.retain(|_, value| value.kind != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL);
    }
    if dest == 0 || (mode != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0
        && mode != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT.0) {
        return Err("unsupported shadow copy mode or destination".into());
    }
    let old = old.ok_or("shadow copy source is unavailable")?;
    if old.owner != list.as_raw() as usize {
        return Err("shadow copy source belongs to another recording".into());
    }
    let mut prepared = Prepared::new(LIMIT - state.bytes);
    let result = prepared.buffer(dev, old.bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE)?;
    state.retain(prepared);
    (native_vtable(list).CopyRaytracingAccelerationStructure)(
        list.as_raw(), result.GetGPUVirtualAddress(), old.resource.GetGPUVirtualAddress(),
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE,
    );
    barriers(list, &[uav_barrier(Some(&result))]);
    state.serial += 1;
    let opaque = old.opaque && old.owner == list.as_raw() as usize && managed_span(dest, old.guest_bytes);
    let copied = HostAs { resource: result, opaque, serial: state.serial, owner: list.as_raw() as usize, ..old };
    state.structures.insert(dest, copied.clone());
    Ok(copied)
}

#[cfg(test)]
mod tests;
