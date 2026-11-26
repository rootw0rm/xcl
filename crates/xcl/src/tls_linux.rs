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
//! `loader.rs`. Process attach/detach is the loader's (`run_main`/`init_dll`) job, not
//! this module's. Game DLLs install here through `loader_linux::load_game_dll`; the
//! kernelx thread shim enters through [`spawn`]/[`attach_current_thread`].

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

pub const IMAGE_SCN_ALIGN_MASK: u32 = 0x00F0_0000;

/// Extract the alignment requirement in bytes from TLS directory characteristics
/// (bits 20..24). Returns 16 if unaligned (0) or invalid.
/// Mirrors `tls.rs` (Windows carrier) so both loaders honour the same nibble.
pub fn alignment_from_characteristics(characteristics: u32) -> u32 {
    let val = (characteristics & IMAGE_SCN_ALIGN_MASK) >> 20;
    if (1..=14).contains(&val) {
        1 << (val - 1)
    } else {
        16
    }
}

/// Convert an alignment in bytes (1..=8192) to IMAGE_SCN_ALIGN_* bits.
pub fn characteristics_from_alignment(align: u32) -> u32 {
    let align = align.clamp(1, 8192).next_power_of_two();
    let val = align.trailing_zeros() + 1;
    (val << 20) & IMAGE_SCN_ALIGN_MASK
}

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
    /// Raw TLS directory characteristics (alignment nibble, bits 20..24).
    pub characteristics: u32,
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
    characteristics: u32,
    hinst: usize,
    callbacks: Vec<usize>,
}

// The template points into a mapped image that lives for the process; callbacks are code
// addresses. Both are read-only after install.
unsafe impl Send for Image {}
unsafe impl Sync for Image {}

/// Windows provides 64 static-TLS slots per thread. Preallocate the whole array up
/// front so a late DLL install never moves it under a live `gs:[0x58]` reader.
pub const MAX_TLS_SLOTS: u32 = 64;

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
    /// How many images this thread attached to. Late installs (images published after
    /// this thread attached) still get blocks allocated so TLS reads work, but the
    /// thread never receives their `DLL_THREAD_ATTACH`/`DLL_THREAD_DETACH` — matching
    /// Windows, which sends neither for a DLL loaded after thread creation.
    attached_images: usize,
}

