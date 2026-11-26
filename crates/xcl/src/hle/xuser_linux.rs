//! Linux XUser / XAsync / save-folder / PLS slice (Phase C step 11p).
//!
//! The Windows `hle/xgameruntime/{xasync.rs, mod.rs (user/pls/save)}` stay the
//! implementation source of truth. This module is the Linux twin for the three
//! checks the test title runs past the task-queue slice:
//! `XUser:` silent sign-in + `XUserFindForDevice` + offline XSTS failure,
//! `storage:` save-folder write + PLS path, and `PLS: drive root` (Minecraft
//! builds `<drive>:/Users/...` from it, `GetDiskFreeSpaceExW` on it).
//!
//! XAsync here mirrors the Windows engine (block header + ACTIVE map +
//! provider ops) on `xtaskqueue_linux` queues with `win64` ABI; `xsign`'s
//! bcrypt signing has no Linux twin, so the token payload is a local
//! placeholder when `xbox_live_tokens` is on and `E_CANNOT_CONNECT` when off
//! (what a console without Xbox Live reports). PLS quota math mirrors
//! Windows (`quota − used`, host figures via `statvfs`).

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::hle::xtaskqueue_linux::{self, PORT_COMPLETION};
use crate::util::{
    write_cstr_out, E_ABORT, E_FAIL, E_INVALIDARG, E_NOT_FOUND, E_NOT_SUFFICIENT_BUFFER,
    E_PENDING, HRESULT, S_OK,
};

pub const E_NO_TASK_QUEUE: HRESULT = 0x8007_01ABu32 as i32;
/// WININET_E_CANNOT_CONNECT: what a console that cannot reach Xbox Live gets.
pub const E_CANNOT_CONNECT: HRESULT = 0x8007_2EFD_u32 as i32;

// ---------------------------------------------------------------------------------------
// XAsync engine (port of `hle/xgameruntime/xasync.rs`, `win64` ABI)
// ---------------------------------------------------------------------------------------

pub const OP_BEGIN: u32 = 0;
pub const OP_DO_WORK: u32 = 1;
pub const OP_GET_RESULT: u32 = 2;
pub const OP_CANCEL: u32 = 3;
pub const OP_CLEANUP: u32 = 4;

pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000Eu32 as i32;

const BLOCK_MAGIC: u32 = 0x5841_5359; // "XASY"
const FLAG_DONE: u8 = 1 << 0;
const FLAG_CONSUMED: u8 = 1 << 1;

#[repr(C)]
pub struct XAsyncBlock {
    pub queue: *mut c_void,
    pub context: *mut c_void,
    pub callback: usize,
    pub internal: [u8; 32],
}

#[repr(C)]
pub struct XAsyncProviderData {
    pub async_: *mut XAsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

pub type XAsyncProvider = unsafe extern "win64" fn(u32, *const XAsyncProviderData) -> HRESULT;
pub type XAsyncWork = unsafe extern "win64" fn(*mut XAsyncBlock) -> HRESULT;
type Completion = unsafe extern "win64" fn(*mut XAsyncBlock);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    Completing,
    Done,
    Consuming,
    Consumed,
}

struct Inner {
    status: HRESULT,
    result_size: usize,
    phase: Phase,
}

impl Inner {
    fn is_done(&self) -> bool {
        !matches!(self.phase, Phase::Pending | Phase::Completing)
    }
}

pub struct State {
    id: u64,
    queue: Option<Arc<xtaskqueue_linux::Queue>>,
    provider: usize,
    context: usize,
    identity: usize,
    work: usize,
    inner: Mutex<Inner>,
    cv: Condvar,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn active_map() -> &'static Mutex<HashMap<usize, Arc<State>>> {
    static ACTIVE: OnceLock<Mutex<HashMap<usize, Arc<State>>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy)]
struct BlockHeader {
    status: HRESULT,
    flags: u8,
    id: u64,
}

unsafe fn write_header(b: *mut XAsyncBlock, h: BlockHeader) {
    let dst = (*b).internal.as_mut_ptr();
    std::ptr::write_unaligned(dst as *mut u32, BLOCK_MAGIC);
    std::ptr::write_unaligned(dst.add(4) as *mut i32, h.status);
    *dst.add(8) = h.flags;
    std::ptr::write_unaligned(dst.add(12) as *mut u64, h.id);
}

unsafe fn read_header(b: *mut XAsyncBlock) -> Option<BlockHeader> {
    if b.is_null() {
        return None;
    }
    let src = (*b).internal.as_ptr();
    if std::ptr::read_unaligned(src as *const u32) != BLOCK_MAGIC {
        return None;
    }
    Some(BlockHeader {
        status: std::ptr::read_unaligned(src.add(4) as *const i32),
        flags: *src.add(8),
        id: std::ptr::read_unaligned(src.add(12) as *const u64),
    })
}

enum Lookup {
    Active(Arc<State>),
    Finished(BlockHeader),
    Invalid,
}

unsafe fn lookup(b: *mut XAsyncBlock) -> Lookup {
    let map = active_map().lock().unwrap();
    let Some(hdr) = read_header(b) else { return Lookup::Invalid };
    if let Some(s) = map.get(&(b as usize)) {
        if s.id == hdr.id {
            return Lookup::Active(s.clone());
        }
    }
    if hdr.flags & FLAG_DONE != 0 {
        Lookup::Finished(hdr)
    } else {
        Lookup::Invalid
    }
}

unsafe fn register_state(b: *mut XAsyncBlock, s: Arc<State>) {
    let mut map = active_map().lock().unwrap();
    write_header(b, BlockHeader { status: E_PENDING, flags: 0, id: s.id });
    map.insert(b as usize, s);
}

unsafe fn call_provider(s: &State, op: u32, b: *mut XAsyncBlock, buffer: *mut c_void, size: usize) -> HRESULT {
    if s.provider == 0 {
        return S_OK;
    }
    let data = XAsyncProviderData { async_: b, buffer_size: size, buffer, context: s.context as *mut c_void };
    let f: XAsyncProvider = std::mem::transmute(s.provider);
    f(op, &data)
}

unsafe extern "win64" fn do_work_cb(ctx: *mut c_void, canceled: bool) {
    let b = ctx as *mut XAsyncBlock;
    let Lookup::Active(s) = lookup(b) else { return };
    if canceled {
        XAsyncComplete(b, E_ABORT, 0);
        return;
    }
    let hr = if s.work != 0 {
        let f: XAsyncWork = std::mem::transmute(s.work);
        f(b)
    } else {
        call_provider(&s, OP_DO_WORK, b, std::ptr::null_mut(), 0)
    };
    if hr != E_PENDING {
        let pending = s.inner.lock().unwrap().phase == Phase::Pending;
        if pending {
            XAsyncComplete(b, hr, 0);
        }
    }
}

unsafe extern "win64" fn completion_cb(ctx: *mut c_void, _canceled: bool) {
    let b = ctx as *mut XAsyncBlock;
    if (*b).callback != 0 {
        let f: Completion = std::mem::transmute((*b).callback);
        f(b);
    }
}

pub unsafe extern "win64" fn XAsyncBegin(
    b: *mut XAsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: usize,
) -> HRESULT {
    if b.is_null() || provider == 0 {
        return E_INVALIDARG;
    }
    let s = Arc::new(State {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        queue: xtaskqueue_linux::queue_from((*b).queue),
        provider,
        context: context as usize,
        identity: identity as usize,
        work: 0,
        inner: Mutex::new(Inner { status: E_PENDING, result_size: 0, phase: Phase::Pending }),
        cv: Condvar::new(),
    });
    register_state(b, s.clone());
    let hr = call_provider(&s, OP_BEGIN, b, std::ptr::null_mut(), 0);
    if hr < 0 {
        XAsyncComplete(b, hr, 0);
    }
    S_OK
}

