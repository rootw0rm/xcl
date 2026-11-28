//! Linux acphal slice (Phase C): the audio processor's 32-bit APU address space.
//!
//! The Windows `hle/acphal.rs` stays the implementation source of truth for the full
//! audio-processor path (SHAPE contexts, the XApu decoder channel, XDsp commands). On
//! Linux only the part a booting title asks for first is ported here: the APU memory
//! slice. Retail's audio init calls `ApuCreateHeap` at `exe+0x43ad933` and asserts
//! (`int3` at `exe+0x43ad94e`) when it fails; the console's APU heap is ordinary
//! process memory addressed by a separate 32-bit APU address, so these five calls are
//! real semantics, not a fake success.
//!
//! The console CPU addresses audio buffers by 32-bit APU addresses, not VAs:
//! `ApuAlloc` returns both, `ApuMapVirtualAddress` names the title's own memory, and
//! `ApuMapApuAddress` resolves an APU address back to the VA. The APU address ranges
//! live in `[APU_BASE, 4 GiB)` and must not overlap. `ApuCreateHeap` itself allocates
//! nothing: on the console it sizes the processor's heap, and here the heap is the
//! title's memory.
//!
//! SHAPE contexts, `AcpHalCreate` and the XApu/XDsp command channels stay stub
//! metadata until a measured consumer reaches them.

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::util::{HRESULT, S_OK};

const E_INVALIDARG: HRESULT = 0x8007_0057u32 as HRESULT;
const E_OUTOFMEMORY: HRESULT = 0x8007_000Eu32 as HRESULT;
const E_UNEXPECTED: HRESULT = 0x8000_FFFFu32 as HRESULT;

// The Win32 constants `kernelx_linux` keeps private; same values.
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;

// ---------------------------------------------------------------------------------------
// memory the audio processor addresses (32-bit APU addresses)
// ---------------------------------------------------------------------------------------

struct Region {
    va: usize,
    size: u32,
    apu: u32,
    /// allocated here (ApuAlloc), released by ApuFree
    owned: bool,
}

struct Apu {
    regions: Vec<Region>,
    next: u32,
}

static APU: Mutex<Apu> = Mutex::new(Apu { regions: Vec::new(), next: 0x1000_0000 });

const APU_BASE: u64 = 0x1000_0000;
const APU_LIMIT: u64 = (u32::MAX as u64) + 1;

impl Apu {
    fn region_end(r: &Region) -> u64 {
        let len = (r.size.max(1) as u64 + 0xFFF) & !0xFFF;
        (r.apu as u64).saturating_add(len)
    }

    fn overlaps(&self, start: u64, end: u64) -> Option<u64> {
        self.regions
            .iter()
            .filter(|r| {
                let rs = r.apu as u64;
                let re = Self::region_end(r);
                start < re && rs < end
            })
            .map(Self::region_end)
            .max()
    }

    fn find_free_range(&self, step: u64) -> Option<u32> {
        if step == 0 || step > APU_LIMIT - APU_BASE {
            return None;
        }
        // Fast path from `self.next`, then wrap once to `APU_BASE` to reclaim freed ranges.
        for start_hint in [self.next as u64, APU_BASE] {
            let mut cand = start_hint.max(APU_BASE);
            while let Some(end) = cand.checked_add(step).filter(|&e| e <= APU_LIMIT) {
                match self.overlaps(cand, end) {
                    None => return Some(cand as u32),
                    Some(skip_to) => {
                        let next = (skip_to + 0xFFF) & !0xFFF;
                        if next <= cand {
                            break;
                        }
                        cand = next;
                    }
                }
            }
        }
        None
    }

