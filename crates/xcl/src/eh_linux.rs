//! Linux fatal-signal reporting for manually mapped images.
//!
//! The `sigaction` handler only records fixed-size values and wakes a watchdog thread. The
//! watchdog formats the report and re-raises the original signal, so the signal handler does
//! not allocate, take Rust locks or perform file I/O. Windows `.pdata`/C++ unwinding is not
//! implemented here.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

const FATAL_SIGNALS: [libc::c_int; 4] = [libc::SIGSEGV, libc::SIGILL, libc::SIGFPE, libc::SIGBUS];
static INSTALLED: AtomicBool = AtomicBool::new(false);
static HANDLING: AtomicBool = AtomicBool::new(false);
static CRASH_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CRASH_PC: AtomicUsize = AtomicUsize::new(0);
static CRASH_ADDRESS: AtomicUsize = AtomicUsize::new(0);
static CRASH_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

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

fn signal_name(signal: libc::c_int) -> &'static str {
    match signal {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        libc::SIGBUS => "SIGBUS",
        _ => "fatal signal",
    }
}

unsafe extern "C" fn crash_handler(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut std::ffi::c_void,
) {
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
    let message = format!(
        "{} (signal {signal}) at {}; pc {pc:#x}; fault address {address:#x}",
        signal_name(signal),
        crate::modules::describe_address(pc)
    );
    crate::report::write_crash(message);
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pc_hook_is_an_explicit_unsupported_path() {
        let error = install_pc_hook().unwrap_err();
        assert!(error.contains("not available on Linux"), "{error}");
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