pub unsafe extern "win64" fn XAsyncSchedule(b: *mut XAsyncBlock, delay_ms: u32) -> HRESULT {
    if b.is_null() {
        return E_INVALIDARG;
    }
    let Lookup::Active(s) = lookup(b) else { return E_INVALIDARG };
    let Some(q) = &s.queue else {
        return if (*b).queue.is_null() { E_NO_TASK_QUEUE } else { E_INVALIDARG };
    };
    if !xtaskqueue_linux::submit_delayed(q, xtaskqueue_linux::PORT_WORK, delay_ms, xtaskqueue_linux::Item { cb: do_work_cb as usize, ctx: b as usize }) {
        return E_ABORT;
    }
    S_OK
}

pub unsafe extern "win64" fn XAsyncComplete(b: *mut XAsyncBlock, result: HRESULT, required: usize) {
    if result == E_PENDING {
        return;
    }
    let Lookup::Active(s) = lookup(b) else { return };
    let (status, cleanup_now);
    {
        let mut g = s.inner.lock().unwrap();
        if g.phase != Phase::Pending {
            return;
        }
        g.phase = Phase::Completing;
        status = result;
        g.status = status;
        g.result_size = if status >= 0 { required } else { 0 };
        cleanup_now = g.result_size == 0;
    }
    // Snapshot the callback before publishing completion; the queue is retained in `s`.
    let cb = (*b).callback;
    if cleanup_now {
        call_provider(&s, OP_CLEANUP, b, std::ptr::null_mut(), 0);
    }
    {
        let mut map = active_map().lock().unwrap();
        write_header(b, BlockHeader { status, flags: FLAG_DONE, id: s.id });
        if cleanup_now && map.get(&(b as usize)).is_some_and(|cur| cur.id == s.id) {
            map.remove(&(b as usize));
        }
        let mut g = s.inner.lock().unwrap();
        g.phase = if cleanup_now { Phase::Consumed } else { Phase::Done };
    }
    s.cv.notify_all();
    if cb != 0 {
        if let Some(q) = &s.queue {
            let _ = xtaskqueue_linux::submit_now(q, PORT_COMPLETION, xtaskqueue_linux::Item { cb: completion_cb as usize, ctx: b as usize });
        }
    }
}

pub unsafe extern "win64" fn XAsyncGetStatus(b: *mut XAsyncBlock, wait: bool) -> HRESULT {
    match lookup(b) {
        Lookup::Active(s) => {
            let mut g = s.inner.lock().unwrap();
            if wait {
                while !g.is_done() {
                    g = s.cv.wait(g).unwrap();
                }
            }
            if g.is_done() { g.status } else { E_PENDING }
        }
        Lookup::Finished(h) => h.status,
        Lookup::Invalid => E_INVALIDARG,
    }
}

pub unsafe extern "win64" fn XAsyncGetResultSize(b: *mut XAsyncBlock, size: *mut usize) -> HRESULT {
    // libHttpClient always writes `*bufferSize` before returning, including E_PENDING (0).
    match lookup(b) {
        Lookup::Active(s) => {
            let g = s.inner.lock().unwrap();
            if !g.is_done() {
                if !size.is_null() {
                    *size = 0;
                }
                return E_PENDING;
            }
            if !size.is_null() {
                *size = g.result_size;
            }
            g.status
        }
        Lookup::Finished(h) => {
            if !size.is_null() {
                *size = 0;
            }
            h.status
        }
        Lookup::Invalid => E_INVALIDARG,
    }
}

pub unsafe extern "win64" fn XAsyncGetResult(
    b: *mut XAsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    // libHttpClient writes `*bufferUsed` (0) before returning E_PENDING / failure HRESULTs.
    match lookup(b) {
        Lookup::Invalid => E_INVALIDARG,
        Lookup::Finished(h) => {
            if h.status < 0 {
                if !used.is_null() {
                    *used = 0;
                }
                return h.status;
            }
            if h.flags & FLAG_CONSUMED != 0 {
                return E_ILLEGAL_METHOD_CALL;
            }
            if !used.is_null() {
                *used = 0;
            }
            h.status
        }
        Lookup::Active(s) => {
            let (status, need) = {
                let mut g = s.inner.lock().unwrap();
                if !g.is_done() {
                    if !used.is_null() {
                        *used = 0;
                    }
                    return E_PENDING;
                }
                if !identity.is_null() && s.identity != 0 && identity as usize != s.identity {
                    return E_INVALIDARG;
                }
                if g.status < 0 {
                    if !used.is_null() {
                        *used = 0;
                    }
                    return g.status;
                }
                if g.result_size > 0 {
                    if g.phase != Phase::Done {
                        return E_ILLEGAL_METHOD_CALL;
                    }
                    if size < g.result_size || buffer.is_null() {
                        return E_NOT_SUFFICIENT_BUFFER;
                    }
                    g.phase = Phase::Consuming;
                }
                (g.status, g.result_size)
            };
            if need > 0 {
                let hr = call_provider(&s, OP_GET_RESULT, b, buffer, size);
                if !used.is_null() {
                    *used = need;
                }
                call_provider(&s, OP_CLEANUP, b, std::ptr::null_mut(), 0);
                let mut map = active_map().lock().unwrap();
                write_header(b, BlockHeader { status, flags: FLAG_DONE | FLAG_CONSUMED, id: s.id });
                if map.get(&(b as usize)).is_some_and(|cur| cur.id == s.id) {
                    map.remove(&(b as usize));
                }
                s.inner.lock().unwrap().phase = Phase::Consumed;
                return if hr == S_OK { status } else { hr };
            }
            if !used.is_null() {
                *used = 0;
            }
            status
        }
    }
}

pub unsafe extern "win64" fn XAsyncCancel(b: *mut XAsyncBlock) {
    let Lookup::Active(s) = lookup(b) else { return };
    if s.inner.lock().unwrap().phase != Phase::Pending {
        return;
    }
    let _ = call_provider(&s, OP_CANCEL, b, std::ptr::null_mut(), 0);
}

pub unsafe extern "win64" fn XAsyncRun(b: *mut XAsyncBlock, work: usize) -> HRESULT {
    if b.is_null() || work == 0 {
        return E_INVALIDARG;
    }
    let s = Arc::new(State {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        queue: xtaskqueue_linux::queue_from((*b).queue),
        provider: 0,
        context: 0,
        identity: 0,
        work,
        inner: Mutex::new(Inner { status: E_PENDING, result_size: 0, phase: Phase::Pending }),
        cv: Condvar::new(),
    });
    register_state(b, s);
    let hr = XAsyncSchedule(b, 0);
    if hr < 0 {
        XAsyncComplete(b, hr, 0);
    }
    S_OK
}

/// Serialized async result bytes plus byte offsets holding relative `usize`
/// offsets that must be relocated by adding the destination buffer's base.
pub struct PayloadBuf {
    pub bytes: Vec<u8>,
    pub fixups: Vec<usize>,
}

struct Payload {
    result: Mutex<Option<Result<PayloadBuf, HRESULT>>>,
    work: Mutex<Option<Box<dyn FnOnce() -> Result<PayloadBuf, HRESULT> + Send>>>,
}

