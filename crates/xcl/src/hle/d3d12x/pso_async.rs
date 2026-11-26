//! Non-blocking pipeline creation. Translating a console shader takes milliseconds; compiling
//! the HLSL with DXC takes 0.2 s to seconds, and a title creates thousands of pipelines while
//! it loads. When a pipeline's DXIL is not in the disk cache yet, the title gets a stand-in
//! pipeline (shaders that draw nothing) at once and the real one is built on a worker; binding
//! (`SetPipelineState`, console list replay, the two-pass lookups) uses the real one as soon
//! as it is there. Until then a graphics pipeline draws nothing and a compute pipeline's
//! dispatches are skipped: a bind never builds a pipeline on the title's thread, and waits for
//! one only up to `XCL_PSO_WAIT_MS` (`resolve_wait`), so no dispatch is put off to the submit.
//! `XCL_ASYNC_PSO=0` builds every pipeline in the call, as before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

struct State {
    done: bool,
    orphan: bool,
    released: bool,
    /// the real pipeline state (raw pointer), 0 while building or when building failed
    real: usize,
    /// the driver pipeline binds get till then (`shared`); 0: the stand-in is one itself
    driver: usize,
}

struct Pending {
    /// the build, until a worker takes it
    job: Mutex<Option<Job>>,
    /// the build went to the workers (a lazy one goes on its first `resolve`)
    sent: std::sync::atomic::AtomicBool,
    state: Mutex<State>,
    cv: Condvar,
    compute: bool,
    /// of its shaders (`used`); 0 = none
    key: u64,
    /// it was used (`used::note` is told once)
    used: std::sync::atomic::AtomicBool,
    /// the stand-in was released before the real pipeline was built
    orphan: std::sync::atomic::AtomicBool,
    /// a bind went on without it (past `wait_budget`, or with none): binds do not wait for it
    /// until it is built
    late: std::sync::atomic::AtomicBool,
    /// its dispatches may be left out while it is built (`image_only`): binds do not wait for it
    spared: std::sync::atomic::AtomicBool,
    demand: Mutex<Demand>,
}

/// Whether one of the title's threads waits for the pipeline, and the preload worker building it.
#[derive(Default)]
struct Demand {
    wanted: bool,
    /// a call put off to the submit needs it (`need_by_submit`)
    by_submit: bool,
    /// thread id of the preload worker (lowest priority) building it, 0 = none
    preload_thread: u32,
}

thread_local! {
    /// what this worker is building
    static BUILDING: std::cell::RefCell<Option<Arc<Pending>>> = const { std::cell::RefCell::new(None) };
    /// a build on this thread that is not a worker's: whether one of the title's threads waits for it (`build_waited_on`)
    static WAITED: std::cell::RefCell<Option<Box<dyn Fn() -> bool>>> = const { std::cell::RefCell::new(None) };
}

/// This thread is a worker and one of the title's threads waits for what it is building: the
/// build must not stand back for the title's sake (`real::big_compile_turn`). The same for a
/// build in `build_waited_on` once its `waited` says so.
pub(super) fn build_is_wanted() -> bool {
    BUILDING.with(|b| b.borrow().as_ref().is_some_and(|p| p.demand.lock().unwrap().wanted))
        || WAITED.with(|w| w.borrow().as_ref().is_some_and(|f| f()))
}

/// `build` on a thread that is not a worker, for a pipeline the title may come to wait for: while
/// `waited` says one of the title's threads does, `build_is_wanted` says so on this thread.
pub(super) fn build_waited_on<R>(waited: impl Fn() -> bool + 'static, build: impl FnOnce() -> R) -> R {
    WAITED.with(|w| *w.borrow_mut() = Some(Box::new(waited)));
    let r = build();
    WAITED.with(|w| *w.borrow_mut() = None);
    r
}

/// Whether one of the title's threads waits for what this thread is doing, to be asked later and
/// from another thread: a worker's build as `build_is_wanted`; a thread that is not XCL's is the
/// title's own.
pub fn wanted_here() -> crate::gpu::recomp::dxc::Wanted {
    if let Some(p) = BUILDING.with(|b| b.borrow().clone()) {
        return Box::new(move || p.demand.lock().unwrap().wanted);
    }
    let own = crate::state::boot().is_some() && !std::thread::current().name().is_some_and(|n| n.starts_with("xcl-"));
    Box::new(move || own)
}

/// A worker's build of `p` (`preload`: by one of the lowest-priority workers).
fn build(p: &Arc<Pending>, job: Job, preload: bool) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL};
    BUILDING.with(|b| *b.borrow_mut() = Some(p.clone()));
    if preload {
        let mut d = p.demand.lock().unwrap();
        d.preload_thread = unsafe { GetCurrentThreadId() };
        if d.wanted {
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL) };
        }
    }
    // (a panic in a build ends it as a failed one: whoever waits for it is not left waiting)
    complete(p, std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or(0));
    if preload {
        // (under the lock: `want` does not raise this thread after it)
        let mut d = p.demand.lock().unwrap();
        d.preload_thread = 0;
        if d.wanted {
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
        }
    }
    BUILDING.with(|b| *b.borrow_mut() = None);
}

impl Pending {
    /// One of the title's threads is about to wait for it: the worker building it goes on now,
    /// at a normal priority.
    fn want(&self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenThread, SetThreadPriority, THREAD_PRIORITY_NORMAL, THREAD_SET_LIMITED_INFORMATION};
        let mut d = self.demand.lock().unwrap();
        if !std::mem::replace(&mut d.wanted, true) && d.preload_thread != 0 {
            unsafe {
                let h = OpenThread(THREAD_SET_LIMITED_INFORMATION, 0, d.preload_thread);
                if !h.is_null() {
                    SetThreadPriority(h, THREAD_PRIORITY_NORMAL);
                    CloseHandle(h);
                }
            }
        }
    }

    fn note_use(&self) {
        if self.key != 0 && !self.used.swap(true, Ordering::Relaxed) {
            used::note(self.key);
        }
    }

    /// Mark the stand-in released under `st`, take its built pipeline if done (or mark it
    /// orphaned so any worker drops its result), and wake waiters.
    fn finish_release(&self, st: &mut State) -> (bool, usize) {
        st.released = true;
        let real = if st.done {
            std::mem::take(&mut st.real)
        } else {
            // the worker stores its result into a Pending nobody will read: mark it orphaned
            st.orphan = true;
            0
        };
        self.cv.notify_all();
        (st.orphan, real)
    }
}

/// stand-in handed to the title → its real pipeline
static PENDING: Mutex<Option<HashMap<usize, Arc<Pending>>>> = Mutex::new(None);

/// The build is done (`real`: its pipeline, 0 = failed).
fn complete(p: &Pending, real: usize) {
    if real == 0 {
        FAILED.fetch_add(1, Ordering::Relaxed);
    } else {
        BUILT.fetch_add(1, Ordering::Relaxed);
    }
    let orphan = {
        let mut st = p.state.lock().unwrap();
        let orphan = st.orphan;
        if !orphan {
            st.real = real;
        }
        st.done = true;
        p.cv.notify_all();
        orphan
    };
    // outside `state`: the Release override takes PENDING (`counts` takes PENDING, then `state`)
    if orphan {
        release(real);
    }
}
static BUILT: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);
/// binds waiting right now for a pipeline still being built (`title_held`)
static HELD: AtomicUsize = AtomicUsize::new(0);

/// The PC calls of the title's threads (`pc_call`): the ones going on, and how long the ones
/// that ended took since the last frame was shown.
struct PcCalls {
    /// (id, when it began)
    now: Vec<(u64, std::time::Instant)>,
    /// the frames shown when `spent` began to count (`None` before the first observation)
    frame: Option<u64>,
    /// when the latest frame advance was first observed; calls that started earlier are clamped here
    frame_since: Option<std::time::Instant>,
    /// the time of the PC calls that ended since then
    spent: std::time::Duration,
}

impl PcCalls {
    const fn new() -> Self {
        Self { now: Vec::new(), frame: None, frame_since: None, spent: std::time::Duration::ZERO }
    }

    /// `frame` frames have been shown: the calls before the last of them are not this wait's.
    fn at(&mut self, frame: u64, now: std::time::Instant) {
        match self.frame {
            None => self.frame = Some(frame),
            Some(prev) if frame > prev => {
                self.frame = Some(frame);
                self.frame_since = Some(now);
                self.spent = std::time::Duration::ZERO;
            }
            Some(_) => {}
        }
    }

    fn clamp(&self, since: std::time::Instant) -> std::time::Instant {
        self.frame_since.map_or(since, |f| since.max(f))
    }

    fn begin(&mut self, id: u64, frame: u64, now: std::time::Instant) {
        self.at(frame, now);
        self.now.push((id, now));
    }

    fn end(&mut self, id: u64, frame: u64, now: std::time::Instant) {
        self.at(frame, now);
        if let Some(i) = self.now.iter().position(|c| c.0 == id) {
            let (_, since) = self.now.swap_remove(i);
            self.spent += now.saturating_duration_since(self.clamp(since));
        }
    }

    /// Whether a call going on holds the title: the PC calls since the last frame, it included,
    /// have taken `PC_CALL_HOLDS_AFTER`.
    fn holds(&mut self, frame: u64, now: std::time::Instant) -> bool {
        self.at(frame, now);
        let spent = self.spent;
        self.now.iter().any(|(_, since)| spent + now.saturating_duration_since(self.clamp(*since)) >= PC_CALL_HOLDS_AFTER)
    }
}