    /// The APU address of `va` – in a known region (for non-owned mappings covering the full span),
    /// or a new non-overlapping 32-bit APU range of `size` bytes.
    fn address(&mut self, va: usize, size: u32, owned: bool) -> Option<u32> {
        let span = size.max(1) as usize;
        if !owned {
            if let Some(r) = self.regions.iter().find(|r| {
                va >= r.va && (va - r.va).saturating_add(span) <= r.size.max(1) as usize
            }) {
                return Some(r.apu + (va - r.va) as u32);
            }
        }
        let step = (size.max(1) as u64 + 0xFFF) & !0xFFF;
        let apu = self.find_free_range(step)?;
        let next = (apu as u64) + step;
        self.next = if next < APU_LIMIT { next as u32 } else { APU_BASE as u32 };
        self.regions.push(Region { va, size, apu, owned });
        Some(apu)
    }
}

/// `size` bytes aligned to `align` (zeroed; the allocation's base is `va` rounded down to
/// 64 KiB – VirtualAlloc's granularity – so MEM_RELEASE of it frees all)
unsafe fn alloc_aligned(size: usize, align: usize) -> Option<usize> {
    let align = align.max(1).checked_next_power_of_two()?;
    if align <= 0x1_0000 {
        let p = crate::hle::kernelx_linux::VirtualAlloc(
            std::ptr::null(),
            size.max(1),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        ) as usize;
        return (p != 0).then_some(p);
    }
    let reserve = size.checked_add(align)?;
    // reserve enough to find an aligned start, then reserve exactly there
    for _ in 0..8 {
        let p = crate::hle::kernelx_linux::VirtualAlloc(
            std::ptr::null(),
            reserve,
            MEM_RESERVE,
            PAGE_READWRITE,
        ) as usize;
        if p == 0 {
            return None;
        }
        crate::hle::kernelx_linux::VirtualFree(p as *mut c_void, 0, MEM_RELEASE);
        let a = p.checked_add(align - 1)? & !(align - 1);
        let q = crate::hle::kernelx_linux::VirtualAlloc(
            a as *const c_void,
            size.max(1),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        ) as usize;
        if q == a {
            return Some(q);
        }
        if q != 0 {
            crate::hle::kernelx_linux::VirtualFree(q as *mut c_void, 0, MEM_RELEASE);
        }
    }
    None
}

/// ApuCreateHeap(size, flags): the heap ApuAlloc takes from – the process's memory here.
pub unsafe extern "win64" fn ApuCreateHeap(size: u32, flags: u32) -> HRESULT {
    crate::xlog_info!("acphal: ApuCreateHeap({size:#x}, {flags:#x}) – the audio processor's memory is the process's");
    S_OK
}

/// ApuAlloc(&va, &apu, size, alignment, flags)
pub unsafe extern "win64" fn ApuAlloc(
    va: *mut *mut c_void,
    apu: *mut u32,
    size: u32,
    alignment: u32,
    _flags: u32,
) -> HRESULT {
    if va.is_null() {
        return E_INVALIDARG;
    }
    let Some(p) = alloc_aligned(size as usize, alignment as usize) else {
        *va = std::ptr::null_mut();
        return E_OUTOFMEMORY;
    };
    let Some(a) = APU.lock().unwrap().address(p, size, true) else {
        crate::hle::kernelx_linux::VirtualFree(p as *mut c_void, 0, MEM_RELEASE);
        *va = std::ptr::null_mut();
        return E_OUTOFMEMORY;
    };
    *va = p as *mut c_void;
    if !apu.is_null() {
        *apu = a;
    }
    crate::xlog_debug!(
        "apu: ApuAlloc({size:#x}, align {alignment:#x}) -> va {p:#x} apu {a:#x}"
    );
    S_OK
}

/// ApuFree(va)
pub unsafe extern "win64" fn ApuFree(va: *mut c_void) -> HRESULT {
    if va.is_null() {
        return E_INVALIDARG;
    }
    let owned = {
        let mut g = APU.lock().unwrap();
        let i = g
            .regions
            .iter()
            .position(|r| r.va == va as usize && r.owned)
            .or_else(|| g.regions.iter().position(|r| r.va == va as usize));
        i.map(|i| g.regions.swap_remove(i).owned).unwrap_or(false)
    };
    if owned {
        crate::hle::kernelx_linux::VirtualFree(va, 0, MEM_RELEASE);
    }
    S_OK
}

