//! Linux XTaskQueue slice (Phase C step 11o): manual/delayed/pool dispatch.
//!
//! The Windows `hle/xgameruntime/taskqueue.rs` stays the implementation source of truth
//! (GDK `XTaskQueue.h` contract, libHttpClient semantics). On Linux there is no
//! `RegisterWaitForSingleObject`, so waiter registration is a logged `E_NOTIMPL`;
//! everything the test title's `XTaskQueue:` checks need — manual/delayed/thread-pool
//! submit, `Dispatch` with timeout, `Terminate` (canceled drain + delayed cancel),
//! composite ports sharing, duplicate/close handles, `IsEmpty`, process queue,
//! monitors, plus the `XNetworking` UDP-port/connectivity shims — is real `win64`
//! code here so the imports resolve as HLE. `XAsync`-backed variants stay `Stub`
//! until the async slice lands.

#![allow(non_snake_case)]

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::{E_ABORT, E_INVALIDARG, E_NOTIMPL, HRESULT, S_OK};

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

#[derive(Clone, Copy)]
struct Monitor {
    token: u64,
    queue_handle: usize,
    ctx: usize,
    cb: usize,
}

pub struct Port {
    mode: u32,
    items: Mutex<VecDeque<QueuedItem>>,
    cv: Condvar,
    running: AtomicBool,
}

impl Port {
    fn new(mode: u32) -> Arc<Port> {
        Arc::new(Port {
            mode,
            items: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            running: AtomicBool::new(false),
        })
    }
}

struct QueueState {
    terminated: bool,
    inflight: [usize; 2],
}

pub struct Queue {
    ports: [Arc<Port>; 2],
    #[allow(dead_code)]
    owns_ports: bool,
    state: Mutex<QueueState>,
    idle_cv: Condvar,
    monitors: Mutex<Vec<Monitor>>,
}

pub struct QueueHandle {
    magic: u64,
    pub queue: Arc<Queue>,
}

const MAGIC: u64 = 0x5843_4C51_5545_5545; // "XCLQUEUE"

type Job = Box<dyn FnOnce() + Send>;

fn pool() -> &'static Mutex<mpsc::Sender<Job>> {
    static POOL: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        for i in 0..n {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("xcl-taskqueue-{i}"))
                .spawn(move || loop {
                    let job = { rx.lock().unwrap().recv() };
                    match job {
                        Ok(j) => j(),
                        Err(_) => break,
                    }
                })
                .expect("spawn task queue worker");
        }
        Mutex::new(tx)
    })
}

