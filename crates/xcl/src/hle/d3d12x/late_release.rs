//! The title's last `Release` of a resource, put off until the GPU is past the work in flight.
//!
//! D3D12 on the PC asks that an object outlives the GPU work that uses it. A console title
//! keeps to that by its own count of finished frames – but XCL's submits are not the title's:
//! texture uploads go out with a later submit, tile mappings on a queue of XCL's own. A retail
//! title's final `Release` of a resource then came while a queue still had work on it (debug
//! layer: "CORRUPTION: … is referenced by GPU operations in-flight", id 921).
//!
//! The last reference is held here instead. Each batch of held resources gets a fence value
//! signalled on every queue with work on it; the batch goes once all of them have passed it.
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

/// a batch is not closed more often than this (a `Signal` a queue each time)
const SEAL_EVERY: Duration = Duration::from_millis(10);
/// how often the thread here looks for batches that can go
const TICK: Duration = Duration::from_millis(4);

struct Batch {
    dead: Vec<usize>,
    /// (fence, value) a queue: all reached = nothing submitted before the batch closed is left
    marks: Vec<(ID3D12Fence, u64)>,
}

#[derive(Default)]
struct Held {
    /// queue → its fence here and the last value signalled on it
    fences: HashMap<usize, (ID3D12Fence, u64)>,
    open: Vec<usize>,
    sealed: VecDeque<Batch>,
    last_seal: Option<Instant>,
}

// (the interface pointers are only used under the lock)
unsafe impl Send for Held {}

impl Held {
    fn is_empty(&self) -> bool {
        self.open.is_empty() && self.sealed.is_empty()
    }

    /// Close the open batch (at most every `SEAL_EVERY`, or `seal_now`) with a fence value on each of `queues`,
    /// and take out the batches every queue is past: the resources to release now.
    unsafe fn drain(&mut self, queues: &[ID3D12CommandQueue], seal_now: bool) -> Vec<usize> {
        if !self.open.is_empty() && (seal_now || self.last_seal.is_none_or(|t| t.elapsed() >= SEAL_EVERY)) {
            let mut marks = Vec::with_capacity(queues.len());
            let mut ok = true;
            for q in queues {
                let key = q.as_raw() as usize;
                if !self.fences.contains_key(&key) {
                    let mut dev: Option<ID3D12Device> = None;
                    let made = q.GetDevice(&mut dev).ok().and(dev).and_then(|d| d.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE).ok());
                    let Some(f) = made else {
                        ok = false;
                        break;
                    };
                    self.fences.insert(key, (f, 0));
                }
                let (fence, value) = self.fences.get_mut(&key).unwrap();
                if q.Signal(&*fence, *value + 1).is_err() {
                    self.fences.remove(&key);
                    ok = false;
                    break;
                }
                *value += 1;
                marks.push((fence.clone(), *value));
            }
            // (no fence or no signal: the device is gone – the batch stays open, and held)
            if ok {
                self.last_seal = Some(Instant::now());
                let dead = std::mem::take(&mut self.open);
                self.sealed.push_back(Batch { dead, marks });
            }
        }
        let mut gone = Vec::new();
        while self.sealed.front().is_some_and(|b| b.marks.iter().all(|(f, v)| f.GetCompletedValue() >= *v)) {
            gone.extend(self.sealed.pop_front().unwrap().dead);
        }
        gone
    }
}

static HELD: Mutex<Option<Held>> = Mutex::new(None);

/// Whether last releases are held (`XCL_LATE_RELEASE=0` turns it off).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("XCL_LATE_RELEASE").is_ok_and(|v| v.trim() == "0"))
}

/// how long a last `Release` itself waits for queues with nothing to do to say so
const IDLE_WAIT: Duration = Duration::from_micros(300);

fn ensure_thread() {
    static THREAD: std::sync::Once = std::sync::Once::new();
    THREAD.call_once(|| {
        let _ = std::thread::Builder::new().name("xcl-late-release".into()).spawn(|| loop {
            std::thread::sleep(TICK);
            unsafe { drain_known(false) };
        });
    });
}

/// Queue `obj` for release on the background thread once all queues pass the current fence,
/// without spinning the caller thread.
pub unsafe fn hold_deferred(obj: usize) {
    HELD.lock().unwrap().get_or_insert_with(Held::default).open.push(obj);
    ensure_thread();
}

/// `res` (an adopted resource) holds one reference that is now this module's. With no work in
/// flight it goes before this returns (the queues pass a fence value at once): a title that
/// frees the memory under a resource right after its last `Release` finds it unpinned, as
/// before. Otherwise the thread here lets it go.
pub unsafe fn hold(res: usize) {
    HELD.lock().unwrap().get_or_insert_with(Held::default).open.push(res);
    let start = Instant::now();
    drain_known(true);
    while start.elapsed() < IDLE_WAIT && HELD.lock().unwrap().as_ref().is_some_and(|h| !h.is_empty()) {
        std::hint::spin_loop();
        drain_known(false);
    }
    if HELD.lock().unwrap().as_ref().is_none_or(Held::is_empty) {
        return;
    }
    // A thread of its own lets the held ones go: the title may release a resource and then
    // wait for its memory without another submit (an imported heap pins the pages under its
    // resources – unmapping them stood still while one was held "until the next submit").
    ensure_thread();
}

