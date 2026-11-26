//! Process-wide logger. The file handle is opened once at startup (before any file
//! hooks are installed) so logging never re-enters the VFS.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn parse(s: &str) -> Level {
        match s.to_ascii_lowercase().as_str() {
            "error" => Level::Error,
            "warn" | "warning" => Level::Warn,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => Level::Info,
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

static FILE: Mutex<Option<File>> = Mutex::new(None);
static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static ECHO: AtomicBool = AtomicBool::new(true);
static WRITE_FAILED: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

fn report_write_result(
    reported: &AtomicBool,
    result: io::Result<()>,
    report: impl FnOnce(&io::Error),
) {
    if let Err(error) = result {
        if !reported.swap(true, Ordering::Relaxed) {
            report(&error);
        }
    }
}

fn report_log_write_error(error: &io::Error) {
    let _ = writeln!(std::io::stderr().lock(), "xcl: cannot write to the log: {error}");
}

fn flush_log_file_with(
    file: &mut impl Write,
    reported: &AtomicBool,
    report: impl FnOnce(&io::Error),
) {
    report_write_result(reported, file.flush(), report);
}

fn write_log_entry_with(
    file: &mut impl Write,
    line: &[u8],
    flush: bool,
    reported: &AtomicBool,
    mut report: impl FnMut(&io::Error),
) {
    report_write_result(reported, file.write_all(line), |error| report(error));
    if flush {
        flush_log_file_with(file, reported, |error| report(error));
    }
}

fn write_log_entry(file: &mut impl Write, line: &[u8], flush: bool) {
    write_log_entry_with(file, line, flush, &WRITE_FAILED, report_log_write_error);
}

fn flush_log_file(file: &mut impl Write) {
    flush_log_file_with(file, &WRITE_FAILED, report_log_write_error);
}

/// Seconds since the log started (the host's start) – the times log lines carry.
pub fn uptime() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub fn init(path: Option<&Path>, level: Level, echo_stderr: bool) {
    START.get_or_init(Instant::now);
    LEVEL.store(level as u8, Ordering::Relaxed);
    ECHO.store(echo_stderr, Ordering::Relaxed);
    if let Some(p) = path {
        // a log that cannot be opened is said once on stderr: `xlog_*` would go nowhere, and the
        // console has already been given its path (#2277); the run goes on without it
        match open(p) {
            Ok(f) => {
                if let Ok(mut g) = FILE.lock() {
                    *g = Some(f);
                }
            }
            Err(e) => eprintln!("xcl: {e}"),
        }
    }
}

/// The log file at `p`, its folder made first; the error names the path and the OS error.
fn open(p: &Path) -> Result<File, String> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create the log folder {}: {e}", dir.display()))?;
    }
    File::create(p).map_err(|e| format!("cannot open the log {}: {e}", p.display()))
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

#[cfg(windows)]
fn thread_id() -> u32 {
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

/// Linux: std exposes no numeric TID, and `ThreadId` is stable and distinct per thread.
// LINUX-TODO: Phase E — use the kernel tid once the kernelx thread shim owns thread creation.
#[cfg(not(windows))]
fn thread_id() -> String {
    format!("{:?}", std::thread::current().id())
}

pub fn write(level: Level, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let tid = thread_id();
    let line = format!("[{t:9.3}][{tid:5}][{}] {args}\n", level.tag());
    if let Ok(mut g) = FILE.try_lock().or_else(|_| FILE.lock()) {
        if let Some(f) = g.as_mut() {
            write_log_entry(f, line.as_bytes(), level <= Level::Warn);
        }
    }
    if ECHO.load(Ordering::Relaxed) && level <= Level::Info {
        let _ = std::io::stderr().write_all(format!("xcl: {args}\n").as_bytes());
    }
}

pub fn flush() {
    if let Ok(mut g) = FILE.lock() {
        if let Some(f) = g.as_mut() {
            flush_log_file(f);
        }
    }
}

/// A panic goes to the log as well as to stderr.
pub fn log_panics() {
    let print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write(Level::Error, format_args!("thread '{}' {}", std::thread::current().name().unwrap_or(""), info.to_string().replace('\n', " ")));
        print(info);
    }));
}

