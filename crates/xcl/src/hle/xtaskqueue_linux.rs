//! Linux XTaskQueue slice (Phase C step 11o): manual/delayed/pool dispatch.
//!
//! The Windows `hle/xgameruntime/taskqueue.rs` stays the implementation source of truth
//! (GDK `XTaskQueue.h` contract, libHttpClient semantics). On Linux there is no
//! `RegisterWaitForSingleObject`, so waiter registration is served by a helper thread per
//! waiter that blocks in `kernelx_linux::WaitForSingleObject` (short poll so unregister /
//! terminate stays prompt) and submits through the same `submit_now` path; token map,
//! immutable waiter, self-unregister guard, and terminate cleanup mirror the Windows
//! waiter subsystem (`#1239` + `#315`). Everything the test title's `XTaskQueue:` checks need — manual/delayed/thread-pool
//! submit, `Dispatch` with timeout, `Terminate` (canceled drain + delayed cancel),
//! composite ports sharing, duplicate/close handles, `IsEmpty`, process queue,
//! monitors, plus the `XNetworking` UDP-port/connectivity shims — is real `win64`
//! code here so the imports resolve as HLE. The UDP-port async pair runs on the
//! title's XAsync engine; other `XAsync`-backed variants stay `Stub` until their
//! slice lands.

#![allow(non_snake_case)]

use std::cell::Cell;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::hle::xuser_linux::{begin_with, XAsyncBlock, XAsyncGetResult};
use crate::util::{E_ABORT, E_FAIL, E_INVALIDARG, HRESULT, S_OK};

pub const MODE_MANUAL: u32 = 0;
pub const MODE_THREAD_POOL: u32 = 1;
pub const MODE_SERIALIZED_THREAD_POOL: u32 = 2;
pub const MODE_IMMEDIATE: u32 = 3;
pub const PORT_WORK: u32 = 0;
pub const PORT_COMPLETION: u32 = 1;

pub type XTaskQueueCallback = unsafe extern "win64" fn(*mut c_void, bool);
pub type XTaskQueueTerminatedCallback = unsafe extern "win64" fn(*mut c_void);
pub type XTaskQueueMonitorCallback = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32);

#[derive(Clone, Copy)]
pub struct Item {
    pub cb: usize,
    pub ctx: usize,
}

impl Item {
    pub unsafe fn run(self, canceled: bool) {
        let f: XTaskQueueCallback = std::mem::transmute(self.cb);
        f(self.ctx as *mut c_void, canceled);
    }
}

struct QueuedItem {
    item: Item,
    queue: Arc<Queue>,
    port: u32,
}

struct Monitor {
    token: u64,
    ctx: usize,
    cb: usize,
    state: Mutex<MonitorState>,
    idle: Condvar,
}

#[derive(Default)]
struct MonitorState {
    revoked: bool,
    running: usize,
}

thread_local! {
    static MONITOR_DEPTH: Cell<usize> = const { Cell::new(0) };
}

impl Monitor {
    /// Runs the callback unless the registration was revoked; returns true when a
    /// revoked registration has no callbacks left (the caller then drops it).
    fn invoke(&self, handle: *mut c_void, port: u32) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.revoked {
                return false;
            }
            state.running += 1;
        }

        MONITOR_DEPTH.with(|depth| depth.set(depth.get() + 1));
        unsafe {
            let callback: XTaskQueueMonitorCallback = std::mem::transmute(self.cb);
            callback(self.ctx as *mut c_void, handle, port);
        }
        MONITOR_DEPTH.with(|depth| depth.set(depth.get() - 1));

        let mut state = self.state.lock().unwrap();
        state.running -= 1;
        let drained = state.revoked && state.running == 0;
        if drained {
            self.idle.notify_all();
        }
        drained
    }

    fn revoke(&self) -> bool {
        // Keep callback-stack unregistration nonblocking. Waiting from a monitor can
        // deadlock with itself or with another monitor that unregisters it.
        let outside_monitor = MONITOR_DEPTH.with(|depth| depth.get() == 0);
        let mut state = self.state.lock().unwrap();
        state.revoked = true;
        while outside_monitor && state.running != 0 {
            state = self.idle.wait(state).unwrap();
        }
        state.running == 0
    }
}

pub struct Port {
    mode: u32,
    items: Mutex<VecDeque<QueuedItem>>,
    cv: Condvar,
    running: AtomicBool,
    queues: Mutex<Vec<Weak<Queue>>>,
}

impl Port {
    fn new(mode: u32) -> Arc<Port> {
        Arc::new(Port {
            mode,
            items: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            running: AtomicBool::new(false),
            queues: Mutex::new(Vec::new()),
        })
    }

    fn attach(&self, q: &Arc<Queue>) {
        let mut qs = self.queues.lock().unwrap();
        qs.retain(|w| w.strong_count() > 0);
        qs.push(Arc::downgrade(q));
    }

    fn notify_monitors(self: &Arc<Port>) {
        let owners: Vec<Arc<Queue>> = {
            let mut qs = self.queues.lock().unwrap();
            let mut live = Vec::with_capacity(qs.len());
            qs.retain(|w| match w.upgrade() {
                Some(q) => {
                    live.push(q);
                    true
                }
                None => false,
            });
            live
        };
        for owner in owners {
            if owner.is_terminated() {
                continue;
            }
            let snapshot: Vec<Arc<Monitor>> = owner.monitors.lock().unwrap().clone();
            if snapshot.is_empty() {
                continue;
            }
            for owner_port in 0..2 {
                if Arc::ptr_eq(&owner.ports[owner_port], self) {
                    for monitor in &snapshot {
                        if owner.is_terminated() {
                            break;
                        }
                        let temp = new_handle(owner.clone()) as *mut c_void;
                        let drained = monitor.invoke(temp, owner_port as u32);
                        unsafe {
                            XTaskQueueCloseHandle(temp);
                        }
                        if drained {
                            owner.remove_monitor(monitor.token);
                        }
                    }
                }
            }
        }
    }
}

struct QueueState {
    terminated: bool,
    /// Pending async termination callbacks not yet pushed onto `PORT_COMPLETION`.
    termination_cb_pending: usize,
    inflight: [usize; 2],
}

pub struct Queue {
    ports: [Arc<Port>; 2],
    #[allow(dead_code)]
    owns_ports: bool,
    state: Mutex<QueueState>,
    idle_cv: Condvar,
    monitors: Mutex<Vec<Arc<Monitor>>>,
}

pub struct QueueHandle {
    magic: u64,
    pub queue: Arc<Queue>,
}

const MAGIC: u64 = 0x5843_4C51_5545_5545; // "XCLQUEUE"

type Job = Box<dyn FnOnce() + Send>;

const MAX_WORKERS: usize = 256;
const IDLE_EXIT: Duration = Duration::from_secs(30);

struct Pool {
    jobs: Mutex<PoolState>,
    cv: Condvar,
    base: usize,
}

struct PoolState {
    queue: VecDeque<Job>,
    workers: usize,
    idle: usize,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool {
        jobs: Mutex::new(PoolState { queue: VecDeque::new(), workers: 0, idle: 0 }),
        cv: Condvar::new(),
        base: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8),
    })
}

fn worker(p: &'static Pool) {
    let mut g = p.jobs.lock().unwrap();
    loop {
        if let Some(job) = g.queue.pop_front() {
            drop(g);
            job();
            g = p.jobs.lock().unwrap();
            continue;
        }
        g.idle += 1;
        let (mut ng, t) = p.cv.wait_timeout(g, IDLE_EXIT).unwrap();
        ng.idle -= 1;
        if t.timed_out() && ng.queue.is_empty() && ng.workers > p.base {
            ng.workers -= 1;
            return;
        }
        g = ng;
    }
}

fn spawn(job: Job) {
    let p = pool();
    let mut g = p.jobs.lock().unwrap();
    g.queue.push_back(job);
    if g.idle >= g.queue.len() {
        p.cv.notify_one();
        return;
    }
    if g.workers >= MAX_WORKERS {
        p.cv.notify_one();
        return;
    }
    let id = g.workers;
    g.workers += 1;
    drop(g);
    if std::thread::Builder::new()
        .name(format!("xcl-taskqueue-{id}"))
        .spawn(move || worker(p))
        .is_err()
    {
        let mut g = p.jobs.lock().unwrap();
        g.workers -= 1;
        p.cv.notify_one();
    }
}

struct Timed {
    at: Instant,
    seq: u64,
    queue: Arc<Queue>,
    port: u32,
    item: Item,
}
impl PartialEq for Timed {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at && self.seq == o.seq
    }
}
impl Eq for Timed {}
impl PartialOrd for Timed {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Timed {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.at.cmp(&self.at).then(o.seq.cmp(&self.seq))
    }
}

fn timer() -> &'static (Mutex<BinaryHeap<Timed>>, Condvar) {
    static T: OnceLock<(Mutex<BinaryHeap<Timed>>, Condvar)> = OnceLock::new();
    static STARTED: AtomicBool = AtomicBool::new(false);
    let t = T.get_or_init(|| (Mutex::new(BinaryHeap::new()), Condvar::new()));
    if !STARTED.swap(true, Ordering::AcqRel) {
        std::thread::Builder::new()
            .name("xcl-taskqueue-timer".into())
            .spawn(|| {
                let (m, cv) = timer();
                let mut g = m.lock().unwrap();
                loop {
                    let now = Instant::now();
                    match g.peek() {
                        None => g = cv.wait(g).unwrap(),
                        Some(t) if t.at <= now => {
                            let t = g.pop().unwrap();
                            // Reserve an inflight slot before releasing the timer lock so a
                            // concurrent XTaskQueueTerminate cannot observe inflight == 0 and
                            // drop or outrun this due callback.
                            t.queue.state.lock().unwrap().inflight[t.port as usize] += 1;
                            drop(g);
                            dispatch_due_timed(&t.queue, t.port, t.item);
                            g = m.lock().unwrap();
                        }
                        Some(t) => {
                            let d = t.at - now;
                            g = cv.wait_timeout(g, d).unwrap().0;
                        }
                    }
                }
            })
            .expect("spawn timer");
    }
    t
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static NEXT_MONITOR_TOKEN: AtomicU64 = AtomicU64::new(1);

