//! Where the process's memory goes: one line for the log – at a crash, when the process ends
//! with an error, and while the title runs whenever its commit charge has grown by a GiB
//! (at most every 30 s). Linux twin of upstream `memstat.rs` (#194): same `GRAPHICS` /
//! `WINDOWS` / `HEAP` / `PHYSICAL` kinds and gross `note()` counters (frees are not taken
//! off), same `line()` / `log()` / `start()` shape.
//!
//! Linux adaptations (documented deviations, not omissions):
//! * `windows` / `windows-sys` only upstream: `process()` reads `/proc/self/status`
//!   (`VmData` commit via [`crate::platform::process_private_bytes`] + `VmRSS` working
//!   set); `system()` reads `/proc/meminfo` (RAM + swap as the commit limit).
//! * `video()` is DEFERRED: no Vulkan video-memory query is wired yet (the keeper allows
//!   "video side via Vulkan queries later"). The line omits the segment when unavailable.
//! * sparse windows live in the Windows `d3d12x` backend only; the Linux null backend has
//!   no sparse windows, so the line omits that segment too (same `None`-omits shape as
//!   upstream's `sparse::stats()`).

use std::sync::atomic::{AtomicU64, Ordering};

/// Kinds of memory the title asks kernelx for (`note`).
pub const GRAPHICS: usize = 0;
pub const WINDOWS: usize = 1;
pub const HEAP: usize = 2;
pub const PHYSICAL: usize = 3;