/// ApuMapVirtualAddress(&apu, va, size): an APU address for the title's own memory
pub unsafe extern "win64" fn ApuMapVirtualAddress(
    apu: *mut u32,
    va: *mut c_void,
    size: u32,
) -> HRESULT {
    if apu.is_null() || va.is_null() {
        return E_INVALIDARG;
    }
    let Some(a) = APU.lock().unwrap().address(va as usize, size, false) else {
        return E_OUTOFMEMORY;
    };
    *apu = a;
    crate::xlog_debug!("apu: ApuMapVirtualAddress({va:p}, {size:#x}) -> apu {a:#x}");
    S_OK
}

/// ApuMapApuAddress(apu) -> the memory an APU address stands for (null: none)
pub unsafe extern "win64" fn ApuMapApuAddress(apu: u32) -> *mut c_void {
    let g = APU.lock().unwrap();
    g.regions
        .iter()
        .find(|r| apu >= r.apu && apu - r.apu < r.size.max(1))
        .map_or(std::ptr::null_mut(), |r| (r.va + (apu - r.apu) as usize) as *mut c_void)
}

/// ApuIsVirtualAddressValid: all of the process's memory is the processor's here
pub unsafe extern "win64" fn ApuIsVirtualAddressValid(_va: *const c_void, _size: u32) -> i32 {
    1
}

/// ApuHeapGetState(&state, flags): free bytes, bytes in use, bytes lost, the largest free
/// block, blocks – there is always room
pub unsafe extern "win64" fn ApuHeapGetState(state: *mut u32, _flags: u32) -> HRESULT {
    if state.is_null() {
        return E_INVALIDARG;
    }
    let g = APU.lock().unwrap();
    let used: u32 =
        g.regions.iter().filter(|r| r.owned).map(|r| r.size).fold(0, u32::wrapping_add);
    let s = std::slice::from_raw_parts_mut(state, 5);
    s.copy_from_slice(&[256 << 20, used, 0, 64 << 20, g.regions.len() as u32]);
    S_OK
}

// ---------------------------------------------------------------------------------------
// SHAPE contexts (AcpHalAllocateShapeContexts)
// ---------------------------------------------------------------------------------------
//
// The processor's SHAPE mixer contexts: five arrays, one per kind, 64 bytes each,
// allocated in APU memory. Retail's audio init passes a global `ShapeContexts` and
// asserts when this returns a failure. The mixer does not run here – XCL presents its
// own frame and the title mixes its audio itself – so the arrays only have to exist.

/// AcpHalAllocateShapeContexts' argument: counts of the five kinds, then an array of each
#[repr(C)]
pub struct ShapeContexts {
    counts: [u32; 5],
    reserved: u32,
    contexts: [*mut c_void; 5],
}

/// bytes of a context entry (the largest seen)
const CONTEXT_BYTES: u32 = 64;
static SHAPE: Mutex<[usize; 5]> = Mutex::new([0; 5]);

unsafe fn allocate_shape_contexts_with<F>(c: *mut ShapeContexts, mut alloc_fn: F) -> HRESULT
where
    F: FnMut(usize, u32) -> Result<*mut c_void, HRESULT>,
{
    if c.is_null() {
        return E_INVALIDARG;
    }
    let c = &mut *c;
    let mut sizes = [0u32; 5];
    for i in 0..5 {
        if c.counts[i] != 0 {
            let Some(bytes) = c.counts[i].checked_mul(CONTEXT_BYTES) else {
                c.contexts = [std::ptr::null_mut(); 5];
                return E_OUTOFMEMORY;
            };
            sizes[i] = bytes;
        }
    }
    let mut new_arrays = [0usize; 5];
    let mut new_ptrs = [std::ptr::null_mut(); 5];
    for i in 0..5 {
        if sizes[i] == 0 {
            continue;
        }
        match alloc_fn(i, sizes[i]) {
            Ok(va) => {
                new_arrays[i] = va as usize;
                new_ptrs[i] = va;
            }
            Err(hr) => {
                for &a in &new_arrays[..i] {
                    if a != 0 {
                        ApuFree(a as *mut c_void);
                    }
                }
                c.contexts = [std::ptr::null_mut(); 5];
                return hr;
            }
        }
    }
    let old_arrays = {
        let mut arrays = SHAPE.lock().unwrap();
        let old = std::mem::replace(&mut *arrays, new_arrays);
        c.contexts = new_ptrs;
        old
    };
    for a in old_arrays.into_iter().filter(|&a| a != 0) {
        ApuFree(a as *mut c_void);
    }
    crate::xlog_info!(
        "acphal: SHAPE contexts {:?} (they do not run: no audio from the console's mixer)",
        c.counts
    );
    S_OK
}