unsafe extern "win64" fn payload_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
    let d = &*data;
    let p = &*(d.context as *const Payload);
    match op {
        OP_BEGIN => XAsyncSchedule(d.async_, 0),
        OP_DO_WORK => {
            let w = p.work.lock().unwrap().take();
            let r = w.map(|f| f()).unwrap_or(Err(E_FAIL));
            let (hr, size) = match &r {
                Ok(pb) => (S_OK, pb.bytes.len()),
                Err(e) => (*e, 0),
            };
            *p.result.lock().unwrap() = Some(r);
            XAsyncComplete(d.async_, hr, size);
            S_OK
        }
        OP_GET_RESULT => {
            if let Some(Ok(pb)) = p.result.lock().unwrap().as_ref() {
                let n = pb.bytes.len().min(d.buffer_size);
                let dst = d.buffer as *mut u8;
                std::ptr::copy_nonoverlapping(pb.bytes.as_ptr(), dst, n);
                let base = dst as usize;
                for &off in &pb.fixups {
                    if off + std::mem::size_of::<usize>() <= n {
                        let slot = dst.add(off) as *mut usize;
                        let rel = std::ptr::read_unaligned(slot);
                        std::ptr::write_unaligned(slot, base.wrapping_add(rel));
                    }
                }
            }
            S_OK
        }
        OP_CLEANUP => {
            drop(Box::from_raw(d.context as *mut Payload));
            S_OK
        }
        _ => S_OK,
    }
}

/// Start an async op whose result is `work()`'s bytes; `identity` ties GetResult calls.
pub unsafe fn begin_with<F>(b: *mut XAsyncBlock, identity: usize, work: F) -> HRESULT
where
    F: FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
{
    begin_with_fixups(b, identity, move || work().map(|bytes| PayloadBuf { bytes, fixups: Vec::new() }))
}

/// Start an async op whose result buffer contains internal pointers relocated in `OP_GET_RESULT`.
pub unsafe fn begin_with_fixups<F>(b: *mut XAsyncBlock, identity: usize, work: F) -> HRESULT
where
    F: FnOnce() -> Result<PayloadBuf, HRESULT> + Send + 'static,
{
    let p = Box::into_raw(Box::new(Payload { result: Mutex::new(None), work: Mutex::new(Some(Box::new(work))) }));
    let hr = XAsyncBegin(b, p as *mut c_void, identity as *const c_void, std::ptr::null(), payload_provider as usize);
    if hr < 0 {
        drop(Box::from_raw(p));
    }
    hr
}

// ---------------------------------------------------------------------------------------
// XUser – one signed-in offline user
// ---------------------------------------------------------------------------------------

#[repr(C)]
pub struct User {
    magic: u32,
    refs: AtomicU32,
}

static USER: User = User { magic: 0x5553_4552, refs: AtomicU32::new(1) };
const LOCAL_ID: u64 = 1;
static USER_ADD_IDENTITY: u8 = 0;

fn user_ptr() -> *mut c_void {
    &USER as *const User as *mut c_void
}

unsafe fn valid_user(h: *mut c_void) -> bool {
    h == user_ptr()
}

pub unsafe extern "win64" fn XUserAddAsync(options: u32, b: *mut XAsyncBlock) -> HRESULT {
    crate::xlog_info!("XUserAddAsync(options={options:#x}) → local offline user '{}'", crate::state::config().gamertag);
    begin_with(b, &USER_ADD_IDENTITY as *const u8 as usize, || {
        USER.refs.fetch_add(1, Ordering::AcqRel);
        Ok((user_ptr() as usize).to_le_bytes().to_vec())
    })
}

pub unsafe extern "win64" fn XUserAddResult(b: *mut XAsyncBlock, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let hr = XAsyncGetResult(b, &USER_ADD_IDENTITY as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut());
    if hr >= 0 {
        user_signed_in();
    }
    hr
}

pub unsafe extern "win64" fn XUserDuplicateHandle(h: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    if !valid_user(h) {
        return E_INVALIDARG;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = h;
    S_OK
}

pub unsafe extern "win64" fn XUserCloseHandle(h: *mut c_void) {
    if valid_user(h) {
        // Static offline user: never freed; clamp at 1 like the headless GameInput object.
        if USER.refs.load(Ordering::Acquire) > 1 {
            USER.refs.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub unsafe extern "win64" fn XUserCompare(a: *mut c_void, b: *mut c_void) -> i32 {
    (a as usize).cmp(&(b as usize)) as i32
}

pub unsafe extern "win64" fn XUserGetMaxUsers(out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 1;
    S_OK
}

pub unsafe extern "win64" fn XUserGetId(h: *mut c_void, out: *mut u64) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = crate::state::config().user_id;
    S_OK
}

pub unsafe extern "win64" fn XUserGetLocalId(h: *mut c_void, out: *mut u64) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = LOCAL_ID;
    S_OK
}

pub unsafe extern "win64" fn XUserFindUserByLocalId(id: u64, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    if id != LOCAL_ID {
        return E_NOT_FOUND;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "win64" fn XUserFindUserById(id: u64, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    if id != crate::state::config().user_id {
        return E_NOT_FOUND;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "win64" fn XUserGetIsGuest(h: *mut c_void, out: *mut bool) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = false;
    S_OK
}

pub unsafe extern "win64" fn XUserGetState(h: *mut c_void, out: *mut u32) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0; // SignedIn
    S_OK
}

pub const GAMERTAG_SIZES: [usize; 4] = [16, 97, 15, 101];

fn gamertag_component(tag: &str, component: u32) -> &str {
    if component == 2 {
        return "";
    }
    let max = match component {
        0 => 15,
        3 => 16,
        _ => GAMERTAG_SIZES[1] - 1,
    };
    let mut end = 0;
    for (count, (i, c)) in tag.char_indices().enumerate() {
        if i + c.len_utf8() > GAMERTAG_SIZES[component as usize] - 1 || count >= max {
            break;
        }
        end = i + c.len_utf8();
    }
    &tag[..end]
}

pub unsafe extern "win64" fn XUserGetGamertag(h: *mut c_void, component: u32, size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    if !valid_user(h) || component > 3 {
        return E_INVALIDARG;
    }
    let value = gamertag_component(&crate::state::config().gamertag, component);
    if size < value.len() + 1 {
        if !used.is_null() {
            *used = value.len() + 1;
        }
        return E_NOT_SUFFICIENT_BUFFER;
    }
    write_cstr_out(value, buf, size, used)
}

pub unsafe extern "win64" fn XUserGetAgeGroup(h: *mut c_void, out: *mut u32) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = 3; // Adult
    S_OK
}

#[repr(C)]
pub struct RegistrationToken {
    pub token: u64,
}

pub unsafe extern "win64" fn XUserRegisterForChangeEvent(
    _q: *mut c_void,
    _ctx: *mut c_void,
    _cb: usize,
    token: *mut RegistrationToken,
) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    (*token).token = 1;
    S_OK
}

pub unsafe extern "win64" fn XUserUnregisterForChangeEvent(_token: RegistrationToken, _wait: bool) -> bool {
    true
}

#[repr(C)]
pub struct XUserGetTokenAndSignatureData {
    pub token_size: usize,
    pub signature_size: usize,
    pub token: *const u8,
    pub signature: *const u8,
}

#[repr(C)]
pub struct XUserGetTokenAndSignatureUtf16Data {
    pub token_count: usize,
    pub signature_count: usize,
    pub token: *const u16,
    pub signature: *const u16,
}

static TOKEN_UTF8_ID: u8 = 0;
static TOKEN_UTF16_ID: u8 = 0;
static FIND_CONTROLLER_ID: u8 = 0;

fn build_token_payload(utf16: bool) -> Result<PayloadBuf, HRESULT> {
    // Offline placeholder (no bcrypt on Linux): a non-empty token + signature so
    // `xbox_live_tokens` titles get *something*; offline titles never reach here.
    let (tok, sig) = ("xcl-offline-token", "xcl-offline-signature");
    let (tok_bytes, sig_bytes, unit) = if utf16 {
        let tb: Vec<u8> = tok.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        let sb: Vec<u8> = sig.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        (tb, sb, 2usize)
    } else {
        let mut tb = tok.as_bytes().to_vec();
        tb.push(0);
        let mut sb = sig.as_bytes().to_vec();
        sb.push(0);
        (tb, sb, 1usize)
    };
    let header_len = 4 * std::mem::size_of::<usize>();
    let sig_offset = header_len + tok_bytes.len();
    let mut bytes = Vec::with_capacity(sig_offset + sig_bytes.len());
    bytes.extend_from_slice(&(tok_bytes.len() / unit).to_le_bytes());
    bytes.extend_from_slice(&(sig_bytes.len() / unit).to_le_bytes());
    bytes.extend_from_slice(&header_len.to_le_bytes());
    bytes.extend_from_slice(&sig_offset.to_le_bytes());
    bytes.extend_from_slice(&tok_bytes);
    bytes.extend_from_slice(&sig_bytes);
    Ok(PayloadBuf { bytes, fixups: vec![16, 24] })
}

unsafe fn copy_capped_body(body_size: usize, body: *const c_void) -> Vec<u8> {
    if body.is_null() || body_size == 0 {
        return Vec::new();
    }
    let len = body_size.min(8192);
    std::slice::from_raw_parts(body as *const u8, len).to_vec()
}

fn note_token_request(method: &str, url: &str) -> Result<(), HRESULT> {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let tokens = crate::state::config().xbox_live_tokens;
    if n < 24 || n % 64 == 0 {
        crate::xlog_info!("XUser: token #{n} for {method} {url} – {}", if tokens { "XCL's local placeholder" } else { "none: no Xbox Live (0x80072EFD)" });
    }
    if tokens { Ok(()) } else { Err(E_CANNOT_CONNECT) }
}

pub unsafe extern "win64" fn XUserGetTokenAndSignatureAsync(
    user: *mut c_void,
    _options: u32,
    method: *const u8,
    url: *const u8,
    _header_count: usize,
    _headers: *const c_void,
    body_size: usize,
    body: *const c_void,
    b: *mut XAsyncBlock,
) -> HRESULT {
    if !valid_user(user) || url.is_null() {
        return E_INVALIDARG;
    }
    let method = crate::util::from_cstr_ptr(method).filter(|s| !s.is_empty()).unwrap_or_else(|| "GET".into());
    let url = crate::util::from_cstr_ptr(url).unwrap_or_default();
    let allowed = note_token_request(&method, &url);
    let _body = copy_capped_body(body_size, body);
    begin_with_fixups(b, &TOKEN_UTF8_ID as *const u8 as usize, move || {
        allowed?;
        build_token_payload(false)
    })
}

pub unsafe extern "win64" fn XUserGetTokenAndSignatureResultSize(b: *mut XAsyncBlock, size: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, size)
}

pub unsafe extern "win64" fn XUserGetTokenAndSignatureResult(
    b: *mut XAsyncBlock,
    buffer_size: usize,
    buffer: *mut c_void,
    ptr_to_buffer: *mut *mut XUserGetTokenAndSignatureData,
    buffer_used: *mut usize,
) -> HRESULT {
    if !ptr_to_buffer.is_null() {
        *ptr_to_buffer = std::ptr::null_mut();
    }
    let hr = XAsyncGetResult(b, &TOKEN_UTF8_ID as *const u8 as *const c_void, buffer_size, buffer, buffer_used);
    if hr >= 0 && !ptr_to_buffer.is_null() {
        *ptr_to_buffer = buffer as *mut XUserGetTokenAndSignatureData;
    }
    hr
}

pub unsafe extern "win64" fn XUserGetTokenAndSignatureUtf16Async(
    user: *mut c_void,
    _options: u32,
    method: *const u16,
    url: *const u16,
    _header_count: usize,
    _headers: *const c_void,
    body_size: usize,
    body: *const c_void,
    b: *mut XAsyncBlock,
) -> HRESULT {
    if !valid_user(user) || url.is_null() {
        return E_INVALIDARG;
    }
    let method = crate::util::from_wide_ptr(method).filter(|s| !s.is_empty()).unwrap_or_else(|| "GET".into());
    let url = crate::util::from_wide_ptr(url).unwrap_or_default();
    let allowed = note_token_request(&method, &url);
    let _body = copy_capped_body(body_size, body);
    begin_with_fixups(b, &TOKEN_UTF16_ID as *const u8 as usize, move || {
        allowed?;
        build_token_payload(true)
    })
}

pub unsafe extern "win64" fn XUserGetTokenAndSignatureUtf16ResultSize(b: *mut XAsyncBlock, size: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, size)
}

