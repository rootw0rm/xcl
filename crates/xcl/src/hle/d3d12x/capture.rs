//! One draw or dispatch written to disk. `XCL_CAPTURE_DRAW=<container fnv64, hex>[:<nth call>]`:
//! the nth draw / dispatch (default the first) whose VS, PS or CS is that container is written
//! to `<data>\captures\<hash>_<n>_<time>\` (`manifest`) with what it takes to run it alone on
//! a fresh device: the console shader containers, the pipeline description, the root arguments
//! (the user SGPRs `io` as `bind_abi` made them), the fixed-function state, the console
//! descriptor words of the table slots, the bytes of the unified memory they name, and the
//! textures the slots and the targets name with the PC views made of them.
//!
//! Memory is read on the CPU when the call's list is submitted (titles fill buffers after they
//! record the call). The contents of the textures are copied on
//! the GPU where the call stands and read back after the submit (`textures`).
//!
//! The matching calls themselves are NOT run unless `XCL_CAPTURE_RUN=1`: the capture is for
//! calls that hang the GPU, and what is written after one of those would be lost with the
//! device. `XCL_CAPTURE_SLOTS` (default 512): slots looked at per descriptor table.
//! `XCL_CAPTURE_MAX_MB` (default 2048): most bytes written; what does not fit is listed in the
//! manifest's `left_out`. `XCL_CAPTURE_ROOT_BYTES` (default 65536): bytes taken at each root
//! buffer address. `XCL_CAPTURE_EXTRA=addr+len,…` (hex): more memory ranges.
//!
//! Which hash to give: with the variable set – `XCL_CAPTURE_DRAW=list` for that alone – every
//! pipeline made from translated console shaders is logged with its shaders' hashes
//! (`CAPTURE: pipeline …`, the first 200), and the first 20 calls of the chosen shader with
//! their arguments.
//!
//! Not captured: multisampled textures, the arguments of an ExecuteIndirect, memory reached
//! through a pointer kept in a buffer (`XCL_CAPTURE_EXTRA`), console geometry and mesh stages.
//! Off unless the variable is set: nothing is noted and no call looks further than `on()`.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::console_list::{Layout, Param};
use manifest::*;

pub use manifest::Call;

pub mod manifest;
mod textures;

unsafe fn pod_bytes<T>(v: &T) -> &[u8] {
    std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>())
}

// ---------------------------------------------------------------------------------------
// the switch
// ---------------------------------------------------------------------------------------

fn target() -> Option<(u64, u32)> {
    static T: OnceLock<Option<(u64, u32)>> = OnceLock::new();
    *T.get_or_init(|| {
        let v = std::env::var("XCL_CAPTURE_DRAW").ok()?;
        let (h, n) = v.split_once(':').unwrap_or((&v, "1"));
        Some((u64::from_str_radix(h.trim().trim_start_matches("0x"), 16).ok()?, n.trim().parse().ok().filter(|&n| n > 0)?))
    })
}

pub fn on() -> bool {
    target().is_some()
}