/// AcpHalAllocateShapeContexts(&contexts): an array of each kind in APU memory (none run)
pub unsafe extern "win64" fn AcpHalAllocateShapeContexts(c: *mut ShapeContexts) -> HRESULT {
    allocate_shape_contexts_with(c, |_, bytes| {
        let mut va = std::ptr::null_mut();
        let hr = ApuAlloc(&mut va, std::ptr::null_mut(), bytes, 256, 0);
        if hr < 0 { Err(hr) } else { Ok(va) }
    })
}

/// AcpHalReleaseShapeContexts()
pub unsafe extern "win64" fn AcpHalReleaseShapeContexts() -> HRESULT {
    let arrays = std::mem::take(&mut *SHAPE.lock().unwrap());
    for a in arrays.into_iter().filter(|&a| a != 0) {
        ApuFree(a as *mut c_void);
    }
    S_OK
}

// ---------------------------------------------------------------------------------------
// the processor's command / message channel (AcpHalCreate)
// ---------------------------------------------------------------------------------------
//
// An object whose vtable is Connect(commands, messages), Disconnect,
// SubmitCommand(kind, id, frame, data, size), PopMessage(&message) -> bool,
// GetNumMessages, Release. Messages are `{u32 kind, u32, u64 value, u64 id}`; the
// channel acknowledges every command with id != 0 and ticks a 128-sample audio frame
// every 2667 µs once connected. No audio is mixed.

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: u32,
    pub reserved: u32,
    pub value: u64,
    pub id: u64,
}

pub const MSG_AUDIO_FRAME: u32 = 2;
pub const MSG_COMMAND_DONE: u32 = 0x10;
pub const MSG_DISCONNECTED: u32 = 0x80;

/// one audio frame: 128 samples at 48 kHz
const FRAME: Duration = Duration::from_micros(2667);

/// What the channel holds: queued messages and when the next audio frame is due.
#[derive(Default)]
pub struct Channel {
    messages: VecDeque<Message>,
    connected: bool,
    next_frame: Option<Instant>,
}

impl Channel {
    pub fn connect(&mut self, now: Instant) {
        self.connected = true;
        // A reconnect starts a fresh channel: messages queued for the old one
        // (a MSG_DISCONNECT nobody popped, stale command acks) must not surface.
        self.messages.clear();
        self.next_frame = Some(now + FRAME);
    }

    pub fn disconnect(&mut self) {
        self.connected = false;
        self.messages.push_back(Message { kind: MSG_DISCONNECTED, ..Default::default() });
    }

    /// A command: done at once (a message for it unless its id is 0 – nobody waits).
    pub fn submit(&mut self, id: u64) -> bool {
        if !self.connected {
            return false;
        }
        if id != 0 {
            self.messages.push_back(Message { kind: MSG_COMMAND_DONE, id, ..Default::default() });
        }
        true
    }

    fn frame_due(&self, now: Instant) -> bool {
        self.connected && self.next_frame.is_some_and(|t| now >= t)
    }

