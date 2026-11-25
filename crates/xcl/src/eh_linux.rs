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

/// Set once the crash path has dumped the other threads (port of upstream #164's
/// `DUMPED` once-guard: a second fatal signal while the first report is still being
/// written must not dump again).
static DUMPED: AtomicBool = AtomicBool::new(false);

/// Claims the one crash-path thread dump (`true`: the caller dumps; `false`: already
/// dumped). Factored so unit tests can check the once-guard without touching the
/// process-global `DUMPED`.
fn claim_crash_dump(dumped: &AtomicBool) -> bool {
    !dumped.swap(true, Ordering::AcqRel)
}

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

// ---------------------------------------------------------------------------------------
// thread dumps: unwind a copy of each stopped thread's stack (#253)
// ---------------------------------------------------------------------------------------
//
// While a thread is stopped the dump takes only its registers and a copy of its stack
// (system calls only: no heap, no Rust lock a stopped thread may hold). The thread then
// goes on (or ends) and the unwinding reads the COPY, with addresses into the stack moved
// to it, ending where a frame would leave it – the shape of upstream #253, whose
// `RtlVirtualUnwind` read live memory past the resume (a freed stack, a stale frame
// register to near-null). Linux has no `RtlVirtualUnwind`: the stop is a targeted
// `SIGUSR2` whose handler parks the thread, the copy is `pread` from `/proc/self/mem`
// bounded by the mapping end in `/proc/self/maps`, and the unwind is a frame-pointer
// walk with a leaf fallback, all inside the copy.

/// Bytes of a thread's stack (from RSP up) a thread dump keeps.
const DUMP_STACK: usize = 0x4_0000;

/// Signal that parks a thread for a dump. The title is a Windows game: it never installs
/// POSIX handlers itself, so this host-internal signal is never its own.
const DUMP_SIGNAL: libc::c_int = libc::SIGUSR2;

/// End of the mapping containing `addr` (`None`: unmapped), from `/proc/self/maps` with
/// `open`/`read`/`close` only – stack buffers, no heap, no lock.
unsafe fn stack_region_end(addr: u64) -> Option<u64> {
    const PATH: &[u8] = b"/proc/self/maps\0";
    let fd = libc::open(PATH.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 8192];
    let mut tail = [0u8; 256];
    let mut tail_len = 0usize;
    let mut end: Option<u64> = None;
    loop {
        let r = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
        if r <= 0 {
            break;
        }
        let chunk = &buf[..r as usize];
        // first line may continue the previous chunk's tail
        let mut start = 0usize;
        if tail_len > 0 {
            if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
                if let Some(e) = mapping_end_containing(&tail[..tail_len], &chunk[..nl], addr) {
                    end = Some(e);
                    tail_len = 0;
                    if end.is_some() {
                        break;
                    }
                } else {
                    tail_len = 0;
                }
                start = nl + 1;
            } else {
                // still no newline: keep the tail bounded, skip this chunk's head
                let keep = chunk.len().min(tail.len() - tail_len);
                tail[tail_len..tail_len + keep].copy_from_slice(&chunk[..keep]);
                tail_len += keep;
                continue;
            }
        }
        let mut line_start = start;
        for (i, &b) in chunk.iter().enumerate().skip(start) {
            if b == b'\n' {
                if let Some(e) = mapping_end_containing(&[], &chunk[line_start..i], addr) {
                    end = Some(e);
                    break;
                }
                line_start = i + 1;
            }
        }
        if end.is_some() {
            break;
        }
        // stash the incomplete last line (bounded: overlong lines cannot hold addr anyway –
        // the address range is at the head, parsed before the path can overflow it)
        let rest = &chunk[line_start..];
        if rest.is_empty() {
            tail_len = 0;
        } else if rest.len() <= tail.len() {
            tail[..rest.len()].copy_from_slice(rest);
            tail_len = rest.len();
        } else {
            if let Some(e) = mapping_end_containing(&[], rest, addr) {
                end = Some(e);
                break;
            }
            tail_len = 0;
        }
        if (r as usize) < buf.len() {
            break;
        }
    }
    libc::close(fd);
    end
}

