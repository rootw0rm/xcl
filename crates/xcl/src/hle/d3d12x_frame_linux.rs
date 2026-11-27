//! Linux twin of the console frame clock (`hle/d3d12x/frame.rs` is Windows-only).
//! Split 1a: the pure core — interval helpers plus token issuance and flip statistics.
//! Split 1b: the schedule state machine (`set_frame_interval`,
//! `schedule_frame_event`) plus the `signal_ancillary` honest stub. Split 1c:
//! the `Condvar`/monotonic clock thread with `wait_frame_event`. Split 1d(i):
//! ancillary handle routing — `register_ancillary` remembers the schedule
//! block's handles per type and the clock thread signals them at fire;
//! device slots #78–#81 route here from `d3d12x_linux` (queue #30 follows).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use crate::util::{E_FAIL, E_INVALIDARG, HRESULT, S_FALSE, S_OK};

const TOKEN_MASK: u64 = 0xFFFF_FFFF_FFFF;
const KNOWN_TOKENS: usize = 64;
const FLIPPED_KEEP: usize = 64;

/// `XCL_FRAME_INTERVAL_US`: `None` honors the title, `Some(0)` uncaps, else clamp.
pub fn parse_interval_override(raw: Option<&str>) -> Option<u32> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("uncapped") || s.eq_ignore_ascii_case("off") {
        return Some(0);
    }
    s.parse::<u32>().ok()
}

/// Effective clock interval (µs); `None` means uncapped.
pub fn effective_clock_interval_us(requested_us: u32, override_us: Option<u32>) -> Option<u32> {
    if requested_us == 0 {
        return None;
    }
    match override_us {
        None => Some(requested_us),
        Some(0) => None,
        Some(ov) => Some(ov.min(requested_us).max(1000)),
    }
}

/// Intervals the console accepts (µs); the 120 Hz ones only on high-refresh.
pub fn interval_valid(len_us: u32, high_refresh: bool) -> bool {
    let near = |base: u32, span: u32| len_us.abs_diff(base) <= span;
    if near(8333, 9) || near(25000, 25) {
        return high_refresh;
    }
    near(16667, 16) || near(33333, 34) || near(50000, 49) || near(66667, 67)
}

/// First deadline `base + offset + k·interval` after `after` (1b drives waits with this).
pub fn next_deadline(base: i64, interval: i64, offset: i64, after: i64) -> i64 {
    if interval <= 0 {
        return i64::MAX;
    }
    let first = base + offset;
    if after < first {
        return first;
    }
    first + ((after - first) / interval + 1) * interval
}

/// Monotonic now in ns.
fn nanos() -> i64 {
    static BOOT: OnceLock<std::time::Instant> = OnceLock::new();
    let boot = BOOT.get_or_init(std::time::Instant::now);
    boot.elapsed().as_nanos().min(i64::MAX as u128) as i64
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
/// Test-only serialization: the clock is one per process, so every test that
/// drives it (this module's and `d3d12x_linux`'s 1d routing test) takes turns.
#[cfg(test)]
pub(crate) static CLOCK_SERIAL: Mutex<()> = Mutex::new(());
static ISSUED: Mutex<Vec<(u64, i64)>> = Mutex::new(Vec::new()); // (token, origin ns)
static FLIPPED: Mutex<Vec<(u64, i64, i64)>> = Mutex::new(Vec::new()); // (token, origin, flip)
static LAST_ORIGIN: AtomicI64 = AtomicI64::new(0);

/// Hand out a waiter token for `origin` (`token_in == 0` issues one, else re-stamps).
/// 1c waits call this; the unit tests below drive it directly too.
fn issue(token: &mut u64, token_in: u64, origin: i64) {
    let mut issued = ISSUED.lock().unwrap();
    if token_in != 0 {
        if let Some(e) = issued.iter_mut().find(|e| e.0 == token_in) {
            e.1 = origin;
        }
        return;
    }
    let mut t = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed) & TOKEN_MASK;
    if t == 0 {
        t = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed) & TOKEN_MASK;
    }
    if issued.len() >= KNOWN_TOKENS {
        issued.remove(0);
    }
    issued.push((t, origin));
    *token = t;
}

