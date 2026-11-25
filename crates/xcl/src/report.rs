//! `report.json`: what the title imported, which stubs it actually called, which console
//! paths were not mapped, which game files were redirected – the to-do list for the
//! next iteration.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::resolver::{self, Source};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct StubCall {
    pub dll: String,
    pub function: String,
    pub calls: u64,
    pub first_caller: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MissingImport {
    pub importer: String,
    pub dll: String,
    pub function: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ModuleInfo {
    pub name: String,
    pub base: String,
    pub size: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct VfsInfo {
    pub unmapped_paths: Vec<String>,
    pub redirected_writes: Vec<String>,
    pub whiteouts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Report {
    pub xcl_version: String,
    pub title: String,
    pub exe: String,
    pub crash: Option<String>,
    pub exit_code: Option<i64>,
    /// what the PC has of what a run needs (DXC, GPU features), checked by `xcl run` first
    #[cfg(windows)]
    #[serde(default)]
    pub environment: Vec<crate::preflight::Check>,
    /// counts of static imports by how they were resolved
    pub imports_by_source: BTreeMap<String, usize>,
    /// imports that ended up as auto-stubs (static analysis)
    pub stubbed_imports: Vec<MissingImport>,
    /// stubs the title actually called at run time (sorted by call count)
    pub called_stubs: Vec<StubCall>,
    pub modules: Vec<ModuleInfo>,
    pub vfs: VfsInfo,
    /// shader formats seen at pipeline creation (dxil / dxbc / container-unknown / rdna2-raw …)
    #[serde(default)]
    pub shaders: crate::gpu::shader::ShaderStats,
    /// the DXC that compiled them: folder and versions of dxcompiler.dll and dxil.dll (empty:
    /// none was loaded)
    #[cfg(windows)]
    #[serde(default)]
    pub dxc: crate::gpu::recomp::dxc::Toolchain,
    /// D3D12.X backend: adapter, interface layouts, unknown IIDs, fallbacks, presented frames
    #[cfg(windows)]
    #[serde(default)]
    pub d3d12: crate::hle::d3d12x::real::D3d12Stats,
    /// XAudio2: engines, mastering/source voices, XMA voices played as silence, dropped console flags
    #[cfg(windows)]
    #[serde(default)]
    pub audio: crate::hle::xaudio2::AudioStats,
    /// XGameRuntime through the GDK thunks: which classes the title asked for and who answered,
    /// XCL slots it called that XCL does not implement
    #[cfg(windows)]
    #[serde(default)]
    pub xgameruntime: crate::hle::xgameruntime::api::ApiStats,
    /// Same as above, Linux thunk-protocol slice (no PC runtime leg).
    #[cfg(not(windows))]
    #[serde(default)]
    pub xgameruntime: crate::hle::xgameruntime_api_linux::ApiStats,
    /// DirectStorage: BCPack requests unpacked and refused (no decoder) by mode, and bytes filled under
    /// `XCL_DSTORAGE_BCPACK_FILL`
    #[cfg(windows)]
    #[serde(default)]
    pub dstorage: crate::hle::dstorage::DStorageStats,
    pub notes: Vec<String>,
}

static CRASH: Mutex<Option<String>> = Mutex::new(None);
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn source_label(s: &Source) -> String {
    match s {
        Source::Hle(m) => format!("hle:{m}"),
        Source::HleOther(m) => format!("hle-cross:{m}"),
        Source::System(d) => format!("system:{d}"),
        Source::GameDll(d) => format!("game-dll:{d}"),
        Source::Stub => "stub".into(),
    }
}

fn import_stats() -> (BTreeMap<String, usize>, Vec<MissingImport>) {
    let records = resolver::records();
    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut stubbed = Vec::new();
    for r in &records {
        let label = match &r.source {
            Source::System(_) => "system".to_string(),
            other => source_label(other),
        };
        *by_source.entry(label).or_default() += 1;
        if r.source == Source::Stub {
            stubbed.push(MissingImport { importer: r.importer.clone(), dll: r.dll.clone(), function: r.function.clone() });
        }
    }
    (by_source, stubbed)
}

fn called_stubs() -> Vec<StubCall> {
    let mut called: Vec<StubCall> = crate::stubs::all_stubs()
        .into_iter()
        .filter(|s| s.calls.load(Ordering::Relaxed) > 0)
        .map(|s| StubCall {
            dll: s.dll.clone(),
            function: s.name.clone(),
            calls: s.calls.load(Ordering::Relaxed),
            first_caller: crate::modules::describe_address(s.first_caller.load(Ordering::Relaxed)),
        })
        .collect();
    called.sort_by(|a, b| b.calls.cmp(&a.calls));
    called
}

pub fn build() -> Report {
    let boot = crate::state::boot();
    let (imports_by_source, stubbed_imports) = import_stats();
    let modules = crate::modules::mapped_modules()
        .iter()
        .map(|m| ModuleInfo { name: m.name.clone(), base: format!("{:#x}", m.base), size: format!("{:#x}", m.size) })
        .collect();
    let vfs = crate::vfs::get()
        .map(|v| VfsInfo { unmapped_paths: v.unmapped_paths(), redirected_writes: v.redirected_writes(), whiteouts: v.whiteouts() })
        .unwrap_or_default();
    #[cfg(windows)]
    let notes = vec![
        "D3D12.X methods missing on PC show up in called_stubs as '<Interface>::#slot (not on PC)'.".into(),
        "Console-format shaders are replaced (fallback_pipelines) until the RDNA2 recompiler is finished.".into(),
        "Direct Nt* calls on files do not go through the VFS.".into(),
    ];
    #[cfg(not(windows))]
    let notes = vec!["Linux reports fatal signals; Windows .pdata/C++ unwinding is not ported yet.".into()];
    Report {
        xcl_version: env!("CARGO_PKG_VERSION").to_string(),
        title: boot.map(|b| b.title.clone()).unwrap_or_default(),
        exe: boot.map(|b| b.exe_path.to_string_lossy().into_owned()).unwrap_or_default(),
        crash: lock_within(&CRASH, Duration::ZERO).and_then(|g| g.clone()),
        exit_code: None,
        #[cfg(windows)]
        environment: Vec::new(),
        imports_by_source,
        stubbed_imports,
        called_stubs: called_stubs(),
        modules,
        vfs,
        shaders: crate::gpu::shader::stats(),
        #[cfg(windows)]
        dxc: crate::gpu::recomp::dxc::toolchain(),
        #[cfg(windows)]
        d3d12: crate::hle::d3d12x::real::stats(),
        #[cfg(windows)]
        audio: crate::hle::xaudio2::stats(),
        #[cfg(windows)]
        xgameruntime: crate::hle::xgameruntime::api::stats(),
        #[cfg(not(windows))]
        xgameruntime: crate::hle::xgameruntime_api_linux::stats(),
        #[cfg(windows)]
        dstorage: crate::hle::dstorage::stats(),
        notes,
    }
}

/// `m.try_lock()`, retried for `wait`: never an endless wait – the holder may be this very
/// thread, crashed inside `build()`. A poisoned lock is taken: the thread that held it panicked
/// (the very crash this report is about), and its data – the crash text, the report being
/// written – is what has to be read, not thrown away.
fn lock_within<T>(m: &Mutex<T>, wait: Duration) -> Option<MutexGuard<'_, T>> {
    let end = Instant::now() + wait;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(e)) => return Some(e.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}

thread_local! {
    /// Same-thread re-entrancy flag for the report writers below: a crash inside `build()`
    /// must not deadlock the retry on the write lock it already holds.
    static IN_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sets `IN_WRITE` for its lifetime, restoring the previous value on drop.
struct WriteGuard {
    outer: bool,
}

impl WriteGuard {
    fn enter() -> Self {
        let outer = IN_WRITE.with(|f| f.replace(true));
        Self { outer }
    }

    /// True when this thread is already inside a report write (it crashed midway).
    fn reentered(&self) -> bool {
        self.outer
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        IN_WRITE.with(|f| f.set(self.outer));
    }
}

/// Last-resort report: the crash description plus whatever the caller already knows.
/// Written when the full `build()` cannot run (contended locks, a panic midway) so a
/// crash is never silently lost.
fn minimal_report(title: String, exe: String, crash: Option<String>, code: Option<i64>) -> Report {
    let mut r = Report {
        xcl_version: env!("CARGO_PKG_VERSION").to_string(),
        title,
        exe,
        crash,
        exit_code: code,
        ..Default::default()
    };
    // A crash recorded while the full report was building must not be lost.
    if r.crash.is_none() {
        r.crash = lock_within(&CRASH, Duration::ZERO).and_then(|g| g.clone());
    }
    r
}

fn write_fallback(boot: &crate::config::Boot, crash: Option<String>, code: Option<i64>) {
    let r = minimal_report(
        boot.title.clone(),
        boot.exe_path.to_string_lossy().into_owned(),
        crash,
        code,
    );
    if let Ok(s) = serde_json::to_string_pretty(&r) {
        let _ = std::fs::write(boot.report_path(), s);
    }
}

/// Full crash report for the watchdog thread: everything `build()` collects
/// (called stubs, imports, modules, VFS) plus the crash description and the
/// signal's exit code. Runs off-signal (watchdog thread), so locks, allocation
/// and file I/O are fine here – unlike in the signal handler itself.
pub fn write_crash_with_code(crash: String, code: i64) {
    if let Some(mut g) = lock_within(&CRASH, Duration::from_secs(2)) {
        g.get_or_insert(crash.clone());
    }
    let guard = WriteGuard::enter();
    if guard.reentered() {
        // This thread crashed inside a report write: skip the locks and write the crash
        // description directly instead of deadlocking on them.
        if let Some(boot) = crate::state::boot() {
            write_fallback(&boot, Some(crash), Some(code));
        }
        return;
    }
    let Some(_g) = lock_within(&WRITE_LOCK, Duration::from_secs(2)) else {
        // Contention (the holder may be this very thread, crashed inside `build()`):
        // write the crash text now rather than losing the report.
        if let Some(boot) = crate::state::boot() {
            write_fallback(&boot, Some(crash), Some(code));
        }
        return;
    };
    let Some(boot) = crate::state::boot() else { return };
    let mut report = match std::panic::catch_unwind(build) {
        Ok(r) => r,
        Err(_) => {
            write_fallback(&boot, Some(crash), Some(code));
            return;
        }
    };
    // `build()` re-reads CRASH above, so the description is already in place.
    report.crash = Some(crash);
    report.exit_code = Some(code);
    // The boot record is the watchdog's only ground truth; `build()` fills the
    // same fields from the same source, but keep these pinned regardless.
    report.title = boot.title.clone();
    report.exe = boot.exe_path.to_string_lossy().into_owned();
    if let Ok(s) = serde_json::to_string_pretty(&report) {
        let _ = std::fs::write(boot.report_path(), s);
    }
}

/// Write a crash-only report from outside signal context. The Linux signal handler records
/// fixed-size values and wakes a watchdog thread; keeping allocation, locks and file I/O in
/// that thread avoids doing non-async-signal-safe work in the handler itself.
pub fn write_crash(crash: String) {
    write_crash_with_code(crash, 128 + libc::SIGSEGV as i64);
}

/// Write `report.json` (host side) with an explicit exit code. `ExitProcess` uses
/// this: it terminates via `std::process::exit`, so the launcher's post-return
/// `summarize` patch never runs for that path.
pub fn write_final_with_code(crash: Option<String>, code: i64) {
    #[cfg(windows)]
    crate::hle::kbm::release();
    let wait = if crash.is_some() { Duration::from_secs(2) } else { Duration::ZERO };
    if let Some(c) = &crash {
        if let Some(mut g) = lock_within(&CRASH, wait) {
            g.get_or_insert(c.clone());
        }
    }
    let guard = WriteGuard::enter();
    if guard.reentered() {
        if let Some(boot) = crate::state::boot() {
            write_fallback(&boot, crash, Some(code));
        }
        return;
    }
    let Some(_g) = lock_within(&WRITE_LOCK, wait) else {
        if let Some(boot) = crate::state::boot() {
            write_fallback(&boot, crash, Some(code));
        }
        return;
    };
    let Some(boot) = crate::state::boot() else { return };
    let mut r = match std::panic::catch_unwind(build) {
        Ok(r) => r,
        Err(_) => {
            write_fallback(&boot, crash, Some(code));
            return;
        }
    };
    // Late re-check: a crash recorded while `build()` was running is captured before exit.
    if r.crash.is_none() {
        r.crash = lock_within(&CRASH, Duration::ZERO).and_then(|g| g.clone());
    }
    r.exit_code = Some(code);
    if let Ok(s) = serde_json::to_string_pretty(&r) {
        let _ = std::fs::write(boot.report_path(), s);
    }
}

/// Write `report.json` (host side). `crash` is remembered for later writes.
pub fn write_final(crash: Option<String>) {
    // the process ends: a cursor held in the XCL window is let go
    #[cfg(windows)]
    crate::hle::kbm::release();
    // `XCL_DIRECT_DXIL_STATS=1`: the direct DXIL translator's live fallback summary
    crate::gpu::dxil::coverage::dump_fallbacks();
    // a crash is kept even while another thread writes the report, and is written once that
    // thread is done – the process ends right after (#106); other calls give way at once
    let wait = if crash.is_some() { Duration::from_secs(2) } else { Duration::ZERO };
    if let Some(c) = &crash {
        if let Some(mut g) = lock_within(&CRASH, wait) {
            g.get_or_insert(c.clone());
        }
    }
    let guard = WriteGuard::enter();
    if guard.reentered() {
        if let Some(boot) = crate::state::boot() {
            write_fallback(&boot, crash, None);
        }
        return;
    }
    let Some(_g) = lock_within(&WRITE_LOCK, wait) else {
        if let Some(boot) = crate::state::boot() {
            write_fallback(&boot, crash, None);
        }
        return;
    };
    let Some(boot) = crate::state::boot() else { return };
    let mut r = match std::panic::catch_unwind(build) {
        Ok(r) => r,
        Err(_) => {
            write_fallback(&boot, crash, None);
            return;
        }
    };
    // Late re-check: a crash recorded while `build()` was running is captured before exit.
    if r.crash.is_none() {
        r.crash = lock_within(&CRASH, Duration::ZERO).and_then(|g| g.clone());
    }
    if let Ok(s) = serde_json::to_string_pretty(&r) {
        let _ = std::fs::write(boot.report_path(), s);
    }
}

pub fn load(path: &std::path::Path) -> Option<Report> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread that panics inside `build()` (or while holding the crash text) poisons the lock.
    /// The report – and above all the crash – must still be written: the poisoned data is taken.
    #[test]
    fn a_poisoned_lock_is_taken_instead_of_losing_the_report() {
        let m = std::sync::Arc::new(Mutex::new(7u32));
        let other = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = other.lock().unwrap();
            panic!("the thread writing the report panicked");
        })
        .join();
        assert!(m.is_poisoned());
        let g = lock_within(&m, Duration::ZERO).expect("a poisoned lock is still taken");
        assert_eq!(*g, 7);
        drop(g);

        // A lock held by this very thread is not waited for forever (a report from a thread
        // that is already inside `build()` must not wedge the process at exit).
        let held = Mutex::new(1u32);
        let g = held.lock().unwrap();
        assert!(lock_within(&held, Duration::from_millis(20)).is_none());
        drop(g);
    }

    #[test]
    fn write_contention_falls_back_to_a_minimal_report() {
        let dir = std::env::temp_dir().join(format!(
            "xcl-report-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let boot = crate::config::Boot {
            title: "fallback-test".into(),
            exe_path: dir.join("fallback-test.exe"),
            game_root: dir.clone(),
            data_dir: dir.clone(),
            config: crate::config::Config::default(),
        };
        write_fallback(&boot, Some("fallback-boom".into()), Some(139));
        let raw = std::fs::read_to_string(boot.report_path()).unwrap();
        let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            report["crash"].as_str().is_some_and(|c| c.contains("fallback-boom")),
            "{raw}"
        );
        assert_eq!(report["exit_code"], serde_json::json!(139));
        assert_eq!(report["title"], serde_json::json!("fallback-test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A poisoned `WRITE_LOCK` (watchdog thread crashed midway through a previous write)
    /// still yields a full crash report through the watchdog's write path, in a child
    /// process (lock globals are per-process).
    #[cfg(target_os = "linux")]
    #[test]
    fn poisoned_write_lock_still_writes_the_crash_report() {
        const CHILD: &str = "XCL_REPORT_POISON_CHILD";
        const DIR_ENV: &str = "XCL_REPORT_POISON_DIR";
        if std::env::var_os(CHILD).is_some() {
            std::panic::set_hook(Box::new(|_| {}));
            let dir = std::path::PathBuf::from(std::env::var(DIR_ENV).unwrap());
            std::fs::create_dir_all(dir.join("logs")).unwrap();
            crate::state::set_boot(crate::config::Boot {
                title: "poison-test".into(),
                exe_path: dir.join("poison-test.exe"),
                game_root: dir.clone(),
                data_dir: dir.clone(),
                config: crate::config::Config::default(),
            });
            let _ = std::panic::catch_unwind(|| {
                let _held = WRITE_LOCK.lock().unwrap();
                panic!("poison the write lock");
            });
            crate::report::write_crash_with_code("poison-boom".into(), 139);
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "xcl-report-poison-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "report::tests::poisoned_write_lock_still_writes_the_crash_report",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(DIR_ENV, &dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let raw = std::fs::read_to_string(dir.join("logs").join("report.json")).unwrap();
        let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            report["crash"].as_str().is_some_and(|c| c.contains("poison-boom")),
            "{raw}"
        );
        assert_eq!(report["exit_code"], serde_json::json!(139));
        // The poisoned lock is taken, so the full report still lands, not just the fallback.
        assert!(report["called_stubs"].is_array(), "{raw}");
        assert!(report["imports_by_source"].is_object(), "{raw}");
        assert!(report["modules"].is_array(), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_is_kept_while_another_thread_writes_the_report() {
        let (held, wait_held) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let _g = WRITE_LOCK.lock().unwrap();
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        wait_held.recv().unwrap();
        // no boot record in unit tests: nothing is written to disk
        write_final(Some("boom".into()));
        writer.join().unwrap();
        assert_eq!(CRASH.lock().unwrap().as_deref(), Some("boom"));
        *CRASH.lock().unwrap() = None;
    }

    /// Synthetic SIGTRAP data (game exe + retail-style offset) goes through the
    /// watchdog's write path in a child process (boot/module globals are per-process)
    /// and the JSON must carry the crash site, the signal exit code and the stubs.
    #[cfg(target_os = "linux")]
    #[test]
    fn crash_report_carries_signal_module_offset_and_stubs() {
        const CHILD: &str = "XCL_REPORT_CRASH_CHILD";
        const DIR_ENV: &str = "XCL_REPORT_CRASH_DIR";
        if std::env::var_os(CHILD).is_some() {
            let dir = std::path::PathBuf::from(std::env::var(DIR_ENV).unwrap());
            std::fs::create_dir_all(dir.join("logs")).unwrap();
            crate::state::set_boot(crate::config::Boot {
                title: "signal-test".into(),
                exe_path: dir.join("signal-test.exe"),
                game_root: dir.clone(),
                data_dir: dir.clone(),
                config: crate::config::Config::default(),
            });
            let base = 0x6E5F_0000_0000usize;
            let offset = 0x16624a5usize;
            crate::modules::register_mapped(std::sync::Arc::new(crate::modules::MappedModule {
                name: "game_scarlett_master.exe".into(),
                path: dir.join("game_scarlett_master.exe"),
                base,
                size: 0x200_0000,
                entry_rva: 0,
                is_dll: false,
                exports: Default::default(),
                tls_callbacks: Default::default(),
                no_thread_calls: Default::default(),
                initialized: Default::default(),
                stack_reserve: 0,
            }));
            let crash = crate::eh::format_crash(libc::SIGTRAP, base + offset, base + offset);
            crate::report::write_crash_with_code(crash, 128 + libc::SIGTRAP as i64);
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "xcl-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "report::tests::crash_report_carries_signal_module_offset_and_stubs",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(DIR_ENV, &dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let raw = std::fs::read_to_string(dir.join("logs").join("report.json")).unwrap();
        let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let crash = report["crash"].as_str().expect("crash description");
        assert!(crash.contains("SIGTRAP"), "{crash}");
        assert!(crash.contains("signal 5"), "{crash}");
        assert!(crash.contains("game_scarlett_master.exe+0x16624a5"), "{crash}");
        assert_eq!(report["exit_code"], serde_json::json!(133));
        assert!(report["called_stubs"].is_array(), "{raw}");
        assert!(report["imports_by_source"].is_object(), "{raw}");
        assert!(
            report["modules"].as_array().is_some_and(|m| m
                .iter()
                .any(|m| m["name"] == "game_scarlett_master.exe")),
            "{raw}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