/// End address of the maps line `head ++ rest` when its range contains `addr`
/// (`head` is the previous chunk's tail, possibly empty). Lines look like
/// `start-end perms ...`: only the head is parsed.
fn mapping_end_containing(head: &[u8], rest: &[u8], addr: u64) -> Option<u64> {
    fn hex_value(line: &[u8], mut i: usize) -> (u64, usize) {
        let mut v = 0u64;
        while i < line.len() {
            let d = match line[i] {
                b'0'..=b'9' => (line[i] - b'0') as u64,
                b'a'..=b'f' => (line[i] - b'a' + 10) as u64,
                _ => break,
            };
            v = v.wrapping_shl(4) | d;
            i += 1;
        }
        (v, i)
    }
    // copy the head (at most "ffffffffffffffff-") plus the range start into a stack line
    let mut line = [0u8; 64];
    let n = (head.len() + rest.len()).min(line.len());
    let mut joined = 0usize;
    for &b in head.iter().chain(rest.iter()).take(n) {
        line[joined] = b;
        joined += 1;
    }
    let (start, i) = hex_value(&line[..joined], 0);
    if i >= joined || line[i] != b'-' {
        return None;
    }
    let (stop, _) = hex_value(&line[..joined], i + 1);
    (start <= addr && addr < stop).then_some(stop)
}

/// The stack from `rsp` up (to the end of its region, at most `buf.len()` bytes) copied to
/// `buf`: its length. Only system calls – no lock a stopped thread may hold.
///
/// A short copy (fewer bytes than the region allows) is still returned when it holds at
/// least one word: between the maps lookup and the read a neighbor thread's exit can
/// punch a hole into the merged anonymous area the stack lives in (thread churn around
/// a dump), and `/proc/self/mem` then short-reads or errors past the hole. The unwinder
/// is bounds-checked and ends where the copy ends, so a truncated copy still unwinds
/// the hottest frames – the same clamp upstream's region end provides.
unsafe fn copy_stack(rsp: u64, buf: &mut [u8]) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    let end = stack_region_end(rsp)?;
    let n = (end.saturating_sub(rsp) as usize).min(buf.len());
    if n < 8 {
        return None;
    }
    const PATH: &[u8] = b"/proc/self/mem\0";
    let fd = libc::open(PATH.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return None;
    }
    let mut off = 0usize;
    while off < n {
        let r = libc::pread(
            fd,
            buf.as_mut_ptr().add(off) as *mut libc::c_void,
            n - off,
            rsp.wrapping_add(off as u64) as libc::off_t,
        );
        if r == 0 {
            break;
        }
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        off += r as usize;
    }
    libc::close(fd);
    (off >= 8).then_some(off)
}

/// A stopped thread's registers for the copy-then-unwind walk.
#[derive(Clone, Copy, Default)]
struct ThreadRegs {
    rax: u64,
    rcx: u64,
    rdx: u64,
    rbx: u64,
    rsp: u64,
    rbp: u64,
    rsi: u64,
    rdi: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
}

#[cfg(target_arch = "x86_64")]
unsafe fn regs_from_ucontext(ctx: *const libc::c_void) -> ThreadRegs {
    let uc = &*(ctx as *const libc::ucontext_t);
    let g = |r: libc::c_int| uc.uc_mcontext.gregs[r as usize] as u64;
    ThreadRegs {
        rax: g(libc::REG_RAX),
        rcx: g(libc::REG_RCX),
        rdx: g(libc::REG_RDX),
        rbx: g(libc::REG_RBX),
        rsp: g(libc::REG_RSP),
        rbp: g(libc::REG_RBP),
        rsi: g(libc::REG_RSI),
        rdi: g(libc::REG_RDI),
        r8: g(libc::REG_R8),
        r9: g(libc::REG_R9),
        r10: g(libc::REG_R10),
        r11: g(libc::REG_R11),
        r12: g(libc::REG_R12),
        r13: g(libc::REG_R13),
        r14: g(libc::REG_R14),
        r15: g(libc::REG_R15),
        rip: g(libc::REG_RIP),
    }
}

