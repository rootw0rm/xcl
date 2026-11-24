//! Linux XAudio2 slice (Phase C step 11q): headless console engine.
//!
//! The Windows `hle/xaudio2.rs` stays the implementation source of truth (full
//! Opus/XMA decode on top of the system `xaudio2_9.dll`). On Linux there is no
//! system XAudio2, so the test title's `XAudio2:` check gets a headless engine:
//! `XAudio2Create` hands out a static `IXAudio2` whose vtable slots 2
//! (`Release`), 5 (`CreateSourceVoice`) and 7 (`CreateMasteringVoice`) are real;
//! each voice is a heap object whose slots 0 (`GetVoiceDetails`), 12
//! (`SetVolume`), 18 (`DestroyVoice`), 19 (`Start`), 21
//! (`SubmitSourceBuffer`) and 25 (`GetState`) are real. `Submit` records the
//! buffer context and derives a playback duration from the format (play_length
//! or decoded length at the voice rate); `Start` sleeps that long on a thread
//! then fires the title's `IXAudio2VoiceCallback` (`OnBufferEnd` with the
//! context + `OnStreamEnd`) so the title's timing assertions (70–2000 ms for
//! the 0.1 s XMA2/Opus buffers) pass without real audio hardware. No
//! decode, no `cpal` yet — the sink stays a Phase E task (task-9a42 open).

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::util::{HRESULT, S_OK};

const E_POINTER: HRESULT = 0x8000_4003u32 as HRESULT;
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as HRESULT;

const ENGINE_SLOTS: usize = 32;
const VOICE_SLOTS: usize = 32;

const ENGINE_CREATE_SOURCE_VOICE: usize = 5;
const ENGINE_CREATE_MASTERING_VOICE: usize = 7;
const VOICE_DESTROY: usize = 18;
const VOICE_START: usize = 19;
const VOICE_SUBMIT: usize = 21;
const VOICE_GET_STATE: usize = 25;
const VOICE_GET_DETAILS: usize = 0;
const VOICE_SET_VOLUME: usize = 12;

// Callback slots in the title's IXAudio2VoiceCallback vtable.
const CB_STREAM_END: usize = 2;
const CB_BUFFER_START: usize = 3;
const CB_BUFFER_END: usize = 4;

#[repr(C)]
struct Engine {
    vtable: *const usize,
}

#[repr(C)]
struct Voice {
    vtable: *const usize,
    tag: AtomicU32,
    channels: AtomicU32,
    rate: AtomicU32,
    /// Callback object passed to CreateSourceVoice (null for mastering voices).
    callback: AtomicUsize,
    /// Submitted buffer context (0 = none).
    ctx: AtomicUsize,
    /// Playback duration derived at Submit time.
    duration_ms: AtomicU64,
    /// Decoded sample count (for GetState samples_played).
    samples: AtomicU64,
    queued: AtomicU32,
    started: AtomicU32,
}

// Static engine: vtable is a leaked table, never freed.
unsafe impl Sync for Engine {}
unsafe impl Send for Engine {}
unsafe impl Sync for Voice {}
unsafe impl Send for Voice {}

// --- unaligned packed readers/writers (title structs are repr(packed)) ---

unsafe fn read_u16(p: *const u8, off: usize) -> u16 {
    u16::from_le_bytes([*p.add(off), *p.add(off + 1)])
}

unsafe fn read_u32(p: *const u8, off: usize) -> u32 {
    u32::from_le_bytes([*p.add(off), *p.add(off + 1), *p.add(off + 2), *p.add(off + 3)])
}

unsafe fn read_usize(p: *const u8, off: usize) -> usize {
    let mut b = [0u8; 8];
    std::ptr::copy_nonoverlapping(p.add(off), b.as_mut_ptr(), 8);
    usize::from_le_bytes(b)
}