unsafe fn release_obj(obj: usize) {
    if super::vram::recycle_ready(obj) {
        return;
    }
    let raw = obj as *mut c_void;
    let release_fn = if super::real::is_adopted(raw) {
        super::real::orig_fn(raw, 2)
    } else {
        *(*(raw as *const *const usize)).add(2)
    };
    let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(release_fn);
    f(raw);
}

/// Release what every queue XCL knows of is past: the title's, the tile-mapping one, the
/// upload ones.
pub unsafe fn drain_known(seal_now: bool) {
    if HELD.lock().unwrap().as_ref().is_none_or(Held::is_empty) {
        return;
    }
    let mut queues = super::present::known_queue_refs();
    queues.extend(super::sparse::queue());
    queues.extend(super::title_textures::upload_queues());
    let gone = match HELD.lock().unwrap().as_mut() {
        Some(h) => h.drain(&queues, seal_now),
        None => return,
    };
    // (outside the lock: a release may end other objects, whose releases come back here)
    for obj in gone {
        release_obj(obj);
    }
}

#[cfg(test)]
pub(super) fn forget_queue_for_test(queue: usize) {
    if let Some(h) = HELD.lock().unwrap().as_mut() {
        h.fences.remove(&queue);
    }
}

#[cfg(test)]
pub(super) fn is_held(obj: usize) -> bool {
    let g = HELD.lock().unwrap();
    g.as_ref().is_some_and(|h| h.open.contains(&obj) || h.sealed.iter().any(|b| b.dead.contains(&obj)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    /// A held resource stays while a queue has not got past the work submitted before it was
    /// released, and goes once the queue has.
    #[test]
    fn a_held_resource_goes_when_the_queue_is_past_its_batch() {
        unsafe {
            let Ok(dev) = crate::gpu::agility::create_device(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).unwrap();
            // the queue stands still, as with work in flight: it waits for a fence nobody has signalled
            let gate: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Wait(&gate, 1).unwrap();
            let mut res: Option<ID3D12Resource> = None;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 4096,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut res).unwrap();
            let raw = res.unwrap().into_raw() as usize;

            let mut held = Held::default();
            held.open.push(raw);
            let queues = [queue.clone()];
            assert!(held.drain(&queues, false).is_empty(), "the queue has not passed the batch's fence value");
            std::thread::sleep(Duration::from_millis(100));
            assert!(held.drain(&queues, false).is_empty(), "still in flight: still held");
            assert!(!held.is_empty());

            gate.Signal(1).unwrap();
            let start = Instant::now();
            let mut gone = Vec::new();
            while gone.is_empty() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
                gone = held.drain(&queues, false);
            }
            assert_eq!(gone, vec![raw], "released once the queue is past the batch");
            assert!(held.is_empty());
            drop(ID3D12Resource::from_raw(raw as *mut c_void));
        }
    }

    /// With no queue at all nothing can be in flight: the next drain lets the batch go.
    #[test]
    fn without_queues_a_batch_goes_at_once() {
        let mut held = Held::default();
        held.open.push(0x1000);
        assert_eq!(unsafe { held.drain(&[], false) }, vec![0x1000]);
        assert!(held.is_empty());
    }

    /// An unadopted COM object held until GPU retirement is released safely via vtable slot 2
    /// once the blocking queue passes its batch fence.
    #[test]
    fn unadopted_com_object_is_released_after_queue_retirement() {
        use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
        use std::sync::Arc;
        use windows::core::GUID;

        #[repr(C)]
        struct TestComObj {
            vt: *const [usize; 3],
            refs: AtomicU32,
            drops: Arc<AtomicUsize>,
        }
        unsafe extern "system" fn qi(_: *mut c_void, _: *const GUID, out: *mut *mut c_void) -> i32 {
            if !out.is_null() {
                *out = std::ptr::null_mut();
            }
            -2147467262 // E_NOINTERFACE
        }
        unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
            (&*(this as *const TestComObj)).refs.fetch_add(1, Ordering::AcqRel) + 1
        }
        unsafe extern "system" fn release(this: *mut c_void) -> u32 {
            let n = (&*(this as *const TestComObj)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
            if n == 0 {
                let obj = Box::from_raw(this as *mut TestComObj);
                obj.drops.fetch_add(1, Ordering::AcqRel);
            }
            n
        }
        let vt: *const [usize; 3] = Box::leak(Box::new([qi as usize, add_ref as usize, release as usize]));

        unsafe {
            let Ok(dev) = crate::gpu::agility::create_device(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            let queue: ID3D12CommandQueue =
                dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COMPUTE, ..Default::default() }).unwrap();
            let gate: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Wait(&gate, 1).unwrap();

            let drops = Arc::new(AtomicUsize::new(0));
            let obj = Box::into_raw(Box::new(TestComObj {
                vt,
                refs: AtomicU32::new(1),
                drops: drops.clone(),
            })) as usize;
            assert!(!super::super::real::is_adopted(obj as *mut c_void));

            let mut held = Held::default();
            held.open.push(obj);
            let queues = [queue];
            assert!(held.drain(&queues, false).is_empty());
            assert_eq!(drops.load(Ordering::Acquire), 0);

            gate.Signal(1).unwrap();
            let start = Instant::now();
            let mut gone = Vec::new();
            while gone.is_empty() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
                gone = held.drain(&queues, false);
            }
            assert_eq!(gone, vec![obj]);
            for raw in gone {
                release_obj(raw);
            }
            assert_eq!(drops.load(Ordering::Acquire), 1);
        }
    }
}