impl Queue {
    pub fn new(work_mode: u32, completion_mode: u32) -> Arc<Queue> {
        let q = Arc::new(Queue {
            ports: [Port::new(work_mode), Port::new(completion_mode)],
            owns_ports: true,
            state: Mutex::new(QueueState { terminated: false, termination_cb_pending: 0, inflight: [0, 0] }),
            idle_cv: Condvar::new(),
            monitors: Mutex::new(Vec::new()),
        });
        q.ports[0].attach(&q);
        q.ports[1].attach(&q);
        q
    }

    pub fn composite(work: Arc<Port>, completion: Arc<Port>) -> Arc<Queue> {
        let q = Arc::new(Queue {
            ports: [work, completion],
            owns_ports: false,
            state: Mutex::new(QueueState { terminated: false, termination_cb_pending: 0, inflight: [0, 0] }),
            idle_cv: Condvar::new(),
            monitors: Mutex::new(Vec::new()),
        });
        q.ports[0].attach(&q);
        if !Arc::ptr_eq(&q.ports[0], &q.ports[1]) {
            q.ports[1].attach(&q);
        }
        q
    }

    pub fn is_terminated(&self) -> bool {
        self.state.lock().unwrap().terminated
    }

    fn done_one(&self, port: u32) {
        let mut state = self.state.lock().unwrap();
        state.inflight[port as usize] -= 1;
        if state.inflight[0] == 0 && state.inflight[1] == 0 {
            self.idle_cv.notify_all();
        }
    }

    fn remove_monitor(&self, token: u64) {
        self.monitors.lock().unwrap().retain(|monitor| monitor.token != token);
    }

    unsafe fn execute(self: &Arc<Queue>, port: u32, item: Item) {
        let canceled = self.is_terminated();
        item.run(canceled);
        self.done_one(port);
    }
}

fn drain_serialized(p: Arc<Port>) {
    loop {
        let next = p.items.lock().unwrap().pop_front();
        match next {
            Some(qi) => unsafe { qi.queue.execute(qi.port, qi.item) },
            None => {
                p.running.store(false, Ordering::Release);
                let more = !p.items.lock().unwrap().is_empty();
                if more && !p.running.swap(true, Ordering::AcqRel) {
                    continue;
                }
                break;
            }
        }
    }
}

pub fn submit_now(q: &Arc<Queue>, port: u32, item: Item) -> bool {
    submit(q, port, item, false)
}

/// `submit_now`, but `terminating` also lets the termination callback through once `q` is terminated.
fn submit(q: &Arc<Queue>, port: u32, item: Item, terminating: bool) -> bool {
    let p = &q.ports[port as usize];
    match p.mode {
        MODE_IMMEDIATE => {
            {
                let mut state = q.state.lock().unwrap();
                if state.terminated && !terminating {
                    return false;
                }
                if terminating {
                    state.termination_cb_pending = state.termination_cb_pending.saturating_sub(1);
                }
                state.inflight[port as usize] += 1;
            }
            p.notify_monitors();
            unsafe { q.execute(port, item) };
        }
        MODE_THREAD_POOL => {
            {
                let mut state = q.state.lock().unwrap();
                if state.terminated && !terminating {
                    return false;
                }
                if terminating {
                    state.termination_cb_pending = state.termination_cb_pending.saturating_sub(1);
                }
                state.inflight[port as usize] += 1;
            }
            p.notify_monitors();
            let q2 = q.clone();
            spawn(Box::new(move || unsafe { q2.execute(port, item) }));
        }
        _ => {
            {
                let mut items = p.items.lock().unwrap();
                {
                    let mut state = q.state.lock().unwrap();
                    if state.terminated && !terminating {
                        return false;
                    }
                    if terminating {
                        state.termination_cb_pending = state.termination_cb_pending.saturating_sub(1);
                    }
                    state.inflight[port as usize] += 1;
                }
                items.push_back(QueuedItem { item, queue: q.clone(), port });
            }
            if terminating {
                p.cv.notify_all();
            } else {
                p.cv.notify_one();
            }
            p.notify_monitors();
            // Only the serialized pool gets an automatic drain worker; a manual port
            // waits for an explicit XTaskQueueDispatch on the caller's thread (#2806).
            if p.mode == MODE_SERIALIZED_THREAD_POOL && !p.running.swap(true, Ordering::AcqRel) {
                let p2 = p.clone();
                spawn(Box::new(move || drain_serialized(p2)));
            }
        }
    }
    true
}

/// Dispatch a delayed item popped from the timer heap whose `inflight` slot was already
/// reserved under the timer lock so `XTaskQueueTerminate` cannot finish before it runs.
fn dispatch_due_timed(q: &Arc<Queue>, port: u32, item: Item) {
    if submit_now(q, port, item) {
        q.done_one(port);
    } else {
        unsafe { q.execute(port, item) };
    }
}

pub fn submit_delayed(q: &Arc<Queue>, port: u32, delay_ms: u32, item: Item) -> bool {
    if delay_ms == 0 {
        return submit_now(q, port, item);
    }
    let (m, cv) = timer();
    let mut heap = m.lock().unwrap();
    if q.is_terminated() {
        return false;
    }
    heap.push(Timed {
        at: Instant::now() + Duration::from_millis(delay_ms as u64),
        seq: SEQ.fetch_add(1, Ordering::Relaxed),
        queue: q.clone(),
        port,
        item,
    });
    drop(heap);
    cv.notify_all();
    true
}