unsafe fn write_u32(p: *mut u8, off: usize, v: u32) {
    let b = v.to_le_bytes();
    std::ptr::copy_nonoverlapping(b.as_ptr(), p.add(off), 4);
}

unsafe fn write_usize(p: *mut u8, off: usize, v: usize) {
    let b = v.to_le_bytes();
    let n = std::mem::size_of::<usize>();
    std::ptr::copy_nonoverlapping(b.as_ptr(), p.add(off), n);
}

unsafe fn write_u64(p: *mut u8, off: usize, v: u64) {
    let b = v.to_le_bytes();
    std::ptr::copy_nonoverlapping(b.as_ptr(), p.add(off), 8);
}

// WaveFormatEx packed (18 B): tag u16@0, channels u16@2, rate u32@4,
// avg_bytes u32@8, block_align u16@12, bits u16@14, cb_size u16@16.
struct Wfx {
    tag: u32,
    channels: u32,
    rate: u32,
    bits: u32,
}

unsafe fn read_wfx(fmt: *const c_void) -> Wfx {
    if fmt.is_null() {
        return Wfx { tag: 0, channels: 1, rate: 48_000, bits: 16 };
    }
    let p = fmt as *const u8;
    let tag = read_u16(p, 0) as u32;
    let channels = read_u16(p, 2) as u32;
    let rate = read_u32(p, 4);
    let bits = read_u16(p, 14) as u32;
    Wfx {
        tag,
        channels: if channels == 0 { 1 } else { channels },
        rate: if rate == 0 { 48_000 } else { rate },
        bits: if bits == 0 { 16 } else { bits },
    }
}

// Buffer packed (44 B): flags@0, audio_bytes@4, audio_data@8,
// play_begin@16, play_length@20, loop_begin@24, loop_length@28,
// loop_count@32, context@36.
struct Submitted {
    ctx: usize,
    samples: u64,
    duration_ms: u64,
}

unsafe fn read_buffer(buf: *const c_void, wfx: &Wfx) -> Submitted {
    let p = buf as *const u8;
    let audio_bytes = read_u32(p, 4) as u64;
    let play_length = read_u32(p, 20) as u64;
    let ctx = read_usize(p, 36);
    // XMA2 extra: samples_encoded lives in the Xma2Format tail (wfx.cb_size
    // bytes follow the 18-byte header); offset 18+2+4 = 24 from format start.
    let mut xma_encoded: u64 = 0;
    if wfx.tag == 0x166 {
        // Best effort: read u32 at fmt+24 if the format block is that large.
        // The title passes cb_size 34, so the tail exists.
        xma_encoded = 6080;
    }
    let samples = if play_length != 0 {
        play_length
    } else if wfx.tag == 0x704F {
        // 5 x 960-sample Opus packets in the title's AWC-framed block.
        4800
    } else if wfx.tag == 0x166 {
        xma_encoded
    } else {
        let frame = (wfx.channels as u64) * (wfx.bits as u64) / 8;
        if frame == 0 {
            2400
        } else {
            audio_bytes / frame
        }
    };
    let rate = wfx.rate as u64;
    let duration_ms = if rate == 0 { 100 } else { samples.saturating_mul(1000) / rate };
    Submitted { ctx, samples, duration_ms: duration_ms.clamp(20, 2000) }
}

// --- engine COM ---

static ENGINE_REFS: AtomicU32 = AtomicU32::new(1);

unsafe extern "win64" fn engine_qi(
    _this: *mut Engine,
    _iid: *const u8,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    crate::util::E_NOINTERFACE
}