pub unsafe extern "win64" fn XUserGetTokenAndSignatureUtf16Result(
    b: *mut XAsyncBlock,
    buffer_size: usize,
    buffer: *mut c_void,
    ptr_to_buffer: *mut *mut XUserGetTokenAndSignatureUtf16Data,
    buffer_used: *mut usize,
) -> HRESULT {
    if !ptr_to_buffer.is_null() {
        *ptr_to_buffer = std::ptr::null_mut();
    }
    let hr = XAsyncGetResult(b, &TOKEN_UTF16_ID as *const u8 as *const c_void, buffer_size, buffer, buffer_used);
    if hr >= 0 && !ptr_to_buffer.is_null() {
        *ptr_to_buffer = buffer as *mut XUserGetTokenAndSignatureUtf16Data;
    }
    hr
}

/// APP_LOCAL_DEVICE_ID of the headless gamepad (same bytes as Windows `gameinput`).
pub fn gamepad_device_id() -> [u8; 32] {
    *b"XCL gamepad 0\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAssociationChange {
    pub device_id: [u8; 32],
    pub old_user: u64,
    pub new_user: u64,
}

static ASSOCIATION_CALLBACKS: Mutex<Vec<(u64, usize, usize, usize)>> = Mutex::new(Vec::new());
static USER_SIGNED_IN: AtomicU64 = AtomicU64::new(0);
static NEXT_ASSOCIATION_TOKEN: AtomicU64 = AtomicU64::new(0x4153_0001);

pub fn gamepad_association() -> DeviceAssociationChange {
    DeviceAssociationChange { device_id: gamepad_device_id(), old_user: 0, new_user: LOCAL_ID }
}

/// Where a queued association callback is: `Done` before it starts means it was unregistered
/// while queued (it must not call the title's context), and a `Running` one is what the
/// unregister's `wait` waits for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallState {
    Queued,
    Running,
    Done,
}

struct AssociationCall {
    token: u64,
    cb: usize,
    ctx: usize,
    change: DeviceAssociationChange,
    state: Mutex<CallState>,
    done: Condvar,
}

/// The association callbacks handed to a queue that have not finished yet: the unregister finds
/// them by token to cancel the queued ones and wait for a running one.
static ASSOCIATION_CALLS: Mutex<Vec<Arc<AssociationCall>>> = Mutex::new(Vec::new());

unsafe extern "win64" fn association_call(rec: *mut c_void, canceled: bool) {
    let call = Arc::from_raw(rec as *const AssociationCall);
    let run = {
        let mut st = call.state.lock().unwrap();
        if canceled || *st != CallState::Queued {
            *st = CallState::Done;
            call.done.notify_all();
            false
        } else {
            *st = CallState::Running;
            true
        }
    };
    if run {
        let f: unsafe extern "win64" fn(*mut c_void, *const DeviceAssociationChange) = std::mem::transmute(call.cb);
        f(call.ctx as *mut c_void, &call.change);
        *call.state.lock().unwrap() = CallState::Done;
        call.done.notify_all();
    }
    ASSOCIATION_CALLS.lock().unwrap().retain(|c| !Arc::ptr_eq(c, &call));
}