static PC_CALLS: Mutex<PcCalls> = Mutex::new(PcCalls::new());
/// From this much time in PC calls since the last frame on, a PC call holds the title: the
/// console's calls return at once.
const PC_CALL_HOLDS_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether one of the title's threads is held where a console title never waits: a bind is
/// waiting for a pipeline still being built, or a PC call is going on after such calls have
/// kept the title's threads for long (`pc_call`). `kernelx::Sleep` does not count that time.
pub fn title_held() -> bool {
    HELD.load(Ordering::Acquire) != 0 || held_by_pc_call()
}

fn held_by_pc_call() -> bool {
    PC_CALLS.lock().unwrap().holds(super::present::presents(), std::time::Instant::now())
}

/// A call of the title into D3D12 that XCL or the PC's driver can keep for long where the
/// console's takes no time: a submission (XCL's uploads and tile mappings before the lists, the
/// driver's compiling in them), a texture's placement, a pipeline built in the call. While it
/// lasts the title is held (`title_held`), once the PC calls since the last frame shown, this
/// one included, have taken `PC_CALL_HOLDS_AFTER`: one long call, or many short ones in a row
/// (a retail title placed 16 384 textures in 29 s without a frame, 1 - 50 ms each). A title
/// that shows its frames is not held by its submissions.
pub struct PcCall(u64);

pub fn pc_call() -> PcCall {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    PC_CALLS.lock().unwrap().begin(id, super::present::presents(), std::time::Instant::now());
    PcCall(id)
}

impl Drop for PcCall {
    fn drop(&mut self) {
        PC_CALLS.lock().unwrap().end(self.0, super::present::presents(), std::time::Instant::now());
    }
}

/// Whether this thread is building a pipeline for `build` (a worker, not one of the title's).
pub(super) fn building_here() -> bool {
    BUILDING.with(|b| b.borrow().is_some())
}

/// A bind's wait for a pipeline still being built (`title_held`).
struct Held;

impl Held {
    fn new() -> Held {
        HELD.fetch_add(1, Ordering::AcqRel);
        Held
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn enabled() -> bool {
    std::env::var("XCL_ASYNC_PSO").map_or(true, |v| v != "0")
}

/// Builds the real pipeline: its raw pointer, 0 = failed.
pub type Job = Box<dyn FnOnce() -> usize + Send>;

/// pipelines a submit will wait for: the workers take these before their queue (`need_by_submit`)
static NEEDED: Mutex<std::collections::VecDeque<Arc<Pending>>> = Mutex::new(std::collections::VecDeque::new());

fn next_needed() -> Option<Arc<Pending>> {
    NEEDED.lock().unwrap().pop_front()
}

/// Drop an unstarted orphaned job outside all locks and wake any concurrent waiter.
fn drop_unstarted_job(p: &Pending) {
    let job = p.job.lock().unwrap().take();
    if let Some(job) = job {
        {
            let mut st = p.state.lock().unwrap();
            st.done = true;
            p.cv.notify_all();
        }
        drop(job);
    }
}

type Work = Box<dyn FnOnce() + Send>;

fn queue() -> &'static Mutex<mpsc::Sender<Work>> {
    static Q: OnceLock<Mutex<mpsc::Sender<Work>>> = OnceLock::new();
    Q.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Work>();
        let rx = Arc::new(Mutex::new(rx));
        // translation recurses deeply on big programs; DXC itself runs on its own pool
        let n = crate::gpu::recomp::dxc::compile_threads();
        for k in 0..n {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("xcl-pso-{k}")).stack_size(64 << 20).spawn(move || loop {
                let next = rx.lock().unwrap().recv();
                let Ok(work) = next else { return };
                while let Some(p) = next_needed() {
                    build_queued(&p);
                }
                work();
            });
        }
        Mutex::new(tx)
    })
}

fn enqueue(p: Arc<Pending>) {
    let _ = queue().lock().unwrap().send(Box::new(move || build_queued(&p)));
}

fn build_queued(p: &Arc<Pending>) {
    // (released by the title meanwhile)
    if p.state.lock().unwrap().orphan {
        drop_unstarted_job(p);
        return;
    }
    // (None: a preload worker took it)
    let job = p.job.lock().unwrap().take();
    if let Some(job) = job {
        build(p, job, false);
    }
}

/// The caller also drains its batch. Multiple workers can therefore build
/// different chains without ever waiting for a free worker to start a piece.
pub(super) fn build_pieces(jobs: Vec<Job>) -> Vec<usize> {
    let count = jobs.len();
    let jobs = Arc::new(Mutex::new(jobs.into_iter().enumerate().collect::<Vec<_>>()));
    let (send, recv) = mpsc::channel();
    let drain = move || loop {
        let job = jobs.lock().unwrap().pop();
        let Some((i, job)) = job else { return };
        let _ = send.send((i, job()));
    };
    // the helpers build for this worker's pipeline: the title waiting for it waits for them too
    let parent = BUILDING.with(|b| b.borrow().clone());
    for _ in 1..count.min(crate::gpu::recomp::dxc::compile_threads()) {
        let (drain, parent) = (drain.clone(), parent.clone());
        let _ = queue().lock().unwrap().send(Box::new(move || {
            let before = BUILDING.with(|b| b.replace(parent));
            drain();
            BUILDING.with(|b| *b.borrow_mut() = before);
        }));
    }
    drain();
    let mut results = vec![0; count];
    for (i, result) in recv.iter().take(count) { results[i] = result; }
    results
}

/// Release a pipeline state XCL owns (raw pointer, 0 = none).
pub fn release(p: usize) {
    if p == 0 {
        return;
    }
    unsafe {
        if !is_com_pso(p) {
            return;
        }
        if super::late_release::enabled() && !super::real::is_adopted(p as *mut std::ffi::c_void) {
            let raw = p as *mut std::ffi::c_void;
            let vt = *(raw as *const *const usize);
            let add: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 = std::mem::transmute(*vt.add(1));
            let rel: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 = std::mem::transmute(*vt.add(2));
            if add(raw) == 2 {
                rel(raw);
                super::late_release::hold_deferred(p);
                return;
            }
            rel(raw);
            rel(raw);
            return;
        }
        drop(<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState as windows::core::Interface>::from_raw(p as *mut std::ffi::c_void));
    }
}

/// `standin` went to the title; `job` builds the real pipeline on a worker.
pub fn submit(standin: usize, compute: bool, job: Job) {
    submit_as(standin, compute, job, true, 0);
}

/// Lazy pipeline creation (`XCL_LAZY_PSO=0`: off): a title's pipelines with console shaders get
/// their stand-in when made and are built when first used – the first bind of one sends it to
/// the workers. A retail game makes ~51 000 pipelines in its first 30 s and uses few; each one
/// built stays in the driver (~400 KB), and the process ran out of commit.
pub fn lazy() -> bool {
    static L: OnceLock<bool> = OnceLock::new();
    *L.get_or_init(|| enabled() && std::env::var("XCL_LAZY_PSO").map_or(true, |v| v != "0"))
}

/// `submit`, sent to the workers only on its first `resolve` (`lazy`) – or now, at a low
/// priority, when an earlier run used a pipeline of these shaders (`key`, 0 = none; `used`).
pub fn submit_lazy(standin: usize, compute: bool, job: Job, key: u64) {
    submit_as(standin, compute, job, false, key);
}

fn submit_as(standin: usize, compute: bool, job: Job, send: bool, key: u64) {
    let p = Arc::new(Pending {
        job: Mutex::new(Some(job)),
        sent: std::sync::atomic::AtomicBool::new(send),
        state: Mutex::new(State { done: false, orphan: false, released: false, real: 0, driver: 0 }),
        cv: Condvar::new(),
        compute,
        key,
        used: Default::default(),
        late: Default::default(),
        spared: Default::default(),
        demand: Default::default(),
    });
    PENDING.lock().unwrap().get_or_insert_with(HashMap::new).insert(standin, p.clone());
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::xlog_info!("D3D12.X: pipelines whose shaders are not compiled yet get a stand-in and are built in the background");
    });
    if send {
        enqueue(p);
    } else if used::known(key) {
        if let Some(q) = preload() {
            q.list.lock().unwrap().push_back(p);
            q.cv.notify_one();
        }
    }
}

/// The keys of the pipelines earlier runs used (`used-pipelines.txt` beside the DXIL cache):
/// what `xcl shaders --precompile --used-only` builds.
pub fn earlier_used_keys() -> std::collections::HashSet<u64> {
    used::parse(&std::fs::read_to_string(crate::gpu::recomp::dxc::cache_dir().join("used-pipelines.txt")).unwrap_or_default())
}

/// The pipelines a title used, by a hash of their shaders, kept between runs
/// (`used-pipelines.txt` beside the DXIL cache, a key a line).
///
/// A lazy pipeline is built at its first use: until then a graphics one draws nothing, and a
/// compute one's dispatches are skipped (after a wait of up to `XCL_PSO_WAIT_MS`). A retail
/// title's first frames of its world use hundreds of pipelines for the first time at once. The
/// pipelines of an earlier run are built while the title loads instead (`preload`): measured on
/// a build that skips a dispatch instead of waiting for its pipeline, ~2 170 dispatches were
/// skipped in those frames without this and 94 – 150 with it.
mod used {
    use std::collections::HashSet;
    use std::io::Write;
    use std::sync::{Mutex, OnceLock, RwLock};

