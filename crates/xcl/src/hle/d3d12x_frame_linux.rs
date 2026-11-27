//! Linux twin of the console frame clock (`hle/d3d12x/frame.rs` is Windows-only).
//! Split 1a: the pure core — interval helpers plus token issuance and flip statistics.
//! Split 1b (this): the schedule state machine (`set_frame_interval`,
//! `schedule_frame_event`) plus the `signal_ancillary` honest stub. Split 1c
//! adds the `Condvar`/monotonic clock thread with `wait_frame_event`;
//! device slots #78–#81 and queue #30 `PresentX` stay unrouted until 1d.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::util::{E_FAIL, E_INVALIDARG, HRESULT, S_FALSE};

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
static ISSUED: Mutex<Vec<(u64, i64)>> = Mutex::new(Vec::new()); // (token, origin ns)
static FLIPPED: Mutex<Vec<(u64, i64, i64)>> = Mutex::new(Vec::new()); // (token, origin, flip)
static LAST_ORIGIN: AtomicI64 = AtomicI64::new(0);

/// Hand out a waiter token for `origin` (`token_in == 0` issues one, else re-stamps).
/// 1b calls this from `wait_frame_event`; until then the unit test below drives it.
#[allow(dead_code)]
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

// --- Split 1b: schedule state machine + `signal_ancillary` honest stub. ---
// Unrouted (null default): device slots #78–#81 and queue #30 stay logging
// stubs in `d3d12x_linux`, so this clock only runs once routed behind
// `XCL_VULKAN_BACKEND` — no behavior change while unset. Split 1c adds the
// `Condvar`/monotonic clock thread with `wait_frame_event`; `PresentX` stays
// stubbed until 1d.

const TYPES: usize = 3;
/// What `schedule_frame_event` returns when it starts the clock.
const STARTED_CLOCK: HRESULT = 0x1000_0000;
/// `ScheduleFrameEventX` flag.
const UNSCHEDULE: u32 = 1;

struct Clock {
    interval_us: u32,
    active: bool,
    offset_us: [u32; TYPES],
    scheduled: [bool; TYPES],
    /// Bumped on every change: the 1c clock thread re-plans off this.
    generation: u64,
    base_ns: i64,
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock {
    interval_us: 0,
    active: false,
    offset_us: [0; TYPES],
    scheduled: [false; TYPES],
    generation: 0,
    base_ns: 0,
});

/// `XCL_FRAME_INTERVAL_US` override, read once (1a parses; the 1c clock applies).
#[allow(dead_code)]
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
    S_FALSE
}

/// Device #79 twin (no ancillary block yet — 1c routes handles): `E_INVALIDARG`
/// without an interval, for type >= 3, for a nonzero type before the origin is
/// scheduled, or when unscheduling the origin; `STARTED_CLOCK` when this call
/// starts the clock, else `S_FALSE`.
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
    if started {
        STARTED_CLOCK
    } else {
        S_FALSE
    }
}

/// Ancillary handles of a frame event: honest stub (1c routes real handles).
/// Returns `S_FALSE` — nothing was signaled — and never fake-successes.
pub fn signal_ancillary(count: usize) -> HRESULT {
    if count > 0 {
        crate::xlog_debug!("frame clock: {count} ancillary handles not routed yet");
    }
    S_FALSE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock is one per process: the tests that drive it take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

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

    /// 1b failing-first: the schedule state machine validates like Windows
    /// (the 1c clock thread has no waiter yet, so pacing is not asserted here).
    #[test]
    fn schedule_validates_like_the_windows_clock() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
}