/// `PresentX` flipped `token` (1a records it; later splits route the call).
pub fn mark_flipped(token: u64) {
    if token == 0 {
        return;
    }
    let origin = ISSUED
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e.0 == token)
        .map(|e| e.1)
        .unwrap_or_else(|| LAST_ORIGIN.load(Ordering::Relaxed));
    let mut f = FLIPPED.lock().unwrap();
    f.push((token, origin, nanos()));
    if f.len() > FLIPPED_KEEP {
        let n = f.len() - FLIPPED_KEEP;
        f.drain(..n);
    }
}

/// #81 core: origin/flip ns of a flipped token, `E_FAIL` when not flipped yet.
pub fn frame_statistics(token: u64) -> Result<(i64, i64), HRESULT> {
    FLIPPED
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|f| f.0 == token)
        .map(|f| (f.1, f.2))
        .map(|(o, f)| (if o != 0 && o <= f { o } else { f }, f))
        .ok_or(E_FAIL)
}

// --- Split 1c: `Condvar`/monotonic clock thread + `wait_frame_event`. ---
// Split 1d(i) routes device slots #78–#81 here (behind `XCL_VULKAN_BACKEND` —
// no behavior change while unset); queue #30 `PresentX` follows in 1d(ii).

const TYPES: usize = 3;
/// What `schedule_frame_event` returns when it starts the clock.
const STARTED_CLOCK: HRESULT = 0x1000_0000;
/// `ScheduleFrameEventX` flag.
const UNSCHEDULE: u32 = 1;
/// `WaitFrameEventX` flag: take a fresh stamp on timeout instead of failing it.
const TOKEN_ON_TIMEOUT: u32 = 2;
/// Origins fired and not yet consumed by a wait (the console caps pending
/// origins at 4): each token gets its own frame's origin.
const ORIGINS_KEEP: usize = 4;

struct Clock {
    interval_us: u32,
    active: bool,
    offset_us: [u32; TYPES],
    scheduled: [bool; TYPES],
    /// Origins fired since boot, per type: waiters sleep until this moves.
    fired: [u64; TYPES],
    /// Bumped on every change: the clock thread re-plans off this.
    generation: u64,
    base_ns: i64,
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock {
    interval_us: 0,
    active: false,
    offset_us: [0; TYPES],
    scheduled: [false; TYPES],
    fired: [0; TYPES],
    generation: 0,
    base_ns: 0,
});
/// Wakes the clock thread and the waiters (paired with `CLOCK`'s guard).
static CLOCK_WAKE: Condvar = Condvar::new();
static THREAD: AtomicBool = AtomicBool::new(false);
static ORIGINS: Mutex<VecDeque<i64>> = Mutex::new(VecDeque::new());

/// `XCL_FRAME_INTERVAL_US` override, read once (1a parses, the clock applies).
fn interval_override() -> Option<u32> {
    static OVERRIDE: OnceLock<Option<u32>> = OnceLock::new();
    *OVERRIDE
        .get_or_init(|| parse_interval_override(std::env::var("XCL_FRAME_INTERVAL_US").ok().as_deref()))
}

/// Device #78 twin (length only): `E_INVALIDARG` off-display intervals, else
/// `S_FALSE`; length 0 stops the clock. 120 Hz needs a high-refresh display,
/// unprobed on Linux, so those lengths stay invalid until 1c.
pub fn set_frame_interval(len_us: u32) -> HRESULT {
    if len_us != 0 && !interval_valid(len_us, false) {
        return E_INVALIDARG;
    }
    {
        let mut c = CLOCK.lock().unwrap();
        if c.interval_us != len_us {
            c.scheduled[0] = false;
            if c.active && len_us != 0 {
                c.base_ns = nanos();
            }
        }
        c.interval_us = len_us;
        if len_us == 0 {
            c.active = false;
            c.scheduled = [false; TYPES];
        }
        c.generation += 1;
    }
    CLOCK_WAKE.notify_all();
    S_FALSE
}