/// The return addresses of the frames from `regs` on (its RIP first), unwound from the
/// thread's stack copy: `stack` is the bytes from `regs.rsp` on, taken while the thread
/// was stopped. The thread has gone on (or ended) since, so the walk reads the copy –
/// addresses into the stack are moved to it – and ends where a frame would leave it.
fn unwind_regs(regs: &mut ThreadRegs, n: usize, stack: &[u8]) -> Vec<usize> {
    if stack.is_empty() || n == 0 {
        return Vec::new();
    }
    let (orig, copy, len) = (regs.rsp, stack.as_ptr() as u64, stack.len() as u64);
    if orig == 0 {
        return Vec::new();
    }
    let in_copy = |v: u64| v >= copy && v < copy + len;
    // registers restored from the copy hold addresses into the original stack
    let to_copy = |r: &mut ThreadRegs| {
        for f in [
            &mut r.rax, &mut r.rcx, &mut r.rdx, &mut r.rbx, &mut r.rsp, &mut r.rbp,
            &mut r.rsi, &mut r.rdi, &mut r.r8, &mut r.r9, &mut r.r10, &mut r.r11,
            &mut r.r12, &mut r.r13, &mut r.r14, &mut r.r15,
        ] {
            if *f >= orig && *f < orig + len {
                *f = *f - orig + copy;
            }
        }
    };
    let read_word = |at: u64| -> Option<u64> {
        if !in_copy(at) || at + 8 > copy + len {
            return None;
        }
        Some(unsafe { (copy as *const u8).add((at - copy) as usize).cast::<u64>().read_unaligned() })
    };
    to_copy(regs);
    let mut out = Vec::new();
    for _ in 0..n {
        let rip = regs.rip;
        if rip == 0 || !in_copy(regs.rsp) || regs.rsp + 8 > copy + len {
            break;
        }
        out.push(rip as usize);
        // a frame-pointer link inside the copy: [rbp] is the caller's rbp, [rbp+8] its return
        let rbp = regs.rbp;
        if rbp & 7 == 0 {
            if let (Some(next), Some(ret)) = (read_word(rbp), read_word(rbp.wrapping_add(8))) {
                let next_orig = next >= orig && next < orig + len;
                if ret != 0 && (next == 0 || next_orig || in_copy(next)) && next != rbp {
                    regs.rbp = next;
                    regs.rip = ret;
                    regs.rsp = rbp.wrapping_add(16);
                    to_copy(regs);
                    continue;
                }
            }
        }
        // a leaf function: the return address is at RSP
        match read_word(regs.rsp) {
            Some(ret) => {
                regs.rip = ret;
                regs.rsp = regs.rsp.wrapping_add(8);
            }
            None => break,
        }
        to_copy(regs);
    }
    out
}

// One dump slot per thread the dump may park: the handler touches atomics only
// (async-signal-safe), the dumper owns the slot between claim and release.
struct DumpSlot {
    tid: AtomicUsize,
    ready: AtomicBool,
    release: AtomicBool,
    regs: [AtomicUsize; 17],
}

impl DumpSlot {
    const fn new() -> DumpSlot {
        DumpSlot {
            tid: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
            release: AtomicBool::new(false),
            regs: [const { AtomicUsize::new(0) }; 17],
        }
    }
}

const MAX_DUMP_THREADS: usize = 64;
static DUMP_SLOTS: [DumpSlot; MAX_DUMP_THREADS] = [const { DumpSlot::new() }; MAX_DUMP_THREADS];
static DUMP_INSTALLED: AtomicBool = AtomicBool::new(false);
static DUMPING: AtomicBool = AtomicBool::new(false);

/// Parked-thread handler: store this thread's registers in its claimed slot, then wait
/// (bounded) for the dumper to copy the stack. Async-signal-safe: atomics and
/// `clock_gettime` only – no heap, no lock the thread may hold elsewhere.
unsafe extern "C" fn dump_handler(
    _signal: libc::c_int,
    _info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let me = libc::syscall(libc::SYS_gettid) as usize;
    let Some(slot) = DUMP_SLOTS.iter().find(|s| s.tid.load(Ordering::Acquire) == me) else {
        return;
    };
    #[cfg(target_arch = "x86_64")]
    {
        let r = regs_from_ucontext(context);
        let v = [
            r.rax as usize, r.rcx as usize, r.rdx as usize, r.rbx as usize, r.rsp as usize,
            r.rbp as usize, r.rsi as usize, r.rdi as usize, r.r8 as usize, r.r9 as usize,
            r.r10 as usize, r.r11 as usize, r.r12 as usize, r.r13 as usize, r.r14 as usize,
            r.r15 as usize, r.rip as usize,
        ];
        for (i, w) in v.iter().enumerate() {
            slot.regs[i].store(*w, Ordering::Release);
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = context;
        for w in slot.regs.iter() {
            w.store(0, Ordering::Release);
        }
    }
    slot.ready.store(true, Ordering::Release);
    // the dumper copies the stack, then releases us; never park forever (500 ms) –
    // a dumper that died between claim and release must not hang the title
    let mut now = std::mem::zeroed::<libc::timespec>();
    libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
    let until = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 + 500_000_000;
    while !slot.release.load(Ordering::Acquire) {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
        if now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 >= until {
            break;
        }
        std::hint::spin_loop();
    }
}

fn install_dump_handler() {
    if DUMP_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = dump_handler as libc::sighandler_t;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(DUMP_SIGNAL, &action, std::ptr::null_mut()) != 0 {
            crate::xlog_warn!(
                "thread dump: sigaction({DUMP_SIGNAL}) failed: {}",
                std::io::Error::last_os_error()
            );
            DUMP_INSTALLED.store(false, Ordering::Release);
        }
    }
}