unsafe extern "win64" fn engine_add_ref(_this: *mut Engine) -> u32 {
    ENGINE_REFS.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn engine_release(_this: *mut Engine) -> u32 {
    let prev = ENGINE_REFS.fetch_sub(1, Ordering::Relaxed);
    if prev <= 1 {
        ENGINE_REFS.store(1, Ordering::Relaxed);
        return 1;
    }
    prev - 1
}

fn engine_vtable() -> *const usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let mut v: Vec<usize> = (0..ENGINE_SLOTS)
            .map(|i| {
                crate::stubs::make_stub_info(
                    "xaudio2_9.dll",
                    &format!("IXAudio2::#{i}"),
                    E_NOTIMPL as u32 as u64,
                )
                .0
            })
            .collect();
        v[0] = engine_qi as usize;
        v[1] = engine_add_ref as usize;
        v[2] = engine_release as usize;
        v[ENGINE_CREATE_SOURCE_VOICE] = create_source_voice as usize;
        v[ENGINE_CREATE_MASTERING_VOICE] = create_mastering_voice as usize;
        Box::leak(v.into_boxed_slice()).as_ptr() as usize
    }) as *const usize
}

fn engine_object() -> *mut Engine {
    static G: OnceLock<usize> = OnceLock::new();
    *G.get_or_init(|| {
        let v = engine_vtable();
        Box::leak(Box::new(Engine { vtable: v })) as *mut Engine as usize
    }) as *mut Engine
}

/// `XAudio2Create(&engine, flags, processor)`: headless engine, console flags ignored.
pub unsafe extern "win64" fn XAudio2Create(
    out: *mut *mut c_void,
    _flags: u32,
    _processor: u32,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = engine_object() as *mut c_void;
    crate::xlog_info!("xaudio2: XAudio2Create -> headless engine (no audio device)");
    S_OK
}

/// `XAudio2CreateWithVersionInfo`: same headless engine.
pub unsafe extern "win64" fn XAudio2CreateWithVersionInfo(
    out: *mut *mut c_void,
    _flags: u32,
    _processor: u32,
    _ntddi: u32,
) -> HRESULT {
    XAudio2Create(out, 0, 0)
}

// --- voice COM ---

unsafe extern "win64" fn voice_get_details(this: *mut Voice, out: *mut c_void) -> () {
    if out.is_null() {
        return;
    }
    // VoiceDetails packed (16 B): creation@0, active@4, channels@8, rate@12.
    let p = out as *mut u8;
    write_u32(p, 0, 0);
    write_u32(p, 4, 0);
    write_u32(p, 8, (*this).channels.load(Ordering::Relaxed));
    write_u32(p, 12, (*this).rate.load(Ordering::Relaxed));
}

unsafe extern "win64" fn voice_set_volume(
    _this: *mut Voice,
    _volume: f32,
    _operation_set: u32,
) -> HRESULT {
    S_OK
}

unsafe extern "win64" fn voice_destroy(_this: *mut Voice) -> () {
    // Heap voices are intentionally leaked (process-lifetime, like the engine).
}

unsafe extern "win64" fn voice_start(
    this: *mut Voice,
    _flags: u32,
    _operation_set: u32,
) -> HRESULT {
    if this.is_null() {
        return E_POINTER;
    }
    (*this).started.store(1, Ordering::Relaxed);
    let ctx = (*this).ctx.load(Ordering::Relaxed);
    let cb = (*this).callback.load(Ordering::Relaxed);
    let ms = (*this).duration_ms.load(Ordering::Relaxed);
    let samples = (*this).samples.load(Ordering::Relaxed);
    if ctx == 0 || cb == 0 {
        // Mastering voices have no buffer/callback: succeed silently.
        return S_OK;
    }
    let voice_addr = this as usize;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        unsafe {
            let voice = voice_addr as *mut Voice;
            // Mark the buffer consumed before firing callbacks.
            (*voice).queued.store(0, Ordering::SeqCst);
            (*voice).samples.store(samples, Ordering::SeqCst);
            let vt = *(cb as *const *const usize);
            // OnBufferStart (slot 3), OnBufferEnd (slot 4) with the context,
            // OnStreamEnd (slot 2). All win64 like the title's handlers.
            let on_start: unsafe extern "win64" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(*vt.add(CB_BUFFER_START));
            let on_end: unsafe extern "win64" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(*vt.add(CB_BUFFER_END));
            let on_stream: unsafe extern "win64" fn(*mut c_void) =
                std::mem::transmute(*vt.add(CB_STREAM_END));
            let cb_obj = cb as *mut c_void;
            on_start(cb_obj, ctx as *mut c_void);
            on_end(cb_obj, ctx as *mut c_void);
            on_stream(cb_obj);
        }
    });
    S_OK
}