/// A pipeline from translated console shaders was made (every kind: `dred::label`'s callers):
/// with `XCL_CAPTURE_DRAW` set, to a shader or to `list`, it is logged with its shaders'
/// hashes – the first 200, and any with the chosen shader.
pub(super) fn list_pipeline(pso: usize, shaders: &[(&str, Option<&[u8]>)]) {
    static LISTING: OnceLock<bool> = OnceLock::new();
    static LISTED: AtomicU32 = AtomicU32::new(0);
    if !*LISTING.get_or_init(|| std::env::var_os("XCL_CAPTURE_DRAW").is_some()) {
        return;
    }
    let hashes: Vec<(&str, u64)> = shaders.iter().filter_map(|(stage, b)| Some((*stage, crate::gpu::shader::fnv64((*b)?)))).collect();
    let chosen = target().is_some_and(|(h, _)| hashes.iter().any(|x| x.1 == h));
    if !chosen && LISTED.fetch_add(1, Ordering::Relaxed) >= 200 {
        return;
    }
    let text: Vec<String> = hashes.iter().map(|(stage, h)| format!("{stage} {h:016x}")).collect();
    crate::xlog_info!("CAPTURE: pipeline {pso:#x}: {}{}", text.join(" "), if chosen { " – has the XCL_CAPTURE_DRAW shader" } else { "" });
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn run_the_calls() -> bool {
    static R: OnceLock<bool> = OnceLock::new();
    *R.get_or_init(|| std::env::var("XCL_CAPTURE_RUN").is_ok_and(|v| v.trim() == "1"))
}

// ---------------------------------------------------------------------------------------
// what is noted while the title runs
// ---------------------------------------------------------------------------------------

struct PsoInfo {
    /// (stage, blob)
    blobs: Vec<(&'static str, Vec<u8>)>,
    pso: Option<Pso>,
    split: bool,
    windows: u32,
}

static PSOS: Mutex<Option<HashMap<usize, Arc<PsoInfo>>>> = Mutex::new(None);

/// A graphics pipeline from translated console shaders was made: `copy` is the description the
/// PC got (its shaders are DXIL), `d` the title's (the console blobs, the input layout).
pub(super) unsafe fn note_graphics_pso(pso: usize, d: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, copy: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, rect_list_gs: bool, split: bool) {
    let Some((hash, _)) = target() else { return };
    let blob = |b: &D3D12_SHADER_BYTECODE| (!b.pShaderBytecode.is_null() && b.BytecodeLength > 0).then(|| std::slice::from_raw_parts(b.pShaderBytecode as *const u8, b.BytecodeLength).to_vec());
    let blobs: Vec<(&'static str, Vec<u8>)> = [("vs", &d.VS), ("ps", &d.PS), ("gs", &d.GS)].into_iter().filter_map(|(s, b)| Some((s, blob(b)?))).collect();
    if !blobs.iter().any(|(_, b)| crate::gpu::shader::fnv64(b) == hash) {
        return;
    }
    let il = &d.InputLayout;
    let input = if il.NumElements > 0 && !il.pInputElementDescs.is_null() {
        std::slice::from_raw_parts(il.pInputElementDescs, il.NumElements as usize)
            .iter()
            .map(|e| InputElement {
                name: e.SemanticName.to_string().unwrap_or_default(),
                index: e.SemanticIndex,
                format: e.Format.0,
                slot: e.InputSlot,
                offset: e.AlignedByteOffset,
                class: e.InputSlotClass.0,
                step: e.InstanceDataStepRate,
            })
            .collect()
    } else {
        Vec::new()
    };
    let info = PsoInfo { blobs, pso: Some(Pso { desc: hex(pod_bytes(copy)), input, rect_list_gs }), split, windows: crate::gpu::recomp::pso::mem_windows() };
    PSOS.lock().unwrap().get_or_insert_with(HashMap::new).insert(pso, Arc::new(info));
    crate::xlog_info!("CAPTURE: the draws of pipeline {pso:#x} count as calls of {hash:016x}");
}

/// A compute pipeline from the translated console program `cs` was made.
pub(super) fn note_compute_pso(pso: usize, cs: &[u8], split: bool) {
    let Some((hash, _)) = target() else { return };
    if crate::gpu::shader::fnv64(cs) != hash {
        return;
    }
    let info = PsoInfo { blobs: vec![("cs", cs.to_vec())], pso: None, split, windows: crate::gpu::recomp::pso::mem_windows() };
    PSOS.lock().unwrap().get_or_insert_with(HashMap::new).insert(pso, Arc::new(info));
    crate::xlog_info!("CAPTURE: the dispatches of pipeline {pso:#x} count as calls of {hash:016x}");
}

pub(super) fn forget_pso(pso: usize) {
    if on() {
        if let Some(m) = PSOS.lock().unwrap().as_mut() {
            m.remove(&pso);
        }
    }
}

/// counts noted views and released resources: a view made before its resource's address was
/// last released names a dead (or another) object
static SEQ: AtomicU64 = AtomicU64::new(1);
static RELEASED: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);

#[derive(Clone)]
struct ViewRec {
    res: usize,
    desc: Option<Vec<u8>>,
    seq: u64,
}

impl ViewRec {
    fn live(&self) -> bool {
        self.res != 0 && RELEASED.lock().unwrap().as_ref().and_then(|m| m.get(&self.res)).is_none_or(|&gone| gone < self.seq)
    }
}

/// index in the shader-visible heap → the view made there
static VIEWS: Mutex<Option<HashMap<u32, ViewRec>>> = Mutex::new(None);
/// RTV / DSV handle → the view made there
static TARGET_VIEWS: Mutex<Option<HashMap<usize, ViewRec>>> = Mutex::new(None);

unsafe fn rec<T>(res: usize, desc: *const T) -> ViewRec {
    ViewRec { res, desc: desc.as_ref().map(|d| pod_bytes(d).to_vec()), seq: SEQ.fetch_add(1, Ordering::Relaxed) }
}

/// The PC view made at heap index `index` (`uav`: the index is the UAV's, a slot's index + 1).
pub(super) unsafe fn note_view<T>(index: u32, res: usize, desc: *const T) {
    if on() {
        VIEWS.lock().unwrap().get_or_insert_with(HashMap::new).insert(index, rec(res, desc));
    }
}

/// The two PC descriptors of a view slot were copied to another slot.
pub(super) fn copy_views(src: u32, dst: u32) {
    if !on() || src == dst {
        return;
    }
    let mut g = VIEWS.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    for k in 0..2 {
        match m.get(&(src + k)).cloned() {
            Some(v) => m.insert(dst + k, v),
            None => m.remove(&(dst + k)),
        };
    }
}

/// sampler heap index → the sampler made there
static SAMPLERS: Mutex<Option<HashMap<u32, Vec<u8>>>> = Mutex::new(None);

pub(super) unsafe fn note_sampler(index: u32, desc: *const D3D12_SAMPLER_DESC) {
    if on() {
        if let Some(d) = desc.as_ref() {
            SAMPLERS.lock().unwrap().get_or_insert_with(HashMap::new).insert(index, pod_bytes(d).to_vec());
        }
    }
}

pub(super) fn copy_sampler(src: u32, dst: u32) {
    if on() && src != dst {
        let mut g = SAMPLERS.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        if let Some(d) = m.get(&src).cloned() {
            m.insert(dst, d);
        }
    }
}

/// An RTV or DSV was made at `handle`.
pub(super) unsafe fn note_target_view<T>(handle: usize, res: usize, desc: *const T) {
    if on() {
        TARGET_VIEWS.lock().unwrap().get_or_insert_with(HashMap::new).insert(handle, rec(res, desc));
    }
}

/// The resource at `res` is gone.
pub(super) fn forget_resource(res: usize) {
    if on() {
        RELEASED.lock().unwrap().get_or_insert_with(HashMap::new).insert(res, SEQ.fetch_add(1, Ordering::Relaxed));
        if let Some(m) = STATES.lock().unwrap().as_mut() {
            m.global.remove(&res);
        }
    }
}

/// The states transitions left a resource in: one for all of it, then those of the
/// subresources transitioned on their own since.
#[derive(Default, Clone, Debug)]
struct ResState {
    all: Option<u32>,
    subs: HashMap<u32, u32>,
}

impl ResState {
    fn apply(&mut self, sub: u32, after: u32) {
        if sub == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES {
            *self = ResState { all: Some(after), subs: HashMap::new() };
        } else {
            self.subs.insert(sub, after);
        }
    }

    /// The one state all `n` subresources are in; an error when some are in another or one
    /// was transitioned on its own while the rest are not known.
    fn uniform(&self, n: u32) -> Result<u32, String> {
        let mut seen = None;
        for k in 0..n.max(1) {
            let Some(s) = self.subs.get(&k).copied().or(self.all) else {
                return Err(format!("subresource {k} not transitioned while {} others were", self.subs.len()));
            };
            match seen {
                Some(x) if x != s => return Err(format!("subresources in different states ({x:#x}, {s:#x})")),
                _ => seen = Some(s),
            }
        }
        seen.ok_or_else(|| "no state".into())
    }
}

#[derive(Default)]
struct States {
    /// list → resource → the states its transitions on that list's recording left
    lists: HashMap<usize, HashMap<usize, ResState>>,
    /// resource → the states of the transitions recorded on any list
    global: HashMap<usize, ResState>,
}

static STATES: Mutex<Option<States>> = Mutex::new(None);

/// A transition barrier the PC list got (`sub`: the subresource, or all of them).
pub(super) fn note_transition(list: usize, res: usize, sub: u32, after: u32) {
    let mut g = STATES.lock().unwrap();
    let s = g.get_or_insert_with(States::default);
    s.lists.entry(list).or_default().entry(res).or_default().apply(sub, after);
    s.global.entry(res).or_default().apply(sub, after);
}

/// How `Texture::state_source` begins when a texture's subresources are not in one known state:
/// such a texture is not copied (one transition of all its subresources would be wrong).
pub(super) const PARTIAL_STATE: &str = "partial:";

/// The list starts a new recording.
pub(super) fn list_reset(list: usize) {
    if on() {
        if let Some(s) = STATES.lock().unwrap().as_mut() {
            s.lists.remove(&list);
        }
    }
}

thread_local! {
    /// the call being bound on this thread
    static CALL: Cell<Option<Call>> = const { Cell::new(None) };
}

pub(super) fn set_call(c: Call) {
    if on() {
        CALL.with(|x| x.set(Some(c)));
    }
}

/// `set_call` unless the caller of the caller has set it (with more of the arguments).
pub(super) fn set_call_if_unset(c: Call) {
    if on() {
        CALL.with(|x| {
            if x.get().is_none() {
                x.set(Some(c));
            }
        });
    }
}

// ---------------------------------------------------------------------------------------
// the capture
// ---------------------------------------------------------------------------------------

struct Pending {
    list: usize,
    dir: PathBuf,
    m: Manifest,
    /// (address, bytes, what)
    ranges: Vec<(usize, usize, String)>,
    snaps: Vec<textures::Snap>,
    dev: Option<ID3D12Device>,
    budget: u64,
    memory_done: bool,
}

// D3D12 objects are free-threaded
unsafe impl Send for Pending {}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static CALLS: AtomicU32 = AtomicU32::new(0);

/// A draw / dispatch with the translated pipeline `pso` is bound on `this` (`bind::bind_abi`,
/// its root arguments as given there). True: the call is not to be run.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn on_call(this: *mut c_void, pso: usize, gfx: bool, layout: Option<&Layout>, args: &[u8], inline: usize, io: &[u32], table_at: usize) -> bool {
    let call = CALL.with(|c| c.take());
    let Some((hash, nth)) = target() else { return false };
    let Some(info) = PSOS.lock().unwrap().as_ref().and_then(|m| m.get(&pso).cloned()) else { return false };
    let n = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 20 || n == nth {
        crate::xlog_info!("CAPTURE: call #{n} of {hash:016x}: {call:?}{}", if run_the_calls() { "" } else { " (not run)" });
    }
    if n == nth {
        match gather(this, &info, hash, n, call, gfx, layout, args, inline, io, table_at) {
            Ok(p) => {
                crate::xlog_info!(
                    "CAPTURE: call #{n} noted ({} memory ranges, {} textures, {} of them copied on the list) – written to {} when its list is submitted",
                    p.ranges.len(),
                    p.m.textures.len(),
                    p.snaps.len(),
                    p.dir.display()
                );
                PENDING.lock().unwrap().push(p);
            }
            Err(e) => crate::xlog_warn!("CAPTURE: call #{n} not captured: {e}"),
        }
    }
    !run_the_calls()
}

fn word(args: &[u8], at: usize) -> u32 {
    args.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

pub(super) unsafe fn resource_desc(res: usize) -> Option<D3D12_RESOURCE_DESC> {
    let p = res as *mut c_void;
    ID3D12Resource::from_raw_borrowed(&p).map(|r| r.GetDesc())
}

/// The subresources of the texture `res` (described by `d`), planes included, as transition
/// barriers number them.
unsafe fn subresource_count(res: usize, d: &D3D12_RESOURCE_DESC) -> u32 {
    let p = res as *mut c_void;
    let mut info = D3D12_FEATURE_DATA_FORMAT_INFO { Format: d.Format, PlaneCount: 1 };
    let mut dev: Option<ID3D12Device> = None;
    if let Some(r) = ID3D12Resource::from_raw_borrowed(&p) {
        if r.GetDevice(&mut dev).is_ok() {
            if let Some(dev) = &dev {
                let _ = dev.CheckFeatureSupport(D3D12_FEATURE_FORMAT_INFO, &mut info as *mut _ as *mut c_void, std::mem::size_of_val(&info) as u32);
            }
        }
    }
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { d.DepthOrArraySize as u32 };
    d.MipLevels.max(1) as u32 * layers.max(1) * info.PlaneCount.max(1) as u32
}

/// The unified-memory address of a PC GPU address in one of the window buffers.
fn uma_address_of(u: &crate::gpu::uma::Uma, va: u64) -> Option<usize> {
    let vas = super::real::uma_window_vas();
    (0..u.shader_windows()).find_map(|k| {
        let w = vas.get(k).copied().filter(|&w| w != 0)?;
        let (base, size) = u.window(k);
        (va >= w && va < w + size as u64).then(|| base + (va - w) as usize)
    })
}

/// (state, why) the texture `res` of `n` subresources is taken to be in where the call stands
/// on `list`: one state for all of them, else `PARTIAL_STATE` and why.
fn state_of(list: usize, res: usize, n: u32, hint: Option<(u32, &str)>) -> (u32, String) {
    let g = STATES.lock().unwrap();
    let known = |r: &ResState, from: &str| match r.uniform(n) {
        Ok(s) => (s, from.to_string()),
        Err(e) => (0, format!("{PARTIAL_STATE} {e} ({from})")),
    };
    if let Some(r) = g.as_ref().and_then(|s| s.lists.get(&list)?.get(&res)) {
        return known(r, "last transition on this list");
    }
    if let Some((s, why)) = hint {
        return (s, why.into());
    }
    if let Some(r) = g.as_ref().and_then(|s| s.global.get(&res)) {
        return known(r, "last transition recorded on any list");
    }
    ((D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0) as u32, "assumed (no transition seen)".into())
}

struct Gather {
    list: usize,
    m: Manifest,
    by_res: HashMap<usize, usize>,
}

impl Gather {
    unsafe fn texture(&mut self, res: usize, used: String, hint: Option<(u32, &str)>) -> Option<usize> {
        if let Some(&id) = self.by_res.get(&res) {
            self.m.textures[id].uses.push(used);
            return Some(id);
        }
        let d = resource_desc(res)?;
        if d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            return None;
        }
        let id = self.m.textures.len();
        let (state, state_source) = state_of(self.list, res, subresource_count(res, &d), hint);
        self.m.textures.push(Texture {
            id,
            resource: format!("{res:#x}"),
            dimension: d.Dimension.0,
            width: d.Width,
            height: d.Height,
            depth_or_array: d.DepthOrArraySize,
            mips: d.MipLevels,
            format: d.Format.0,
            samples: d.SampleDesc.Count,
            flags: d.Flags.0,
            title_address: super::real::title_address(res).or_else(|| super::title_textures::address(res)).map(|a| a as u64),
            state,
            state_source,
            uses: vec![used],
            ..Default::default()
        });
        self.by_res.insert(res, id);
        Some(id)
    }

    unsafe fn target(&mut self, handle: usize, used: String, state: u32) -> Target {
        let v = TARGET_VIEWS.lock().unwrap().as_ref().and_then(|m| m.get(&handle).cloned()).filter(ViewRec::live);
        let texture = match &v {
            Some(v) => self.texture(v.res, used, Some((state, "bound as a target"))),
            None => None,
        };
        Target { handle: handle as u64, texture, desc: v.and_then(|v| v.desc).map(|d| hex(&d)) }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn gather(this: *mut c_void, info: &PsoInfo, hash: u64, n: u32, call: Option<Call>, gfx: bool, layout: Option<&Layout>, args: &[u8], inline: usize, io: &[u32], table_at: usize) -> Result<Pending, String> {
    let u = crate::gpu::uma::get().ok_or("no unified memory")?;
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let dir = crate::state::data_dir().join("captures").join(format!("{hash:016x}_{n}_{time}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut g = Gather { list: this as usize, m: Manifest { version: manifest::VERSION, hash: format!("{hash:016x}"), call_number: n, call, graphics: gfx, ..Default::default() }, by_res: HashMap::new() };
    for (stage, blob) in &info.blobs {
        let file = format!("{stage}.bin");
        std::fs::write(dir.join(&file), blob).map_err(|e| format!("{file}: {e}"))?;
        g.m.shaders.push(Shader { stage: stage.to_string(), hash: format!("{:016x}", crate::gpu::shader::fnv64(blob)), file });
    }
    g.m.mem_windows = info.windows;
    g.m.split_wave64 = info.split;
    g.m.pso = info.pso.clone();
    g.m.uma = Uma { base: u.base() as u64, windows: (0..u.shader_windows()).map(|k| (u.window(k).0 as u64, u.window(k).1 as u64)).collect() };
    g.m.root = Root { args: hex(args), inline: inline as u32, io: io.to_vec(), table_at: table_at as u64, ..Default::default() };
    let mut ranges: Vec<(usize, usize, String)> = vec![(table_at, args.len().saturating_sub(inline).max(16), "argument table".into())];
    // the targets first: what a slot names too is then in its target state
    if gfx {
        if let Some(s) = super::list_x::capture_state(this) {
            g.m.state = State {
                topology: s.topology,
                viewports: s.viewports.iter().map(|v| [v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth]).collect(),
                scissors: s.scissors.iter().map(|r| [r.left, r.top, r.right, r.bottom]).collect(),
                blend_factor: s.blend,
                stencil_ref: s.stencil,
            };
            for (k, &h) in s.rtvs.iter().enumerate() {
                let t = g.target(h, format!("render target {k}"), D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32);
                g.m.targets.push(t);
            }
            if let Some(h) = s.dsv {
                g.m.depth = Some(g.target(h, "depth buffer".into(), D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32));
            }
            for (slot, v) in &s.vertex_buffers {
                let address = uma_address_of(u, v.BufferLocation).unwrap_or(0);
                if address != 0 {
                    ranges.push((address, v.SizeInBytes as usize, format!("vertex buffer {slot}")));
                } else {
                    g.m.left_out.push(format!("vertex buffer {slot} at PC address {:#x}: not in a memory window", v.BufferLocation));
                }
                g.m.vertex_buffers.push(VertexBuffer { slot: *slot, address: address as u64, size: v.SizeInBytes, stride: v.StrideInBytes });
            }
            if let Some(v) = s.index_buffer {
                let address = uma_address_of(u, v.BufferLocation).unwrap_or(0);
                if address != 0 {
                    ranges.push((address, v.SizeInBytes as usize, "index buffer".into()));
                } else {
                    g.m.left_out.push(format!("index buffer at PC address {:#x}: not in a memory window", v.BufferLocation));
                }
                g.m.index_buffer = Some(IndexBuffer { address: address as u64, size: v.SizeInBytes, format: v.Format.0 });
            }
        }
    }
    // root arguments: tables (their slots, and what the slots name) and root buffers
    let slots = env_number("XCL_CAPTURE_SLOTS", 512) as usize;
    let root_bytes = env_number("XCL_CAPTURE_ROOT_BYTES", 64 << 10) as usize;
    let mut views: HashMap<u32, View> = HashMap::new();
    for (i, p) in layout.map_or(&[][..], |l| &l.params[..]).iter().enumerate() {
        match *p {
            Param::Constants { at, count } => g.m.root.params.push(RootParam { kind: "constants".into(), at, count, value: 0 }),
            Param::Descriptor { at } => {
                let va = word(args, at as usize) as u64 | (word(args, at as usize + 4) as u64) << 32;
                g.m.root.params.push(RootParam { kind: "descriptor".into(), at, count: 2, value: va });
                if va != 0 {
                    ranges.push((va as usize, root_bytes, format!("p{i} root buffer")));
                }
            }
            Param::Table { at } => {
                let t = word(args, at as usize);
                g.m.root.params.push(RootParam { kind: "table".into(), at, count: 1, value: t as u64 });
                if t == 0 {
                    continue;
                }
                let base = u.base() + t as usize;
                let mut seen = 0;
                for k in 0..slots {
                    let at = base + 32 * k;
                    if !crate::gpu::shader::readable(at, 32) {
                        break;
                    }
                    seen = k + 1;
                    let w = (at as *const [u32; 8]).read_unaligned();
                    if w == [0; 8] {
                        continue;
                    }
                    let kind = if w[3] >> 28 >= 8 && w[0] < 1 << 22 {
                        // an image: the SRV at its index, the UAV beside it
                        for uav in [false, true] {
                            let index = w[0] + uav as u32;
                            if views.contains_key(&index) {
                                continue;
                            }
                            let Some(v) = VIEWS.lock().unwrap().as_ref().and_then(|m| m.get(&index).cloned()).filter(ViewRec::live) else { continue };
                            let hint = uav.then_some((D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32, "has a UAV in a slot (assumed)"));
                            // an SRV-only texture keeps the default (a shader-resource state)
                            let hint = hint.filter(|_| resource_desc(v.res).is_some_and(|d| d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0));
                            if let Some(texture) = g.texture(v.res, format!("p{i} +{:#x} {}", 32 * k, if uav { "UAV" } else { "SRV" }), hint) {
                                views.insert(index, View { index, uav, texture, desc: v.desc.map(|d| hex(&d)) });
                            }
                        }
                        "image"
                    } else if w[3] >> 30 == 0 && w[2] != 0 {
                        let b = crate::gpu::desc::buffer(&[w[0], w[1], w[2], w[3]]);
                        let bytes = (if b.stride == 0 { b.num_records as u64 } else { b.stride as u64 * b.num_records as u64 }).min(256 << 20) as usize;
                        if b.address != 0 && u.contains(b.address as usize) {
                            ranges.push((b.address as usize, bytes, format!("p{i} +{:#x} buffer", 32 * k)));
                            "buffer"
                        } else {
                            "other"
                        }
                    } else {
                        "other"
                    };
                    g.m.slots.push(Slot { param: i as u32, offset: 32 * k as u32, words: w, kind: kind.into() });
                }
                ranges.push((base, 32 * seen.max(1), format!("p{i} descriptor table")));
            }
        }
    }
    if let Ok(extra) = std::env::var("XCL_CAPTURE_EXTRA") {
        for (a, l) in extra.split(',').filter_map(|r| r.split_once('+')) {
            let hx = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
            if let (Some(a), Some(l)) = (hx(a), hx(l)) {
                ranges.push((a, l, "XCL_CAPTURE_EXTRA".into()));
            }
        }
    }
    g.m.samplers = SAMPLERS.lock().unwrap().as_ref().map_or(Vec::new(), |m| m.iter().map(|(&index, d)| Sampler { index, desc: hex(d) }).collect());
    g.m.samplers.sort_by_key(|s| s.index);
    g.m.views = views.into_values().collect();
    g.m.views.sort_by_key(|v| v.index);
    let mut budget = env_number("XCL_CAPTURE_MAX_MB", 2048) << 20;
    // the textures as they are where the call stands: copies recorded on the list
    let (snaps, dev) = textures::copy_all(this, &mut g.m, &g.by_res, &mut budget);
    Ok(Pending { list: this as usize, dir, m: g.m, ranges, snaps, dev, budget, memory_done: false })
}

/// Before `lists` are submitted: the memory of the captures recorded on them, as the CPU has it now.
pub(super) unsafe fn before_execute(lists: &[*mut c_void]) {
    if !on() {
        return;
    }
    for p in PENDING.lock().unwrap().iter_mut().filter(|p| !p.memory_done && lists.contains(&(p.list as *mut c_void))) {
        p.memory_done = true;
        if let Err(e) = write_memory(p) {
            crate::xlog_warn!("CAPTURE: memory not written: {e}");
            p.m.left_out.push(format!("memory: {e}"));
        }
    }
}

unsafe fn write_memory(p: &mut Pending) -> Result<(), String> {
    use std::io::Write;
    let u = crate::gpu::uma::get().ok_or("no unified memory")?;
    // each range within its window, then merged where they touch
    let mut r: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for (a, len, what) in std::mem::take(&mut p.ranges) {
        let Some(k) = u.window_of(a) else {
            p.m.left_out.push(format!("memory {a:#x}+{len:#x} ({what}): not in unified memory, a translated shader cannot read it"));
            continue;
        };
        let (base, size) = u.window(k);
        r.push((a, len.min(base + size - a), vec![what]));
    }
    r.sort_by_key(|x| x.0);
    let mut merged: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for (a, len, what) in r {
        match merged.last_mut() {
            Some(last) if a <= last.0 + last.1 && u.window_of(a) == u.window_of(last.0) => {
                last.1 = last.1.max(a + len - last.0);
                last.2.extend(what);
            }
            _ => merged.push((a, len, what)),
        }
    }
    p.m.memory_file = "memory.bin".into();
    let mut file = std::io::BufWriter::new(std::fs::File::create(p.dir.join(&p.m.memory_file)).map_err(|e| e.to_string())?);
    let mut at = 0u64;
    for (a, len, what) in merged {
        if len as u64 > p.budget {
            p.m.left_out.push(format!("memory {a:#x}+{len:#x} ({what:?}): over what is left of XCL_CAPTURE_MAX_MB"));
            continue;
        }
        p.budget -= len as u64;
        let mut unreadable = 0u64;
        let mut done = 0;
        while done < len {
            // a page at a time: what is not committed is written as zeros
            let n = (0x1000 - ((a + done) & 0xFFF)).min(len - done);
            if crate::gpu::shader::readable(a + done, n) {
                file.write_all(std::slice::from_raw_parts((a + done) as *const u8, n)).map_err(|e| e.to_string())?;
            } else {
                file.write_all(&vec![0u8; n]).map_err(|e| e.to_string())?;
                unreadable += n as u64;
            }
            done += n;
        }
        let k = u.window_of(a).unwrap_or(0);
        p.m.memory.push(Range { address: a as u64, len: len as u64, window: k as u32, offset: (a - u.window(k).0) as u64, file_offset: at, what, unreadable });
        at += len as u64;
    }
    file.flush().map_err(|e| e.to_string())
}

/// `lists` were submitted to `queue`: the captures recorded on them are finished.
pub(super) unsafe fn after_execute(queue: *mut c_void, lists: &[*mut c_void]) {
    if !on() {
        return;
    }
    let mine: Vec<Pending> = {
        let mut g = PENDING.lock().unwrap();
        let (mine, rest): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut *g).into_iter().partition(|p| lists.contains(&(p.list as *mut c_void)));
        *g = rest;
        mine
    };
    for mut p in mine {
        let t = std::time::Instant::now();
        if let Err(e) = super::title_textures::own_work(|| textures::read_back(queue, &p.dir, &mut p.m, &p.snaps, p.dev.as_ref())) {
            crate::xlog_warn!("CAPTURE: textures not read back: {e}");
            p.m.left_out.push(format!("textures: {e}"));
        }
        match p.m.write(&p.dir) {
            Ok(()) => crate::xlog_info!(
                "CAPTURE: written to {} ({} memory ranges, {} textures, {} left out, {} ms) – replay it with: xcl replay \"{}\"",
                p.dir.display(),
                p.m.memory.len(),
                p.m.textures.iter().filter(|t| t.file.is_some()).count(),
                p.m.left_out.len(),
                t.elapsed().as_millis(),
                p.dir.display()
            ),
            Err(e) => crate::xlog_warn!("CAPTURE: manifest not written: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view noted before its resource's address was released does not count; one noted after does.
    #[test]
    fn views_of_released_resources_are_not_live() {
        let res = 0x7E57_CA00usize;
        let old = ViewRec { res, desc: None, seq: SEQ.fetch_add(1, Ordering::Relaxed) };
        assert!(old.live());
        RELEASED.lock().unwrap().get_or_insert_with(HashMap::new).insert(res, SEQ.fetch_add(1, Ordering::Relaxed));
        assert!(!old.live());
        let new = ViewRec { res, desc: None, seq: SEQ.fetch_add(1, Ordering::Relaxed) };
        assert!(new.live());
        assert!(!ViewRec { res: 0, desc: None, seq: u64::MAX }.live());
    }

    /// The state a texture is taken to be in: its last transition on the list, else the hint of
    /// its use, else its last transition anywhere, else a shader-resource state.
    #[test]
    fn a_texture_state_comes_from_the_list_first() {
        let (list, other, res) = (0x7E57_1000usize, 0x7E57_2000usize, 0x7E57_3000usize);
        let all = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        assert_eq!(state_of(list, res, 1, None).0, 0xC0);
        assert_eq!(state_of(list, res, 1, Some((4, "bound as a target"))), (4, "bound as a target".to_string()));
        note_transition(other, res, all, 0x8);
        assert_eq!(state_of(list, res, 1, None).0, 0x8);
        assert_eq!(state_of(list, res, 1, Some((4, "bound as a target"))).0, 4);
        note_transition(list, res, all, 0x10);
        assert_eq!(state_of(list, res, 1, Some((4, "bound as a target"))).0, 0x10);
        // (the list's recording over: the last transition anywhere is that one)
        list_reset_for_test(list);
        assert_eq!(state_of(list, res, 1, None).0, 0x10);
        STATES.lock().unwrap().as_mut().unwrap().global.remove(&res);
    }

    /// A transition of one subresource (a mip, a slice) changes that one: the texture is in one
    /// known state again only when all its subresources are.
    #[test]
    fn a_partial_transition_leaves_the_texture_state_unknown() {
        let (list, res) = (0x7E58_1000usize, 0x7E58_3000usize);
        let all = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
        // 3 mips: all of them, then mip 1 alone
        note_transition(list, res, all, 0x80);
        assert_eq!(state_of(list, res, 3, None), (0x80, "last transition on this list".to_string()));
        note_transition(list, res, 1, 0x4);
        let (_, why) = state_of(list, res, 3, None);
        assert!(why.starts_with(PARTIAL_STATE) && why.contains("different states"), "{why}");
        // the others follow one by one: one state again
        note_transition(list, res, 0, 0x4);
        assert!(state_of(list, res, 3, None).1.starts_with(PARTIAL_STATE));
        note_transition(list, res, 2, 0x4);
        assert_eq!(state_of(list, res, 3, None).0, 0x4);
        // a transition of all of them replaces what the subresources had
        note_transition(list, res, 1, 0x8);
        note_transition(list, res, all, 0x10);
        assert_eq!(state_of(list, res, 3, None).0, 0x10);
        // only one subresource ever transitioned, the others unknown: no state for the texture
        let res2 = 0x7E58_4000usize;
        note_transition(list, res2, 0, 0x4);
        let (_, why) = state_of(list, res2, 2, None);
        assert!(why.starts_with(PARTIAL_STATE) && why.contains("subresource 1"), "{why}");
        // (a hint does not stand in for a partial transition recorded on the list)
        assert!(state_of(list, res2, 2, Some((4, "bound as a target"))).1.starts_with(PARTIAL_STATE));
        list_reset_for_test(list);
        let mut g = STATES.lock().unwrap();
        let st = g.as_mut().unwrap();
        st.global.remove(&res);
        st.global.remove(&res2);
    }

    fn list_reset_for_test(list: usize) {
        STATES.lock().unwrap().as_mut().unwrap().lists.remove(&list);
    }

    #[test]
    fn root_argument_words_read_past_the_end_as_zero() {
        let args = [1u8, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!((word(&args, 0), word(&args, 4), word(&args, 6), word(&args, 64)), (1, 2, 0, 0));
    }
}