    struct Used {
        earlier: RwLock<HashSet<u64>>,
        now: Mutex<(HashSet<u64>, Option<std::fs::File>)>,
    }

    /// The keys in a `used-pipelines.txt` (lines that are not a hexadecimal key are passed over).
    pub fn parse(text: &str) -> HashSet<u64> {
        text.lines().filter_map(|l| u64::from_str_radix(l.trim(), 16).ok()).filter(|&k| k != 0).collect()
    }

    fn get() -> &'static Used {
        static U: OnceLock<Used> = OnceLock::new();
        U.get_or_init(|| {
            // (a test neither reads nor writes the user's file)
            if cfg!(test) {
                return Used { earlier: RwLock::new(HashSet::new()), now: Mutex::new((HashSet::new(), None)) };
            }
            let path = crate::gpu::recomp::dxc::cache_dir().join("used-pipelines.txt");
            let earlier = parse(&std::fs::read_to_string(&path).unwrap_or_default());
            if !earlier.is_empty() {
                crate::xlog_info!("D3D12.X: the pipelines of {} shader sets an earlier run used are built ahead of their first use", earlier.len());
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
            Used { now: Mutex::new((earlier.clone(), file)), earlier: RwLock::new(earlier) }
        })
    }

    /// An earlier run used a pipeline of these shaders.
    pub fn known(key: u64) -> bool {
        key != 0 && get().earlier.read().unwrap().contains(&key)
    }

    /// A pipeline of these shaders is used now: kept for the next run.
    pub fn note(key: u64) {
        if key == 0 {
            return;
        }
        let mut g = get().now.lock().unwrap();
        if g.0.insert(key) {
            if let Some(f) = g.1.as_mut() {
                let _ = writeln!(f, "{key:016x}");
            }
        }
    }

    /// (tests) This run used a pipeline of these shaders.
    #[cfg(test)]
    pub fn noted(key: u64) -> bool {
        get().now.lock().unwrap().0.contains(&key)
    }

    #[cfg(test)]
    pub fn earlier_run_used(key: u64) {
        get().earlier.write().unwrap().insert(key);
    }
}

/// The workers that build the pipelines of an earlier run ahead of their first use
/// (`XCL_PSO_PRELOAD=0`: off). Apart from the ones a bind sends to (`queue`) and at the lowest
/// priority: a pipeline that is bound is not held up by the ones made ahead (whoever takes
/// `job` first builds it), and the title's own threads are not either.
struct Preload {
    list: Mutex<std::collections::VecDeque<Arc<Pending>>>,
    cv: Condvar,
}

fn preload() -> Option<&'static Preload> {
    static Q: OnceLock<Option<Preload>> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let q = Q
        .get_or_init(|| {
            if std::env::var("XCL_PSO_PRELOAD").is_ok_and(|v| v.trim() == "0") {
                return None;
            }
            Some(Preload { list: Mutex::new(Default::default()), cv: Condvar::new() })
        })
        .as_ref()?;
    STARTED.call_once(|| {
        let n = (crate::gpu::recomp::dxc::compile_threads() / 4).clamp(2, 4);
        for k in 0..n {
            let _ = std::thread::Builder::new().name(format!("xcl-pso-pre-{k}")).stack_size(64 << 20).spawn(move || {
                unsafe {
                    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST};
                    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
                }
                static DONE: AtomicUsize = AtomicUsize::new(0);
                loop {
                    let p = {
                        let mut g = q.list.lock().unwrap();
                        loop {
                            if let Some(p) = g.pop_front() {
                                break p;
                            }
                            g = q.cv.wait(g).unwrap();
                        }
                    };
                    // (released by the title meanwhile)
                    if p.state.lock().unwrap().orphan {
                        drop_unstarted_job(&p);
                        continue;
                    }
                    // (None: a worker a bind sent it to took it)
                    let job = p.job.lock().unwrap().take();
                    if let Some(job) = job {
                        build(&p, job, true);
                        let n = DONE.fetch_add(1, Ordering::Relaxed) + 1;
                        if n.is_power_of_two() {
                            crate::xlog_info!("D3D12.X: {n} pipelines built ahead of their first use");
                        }
                    }
                }
            });
        }
    });
    Some(q)
}

#[cfg(test)]
mod used_tests {
    use super::*;

    /// A PC call that has kept one of the title's threads for long holds the title, a short one
    /// does not, and the hold ends with the call.
    #[test]
    fn a_long_pc_call_holds_the_title() {
        let mut calls = PcCalls::new();
        let began = std::time::Instant::now();
        calls.begin(1, 0, began);
        calls.begin(2, 0, began + 2 * PC_CALL_HOLDS_AFTER);
        assert!(!calls.holds(0, began), "a call that has just begun holds the title");
        let later = began + 2 * PC_CALL_HOLDS_AFTER;
        assert!(calls.holds(0, later), "a PC call has lasted {:?}, but the title is not held", 2 * PC_CALL_HOLDS_AFTER);
        calls.end(1, 0, later);
        calls.end(2, 0, later);
        assert!(!calls.holds(0, later), "the calls are over, the title is still held");
    }

    /// Short PC calls add up between two frames: none of them is long, but after many in a row
    /// the one going on holds the title. The title's own time between them is not held, and a
    /// frame shown ends it.
    #[test]
    fn short_pc_calls_between_two_frames_hold_the_title() {
        let mut calls = PcCalls::new();
        let step = PC_CALL_HOLDS_AFTER / 10;
        let mut now = std::time::Instant::now();
        for id in 0..20 {
            calls.begin(id, 7, now);
            assert!(id >= 9 || !calls.holds(7, now + step), "{id} short calls hold the title already");
            now += step;
            calls.end(id, 7, now);
        }
        assert!(!calls.holds(7, now), "no PC call is going on, the title is held");
        calls.begin(20, 7, now);
        assert!(calls.holds(7, now), "twenty short PC calls since the last frame and one going on, but the title is not held");
        assert!(!calls.holds(8, now), "a frame was shown, the calls before it still hold the title");
        calls.end(20, 8, now);
    }

    /// While a bind waits (within its budget) for a pipeline still being built, the title is held
    /// (`title_held`): `kernelx::Sleep` does not count that time.
    #[test]
    fn a_bind_waiting_for_a_build_holds_the_title() {
        let standin = 0x7E57_B002usize;
        let (started, building) = mpsc::channel();
        let (go, finish) = mpsc::channel::<()>();
        submit_lazy(standin, true, Box::new(move || {
            let _ = started.send(());
            let _ = finish.recv();
            0x5252
        }), 0);
        let bind = std::thread::spawn(move || resolve_wait_for(standin, Some(std::time::Duration::from_secs(30))));
        building.recv_timeout(std::time::Duration::from_secs(10)).expect("the pipeline is being built");
        // (built by a worker, with the bind about to wait for it)
        let t = std::time::Instant::now();
        while !title_held() && t.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(title_held(), "a bind waits for a build, but the title is not held");
        go.send(()).unwrap();
        assert_eq!(bind.join().unwrap(), 0x5252);
        let _ = released(standin);
    }

    /// A stand-in that is no driver object (`shared`) never reaches the driver: binds get the
    /// driver pipeline shared with other stand-ins until the real one is built, and after a build
    /// that failed.
    #[test]
    fn a_bind_of_a_shared_stand_in_gets_its_driver_pipeline() {
        let (fails, builds, driver) = (0x7E57_B011usize, 0x7E57_B012usize, 0x7E57_B013usize);
        submit_lazy(fails, false, Box::new(|| 0), 0);
        submit_lazy(builds, false, Box::new(|| 0x1234), 0);
        for s in [fails, builds] {
            shared(s, driver);
            assert_eq!(entry(s).unwrap().stand(s), driver);
        }
        assert_eq!(resolve_wait_for(fails, None), driver, "not built yet");
        assert_eq!(wait_built(fails), driver, "its build failed");
        assert_eq!(resolve(fails), driver);
        assert_eq!(resolve_wait_for(builds, None), driver, "not built yet");
        assert_eq!(wait_built(builds), 0x1234);
        assert_eq!(resolve_for_bind(builds), 0x1234);
        // (0x1234 is no pipeline: taken, not released)
        assert_eq!((released(fails), released(builds)), (0, 0x1234));
    }