fn task_tids() -> Vec<i32> {
    let me = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    match std::fs::read_dir("/proc/self/task") {
        Ok(entries) => entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&t| t != me)
            .take(MAX_DUMP_THREADS)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `XCL_THREAD_DUMP_AT`: every other thread of the process stopped for a moment – where it
/// is (RIP) and the calls on its stack that lead there, in the log. Only the registers
/// and a copy of the stack are taken while a thread is parked: the unwinding, the module
/// lookup and the formatting take locks the parked thread may hold. The unwinding reads
/// the copy: the thread goes on, or ends.
pub fn dump_threads() {
    install_dump_handler();
    if DUMPING.swap(true, Ordering::AcqRel) {
        return;
    }
    // allocated before any thread is parked: it may hold the heap's lock. (The slack past
    // the copy: a frame's unwinding may read a little beyond where the copy ends.)
    let mut stack = vec![0u8; DUMP_STACK + 0x1_0000];
    let pid = unsafe { libc::getpid() };
    let mut lines = Vec::new();
    for tid in task_tids() {
        let Some(slot) = DUMP_SLOTS.iter().find(|s| s.tid.load(Ordering::Acquire) == 0) else {
            break;
        };
        for w in slot.regs.iter() {
            w.store(0, Ordering::Relaxed);
        }
        slot.ready.store(false, Ordering::Relaxed);
        slot.release.store(false, Ordering::Relaxed);
        slot.tid.store(tid as usize, Ordering::Release);
        if unsafe { libc::tgkill(pid, tid, DUMP_SIGNAL) } != 0 {
            slot.tid.store(0, Ordering::Release);
            continue;
        }
        // the parked thread stores its registers, then waits for the copy
        let mut waited = 0;
        while !slot.ready.load(Ordering::Acquire) && waited < 200 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            waited += 1;
        }
        if !slot.ready.load(Ordering::Acquire) {
            // exited, or blocking the dump signal: let it go if it arrives late
            slot.tid.store(0, Ordering::Release);
            continue;
        }
        let v: [usize; 17] = std::array::from_fn(|i| slot.regs[i].load(Ordering::Acquire));
        let mut regs = ThreadRegs {
            rax: v[0] as u64,
            rcx: v[1] as u64,
            rdx: v[2] as u64,
            rbx: v[3] as u64,
            rsp: v[4] as u64,
            rbp: v[5] as u64,
            rsi: v[6] as u64,
            rdi: v[7] as u64,
            r8: v[8] as u64,
            r9: v[9] as u64,
            r10: v[10] as u64,
            r11: v[11] as u64,
            r12: v[12] as u64,
            r13: v[13] as u64,
            r14: v[14] as u64,
            r15: v[15] as u64,
            rip: v[16] as u64,
        };
        let mut len = 0usize;
        let copied = unsafe { copy_stack(regs.rsp, &mut stack[..DUMP_STACK]).inspect(|&n| len = n) };
        // the thread goes on (or has ended): the unwinding reads the copy
        slot.release.store(true, Ordering::Release);
        slot.tid.store(0, Ordering::Release);
        if copied.is_some() {
            let frames: Vec<String> = unwind_regs(&mut regs, 14, &stack[..len])
                .into_iter()
                .map(crate::modules::describe_address)
                .collect();
            lines.push(format!("thread {tid}: {}", frames.join(" < ")));
        }
    }
    crate::xlog_info!("thread dump ({} threads):\n{}", lines.len(), lines.join("\n"));
    crate::log::flush();
    DUMPING.store(false, Ordering::Release);
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
    // the other threads now: a title crash handler can keep the process for a long time
    // before it ends, so name what its silent threads were doing (port of upstream #164;
    // watchdog-thread context, not the raw signal handler, so parking threads is safe).
    if claim_crash_dump(&DUMPED) {
        dump_threads();
    }
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

    /// recursion whose frames hold return addresses and saved registers that change as it runs
    #[inline(never)]
    fn churn_stack(depth: u32, seed: u64) -> u64 {
        let local = std::hint::black_box([seed; 8]);
        if depth == 0 {
            return local.iter().fold(0u64, |a, &x| a.wrapping_add(x));
        }
        churn_stack(depth - 1, seed.wrapping_mul(6364136223846793005).wrapping_add(depth as u64)) ^ local[depth as usize % 8]
    }

    #[test]
    fn copy_stack_copies_the_live_stack() {
        let rsp: u64 = unsafe {
            let r: u64;
            std::arch::asm!("mov {}, rsp", out(reg) r, options(nomem, nostack, preserves_flags));
            r
        };
        let mut buf = vec![0u8; DUMP_STACK];
        let n = unsafe { copy_stack(rsp, &mut buf) }.expect("the current stack is mapped");
        // a neighbor thread's exit can punch a hole past the maps lookup, truncating the
        // copy: the first word (this frame) is always there, the tail is best-effort
        assert!(n >= 8 && n <= DUMP_STACK, "bounded copy: {n}");
        let live = unsafe { (rsp as *const u64).read_unaligned() };
        assert_eq!(u64::from_le_bytes(buf[..8].try_into().unwrap()), live);
    }

    #[test]
    fn unwind_reads_the_copy_not_live_memory() {
        // fake original stack: leaf return addresses at its base
        let mut orig = vec![0u8; 64];
        for (i, w) in [0xBBBB_0001u64, 0xBBBB_0002, 0xBBBB_0003, 0].iter().enumerate() {
            orig[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        let base = orig.as_ptr() as u64;
        let stack = orig.clone();
        let copy = stack.as_ptr() as u64;
        let mut regs = ThreadRegs {
            rip: 0xAAAA_0001,
            rsp: base,
            rsi: base + 8,
            ..Default::default()
        };
        let frames = unwind_regs(&mut regs, 14, &stack);
        assert_eq!(frames, vec![0xAAAA_0001usize, 0xBBBB_0001, 0xBBBB_0002, 0xBBBB_0003]);
        // registers into the original stack moved to the copy
        assert_eq!(regs.rsi, copy + 8);
        // a live edit after the copy is invisible to the unwinder
        orig[0] = 0xFF;
        let mut regs2 = ThreadRegs { rip: 0xAAAA_0001, rsp: base, ..Default::default() };
        assert_eq!(unwind_regs(&mut regs2, 14, &stack), frames);
        // a walk that would leave the copy ends there: no zero terminator, so the leaf
        // steps run to the end (the initial RIP + 7 words pushed; the 8th word is in RIP
        // when RSP runs out, and the walk stops)
        let full = vec![0x01u8; 64];
        let full_base = full.as_ptr() as u64;
        let full_copy = full.clone();
        let mut regs3 = ThreadRegs { rip: 0xAAAA_0001, rsp: full_base, ..Default::default() };
        let walked = unwind_regs(&mut regs3, 14, &full_copy);
        assert_eq!(walked.len(), 8, "{walked:x?}");
        assert_eq!(walked[0], 0xAAAA_0001);
        // a zero RIP ends the walk at once
        let mut regs4 = ThreadRegs { rip: 0, rsp: base, ..Default::default() };
        assert!(unwind_regs(&mut regs4, 14, &stack).is_empty());
    }

    /// Thread dumps while threads run, start and end around them (#106: a dump read a near-null
    /// address inside the unwinder).
    #[test]
    fn thread_dumps_while_threads_run_and_exit() {
        use std::sync::atomic::AtomicBool;
        static STOP: AtomicBool = AtomicBool::new(false);
        let busy: Vec<_> = (0..4u64)
            .map(|i| std::thread::spawn(move || {
                let mut n = 0u64;
                while !STOP.load(Ordering::Relaxed) {
                    n = n.wrapping_add(churn_stack((n % 40) as u32 + 1, n ^ i));
                }
                n
            }))
            .collect();
        let churn: Vec<_> = (0..4u64)
            .map(|i| std::thread::spawn(move || {
                let mut n = 0u64;
                while !STOP.load(Ordering::Relaxed) {
                    n = std::thread::spawn(move || churn_stack((n % 24) as u32 + 1, n ^ i)).join().unwrap();
                }
            }))
            .collect();
        for _ in 0..40 {
            dump_threads();
        }
        STOP.store(true, Ordering::Relaxed);
        churn.into_iter().for_each(|t| t.join().unwrap());
        busy.into_iter().for_each(|t| {
            t.join().unwrap();
        });
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
    fn crash_dump_claims_once() {
        let dumped = AtomicBool::new(false);
        assert!(claim_crash_dump(&dumped), "first crash dumps threads");
        assert!(!claim_crash_dump(&dumped), "second crash does not dump again");
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