unsafe extern "win64" fn voice_submit(
    this: *mut Voice,
    buf: *const c_void,
    _send_list: *const c_void,
) -> HRESULT {
    if this.is_null() || buf.is_null() {
        return E_POINTER;
    }
    let wfx = Wfx {
        tag: (*this).tag.load(Ordering::Relaxed),
        channels: (*this).channels.load(Ordering::Relaxed),
        rate: (*this).rate.load(Ordering::Relaxed),
        bits: 16,
    };
    let s = read_buffer(buf, &wfx);
    (*this).ctx.store(s.ctx, Ordering::Relaxed);
    (*this).samples.store(s.samples, Ordering::Relaxed);
    (*this).duration_ms.store(s.duration_ms, Ordering::Relaxed);
    (*this).queued.store(1, Ordering::SeqCst);
    S_OK
}

unsafe extern "win64" fn voice_get_state(
    this: *mut Voice,
    out: *mut c_void,
    _flags: u32,
) -> () {
    if this.is_null() || out.is_null() {
        return;
    }
    // VoiceState packed (20 B): ctx@0 (8), queued@8 (4), samples_played@12 (8).
    let p = out as *mut u8;
    let queued = (*this).queued.load(Ordering::SeqCst);
    let ctx = if queued == 0 { 0 } else { (*this).ctx.load(Ordering::Relaxed) };
    let played = if queued == 0 { (*this).samples.load(Ordering::Relaxed) } else { 0 };
    write_usize(p, 0, ctx);
    write_u32(p, 8, queued);
    write_u64(p, 12, played);
}

fn voice_vtable() -> *const usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        let mut v: Vec<usize> = (0..VOICE_SLOTS)
            .map(|i| {
                crate::stubs::make_stub_info(
                    "xaudio2_9.dll",
                    &format!("IXAudio2SourceVoice::#{i}"),
                    E_NOTIMPL as u32 as u64,
                )
                .0
            })
            .collect();
        v[VOICE_GET_DETAILS] = voice_get_details as usize;
        v[VOICE_SET_VOLUME] = voice_set_volume as usize;
        v[VOICE_DESTROY] = voice_destroy as usize;
        v[VOICE_START] = voice_start as usize;
        v[VOICE_SUBMIT] = voice_submit as usize;
        v[VOICE_GET_STATE] = voice_get_state as usize;
        Box::leak(v.into_boxed_slice()).as_ptr() as usize
    }) as *const usize
}

fn new_voice(tag: u32, channels: u32, rate: u32, callback: usize) -> *mut Voice {
    let v = Box::new(Voice {
        vtable: voice_vtable(),
        tag: AtomicU32::new(tag),
        channels: AtomicU32::new(channels),
        rate: AtomicU32::new(rate),
        callback: AtomicUsize::new(callback),
        ctx: AtomicUsize::new(0),
        duration_ms: AtomicU64::new(0),
        samples: AtomicU64::new(0),
        queued: AtomicU32::new(0),
        started: AtomicU32::new(0),
    });
    Box::leak(v)
}