/// Dispatch one queued item on the caller's thread. Returns true when something ran.
pub fn dispatch(q: &Arc<Queue>, port: u32, timeout_ms: u32) -> bool {
    let p = &q.ports[port as usize];
    // Serialization applies to the automatic worker. Explicit callers can also claim queued work.
    if !matches!(p.mode, MODE_MANUAL | MODE_SERIALIZED_THREAD_POOL) {
        return false;
    }
    let mut g = p.items.lock().unwrap();
    if g.is_empty() && timeout_ms != 0 {
        let dur = if timeout_ms == u32::MAX {
            Duration::from_secs(u64::MAX / 4)
        } else {
            Duration::from_millis(timeout_ms as u64)
        };
        let deadline = Instant::now() + dur.min(Duration::from_secs(365 * 24 * 3600));
        while g.is_empty() && {
            let state = q.state.lock().unwrap();
            !state.terminated || (port == PORT_COMPLETION && state.termination_cb_pending > 0)
        } {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = p.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
    let Some(qi) = g.pop_front() else { return false };
    drop(g);
    unsafe { qi.queue.execute(qi.port, qi.item) };
    true
}

static HANDLES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

static PORT_HANDLES: Mutex<Option<HashMap<usize, std::sync::Weak<Port>>>> = Mutex::new(None);

fn register_port_handle(p: &Arc<Port>) -> *mut c_void {
    let key = Arc::as_ptr(p) as usize;
    let mut guard = PORT_HANDLES.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() > 64 {
        map.retain(|_, w| w.strong_count() > 0);
    }
    map.insert(key, Arc::downgrade(p));
    key as *mut c_void
}

fn port_from_handle(h: *mut c_void) -> Option<Arc<Port>> {
    if h.is_null() {
        return None;
    }
    let guard = PORT_HANDLES.lock().unwrap();
    guard.as_ref()?.get(&(h as usize))?.upgrade()
}

pub fn new_handle(q: Arc<Queue>) -> *mut QueueHandle {
    let h = Box::into_raw(Box::new(QueueHandle { magic: MAGIC, queue: q }));
    HANDLES.lock().unwrap().get_or_insert_with(HashSet::new).insert(h as usize);
    h
}

pub unsafe fn queue_from(h: *mut c_void) -> Option<Arc<Queue>> {
    if h.is_null() {
        return process_queue();
    }
    let live = HANDLES.lock().unwrap();
    if !live.as_ref().is_some_and(|s| s.contains(&(h as usize))) {
        return None;
    }
    let qh = &*(h as *const QueueHandle);
    (qh.magic == MAGIC).then(|| qh.queue.clone())
}

struct ProcessQueueState {
    initialized: bool,
    queue: Option<Arc<Queue>>,
}

impl ProcessQueueState {
    fn current(&mut self) -> Option<Arc<Queue>> {
        if !self.initialized {
            self.initialized = true;
            self.queue = Some(Queue::new(MODE_THREAD_POOL, MODE_THREAD_POOL));
        }
        self.queue.clone()
    }

    fn set(&mut self, queue: Option<Arc<Queue>>) {
        self.initialized = true;
        self.queue = queue;
    }
}

static PROCESS_QUEUE: Mutex<ProcessQueueState> = Mutex::new(ProcessQueueState {
    initialized: false,
    queue: None,
});

pub fn process_queue() -> Option<Arc<Queue>> {
    PROCESS_QUEUE.lock().unwrap().current()
}

pub unsafe extern "win64" fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    if work > 3 || completion > 3 {
        return E_INVALIDARG;
    }
    *out = new_handle(Queue::new(work, completion)) as *mut c_void;
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueGetPort(h: *mut c_void, port: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    if port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    *out = register_port_handle(&q.ports[port as usize]);
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueCreateComposite(
    work_port: *mut c_void,
    completion_port: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let (Some(w), Some(c)) = (port_from_handle(work_port), port_from_handle(completion_port)) else {
        return E_INVALIDARG;
    };
    *out = new_handle(Queue::composite(w, c)) as *mut c_void;
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueDuplicateHandle(h: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    match queue_from(h) {
        Some(q) => {
            *out = new_handle(q) as *mut c_void;
            S_OK
        }
        None => E_INVALIDARG,
    }
}

pub unsafe extern "win64" fn XTaskQueueCloseHandle(h: *mut c_void) {
    if h.is_null() {
        return;
    }
    let removed = HANDLES.lock().unwrap().as_mut().is_some_and(|s| s.remove(&(h as usize)));
    if removed {
        let qh = h as *mut QueueHandle;
        (*qh).magic = 0;
        drop(Box::from_raw(qh));
    }
}

pub unsafe extern "win64" fn XTaskQueueSubmitCallback(h: *mut c_void, port: u32, ctx: *mut c_void, cb: usize) -> HRESULT {
    if cb == 0 || port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    if !submit_now(&q, port, Item { cb, ctx: ctx as usize }) {
        return E_ABORT;
    }
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueSubmitDelayedCallback(h: *mut c_void, port: u32, delay: u32, ctx: *mut c_void, cb: usize) -> HRESULT {
    if cb == 0 || port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    if !submit_delayed(&q, port, delay, Item { cb, ctx: ctx as usize }) {
        return E_ABORT;
    }
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueDispatch(h: *mut c_void, port: u32, timeout: u32) -> bool {
    if port > 1 {
        return false;
    }
    match queue_from(h) {
        Some(q) => dispatch(&q, port, timeout),
        None => false,
    }
}

pub unsafe extern "win64" fn XTaskQueueIsEmpty(h: *mut c_void, port: u32) -> bool {
    match queue_from(h) {
        Some(q) if port <= 1 => {
            // A delayed callback not yet due is still queued (libHttpClient: m_pendingList).
            // Lock order: timer -> p.items -> q.state.
            let delayed = timer().0.lock().unwrap();
            let items = q.ports[port as usize].items.lock().unwrap();
            let state = q.state.lock().unwrap();
            !delayed.iter().any(|t| t.port == port && Arc::ptr_eq(&t.queue, &q))
                && !items.iter().any(|qi| Arc::ptr_eq(&qi.queue, &q))
                && state.inflight[port as usize] == 0
        }
        _ => true,
    }
}

pub unsafe extern "win64" fn XTaskQueueTerminate(h: *mut c_void, wait: bool, ctx: *mut c_void, cb: usize) -> HRESULT {
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    {
        let mut state = q.state.lock().unwrap();
        state.terminated = true;
        if !wait && cb != 0 {
            state.termination_cb_pending += 1;
        }
    }
    q.monitors.lock().unwrap().clear();
    for port in 0..2 {
        let mine = {
            let mut items = q.ports[port].items.lock().unwrap();
            let mut mine = Vec::new();
            let mut keep = VecDeque::with_capacity(items.len());
            while let Some(qi) = items.pop_front() {
                if Arc::ptr_eq(&qi.queue, &q) {
                    mine.push(qi);
                } else {
                    keep.push_back(qi);
                }
            }
            *items = keep;
            mine
        };
        q.ports[port].cv.notify_all();
        for qi in mine {
            qi.queue.execute(qi.port, qi.item);
        }
    }
    let delayed: Vec<Timed> = {
        let (m, cv) = timer();
        let mut heap = m.lock().unwrap();
        let (mine, keep): (Vec<Timed>, Vec<Timed>) = std::mem::take(&mut *heap).into_vec().into_iter().partition(|t| Arc::ptr_eq(&t.queue, &q));
        *heap = BinaryHeap::from(keep);
        cv.notify_all();
        mine
    };
    for t in delayed {
        t.queue.state.lock().unwrap().inflight[t.port as usize] += 1;
        t.queue.execute(t.port, t.item);
    }
    // Mirror the Windows waiter cleanup (#315): drop every waiter still attached to `q`
    // so termination does not leak helper threads or `Arc<Queue>` references when the
    // title terminates without unregistering each waiter.
    let waiters: Vec<Arc<Waiter>> = {
        let mut map = waiters().lock().unwrap();
        let keys: Vec<u64> = map.iter().filter(|(_, w)| Arc::ptr_eq(&w.queue, &q)).map(|(&k, _)| k).collect();
        keys.into_iter().filter_map(|k| map.remove(&k)).collect()
    };
    for w in waiters {
        unregister_waiter(w);
    }
    let ctx = ctx as usize;
    let finish = move || {
        let mut g = q.state.lock().unwrap();
        while g.inflight[0] != 0 || g.inflight[1] != 0 {
            g = q.idle_cv.wait(g).unwrap();
        }
        drop(g);
        if cb == 0 {
            return;
        }
        if wait {
            // Run here: on a manual completion port nobody may dispatch while the caller
            // waits (the docs warn that wait = true there may deadlock).
            let f: XTaskQueueTerminatedCallback = std::mem::transmute(cb);
            f(ctx as *mut c_void);
        } else {
            // The callback is queued to the completion port, like libHttpClient's termination
            // entry: on a manual port it runs from the title's own XTaskQueueDispatch.
            let item = Item { cb: run_terminated as usize, ctx: Box::into_raw(Box::new((cb, ctx))) as usize };
            submit(&q, PORT_COMPLETION, item, true);
        }
    };
    if wait {
        finish();
    } else {
        spawn(Box::new(finish));
    }
    S_OK
}

/// Runs a termination callback queued by `XTaskQueueTerminate`; `ctx` is its boxed `(cb, ctx)`.
unsafe extern "win64" fn run_terminated(ctx: *mut c_void, _canceled: bool) {
    let (cb, ctx) = *Box::from_raw(ctx as *mut (usize, usize));
    let f: XTaskQueueTerminatedCallback = std::mem::transmute(cb);
    f(ctx as *mut c_void);
}

pub unsafe extern "win64" fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *mut c_void) -> bool {
    if out.is_null() {
        return false;
    }
    match process_queue() {
        Some(queue) => {
            *out = new_handle(queue) as *mut c_void;
            true
        }
        None => {
            *out = std::ptr::null_mut();
            false
        }
    }
}

pub unsafe extern "win64" fn XTaskQueueSetCurrentProcessTaskQueue(h: *mut c_void) {
    if h.is_null() {
        PROCESS_QUEUE.lock().unwrap().set(None);
    } else if let Some(queue) = queue_from(h) {
        PROCESS_QUEUE.lock().unwrap().set(Some(queue));
    }
}

#[repr(C)]
pub struct RegistrationToken {
    pub token: u64,
}

struct Waiter {
    queue: Arc<Queue>,
    port: u32,
    item: Item,
    /// kernelx waitable handle the helper thread blocks on.
    handle: usize,
    /// Set before unregistering so in-flight or queued callbacks stop submitting immediately.
    stopped: AtomicBool,
}

static NEXT_WAITER_TOKEN: AtomicU64 = AtomicU64::new(1);

fn waiters() -> &'static Mutex<HashMap<u64, Arc<Waiter>>> {
    static W: OnceLock<Mutex<HashMap<u64, Arc<Waiter>>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

thread_local! {
    static IN_WAITER: Cell<usize> = const { Cell::new(0) };
}

struct InWaiterGuard(usize);
impl Drop for InWaiterGuard {
    fn drop(&mut self) {
        IN_WAITER.with(|c| c.set(self.0));
    }
}

/// Mark a waiter stopped. The helper thread exits on its next short poll (or on its
/// current fire); removal from the map already happened at the call site, so this never
/// blocks — unregistering from inside the waiter's own callback cannot deadlock.
fn unregister_waiter(w: Arc<Waiter>) {
    w.stopped.store(true, Ordering::Release);
}

fn waiter_fire(w: &Arc<Waiter>) {
    let prev = IN_WAITER.with(|c| c.replace(Arc::as_ptr(w) as usize));
    let _guard = InWaiterGuard(prev);
    if !w.stopped.load(Ordering::Acquire) && !w.queue.is_terminated() {
        submit_now(&w.queue, w.port, w.item);
    }
}

/// Helper-thread body: Linux has no `RegisterWaitForSingleObject`, so each waiter gets a
/// thread that blocks in `WaitForSingleObject` with a short poll. The `Waiter` is immutable
/// after registration (no post-activation mutation race, #1239 issue 1); the 5 ms poll keeps
/// unregister/terminate prompt without a dedicated wake mechanism.
fn waiter_thread(w: Arc<Waiter>) {
    use crate::hle::kernelx_linux::{WAIT_FAILED, WAIT_OBJECT_0, WaitForSingleObject};
    loop {
        if w.stopped.load(Ordering::Acquire) || w.queue.is_terminated() {
            return;
        }
        // SAFETY: `handle` is a kernelx waitable id; invalid ids yield WAIT_FAILED.
        let r = unsafe { WaitForSingleObject(w.handle, 5) };
        if w.stopped.load(Ordering::Acquire) || w.queue.is_terminated() {
            return;
        }
        if r == WAIT_OBJECT_0 {
            waiter_fire(&w);
        } else if r == WAIT_FAILED {
            return;
        }
    }
}

pub unsafe extern "win64" fn XTaskQueueRegisterWaiter(
    h: *mut c_void,
    port: u32,
    handle: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut RegistrationToken,
) -> HRESULT {
    if port > 1 || token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    if q.is_terminated() {
        return E_ABORT;
    }
    let handle = handle as usize;
    if handle == 0 {
        return E_FAIL;
    }
    // Immutable after registration: the helper thread only ever reads `Waiter`.
    let w = Arc::new(Waiter {
        queue: q.clone(),
        port,
        item: Item { cb, ctx: ctx as usize },
        handle,
        stopped: AtomicBool::new(false),
    });
    let tok = NEXT_WAITER_TOKEN.fetch_add(1, Ordering::Relaxed);
    waiters().lock().unwrap().insert(tok, w.clone());
    // A concurrent Terminate between the first check and the insert must not leave a live
    // waiter on a dead queue (#315 post-insert race).
    if q.is_terminated() {
        if let Some(removed) = waiters().lock().unwrap().remove(&tok) {
            unregister_waiter(removed);
        }
        return E_ABORT;
    }
    std::thread::Builder::new()
        .name("xcl-taskqueue-waiter".into())
        .spawn(move || waiter_thread(w))
        .map_err(|_| ())
        .ok();
    (*token).token = tok;
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueUnregisterWaiter(_h: *mut c_void, token: RegistrationToken) {
    if token.token == 0 {
        return;
    }
    if let Some(w) = waiters().lock().unwrap().remove(&token.token) {
        unregister_waiter(w);
    }
}

pub unsafe extern "win64" fn XTaskQueueRegisterMonitor(
    h: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut RegistrationToken,
) -> HRESULT {
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    let id = NEXT_MONITOR_TOKEN.fetch_add(1, Ordering::Relaxed);
    q.monitors.lock().unwrap().push(Arc::new(Monitor {
        token: id,
        ctx: ctx as usize,
        cb,
        state: Mutex::new(MonitorState::default()),
        idle: Condvar::new(),
    }));
    (*token).token = id;
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueUnregisterMonitor(h: *mut c_void, token: RegistrationToken) {
    let Some(q) = queue_from(h) else { return };
    // Keep the registration discoverable until its callbacks finish so concurrent
    // unregistration calls can wait for the same drain. Never wait with the queue lock held.
    let monitor = q.monitors.lock().unwrap().iter().find(|m| m.token == token.token).cloned();
    if let Some(monitor) = monitor {
        if monitor.revoke() {
            q.remove_monitor(monitor.token);
        }
    }
}

// ---------------------------------------------------------------------------------------
// XNetworking offline shims (title-visible subset)
// ---------------------------------------------------------------------------------------

pub const DEFAULT_UDP_PORT: u16 = 3074;

/// `XCL_NETWORK=0` on Linux: the same env the Windows `netoff` module reads
/// (`hle_linux` has no `netoff` yet — see the LINUX-TODO in `hle_linux.rs`).
fn network_off() -> bool {
    std::env::var("XCL_NETWORK").is_ok_and(|v| v.trim() == "0")
}

/// The hint's level with no host to ask (Linux has no `iphlpapi`): `None` (1) with
/// `XCL_NETWORK=0`, as on a console without a network, else `InternetAccess` (3).
/// Mirrors the Windows `connectivity_level(None, off)` outcome (#494).
fn connectivity_level(network_off: bool) -> u32 {
    if network_off { 1 } else { 3 }
}

/// Split `http(s)://[user@]host[:port]/...` into its host and whether it is a plain-HTTP
/// loopback URL. Mirrors the Windows `url_host_and_is_plain_loopback` (#1382): the whole
/// `127.0.0.0/8` plus `::1`/`localhost` count, via `IpAddr::is_loopback`.
/// Currently a contract pin for the test vectors (no WinHTTP handle on Linux to read a URL
/// from — same no-consumer class as #1292/#1503/#843); a future Linux socket layer reuses it.
#[allow(dead_code)]
fn url_host_and_is_plain_loopback(url: &str) -> (String, bool) {
    let lower = url.to_ascii_lowercase();
    let is_http = lower.starts_with("http://");
    let after_scheme = match url.split_once("://") {
        Some((_, r)) => r,
        None => url,
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => "",
        }
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    let is_loopback_ip = host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let loopback = host.eq_ignore_ascii_case("localhost") || is_loopback_ip;
    (host.to_string(), is_http && loopback)
}

/// The one notification a register function sends right away, with the current value.
/// Mirrors the Windows `InitialNotification<T>` (#1237).
struct InitialNotification<T> {
    cb: usize,
    ctx: usize,
    value: T,
}

/// Queue `call(record)` on `queue`'s completion port; the record is freed by `call`, or here
/// when the queue does not take it. Mirrors the Windows `notify_initial` (#1237).
unsafe fn notify_initial<T>(queue: *mut c_void, ctx: *mut c_void, cb: usize, value: T, call: usize, what: &str) {
    let rec = Box::into_raw(Box::new(InitialNotification { cb, ctx: ctx as usize, value }));
    let submitted = queue_from(queue)
        .map(|q| submit_now(&q, PORT_COMPLETION, Item { cb: call, ctx: rec as usize }))
        .unwrap_or(false);
    if !submitted {
        drop(Box::from_raw(rec));
        crate::xlog_warn!("XNetworking: the initial {what} callback could not be queued");
    }
}

unsafe extern "win64" fn udp_port_changed_call(rec: *mut c_void, canceled: bool) {
    let r = Box::from_raw(rec as *mut InitialNotification<u16>);
    if !canceled {
        let f: unsafe extern "win64" fn(*mut c_void, u16) = std::mem::transmute(r.cb);
        f(r.ctx as *mut c_void, r.value);
    }
}

unsafe extern "win64" fn connectivity_hint_changed_call(rec: *mut c_void, canceled: bool) {
    let r = Box::from_raw(rec as *mut InitialNotification<XNetworkingConnectivityHint>);
    if !canceled {
        let f: unsafe extern "win64" fn(*mut c_void, *const XNetworkingConnectivityHint) = std::mem::transmute(r.cb);
        f(r.ctx as *mut c_void, &r.value);
    }
}

pub unsafe extern "win64" fn XNetworkingQueryPreferredLocalUdpMultiplayerPort(port: *mut u16) -> HRESULT {
    if port.is_null() {
        return E_INVALIDARG;
    }
    *port = DEFAULT_UDP_PORT;
    S_OK
}

static UDP_PORT_ASYNC_ID: u8 = 0;

/// The port is a constant, so the async answer is immediate; the payload and the
/// one-shot status live in the title's own XAsync engine (`hle/xuser_linux.rs`).
pub unsafe extern "win64" fn XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync(b: *mut XAsyncBlock) -> HRESULT {
    begin_with(b, &UDP_PORT_ASYNC_ID as *const u8 as usize, || Ok(DEFAULT_UDP_PORT.to_le_bytes().to_vec()))
}

pub unsafe extern "win64" fn XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult(
    b: *mut XAsyncBlock,
    port: *mut u16,
) -> HRESULT {
    if port.is_null() {
        return E_INVALIDARG;
    }
    *port = 0;
    XAsyncGetResult(b, &UDP_PORT_ASYNC_ID as *const u8 as *const c_void, std::mem::size_of::<u16>(), port as *mut c_void, std::ptr::null_mut())
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XNetworkingConnectivityHint {
    pub connectivity_level: u32,
    pub connectivity_cost: u32,
    pub iana_interface_type: u32,
    pub network_initialized: bool,
    pub approaching_data_limit: bool,
    pub over_data_limit: bool,
    pub roaming: bool,
}

pub unsafe extern "win64" fn XNetworkingGetConnectivityHint(out: *mut XNetworkingConnectivityHint) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // Offline headless: InternetAccess over Ethernet when the network is on, None (1) with
    // `XCL_NETWORK=0` as on a console without a network (#494); initialized, unmetered.
    *out = XNetworkingConnectivityHint {
        connectivity_level: connectivity_level(network_off()),
        connectivity_cost: 1,
        iana_interface_type: 6,
        network_initialized: true,
        approaching_data_limit: false,
        over_data_limit: false,
        roaming: false,
    };
    S_OK
}

static NEXT_NET_TOKEN: AtomicU64 = AtomicU64::new(1);

/// The port never changes, but registering "always fires an initial notification callback"
/// (GDK docs), and a title may wait for it (#1237).
pub unsafe extern "win64" fn XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(
    queue: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    *token = NEXT_NET_TOKEN.fetch_add(1, Ordering::Relaxed);
    notify_initial(queue, ctx, cb, DEFAULT_UDP_PORT, udp_port_changed_call as usize, "UDP port");
    S_OK
}

pub unsafe extern "win64" fn XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged(_token: u64, _wait: bool) -> bool {
    true
}

/// Registering "sends an initial notification callback" (GDK docs) with the current hint: a
/// title that waits for it to see `network_initialized` would otherwise wait forever (#1237).
pub unsafe extern "win64" fn XNetworkingRegisterConnectivityHintChanged(
    queue: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    *token = NEXT_NET_TOKEN.fetch_add(1, Ordering::Relaxed);
    let mut hint: XNetworkingConnectivityHint = std::mem::zeroed();
    XNetworkingGetConnectivityHint(&mut hint);
    notify_initial(queue, ctx, cb, hint, connectivity_hint_changed_call as usize, "connectivity hint");
    S_OK
}

pub unsafe extern "win64" fn XNetworkingUnregisterConnectivityHintChanged(_token: u64, _wait: bool) -> bool {
    true
}

/// Implemented exports with real code addresses.
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("XTaskQueueCreate", XTaskQueueCreate as usize),
        ("XTaskQueueCreateComposite", XTaskQueueCreateComposite as usize),
        ("XTaskQueueGetPort", XTaskQueueGetPort as usize),
        ("XTaskQueueDuplicateHandle", XTaskQueueDuplicateHandle as usize),
        ("XTaskQueueCloseHandle", XTaskQueueCloseHandle as usize),
        ("XTaskQueueSubmitCallback", XTaskQueueSubmitCallback as usize),
        ("XTaskQueueSubmitDelayedCallback", XTaskQueueSubmitDelayedCallback as usize),
        ("XTaskQueueDispatch", XTaskQueueDispatch as usize),
        ("XTaskQueueIsEmpty", XTaskQueueIsEmpty as usize),
        ("XTaskQueueTerminate", XTaskQueueTerminate as usize),
        ("XTaskQueueGetCurrentProcessTaskQueue", XTaskQueueGetCurrentProcessTaskQueue as usize),
        ("XTaskQueueSetCurrentProcessTaskQueue", XTaskQueueSetCurrentProcessTaskQueue as usize),
        ("XTaskQueueRegisterWaiter", XTaskQueueRegisterWaiter as usize),
        ("XTaskQueueUnregisterWaiter", XTaskQueueUnregisterWaiter as usize),
        ("XTaskQueueRegisterMonitor", XTaskQueueRegisterMonitor as usize),
        ("XTaskQueueUnregisterMonitor", XTaskQueueUnregisterMonitor as usize),
        (
            "XNetworkingQueryPreferredLocalUdpMultiplayerPort",
            XNetworkingQueryPreferredLocalUdpMultiplayerPort as usize,
        ),
        (
            "XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync",
            XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync as usize,
        ),
        (
            "XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult",
            XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult as usize,
        ),
        (
            "XNetworkingGetConnectivityHint",
            XNetworkingGetConnectivityHint as usize,
        ),
        (
            "XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged",
            XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged as usize,
        ),
        (
            "XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged",
            XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged as usize,
        ),
        (
            "XNetworkingRegisterConnectivityHintChanged",
            XNetworkingRegisterConnectivityHintChanged as usize,
        ),
        (
            "XNetworkingUnregisterConnectivityHintChanged",
            XNetworkingUnregisterConnectivityHintChanged as usize,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static HITS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn bump(ctx: *mut c_void, _canceled: bool) {
        HITS.fetch_add(ctx as usize as u32, Ordering::SeqCst);
    }

    unsafe extern "win64" fn counted(ctx: *mut c_void, _canceled: bool) {
        let hits = &*(ctx as *const AtomicU32);
        hits.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn manual_delayed_and_pool_dispatch() {
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            let before = HITS.load(Ordering::SeqCst);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, 1 as _, bump as usize), S_OK);
            assert_eq!(HITS.load(Ordering::SeqCst), before, "manual port must wait for dispatch");
            assert!(XTaskQueueDispatch(h, PORT_WORK, 0));
            assert_eq!(HITS.load(Ordering::SeqCst), before + 1);
            assert!(!XTaskQueueDispatch(h, PORT_WORK, 0));
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 30, 100 as _, bump as usize), S_OK);
            assert!(XTaskQueueDispatch(h, PORT_WORK, 2000));
            assert_eq!(HITS.load(Ordering::SeqCst), before + 101);
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            XTaskQueueCloseHandle(h);

            for mode in [MODE_THREAD_POOL, MODE_SERIALIZED_THREAD_POOL] {
                let mut t = std::ptr::null_mut();
                assert_eq!(XTaskQueueCreate(mode, mode, &mut t), S_OK);
                let b = HITS.load(Ordering::SeqCst);
                for _ in 0..25 {
                    assert_eq!(XTaskQueueSubmitCallback(t, PORT_WORK, 1 as _, bump as usize), S_OK);
                }
                assert_eq!(XTaskQueueTerminate(t, true, std::ptr::null_mut(), 0), S_OK);
                assert_eq!(HITS.load(Ordering::SeqCst), b + 25);
                XTaskQueueCloseHandle(t);
            }
        }
    }

    #[test]
    fn composite_termination_preserves_sibling_work() {
        unsafe {
            let mut base = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut base), S_OK);
            let mut wp = std::ptr::null_mut();
            let mut cp = std::ptr::null_mut();
            assert_eq!(XTaskQueueGetPort(base, PORT_WORK, &mut wp), S_OK);
            assert_eq!(XTaskQueueGetPort(base, PORT_COMPLETION, &mut cp), S_OK);
            let mut comp = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreateComposite(wp, cp, &mut comp), S_OK);
            let hits = AtomicU32::new(0);
            let hits2 = AtomicU32::new(0);
            assert_eq!(
                XTaskQueueSubmitCallback(base, PORT_WORK, &hits as *const AtomicU32 as *mut c_void, counted as usize),
                S_OK
            );
            assert_eq!(
                XTaskQueueSubmitCallback(comp, PORT_WORK, &hits2 as *const AtomicU32 as *mut c_void, counted as usize),
                S_OK
            );
            assert_eq!(XTaskQueueTerminate(comp, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(hits.load(Ordering::SeqCst), 0);
            assert!(XTaskQueueDispatch(base, PORT_WORK, 0));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            XTaskQueueCloseHandle(comp);
            XTaskQueueCloseHandle(base);
        }
    }

    #[test]
    fn is_empty_counts_delayed_callbacks_not_yet_due() {
        unsafe extern "win64" fn noop(_ctx: *mut c_void, _canceled: bool) {}
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut h), S_OK);
            assert!(XTaskQueueIsEmpty(h, PORT_WORK));
            assert_eq!(XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 60_000, std::ptr::null_mut(), noop as usize), S_OK);
            assert!(!XTaskQueueIsEmpty(h, PORT_WORK), "a delayed callback not yet due is still queued");
            assert!(XTaskQueueIsEmpty(h, PORT_COMPLETION), "it is queued on the work port only");
            // termination cancels it: nothing is left
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert!(XTaskQueueIsEmpty(h, PORT_WORK));
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn due_delayed_callback_is_canceled_before_terminate_finishes() {
        struct RaceState {
            item_runs: AtomicU32,
            item_canceled: AtomicBool,
            item_ran_after_term: AtomicBool,
            term_done: AtomicBool,
        }
        unsafe extern "win64" fn item_cb(ctx: *mut c_void, canceled: bool) {
            let s = &*(ctx as *const RaceState);
            if s.term_done.load(Ordering::Acquire) {
                s.item_ran_after_term.store(true, Ordering::Release);
            }
            if canceled {
                s.item_canceled.store(true, Ordering::Release);
            }
            s.item_runs.fetch_add(1, Ordering::SeqCst);
        }
        unsafe extern "win64" fn term_cb(ctx: *mut c_void) {
            let s = &*(ctx as *const RaceState);
            s.term_done.store(true, Ordering::Release);
        }

        // Deterministic check of the pop-then-terminate window: reserve inflight (as timer()
        // does under the timer lock), start XTaskQueueTerminate(wait=true), confirm Terminate
        // waits while terminated==true, then dispatch the due item and verify it runs canceled
        // before term_cb.
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            let q = queue_from(h).unwrap();
            let state = Arc::new(RaceState {
                item_runs: AtomicU32::new(0),
                item_canceled: AtomicBool::new(false),
                item_ran_after_term: AtomicBool::new(false),
                term_done: AtomicBool::new(false),
            });

            q.state.lock().unwrap().inflight[PORT_WORK as usize] += 1;
            let h_addr = h as usize;
            let s_addr = Arc::as_ptr(&state) as usize;
            let term_thread = std::thread::spawn(move || {
                XTaskQueueTerminate(h_addr as *mut c_void, true, s_addr as *mut c_void, term_cb as usize)
            });
            while !q.is_terminated() {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(10));
            assert!(!state.term_done.load(Ordering::Acquire), "Terminate fired term_cb while due timer item was inflight");

            dispatch_due_timed(&q, PORT_WORK, Item { cb: item_cb as usize, ctx: Arc::as_ptr(&state) as usize });
            assert_eq!(term_thread.join().unwrap(), S_OK);
            assert!(state.term_done.load(Ordering::Acquire));
            assert_eq!(state.item_runs.load(Ordering::SeqCst), 1);
            assert!(state.item_canceled.load(Ordering::Acquire));
            assert!(!state.item_ran_after_term.load(Ordering::Acquire));
            XTaskQueueCloseHandle(h);
        }

        // Real timer-thread race over 40 iterations: every accepted delayed callback runs
        // exactly once and never after term_cb.
        for iter in 0..40 {
            unsafe {
                let mut h = std::ptr::null_mut();
                assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
                let state = Arc::new(RaceState {
                    item_runs: AtomicU32::new(0),
                    item_canceled: AtomicBool::new(false),
                    item_ran_after_term: AtomicBool::new(false),
                    term_done: AtomicBool::new(false),
                });
                let hr = XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 1, Arc::as_ptr(&state) as *mut c_void, item_cb as usize);
                assert_eq!(hr, S_OK);
                if iter % 2 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(XTaskQueueTerminate(h, true, Arc::as_ptr(&state) as *mut c_void, term_cb as usize), S_OK);
                assert!(state.term_done.load(Ordering::Acquire));
                assert_eq!(state.item_runs.load(Ordering::SeqCst), 1, "delayed callback dropped or double-run on iter {iter}");
                assert!(!state.item_ran_after_term.load(Ordering::Acquire), "delayed callback ran after term_cb on iter {iter}");
                XTaskQueueCloseHandle(h);
            }
        }
    }

    #[test]
    fn termination_callback_runs_from_a_manual_completion_port_dispatch() {
        unsafe extern "win64" fn terminated(ctx: *mut c_void) {
            *(*(ctx as *const Mutex<Option<std::thread::ThreadId>>)).lock().unwrap() = Some(std::thread::current().id());
        }
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_MANUAL, &mut h), S_OK);
            let ran_on: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);
            let ctx = &ran_on as *const _ as *mut c_void;
            assert_eq!(XTaskQueueTerminate(h, false, ctx, terminated as usize), S_OK);
            // Wait until the callback is queued (or, wrongly, has already run elsewhere).
            let deadline = Instant::now() + Duration::from_secs(2);
            while XTaskQueueIsEmpty(h, PORT_COMPLETION) && ran_on.lock().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(*ran_on.lock().unwrap(), None, "the termination callback ran before the completion port was dispatched");
            assert!(XTaskQueueDispatch(h, PORT_COMPLETION, 0));
            assert_eq!(*ran_on.lock().unwrap(), Some(std::thread::current().id()));
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn thread_pool_callbacks_finish_when_more_block_than_there_are_cores() {
        static POOL_RELEASED: AtomicBool = AtomicBool::new(false);
        static POOL_WAITERS_DONE: AtomicU32 = AtomicU32::new(0);

        unsafe extern "win64" fn waiter(_ctx: *mut c_void, _canceled: bool) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !POOL_RELEASED.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            POOL_WAITERS_DONE.fetch_add(1, Ordering::SeqCst);
        }

        unsafe extern "win64" fn releaser(_ctx: *mut c_void, _canceled: bool) {
            POOL_RELEASED.store(true, Ordering::SeqCst);
        }

        POOL_RELEASED.store(false, Ordering::SeqCst);
        POOL_WAITERS_DONE.store(0, Ordering::SeqCst);

        const WAITERS: u32 = 16;
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            for _ in 0..WAITERS {
                assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), waiter as usize), S_OK);
            }
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), releaser as usize), S_OK);
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            XTaskQueueCloseHandle(h);
        }
        assert_eq!(
            POOL_WAITERS_DONE.load(Ordering::SeqCst),
            WAITERS,
            "blocked pool callbacks starved the releaser callback"
        );
    }

    #[test]
    fn serialized_work_port_can_dispatch_on_the_caller() {
        use std::sync::mpsc;
        const TIMEOUT: Duration = Duration::from_secs(5);

        struct First {
            started: mpsc::Sender<std::thread::ThreadId>,
            release: mpsc::Receiver<()>,
            done: mpsc::Sender<bool>,
        }

        unsafe extern "win64" fn first(ctx: *mut c_void, canceled: bool) {
            let first = Box::from_raw(ctx.cast::<First>());
            let _ = first.started.send(std::thread::current().id());
            let released = first.release.recv_timeout(TIMEOUT).is_ok();
            let _ = first.done.send(released && !canceled);
        }

        type ResultSender = mpsc::Sender<(std::thread::ThreadId, bool)>;

        unsafe extern "win64" fn report_thread(ctx: *mut c_void, canceled: bool) {
            let done = Box::from_raw(ctx.cast::<ResultSender>());
            let _ = done.send((std::thread::current().id(), canceled));
        }

        unsafe fn submit_record(queue: *mut c_void, done: ResultSender) -> HRESULT {
            let ctx = Box::into_raw(Box::new(done));
            let hr = XTaskQueueSubmitCallback(queue, PORT_WORK, ctx.cast(), report_thread as usize);
            if hr != S_OK {
                drop(Box::from_raw(ctx));
            }
            hr
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (first_done_tx, first_done_rx) = mpsc::channel();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let (third_done_tx, third_done_rx) = mpsc::channel();
        let caller = std::thread::current().id();

        unsafe {
            let mut queue = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_SERIALIZED_THREAD_POOL, MODE_MANUAL, &mut queue), S_OK);

            // Callback contexts own their channels even if the caller times out.
            let first_context = Box::into_raw(Box::new(First { started: started_tx, release: release_rx, done: first_done_tx }));
            let first_hr = XTaskQueueSubmitCallback(queue, PORT_WORK, first_context.cast(), first as usize);
            if first_hr != S_OK {
                drop(Box::from_raw(first_context));
            }
            let first_thread = if first_hr == S_OK { started_rx.recv_timeout(TIMEOUT).ok() } else { None };
            let empty_dispatch = first_thread.is_some() && XTaskQueueDispatch(queue, PORT_WORK, 0);

            let mut second_hr = crate::util::E_ABORT;
            let mut third_hr = crate::util::E_ABORT;
            if first_thread.is_some() {
                second_hr = submit_record(queue, second_done_tx);
                third_hr = submit_record(queue, third_done_tx);
            }
            let dispatched = second_hr == S_OK && XTaskQueueDispatch(queue, PORT_WORK, 0);
            let early = second_done_rx.try_recv().ok();
            let before_release = early.is_some();
            let third_early = third_done_rx.try_recv().ok();

            let _ = release_tx.send(());
            let first_released = first_thread.is_some() && first_done_rx.recv_timeout(TIMEOUT) == Ok(true);
            let second_done = if second_hr == S_OK {
                early.or_else(|| second_done_rx.recv_timeout(TIMEOUT).ok())
            } else {
                None
            };
            let third_done = if third_hr == S_OK {
                third_early.or_else(|| third_done_rx.recv_timeout(TIMEOUT).ok())
            } else {
                None
            };
            XTaskQueueCloseHandle(queue);

            let on_caller = second_done.as_ref().is_some_and(|(thread, _)| thread == &caller);
            println!("serialized dispatch: dispatched={dispatched} before_release={before_release} on_caller={on_caller}");
            assert_eq!(first_hr, S_OK);
            assert!(first_released, "first callback must finish after the explicit release");
            assert_ne!(first_thread, Some(caller), "automatic dispatch must use a pool worker");
            assert!(!empty_dispatch, "an in-flight callback is not queued work");
            assert_eq!(second_hr, S_OK);
            assert_eq!(third_hr, S_OK);
            assert_eq!(second_done.as_ref().map(|(_, canceled)| *canceled), Some(false));
            assert!(dispatched && before_release && on_caller, "explicit dispatch must run the queued callback on its caller");
            assert!(third_early.is_none(), "one explicit dispatch must consume only one callback");
            assert_eq!(third_done, first_thread.map(|thread| (thread, false)), "the original worker must keep draining the port");
        }
    }

    #[test]
    fn networking_reports_offline_console_values() {
        unsafe {
            let mut port = 0u16;
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPort(&mut port), S_OK);
            assert_eq!(port, 3074);
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPort(std::ptr::null_mut()), E_INVALIDARG);
            let mut hint: XNetworkingConnectivityHint = std::mem::zeroed();
            assert_eq!(XNetworkingGetConnectivityHint(&mut hint), S_OK);
            // `XCL_NETWORK=0` is unset in the test env: InternetAccess (3). The off value (1)
            // is pinned by `connectivity_level_without_network_is_none` without mutating env.
            assert_eq!(hint.connectivity_level, 3);
            assert_eq!(hint.iana_interface_type, 6);
            assert!(hint.network_initialized);
            let bytes = std::slice::from_raw_parts(&hint as *const _ as *const u8, 16);
            assert_eq!(bytes[12], 1, "hint[12] is network_initialized");
        }
    }

    #[test]
    fn connectivity_level_without_network_is_none() {
        // Mirrors the Windows `connectivity_level(host, off)` contract with no host to ask
        // (Linux has no `iphlpapi`): off means None (1), on means InternetAccess (3).
        assert_eq!(connectivity_level(true), 1);
        assert_eq!(connectivity_level(false), 3);
        assert!(!network_off(), "test env must not set XCL_NETWORK=0");
    }

    #[test]
    fn networking_registers_fire_an_initial_callback() {
        use std::sync::Mutex;
        struct Seen {
            calls: AtomicU32,
            port: AtomicU32,
            hint: Mutex<Option<XNetworkingConnectivityHint>>,
        }
        unsafe extern "win64" fn on_port(ctx: *mut c_void, port: u16) {
            let seen = &*(ctx as *const Seen);
            seen.port.store(port as u32, Ordering::SeqCst);
            seen.calls.fetch_add(1, Ordering::SeqCst);
        }
        unsafe extern "win64" fn on_hint(ctx: *mut c_void, hint: *const XNetworkingConnectivityHint) {
            let seen = &*(ctx as *const Seen);
            *seen.hint.lock().unwrap() = Some(*hint);
            seen.calls.fetch_add(1, Ordering::SeqCst);
        }
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_IMMEDIATE, MODE_IMMEDIATE, &mut q), S_OK);
            let seen = Seen { calls: AtomicU32::new(0), port: AtomicU32::new(0), hint: Mutex::new(None) };
            let ctx = &seen as *const Seen as *mut c_void;
            let mut token = 0u64;
            assert_eq!(XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(q, ctx, on_port as usize, &mut token), S_OK);
            assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
            assert_eq!(seen.port.load(Ordering::SeqCst), DEFAULT_UDP_PORT as u32);
            assert!(XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged(token, true));

            seen.calls.store(0, Ordering::SeqCst);
            assert_eq!(XNetworkingRegisterConnectivityHintChanged(q, ctx, on_hint as usize, &mut token), S_OK);
            assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
            let hint = seen.hint.lock().unwrap().expect("no connectivity hint callback");
            assert!(hint.network_initialized);
            assert!((1..=4).contains(&hint.connectivity_level));
            assert!(XNetworkingUnregisterConnectivityHintChanged(token, true));

            // the callback is required: null is E_INVALIDARG and queues nothing.
            assert_eq!(XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(q, ctx, 0, &mut token), E_INVALIDARG);
            assert_eq!(XNetworkingRegisterConnectivityHintChanged(q, ctx, 0, &mut token), E_INVALIDARG);
            assert_eq!(seen.calls.load(Ordering::SeqCst), 1);

            XTaskQueueCloseHandle(q);
        }
    }

    #[test]
    fn loopback_detection_covers_subnets() {
        // Portable half of #1382: the whole 127.0.0.0/8 plus ::1/localhost count as loopback.
        assert_eq!(url_host_and_is_plain_loopback("http://127.0.0.1:3551/account/api/oauth/token"), ("127.0.0.1".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("http://127.0.0.2:8080/test"), ("127.0.0.2".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("http://127.1.2.3:8080/test"), ("127.1.2.3".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("https://127.0.0.1:3551/secure"), ("127.0.0.1".into(), false));
        assert_eq!(url_host_and_is_plain_loopback("http://[::1]:8080/test"), ("::1".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("https://social.xboxlive.com/users"), ("social.xboxlive.com".into(), false));
        assert_eq!(url_host_and_is_plain_loopback("http://user:pass@127.0.0.1:8080/test"), ("127.0.0.1".into(), true));
        assert_eq!(url_host_and_is_plain_loopback("http://[::1/bad"), ("".into(), false));
        assert_eq!(url_host_and_is_plain_loopback(""), ("".into(), false));
    }

    #[test]
    fn waiter_fires_once_on_signaled_event() {
        use crate::hle::kernelx_linux::{CloseHandle, CreateEventW, SetEvent};
        unsafe extern "win64" fn count_hit(ctx: *mut c_void, _canceled: bool) {
            let hits = &*(ctx as *const AtomicU32);
            hits.fetch_add(1, Ordering::SeqCst);
        }
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            assert_ne!(event, 0);
            // Out-of-bounds port is rejected instead of panicking in the waiter thread.
            let mut bad = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, 2, event as *mut c_void, std::ptr::null_mut(), count_hit as usize, &mut bad),
                E_INVALIDARG
            );
            let hits = AtomicU32::new(0);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(
                    h,
                    PORT_WORK,
                    event as *mut c_void,
                    &hits as *const AtomicU32 as *mut c_void,
                    count_hit as usize,
                    &mut tok
                ),
                S_OK
            );
            assert_ne!(tok.token, 0);
            assert_ne!(SetEvent(event), 0);
            let deadline = Instant::now() + Duration::from_secs(2);
            while hits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(hits.load(Ordering::SeqCst), 1, "signaled waiter never fired");
            XTaskQueueUnregisterWaiter(h, tok);
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(CloseHandle(event), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn waiter_unregister_is_idempotent_and_bogus_tokens_are_safe() {
        use crate::hle::kernelx_linux::{CloseHandle, CreateEventW};
        unsafe extern "win64" fn noop(_ctx: *mut c_void, _canceled: bool) {}
        unsafe {
            // Zero/bogus tokens on no queue must be safe no-ops.
            XTaskQueueUnregisterWaiter(std::ptr::null_mut(), RegistrationToken { token: 0 });
            XTaskQueueUnregisterWaiter(std::ptr::null_mut(), RegistrationToken { token: 0xDEAD_BEEF_CAFE_BABE });
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            assert_ne!(event, 0);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(h, PORT_WORK, event as *mut c_void, std::ptr::null_mut(), noop as usize, &mut tok),
                S_OK
            );
            XTaskQueueUnregisterWaiter(h, RegistrationToken { token: tok.token });
            // Duplicate and bogus unregisters are safe no-ops, never a double-free.
            XTaskQueueUnregisterWaiter(h, RegistrationToken { token: tok.token });
            XTaskQueueUnregisterWaiter(h, RegistrationToken { token: 0x1235 });
            assert!(!waiters().lock().unwrap().contains_key(&tok.token));
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(CloseHandle(event), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn waiter_self_unregister_does_not_deadlock() {
        use crate::hle::kernelx_linux::{CloseHandle, CreateEventW, SetEvent};
        struct SelfCtx {
            queue: usize,
            token: AtomicU64,
            done: (Mutex<bool>, Condvar),
        }
        unsafe extern "win64" fn self_unregister_cb(ctx: *mut c_void, _canceled: bool) {
            let c = &*(ctx as *const SelfCtx);
            let mut tok = c.token.load(Ordering::Acquire);
            while tok == 0 {
                std::thread::yield_now();
                tok = c.token.load(Ordering::Acquire);
            }
            XTaskQueueUnregisterWaiter(c.queue as *mut c_void, RegistrationToken { token: tok });
            // Duplicate unregister from the same callback must also be a safe no-op.
            XTaskQueueUnregisterWaiter(c.queue as *mut c_void, RegistrationToken { token: tok });
            let mut d = c.done.0.lock().unwrap();
            *d = true;
            c.done.1.notify_all();
        }
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_IMMEDIATE, MODE_IMMEDIATE, &mut h), S_OK);
            let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            assert_ne!(event, 0);
            let ctx = Box::new(SelfCtx {
                queue: h as usize,
                token: AtomicU64::new(0),
                done: (Mutex::new(false), Condvar::new()),
            });
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(
                    h,
                    PORT_WORK,
                    event as *mut c_void,
                    &*ctx as *const SelfCtx as *mut c_void,
                    self_unregister_cb as usize,
                    &mut tok
                ),
                S_OK
            );
            ctx.token.store(tok.token, Ordering::Release);
            assert_ne!(SetEvent(event), 0);
            let (d, timed_out) = ctx
                .done
                .1
                .wait_timeout_while(ctx.done.0.lock().unwrap(), Duration::from_secs(2), |done| !*done)
                .unwrap();
            assert!(*d && !timed_out.timed_out(), "self-unregister inside waiter callback deadlocked");
            drop(d);
            assert!(!waiters().lock().unwrap().contains_key(&tok.token));
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(CloseHandle(event), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn waiter_terminated_queue_cleans_up_waiters() {
        use crate::hle::kernelx_linux::{CloseHandle, CreateEventW, SetEvent};
        unsafe extern "win64" fn count_hit(ctx: *mut c_void, _canceled: bool) {
            let hits = &*(ctx as *const AtomicU32);
            hits.fetch_add(1, Ordering::SeqCst);
        }
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_THREAD_POOL, MODE_THREAD_POOL, &mut h), S_OK);
            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            assert_ne!(event, 0);
            let hits = AtomicU32::new(0);
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterWaiter(
                    h,
                    PORT_WORK,
                    event as *mut c_void,
                    &hits as *const AtomicU32 as *mut c_void,
                    count_hit as usize,
                    &mut tok
                ),
                S_OK
            );
            assert!(waiters().lock().unwrap().contains_key(&tok.token));
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert!(!waiters().lock().unwrap().contains_key(&tok.token));
            // Late unregister is a safe no-op; a post-terminate signal never fires.
            XTaskQueueUnregisterWaiter(h, tok);
            assert_ne!(SetEvent(event), 0);
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(hits.load(Ordering::SeqCst), 0, "waiter fired after Terminate");
            assert_eq!(CloseHandle(event), 1);
            XTaskQueueCloseHandle(h);
        }
    }

    #[test]
    fn taskqueue_functions_null_outptrs_on_failure() {
        unsafe {
            let mut q_out = 0xDEADBEEF as *mut c_void;
            assert_eq!(XTaskQueueCreate(99, 0, &mut q_out), E_INVALIDARG);
            assert_eq!(q_out, std::ptr::null_mut());

            let mut p_out = 0xDEADBEEF as *mut c_void;
            assert_eq!(XTaskQueueGetPort(0x1234 as *mut c_void, 0, &mut p_out), E_INVALIDARG);
            assert_eq!(p_out, std::ptr::null_mut());

            let mut p_out2 = 0xDEADBEEF as *mut c_void;
            assert_eq!(XTaskQueueGetPort(std::ptr::null_mut(), 99, &mut p_out2), E_INVALIDARG);
            assert_eq!(p_out2, std::ptr::null_mut());

            let mut comp_out = 0xDEADBEEF as *mut c_void;
            assert_eq!(XTaskQueueCreateComposite(std::ptr::null_mut(), std::ptr::null_mut(), &mut comp_out), E_INVALIDARG);
            assert_eq!(comp_out, std::ptr::null_mut());

            let mut dup_out = 0xDEADBEEF as *mut c_void;
            assert_eq!(XTaskQueueDuplicateHandle(0x1234 as *mut c_void, &mut dup_out), E_INVALIDARG);
            assert_eq!(dup_out, std::ptr::null_mut());
        }
    }

    #[test]
    fn unregister_monitor_waits_for_an_active_invocation() {
        struct Capture {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            calls: AtomicU32,
        }

        unsafe extern "win64" fn monitor(ctx: *mut c_void, _: *mut c_void, _: u32) {
            let capture = &*ctx.cast::<Capture>();
            if capture.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                let _ = capture.entered.send(());
                let _ = capture.release.lock().unwrap().recv_timeout(Duration::from_secs(5));
            }
        }

        unsafe extern "win64" fn work(_: *mut c_void, _: bool) {}

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let capture = Box::new(Capture { entered: entered_tx, release: Mutex::new(release_rx), calls: AtomicU32::new(0) });
        let mut h = std::ptr::null_mut();
        let mut token = RegistrationToken { token: 0 };
        unsafe {
            assert_eq!(XTaskQueueCreate(MODE_IMMEDIATE, MODE_IMMEDIATE, &mut h), S_OK);
            assert_eq!(XTaskQueueRegisterMonitor(h, (&*capture as *const Capture).cast_mut().cast(), monitor as usize, &mut token), S_OK);
        }

        let handle = h as usize;
        let submit = std::thread::spawn(move || unsafe {
            XTaskQueueSubmitCallback(handle as *mut c_void, PORT_WORK, std::ptr::null_mut(), work as usize)
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(2));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let unregister: [_; 2] = std::array::from_fn(|_| {
            let started = started_tx.clone();
            let finished = finished_tx.clone();
            let id = token.token;
            std::thread::spawn(move || unsafe {
                let _ = started.send(());
                XTaskQueueUnregisterMonitor(handle as *mut c_void, RegistrationToken { token: id });
                let _ = finished.send(());
            })
        });
        let started = (0..2).all(|_| started_rx.recv_timeout(Duration::from_secs(2)).is_ok());
        let early = finished_rx.recv_timeout(Duration::from_millis(250));
        let _ = release_tx.send(());
        let remaining = 2 - usize::from(early.is_ok());
        let finished = (0..remaining).all(|_| finished_rx.recv_timeout(Duration::from_secs(2)).is_ok());
        let submitted = submit.join().unwrap();
        for call in unregister {
            call.join().unwrap();
        }
        let second = unsafe { XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), work as usize) };
        unsafe { XTaskQueueCloseHandle(h) };

        assert!(entered.is_ok() && started && finished, "monitor control threads did not finish their steps");
        assert!(matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout)), "unregister returned before the active monitor ended");
        assert_eq!((submitted, second), (S_OK, S_OK));
        assert_eq!(capture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unregister_monitor_inside_its_callback_stops_later_invocations() {
        struct Capture {
            token: RegistrationToken,
            calls: u32,
        }

        unsafe extern "win64" fn monitor(ctx: *mut c_void, queue: *mut c_void, _: u32) {
            let capture = &mut *ctx.cast::<Capture>();
            capture.calls += 1;
            XTaskQueueUnregisterMonitor(queue, RegistrationToken { token: capture.token.token });
        }

        unsafe extern "win64" fn work(_: *mut c_void, _: bool) {}

        let mut capture = Capture { token: RegistrationToken { token: 0 }, calls: 0 };
        let mut h = std::ptr::null_mut();
        unsafe {
            assert_eq!(XTaskQueueCreate(MODE_IMMEDIATE, MODE_IMMEDIATE, &mut h), S_OK);
            let ctx = (&mut capture as *mut Capture).cast();
            assert_eq!(XTaskQueueRegisterMonitor(h, ctx, monitor as usize, &mut capture.token), S_OK);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), work as usize), S_OK);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, std::ptr::null_mut(), work as usize), S_OK);
            XTaskQueueCloseHandle(h);
        }
        assert_eq!(capture.calls, 1);
    }

    #[test]
    fn unregister_monitor_skips_a_revoked_snapshot_entry() {
        struct Gate {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }

        unsafe extern "win64" fn hold(ctx: *mut c_void, _: *mut c_void, _: u32) {
            let gate = &*ctx.cast::<Gate>();
            let _ = gate.entered.send(());
            let _ = gate.release.lock().unwrap().recv_timeout(Duration::from_secs(5));
        }

        unsafe extern "win64" fn count(ctx: *mut c_void, _: *mut c_void, _: u32) {
            (*ctx.cast::<AtomicU32>()).fetch_add(1, Ordering::Relaxed);
        }

        unsafe extern "win64" fn work(_: *mut c_void, _: bool) {}

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let gate = Gate { entered: entered_tx, release: Mutex::new(release_rx) };
        let calls = AtomicU32::new(0);
        let mut h = std::ptr::null_mut();
        let mut blocker = RegistrationToken { token: 0 };
        let mut revoked = RegistrationToken { token: 0 };
        unsafe {
            assert_eq!(XTaskQueueCreate(MODE_IMMEDIATE, MODE_IMMEDIATE, &mut h), S_OK);
            assert_eq!(XTaskQueueRegisterMonitor(h, (&gate as *const Gate).cast_mut().cast(), hold as usize, &mut blocker), S_OK);
            assert_eq!(XTaskQueueRegisterMonitor(h, (&calls as *const AtomicU32).cast_mut().cast(), count as usize, &mut revoked), S_OK);
        }

        let handle = h as usize;
        let submit = std::thread::spawn(move || unsafe {
            XTaskQueueSubmitCallback(handle as *mut c_void, PORT_WORK, std::ptr::null_mut(), work as usize)
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(2));
        unsafe { XTaskQueueUnregisterMonitor(h, revoked) };
        let _ = release_tx.send(());
        let submitted = submit.join().unwrap();
        unsafe {
            XTaskQueueUnregisterMonitor(h, blocker);
            XTaskQueueCloseHandle(h);
        }

        assert!(entered.is_ok(), "first monitor did not hold the notification snapshot");
        assert_eq!(submitted, S_OK);
        assert_eq!(calls.load(Ordering::Relaxed), 0, "revoked monitor still ran from an old snapshot");
    }

    #[test]
    fn monitors_fire_across_composite_ports_and_outlive_registering_handle() {
        struct MonitorCapture {
            events: Mutex<Vec<(usize, u32, bool, HRESULT)>>,
            duplicated: Mutex<Vec<usize>>,
            dispatch_inline: bool,
        }

        unsafe extern "win64" fn noop(_ctx: *mut c_void, _canceled: bool) {}
        unsafe extern "win64" fn inc_u32(ctx: *mut c_void, canceled: bool) {
            if !canceled {
                (*(ctx as *const AtomicU32)).fetch_add(1, Ordering::SeqCst);
            }
        }
        unsafe extern "win64" fn capture_monitor(ctx: *mut c_void, q: *mut c_void, port: u32) {
            let cap = &*(ctx as *const MonitorCapture);
            let mut dup = std::ptr::null_mut();
            let dup_hr = XTaskQueueDuplicateHandle(q, &mut dup);
            if dup_hr >= 0 && !dup.is_null() {
                cap.duplicated.lock().unwrap().push(dup as usize);
            }
            let dispatched = if cap.dispatch_inline { XTaskQueueDispatch(q, port, 0) } else { false };
            cap.events.lock().unwrap().push((q as usize, port, dispatched, dup_hr));
        }

        unsafe {
            // 1. Composite queue sharing a manual pump queue's ports: submitting to the composite
            // queue must notify the pump queue's monitor with the pump queue's attached port index.
            let mut main_q = std::ptr::null_mut();
            let mut bg_q = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut main_q), S_OK);
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_MANUAL, &mut bg_q), S_OK);

            let mut bg_work = std::ptr::null_mut();
            let mut main_comp = std::ptr::null_mut();
            let mut main_work = std::ptr::null_mut();
            assert_eq!(XTaskQueueGetPort(bg_q, PORT_WORK, &mut bg_work), S_OK);
            assert_eq!(XTaskQueueGetPort(main_q, PORT_COMPLETION, &mut main_comp), S_OK);
            assert_eq!(XTaskQueueGetPort(main_q, PORT_WORK, &mut main_work), S_OK);

            let mut comp_q = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreateComposite(bg_work, main_comp, &mut comp_q), S_OK);
            // Cross-slot composite: `main_q`'s WORK port attached as `comp_cross`'s COMPLETION port.
            let mut comp_cross = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreateComposite(bg_work, main_work, &mut comp_cross), S_OK);

            let cap = MonitorCapture {
                events: Mutex::new(Vec::new()),
                duplicated: Mutex::new(Vec::new()),
                dispatch_inline: true,
            };
            let mut tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(main_q, &cap as *const MonitorCapture as *mut c_void, capture_monitor as usize, &mut tok),
                S_OK
            );

            let ran = AtomicU32::new(0);
            assert_eq!(XTaskQueueSubmitCallback(comp_q, PORT_COMPLETION, &ran as *const AtomicU32 as *mut c_void, inc_u32 as usize), S_OK);
            assert_eq!(ran.load(Ordering::SeqCst), 1, "monitor on main_q did not wake/dispatch composite completion");
            // Each callback's temporary handle must be gone as soon as its callback returns. Checked
            // right away: a later `XTaskQueueDuplicateHandle` can be given the freed handle's address.
            let temp0 = cap.events.lock().unwrap()[0].0;
            assert!(queue_from(temp0 as *mut c_void).is_none(), "temporary monitor handle leaked past callback");
            assert_eq!(XTaskQueueSubmitCallback(comp_cross, PORT_COMPLETION, &ran as *const AtomicU32 as *mut c_void, inc_u32 as usize), S_OK);
            assert_eq!(ran.load(Ordering::SeqCst), 2, "cross-slot composite port did not notify main_q on PORT_WORK");
            let temp1 = cap.events.lock().unwrap()[1].0;
            assert!(queue_from(temp1 as *mut c_void).is_none(), "temporary monitor handle leaked past callback");
            // Submitting to `comp_q`'s `bg_work` port must NOT notify `main_q`'s monitor.
            assert_eq!(XTaskQueueSubmitCallback(comp_q, PORT_WORK, std::ptr::null_mut(), noop as usize), S_OK);
            assert!(XTaskQueueDispatch(bg_q, PORT_WORK, 0));

            {
                let ev = cap.events.lock().unwrap();
                assert_eq!(ev.len(), 2);
                assert_eq!((ev[0].1, ev[0].2, ev[0].3), (PORT_COMPLETION, true, S_OK));
                assert_eq!((ev[1].1, ev[1].2, ev[1].3), (PORT_WORK, true, S_OK));
            }
            for dup in cap.duplicated.lock().unwrap().drain(..) {
                assert!(queue_from(dup as *mut c_void).is_some(), "duplicated handle from monitor must stay valid");
                XTaskQueueCloseHandle(dup as *mut c_void);
            }

            // 2. Closing the handle used at registration (while a duplicate stays open) and
            // registering on the NULL process queue both pass a valid temporary handle to callbacks.
            let mut dup_main = std::ptr::null_mut();
            assert_eq!(XTaskQueueDuplicateHandle(main_q, &mut dup_main), S_OK);
            XTaskQueueCloseHandle(main_q);
            cap.events.lock().unwrap().clear();
            assert_eq!(XTaskQueueSubmitCallback(dup_main, PORT_COMPLETION, &ran as *const AtomicU32 as *mut c_void, inc_u32 as usize), S_OK);
            assert_eq!(ran.load(Ordering::SeqCst), 3);
            {
                let ev = cap.events.lock().unwrap();
                assert_eq!(ev.len(), 1);
                assert_ne!(ev[0].0, 0);
                assert_eq!((ev[0].1, ev[0].2, ev[0].3), (PORT_COMPLETION, true, S_OK));
            }
            for dup in cap.duplicated.lock().unwrap().drain(..) {
                XTaskQueueCloseHandle(dup as *mut c_void);
            }

            let proc_cap = MonitorCapture {
                events: Mutex::new(Vec::new()),
                duplicated: Mutex::new(Vec::new()),
                dispatch_inline: false,
            };
            let mut proc_tok = RegistrationToken { token: 0 };
            assert_eq!(
                XTaskQueueRegisterMonitor(std::ptr::null_mut(), &proc_cap as *const MonitorCapture as *mut c_void, capture_monitor as usize, &mut proc_tok),
                S_OK
            );
            assert_eq!(XTaskQueueSubmitCallback(std::ptr::null_mut(), PORT_WORK, std::ptr::null_mut(), noop as usize), S_OK);
            XTaskQueueUnregisterMonitor(std::ptr::null_mut(), proc_tok);
            {
                let ev = proc_cap.events.lock().unwrap();
                assert_eq!(ev.len(), 1);
                assert_ne!(ev[0].0, 0, "NULL process-queue monitor received a null handle");
                assert_eq!((ev[0].1, ev[0].3), (PORT_WORK, S_OK));
            }
            for dup in proc_cap.duplicated.lock().unwrap().drain(..) {
                XTaskQueueCloseHandle(dup as *mut c_void);
            }

            // 3. Terminating the monitored queue stops notifications from sibling composite queues.
            cap.events.lock().unwrap().clear();
            assert_eq!(XTaskQueueTerminate(dup_main, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(XTaskQueueSubmitCallback(comp_q, PORT_COMPLETION, std::ptr::null_mut(), noop as usize), S_OK);
            assert!(XTaskQueueDispatch(comp_q, PORT_COMPLETION, 0));
            assert!(cap.events.lock().unwrap().is_empty(), "terminated queue still fired monitor");

            XTaskQueueTerminate(comp_cross, true, std::ptr::null_mut(), 0);
            XTaskQueueTerminate(comp_q, true, std::ptr::null_mut(), 0);
            XTaskQueueTerminate(bg_q, true, std::ptr::null_mut(), 0);
            XTaskQueueCloseHandle(comp_cross);
            XTaskQueueCloseHandle(comp_q);
            XTaskQueueCloseHandle(bg_q);
            XTaskQueueCloseHandle(dup_main);
        }
    }
}