/// Bytes the title asked for since the start, by kind (gross: frees are not taken off).
static TITLE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The title committed `bytes` of `kind` (`GRAPHICS`: memory that gets video memory;
/// `WINDOWS`: an ordinary `VirtualAlloc` commit; `HEAP`: `XMemAlloc`; `PHYSICAL`: the
/// homes of its physical pages).
pub fn note(kind: usize, bytes: usize) {
    if kind < TITLE.len() {
        TITLE[kind].fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Bytes noted so far for `kind` (tests only: pins the `note()` wiring without
/// parsing the rendered line; other tests may note concurrently, so callers assert
/// growth of *at least* their allocation).
#[cfg(test)]
pub fn gross(kind: usize) -> u64 {
    TITLE.get(kind).map(|a| a.load(Ordering::Relaxed)).unwrap_or(0)
}

fn gib(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
}

/// Parse a `kB` field from `/proc/self/status` or `/proc/meminfo` text.
fn parse_kib(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        let mut fields = value.split_whitespace();
        let kib = fields.next()?.parse::<u64>().ok()?;
        if let Some(unit) = fields.next() {
            if !unit.eq_ignore_ascii_case("kB") {
                return None;
            }
        }
        Some(kib)
    })
}

/// (commit charge, working set) of the process. Commit reuses the platform counter
/// (`VmData`, the closest analogue to `PrivateUsage`); the working set is `VmRSS`.
fn process() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let commit = crate::platform::process_private_bytes();
    let ws = parse_kib(&status, "VmRSS")?.saturating_mul(1024);
    (commit != 0 || ws != 0).then_some((commit, ws))
}

/// (commit in use, commit limit, physical memory free, load %) of the system, from
/// `/proc/meminfo`: the limit is RAM + swap (the analogue of `ullTotalPageFile`), the
/// free figure is available RAM, and use is the limit minus all available (RAM + swap).
fn system() -> Option<(u64, u64, u64, u32)> {
    let mem = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total = parse_kib(&mem, "MemTotal")?;
    let avail = parse_kib(&mem, "MemAvailable").or_else(|| parse_kib(&mem, "MemFree"))?;
    let swap_total = parse_kib(&mem, "SwapTotal").unwrap_or(0);
    let swap_free = parse_kib(&mem, "SwapFree").unwrap_or(0);
    let limit = total.saturating_add(swap_total).saturating_mul(1024);
    if limit == 0 {
        return None;
    }
    let free = avail.saturating_mul(1024);
    let avail_total = avail.saturating_add(swap_free).saturating_mul(1024);
    let used = limit.saturating_sub(avail_total);
    let load = (used / (limit / 100).max(1)).min(100) as u32;
    Some((used, limit, free, load))
}

/// ((in use, budget) local video memory, (in use, budget) non-local) – DEFERRED on
/// Linux (no Vulkan video-memory query wired yet): always `None`, so the line omits
/// the segment, exactly as upstream omits it when the adapter is unavailable.
fn video() -> Option<((u64, u64), (u64, u64))> {
    None
}

/// (tiles of title memory with video memory, pool heaps holding them) – DEFERRED on
/// Linux (the null `d3d12x` backend has no sparse windows): always `None`, so the
/// line omits the segment, exactly as upstream omits it with no sparse windows.
fn sparse_windows() -> Option<(usize, usize)> {
    None
}

/// The line: `memory: process commit 61.2 GiB (working set 24.1 GiB); system commit 74.9 of
/// 79.5 GiB, 5.1 GiB of RAM free (84 % load); the title asked for graphics 20.1 GiB,
/// Windows 1.2 GiB, heaps 3.4 GiB, physical pages 0.0 GiB` (plus `video …` / `sparse
/// windows …` segments when their backends land).
pub fn line() -> String {
    let mut parts = Vec::new();
    if let Some((commit, ws)) = process() {
        parts.push(format!("process commit {} (working set {})", gib(commit), gib(ws)));
    }
    if let Some((used, limit, free, load)) = system() {
        parts.push(format!(
            "system commit {} of {}, {} of RAM free ({load} % load)",
            gib(used),
            gib(limit),
            gib(free)
        ));
    }
    if let Some(((lu, lb), (su, sb))) = video() {
        parts.push(format!("video {} of {} local, {} of {} shared", gib(lu), gib(lb), gib(su), gib(sb)));
    }
    let t = TITLE.each_ref().map(|a| a.load(Ordering::Relaxed));
    parts.push(format!(
        "the title asked for graphics {}, Windows {}, heaps {}, physical pages {}",
        gib(t[GRAPHICS]),
        gib(t[WINDOWS]),
        gib(t[HEAP]),
        gib(t[PHYSICAL])
    ));
    if let Some((tiles, pools)) = sparse_windows() {
        parts.push(format!("sparse windows {tiles} tiles ({}) in {pools} pools", gib((tiles as u64) << 16)));
    }
    format!("memory: {}", parts.join("; "))
}

/// The log line now (a crash, the process ending with an error).
pub fn log() {
    crate::xlog_info!("{}", line());
}

/// A thread that logs the line whenever the process's commit charge has grown by a GiB since
/// the last line, looking every 30 s.
pub fn start() {
    let _ = std::thread::Builder::new().name("xcl-memstat".into()).spawn(|| {
        let mut last = 0u64;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let Some((commit, _)) = process() else { continue };
            if commit >= last + (1 << 30) {
                last = commit;
                log();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gib_formats_to_one_decimal_gib() {
        assert_eq!(gib(0), "0.0 GiB");
        assert_eq!(gib(1 << 30), "1.0 GiB");
        assert_eq!(gib((61u64 << 30) + (1 << 29)), "61.5 GiB");
    }

    #[test]
    fn status_kib_parses_vm_fields_and_rejects_units() {
        let status = "Name:\txcl\nVmData:\t   2048 kB\nVmRSS:\t    1024 kB\n";
        assert_eq!(parse_kib(status, "VmData"), Some(2048));
        assert_eq!(parse_kib(status, "VmRSS"), Some(1024));
        assert_eq!(parse_kib("Name:\txcl\n", "VmData"), None);
        assert_eq!(parse_kib("VmData:\t 2048 MB\n", "VmData"), None);
        assert_eq!(parse_kib("VmData:\t lots kB\n", "VmData"), None);
    }

    #[test]
    fn meminfo_parses_totals_and_swap() {
        let mem = "MemTotal:        8192 kB\nMemAvailable:    4096 kB\nSwapTotal:       8192 kB\nSwapFree:        8192 kB\n";
        assert_eq!(parse_kib(mem, "MemTotal"), Some(8192));
        assert_eq!(parse_kib(mem, "SwapFree"), Some(8192));
    }

    #[test]
    fn process_and_system_read_live_proc() {
        // Live `/proc`: both must parse on any Linux host (values are nonzero here).
        let (commit, ws) = process().expect("process() reads /proc/self/status");
        assert!(ws > 0, "working set: {ws}");
        let _ = commit;
        let (used, limit, free, load) = system().expect("system() reads /proc/meminfo");
        assert!(limit > 0 && free > 0, "limit {limit} free {free}");
        assert!(used <= limit, "used {used} limit {limit}");
        assert!(load <= 100, "load {load}");
    }

    #[test]
    fn note_counters_are_gross_per_kind() {
        let before: [u64; 4] = TITLE.each_ref().map(|a| a.load(Ordering::Relaxed));
        // Out-of-range kinds never panic (a caller bug must not crash the title).
        note(99, 1 << 30);
        note(GRAPHICS, 1 << 30);
        note(WINDOWS, 1 << 20);
        note(HEAP, 1 << 20);
        note(PHYSICAL, 1 << 20);
        let after: [u64; 4] = TITLE.each_ref().map(|a| a.load(Ordering::Relaxed));
        assert_eq!(after[GRAPHICS] - before[GRAPHICS], 1 << 30);
        assert_eq!(after[WINDOWS] - before[WINDOWS], 1 << 20);
        assert_eq!(after[HEAP] - before[HEAP], 1 << 20);
        assert_eq!(after[PHYSICAL] - before[PHYSICAL], 1 << 20);
        // Restore so the gross counters do not leak GiBs into other tests' lines.
        TITLE[GRAPHICS].fetch_sub(1 << 30, Ordering::Relaxed);
        TITLE[WINDOWS].fetch_sub(1 << 20, Ordering::Relaxed);
        TITLE[HEAP].fetch_sub(1 << 20, Ordering::Relaxed);
        TITLE[PHYSICAL].fetch_sub(1 << 20, Ordering::Relaxed);
    }

    #[test]
    fn line_names_every_title_kind() {
        let l = line();
        assert!(l.starts_with("memory: "), "{l}");
        assert!(l.contains("the title asked for graphics "), "{l}");
        assert!(l.contains("Windows "), "{l}");
        assert!(l.contains("heaps "), "{l}");
        assert!(l.contains("physical pages "), "{l}");
    }
}
