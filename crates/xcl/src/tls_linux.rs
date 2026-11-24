//! Static TLS (`__declspec(thread)`) for manually mapped Windows images on Linux.
//!
//! Windows PE code reads static TLS through `gs:[0x58]`
//! (`TEB.ThreadLocalStoragePointer`) and the image's `_tls_index` (`DWORD` in the image):
//!
//! ```text
//! mov rax, qword ptr gs:[0x58]
//! mov rax, [rax + _tls_index*8]
//! ```
//!
//! Linux keeps its thread pointer in `FS`, so `GS` is free: every attached host thread gets
//! a minimal TEB whose `0x58` slot points at an array of per-image blocks. The image's
//! `_tls_index` is written to the first free slot, exactly as the Windows loader would, and
//! each block is a zero-filled copy of the image's TLS template. Nothing here interposes
//! `pthread_create`: all game threads are created by the host's kernelx shim, so that shim
//! enters [`spawn`] (or calls [`attach_current_thread`]) instead.
//!
//! The process's main image takes TLS slot 0, as a Windows process executable always does.
//! Nothing to exchange: xcl itself is an ELF and its own TLS lives in `FS`, so slot 0 is
//! free (the Windows implementation has to move `xcl.exe`'s block out of the way).
//!
//! `DLL_THREAD_ATTACH` callbacks run after the blocks are installed; `DLL_THREAD_DETACH`
//! callbacks run before the blocks are freed, in the same module/callback order as
//! `loader.rs`. Process attach/detach is the loader's (`run_main`) job, not this module's.
//!
//! `// LINUX-TODO: Phase C step-10` — the game-DLL load path does not exist yet; when it
//! lands it installs DLL images here too (the main image installs at `load_main`). The
//! kernelx thread shim is ported in the same step.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::cell::Cell;
use std::ffi::c_void;
use std::io;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

/// No slot assigned (Windows' `TLS_OUT_OF_INDEXES`).
pub const NO_INDEX: u32 = u32::MAX;

/// `PIMAGE_TLS_CALLBACK` on x64: `void (*)(PVOID hinst, DWORD reason, PVOID reserved)`.
/// PE code uses the Windows x64 ABI even when the host is Linux.
pub type TlsCallback = unsafe extern "win64" fn(*mut c_void, u32, *mut c_void);

pub const DLL_PROCESS_DETACH: u32 = 0;
pub const DLL_PROCESS_ATTACH: u32 = 1;
pub const DLL_THREAD_ATTACH: u32 = 2;
pub const DLL_THREAD_DETACH: u32 = 3;

/// TEB offsets (x64): `ThreadLocalStoragePointer` and the self pointer.
const TEB_SELF: usize = 0x30;
const TEB_TLS_POINTER: usize = 0x58;
const TEB_SIZE: usize = 0x100;
/// Windows allocates TLS blocks 16-byte aligned.
const BLOCK_ALIGN: usize = 16;

/// `arch_prctl` codes (asm/prctl.h); `libc` does not export them.
const ARCH_SET_GS: libc::c_int = 0x1001;
const ARCH_GET_GS: libc::c_int = 0x1004;

/// One mapped image's TLS directory, as the loader parsed it.
pub struct ImageSpec {
    pub name: String,
    /// Already-relocated template bytes in the mapped image.
    pub template: usize,
    pub template_len: usize,
    /// Template plus zero fill.
    pub total_len: u64,
    /// Address of the image's `_tls_index`; 0 leaves it alone.
    pub index_ptr: usize,
    /// The module handle Windows passes to the callbacks (the image base).
    pub hinst: usize,
    pub callbacks: Vec<usize>,
    /// The process's main image: slot 0, as on Windows.
    pub main: bool,
}

struct Image {
    name: String,
    index: u32,
    template: usize,
    template_len: usize,
    total_len: usize,
    hinst: usize,
    callbacks: Vec<usize>,
}

// The template points into a mapped image that lives for the process; callbacks are code
// addresses. Both are read-only after install.
unsafe impl Send for Image {}
unsafe impl Sync for Image {}