#[macro_export]
macro_rules! xlog_error { ($($a:tt)*) => { $crate::log::write($crate::log::Level::Error, format_args!($($a)*)) } }
#[macro_export]
macro_rules! xlog_warn { ($($a:tt)*) => { $crate::log::write($crate::log::Level::Warn, format_args!($($a)*)) } }
#[macro_export]
macro_rules! xlog_info { ($($a:tt)*) => { $crate::log::write($crate::log::Level::Info, format_args!($($a)*)) } }
#[macro_export]
macro_rules! xlog_debug { ($($a:tt)*) => { if $crate::log::enabled($crate::log::Level::Debug) { $crate::log::write($crate::log::Level::Debug, format_args!($($a)*)) } } }
#[macro_export]
macro_rules! xlog_trace { ($($a:tt)*) => { if $crate::log::enabled($crate::log::Level::Trace) { $crate::log::write($crate::log::Level::Trace, format_args!($($a)*)) } } }

#[cfg(test)]
mod tests {
    use super::*;

    struct FaultWriter {
        fail_write: bool,
        fail_flush: bool,
        writes: usize,
        flushes: usize,
    }

    impl Write for FaultWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.fail_write {
                Err(io::Error::other("write failed"))
            } else {
                Ok(bytes.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn log_write_failures_are_reported_once() {
        let reported = AtomicBool::new(false);
        let mut messages = Vec::new();
        let mut file = FaultWriter { fail_write: true, fail_flush: true, writes: 0, flushes: 0 };
        write_log_entry_with(
            &mut file,
            b"log entry",
            true,
            &reported,
            |error| messages.push(error.to_string()),
        );

        assert_eq!(file.writes, 1);
        assert_eq!(file.flushes, 1, "flush is still attempted after a write failure");
        assert_eq!(messages, vec![String::from("write failed")]);
    }

    #[test]
    fn a_log_flush_failure_is_reported() {
        let reported = AtomicBool::new(false);
        let mut messages = Vec::new();
        let mut file = FaultWriter { fail_write: false, fail_flush: true, writes: 0, flushes: 0 };

        flush_log_file_with(&mut file, &reported, |error| messages.push(error.to_string()));

        assert_eq!(file.flushes, 1);
        assert_eq!(messages, vec![String::from("flush failed")]);
    }

    #[cfg(not(windows))]
    #[test]
    fn thread_id_is_stable_on_a_thread_and_distinct_across_threads() {
        let here = super::thread_id();
        assert_eq!(here, super::thread_id());
        let there = std::thread::spawn(super::thread_id).join().unwrap();
        assert_ne!(here, there);
    }

    #[test]
    fn a_panic_is_in_the_log() {
        let path = std::env::temp_dir().join(format!("xcl-log-panics-{}.log", std::process::id()));
        *FILE.lock().unwrap() = Some(File::create(&path).unwrap());
        log_panics();
        let died = std::thread::Builder::new().name("xcl-dies".into()).spawn(|| panic!("gone for the run")).unwrap().join();
        *FILE.lock().unwrap() = None;
        let log = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(died.is_err());
        assert!(log.contains("thread 'xcl-dies' panicked at ") && log.contains("gone for the run"), "the log: {log:?}");
    }

    #[test]
    fn a_log_that_cannot_be_opened_names_its_path() {
        let dir = std::env::temp_dir().join(format!("xcl-log-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // a file where the log folder should be: neither the folder nor the log can be made
        let blocker = dir.join("not-a-folder");
        std::fs::write(&blocker, "x").unwrap();
        let bad = blocker.join("logs").join("xcl.log");
        let e = open(&bad).unwrap_err();
        assert!(e.starts_with("cannot create the log folder ") && e.contains("not-a-folder"), "{e}");
        let good = dir.join("logs").join("xcl.log");
        assert!(open(&good).is_ok());
        assert!(good.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