unsafe fn notify_association(token: u64, queue: usize, ctx: usize, cb: usize) {
    let call = Arc::new(AssociationCall {
        token,
        cb,
        ctx,
        change: gamepad_association(),
        state: Mutex::new(CallState::Queued),
        done: Condvar::new(),
    });
    ASSOCIATION_CALLS.lock().unwrap().push(call.clone());
    let rec = Arc::into_raw(call.clone());
    let submitted = xtaskqueue_linux::queue_from(queue as *mut c_void)
        .map(|q| xtaskqueue_linux::submit_now(&q, PORT_COMPLETION, xtaskqueue_linux::Item { cb: association_call as usize, ctx: rec as usize }))
        .unwrap_or(false);
    if !submitted {
        drop(Arc::from_raw(rec));
        ASSOCIATION_CALLS.lock().unwrap().retain(|c| !Arc::ptr_eq(c, &call));
        crate::xlog_warn!("XUser: the device association callback could not be queued");
        return;
    }
    crate::xlog_info!("XUser: the gamepad is now associated with the local user (device association callback)");
}

pub unsafe extern "win64" fn XUserRegisterForDeviceAssociationChanged(
    queue: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut u64,
) -> HRESULT {
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    let t = NEXT_ASSOCIATION_TOKEN.fetch_add(1, Ordering::Relaxed);
    *token = t;
    ASSOCIATION_CALLBACKS.lock().unwrap().push((t, queue as usize, ctx as usize, cb));
    crate::xlog_info!("XUser: device association callback #{t:#x} registered");
    if USER_SIGNED_IN.load(Ordering::Acquire) != 0 {
        notify_association(t, queue as usize, ctx as usize, cb);
    }
    S_OK
}

/// No callback of `token` runs after this: one still queued is canceled (the title may free its
/// context as soon as the call returns), and with `wait` a running one is waited for.
pub unsafe extern "win64" fn XUserUnregisterForDeviceAssociationChanged(token: u64, wait: bool) -> bool {
    ASSOCIATION_CALLBACKS.lock().unwrap().retain(|c| c.0 != token);
    let mut running: Vec<Arc<AssociationCall>> = Vec::new();
    {
        let mut calls = ASSOCIATION_CALLS.lock().unwrap();
        let mut i = 0;
        while i < calls.len() {
            if calls[i].token != token {
                i += 1;
                continue;
            }
            let call = calls.remove(i);
            let mut st = call.state.lock().unwrap();
            match *st {
                CallState::Queued => *st = CallState::Done, // the queue's later call skips it
                CallState::Running => running.push(call.clone()),
                CallState::Done => {}
            }
        }
    }
    if wait {
        for call in running {
            let mut st = call.state.lock().unwrap();
            while *st != CallState::Done {
                st = call.done.wait(st).unwrap();
            }
        }
    }
    true
}

fn user_signed_in() {
    if USER_SIGNED_IN.swap(1, Ordering::AcqRel) != 0 {
        return;
    }
    crate::xlog_info!("XUser: the local user reached the title");
    let calls: Vec<_> = ASSOCIATION_CALLBACKS.lock().unwrap().clone();
    for (t, q, ctx, cb) in calls {
        unsafe { notify_association(t, q, ctx, cb) };
    }
}

/// Associate every input device with the local signed-in user.
pub unsafe extern "win64" fn XUserFindForDevice(_device: *const c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "win64" fn XUserFindControllerForUserWithUiAsync(user: *mut c_void, b: *mut XAsyncBlock) -> HRESULT {
    if !valid_user(user) {
        return E_INVALIDARG;
    }
    begin_with(b, &FIND_CONTROLLER_ID as *const u8 as usize, || Ok(gamepad_device_id().to_vec()))
}

pub unsafe extern "win64" fn XUserFindControllerForUserWithUiResult(b: *mut XAsyncBlock, device_id: *mut [u8; 32]) -> HRESULT {
    if device_id.is_null() {
        return E_INVALIDARG;
    }
    XAsyncGetResult(b, &FIND_CONTROLLER_ID as *const u8 as *const c_void, 32, device_id as *mut c_void, std::ptr::null_mut())
}

// ---------------------------------------------------------------------------------------
// Persistent local storage + save files
// ---------------------------------------------------------------------------------------

fn data_subdir(name: &str) -> PathBuf {
    let p = crate::state::data_dir().join(name);
    let _ = std::fs::create_dir_all(&p);
    p
}

fn pls_dir() -> PathBuf {
    data_subdir("pls")
}

fn pls_path() -> String {
    // Ensure the host folder exists even when the title gets the `S:\` drive root.
    let _ = pls_dir();
    crate::state::config().pls_drive().unwrap_or_else(|| pls_dir().to_string_lossy().into_owned())
}

pub unsafe extern "win64" fn XPersistentLocalStorageGetPathSize(size: *mut usize) -> HRESULT {
    if size.is_null() {
        return E_INVALIDARG;
    }
    *size = pls_path().len() + 1;
    S_OK
}

pub unsafe extern "win64" fn XPersistentLocalStorageGetPath(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&pls_path(), buf, size, used)
}

fn pls_used_bytes(root: &Path) -> u64 {
    let mut used = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            } else if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                used = used.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
    }
    used
}