    /// A bind does not wait for a compute pipeline that writes only images: it gets the stand-in
    /// at once, with no budget, the build goes to the workers
    /// unwanted, and once built the bind gets the real one. Without `image_only` a bind with a
    /// budget waits for it.
    #[test]
    fn a_bind_does_not_wait_for_a_pipeline_that_writes_only_images() {
        let (spared, waited) = (0x7E57_B0A1usize, 0x7E57_B0A2usize);
        let (go, finish) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel();
        submit_lazy(spared, true, Box::new(move || {
            let _ = tx.send(wanted_here()());
            let _ = finish.recv();
            0x5A5A
        }), 0);
        image_only(spared);
        // (on a thread: a bind that waits would wait for `go`, below)
        let (bound_tx, bound) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = bound_tx.send((resolve_wait_for(spared, None), resolve_wait_for(spared, None)));
        });
        let binds = bound.recv_timeout(std::time::Duration::from_secs(5));
        if binds.is_err() {
            let _ = go.send(());
        }
        assert_eq!(binds.expect("the binds waited for the build"), (spared, spared), "still being built: the stand-in");
        assert!(is_pending(spared));
        assert!(!rx.recv_timeout(std::time::Duration::from_secs(10)).expect("a worker builds it"), "nobody waits: the build is not wanted");
        go.send(()).unwrap();
        let start = std::time::Instant::now();
        while is_pending(spared) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(resolve_wait_for(spared, None), 0x5A5A, "built by a worker");
        // the same build without the mark: a bind with a budget waits for it
        submit_lazy(waited, true, Box::new(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            0x5B5B
        }), 0);
        assert_eq!(resolve_wait_for(waited, Some(std::time::Duration::from_secs(30))), 0x5B5B);
        let _ = released(spared);
        let _ = released(waited);
    }

    #[test]
    fn a_pipeline_a_submit_needs_is_built_before_the_queue() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let backlog = 8 * crate::gpu::recomp::dxc::compile_threads();
        let standins: Vec<usize> = (0..=backlog).map(|i| 0x7E57_E000usize + i).collect();
        for &standin in &standins {
            let order = order.clone();
            submit(standin, true, Box::new(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                order.lock().unwrap().push(standin);
                0x6161
            }));
        }
        let last = *standins.last().unwrap();
        need_by_submit(last);
        let start = std::time::Instant::now();
        while standins.iter().any(|&s| is_pending(s)) && start.elapsed() < std::time::Duration::from_secs(30) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let order = order.lock().unwrap();
        assert_eq!(order.len(), standins.len());
        let at = order.iter().position(|&s| s == last).unwrap();
        assert!(at < order.len() / 2, "made last and needed by a submit, it was built {at} of {}", order.len());
        for &standin in &standins {
            let _ = released(standin);
        }
    }

    /// What a bind got before its pipeline was built stays "not built" for it after the build
    /// ends (`bound_unbuilt`), where `is_pending` already says built: `ExecuteIndirect` skipped
    /// on that and sent the stand-in to the PC call. A failed build's stand-in and a pipeline that
    /// is no stand-in are not unbuilt.
    #[test]
    fn a_stand_in_bound_before_its_build_ended_stays_unbuilt_for_that_bind() {
        let (standin, failed) = (0x7E57_B0D1usize, 0x7E57_B0D2usize);
        let (go, finish) = mpsc::channel::<()>();
        submit_lazy(standin, true, Box::new(move || {
            let _ = finish.recv();
            0x6161
        }), 0);
        // (a bind that does not wait: the stand-in while it is built)
        image_only(standin);
        let bound = resolve_wait_for(standin, None);
        assert_eq!(bound, standin);
        assert!(is_pending(standin) && bound_unbuilt(standin, bound));
        go.send(()).unwrap();
        let start = std::time::Instant::now();
        while is_pending(standin) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(resolve(standin), 0x6161);
        assert!(!is_pending(standin), "built");
        assert!(bound_unbuilt(standin, bound), "the bind got the stand-in, not the pipeline built after it");
        assert!(!bound_unbuilt(standin, resolve_wait_for(standin, None)), "a bind after the build gets the real pipeline");
        submit(failed, true, Box::new(|| 0));
        let start = std::time::Instant::now();
        while is_pending(failed) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(resolve(failed), failed);
        assert!(!bound_unbuilt(failed, failed), "a failed build's stand-in is not waiting for a build");
        assert!(!bound_unbuilt(0x7E57_B0D3, 0x7E57_B0D3), "no stand-in");
        let _ = released(standin);
        let _ = released(failed);
    }

    /// A bind waits for a pipeline still being built no longer than its budget and goes on with
    /// the stand-in; later binds do not wait for it again; once built it is the real one.
    #[test]
    fn a_bind_waits_for_a_slow_pipeline_no_longer_than_its_budget() {
        let standin = 0x7E57_B001usize;
        submit(standin, true, Box::new(|| {
            std::thread::sleep(std::time::Duration::from_millis(600));
            0x5151
        }));
        let budget = Some(std::time::Duration::from_millis(30));
        let t = std::time::Instant::now();
        assert_eq!(resolve_wait_for(standin, budget), standin, "not built within the budget: the stand-in");
        assert!(t.elapsed() < std::time::Duration::from_millis(400), "waited {:?}", t.elapsed());
        let t = std::time::Instant::now();
        assert_eq!(resolve_wait_for(standin, budget), standin);
        assert!(t.elapsed() < std::time::Duration::from_millis(20), "a late pipeline is not waited for again ({:?})", t.elapsed());
        let start = std::time::Instant::now();
        while is_pending(standin) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(resolve_wait_for(standin, budget), 0x5151, "built by a worker");
        let _ = released(standin);
    }

    /// A bind with no budget neither builds a cold pipeline on its own thread nor waits for it,
    /// compute or graphics: it gets the stand-in at once, a worker builds the pipeline wanted, and
    /// once built binds get the real one.
    #[test]
    fn a_bind_with_no_budget_neither_builds_nor_waits_for_the_pipeline() {
        for (standin, compute, real) in [(0x7E57_B0C1usize, true, 0x5C5Cusize), (0x7E57_B0C2, false, 0x5D5D)] {
            let (go, finish) = mpsc::channel::<()>();
            let (tx, rx) = mpsc::channel();
            submit_lazy(standin, compute, Box::new(move || {
                let _ = tx.send((std::thread::current().name().map(str::to_owned), wanted_here()()));
                let _ = finish.recv();
                real
            }), 0);
            // (on a thread: a bind that builds or waits would wait for `go`, below)
            let (bound_tx, bound) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = bound_tx.send((resolve_wait_for(standin, None), resolve_wait_for(standin, None)));
            });
            let binds = bound.recv_timeout(std::time::Duration::from_secs(5));
            if binds.is_err() {
                let _ = go.send(());
            }
            assert_eq!(binds.expect("the binds built or waited for the pipeline"), (standin, standin), "still being built: the stand-in");
            let (thread, wanted) = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("a worker builds it");
            assert!(thread.as_deref().is_some_and(|n| n.starts_with("xcl-pso")), "built on {thread:?}, not on a worker");
            assert!(wanted, "the title binds it: its build is wanted");
            assert!(is_pending(standin));
            go.send(()).unwrap();
            assert_eq!(wait_built(standin), real);
            assert_eq!(resolve_wait_for(standin, None), real, "built: the real one");
            assert!(!is_pending(standin));
            let _ = released(standin);
        }
    }

    /// A compile a worker's build starts is wanted once a bind waits for the build – asked from
    /// another thread, as the DXC workers do; a thread that builds nothing (no title runs here)
    /// wants nothing.
    #[test]
    fn a_waiting_bind_makes_the_builds_compiles_wanted() {
        let standin = 0x7E57_B005usize;
        let (tx, rx) = mpsc::channel();
        submit(standin, true, Box::new(move || {
            tx.send(wanted_here()).unwrap();
            let t = std::time::Instant::now();
            while !build_is_wanted() && t.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            0x5454
        }));
        let wanted = rx.recv().unwrap();
        assert!(!wanted(), "nobody waits yet");
        assert_eq!(resolve_wait_for(standin, Some(std::time::Duration::from_secs(30))), 0x5454);
        assert!(wanted());
        assert!(!wanted_here()());
        let _ = released(standin);
    }

    #[test]
    fn a_build_a_submit_needs_is_wanted() {
        let standin = 0x7E57_B006usize;
        let (started_tx, started) = mpsc::channel();
        submit(standin, true, Box::new(move || {
            started_tx.send(()).unwrap();
            let t = std::time::Instant::now();
            while !build_is_wanted() && t.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if build_is_wanted() { 0x5757 } else { 0 }
        }));
        started.recv().unwrap();
        need_by_submit(standin);
        let t = std::time::Instant::now();
        while is_pending(standin) && t.elapsed() < std::time::Duration::from_secs(15) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(resolve(standin), 0x5757, "the build did not hear that a submit waits for it");
        let _ = released(standin);
    }

    /// A worker's build hears that a bind waits for it (and only its own build does).
    #[test]
    fn a_waiting_bind_makes_the_build_wanted() {
        let standin = 0x7E57_B004usize;
        let (started_tx, started) = mpsc::channel();
        submit(standin, true, Box::new(move || {
            let unwanted_first = !build_is_wanted();
            started_tx.send(()).unwrap();
            let t = std::time::Instant::now();
            while !build_is_wanted() && t.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if unwanted_first && build_is_wanted() { 0x5353 } else { 0 }
        }));
        started.recv().unwrap();
        assert!(!build_is_wanted(), "this thread builds nothing");
        assert_eq!(resolve_wait_for(standin, Some(std::time::Duration::from_secs(30))), 0x5353, "the bind's wait reached the build");
        let _ = released(standin);
    }

    #[test]
    fn the_keys_of_a_file_are_read_and_other_lines_passed_over() {
        let keys = used::parse("00000000000000ff\n\nnot a key\n  1a2b \n0\n");
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&0xff) && keys.contains(&0x1a2b));
    }

    /// A lazy pipeline of shaders an earlier run used is built without a bind; another one is
    /// built only at its first use, and that use is kept for the next run.
    #[test]
    fn a_pipeline_an_earlier_run_used_is_built_ahead_of_its_first_use() {
        let (known, unknown) = (0x7E57_7001usize, 0x7E57_7002usize);
        let (known_key, unknown_key) = (0x5EED_0001u64, 0x5EED_0002u64);
        used::earlier_run_used(known_key);
        submit_lazy(known, false, Box::new(|| 0x4321), known_key);
        submit_lazy(unknown, false, Box::new(|| 0x4322), unknown_key);
        let start = std::time::Instant::now();
        while is_pending(known) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!is_pending(known), "built by the workers that build ahead, with no bind");
        assert_eq!(resolve(known), 0x4321);
        assert!(is_pending(unknown), "nobody used it and no earlier run did");
        assert!(!used::noted(unknown_key));
        assert_eq!(resolve_wait_for(unknown, None), unknown, "a bind does not wait for it");
        assert_eq!(wait_built(unknown), 0x4322);
        assert!(used::noted(unknown_key) && used::noted(known_key), "a first use is kept for the next run");
        released(known);
        released(unknown);
    }

    static WORKER_SATURATION_LOCK: Mutex<()> = Mutex::new(());

    /// A queued normal worker job whose stand-in was released before a worker starts it is
    /// skipped as orphaned instead of entering the compiler.
    #[test]
    fn a_released_queued_pipeline_is_not_built_by_normal_workers() {
        let _sat = WORKER_SATURATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let n = crate::gpu::recomp::dxc::compile_threads();
        let (started_tx, started_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut blockers = Vec::with_capacity(n);
        for k in 0..n {
            let standin = 0x7E57_9720usize + k;
            let started_tx = started_tx.clone();
            let gate = gate.clone();
            submit(standin, false, Box::new(move || {
                let _ = started_tx.send(());
                let mut open = gate.0.lock().unwrap();
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
                0x9720 + k
            }));
            blockers.push(standin);
        }
        for _ in 0..n {
            started_rx.recv_timeout(Duration::from_secs(10)).expect("normal workers saturated");
        }

        let target = 0x7E57_97FFusize;
        let ran = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let ran_job = ran.clone();
        let drop_flag = DropFlag(dropped.clone());
        submit(target, false, Box::new(move || {
            let _keep = &drop_flag;
            ran_job.store(true, Ordering::Release);
            0x9721
        }));
        assert_eq!(released(target), 0);

        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();

        let deadline = Instant::now() + Duration::from_secs(10);
        while !dropped.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(dropped.load(Ordering::Acquire), "queued orphan job was never drained");
        assert!(!ran.load(Ordering::Acquire), "released job reached the compiler");

        for standin in blockers {
            let start = Instant::now();
            while is_pending(standin) && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = released(standin);
        }
    }

    #[test]
    fn pc_call_crossing_frame_boundary_does_not_charge_prior_frame_time() {
        let mut calls = PcCalls::new();
        let t0 = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        calls.begin(1, 7, t0);
        // Frame 8 shown after 290 ms; call 1 ends 10 ms later -> only 10 ms counts on frame 8.
        let t_frame8 = t0 + PC_CALL_HOLDS_AFTER + ms(40);
        assert!(!calls.holds(8, t_frame8), "call from frame 7 must not immediately hold frame 8");
        calls.end(1, 8, t_frame8 + ms(10));
        let t_next = t_frame8 + ms(11);
        calls.begin(2, 8, t_next);
        assert!(!calls.holds(8, t_next), "pre-frame duration leaked into frame 8's budget");
        // A call genuinely stuck >= PC_CALL_HOLDS_AFTER after frame 8 still holds the title.
        assert!(!calls.holds(8, t_next + PC_CALL_HOLDS_AFTER - ms(20)));
        assert!(calls.holds(8, t_next + PC_CALL_HOLDS_AFTER));
        calls.end(2, 8, t_next + PC_CALL_HOLDS_AFTER);

        // Also when frame 9 is first noticed inside end() itself, pre-frame time is clamped to 0.
        let t_f9 = t_next + PC_CALL_HOLDS_AFTER + ms(10);
        calls.begin(3, 8, t_f9);
        let t_f9_end = t_f9 + PC_CALL_HOLDS_AFTER + ms(50);
        calls.end(3, 9, t_f9_end);
        calls.begin(4, 9, t_f9_end + ms(1));
        assert!(!calls.holds(9, t_f9_end + ms(1)), "end() on new frame must not charge prior-frame duration");
        calls.end(4, 9, t_f9_end + ms(1) + PC_CALL_HOLDS_AFTER);

        // A stale lower frame number must never rewind self.frame or wipe frame 9's spent budget.
        let t_stale = t_f9_end + PC_CALL_HOLDS_AFTER + ms(10);
        calls.begin(5, 8, t_stale);
        assert!(calls.holds(9, t_stale), "stale lower frame rewound frame 9 or cleared its spent budget");
        calls.end(5, 9, t_stale);
    }

    /// With `XCL_PSO_WAIT_MS`, a submit's wait for a late pipeline that no worker can start (all
    /// are busy) neither ends with the stand-in nor waits for a free worker: after the budget it
    /// builds the pipeline itself.
    #[test]
    fn a_submit_builds_a_late_pipeline_no_worker_has_started() {
        let _sat = WORKER_SATURATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::time::{Duration, Instant};
        let n = crate::gpu::recomp::dxc::compile_threads();
        let (started_tx, started_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut blockers = Vec::with_capacity(n);
        for k in 0..n {
            let standin = 0x7E57_A500usize + k;
            let (tx, g) = (started_tx.clone(), gate.clone());
            submit(standin, false, Box::new(move || {
                let _ = tx.send(());
                let mut open = g.0.lock().unwrap();
                while !*open {
                    open = g.1.wait(open).unwrap();
                }
                0xA500 + k
            }));
            blockers.push(standin);
        }
        for _ in 0..n {
            started_rx.recv_timeout(Duration::from_secs(10)).expect("workers saturated");
        }
        let target = 0x7E57_A5FFusize;
        submit(target, true, Box::new(|| 0xA5FF));
        set_wait_budget_for_test(Some(Duration::from_millis(30)));
        assert_eq!(resolve_wait(target), target, "a bind as the title records: past the budget, late");
        let t = Instant::now();
        wait_for_submit(target);
        let took = t.elapsed();
        set_wait_budget_for_test(None);
        let built = resolve(target);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        for standin in blockers {
            let start = Instant::now();
            while is_pending(standin) && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = released(standin);
        }
        let _ = released(target);
        assert_eq!(built, 0xA5FF, "the submit's wait ended without the pipeline");
        assert!(took < Duration::from_secs(5), "the submit waited for a free worker ({took:?})");
    }

    /// With `XCL_PSO_WAIT_MS`, a submit waits for a pipeline a worker is building at most
    /// `SUBMIT_WAIT_MOST`, then goes on (with the stand-in) and counts it.
    #[test]
    fn a_submit_waits_for_a_build_on_a_worker_no_longer_than_its_bound() {
        let _sat = WORKER_SATURATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::time::{Duration, Instant};
        let standin = 0x7E57_A600usize;
        let (started_tx, started_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let g = gate.clone();
        submit(standin, true, Box::new(move || {
            let _ = started_tx.send(());
            let mut open = g.0.lock().unwrap();
            while !*open {
                open = g.1.wait(open).unwrap();
            }
            0xA600
        }));
        started_rx.recv_timeout(Duration::from_secs(10)).expect("a worker builds it");
        set_wait_budget_for_test(Some(Duration::from_millis(20)));
        set_submit_wait_for_test(Some(Duration::from_millis(300)));
        let gave_up = SUBMIT_GAVE_UP.load(Ordering::Relaxed);
        let t = Instant::now();
        wait_for_submit(standin);
        let took = t.elapsed();
        let pending = is_pending(standin);
        let counted = SUBMIT_GAVE_UP.load(Ordering::Relaxed) > gave_up;
        set_wait_budget_for_test(None);
        set_submit_wait_for_test(None);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let start = Instant::now();
        while is_pending(standin) && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = released(standin);
        assert!(pending, "the build ended before the bound");
        assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(5), "the submit waited {took:?}");
        assert!(counted, "the submit that went on without its pipeline was not counted");
    }

    /// A build that panics on a worker ends as a failed one: binds and submits waiting for it go on.
    #[test]
    fn a_build_that_panics_on_a_worker_ends_as_failed() {
        use std::time::{Duration, Instant};
        let standin = 0x7E57_A700usize;
        submit(standin, true, Box::new(|| panic!("a build that panics (test)")));
        let start = Instant::now();
        while is_pending(standin) && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
        }
        let pending = is_pending(standin);
        let _ = released(standin);
        assert!(!pending, "the panicked build never ended");
    }

    #[test]
    fn released_queued_pipeline_drops_job_immediately_before_workers_drain() {
        let _sat = WORKER_SATURATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        struct OpenGate(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for OpenGate {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() = true;
                self.0 .1.notify_all();
            }
        }

        // Saturate normal workers and verify released() drops the queued job immediately.
        let n = crate::gpu::recomp::dxc::compile_threads();
        let (started_tx, started_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let open_normal = OpenGate(gate.clone());
        let mut blockers = Vec::with_capacity(n);
        for k in 0..n {
            let standin = 0x7E57_A100usize + k;
            let (tx, g) = (started_tx.clone(), gate.clone());
            submit(standin, false, Box::new(move || {
                let _ = tx.send(());
                let mut open = g.0.lock().unwrap();
                while !*open {
                    open = g.1.wait(open).unwrap();
                }
                0xA100 + k
            }));
            blockers.push(standin);
        }
        for _ in 0..n {
            started_rx.recv_timeout(Duration::from_secs(10)).expect("normal workers saturated");
        }
        let target_n = 0x7E57_A1FFusize;
        let (ran_n, dropped_n) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (ran_job, flag_n) = (ran_n.clone(), DropFlag(dropped_n.clone()));
        submit(target_n, false, Box::new(move || {
            let _keep = &flag_n;
            ran_job.store(true, Ordering::Release);
            0xA1FF
        }));
        assert_eq!(released(target_n), 0);
        assert!(dropped_n.load(Ordering::Acquire), "normal queued job was not dropped immediately on released()");
        drop(open_normal);
        assert!(!ran_n.load(Ordering::Acquire));
        for standin in blockers {
            let start = Instant::now();
            while is_pending(standin) && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = released(standin);
        }

        // Saturate preload workers and verify released() drops the preloaded job immediately.
        let m = (crate::gpu::recomp::dxc::compile_threads() / 4).clamp(2, 4);
        let (p_tx, p_rx) = mpsc::channel();
        let p_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let open_pre = OpenGate(p_gate.clone());
        let mut p_blockers = Vec::with_capacity(m);
        for k in 0..m {
            let standin = 0x7E57_A200usize + k;
            let key = 0x5EED_A200u64 + k as u64;
            used::earlier_run_used(key);
            let (tx, g) = (p_tx.clone(), p_gate.clone());
            submit_lazy(standin, false, Box::new(move || {
                let _ = tx.send(());
                let mut open = g.0.lock().unwrap();
                while !*open {
                    open = g.1.wait(open).unwrap();
                }
                0xA200 + k
            }), key);
            p_blockers.push(standin);
        }
        for _ in 0..m {
            p_rx.recv_timeout(Duration::from_secs(10)).expect("preload workers saturated");
        }
        let target_p = 0x7E57_A2FFusize;
        let key_p = 0x5EED_A2FFu64;
        used::earlier_run_used(key_p);
        let (ran_p, dropped_p) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (ran_job, flag_p) = (ran_p.clone(), DropFlag(dropped_p.clone()));
        submit_lazy(target_p, false, Box::new(move || {
            let _keep = &flag_p;
            ran_job.store(true, Ordering::Release);
            0xA2FF
        }), key_p);
        assert_eq!(released(target_p), 0);
        assert!(dropped_p.load(Ordering::Acquire), "preload queued job was not dropped immediately on released()");
        drop(open_pre);
        assert!(!ran_p.load(Ordering::Acquire));
        for standin in p_blockers {
            let start = Instant::now();
            while is_pending(standin) && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = released(standin);
        }
    }

    #[test]
    fn concurrent_resolve_retained_and_release_never_exposes_freed_pipeline() {
        let _sat = WORKER_SATURATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicU32, AtomicUsize};
        use std::sync::Barrier;
        use windows::core::GUID;

        #[repr(C)]
        struct TestComPso {
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
            let prev = (&*(this as *const TestComPso)).refs.fetch_add(1, Ordering::AcqRel);
            assert!(prev >= 1, "AddRef after final release");
            prev + 1
        }
        unsafe extern "system" fn dec_only(this: *mut c_void) -> u32 {
            let prev = (&*(this as *const TestComPso)).refs.fetch_sub(1, Ordering::AcqRel);
            assert!(prev >= 1, "double Release");
            let n = prev - 1;
            if n == 0 {
                let b = Box::from_raw(this as *mut TestComPso);
                b.drops.fetch_add(1, Ordering::AcqRel);
            }
            n
        }
        unsafe extern "system" fn pso_release_sim(this: *mut c_void) -> u32 {
            let probe = || -> (bool, bool, u32) {
                let n = dec_only(this);
                (n == 0, false, n)
            };
            if let Some((n, _, real)) = try_release_standin(this as usize, probe) {
                if n == 0 {
                    release(real);
                }
                return n;
            }
            let (is_zero, _, n) = probe();
            if is_zero {
                release(released(this as usize));
            }
            n
        }
        struct UnstartedReal(usize);
        impl Drop for UnstartedReal {
            fn drop(&mut self) {
                if self.0 != 0 {
                    unsafe { dec_only(self.0 as *mut c_void) };
                }
            }
        }
        let vt: *const [usize; 3] = Box::leak(Box::new([qi as usize, add_ref as usize, pso_release_sim as usize]));

        let total_drops = Arc::new(AtomicUsize::new(0));
        let iterations = 80usize;
        for i in 0..iterations {
            let standin_raw = Box::into_raw(Box::new(TestComPso {
                vt,
                refs: AtomicU32::new(1),
                drops: total_drops.clone(),
            })) as usize;
            let real_raw = Box::into_raw(Box::new(TestComPso {
                vt,
                refs: AtomicU32::new(1),
                drops: total_drops.clone(),
            })) as usize;
            let real_guard = UnstartedReal(real_raw);
            let build_now = i % 2 == 0;
            submit(standin_raw, i % 3 == 0, Box::new(move || {
                let ptr = real_guard.0;
                std::mem::forget(real_guard);
                ptr
            }));
            if build_now {
                let _ = resolve_wait_retained(standin_raw);
            }
            let p = entry(standin_raw).expect("tracked standin");

            let barrier = Arc::new(Barrier::new(5));
            let mut readers = Vec::with_capacity(4);
            for t in 0..4 {
                let b = barrier.clone();
                let p = p.clone();
                readers.push(std::thread::spawn(move || {
                    b.wait();
                    let res = match t % 2 {
                        0 => resolve_pending(&p, standin_raw),
                        _ => resolve_wait_for_pending(&p, standin_raw, None),
                    };
                    if let Some(com) = res.into_com() {
                        // Calling through the retained COM handle must still find a live object.
                        let live = unsafe {
                            use windows::core::Interface;
                            let ptr = com.as_raw();
                            let prev = (&*(ptr as *const TestComPso)).refs.load(Ordering::Acquire);
                            assert!(prev >= 1, "retained COM object was prematurely released");
                            com
                        };
                        drop(live);
                    }
                }));
            }
            barrier.wait();
            unsafe {
                drop(<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState as windows::core::Interface>::from_raw(
                    standin_raw as *mut c_void,
                ));
            }
            for h in readers {
                h.join().unwrap();
            }
        }

        // Wait for any background orphan jobs to finish releasing `real`.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let expected = iterations * 2;
        while total_drops.load(Ordering::Acquire) < expected && std::time::Instant::now() < deadline {
            unsafe { crate::hle::d3d12x::late_release::drain_known(true) };
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(total_drops.load(Ordering::Acquire), expected, "every stand-in and real PSO must be released cleanly");
    }
}

fn entry(pso: usize) -> Option<Arc<Pending>> {
    PENDING.lock().unwrap().as_ref()?.get(&pso).cloned()
}

/// Pipeline pointer resolved from a stand-in, holding a COM reference when `ptr` points to a
/// live `ID3D12PipelineState` / stand-in COM object tracked by `pso_async`.
#[derive(Clone, Default)]
pub struct Resolved {
    ptr: usize,
    com: Option<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState>,
    settled: bool,
}

impl Resolved {
    pub const fn none() -> Self {
        Self { ptr: 0, com: None, settled: false }
    }

    pub const fn unretained_settled(ptr: usize) -> Self {
        Self { ptr, com: None, settled: true }
    }

    pub fn ptr(&self) -> usize {
        self.ptr
    }

    pub fn has_com(&self) -> bool {
        self.com.is_some()
    }

    pub fn is_settled(&self) -> bool {
        self.settled
    }

    pub fn into_com(self) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState> {
        self.com
    }
}

/// Whether `pso` is currently registered as an async/lazy pending pipeline stand-in.
pub fn is_tracked(pso: usize) -> bool {
    entry(pso).is_some()
}

/// Resolve `pso`, reusing `saved` if `pso` was already resolved and released from `PENDING`
/// (or is already COM-retained by `saved`).
pub fn resolve_cached_wait(pso: usize, saved: &Resolved) -> Resolved {
    if !is_tracked(pso) && saved.ptr() != 0 && (saved.ptr() != pso || saved.has_com()) {
        return saved.clone();
    }
    let resolved = resolve_wait_retained(pso);
    if resolved.ptr() == 0 && saved.ptr() != 0 {
        return saved.clone();
    }
    resolved
}

/// `resolve_for_bind_retained`, reusing `saved` if `pso` was already resolved and released.
pub fn resolve_cached_for_bind(pso: usize, saved: &Resolved) -> Resolved {
    if !is_tracked(pso) && saved.ptr() != 0 && (saved.ptr() != pso || saved.has_com()) {
        return saved.clone();
    }
    let resolved = resolve_for_bind_retained(pso);
    if resolved.ptr() == 0 && saved.ptr() != 0 {
        return saved.clone();
    }
    resolved
}

/// Whether `ptr` points to a readable COM object whose vtable has readable AddRef/Release slots.
unsafe fn is_com_pso(ptr: usize) -> bool {
    if ptr < 0x1_0000 || (ptr & (std::mem::align_of::<usize>() - 1)) != 0 || !crate::gpu::shader::readable(ptr, std::mem::size_of::<usize>()) {
        return false;
    }
    let vt = *(ptr as *const usize);
    if !crate::gpu::shader::readable(vt, 3 * std::mem::size_of::<usize>()) {
        return false;
    }
    let slots = std::slice::from_raw_parts(vt as *const usize, 3);
    crate::gpu::shader::readable(slots[1], 1) && crate::gpu::shader::readable(slots[2], 1)
}

/// Retain a COM reference on `ptr` if it points to a live COM object with an `IUnknown` vtable.
/// Synthetic non-pointer test addresses (e.g. `0x1234`) stay unretained (`None`).
unsafe fn retain_com(ptr: usize) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12PipelineState> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D12::ID3D12PipelineState;
    if !is_com_pso(ptr) {
        return None;
    }
    let raw = ptr as *mut std::ffi::c_void;
    ID3D12PipelineState::from_raw_borrowed(&raw).cloned()
}