/// Device #79 twin: `E_INVALIDARG` without an interval, for type >= 3, for a
/// nonzero type before the origin is scheduled, or when unscheduling the
/// origin; `STARTED_CLOCK` when this call starts the clock, else `S_FALSE`.
pub fn schedule_frame_event(ty: u32, offset_us: u32, flags: u32) -> HRESULT {
    let unschedule = flags & UNSCHEDULE != 0;
    let mut c = CLOCK.lock().unwrap();
    if c.interval_us == 0
        || (ty as usize) >= TYPES
        || (!c.active && ty != 0)
        || (unschedule && (ty == 0 || !c.scheduled[0]))
    {
        return E_INVALIDARG;
    }
    let started = !c.active;
    if started {
        c.active = true;
        c.base_ns = nanos();
    }
    if unschedule {
        c.scheduled[ty as usize] = false;
    } else {
        c.scheduled[ty as usize] = true;
        c.offset_us[ty as usize] = offset_us % c.interval_us.max(1);
    }
    c.generation += 1;
    drop(c);
    ensure_clock_thread();
    CLOCK_WAKE.notify_all();
    if started {
        STARTED_CLOCK
    } else {
        S_FALSE
    }
}

/// Ancillary handles of a frame event: honest stub (handles stay unrouted
/// until 1d). Returns `S_FALSE` — nothing was signaled — and never fake-successes.
pub fn signal_ancillary(count: usize) -> HRESULT {
    if count > 0 {
        crate::xlog_debug!("frame clock: {count} ancillary handles not routed yet");
    }
    S_FALSE
}

/// Ancillary handles per frame-event type, registered at schedule time (1d):
/// the clock thread signals each with `kernelx_linux::SetEvent` when the type
/// fires; rescheduling replaces the set, unscheduling clears it.
static ANCILLARY: Mutex<[Vec<usize>; TYPES]> = Mutex::new([Vec::new(), Vec::new(), Vec::new()]);

/// Parse a schedule ancillary block (`{ u32 count; HANDLE* list @8;
/// i32* signaled @0x10 }`, null = none) and remember its handles for `ty`.
/// `E_INVALIDARG` when the count exceeds the console's 63-handle cap, like
/// Windows. Linux trusts the title's block (no `/proc/self/maps` walk yet —
/// `gpu::shader::readable` is windows-only); a garbage block faults like any
/// other Linux slot instead of failing closed.
pub fn register_ancillary(ty: u32, block: *const u8, unschedule: bool) -> HRESULT {
    let mut handles = Vec::new();
    if !block.is_null() {
        let count = unsafe { (block as *const u32).read_unaligned() } as usize;
        if count > 63 {
            return E_INVALIDARG;
        }
        if count > 0 {
            let list = unsafe { (block.add(8) as *const usize).read_unaligned() };
            if list == 0 {
                return E_INVALIDARG;
            }
            handles = (0..count)
                .map(|i| unsafe { (list as *const usize).add(i).read_unaligned() })
                .collect();
        }
    }
    if (ty as usize) < TYPES {
        ANCILLARY.lock().unwrap()[ty as usize] = if unschedule { Vec::new() } else { handles };
    }
    S_OK
}

/// Start the clock thread once: it sleeps on `CLOCK_WAKE` between schedules.
fn ensure_clock_thread() {
    if !THREAD.swap(true, Ordering::SeqCst) {
        let _ = std::thread::Builder::new().name("xcl-frame-clock".into()).spawn(clock_thread);
    }
}