    /// The next message: queued ones first, else an audio frame when one is due (a frame
    /// late by more than 8 is not made up for).
    pub fn pop(&mut self, now: Instant) -> Option<Message> {
        if let Some(m) = self.messages.pop_front() {
            return Some(m);
        }
        if self.frame_due(now) {
            let t = self.next_frame.unwrap_or(now) + FRAME;
            self.next_frame = Some(match now.checked_sub(FRAME * 8) {
                Some(floor) => t.max(floor),
                None => t,
            });
            return Some(Message { kind: MSG_AUDIO_FRAME, ..Default::default() });
        }
        None
    }

    pub fn count(&self, now: Instant) -> u32 {
        self.messages.len() as u32 + self.frame_due(now) as u32
    }
}

#[repr(C)]
struct Hal {
    vtable: *const usize,
    channel: Mutex<Channel>,
}

unsafe extern "win64" fn hal_connect(h: *mut Hal, commands: u32, messages: u32) -> HRESULT {
    (*h).channel.lock().unwrap().connect(Instant::now());
    crate::xlog_info!(
        "acphal: audio processor connected ({commands} commands, {messages} messages) – commands are acknowledged, nothing runs"
    );
    S_OK
}

unsafe extern "win64" fn hal_disconnect(h: *mut Hal) -> HRESULT {
    (*h).channel.lock().unwrap().disconnect();
    S_OK
}

unsafe extern "win64" fn hal_submit(
    h: *mut Hal,
    kind: u32,
    id: u64,
    _frame: u32,
    _data: *const c_void,
    _size: u32,
) -> HRESULT {
    static SEEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    {
        let mut s = SEEN.lock().unwrap();
        if !s.contains(&kind) && s.len() < 64 {
            s.push(kind);
            crate::xlog_info!("acphal: audio processor command {kind:#x} first seen");
        }
    }
    if (*h).channel.lock().unwrap().submit(id) {
        S_OK
    } else {
        E_UNEXPECTED
    }
}

unsafe extern "win64" fn hal_pop(h: *mut Hal, m: *mut Message) -> bool {
    if m.is_null() {
        return false;
    }
    match (*h).channel.lock().unwrap().pop(Instant::now()) {
        Some(x) => {
            *m = x;
            true
        }
        None => false,
    }
}

unsafe extern "win64" fn hal_count(h: *mut Hal) -> u32 {
    (*h).channel.lock().unwrap().count(Instant::now())
}

unsafe extern "win64" fn hal_release(h: *mut Hal) {
    drop(Box::from_raw(h));
}

/// the channel object's vtable (function addresses are not constants, but the table is
/// process-global and lives for the process)
static HAL_VTABLE: LazyLock<[usize; 6]> = LazyLock::new(|| {
    [
        hal_connect as usize,
        hal_disconnect as usize,
        hal_submit as usize,
        hal_pop as usize,
        hal_count as usize,
        hal_release as usize,
    ]
});

fn hal_vtable() -> *const usize {
    HAL_VTABLE.as_ptr()
}

/// AcpHalCreate(&hal)
pub unsafe extern "win64" fn AcpHalCreate(out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let h = Box::new(Hal { vtable: hal_vtable(), channel: Mutex::new(Channel::default()) });
    *out = Box::into_raw(h) as *mut c_void;
    S_OK
}