fn resolve_from_state(st: &State, pso: usize) -> Resolved {
    if st.released {
        return Resolved::none();
    }
    let target = if st.real != 0 {
        st.real
    } else if st.driver != 0 {
        st.driver
    } else {
        pso
    };
    Resolved { ptr: target, com: unsafe { retain_com(target) }, settled: st.done }
}

impl Pending {
    /// What a bind of the stand-in `pso` gives the driver while the pipeline is not built.
    #[cfg(test)]
    fn stand(&self, pso: usize) -> usize {
        match self.state.lock().unwrap().driver {
            0 => pso,
            d => d,
        }
    }
}

/// Stand-in `standin` is no driver object (`standin::shell`): binds get `driver` in its place.
/// Told before the title has the stand-in.
pub fn shared(standin: usize, driver: usize) {
    if let Some(p) = entry(standin) {
        p.state.lock().unwrap().driver = driver;
    }
}

/// Resolve a pending pipeline if tracked, retaining its COM reference before `released()` can
/// drop it. When `pso` is not in `PENDING`, returns `pso` unretained without touching it.
pub fn resolve_if_pending(pso: usize) -> Resolved {
    let Some(p) = entry(pso) else {
        return Resolved::unretained_settled(pso);
    };
    p.note_use();
    resolve_pending(&p, pso)
}