fn spawn(job: Job) {
    let _ = pool().lock().unwrap().send(job);
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
                            drop(g);
                            let _ = submit_now(&t.queue, t.port, t.item);
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
        Arc::new(Queue {
            ports: [Port::new(work_mode), Port::new(completion_mode)],
            owns_ports: true,
            state: Mutex::new(QueueState { terminated: false, inflight: [0, 0] }),
            idle_cv: Condvar::new(),
            monitors: Mutex::new(Vec::new()),
        })
    }

    pub fn composite(work: Arc<Port>, completion: Arc<Port>) -> Arc<Queue> {
        Arc::new(Queue {
            ports: [work, completion],
            owns_ports: false,
            state: Mutex::new(QueueState { terminated: false, inflight: [0, 0] }),
            idle_cv: Condvar::new(),
            monitors: Mutex::new(Vec::new()),
        })
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

    unsafe fn execute(self: &Arc<Queue>, port: u32, item: Item) {
        let canceled = self.is_terminated();
        item.run(canceled);
        self.done_one(port);
    }

    fn notify_monitors(&self, port: u32) {
        let snapshot: Vec<Monitor> = self.monitors.lock().unwrap().clone();
        for m in snapshot {
            unsafe {
                let f: XTaskQueueMonitorCallback = std::mem::transmute(m.cb);
                f(m.ctx as *mut c_void, m.queue_handle as *mut c_void, port);
            }
        }
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
    let p = &q.ports[port as usize];
    match p.mode {
        MODE_IMMEDIATE => {
            {
                let mut state = q.state.lock().unwrap();
                if state.terminated {
                    return false;
                }
                state.inflight[port as usize] += 1;
            }
            q.notify_monitors(port);
            unsafe { q.execute(port, item) };
        }
        MODE_THREAD_POOL => {
            {
                let mut state = q.state.lock().unwrap();
                if state.terminated {
                    return false;
                }
                state.inflight[port as usize] += 1;
            }
            q.notify_monitors(port);
            let q2 = q.clone();
            spawn(Box::new(move || unsafe { q2.execute(port, item) }));
        }
        MODE_SERIALIZED_THREAD_POOL => {
            {
                let mut items = p.items.lock().unwrap();
                {
                    let mut state = q.state.lock().unwrap();
                    if state.terminated {
                        return false;
                    }
                    state.inflight[port as usize] += 1;
                }
                items.push_back(QueuedItem { item, queue: q.clone(), port });
            }
            q.notify_monitors(port);
            if !p.running.swap(true, Ordering::AcqRel) {
                let p2 = p.clone();
                spawn(Box::new(move || drain_serialized(p2)));
            }
        }
        _ => {
            {
                let mut items = p.items.lock().unwrap();
                {
                    let mut state = q.state.lock().unwrap();
                    if state.terminated {
                        return false;
                    }
                    state.inflight[port as usize] += 1;
                }
                items.push_back(QueuedItem { item, queue: q.clone(), port });
            }
            p.cv.notify_one();
            q.notify_monitors(port);
        }
    }
    true
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

pub fn dispatch(q: &Arc<Queue>, port: u32, timeout_ms: u32) -> bool {
    let p = &q.ports[port as usize];
    if p.mode != MODE_MANUAL {
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
        while g.is_empty() && !q.is_terminated() {
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
    if out.is_null() || work > 3 || completion > 3 {
        return E_INVALIDARG;
    }
    *out = new_handle(Queue::new(work, completion)) as *mut c_void;
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueGetPort(h: *mut c_void, port: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || port > 1 {
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
            let items = q.ports[port as usize].items.lock().unwrap();
            let state = q.state.lock().unwrap();
            !items.iter().any(|qi| Arc::ptr_eq(&qi.queue, &q)) && state.inflight[port as usize] == 0
        }
        _ => true,
    }
}

pub unsafe extern "win64" fn XTaskQueueTerminate(h: *mut c_void, wait: bool, ctx: *mut c_void, cb: usize) -> HRESULT {
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    q.state.lock().unwrap().terminated = true;
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
    let ctx = ctx as usize;
    let finish = move || {
        let mut g = q.state.lock().unwrap();
        while g.inflight[0] != 0 || g.inflight[1] != 0 {
            g = q.idle_cv.wait(g).unwrap();
        }
        drop(g);
        if cb != 0 {
            let f: XTaskQueueTerminatedCallback = std::mem::transmute(cb);
            f(ctx as *mut c_void);
        }
    };
    if wait {
        finish();
    } else {
        spawn(Box::new(finish));
    }
    S_OK
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

pub unsafe extern "win64" fn XTaskQueueRegisterWaiter(
    h: *mut c_void,
    port: u32,
    _handle: *mut c_void,
    _ctx: *mut c_void,
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
    crate::xlog_warn!("XTaskQueueRegisterWaiter: no host wait objects on Linux (E_NOTIMPL)");
    E_NOTIMPL
}

pub unsafe extern "win64" fn XTaskQueueUnregisterWaiter(_h: *mut c_void, _token: RegistrationToken) {}

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
    q.monitors.lock().unwrap().push(Monitor {
        token: id,
        queue_handle: h as usize,
        ctx: ctx as usize,
        cb,
    });
    (*token).token = id;
    S_OK
}

pub unsafe extern "win64" fn XTaskQueueUnregisterMonitor(h: *mut c_void, token: RegistrationToken) {
    let Some(q) = queue_from(h) else { return };
    q.monitors.lock().unwrap().retain(|m| m.token != token.token);
}

// ---------------------------------------------------------------------------------------
// XNetworking offline shims (title-visible subset)
// ---------------------------------------------------------------------------------------

pub const DEFAULT_UDP_PORT: u16 = 3074;

pub unsafe extern "win64" fn XNetworkingQueryPreferredLocalUdpMultiplayerPort(port: *mut u16) -> HRESULT {
    if port.is_null() {
        return E_INVALIDARG;
    }
    *port = DEFAULT_UDP_PORT;
    S_OK
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
    // Offline headless: report InternetAccess over Ethernet, initialized, unmetered.
    *out = XNetworkingConnectivityHint {
        connectivity_level: 3,
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

pub unsafe extern "win64" fn XNetworkingRegisterPreferredLocalUdpMultiplayerPortChanged(
    _queue: *mut c_void,
    _ctx: *mut c_void,
    _cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    *token = NEXT_NET_TOKEN.fetch_add(1, Ordering::Relaxed);
    S_OK
}

pub unsafe extern "win64" fn XNetworkingUnregisterPreferredLocalUdpMultiplayerPortChanged(_token: u64, _wait: bool) -> bool {
    true
}

pub unsafe extern "win64" fn XNetworkingRegisterConnectivityHintChanged(
    _queue: *mut c_void,
    _ctx: *mut c_void,
    _cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    *token = NEXT_NET_TOKEN.fetch_add(1, Ordering::Relaxed);
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
    fn networking_reports_offline_console_values() {
        unsafe {
            let mut port = 0u16;
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPort(&mut port), S_OK);
            assert_eq!(port, 3074);
            assert_eq!(XNetworkingQueryPreferredLocalUdpMultiplayerPort(std::ptr::null_mut()), E_INVALIDARG);
            let mut hint: XNetworkingConnectivityHint = std::mem::zeroed();
            assert_eq!(XNetworkingGetConnectivityHint(&mut hint), S_OK);
            assert_eq!(hint.connectivity_level, 3);
            assert_eq!(hint.iana_interface_type, 6);
            assert!(hint.network_initialized);
            let bytes = std::slice::from_raw_parts(&hint as *const _ as *const u8, 16);
            assert_eq!(bytes[12], 1, "hint[12] is network_initialized");
        }
    }
}