static IMAGES: RwLock<Vec<Arc<Image>>> = RwLock::new(Vec::new());
/// Slot 0 belongs to the main image; DLLs allocate from 1.
static NEXT_SLOT: AtomicU32 = AtomicU32::new(1);
/// Serialises image install with thread attach/detach, so a thread cannot miss an image
/// that is installed concurrently (or keep a block for one that failed to install).
static INSTALL: Mutex<()> = Mutex::new(());
/// State of every attached thread, for filling blocks when an image is installed later.
static THREADS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

thread_local! {
    static STATE: Cell<usize> = const { Cell::new(0) };
}

struct Block {
    ptr: *mut u8,
    layout: Layout,
    index: u32,
}

impl Drop for Block {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

#[repr(C, align(16))]
struct FakeTeb {
    bytes: [u8; TEB_SIZE],
}

struct ThreadState {
    teb: Box<FakeTeb>,
    slots: Vec<*mut u8>,
    blocks: Vec<Block>,
    prev_gs: u64,
}

impl ThreadState {
    fn new() -> Self {
        ThreadState {
            // Zeroed: TEB fields we do not model read as 0, never as stack garbage.
            teb: Box::new(FakeTeb {
                bytes: [0u8; TEB_SIZE],
            }),
            slots: Vec::new(),
            blocks: Vec::new(),
            prev_gs: 0,
        }
    }

    fn teb_addr(&self) -> usize {
        &*self.teb as *const FakeTeb as usize
    }

    fn write_teb(&mut self) {
        let addr = self.teb_addr() as u64;
        let slots = self.slots.as_mut_ptr() as u64;
        self.teb.bytes[TEB_SELF..TEB_SELF + 8].copy_from_slice(&addr.to_le_bytes());
        self.teb.bytes[TEB_TLS_POINTER..TEB_TLS_POINTER + 8].copy_from_slice(&slots.to_le_bytes());
    }

    /// Allocate this image's block for the thread and publish it in the TEB array.
    fn attach_image(&mut self, img: &Image) -> Result<(), String> {
        let block = alloc_block(img)?;
        let i = img.index as usize;
        if self.slots.len() <= i {
            self.slots.resize(i + 1, std::ptr::null_mut());
        }
        self.slots[i] = block.ptr;
        self.blocks.push(block);
        // `resize` can move the array; the TEB pointer must follow it.
        self.write_teb();
        Ok(())
    }

    /// Undo [`attach_image`] after a failed install.
    fn detach_image(&mut self, index: u32) {
        if let Some(i) = self.blocks.iter().position(|b| b.index == index) {
            let b = self.blocks.remove(i);
            self.slots[index as usize] = std::ptr::null_mut();
            drop(b);
            self.write_teb();
        }
    }
}

fn alloc_block(img: &Image) -> Result<Block, String> {
    let size = img.total_len.max(1);
    let layout = Layout::from_size_align(size, BLOCK_ALIGN)
        .map_err(|e| format!("TLS block for {} ({size} B): {e}", img.name))?;
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(format!(
            "TLS block for {}: out of memory ({size} B)",
            img.name
        ));
    }
    if img.template_len != 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(img.template as *const u8, ptr, img.template_len);
        }
    }
    Ok(Block {
        ptr,
        layout,
        index: img.index,
    })
}

unsafe fn call_callbacks(img: &Image, reason: u32) {
    for &cb in &img.callbacks {
        let f: TlsCallback = std::mem::transmute(cb);
        f(img.hinst as *mut c_void, reason, std::ptr::null_mut());
    }
}