fn resolve_pending(p: &Arc<Pending>, pso: usize) -> Resolved {
    let st = p.state.lock().unwrap();
    let out = resolve_from_state(&st, pso);
    let should_enqueue = !st.done && !st.released && st.real == 0;
    drop(st);
    if should_enqueue && !p.sent.swap(true, Ordering::AcqRel) {
        enqueue(p.clone());
    }
    out
}

/// The pipeline to use for `pso` now: the real one once built, else the stand-in's driver
/// pipeline (`pso` itself when it is one).
pub fn resolve(pso: usize) -> usize {
    resolve_if_pending(pso).ptr()
}

/// The pipeline behind `pso` now, neither built nor waited for here: the real one once built,
/// else what binds get till then (`pso` itself when it is not a stand-in).
pub fn current(pso: usize) -> usize {
    let Some(p) = entry(pso) else { return pso };
    match p.real.load(Ordering::Acquire) {
        0 => p.stand(pso),
        r => r,
    }
}

/// `resolve` for a bind that needs the pipeline itself (a dispatch, a draw): waits up to
/// `wait_budget` for a pipeline still being built, else gets the stand-in at once.
pub fn resolve_wait(pso: usize) -> usize {
    resolve_wait_with(pso, wait_budget()).ptr()
}

/// `resolve_wait`, returning a retained COM pipeline handle that outlives concurrent `released()`.
pub fn resolve_wait_retained(pso: usize) -> Resolved {
    resolve_wait_with(pso, wait_budget())
}