/// Implemented exports with real code addresses. Everything else in the acphal route
/// (the XApu/XDsp command channels) stays stub metadata in `hle_linux` until a measured
/// consumer reaches it.
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("ApuCreateHeap", ApuCreateHeap as usize),
        ("ApuAlloc", ApuAlloc as usize),
        ("ApuFree", ApuFree as usize),
        ("ApuMapVirtualAddress", ApuMapVirtualAddress as usize),
        ("ApuMapApuAddress", ApuMapApuAddress as usize),
        ("ApuIsVirtualAddressValid", ApuIsVirtualAddressValid as usize),
        ("ApuHeapGetState", ApuHeapGetState as usize),
        ("AcpHalAllocateShapeContexts", AcpHalAllocateShapeContexts as usize),
        ("AcpHalReleaseShapeContexts", AcpHalReleaseShapeContexts as usize),
        ("AcpHalCreate", AcpHalCreate as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Resolve through the Linux registry, not this module's table: the RED for a new
    // slice is "the route still hands out stub metadata", which the module's own list
    // would hide.
    fn acphal_addr(name: &str) -> usize {
        let m = crate::hle::registry()
            .hle_for("acphal.dll")
            .unwrap_or_else(|| panic!("acphal.dll must be routed"));
        let addr = *m
            .exports
            .get(name)
            .unwrap_or_else(|| panic!("{name} must be wired"));
        assert_ne!(addr, 0, "{name} must be implemented, not stub metadata");
        addr
    }

    #[test]
    fn apu_entry_points_are_wired() {
        for name in [
            "ApuCreateHeap",
            "ApuAlloc",
            "ApuFree",
            "ApuMapVirtualAddress",
            "ApuMapApuAddress",
            "ApuIsVirtualAddressValid",
            "ApuHeapGetState",
        ] {
            acphal_addr(name);
        }
    }

    #[test]
    fn apu_memory_maps_both_ways() {
        type CreateHeap = unsafe extern "win64" fn(u32, u32) -> HRESULT;
        type Alloc = unsafe extern "win64" fn(*mut *mut c_void, *mut u32, u32, u32, u32) -> HRESULT;
        type Free = unsafe extern "win64" fn(*mut c_void) -> HRESULT;
        type MapVa = unsafe extern "win64" fn(*mut u32, *mut c_void, u32) -> HRESULT;
        type MapApu = unsafe extern "win64" fn(u32) -> *mut c_void;
        type Valid = unsafe extern "win64" fn(*const c_void, u32) -> i32;
        type HeapState = unsafe extern "win64" fn(*mut u32, u32) -> HRESULT;
        let create_heap: CreateHeap = unsafe { std::mem::transmute(acphal_addr("ApuCreateHeap")) };
        let alloc: Alloc = unsafe { std::mem::transmute(acphal_addr("ApuAlloc")) };
        let free: Free = unsafe { std::mem::transmute(acphal_addr("ApuFree")) };
        let map_va: MapVa = unsafe { std::mem::transmute(acphal_addr("ApuMapVirtualAddress")) };
        let map_apu: MapApu = unsafe { std::mem::transmute(acphal_addr("ApuMapApuAddress")) };
        let valid: Valid = unsafe { std::mem::transmute(acphal_addr("ApuIsVirtualAddressValid")) };
        let heap_state: HeapState = unsafe { std::mem::transmute(acphal_addr("ApuHeapGetState")) };
        unsafe {
            // Retail's request: the console heap is process memory.
            assert_eq!(create_heap(0x1c70_0000, 0x22_6e00), S_OK);
            let (mut va, mut apu) = (std::ptr::null_mut(), 0u32);
            assert_eq!(alloc(&mut va, &mut apu, 0x3000, 0x2_0000, 0), S_OK);
            assert_eq!(va as usize % 0x2_0000, 0, "alignment above 64 KiB");
            assert_eq!(map_apu(apu + 0x10) as usize, va as usize + 0x10);
            let mut again = 0u32;
            assert_eq!(
                map_va(&mut again, (va as usize + 0x20) as *mut c_void, 4),
                S_OK
            );
            assert_eq!(again, apu + 0x20);
            assert_eq!(valid(va, 0x3000), 1);
            let mut state = [0u32; 5];
            assert_eq!(heap_state(state.as_mut_ptr(), 0), S_OK);
            assert!(state[4] >= 1, "the allocation is a block");
            assert!(state[1] >= 0x3000, "in-use bytes cover the allocation");
            assert_eq!(free(va), S_OK);
            assert!(map_apu(apu).is_null(), "a freed range no longer resolves");
            // Invalid arguments fail without writing outputs.
            let mut untouched = 0xDEAD_BEEFu32;
            assert_eq!(map_va(&mut untouched, std::ptr::null_mut(), 16), E_INVALIDARG);
            assert_eq!(untouched, 0xDEAD_BEEF);
            assert_eq!(alloc(std::ptr::null_mut(), &mut untouched, 16, 0x100, 0), E_INVALIDARG);
            assert_eq!(free(std::ptr::null_mut()), E_INVALIDARG);
            assert_eq!(heap_state(std::ptr::null_mut(), 0), E_INVALIDARG);
        }
    }

    #[test]
    fn apu_address_prevents_overflow_overlap_and_rejects_null_va() {
        // u32::MAX cannot fit within [APU_BASE, 4 GiB) without wrapping.
        let mut apu = Apu { regions: Vec::new(), next: 0x1000_0000 };
        assert_eq!(apu.address(0x1_0000_0000, u32::MAX, false), None);

        // Near the 4 GiB boundary: a region that would cross u32::MAX wraps to APU_BASE without
        // overlapping an existing live region at APU_BASE.
        let first = apu.address(0x2_0000_0000, 0x2000, false).unwrap();
        assert_eq!(first, 0x1000_0000);
        apu.next = 0xFFFF_F000;
        let second = apu.address(0x3_0000_0000, 0x2000, false).unwrap();
        assert_eq!(second, 0x1000_2000, "wrapped range must skip past live region at APU_BASE");

        // Owned allocation must register its own entry even if an unowned mapping covers the VA.
        let owned = apu.address(0x2_0000_0000, 0x1000, true).unwrap();
        assert_ne!(owned, first);
        assert!(apu.regions.iter().any(|r| r.apu == owned && r.owned));

        unsafe {
            let mut out = 0xDEAD_BEEFu32;
            assert_eq!(
                ApuMapVirtualAddress(&mut out, std::ptr::null_mut(), 16),
                E_INVALIDARG
            );
            assert_eq!(out, 0xDEAD_BEEF);
        }
    }

    #[test]
    fn acphal_shape_and_channel_entry_points_are_wired() {
        for name in [
            "AcpHalAllocateShapeContexts",
            "AcpHalReleaseShapeContexts",
            "AcpHalCreate",
        ] {
            acphal_addr(name);
        }
    }

    #[test]
    fn shape_contexts_allocate_release_and_roll_back() {
        type AllocShape = unsafe extern "win64" fn(*mut ShapeContexts) -> HRESULT;
        type ReleaseShape = unsafe extern "win64" fn() -> HRESULT;
        let alloc: AllocShape =
            unsafe { std::mem::transmute(acphal_addr("AcpHalAllocateShapeContexts")) };
        let release: ReleaseShape =
            unsafe { std::mem::transmute(acphal_addr("AcpHalReleaseShapeContexts")) };
        unsafe {
            let mut c = ShapeContexts {
                counts: [2, 1, 0, 0, 0],
                reserved: 0,
                contexts: [std::ptr::null_mut(); 5],
            };
            assert_eq!(alloc(&mut c), S_OK);
            assert!(!c.contexts[0].is_null() && !c.contexts[1].is_null());
            assert!(c.contexts[2..].iter().all(|p| p.is_null()));
            assert_eq!(release(), S_OK);
            assert_eq!(release(), S_OK, "release with nothing allocated is a no-op");
            assert_eq!(alloc(std::ptr::null_mut()), E_INVALIDARG);

            // A partial failure frees what the earlier kinds allocated and nulls every
            // pointer, so the caller cannot keep a half-built context table.
            let mut c = ShapeContexts {
                counts: [1, 1, 0, 0, 0],
                reserved: 0,
                contexts: [std::ptr::null_mut(); 5],
            };
            let mut first = 0usize;
            let hr = allocate_shape_contexts_with(&mut c, |i, bytes| {
                if i == 1 {
                    return Err(E_OUTOFMEMORY);
                }
                let mut va = std::ptr::null_mut();
                let hr = ApuAlloc(&mut va, std::ptr::null_mut(), bytes, 256, 0);
                if hr < 0 {
                    return Err(hr);
                }
                first = va as usize;
                Ok(va)
            });
            assert_eq!(hr, E_OUTOFMEMORY);
            assert!(c.contexts.iter().all(|p| p.is_null()));
            assert_ne!(first, 0);
            assert!(
                !APU.lock().unwrap().regions.iter().any(|r| r.va == first),
                "the successful first allocation is rolled back"
            );
        }
    }

    #[test]
    fn channel_acknowledges_and_ticks() {
        let t0 = Instant::now();
        let mut c = Channel::default();
        assert!(!c.submit(7), "not connected yet");
        c.connect(t0);
        assert!(c.submit(7));
        assert!(c.submit(0), "id 0: no message");
        assert_eq!(c.count(t0), 1);
        assert_eq!(c.pop(t0), Some(Message { kind: MSG_COMMAND_DONE, id: 7, ..Default::default() }));
        assert_eq!(c.pop(t0), None, "no frame due yet");
        let later = t0 + FRAME * 3;
        assert_eq!(c.pop(later).map(|m| m.kind), Some(MSG_AUDIO_FRAME));
        c.disconnect();
        assert_eq!(c.pop(later).map(|m| m.kind), Some(MSG_DISCONNECTED));
    }

    #[test]
    fn reconnect_drops_messages_queued_for_the_old_channel() {
        let t0 = Instant::now();
        let mut c = Channel::default();
        c.connect(t0);
        c.disconnect(); // queues MSG_DISCONNECTED; the title never pops it
        c.connect(t0);
        assert_eq!(c.pop(t0), None, "the stale disconnect must not surface on the new channel");
        let later = t0 + FRAME * 3;
        assert_eq!(c.pop(later).map(|m| m.kind), Some(MSG_AUDIO_FRAME));
    }

    #[test]
    fn hal_object_connects_acknowledges_and_releases() {
        type Create = unsafe extern "win64" fn(*mut *mut c_void) -> HRESULT;
        type Connect = unsafe extern "win64" fn(*mut c_void, u32, u32) -> HRESULT;
        type Disconnect = unsafe extern "win64" fn(*mut c_void) -> HRESULT;
        type Submit =
            unsafe extern "win64" fn(*mut c_void, u32, u64, u32, *const c_void, u32) -> HRESULT;
        type Pop = unsafe extern "win64" fn(*mut c_void, *mut Message) -> bool;
        type Release = unsafe extern "win64" fn(*mut c_void);
        let create: Create = unsafe { std::mem::transmute(acphal_addr("AcpHalCreate")) };
        unsafe {
            let mut h: *mut c_void = std::ptr::null_mut();
            assert_eq!(create(&mut h), S_OK);
            assert!(!h.is_null());
            let vt = *(h as *const usize);
            let slot = |i: usize| *((vt as *const usize).add(i));
            let connect: Connect = std::mem::transmute(slot(0));
            let disconnect: Disconnect = std::mem::transmute(slot(1));
            let submit: Submit = std::mem::transmute(slot(2));
            let pop: Pop = std::mem::transmute(slot(3));
            let release: Release = std::mem::transmute(slot(5));
            assert_eq!(connect(h, 0x1000, 0x1000), S_OK);
            // Retail's first command: type 2, id 0xf0f0f0f0, frame -1, size 0.
            assert_eq!(submit(h, 2, 0xf0f0_f0f0, u32::MAX, std::ptr::null(), 0), S_OK);
            let mut m = Message::default();
            assert!(pop(h, &mut m));
            assert_eq!((m.kind, m.id), (MSG_COMMAND_DONE, 0xf0f0_f0f0));
            assert!(!pop(h, std::ptr::null_mut()), "a null message pointer is refused");
            assert_eq!(disconnect(h), S_OK);
            assert!(pop(h, &mut m));
            assert_eq!(m.kind, MSG_DISCONNECTED);
            release(h);
        }
    }
}