fn clock_thread() {
    let mut last = [i64::MIN / 2; TYPES];
    loop {
        // The next events of the current schedule: every type due at the
        // earliest deadline fires together, like Windows.
        let plan = {
            let clock = CLOCK.lock().unwrap();
            if !clock.active {
                None
            } else {
                match effective_clock_interval_us(clock.interval_us, interval_override()) {
                    None => None,
                    Some(eff_us) => {
                        let interval_ns = eff_us as i64 * 1000;
                        let now = nanos();
                        let mut due = Vec::new();
                        for i in 0..TYPES {
                            if !clock.scheduled[i] {
                                continue;
                            }
                            let off_ns = (clock.offset_us[i] % eff_us.max(1)) as i64 * 1000;
                            let mut d = next_deadline(clock.base_ns, interval_ns, off_ns, now - 1);
                            if d - last[i] < interval_ns / 2 {
                                d += interval_ns; // this point of the frame already fired
                            }
                            due.push((d, i));
                        }
                        let first = due.iter().map(|e| e.0).min();
                        first.map(|f| {
                            (
                                clock.generation,
                                f,
                                due.into_iter().filter(|e| e.0 == f).collect::<Vec<_>>(),
                            )
                        })
                    }
                }
            }
        };
        let Some((generation, first, due)) = plan else {
            let clock = CLOCK.lock().unwrap();
            let _unused = CLOCK_WAKE.wait(clock).unwrap();
            continue;
        };
        let wait_ns = first - nanos();
        if wait_ns > 0 {
            let clock = CLOCK.lock().unwrap();
            if clock.generation != generation {
                continue;
            }
            let (clock, _) = CLOCK_WAKE
                .wait_timeout(clock, std::time::Duration::from_nanos(wait_ns as u64))
                .unwrap();
            if clock.generation != generation || nanos() < first {
                continue;
            }
        }
        let mut fired_aux = Vec::new();
        {
            let mut clock = CLOCK.lock().unwrap();
            if clock.generation != generation {
                continue;
            }
            let mut origins = ORIGINS.lock().unwrap();
            let aux = ANCILLARY.lock().unwrap();
            for (_, i) in due {
                last[i] = first;
                clock.fired[i] += 1;
                if i == 0 {
                    LAST_ORIGIN.store(first, Ordering::Relaxed);
                    origins.push_back(first);
                    while origins.len() > ORIGINS_KEEP {
                        origins.pop_front();
                    }
                }
                fired_aux.extend(aux[i].iter().copied());
            }
        }
        CLOCK_WAKE.notify_all();
        // outside the clock lock: SetEvent takes the events lock
        for h in fired_aux {
            unsafe { crate::hle::kernelx_linux::SetEvent(h) };
        }
    }
}

