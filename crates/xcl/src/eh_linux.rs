//! Linux fatal-signal reporting for manually mapped images.
//!
//! The `sigaction` handler only records fixed-size values and wakes a watchdog thread. The
//! watchdog formats the report and re-raises the original signal, so the signal handler does
//! not allocate, take Rust locks or perform file I/O. Windows `.pdata`/C++ unwinding is not
//! implemented here.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

const FATAL_SIGNALS: [libc::c_int; 6] =
    [libc::SIGSEGV, libc::SIGILL, libc::SIGFPE, libc::SIGBUS, libc::SIGTRAP, libc::SIGABRT];
static INSTALLED: AtomicBool = AtomicBool::new(false);
static HANDLING: AtomicBool = AtomicBool::new(false);
static CRASH_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CRASH_PC: AtomicUsize = AtomicUsize::new(0);
static CRASH_ADDRESS: AtomicUsize = AtomicUsize::new(0);
static CRASH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Set once a fatal signal has reached XCL's handler (read by the crash-deadline thread).
static CRASHED: AtomicBool = AtomicBool::new(false);

/// Seconds the process may take to end after a fatal signal before XCL ends it itself:
/// `XCL_CRASH_EXIT_SECONDS` (default 30; 0 turns it off). The title's own crash filter (a
/// crash report of a process with tens of GB committed) can take very long or never return,
/// which leaves a window that is "not responding" and holds all its memory.
fn crash_exit_seconds() -> u64 {
    std::env::var("XCL_CRASH_EXIT_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(30)
}

/// Waits until `crashed` is set, then gives the process `wait` to end by itself (`ending` set:
/// the terminate path has run). Calls `end` and returns true when it did not.
fn crash_deadline(
    crashed: &AtomicBool,
    ending: &AtomicBool,
    wait: std::time::Duration,
    poll: std::time::Duration,
    end: impl FnOnce(),
) -> bool {
    while !crashed.load(Ordering::Acquire) {
        std::thread::sleep(poll);
    }
    let until = std::time::Instant::now() + wait;
    while std::time::Instant::now() < until {
        if ending.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(poll);
    }
    if ending.load(Ordering::Acquire) {
        return false;
    }
    end();
    true
}

/// Ends the process through the raw `_exit` (not the hooked terminate path, whose thread dump
/// could hang the same way the title's crash filter did), with the access-violation status
/// the crash already reported.
fn end_process_now(status: i32) -> ! {
    unsafe { libc::_exit(status) }
}

/// Linux has no `RtlAddFunctionTable`; `.pdata` translation is a later Phase-C step.
// LINUX-TODO: Phase C — translate Windows unwind data before claiming C++ exception support.
pub fn register_function_table(_base: usize, _rva: u32, _size: u32) -> bool {
    crate::xlog_warn!(
        "RtlAddFunctionTable is unavailable on Linux; Windows unwind data is not registered"
    );
    false
}

/// `RtlPcToFileHeader` is a Windows loader hook and has no Linux equivalent.
// LINUX-TODO: Phase C — translate Windows C++ exception metadata before enabling this path.
pub fn install_pc_hook() -> Result<(), String> {
    let message = "RtlPcToFileHeader hook is not available on Linux".to_string();
    crate::xlog_warn!("{message}");
    Err(message)
}

/// Install fatal-signal handlers and the normal-context report watchdog.
pub fn install_crash_handler() {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        crate::xlog_warn!(
            "crash handler: pipe2 failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    CRASH_WRITE_FD.store(write_fd, Ordering::Release);
    if let Err(e) = std::thread::Builder::new()
        .name("xcl-crash-report".into())
        .spawn(move || crash_watchdog(read_fd))
    {
        crate::xlog_warn!("crash handler: cannot start report watchdog: {e}");
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        CRASH_WRITE_FD.store(-1, Ordering::Release);
        return;
    }
    for signal in FATAL_SIGNALS {
        if let Err(e) = install_signal(signal) {
            crate::xlog_warn!("crash handler: {e}");
        }
    }
    let secs = crash_exit_seconds();
    if secs > 0 {
        if let Err(e) = std::thread::Builder::new()
            .name("xcl-crash-deadline".into())
            .spawn(move || {
                crash_deadline(
                    &CRASHED,
                    &ENDING,
                    std::time::Duration::from_secs(secs),
                    std::time::Duration::from_millis(250),
                    || {
                        crate::xlog_error!(
                            "the process had not ended {secs} s after a fatal signal (the title's crash filter): ending it"
                        );
                        crate::log::flush();
                        end_process_now(0xC000_0005u32 as i32);
                    },
                );
            })
        {
            crate::xlog_warn!("crash deadline thread: {e}");
        }
    }
}

fn install_signal(signal: libc::c_int) -> Result<(), String> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = crash_handler as libc::sighandler_t;
    action.sa_flags = libc::SA_SIGINFO;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
        return Err(format!(
            "sigaction({signal}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn fault_pc(context: *mut std::ffi::c_void) -> usize {
    if context.is_null() {
        0
    } else {
        unsafe {
            (*(context as *const libc::ucontext_t)).uc_mcontext.gregs[libc::REG_RIP as usize]
                as usize
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn fault_pc(_context: *mut std::ffi::c_void) -> usize {
    0
}

/// Crash description from synthetic or observed signal data: signal name and number,
/// faulting PC with its nearest module+offset, and the fault address. When the fault
/// is a `SIGSEGV` at address 0 through the title's own `mov dword ptr [0], imm32`
/// exit instruction, the line names the deliberate crash and its tag. Reads guest
/// code through the lock-free module ranges only (never blocks the crash watchdog),
/// so unit tests can check the report contents directly.
pub fn format_crash(signal: libc::c_int, pc: usize, address: usize) -> String {
    let mut crash = format!(
        "{} (signal {signal}) at {}; pc {pc:#x}; fault address {address:#x}",
        signal_name(signal),
        crate::modules::describe_address(pc)
    );
    // A title's own way to end itself: a write of a tag to address 0 (a retail game's
    // GPU watchdog, "GPU Loc Sig", writes 0x4d8bd421; its thread's exit path writes 0).
    // The faulting instruction itself proves the write, so no signal payload beyond
    // SIGSEGV-at-null is needed on Linux.
    if signal == libc::SIGSEGV && address == 0 {
        if let Some(tag) = deliberate_tag_at_pc(pc) {
            crash += &format!(" – the title's own deliberate crash: mov dword ptr [0], {tag:#x}");
        }
    }
    crash
}

/// The tag of `mov dword ptr [0], imm32` (C7 04 25 00 00 00 00 imm32), the instruction titles
/// use to end themselves on purpose.
fn deliberate_null_write(code: &[u8]) -> Option<u32> {
    (code.len() >= 11 && code[..7] == [0xC7, 0x04, 0x25, 0, 0, 0, 0])
        .then(|| u32::from_le_bytes([code[7], code[8], code[9], code[10]]))
}

/// The deliberate-crash tag when `pc` points at that instruction inside a mapped image
/// (`None` when `pc` is in no mapped image, when the 11 bytes would run past the image,
/// or when the bytes are a different store). Lock-free: safe from the crash watchdog
/// even if the faulting thread died holding the module-table lock.
fn deliberate_tag_at_pc(pc: usize) -> Option<u32> {
    let (base, size) = crate::modules::image_range_for_addr(pc)?;
    if pc.checked_add(11)? > base + size {
        return None;
    }
    // The 11 bytes lie inside an image the loader mapped, and the fault itself is a
    // data access to address 0, so the code page is still resident.
    let code = unsafe { std::slice::from_raw_parts(pc as *const u8, 11) };
    deliberate_null_write(code)
}

fn signal_name(signal: libc::c_int) -> &'static str {
    match signal {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        libc::SIGBUS => "SIGBUS",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        _ => "fatal signal",
    }
}

unsafe extern "C" fn crash_handler(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut std::ffi::c_void,
) {
    CRASHED.store(true, Ordering::Release);
    if HANDLING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        terminate_with_signal(signal);
    }
    CRASH_SIGNAL.store(signal, Ordering::Release);
    CRASH_PC.store(fault_pc(context), Ordering::Release);
    CRASH_ADDRESS.store(
        if info.is_null() {
            0
        } else {
            (*info).si_addr() as usize
        },
        Ordering::Release,
    );

    let fd = CRASH_WRITE_FD.load(Ordering::Acquire);
    if fd < 0 {
        terminate_with_signal(signal);
    }
    let byte = 1u8;
    loop {
        match unsafe { libc::write(fd, &byte as *const u8 as *const std::ffi::c_void, 1) } {
            1 => break,
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue
            }
            _ => terminate_with_signal(signal),
        }
    }
    // The faulting instruction remains blocked while the watchdog writes the report. Any
    // unrelated signal must not return us to the fault before that write has completed.
    loop {
        unsafe { libc::pause() };
    }
}

fn crash_watchdog(read_fd: libc::c_int) {
    let mut byte = 0u8;
    loop {
        match unsafe { libc::read(read_fd, &mut byte as *mut u8 as *mut std::ffi::c_void, 1) } {
            1 => break,
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue
            }
            _ => return,
        }
    }
    let signal = CRASH_SIGNAL.load(Ordering::Acquire);
    let pc = CRASH_PC.load(Ordering::Acquire);
    let address = CRASH_ADDRESS.load(Ordering::Acquire);
    // Shell exit-code convention for fatal signals, so the report carries exit_code too.
    crate::report::write_crash_with_code(format_crash(signal, pc, address), 128 + signal as i64);
    terminate_with_signal(signal);
}

fn terminate_with_signal(signal: libc::c_int) -> ! {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    unsafe {
        libc::sigaction(signal, &action, std::ptr::null_mut());
        libc::raise(signal);
        libc::_exit(128 + signal);
    }
}

pub(crate) static ENDING: AtomicBool = AtomicBool::new(false);

pub(crate) fn begin_termination(
    h: isize,
    ending: &AtomicBool,
    wine: impl FnOnce() -> bool,
    write_report: impl FnOnce(),
) -> bool {
    if (h != 0 && h != -1) || ending.swap(true, Ordering::AcqRel) {
        return false;
    }
    // Wine does not deliver the executable's process-detach TLS callback.
    if wine() {
        write_report();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[test]
    fn crash_deadline_ends_a_process_that_does_not_end_itself() {
        let (crashed, ending) = (AtomicBool::new(true), AtomicBool::new(false));
        let mut called = false;
        let did = crash_deadline(&crashed, &ending, Duration::from_millis(80), Duration::from_millis(5), || called = true);
        assert!(did && called, "the process had not ended within the wait: `end` runs");
    }

    #[test]
    fn crash_deadline_leaves_a_process_that_ends_in_time() {
        let (crashed, ending) = (AtomicBool::new(true), AtomicBool::new(false));
        let started = std::time::Instant::now();
        let mut called = false;
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                ending.store(true, Ordering::Release);
            });
            let did = crash_deadline(&crashed, &ending, Duration::from_secs(5), Duration::from_millis(5), || called = true);
            assert!(!did && !called, "the process ended by itself: `end` does not run");
        });
        assert!(started.elapsed() < Duration::from_secs(2), "it returned as soon as the process was ending");
    }

    #[test]
    fn crash_deadline_waits_for_a_crash() {
        let (crashed, ending) = (AtomicBool::new(false), AtomicBool::new(false));
        let mut called = false;
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(60));
                crashed.store(true, Ordering::Release);
            });
            let started = std::time::Instant::now();
            let did = crash_deadline(&crashed, &ending, Duration::from_millis(40), Duration::from_millis(5), || called = true);
            assert!(did && called);
            assert!(started.elapsed() >= Duration::from_millis(95), "the wait starts at the crash, not before");
        });
    }

    std::arch::global_asm!(
        ".pushsection .text",
        ".global xcl_eh_test_fault",
        "xcl_eh_test_fault:",
        "movabs rax, 0x1000",
        "mov al, [rax]",
        "ud2",
        ".popsection",
    );
    extern "C" {
        fn xcl_eh_test_fault();
    }

    #[test]
    fn a_write_of_a_tag_to_address_0_is_named_a_deliberate_crash() {
        // the two of a retail game's GPU watchdog: its tag, and 0 on its thread's exit path
        let watchdog = [
            0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D, 0x48, 0x83,
        ];
        assert_eq!(deliberate_null_write(&watchdog), Some(0x4D8B_D421));
        assert_eq!(
            deliberate_null_write(&[0xC7, 0x04, 0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0xCC]),
            Some(0)
        );
        let mut elsewhere = watchdog;
        elsewhere[3] = 0x10; // mov dword ptr [0x10], imm32: an ordinary store
        assert_eq!(deliberate_null_write(&elsewhere), None);
        assert_eq!(
            deliberate_null_write(&[0x89, 0x04, 0x25, 0, 0, 0, 0]),
            None,
            "a register store"
        );
        assert_eq!(deliberate_null_write(&watchdog[..10]), None, "cut short");
    }

    #[test]
    fn sigsegv_at_null_names_the_deliberate_crash_only_for_that_store() {
        static CODE: [u8; 13] = [
            0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D, 0xCC, 0xCC,
        ];
        let pc = CODE.as_ptr() as usize;
        crate::modules::register_mapped(std::sync::Arc::new(crate::modules::MappedModule {
            name: "deliberate-test.exe".into(),
            path: std::path::PathBuf::from("deliberate-test.exe"),
            base: pc,
            size: CODE.len(),
            entry_rva: 0,
            is_dll: false,
            exports: Default::default(),
            tls_callbacks: Default::default(),
            no_thread_calls: Default::default(),
            initialized: Default::default(),
            stack_reserve: 0,
        }));
        let crash = format_crash(libc::SIGSEGV, pc, 0);
        assert!(
            crash.contains("the title's own deliberate crash: mov dword ptr [0], 0x4d8bd421"),
            "{crash}"
        );
        // Gates that must stay silent while the image is still mapped.
        let read = format_crash(libc::SIGSEGV, pc, 0x1000);
        assert!(!read.contains("deliberate crash"), "{read}");
        let trap = format_crash(libc::SIGTRAP, pc, 0);
        assert!(!trap.contains("deliberate crash"), "{trap}");
        let past_end = format_crash(libc::SIGSEGV, pc + 5, 0);
        assert!(!past_end.contains("deliberate crash"), "{past_end}");
        crate::modules::unregister_mapped(pc);
        // A faulting PC in no mapped image names nothing (and reads nothing).
        let unmapped = format_crash(libc::SIGSEGV, pc, 0);
        assert!(!unmapped.contains("deliberate crash"), "{unmapped}");
    }

    #[test]
    fn pc_hook_is_an_explicit_unsupported_path() {
        let error = install_pc_hook().unwrap_err();
        assert!(error.contains("not available on Linux"), "{error}");
    }

    #[test]
    fn termination_reports_once_for_the_current_process_under_wine() {
        use std::sync::atomic::AtomicBool;
        let ending = AtomicBool::new(false);
        let writes = std::cell::Cell::new(0);
        let report = || writes.set(writes.get() + 1);
        assert!(!begin_termination(42, &ending, || panic!("other process"), report));
        assert!(begin_termination(-1, &ending, || true, report));
        assert!(!begin_termination(0, &ending, || true, report));
        assert_eq!(writes.get(), 1);
        let windows = AtomicBool::new(false);
        assert!(begin_termination(0, &windows, || false, report));
        assert_eq!(writes.get(), 1);
    }

    #[test]
    fn trap_and_abort_are_fatal_signals() {
        assert!(FATAL_SIGNALS.contains(&libc::SIGTRAP));
        assert!(FATAL_SIGNALS.contains(&libc::SIGABRT));
        assert_eq!(signal_name(libc::SIGTRAP), "SIGTRAP");
        assert_eq!(signal_name(libc::SIGABRT), "SIGABRT");
    }

    #[test]
    fn sigtrap_formats_with_module_offset() {
        // Synthetic SIGTRAP data shaped like the retail trap (game exe + offset).
        let base = 0x6E5E_0000_0000usize;
        let offset = 0x16624a5usize;
        crate::modules::register_mapped(std::sync::Arc::new(crate::modules::MappedModule {
            name: "game_scarlett_master.exe".into(),
            path: std::path::PathBuf::from("game_scarlett_master.exe"),
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
        let crash = format_crash(libc::SIGTRAP, base + offset, base + offset);
        crate::modules::unregister_mapped(base);
        assert!(crash.contains("SIGTRAP"), "{crash}");
        assert!(crash.contains("signal 5"), "{crash}");
        assert!(crash.contains("game_scarlett_master.exe+0x16624a5"), "{crash}");
        assert!(crash.contains(&format!("pc {:#x}", base + offset)), "{crash}");
    }

    #[test]
    fn signal_crash_writes_report_and_terminates_child() {
        let dir = std::env::temp_dir().join(format!(
            "xcl-eh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        if std::env::var_os("XCL_EH_CRASH_CHILD").is_some() {
            let dir = std::path::PathBuf::from(std::env::var_os("XCL_EH_CRASH_DIR").unwrap());
            std::fs::create_dir_all(dir.join("logs")).unwrap();
            crate::state::set_boot(crate::config::Boot {
                title: "signal-test".into(),
                exe_path: dir.join("signal-test.exe"),
                game_root: dir.clone(),
                data_dir: dir.clone(),
                config: crate::config::Config::default(),
            });
            let fault = xcl_eh_test_fault as usize;
            let pc = fault + 10; // the `mov al, [rax]` after the 10-byte movabs
            let base = fault - 0x20;
            crate::modules::register_mapped(std::sync::Arc::new(crate::modules::MappedModule {
                name: "signal-test.exe".into(),
                path: dir.join("signal-test.exe"),
                base,
                size: 0x1000,
                entry_rva: 0,
                is_dll: false,
                exports: Default::default(),
                tls_callbacks: Default::default(),
                no_thread_calls: Default::default(),
                initialized: Default::default(),
                stack_reserve: 0,
            }));
            std::fs::write(dir.join("pc"), format!("{pc:#x}")).unwrap();
            install_crash_handler();
            for signal in FATAL_SIGNALS {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) },
                    0
                );
                assert_eq!(action.sa_sigaction, crash_handler as libc::sighandler_t);
                assert_ne!(action.sa_flags & libc::SA_SIGINFO, 0);
            }
            unsafe { xcl_eh_test_fault() };
            std::process::exit(99);
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "eh::tests::signal_crash_writes_report_and_terminates_child",
                "--nocapture",
            ])
            .env("XCL_EH_CRASH_CHILD", "1")
            .env("XCL_EH_CRASH_DIR", &dir)
            .output()
            .unwrap();
        let report_path = dir.join("logs/report.json");
        let report = std::fs::read_to_string(&report_path).unwrap_or_else(|e| {
            panic!(
                "child did not write {}: {e}\nstdout:\n{}\nstderr:\n{}",
                report_path.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        let report: serde_json::Value = serde_json::from_str(&report).unwrap();
        let pc = std::fs::read_to_string(dir.join("pc")).unwrap();
        let crash = report["crash"].as_str().expect("crash description");
        assert!(crash.contains("SIGSEGV"), "{crash}");
        assert!(crash.contains(&format!("pc {pc}")), "{crash}");
        assert!(crash.contains("fault address 0x1000"), "{crash}");
        assert!(crash.contains("signal-test.exe+"), "{crash}");
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGSEGV),
            "child status: {}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
