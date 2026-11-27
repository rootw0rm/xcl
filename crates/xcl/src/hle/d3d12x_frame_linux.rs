//! Linux twin of the console frame clock (`hle/d3d12x/frame.rs` is Windows-only).
//! Split 1a: the pure core — interval helpers plus token issuance and flip statistics.
//! Split 1b adds the schedule state machine, the `Condvar`/monotonic clock thread with
//! `wait_frame_event`, and the `signal_ancillary` stub (the `kernelx_linux::SetEvent`
//! compat point); device slots #78–#81 and `PresentX` stay unrouted until then.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::util::{E_FAIL, HRESULT};

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
}