/// Device #80 twin: block until event `ty` fires (or `timeout_ms` elapses).
/// `S_OK` names the fired event (`token` takes a fresh origin token,
/// re-stamped when one is passed in); `S_FALSE` on timeout; `E_INVALIDARG`
/// when `ty` is not scheduled. Uncapped (`XCL_FRAME_INTERVAL_US=0`) returns
/// immediately with a fresh token, like Windows.
pub fn wait_frame_event(ty: u32, timeout_ms: u32, flags: u32, token: &mut u64) -> HRESULT {
    let token_in = *token;
    let start = {
        let clock = CLOCK.lock().unwrap();
        if (ty as usize) >= TYPES || !clock.scheduled[ty as usize] {
            return E_INVALIDARG;
        }
        if ty == 0
            && token_in != 0
            && (!ISSUED.lock().unwrap().iter().any(|e| e.0 == token_in)
                || flags & TOKEN_ON_TIMEOUT != 0)
        {
            return E_INVALIDARG;
        }
        clock.fired[ty as usize]
    };
    if interval_override() == Some(0) {
        let now = nanos();
        if ty == 0 {
            LAST_ORIGIN.store(now, Ordering::Relaxed);
            issue(token, token_in, now);
        }
        return S_OK;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    loop {
        let clock = CLOCK.lock().unwrap();
        if clock.fired[ty as usize] != start {
            break;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return S_FALSE;
        }
        let (clock, r) = CLOCK_WAKE.wait_timeout(clock, deadline - now).unwrap();
        if clock.fired[ty as usize] != start {
            break;
        }
        if r.timed_out() {
            return S_FALSE;
        }
    }
    if ty == 0 {
        let origin = ORIGINS.lock().unwrap().pop_front().unwrap_or_else(nanos);
        issue(token, token_in, origin);
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_helpers_match_the_windows_clock() {
        assert_eq!(parse_interval_override(None), None);
        assert_eq!(parse_interval_override(Some("bad")), None);
        assert_eq!(parse_interval_override(Some("0")), Some(0));
        assert_eq!(parse_interval_override(Some("OFF")), Some(0));
        assert_eq!(parse_interval_override(Some("8333")), Some(8333));
        assert_eq!(effective_clock_interval_us(0, Some(8333)), None);
        assert_eq!(effective_clock_interval_us(33333, Some(0)), None);
        assert_eq!(effective_clock_interval_us(33333, Some(8333)), Some(8333));
        assert_eq!(effective_clock_interval_us(33333, Some(200)), Some(1000));
        assert!(interval_valid(16667, false) && !interval_valid(16000, false));
        assert!(!interval_valid(8333, false) && interval_valid(8333, true));
        assert_eq!(
            (
                next_deadline(100, 10, 3, 50),
                next_deadline(100, 10, 3, 103)
            ),
            (103, 113)
        );
    }

    #[test]
    fn tokens_and_flips() {
        let (mut a, mut b) = (0u64, 0u64);
        issue(&mut a, 0, 42);
        issue(&mut b, 0, 43);
        assert!(a != 0 && b > a);
        let av = a;
        issue(&mut a, av, 44);
        assert_eq!(
            ISSUED.lock().unwrap().iter().find(|e| e.0 == a).unwrap().1,
            44
        );
        for t in [a, b] {
            mark_flipped(t);
        }
        mark_flipped(0); // never recorded
        assert_eq!(frame_statistics(a).unwrap().0, 44);
        assert!(frame_statistics(b).unwrap().0 >= 43);
        assert_eq!(frame_statistics(0xDEAD), Err(E_FAIL));
    }

    /// 1b: the schedule state machine validates like Windows.
    #[test]
    fn schedule_validates_like_the_windows_clock() {
        let _serial = CLOCK_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(set_frame_interval(16000), E_INVALIDARG);
        assert_eq!(set_frame_interval(16667), S_FALSE);
        assert_eq!(schedule_frame_event(1, 0, 0), E_INVALIDARG, "type 1 needs the origin");
        assert_eq!(schedule_frame_event(3, 0, 0), E_INVALIDARG, "no type 3");
        let hr = schedule_frame_event(0, 0, 0);
        assert!(hr == STARTED_CLOCK || hr == S_FALSE, "{hr:#x}");
        assert_eq!(schedule_frame_event(0, 0, UNSCHEDULE), E_INVALIDARG, "the origin never unschedules");
        assert_eq!(schedule_frame_event(1, 0, UNSCHEDULE), S_FALSE, "unscheduling idle type 1");
        assert_eq!(signal_ancillary(0), S_FALSE, "honest stub: nothing signaled");
        assert_eq!(set_frame_interval(0), S_FALSE, "stops the clock");
        assert_eq!(schedule_frame_event(0, 0, 0), E_INVALIDARG, "no interval, no schedule");
    }

    /// 1c failing-first: waits on a 60 Hz origin return about one interval
    /// apart with fresh tokens (mirrors the Windows `origin_events_...` test).
    #[test]
    fn origin_waits_pace_like_the_windows_clock() {
        let _serial = CLOCK_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(set_frame_interval(16667), S_FALSE);
        assert_eq!(wait_frame_event(0, 10, 0, &mut 0), E_INVALIDARG, "not scheduled");
        let hr = schedule_frame_event(0, 0, 0);
        assert!(hr == STARTED_CLOCK || hr == S_FALSE, "{hr:#x}");
        let mut tokens = Vec::new();
        let t0 = std::time::Instant::now();
        for _ in 0..4 {
            let mut t = 0u64;
            assert_eq!(wait_frame_event(0, 200, 0, &mut t), S_OK);
            tokens.push(t);
        }
        let ms = t0.elapsed().as_millis();
        assert!((25..=500).contains(&ms), "4 origins at 60 Hz took {ms} ms");
        assert!(tokens.windows(2).all(|w| w[1] > w[0]), "{tokens:?}");
        mark_flipped(tokens[2]);
        assert!(frame_statistics(tokens[2]).is_ok());
        assert_eq!(frame_statistics(0xDEAD), Err(E_FAIL));
        assert_eq!(wait_frame_event(1, 10, 0, &mut 0), E_INVALIDARG, "type 1 idle");
        assert_eq!(set_frame_interval(0), S_FALSE, "stops the clock");
    }
}