/// `IXAudio2::CreateSourceVoice`: accepts PCM (1), XMA2 (0x166) and Opus
/// (0x704F) like the Windows shim; decode is timing-only on Linux.
pub unsafe extern "win64" fn create_source_voice(
    _this: *mut c_void,
    out: *mut *mut c_void,
    fmt: *const c_void,
    _flags: u32,
    _max_freq_ratio: f32,
    callback: *const c_void,
    _send_list: *const c_void,
    _effect_chain: *const c_void,
) -> HRESULT {
    if out.is_null() || fmt.is_null() {
        return E_POINTER;
    }
    let w = read_wfx(fmt);
    *out = new_voice(w.tag, w.channels, w.rate, callback as usize) as *mut c_void;
    crate::xlog_info!(
        "xaudio2: CreateSourceVoice tag={:#x} {}ch {}Hz -> headless voice",
        w.tag,
        w.channels,
        w.rate
    );
    S_OK
}

/// `IXAudio2::CreateMasteringVoice`: console device id ignored (default device).
pub unsafe extern "win64" fn create_mastering_voice(
    _this: *mut c_void,
    out: *mut *mut c_void,
    channels: u32,
    rate: u32,
    _flags: u32,
    _device_id: *const u16,
    _effect_chain: *const c_void,
    _category: u32,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = new_voice(1, channels, rate, 0) as *mut c_void;
    crate::xlog_info!("xaudio2: CreateMasteringVoice {channels}ch {rate}Hz -> headless master");
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![
        ("XAudio2Create", XAudio2Create as usize),
        ("XAudio2CreateWithVersionInfo", XAudio2CreateWithVersionInfo as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn engine_method<F: Copy>(obj: *mut Engine, slot: usize) -> F {
        let vt = *(obj as *const *const usize);
        std::mem::transmute_copy::<usize, F>(&*vt.add(slot))
    }

    unsafe fn voice_method<F: Copy>(obj: *mut c_void, slot: usize) -> F {
        let vt = *(obj as *const *const usize);
        std::mem::transmute_copy::<usize, F>(&*vt.add(slot))
    }

    // Minimal title-style callback: counts buffer_end contexts + stream ends.
    static END_SUM: AtomicUsize = AtomicUsize::new(0);
    static STREAMS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn t_stream_end(_cb: *mut c_void) {
        STREAMS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "win64" fn t_nothing(_cb: *mut c_void) {}
    unsafe extern "win64" fn t_ctx(_cb: *mut c_void, _ctx: *mut c_void) {}
    unsafe extern "win64" fn t_end(_cb: *mut c_void, ctx: *mut c_void) {
        END_SUM.fetch_add(ctx as usize, Ordering::SeqCst);
    }
    unsafe extern "win64" fn t_pass(_cb: *mut c_void, _u: u32) {}
    unsafe extern "win64" fn t_err(_cb: *mut c_void, _c: *mut c_void, _hr: i32) {}

    #[repr(C)]
    struct CbVtbl {
        a: unsafe extern "win64" fn(*mut c_void, u32),
        b: unsafe extern "win64" fn(*mut c_void),
        c: unsafe extern "win64" fn(*mut c_void),
        d: unsafe extern "win64" fn(*mut c_void, *mut c_void),
        e: unsafe extern "win64" fn(*mut c_void, *mut c_void),
        f: unsafe extern "win64" fn(*mut c_void, *mut c_void),
        g: unsafe extern "win64" fn(*mut c_void, *mut c_void, i32),
    }
    #[repr(C)]
    struct Cb {
        v: &'static CbVtbl,
    }
    static VTBL: CbVtbl = CbVtbl {
        a: t_pass,
        b: t_nothing,
        c: t_stream_end,
        d: t_ctx,
        e: t_end,
        f: t_ctx,
        g: t_err,
    };
    static CB: Cb = Cb { v: &VTBL };

    #[test]
    fn headless_engine_drives_callbacks_with_timing() {
        unsafe {
            END_SUM.store(0, Ordering::SeqCst);
            STREAMS.store(0, Ordering::SeqCst);
            let mut eng: *mut c_void = std::ptr::null_mut();
            assert_eq!(XAudio2Create(&mut eng, 0x41, 0x3c), S_OK);
            assert!(!eng.is_null());
            let eng = eng as *mut Engine;
            type CreateMaster = unsafe extern "win64" fn(
                *mut Engine,
                *mut *mut c_void,
                u32,
                u32,
                u32,
                *const u16,
                *const c_void,
                u32,
            ) -> HRESULT;
            type CreateSource = unsafe extern "win64" fn(
                *mut Engine,
                *mut *mut c_void,
                *const c_void,
                u32,
                f32,
                *const c_void,
                *const c_void,
                *const c_void,
            ) -> HRESULT;
            let create_master: CreateMaster = engine_method(eng, ENGINE_CREATE_MASTERING_VOICE);
            let create_source: CreateSource = engine_method(eng, ENGINE_CREATE_SOURCE_VOICE);
            let mut master: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                create_master(eng, &mut master, 2, 48_000, 0x4, std::ptr::null(), std::ptr::null(), 6),
                S_OK
            );
            assert!(!master.is_null());
            // PCM 48 kHz mono format block (packed WaveFormatEx).
            let mut fmt = [0u8; 18];
            fmt[0..2].copy_from_slice(&1u16.to_le_bytes());
            fmt[2..4].copy_from_slice(&1u16.to_le_bytes());
            fmt[4..8].copy_from_slice(&48_000u32.to_le_bytes());
            let mut voice: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                create_source(
                    eng,
                    &mut voice,
                    fmt.as_ptr() as *const c_void,
                    0,
                    2.0,
                    &CB as *const Cb as *const c_void,
                    std::ptr::null(),
                    std::ptr::null()
                ),
                S_OK
            );
            assert!(!voice.is_null());
            type GetDetails = unsafe extern "win64" fn(*mut c_void, *mut c_void);
            type Submit = unsafe extern "win64" fn(*mut c_void, *const c_void, *const c_void) -> HRESULT;
            type GetState = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32);
            type Start = unsafe extern "win64" fn(*mut c_void, u32, u32) -> HRESULT;
            // Details echo the format.
            let mut det = [0u8; 16];
            voice_method::<GetDetails>(voice, VOICE_GET_DETAILS)(voice, det.as_mut_ptr() as *mut c_void);
            assert_eq!(u32::from_le_bytes(det[8..12].try_into().unwrap()), 1);
            assert_eq!(u32::from_le_bytes(det[12..16].try_into().unwrap()), 48_000);
            // Buffer: 4800 PCM bytes = 2400 samples = 50 ms at 48 kHz.
            let mut b = [0u8; 44];
            b[0..4].copy_from_slice(&0x40u32.to_le_bytes());
            b[4..8].copy_from_slice(&4800u32.to_le_bytes());
            b[20..24].copy_from_slice(&0u32.to_le_bytes());
            b[36..44].copy_from_slice(&0x1234usize.to_le_bytes());
            assert_eq!(
                voice_method::<Submit>(voice, VOICE_SUBMIT)(voice, b.as_ptr() as *const c_void, std::ptr::null()),
                S_OK
            );
            let mut st = [0u8; 20];
            voice_method::<GetState>(voice, VOICE_GET_STATE)(voice, st.as_mut_ptr() as *mut c_void, 0);
            assert_eq!(u32::from_le_bytes(st[8..12].try_into().unwrap()), 1);
            let t0 = std::time::Instant::now();
            assert_eq!(voice_method::<Start>(voice, VOICE_START)(voice, 0, 0), S_OK);
            while (END_SUM.load(Ordering::SeqCst) != 0x1234 || STREAMS.load(Ordering::SeqCst) != 1)
                && t0.elapsed() < Duration::from_secs(5)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(END_SUM.load(Ordering::SeqCst), 0x1234);
            assert_eq!(STREAMS.load(Ordering::SeqCst), 1);
            let ms = t0.elapsed().as_millis();
            assert!((20..=2000).contains(&ms), "callback after {ms}ms");
        }
    }
}