/// `resolve_wait`, waiting at most `budget` (None: until the pipeline is built).
fn resolve_wait_with(pso: usize, budget: Option<std::time::Duration>) -> Resolved {
    let Some(p) = entry(pso) else {
        return Resolved::unretained_settled(pso);
    };
    p.note_use();
    resolve_wait_for_pending(&p, pso, budget)
}

/// A call put off to the submit is recorded now: wait for its compute pipeline to be built,
/// whether a bind already found it `late` or not. Its segment lies between the list's other work,
/// whose labels and fence signals the title reads as "done": recorded with the stand-in, the
/// dispatch is left out while they still fire. The submitting thread is held for that time
/// (`title_held`), as a bind's wait holds it. A build that fails still gives the stand-in, and a
/// program binds do not wait for (`image_only`) is not waited for.
///
/// Unset `XCL_PSO_WAIT_MS`: until the pipeline is built, as a bind waits. Set: the build is left
/// to the workers for that long first, as a bind leaves it (their turns for big compiles); one no
/// worker has started by then is built here. One a worker builds is waited for at most
/// `SUBMIT_WAIT_MOST`, and the call then goes on with the stand-in (logged and counted):
/// `title_held` stops only `kernelx::Sleep` and the GPU-wait watchdog, not every clock a title
/// reads.
pub fn wait_for_submit(pso: usize) {
    let Some(budget) = wait_budget() else {
        let _ = resolve_wait_for(pso, None);
        return;
    };
    let Some(p) = entry(pso) else { return };
    if !p.compute || p.spared.load(Ordering::Acquire) || p.state.lock().unwrap().done {
        return;
    }
    p.note_use();
    let _held = Held::new();
    if !p.sent.swap(true, Ordering::AcqRel) {
        enqueue(p.clone());
    }
    p.want();
    let built = |p: &Pending, most: std::time::Duration| *p.cv.wait_timeout_while(p.state.lock().unwrap(), most, |st| !st.done).unwrap().0;
    if built(&p, budget) {
        return;
    }
    // no worker has started it: built here, as without the budget
    let job = p.job.lock().unwrap().take();
    if let Some(job) = job {
        return complete(&p, job());
    }
    let most = submit_wait_most();
    if !built(&p, most) {
        let n = SUBMIT_GAVE_UP.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 16 || n.is_power_of_two() {
            crate::xlog_warn!("D3D12.X: deferred dispatch skipped after waiting {} s for its pipeline ({n} so far)", most.as_secs_f32());
        }
    }
}

/// put-off calls that went on with the stand-in after `SUBMIT_WAIT_MOST`
static SUBMIT_GAVE_UP: AtomicUsize = AtomicUsize::new(0);

/// How long a submit waits for a pipeline a worker builds, with `XCL_PSO_WAIT_MS` set: as long as
/// a retail title's watchdog lets its main thread stand still (30 s, #891).
const SUBMIT_WAIT_MOST: std::time::Duration = std::time::Duration::from_secs(30);

fn submit_wait_most() -> std::time::Duration {
    #[cfg(test)]
    if let Some(b) = SUBMIT_WAIT_FOR_TEST.with(|b| b.get()) {
        return b;
    }
    SUBMIT_WAIT_MOST
}