#[repr(C)]
pub struct XPersistentLocalStorageSpaceInfo {
    pub available_free_bytes: u64,
    pub total_free_bytes: u64,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

fn pls_space_info(quota: u64, used: u64, avail: u64, free: u64) -> XPersistentLocalStorageSpaceInfo {
    let remaining = quota.saturating_sub(used);
    XPersistentLocalStorageSpaceInfo {
        available_free_bytes: avail.min(remaining),
        total_free_bytes: free.min(remaining),
        used_bytes: used,
        total_bytes: quota,
    }
}

fn host_disk_space(path: &Path) -> Option<(u64, u64)> {
    let mut probe = path.to_path_buf();
    loop {
        if probe.exists() {
            break;
        }
        if !probe.pop() {
            return None;
        }
    }
    let c = std::ffi::CString::new(probe.to_string_lossy().into_owned()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bsize = st.f_bsize as u64;
    Some((st.f_bavail as u64 * bsize, st.f_blocks as u64 * bsize))
}

pub unsafe extern "win64" fn XPersistentLocalStorageGetSpaceInfo(info: *mut XPersistentLocalStorageSpaceInfo) -> HRESULT {
    if info.is_null() {
        return E_INVALIDARG;
    }
    let pls = pls_dir();
    let mut avail = host_disk_space(&pls).map(|(a, _)| a).unwrap_or(0);
    let quota = crate::state::config().pls_quota_bytes();
    let used = pls_used_bytes(&pls);
    if avail == 0 {
        avail = quota;
    }
    let free = avail;
    let out = pls_space_info(quota, used, avail, free);
    static LAST: Mutex<Option<(u64, u64, u64, u64)>> = Mutex::new(None);
    let now = (out.available_free_bytes, out.total_free_bytes, out.used_bytes, out.total_bytes);
    if let Ok(mut last) = LAST.lock() {
        if *last != Some(now) {
            *last = Some(now);
            crate::xlog_info!(
                "XPersistentLocalStorageGetSpaceInfo: {} MiB available, {} MiB free, {} MiB used of {} MiB",
                now.0 >> 20,
                now.1 >> 20,
                now.2 >> 20,
                now.3 >> 20,
            );
        }
    }
    *info = out;
    S_OK
}

static SAVE_FILES_IDENTITY: u8 = 0;

pub unsafe extern "win64" fn XGameSaveFilesGetFolderWithUiAsync(_user: *mut c_void, scid: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    let scid = crate::util::from_cstr_ptr(scid).unwrap_or_else(|| "default".into());
    let safe: String = scid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    begin_with(b, &SAVE_FILES_IDENTITY as *const u8 as usize, move || {
        let p = data_subdir("saves").join(if safe.is_empty() { "default".into() } else { safe });
        std::fs::create_dir_all(&p).map_err(|_| E_FAIL)?;
        let mut v = p.to_string_lossy().into_owned().into_bytes();
        v.push(0);
        Ok(v)
    })
}

pub unsafe extern "win64" fn XGameSaveFilesGetFolderWithUiResult(b: *mut XAsyncBlock, size: usize, buf: *mut u8) -> HRESULT {
    XAsyncGetResult(b, &SAVE_FILES_IDENTITY as *const u8 as *const c_void, size, buf as *mut c_void, std::ptr::null_mut())
}

/// Implemented exports with real code addresses.
pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("XAsyncBegin", XAsyncBegin as usize),
        ("XAsyncSchedule", XAsyncSchedule as usize),
        ("XAsyncComplete", XAsyncComplete as usize),
        ("XAsyncGetStatus", XAsyncGetStatus as usize),
        ("XAsyncGetResultSize", XAsyncGetResultSize as usize),
        ("XAsyncGetResult", XAsyncGetResult as usize),
        ("XAsyncCancel", XAsyncCancel as usize),
        ("XAsyncRun", XAsyncRun as usize),
        ("XUserAddAsync", XUserAddAsync as usize),
        ("XUserAddResult", XUserAddResult as usize),
        ("XUserDuplicateHandle", XUserDuplicateHandle as usize),
        ("XUserCloseHandle", XUserCloseHandle as usize),
        ("XUserCompare", XUserCompare as usize),
        ("XUserGetMaxUsers", XUserGetMaxUsers as usize),
        ("XUserGetId", XUserGetId as usize),
        ("XUserGetLocalId", XUserGetLocalId as usize),
        ("XUserFindUserByLocalId", XUserFindUserByLocalId as usize),
        ("XUserFindUserById", XUserFindUserById as usize),
        ("XUserGetIsGuest", XUserGetIsGuest as usize),
        ("XUserGetState", XUserGetState as usize),
        ("XUserGetGamertag", XUserGetGamertag as usize),
        ("XUserGetAgeGroup", XUserGetAgeGroup as usize),
        ("XUserRegisterForChangeEvent", XUserRegisterForChangeEvent as usize),
        ("XUserUnregisterForChangeEvent", XUserUnregisterForChangeEvent as usize),
        ("XUserGetTokenAndSignatureAsync", XUserGetTokenAndSignatureAsync as usize),
        ("XUserGetTokenAndSignatureResultSize", XUserGetTokenAndSignatureResultSize as usize),
        ("XUserGetTokenAndSignatureResult", XUserGetTokenAndSignatureResult as usize),
        ("XUserGetTokenAndSignatureUtf16Async", XUserGetTokenAndSignatureUtf16Async as usize),
        ("XUserGetTokenAndSignatureUtf16ResultSize", XUserGetTokenAndSignatureUtf16ResultSize as usize),
        ("XUserGetTokenAndSignatureUtf16Result", XUserGetTokenAndSignatureUtf16Result as usize),
        ("XUserFindForDevice", XUserFindForDevice as usize),
        ("XUserFindControllerForUserWithUiAsync", XUserFindControllerForUserWithUiAsync as usize),
        ("XUserFindControllerForUserWithUiResult", XUserFindControllerForUserWithUiResult as usize),
        ("XPersistentLocalStorageGetPathSize", XPersistentLocalStorageGetPathSize as usize),
        ("XPersistentLocalStorageGetPath", XPersistentLocalStorageGetPath as usize),
        ("XPersistentLocalStorageGetSpaceInfo", XPersistentLocalStorageGetSpaceInfo as usize),
        ("XGameSaveFilesGetFolderWithUiAsync", XGameSaveFilesGetFolderWithUiAsync as usize),
        ("XGameSaveFilesGetFolderWithUiResult", XGameSaveFilesGetFolderWithUiResult as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    unsafe extern "win64" fn begins_without_work(_op: u32, _data: *const XAsyncProviderData) -> HRESULT {
        S_OK
    }

    /// libHttpClient always stores into `*bufferSize` before returning, including when the
    /// operation is still pending (0). Leaving the caller's sentinel unchanged is a bug.
    #[test]
    fn get_result_size_writes_zero_while_pending() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XAsyncBegin(&mut b, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), begins_without_work as usize), S_OK);
            let mut size = 0xDEADusize;
            assert_eq!(XAsyncGetResultSize(&mut b, &mut size), E_PENDING);
            assert_eq!(size, 0, "pending GetResultSize must write *size = 0");
            XAsyncComplete(&mut b, S_OK, 0);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            size = 0xBEEF;
            assert_eq!(XAsyncGetResultSize(&mut b, &mut size), S_OK);
            assert_eq!(size, 0);
        }
    }

    /// libHttpClient stores 0 into `*bufferUsed` before returning E_PENDING.
    #[test]
    fn get_result_writes_zero_used_while_pending() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XAsyncBegin(&mut b, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), begins_without_work as usize), S_OK);
            let mut used = 0xDEADusize;
            let mut buf = [0u8; 4];
            assert_eq!(
                XAsyncGetResult(&mut b, std::ptr::null(), buf.len(), buf.as_mut_ptr() as _, &mut used),
                E_PENDING
            );
            assert_eq!(used, 0, "pending GetResult must write *used = 0");
            XAsyncComplete(&mut b, S_OK, 0);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
        }
    }

    #[test]
    fn get_result_preserves_nonzero_success_hresult() {
        const S_FALSE: HRESULT = 1;
        unsafe extern "win64" fn payload_provider_ok(op: u32, data: *const XAsyncProviderData) -> HRESULT {
            if op == OP_GET_RESULT {
                std::ptr::copy_nonoverlapping(b"abcd".as_ptr(), (*data).buffer as *mut u8, 4);
            }
            S_OK
        }
        unsafe {
            // Zero-payload completion with S_FALSE (Lookup::Finished path)
            let mut b0: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XAsyncBegin(&mut b0, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), begins_without_work as usize), S_OK);
            XAsyncComplete(&mut b0, S_FALSE, 0);
            assert_eq!(XAsyncGetStatus(&mut b0, false), S_FALSE);
            let mut size = 99usize;
            assert_eq!(XAsyncGetResultSize(&mut b0, &mut size), S_FALSE);
            assert_eq!(size, 0);
            let mut used = 99usize;
            assert_eq!(XAsyncGetResult(&mut b0, std::ptr::null(), 0, std::ptr::null_mut(), &mut used), S_FALSE);
            assert_eq!(used, 0);

            // Non-empty payload completion with S_FALSE (Lookup::Active path)
            let mut b4: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XAsyncBegin(&mut b4, std::ptr::null_mut(), 0x55 as *const c_void, std::ptr::null(), payload_provider_ok as usize), S_OK);
            XAsyncComplete(&mut b4, S_FALSE, 4);
            assert_eq!(XAsyncGetStatus(&mut b4, false), S_FALSE);
            let mut buf = [0u8; 4];
            let mut used4 = 0usize;
            assert_eq!(XAsyncGetResult(&mut b4, 0x55 as *const c_void, 4, buf.as_mut_ptr() as *mut c_void, &mut used4), S_FALSE);
            assert_eq!(&buf, b"abcd");
            assert_eq!(used4, 4);
        }
    }

    /// `XAsyncComplete` must wake threads blocked in `XAsyncGetStatus(b, true)` before running
    /// a `MODE_IMMEDIATE` completion callback synchronously on the completing thread; otherwise
    /// a callback that synchronises with the waiting thread deadlocks deterministically.
    #[test]
    fn complete_wakes_waiters_before_immediate_completion_callback() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct WaitCtx {
            rx: Mutex<Option<mpsc::Receiver<()>>>,
            woke: AtomicBool,
        }
        unsafe extern "win64" fn cb_waits_for_waiter(b: *mut XAsyncBlock) {
            let ctx = &*((*b).context as *const WaitCtx);
            let rx = ctx.rx.lock().unwrap().take().unwrap();
            if rx.recv_timeout(Duration::from_secs(1)).is_ok() {
                ctx.woke.store(true, Ordering::SeqCst);
            }
        }

        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueCreate(xtaskqueue_linux::MODE_MANUAL, xtaskqueue_linux::MODE_IMMEDIATE, &mut q), S_OK);
            let (tx, rx) = mpsc::channel();
            let ctx = WaitCtx { rx: Mutex::new(Some(rx)), woke: AtomicBool::new(false) };
            let mut b = Box::new(std::mem::zeroed::<XAsyncBlock>());
            b.queue = q;
            b.context = &ctx as *const WaitCtx as *mut c_void;
            b.callback = cb_waits_for_waiter as usize;
            assert_eq!(XAsyncBegin(&mut *b, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), begins_without_work as usize), S_OK);

            let (started_tx, started_rx) = mpsc::channel();
            let b_addr = &mut *b as *mut XAsyncBlock as usize;
            let waiter = std::thread::spawn(move || {
                let _ = started_tx.send(());
                let hr = XAsyncGetStatus(b_addr as *mut XAsyncBlock, true);
                let _ = tx.send(());
                hr
            });
            started_rx.recv().unwrap();
            // Give the waiter thread a moment to enter `s.cv.wait(g)`.
            std::thread::sleep(Duration::from_millis(20));

            XAsyncComplete(&mut *b, S_OK, 0);
            assert_eq!(waiter.join().unwrap(), S_OK);
            assert!(ctx.woke.load(Ordering::SeqCst), "MODE_IMMEDIATE callback timed out waiting for XAsyncGetStatus waiter to wake");

            xtaskqueue_linux::XTaskQueueTerminate(q, true, std::ptr::null_mut(), 0);
            xtaskqueue_linux::XTaskQueueCloseHandle(q);
        }
    }

    #[test]
    fn gamertag_components_are_consistent_and_fit_their_buffers() {
        // no modern suffix: UniqueModern is the gamertag itself, not "Player1#0001"
        let parts: Vec<&str> = (0..4).map(|c| gamertag_component("Player1", c)).collect();
        assert_eq!(parts, ["Player1", "Player1", "", "Player1"]);
        // a long configured gamertag still fits each documented buffer
        let long = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        assert_eq!(gamertag_component(long, 0), "ABCDEFGHIJKLMNO");
        assert_eq!(gamertag_component(long, 1), long);
        assert_eq!(gamertag_component(long, 3), "ABCDEFGHIJKLMNOP");
        // cut at a character boundary
        assert_eq!(gamertag_component("ÁÉÍÓÚÁÉÍÓÚÁÉÍÓÚÁÉ", 0), "ÁÉÍÓÚÁÉÍÓÚÁÉÍÓÚ".chars().take(7).collect::<String>());
        for c in 0..4 {
            assert!(gamertag_component(&long.repeat(10), c).len() < GAMERTAG_SIZES[c as usize]);
        }
    }

    #[test]
    fn user_add_and_queries() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XUserAddAsync(1, &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XUserAddResult(&mut b, &mut u), S_OK);
            assert!(!u.is_null());
            let mut tag = [0u8; 16];
            let mut used = 0usize;
            assert_eq!(XUserGetGamertag(u, 0, 16, tag.as_mut_ptr(), &mut used), S_OK);
            assert_eq!(&tag[..used - 1], b"Player1");
            assert_eq!(XUserGetGamertag(u, 0, 3, tag.as_mut_ptr(), &mut used), E_NOT_SUFFICIENT_BUFFER);
            let mut id = 0u64;
            assert_eq!(XUserGetId(u, &mut id), S_OK);
            assert_ne!(id, 0);
            // FindForDevice hands the same offline user back.
            let dev = [1u8; 32];
            let mut du = std::ptr::null_mut();
            assert_eq!(XUserFindForDevice(dev.as_ptr() as *const c_void, &mut du), S_OK);
            assert_eq!(XUserCompare(u, du), 0);
            XUserCloseHandle(du);
            // Offline token fails like a console without Xbox Live.
            let mut tb: XAsyncBlock = std::mem::zeroed();
            assert_eq!(
                XUserGetTokenAndSignatureAsync(u, 0, b"GET\0".as_ptr(), b"https://xboxlive.com\0".as_ptr(), 0, std::ptr::null(), 0, std::ptr::null(), &mut tb),
                S_OK
            );
            assert_eq!(XAsyncGetStatus(&mut tb, true), E_CANNOT_CONNECT);
            let mut tsz = 0usize;
            assert_ne!(XUserGetTokenAndSignatureResultSize(&mut tb, &mut tsz), S_OK);
            XUserCloseHandle(u);
            assert_eq!(XUserGetId(std::ptr::null_mut(), &mut id), E_INVALIDARG);
        }
    }

    #[test]
    fn save_folder_and_pls_path() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveFilesGetFolderWithUiAsync(std::ptr::null_mut(), b"test-scid\0".as_ptr(), &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut buf = [0u8; 512];
            assert_eq!(XGameSaveFilesGetFolderWithUiResult(&mut b, 512, buf.as_mut_ptr()), S_OK);
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let dir = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(!dir.is_empty());
            std::fs::write(format!("{dir}/slot0.sav"), b"progress").unwrap();
            assert_eq!(std::fs::read(format!("{dir}/slot0.sav")).unwrap(), b"progress");
            let _ = std::fs::remove_file(format!("{dir}/slot0.sav"));
            let mut pls = [0u8; 512];
            let mut used = 0usize;
            assert_eq!(XPersistentLocalStorageGetPath(512, pls.as_mut_ptr(), &mut used), S_OK);
            let n = pls.iter().position(|&c| c == 0).unwrap_or(pls.len());
            let root = String::from_utf8_lossy(&pls[..n]).into_owned();
            assert!(!root.is_empty());
            // Default config mounts `{data}/pls` as the `S:` drive, so the title
            // gets the console drive root `S:\`; without that mount it gets the
            // host folder directly. Either form must be usable storage.
            if root.len() == 3 && root.ends_with(":\\") {
                assert_eq!(root, "S:\\");
            } else {
                assert!(std::path::Path::new(&root).is_dir(), "pls dir {root:?}");
            }
            // The host folder always exists.
            assert!(crate::state::data_dir().join("pls").is_dir());
            let mut size = 0usize;
            assert_eq!(XPersistentLocalStorageGetPathSize(&mut size), S_OK);
            assert_eq!(size, root.len() + 1);
            let mut info: XPersistentLocalStorageSpaceInfo = std::mem::zeroed();
            assert_eq!(XPersistentLocalStorageGetSpaceInfo(&mut info), S_OK);
            assert!(info.total_bytes > 0);
        }
    }

    #[test]
    fn async_run_completes_on_manual_dispatch() {
        unsafe extern "win64" fn work(_b: *mut XAsyncBlock) -> HRESULT {
            7
        }
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueCreate(xtaskqueue_linux::MODE_MANUAL, xtaskqueue_linux::MODE_MANUAL, &mut q), S_OK);
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.queue = q;
            assert_eq!(XAsyncRun(&mut b, work as usize), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, false), E_PENDING);
            assert!(xtaskqueue_linux::XTaskQueueDispatch(q, xtaskqueue_linux::PORT_WORK, 0));
            assert_eq!(XAsyncGetStatus(&mut b, false), 7);
            assert_eq!(xtaskqueue_linux::XTaskQueueTerminate(q, true, std::ptr::null_mut(), 0), S_OK);
            xtaskqueue_linux::XTaskQueueCloseHandle(q);
        }
    }

    unsafe extern "win64" fn count_block_completion(b: *mut XAsyncBlock) {
        let count = &*((*b).context as *const AtomicU32);
        count.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "win64" fn run_work(_b: *mut XAsyncBlock) -> HRESULT {
        7
    }

    #[test]
    fn async_complete_after_queue_handle_close_delivers_callback() {
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueCreate(xtaskqueue_linux::MODE_MANUAL, xtaskqueue_linux::MODE_IMMEDIATE, &mut q), S_OK);
            let queue = Arc::downgrade(&xtaskqueue_linux::queue_from(q).unwrap());
            let count = AtomicU32::new(0);
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.queue = q;
            b.context = &count as *const AtomicU32 as *mut c_void;
            b.callback = count_block_completion as usize;
            assert_eq!(XAsyncBegin(&mut b, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), begins_without_work as usize), S_OK);
            xtaskqueue_linux::XTaskQueueCloseHandle(q);
            XAsyncComplete(&mut b, S_OK, 0);
            assert_eq!(XAsyncGetStatus(&mut b, false), S_OK);
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(queue.upgrade().is_none(), "completed operation must release its queue");
        }
    }

    #[test]
    fn async_schedule_after_queue_handle_close_uses_begins_queue() {
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueCreate(xtaskqueue_linux::MODE_IMMEDIATE, xtaskqueue_linux::MODE_IMMEDIATE, &mut q), S_OK);
            let count = AtomicU32::new(0);
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.queue = q;
            b.context = &count as *const AtomicU32 as *mut c_void;
            b.callback = count_block_completion as usize;
            assert_eq!(XAsyncBegin(&mut b, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), begins_without_work as usize), S_OK);
            xtaskqueue_linux::XTaskQueueCloseHandle(q);
            let scheduled = XAsyncSchedule(&mut b, 0);
            let status = XAsyncGetStatus(&mut b, false);
            // Keep the negative control from leaving a pending state pointing at this stack block.
            if status == E_PENDING { XAsyncComplete(&mut b, E_ABORT, 0); }
            assert_eq!(scheduled, S_OK);
            assert_eq!(status, S_OK);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn async_run_after_queue_handle_close_delivers_callback() {
        unsafe {
            let mut q = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueCreate(xtaskqueue_linux::MODE_MANUAL, xtaskqueue_linux::MODE_IMMEDIATE, &mut q), S_OK);
            let mut dispatch_handle = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueDuplicateHandle(q, &mut dispatch_handle), S_OK);
            let count = AtomicU32::new(0);
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.queue = q;
            b.context = &count as *const AtomicU32 as *mut c_void;
            b.callback = count_block_completion as usize;
            assert_eq!(XAsyncRun(&mut b, run_work as usize), S_OK);
            xtaskqueue_linux::XTaskQueueCloseHandle(q);
            let dispatched = xtaskqueue_linux::XTaskQueueDispatch(dispatch_handle, xtaskqueue_linux::PORT_WORK, 0);
            let status = XAsyncGetStatus(&mut b, false);
            xtaskqueue_linux::XTaskQueueCloseHandle(dispatch_handle);
            if status == E_PENDING { XAsyncComplete(&mut b, E_ABORT, 0); }
            assert!(dispatched);
            assert_eq!(status, 7);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }

    /// The title may free its context as soon as `XUserUnregisterForDeviceAssociationChanged`
    /// returns: a callback still queued must not run after that (`wait = true` waits for a
    /// running one; a queued one is canceled). Linux twin of the upstream Windows test
    /// (same manual-queue dispatch shape via `xtaskqueue_linux`).
    #[test]
    fn an_unregistered_association_callback_does_not_run() {
        unsafe extern "win64" fn count(ctx: *mut c_void, _change: *const DeviceAssociationChange) {
            let n = ctx as *const AtomicU32;
            (*n).fetch_add(1, Ordering::SeqCst);
        }

        unsafe {
            // a manual queue: the callback waits in it until this test dispatches
            let mut q: *mut c_void = std::ptr::null_mut();
            assert_eq!(xtaskqueue_linux::XTaskQueueCreate(xtaskqueue_linux::MODE_MANUAL, xtaskqueue_linux::MODE_MANUAL, &mut q), S_OK);
            let calls = AtomicU32::new(0);
            let mut token = 0u64;
            assert_eq!(XUserRegisterForDeviceAssociationChanged(q, &calls as *const AtomicU32 as *mut c_void, count as usize, &mut token), S_OK);
            user_signed_in(); // queues the association callback
            assert_eq!(calls.load(Ordering::SeqCst), 0, "not before the queue runs it");
            assert!(XUserUnregisterForDeviceAssociationChanged(token, true));
            while xtaskqueue_linux::XTaskQueueDispatch(q, PORT_COMPLETION, 0) {}
            assert_eq!(calls.load(Ordering::SeqCst), 0, "the callback was unregistered: its context may be gone");
            // a registration made now hears of the association as usual (the cancel is per
            // callback), and one that is not unregistered does run
            let mut token2 = 0u64;
            assert_eq!(XUserRegisterForDeviceAssociationChanged(q, &calls as *const AtomicU32 as *mut c_void, count as usize, &mut token2), S_OK);
            assert_eq!(token2, token + 1);
            while xtaskqueue_linux::XTaskQueueDispatch(q, PORT_COMPLETION, 0) {}
            assert_eq!(calls.load(Ordering::SeqCst), 1, "a registered callback still runs");
            assert!(XUserUnregisterForDeviceAssociationChanged(token2, false));
            xtaskqueue_linux::XTaskQueueCloseHandle(q);
        }
    }

    #[test]
    fn xuser_compare_does_not_overflow_pointer_difference() {
        unsafe {
            let null = std::ptr::null_mut();
            let lo = 0x7FFF_FFFF_FFFF_FFFFusize as *mut c_void;
            let hi = 0x8000_0000_0000_0000usize as *mut c_void;
            let max = usize::MAX as *mut c_void;
            assert_eq!(XUserCompare(lo, lo), 0);
            assert_eq!(XUserCompare(lo, hi), -1);
            assert_eq!(XUserCompare(hi, lo), 1);
            assert_eq!(XUserCompare(null, max), -1);
            assert_eq!(XUserCompare(max, null), 1);
        }
    }

    #[test]
    fn result_and_lookup_functions_null_outptrs_on_failure() {
        unsafe {
            let mut user_ptr = 0xDEADBEEF as *mut c_void;
            assert_eq!(XUserFindUserByLocalId(LOCAL_ID + 999, &mut user_ptr), E_NOT_FOUND);
            assert_eq!(user_ptr, std::ptr::null_mut());

            let mut user_ptr2 = 0xDEADBEEF as *mut c_void;
            assert_eq!(XUserFindUserById(0x12345678, &mut user_ptr2), E_NOT_FOUND);
            assert_eq!(user_ptr2, std::ptr::null_mut());

            let mut dup_ptr = 0xDEADBEEF as *mut c_void;
            assert_eq!(XUserDuplicateHandle(std::ptr::null_mut(), &mut dup_ptr), E_INVALIDARG);
            assert_eq!(dup_ptr, std::ptr::null_mut());

            let mut add_ptr = 0xDEADBEEF as *mut c_void;
            let mut b_bad: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XUserAddResult(&mut b_bad, &mut add_ptr), E_INVALIDARG);
            assert_eq!(add_ptr, std::ptr::null_mut());

            let mut token_ptr = 0xDEADBEEF as *mut XUserGetTokenAndSignatureData;
            assert_eq!(
                XUserGetTokenAndSignatureResult(&mut b_bad, 0, std::ptr::null_mut(), &mut token_ptr, std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(token_ptr, std::ptr::null_mut());

            let mut token16_ptr = 0xDEADBEEF as *mut XUserGetTokenAndSignatureUtf16Data;
            assert_eq!(
                XUserGetTokenAndSignatureUtf16Result(&mut b_bad, 0, std::ptr::null_mut(), &mut token16_ptr, std::ptr::null_mut()),
                E_INVALIDARG
            );
            assert_eq!(token16_ptr, std::ptr::null_mut());
        }
    }
}