fn get_gs() -> Result<u64, String> {
    let mut base = 0u64;
    let r = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_GET_GS, &mut base as *mut u64) };
    if r == -1 {
        Err(format!(
            "arch_prctl(ARCH_GET_GS): {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(base)
    }
}

fn set_gs(base: usize) -> Result<(), String> {
    let r = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, base) };
    if r == -1 {
        Err(format!(
            "arch_prctl(ARCH_SET_GS, {base:#x}): {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

fn lock_install() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    INSTALL
        .lock()
        .map_err(|_| "TLS install lock poisoned".to_string())
}

/// Register an image and allocate its block in every thread that is already attached.
///
/// `// LINUX-TODO: Phase C step-10` — game-DLL process-attach callbacks (`reason == 1`) and
/// DllMain are the loader's `init_dll` work. The main executable's process callbacks run in
/// `loader_linux::run_main` after the current thread has attached.
///
/// # Safety
/// `template` must point at `template_len` readable bytes in a live mapping, and
/// `index_ptr`, when non-zero, at a writable `u32`.
pub unsafe fn install(spec: ImageSpec) -> Result<u32, String> {
    if spec.template_len != 0 && spec.template == 0 {
        return Err(format!("{}: TLS template is null", spec.name));
    }
    if spec.template_len as u64 > spec.total_len {
        return Err(format!(
            "{}: TLS template {} B exceeds the block {} B",
            spec.name, spec.template_len, spec.total_len
        ));
    }
    let total_len = usize::try_from(spec.total_len).map_err(|_| {
        format!(
            "{}: TLS block {} B does not fit usize",
            spec.name, spec.total_len
        )
    })?;
    if Layout::from_size_align(total_len.max(1), BLOCK_ALIGN).is_err() {
        return Err(format!(
            "{}: TLS block {} B is too large",
            spec.name, spec.total_len
        ));
    }

    let _guard = lock_install()?;

    let index = if spec.main {
        if IMAGES
            .read()
            .map_err(|_| "TLS image table poisoned".to_string())?
            .iter()
            .any(|i| i.index == 0)
        {
            return Err("a main image already owns TLS slot 0".to_string());
        }
        0
    } else {
        let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        if slot >= NO_INDEX {
            return Err("TLS slots exhausted".to_string());
        }
        slot
    };

    let img = Arc::new(Image {
        name: spec.name,
        index,
        template: spec.template,
        template_len: spec.template_len,
        total_len,
        hinst: spec.hinst,
        callbacks: spec.callbacks,
    });

    // A thread cannot attach while INSTALL is held: either it is in THREADS and gets the
    // block here, or it attaches after the image is published and finds it in IMAGES.
    let threads = THREADS
        .lock()
        .map_err(|_| "TLS thread table poisoned".to_string())?;
    for (i, &p) in threads.iter().enumerate() {
        let state = &mut *(p as *mut ThreadState);
        if let Err(e) = state.attach_image(&img) {
            for &done in &threads[..i] {
                (*(done as *mut ThreadState)).detach_image(index);
            }
            return Err(e);
        }
    }
    IMAGES
        .write()
        .map_err(|_| "TLS image table poisoned".to_string())?
        .push(img.clone());
    drop(threads);

    if spec.index_ptr != 0 {
        // The image's compiled TLS accesses reload `_tls_index`; publish it before any
        // thread can run image code that reads it.
        fence(Ordering::SeqCst);
        std::ptr::write_volatile(spec.index_ptr as *mut u32, index);
        fence(Ordering::SeqCst);
    }
    crate::xlog_info!(
        "TLS: {}: slot {index}, template {} B, block {} B, {} callbacks",
        img.name,
        spec.template_len,
        total_len,
        img.callbacks.len()
    );
    Ok(index)
}

/// Give the current thread a TEB, its own copy of every image's TLS template, and run the
/// images' `DLL_THREAD_ATTACH` callbacks. Idempotent.
pub fn attach_current_thread() -> Result<(), String> {
    if STATE.with(|s| s.get()) != 0 {
        return Ok(());
    }
    let mut state = Box::new(ThreadState::new());

    let images: Vec<Arc<Image>> = {
        let _guard = lock_install()?;
        let images = IMAGES
            .read()
            .map_err(|_| "TLS image table poisoned".to_string())?
            .clone();
        for img in &images {
            state.attach_image(img)?;
        }
        let prev = get_gs()?;
        state.prev_gs = prev;
        state.write_teb();
        set_gs(state.teb_addr())?;

        let mut threads = THREADS
            .lock()
            .map_err(|_| "TLS thread table poisoned".to_string())?;
        let ptr = Box::into_raw(state);
        threads.push(ptr as usize);
        STATE.with(|s| s.set(ptr as usize));
        images
    };

    // Blocks exist; now the image sees the thread. A callback may install more images.
    for img in &images {
        unsafe { call_callbacks(img, DLL_THREAD_ATTACH) };
    }
    Ok(())
}

/// Run the images' `DLL_THREAD_DETACH` callbacks, free the thread's blocks and restore the
/// previous `GS`. Does nothing on a thread that was never attached.
pub fn detach_current_thread() {
    let ptr = STATE.with(|s| s.replace(0));
    if ptr == 0 {
        return;
    }

    let images = {
        let Ok(_guard) = INSTALL.lock() else { return };
        let images = IMAGES.read().map(|v| v.clone()).unwrap_or_default();
        if let Ok(mut threads) = THREADS.lock() {
            threads.retain(|&p| p != ptr);
        }
        images
    };

    // Same order as the Windows loader: modules in reverse load order, callbacks forward.
    for img in images.iter().rev() {
        unsafe { call_callbacks(img, DLL_THREAD_DETACH) };
    }

    let mut state = unsafe { Box::from_raw(ptr as *mut ThreadState) };
    if let Err(e) = set_gs(state.prev_gs as usize) {
        crate::xlog_warn!("TLS: restoring the previous GS base failed: {e}");
    }
    state.blocks.clear();
}

/// Spawn a host thread that owns static TLS blocks for the mapped images.
///
/// This is the thread entry the kernelx shim uses for game threads: `DLL_THREAD_ATTACH`
/// callbacks run before `f`, `DLL_THREAD_DETACH` callbacks after it, and both also run when
/// `f` panics (the panic is resumed afterwards, as `thread::spawn` would).
pub fn spawn<F, T>(name: &str, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let thread_name = name.to_string();
    std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            if let Err(e) = attach_current_thread() {
                panic!("xcl TLS: attaching thread {thread_name:?}: {e}");
            }
            let result = catch_unwind(AssertUnwindSafe(f));
            detach_current_thread();
            match result {
                Ok(v) => v,
                Err(payload) => resume_unwind(payload),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, AtomicUsize};
    use std::sync::Barrier;

    static TEMPLATE_7: [u8; 8] = [7, 0, 0, 0, 0, 0, 0, 0];
    /// The image the callback test owns; callbacks from other tests' images ignore it.
    static CB_TOKEN: AtomicUsize = AtomicUsize::new(0);
    static CB_INDEX: AtomicU32 = AtomicU32::new(NO_INDEX);
    static CB_SEEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    static CB_ATTACH_BYTE: AtomicU8 = AtomicU8::new(0xFF);

    /// Read byte 0 of this thread's block for `index` the way PE code does.
    unsafe fn read_tls_byte(index: u32) -> u8 {
        let slots: *mut *mut u8;
        core::arch::asm!(
            "mov {}, gs:[0x58]",
            out(reg) slots,
            options(nostack, readonly, preserves_flags)
        );
        let block = *slots.add(index as usize);
        *block
    }

    fn leaked_index() -> usize {
        Box::into_raw(Box::new(NO_INDEX)) as usize
    }

    unsafe extern "win64" fn record_callback(
        hinst: *mut c_void,
        reason: u32,
        _reserved: *mut c_void,
    ) {
        if hinst as usize != CB_TOKEN.load(Ordering::SeqCst) {
            return;
        }
        // Tests run in parallel: every attached thread runs every image's callbacks. Only
        // the thread this test spawned is of interest here.
        if std::thread::current().name() != Some("tls-callbacks") {
            return;
        }
        if reason == DLL_THREAD_ATTACH {
            // The block must be installed before the callback sees the thread.
            let index = CB_INDEX.load(Ordering::Acquire);
            CB_ATTACH_BYTE.store(unsafe { read_tls_byte(index) }, Ordering::Release);
        }
        if let Ok(mut seen) = CB_SEEN.lock() {
            seen.push(reason);
        }
    }

    #[test]
    fn template_seven_is_eight_on_two_threads() {
        let index_ptr = leaked_index();
        let index = unsafe {
            install(ImageSpec {
                name: "tls-test-template".into(),
                template: TEMPLATE_7.as_ptr() as usize,
                template_len: TEMPLATE_7.len(),
                total_len: TEMPLATE_7.len() as u64,
                index_ptr,
                hinst: 0,
                callbacks: Vec::new(),
                main: false,
            })
            .expect("synthetic TLS image installs")
        };
        assert_ne!(index, NO_INDEX);

        let barrier = Arc::new(Barrier::new(2));
        let (b1, b2) = (barrier.clone(), barrier.clone());
        let writer = spawn("tls-7-writer", move || unsafe {
            assert_eq!(read_tls_byte(index), 7, "writer's template");
            b1.wait();
            *read_slot(index) = 8;
            b1.wait();
        })
        .expect("spawn writer");

        let reader = spawn("tls-7-reader", move || unsafe {
            assert_eq!(read_tls_byte(index), 7, "reader's template");
            b2.wait();
            b2.wait();
            // If the block were shared, the writer's 8 would be visible here.
            let seen = read_tls_byte(index);
            *read_slot(index) = 8;
            (seen, read_tls_byte(index))
        })
        .expect("spawn reader");

        writer.join().expect("writer");
        assert_eq!(reader.join().expect("reader"), (7, 8));
    }

    /// Pointer to byte 0 of this thread's block (the inverse of [`read_tls_byte`]).
    unsafe fn read_slot(index: u32) -> *mut u8 {
        let slots: *mut *mut u8;
        core::arch::asm!(
            "mov {}, gs:[0x58]",
            out(reg) slots,
            options(nostack, readonly, preserves_flags)
        );
        *slots.add(index as usize)
    }

    #[test]
    fn thread_callbacks_see_the_block_before_attach_and_run_before_free() {
        let token = Box::into_raw(Box::new(0u8)) as usize;
        CB_TOKEN.store(token, Ordering::SeqCst);
        let index_ptr = leaked_index();
        let index = unsafe {
            install(ImageSpec {
                name: "tls-test-callbacks".into(),
                template: TEMPLATE_7.as_ptr() as usize,
                template_len: TEMPLATE_7.len(),
                total_len: TEMPLATE_7.len() as u64,
                index_ptr,
                hinst: token,
                callbacks: vec![record_callback as usize],
                main: false,
            })
            .expect("callback TLS image installs")
        };
        CB_INDEX.store(index, Ordering::Release);

        let before = CB_SEEN.lock().map(|v| v.len()).unwrap_or(0);
        spawn("tls-callbacks", move || unsafe {
            assert_eq!(read_tls_byte(index), 7);
        })
        .expect("spawn")
        .join()
        .expect("thread with TLS callbacks");

        let seen = CB_SEEN.lock().unwrap();
        assert_eq!(
            &seen[before..],
            &[DLL_THREAD_ATTACH, DLL_THREAD_DETACH],
            "attach runs after the block is installed, detach before it is freed"
        );
        assert_eq!(
            CB_ATTACH_BYTE.load(Ordering::Acquire),
            7,
            "the attach callback saw the image template through gs:[0x58]"
        );
    }

    #[test]
    fn attach_is_idempotent_and_detach_is_safe_without_attach() {
        detach_current_thread();
        attach_current_thread().expect("attach");
        attach_current_thread().expect("second attach");
        detach_current_thread();
        detach_current_thread();
        // A fresh attach allocates a fresh block: the registry is still consistent.
        attach_current_thread().expect("re-attach");
        detach_current_thread();
    }
}