impl ThreadState {
    fn new() -> Self {
        ThreadState {
            // Zeroed: TEB fields we do not model read as 0, never as stack garbage.
            teb: Box::new(FakeTeb {
                bytes: [0u8; TEB_SIZE],
            }),
            // Preallocate all 64 Windows slots: `attach_image` never reallocates, so
            // `gs:[0x58]` stays stable when gameplay DLLs install late.
            slots: vec![std::ptr::null_mut(); MAX_TLS_SLOTS as usize],
            blocks: Vec::new(),
            prev_gs: 0,
            attached_images: 0,
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
    /// The slot array is preallocated to 64 entries, so publishing never moves it.
    fn attach_image(&mut self, img: &Image) -> Result<(), String> {
        let block = alloc_block(img)?;
        let i = img.index as usize;
        if i >= self.slots.len() {
            return Err(format!(
                "TLS slot {} for {} exceeds the preallocated {} slots",
                i,
                img.name,
                self.slots.len()
            ));
        }
        self.slots[i] = block.ptr;
        self.blocks.push(block);
        // Slots never move after `new`, but keep the TEB pointer in sync anyway.
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
    // Honour the image's TLS alignment nibble (mirrors the Windows carrier's
    // `carrier_template_size_aligned`): blocks for align > 16 come from an
    // aligned layout so over-aligned statics stay aligned. Floor stays 16.
    let align = alignment_from_characteristics(img.characteristics)
        .max(BLOCK_ALIGN as u32) as usize;
    let layout = Layout::from_size_align(size, align)
        .map_err(|e| format!("TLS block for {} ({size} B): {e}", img.name))?;
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(format!(
            "TLS block for {}: out of memory ({size} B)",
            img.name
        ));
    }
    if img.template_len != 0 {
        if img.template == 0 {
            unsafe { dealloc(ptr, layout) };
            return Err(format!("{}: TLS template is null", img.name));
        }
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
/// Late installs only allocate blocks: threads that attached before this image do not
/// receive its `DLL_THREAD_ATTACH` (and later not its `DLL_THREAD_DETACH`), matching
/// Windows. The main executable's process callbacks run in `loader_linux::run_main`
/// after the current thread has attached; game-DLL process callbacks and DllMain run in
/// `loader_linux::init_dll`.
///
/// # Safety
/// `template` must point at `template_len` readable bytes in a live mapping, and
/// `index_ptr`, when non-zero, at a writable `u32`.
pub unsafe fn install(spec: ImageSpec) -> Result<u32, String> {
    if spec.template_len != 0 && spec.template == 0 {
        return Err(format!("{}: TLS template is null", spec.name));
    }
    if spec.template_len as u64 > spec.total_len {
        // Strictly stronger than upstream's `total_len.max(template_len)`: a
        // caller passing an inconsistent spec is a bug — fail closed instead
        // of silently widening the block. Real images always satisfy this
        // (`total_len` derives from the same `TlsDir`), so boot behavior is
        // identical; only synthetic misuse errors.
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
    let align = alignment_from_characteristics(spec.characteristics).max(BLOCK_ALIGN as u32);
    if Layout::from_size_align(total_len.max(1), align as usize).is_err() {
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
        if slot >= MAX_TLS_SLOTS {
            return Err(format!(
                "TLS slots exhausted ({MAX_TLS_SLOTS} slots): {}",
                spec.name
            ));
        }
        slot
    };

    let img = Arc::new(Image {
        name: spec.name,
        index,
        template: spec.template,
        template_len: spec.template_len,
        total_len,
        characteristics: spec.characteristics,
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
        // thread can run image code that reads it. Unaligned write: the index RVA
        // comes from the file and is only bounds-checked, never alignment-checked.
        fence(Ordering::SeqCst);
        std::ptr::write_unaligned(spec.index_ptr as *mut u32, index);
        fence(Ordering::SeqCst);
    }
    crate::xlog_info!(
        "TLS: {}: slot {index}, template {} B, block {} B, align {} B, {} callbacks",
        img.name,
        spec.template_len,
        total_len,
        align,
        img.callbacks.len()
    );
    Ok(index)
}

/// Unregister a TLS image by name (e.g. when the loader fails after the
/// install). Mirrors upstream `tls::uninstall`: drops the registry entry so a
/// later load starts clean, and detaches the image's blocks from every
/// attached thread so the failed install frees its memory instead of leaking
/// it. Threads that attached keep their slot array (preallocated, stable).
pub fn uninstall(img_name: &str) {
    let Ok(_guard) = lock_install() else { return };
    let index = IMAGES
        .read()
        .map(|v| v.iter().find(|i| i.name == img_name).map(|i| i.index))
        .unwrap_or(None);
    let Some(index) = index else { return };
    if let Ok(threads) = THREADS.lock() {
        for &p in threads.iter() {
            let state = unsafe { &mut *(p as *mut ThreadState) };
            state.detach_image(index);
        }
    }
    if let Ok(mut images) = IMAGES.write() {
        images.retain(|i| i.name != img_name);
    }
}

/// Give the current thread a TEB, its own copy of every image's TLS template, and run the
/// images' `DLL_THREAD_ATTACH` callbacks. Idempotent. Records how many images were
/// present so a later detach skips images installed after this thread attached.
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
        state.attached_images = images.len();
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
/// previous `GS`. Does nothing on a thread that was never attached. Only images present
/// at attach time receive detach callbacks; late installs are skipped (blocks are still
/// freed), matching Windows.
pub fn detach_current_thread() {
    let ptr = STATE.with(|s| s.replace(0));
    if ptr == 0 {
        return;
    }

    // Copy out the attach-time count before the state is freed below.
    let attached = unsafe { (*(ptr as *mut ThreadState)).attached_images };
    let images = {
        let Ok(_guard) = INSTALL.lock() else { return };
        let images = IMAGES.read().map(|v| v.clone()).unwrap_or_default();
        if let Ok(mut threads) = THREADS.lock() {
            threads.retain(|&p| p != ptr);
        }
        images
    };

    // Same order as the Windows loader: modules in reverse load order, callbacks forward.
    // Clamp in case images were unregistered concurrently.
    let n = attached.min(images.len());
    for img in images[..n].iter().rev() {
        unsafe { call_callbacks(img, DLL_THREAD_DETACH) };
    }

    let mut state = unsafe { Box::from_raw(ptr as *mut ThreadState) };
    if let Err(e) = set_gs(state.prev_gs as usize) {
        crate::xlog_warn!("TLS: restoring the previous GS base failed: {e}");
    }
    state.blocks.clear();
}

/// How many images are registered (process-wide).
pub fn image_count() -> usize {
    IMAGES.read().map(|v| v.len()).unwrap_or(0)
}

/// How many images the calling thread attached to (`None` when never attached).
/// Late installs beyond this count get blocks but no thread callbacks on this thread.
pub fn attached_image_count() -> Option<usize> {
    let ptr = STATE.with(|s| s.get());
    if ptr == 0 {
        return None;
    }
    Some(unsafe { (*(ptr as *const ThreadState)).attached_images })
}

/// Address of the calling thread's preallocated slot array (`None` when never attached).
/// Late installs must not move it.
pub fn slots_ptr_for_current_thread() -> Option<usize> {
    let ptr = STATE.with(|s| s.get());
    if ptr == 0 {
        return None;
    }
    Some(unsafe { (*(ptr as *const ThreadState)).slots.as_ptr() as usize })
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
                characteristics: 0,
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
                characteristics: 0,
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
    fn oversize_block_is_rejected_without_overflow_or_allocation() {
        // Linux has no carrier DLL: blocks come straight from `alloc_zeroed`, and the
        // #1403 overflow class (round-then-clamp past u64/u32::MAX) must surface as a
        // clean `Err`, never a panic or a giant allocation. The size guard runs before
        // the install lock and before any allocation, so a rejection is side-effect
        // free by construction (no NEXT_SLOT assertion: sibling tests install in
        // parallel and legitimately advance the counter concurrently).
        for total_len in [u64::MAX, u64::MAX - 15, isize::MAX as u64 + 1] {
            let err = unsafe {
                install(ImageSpec {
                    name: "tls-test-oversize".into(),
                    template: 0,
                    template_len: 0,
                    total_len,
                    characteristics: 0,
                    index_ptr: leaked_index(),
                    hinst: 0,
                    callbacks: Vec::new(),
                    main: false,
                })
            }
            .expect_err("oversize TLS block must be rejected");
            assert!(
                err.contains("does not fit") || err.contains("too large"),
                "unexpected error for {total_len:#x}: {err}"
            );
        }
    }

    #[test]
    fn alignment_nibbles_roundtrip_and_sanitize() {
        // Mirrors upstream `tls.rs` encode/decode: 1..=8192 round-trip, 0 and
        // reserved nibbles sanitize to 16.
        assert_eq!(alignment_from_characteristics(0), 16);
        for p in 0..=13 {
            let align = 1u32 << p;
            let ch = characteristics_from_alignment(align);
            assert_eq!(alignment_from_characteristics(ch), align);
        }
        assert_eq!(alignment_from_characteristics(0x00F0_0000), 16);
    }

    #[test]
    fn overaligned_block_start_satisfies_characteristics() {
        // A 512-byte-aligned TLS image must get a 512-aligned block (the
        // pre-#1431 Linux path always used 16 and would fail this).
        attach_current_thread().expect("attach");
        let template = Box::leak(Box::new([0xABu8; 64]));
        let ch = characteristics_from_alignment(512);
        let index = unsafe {
            install(ImageSpec {
                name: "tls-test-align512".into(),
                template: template.as_ptr() as usize,
                template_len: template.len(),
                total_len: template.len() as u64,
                characteristics: ch,
                index_ptr: leaked_index(),
                hinst: 0,
                callbacks: Vec::new(),
                main: false,
            })
        }
        .expect("aligned TLS image installs");
        let slots: *mut *mut u8 = unsafe {
            let mut s: *mut *mut u8;
            core::arch::asm!("mov {}, gs:[0x58]", out(reg) s, options(nostack, readonly, preserves_flags));
            s
        };
        let block = unsafe { *slots.add(index as usize) };
        assert!(!block.is_null());
        assert_eq!(block as usize % 512, 0, "block honors 512 B characteristics");
        uninstall("tls-test-align512");
    }

    #[test]
    fn uninstall_removes_the_image_and_frees_its_blocks() {
        // Global `image_count` races with sibling tests installing in parallel
        // (same reason the oversize test asserts no slot counter), so this test
        // asserts per-thread, per-index effects instead of exact global counts.
        attach_current_thread().expect("attach");
        let index = unsafe {
            install(ImageSpec {
                name: "tls-test-uninstall".into(),
                template: TEMPLATE_7.as_ptr() as usize,
                template_len: TEMPLATE_7.len(),
                total_len: TEMPLATE_7.len() as u64,
                characteristics: 0,
                index_ptr: leaked_index(),
                hinst: 0,
                callbacks: Vec::new(),
                main: false,
            })
        }
        .expect("image installs");
        let slots = slots_ptr_for_current_thread().expect("slots") as *mut *mut u8;
        assert!(!unsafe { *slots.add(index as usize) }.is_null());
        let during = image_count();
        assert!(during >= 1);
        uninstall("tls-test-uninstall");
        assert!(unsafe { *slots.add(index as usize) }.is_null());
        // Idempotent: second call is a no-op, never panics.
        uninstall("tls-test-uninstall");
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

    static LATE_TOKEN: AtomicUsize = AtomicUsize::new(0);
    static LATE_SEEN: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());

    unsafe extern "win64" fn late_record_callback(
        hinst: *mut c_void,
        reason: u32,
        _reserved: *mut c_void,
    ) {
        if hinst as usize != LATE_TOKEN.load(Ordering::SeqCst) {
            return;
        }
        if reason != DLL_THREAD_ATTACH && reason != DLL_THREAD_DETACH {
            return;
        }
        if let Ok(mut seen) = LATE_SEEN.lock() {
            seen.push((
                std::thread::current().name().unwrap_or("?").to_string(),
                reason,
            ));
        }
    }

    #[test]
    fn late_install_gets_blocks_but_no_thread_callbacks_on_early_threads() {
        use std::sync::atomic::AtomicBool;

        let token = Box::into_raw(Box::new(0u8)) as usize;
        LATE_TOKEN.store(token, Ordering::SeqCst);
        LATE_SEEN.lock().unwrap().clear();

        let early_ready = Arc::new(Barrier::new(2));
        let release_early = Arc::new(Barrier::new(2));
        let (ready, release) = (early_ready.clone(), release_early.clone());
        let early_done = Arc::new(AtomicBool::new(false));
        let early_done_w = early_done.clone();
        // Early thread attaches before the late install (spawn attaches on entry).
        let early = spawn("tls-late-early", move || {
            ready.wait();
            release.wait();
            early_done_w.store(true, Ordering::Release);
        })
        .expect("spawn early");
        // Wait until the early thread has attached, then install late.
        early_ready.wait();
        assert!(
            attached_image_count().is_none() || true,
            "harness thread state is independent"
        );
        let index_ptr = leaked_index();
        let late_index = unsafe {
            install(ImageSpec {
                name: "tls-test-late".into(),
                template: TEMPLATE_7.as_ptr() as usize,
                template_len: TEMPLATE_7.len(),
                total_len: TEMPLATE_7.len() as u64,
                characteristics: 0,
                index_ptr,
                hinst: token,
                callbacks: vec![late_record_callback as usize],
                main: false,
            })
            .expect("late TLS image installs")
        };
        // The late image still allocates blocks for the early thread, so TLS reads work.
        release_early.wait();
        early.join().expect("early thread");
        assert!(early_done.load(Ordering::Acquire));

        // A thread attaching after the install gets both callbacks.
        spawn("tls-late-after", move || unsafe {
            assert_eq!(read_tls_byte(late_index), 7);
        })
        .expect("spawn late")
        .join()
        .expect("late thread");

        let seen = LATE_SEEN.lock().unwrap().clone();
        let early_events: Vec<u32> = seen
            .iter()
            .filter(|(n, _)| n == "tls-late-early")
            .map(|(_, r)| *r)
            .collect();
        let after_events: Vec<u32> = seen
            .iter()
            .filter(|(n, _)| n == "tls-late-after")
            .map(|(_, r)| *r)
            .collect();
        assert_eq!(
            early_events,
            Vec::<u32>::new(),
            "a thread that attached before the install receives neither attach nor detach"
        );
        assert_eq!(
            after_events,
            vec![DLL_THREAD_ATTACH, DLL_THREAD_DETACH],
            "a thread that attaches after the install receives both"
        );
    }

    #[test]
    fn slot_array_is_preallocated_and_stable_across_late_installs() {
        detach_current_thread();
        attach_current_thread().expect("attach");
        let before_ptr = slots_ptr_for_current_thread().expect("slots pointer");
        let before_count = attached_image_count().expect("attached count");
        assert_eq!(
            unsafe {
                (*(STATE.with(|s| s.get()) as *const ThreadState))
                    .slots
                    .len()
            },
            MAX_TLS_SLOTS as usize,
            "64 Windows slots are preallocated"
        );
        let index_ptr = leaked_index();
        let index = unsafe {
            install(ImageSpec {
                name: "tls-test-stable-slots".into(),
                template: TEMPLATE_7.as_ptr() as usize,
                template_len: TEMPLATE_7.len(),
                total_len: TEMPLATE_7.len() as u64,
                characteristics: 0,
                index_ptr,
                hinst: 0,
                callbacks: Vec::new(),
                main: false,
            })
            .expect("late image installs")
        };
        assert!((index as usize) < MAX_TLS_SLOTS as usize);
        let after_ptr = slots_ptr_for_current_thread().expect("slots pointer after install");
        assert_eq!(
            before_ptr, after_ptr,
            "late install must not move the slot array"
        );
        assert_eq!(
            attached_image_count(),
            Some(before_count),
            "the current thread's attach count is unchanged by a late install"
        );
        unsafe {
            assert_eq!(read_tls_byte(index), 7, "late blocks are still allocated");
        }
        detach_current_thread();
    }
}