#[cfg(test)]
thread_local! {
    /// `SUBMIT_WAIT_MOST` for the submits of this test thread
    static SUBMIT_WAIT_FOR_TEST: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn set_submit_wait_for_test(most: Option<std::time::Duration>) {
    SUBMIT_WAIT_FOR_TEST.with(|b| b.set(most));
}

/// `resolve_wait`, waiting at most `budget` (None: not at all).
fn resolve_wait_for(pso: usize, budget: Option<std::time::Duration>) -> usize {
    let Some(p) = entry(pso) else { return pso };
    p.note_use();
    if p.spared.load(Ordering::Acquire) {
        return resolve_spared(&p, pso).ptr();
    }
    let Some(budget) = budget else { return resolve_unwaited(&p, pso) };
    // (the title's thread is held from here until the pipeline is built or the budget is over)
    let _held = (!p.state.lock().unwrap().done).then(Held::new);
    resolve_within(&p, pso, budget).ptr()
}

fn resolve_wait_for_pending(p: &Arc<Pending>, pso: usize, budget: Option<std::time::Duration>) -> Resolved {
    if p.spared.load(Ordering::Acquire) {
        return resolve_spared(p, pso);
    }
    let Some(budget) = budget else { return resolve_unwaited(&p, pso) };
    // (the title's thread is held from here until the pipeline is built or the budget is over)
    let _held = (!p.state.lock().unwrap().done).then(Held::new);
    resolve_within(p, pso, budget)
}

/// How long a bind waits for a pipeline still being built (`XCL_PSO_WAIT_MS`; unset or 0: not at
/// all, `resolve_unwaited`). A retail game builds compute programs on its own threads: the driver
/// takes up to 30 s for the biggest translated ones, and the title's watchdog ends it when its
/// main thread has been held 30 s. It bounds binds as the title records; a call put off to the
/// submit waits there until its pipeline is built (`wait_for_submit`).
fn wait_budget() -> Option<std::time::Duration> {
    #[cfg(test)]
    if let Some(b) = BUDGET_FOR_TEST.with(|b| b.get()) {
        return Some(b);
    }
    static B: OnceLock<Option<std::time::Duration>> = OnceLock::new();
    *B.get_or_init(|| std::env::var("XCL_PSO_WAIT_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&ms| ms > 0).map(std::time::Duration::from_millis))
}

/// Binds wait at most `XCL_PSO_WAIT_MS`: a dispatch is then not put off to the submit
/// (`list_x::defer`), where it would only wait as long.
pub fn waits_bounded() -> bool {
    wait_budget().is_some()
}

#[cfg(test)]
thread_local! {
    /// `XCL_PSO_WAIT_MS` for the binds of this test thread
    static BUDGET_FOR_TEST: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
}

/// Binds on this thread wait at most `budget` (None: as `XCL_PSO_WAIT_MS` says).
#[cfg(test)]
pub(super) fn set_wait_budget_for_test(budget: Option<std::time::Duration>) {
    BUDGET_FOR_TEST.with(|b| b.set(budget));
}

/// `resolve_wait` for at most `budget`: the build goes to the workers (not this thread), and a
/// pipeline not built by then is `late` – it and later binds of it get the stand-in (their
/// dispatches are skipped) until a worker has built it.
fn resolve_within(p: &Arc<Pending>, pso: usize, budget: std::time::Duration) -> Resolved {
    {
        let st = p.state.lock().unwrap();
        if st.done || st.released {
            return resolve_from_state(&st, pso);
        }
    }
    if !p.sent.swap(true, Ordering::AcqRel) {
        enqueue(p.clone());
    }
    if p.late.load(Ordering::Acquire) {
        let st = p.state.lock().unwrap();
        return resolve_from_state(&st, pso);
    }
    p.want();
    let (st, _) = p.cv.wait_timeout_while(p.state.lock().unwrap(), budget, |st| !st.done && !st.released).unwrap();
    if st.done || st.released {
        return resolve_from_state(&st, pso);
    }
    drop(st);
    p.late.store(true, Ordering::Release);
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n < 16 || n.is_power_of_two() {
        crate::xlog_warn!("D3D12.X: a pipeline still being built after {} ms: its binds go on without it until it is built ({} so far)", budget.as_millis(), n + 1);
    }
    let st = p.state.lock().unwrap();
    resolve_from_state(&st, pso)
}

/// `resolve_wait` with no budget: the real pipeline once built, else the stand-in at once (its
/// dispatches and draws are left out), with the build sent to the workers.
///
/// The title's thread neither builds the pipeline nor waits for it. A bind that built a cold
/// pipeline itself and waited for it kept a retail title's thread in the driver's compile of a
/// 2.4 MB compute program for 466 s, with no frame shown after it (it never finished). Unlike
/// `resolve_spared` the build is `wanted`: the title binds it now, so it goes before the builds
/// made ahead and does not wait for free commit (up to `real::big_compile_turn`'s cap).
fn resolve_unwaited(p: &Arc<Pending>, pso: usize) -> usize {
    match p.real.load(Ordering::Acquire) {
        0 => {
            // (before it is sent: the worker's compiles see it wanted from their start)
            p.want();
            if !p.sent.swap(true, Ordering::AcqRel) {
                enqueue(p.clone());
            }
            if !*p.done.lock().unwrap() && !p.late.swap(true, Ordering::AcqRel) {
                static N: AtomicUsize = AtomicUsize::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                if n < 16 || n.is_power_of_two() {
                    crate::xlog_info!("D3D12.X: a pipeline bound while it is still being built: its binds go on without it until it is ({} so far)", n + 1);
                }
            }
            p.stand(pso)
        }
        r => r,
    }
}

/// `XCL_PSO_IMAGE_SKIP=1`: a bind does not wait for a big compute program that writes nothing
/// but images (`gpu::effects::big_and_image_only`); its dispatches are left out until a worker
/// has built it. Off by default: what such a program draws is missing for that long.
///
/// A retail title's world start binds one ray-tracing resolve program per material (73 – 118
/// KiB of console code, about 1 MB of DXIL each): an RDNA2 driver takes 22 – 172 s for each,
/// one bind at a time, and the title stands still for all of them. `XCL_PSO_WAIT_MS` goes on without every late
/// program, also the culling passes whose counts an `ExecuteIndirect` reads next – a GPU hang.
pub fn image_skip() -> bool {
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| std::env::var("XCL_PSO_IMAGE_SKIP").is_ok_and(|v| v.trim() == "1"))
}

/// The compute pipeline behind stand-in `standin` writes nothing but images: binds go on
/// without it while it is built (`resolve_spared`). Told before the title has the stand-in.
pub fn image_only(standin: usize) {
    if let Some(p) = entry(standin) {
        p.spared.store(true, Ordering::Release);
    }
}

/// `resolve_wait` for a pipeline binds do not wait for (`image_only`): the real one once built,
/// else the stand-in, with the build sent to the workers. Nobody waits, so the build is not
/// `wanted`: a big one takes its turn among the driver's compiles and waits for free commit.
fn resolve_spared(p: &Arc<Pending>, pso: usize) -> Resolved {
    {
        let st = p.state.lock().unwrap();
        if st.real != 0 || st.done || st.released {
            return resolve_from_state(&st, pso);
        }
    }
    if !p.sent.swap(true, Ordering::AcqRel) {
        enqueue(p.clone());
    }
    let st = p.state.lock().unwrap();
    if !st.done && !st.released && !p.late.swap(true, Ordering::AcqRel) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n < 16 || n.is_power_of_two() {
            crate::xlog_info!("D3D12.X: a compute program that writes only images is still being built: its dispatches are left out until it is ({} so far)", n + 1);
        }
    }
    resolve_from_state(&st, pso)
}

/// For binding: compute pipelines are waited for up to `XCL_PSO_WAIT_MS` (`resolve_wait`),
/// graphics ones are not.
pub fn resolve_for_bind(pso: usize) -> usize {
    match entry(pso) {
        Some(p) if p.compute => {
            p.note_use();
            resolve_wait_for_pending(&p, pso, wait_budget()).ptr()
        }
        Some(p) => {
            p.note_use();
            resolve_pending(&p, pso).ptr()
        }
        None => pso,
    }
}

/// `resolve_for_bind`, returning a retained COM pipeline handle that outlives concurrent `released()`.
pub fn resolve_for_bind_retained(pso: usize) -> Resolved {
    match entry(pso) {
        Some(p) if p.compute => {
            p.note_use();
            resolve_wait_for_pending(&p, pso, wait_budget())
        }
        Some(p) => {
            p.note_use();
            resolve_pending(&p, pso)
        }
        None => Resolved::unretained_settled(pso),
    }
}

/// The title released stand-in `pso` for good: forget it (its address may come back for
/// another object) and return its real pipeline to release (0: none, or still being built –
/// the worker's result is then released when it arrives).
pub fn released(pso: usize) -> usize {
    let Some(p) = PENDING.lock().unwrap().as_mut().and_then(|m| m.remove(&pso)) else { return 0 };
    let (orphan, real) = {
        let mut st = p.state.lock().unwrap();
        p.finish_release(&mut st)
    };
    if orphan {
        drop_unstarted_job(&p);
    }
    real
}

/// Retire a tracked adopted stand-in under its `Pending::state` lock, so `resolve*_retained`
/// cannot `AddRef` `pso` concurrently with its final COM reference dropping to zero.
pub fn try_release_standin(
    pso: usize,
    probe: impl FnOnce() -> (bool, bool, u32),
) -> Option<(u32, bool, usize)> {
    let p = entry(pso)?;
    let (held, orphan, real) = {
        let mut st = p.state.lock().unwrap();
        if st.released {
            return None;
        }
        let (is_zero, held, n) = probe();
        if !is_zero {
            return Some((n, false, 0));
        }
        let (orphan, real) = p.finish_release(&mut st);
        (held, orphan, real)
    };
    if let Some(m) = PENDING.lock().unwrap().as_mut() {
        if m.get(&pso).is_some_and(|cur| Arc::ptr_eq(cur, &p)) {
            m.remove(&pso);
        }
    }
    if orphan {
        drop_unstarted_job(&p);
    }
    Some((0, held, real))
}

/// The pipeline of stand-in `pso` once its build is over: the real one, or the stand-in's
/// driver pipeline when the build failed; `pso` itself at once when it is no stand-in (also with
/// `XCL_ASYNC_PSO=0`: pipelines are built in the call, none is pending). Waits as long as the
/// build takes – for tools and tests, never for a bind (`resolve_wait`).
pub fn wait_built(pso: usize) -> usize {
    let Some(p) = entry(pso) else { return pso };
    if !p.sent.swap(true, Ordering::AcqRel) {
        enqueue(p.clone());
    }
    let mut done = p.done.lock().unwrap();
    while !*done {
        done = p.cv.wait(done).unwrap();
    }
    drop(done);
    match p.real.load(Ordering::Acquire) {
        0 => p.stand(pso),
        r => r,
    }
}

/// `pso` is a stand-in whose real pipeline is still being built.
pub fn is_pending(pso: usize) -> bool {
    entry(pso).is_some_and(|p| {
        let st = p.state.lock().unwrap();
        !st.done && !st.released
    })
}

/// A call with `pso` was put off to the submit, which will wait for it: its build goes before the
/// workers' queue, as a wanted one.

/// `pso` is a stand-in with no real pipeline: still being built, or its build failed (it draws nothing).
pub fn is_stand_in(pso: usize) -> bool {
    entry(pso).is_some_and(|p| p.state.lock().unwrap().real == 0)
}
pub fn need_by_submit(pso: usize) {
    let Some(p) = entry(pso) else { return };
    if !std::mem::replace(&mut p.demand.lock().unwrap().by_submit, true) {
        p.want();
        NEEDED.lock().unwrap().push_back(p);
        let _ = queue().lock().unwrap().send(Box::new(|| ()));
    }
}

/// `bound` – what `resolve_wait(standin)` gave a bind – is the stand-in of a pipeline that was not
/// built yet then: still being built, or built since. Not after a failed build (that one is the
/// stand-in for good). Unlike `is_pending(standin)` after the bind, a build that ends between
/// the two does not make the stand-in look like the title's pipeline.
pub fn bound_unbuilt(standin: usize, bound: usize) -> bool {
    entry(standin).is_some_and(|p| {
        let st = p.state.lock().unwrap();
        bound == p.stand(standin) && !(st.done && st.real == 0)
    })
}

/// (built in the background, failed, still building)
pub fn counts() -> (usize, usize, usize) {
    let pending = PENDING.lock().unwrap().as_ref().map_or(0, |m| {
        m.values()
            .filter(|p| {
                let st = p.state.lock().unwrap();
                !st.done && !st.released
            })
            .count()
    });
    (BUILT.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed), pending)
}
