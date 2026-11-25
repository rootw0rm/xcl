//! Linux dstorage slice (Phase C step 12g): file-read subset of `xcrdapi.dll`.
//!
//! The Windows `hle/dstorage.rs` stays the implementation source of truth for the
//! full console DirectStorage path (worker thread, zlib/BCPack, swizzle, memory
//! sources, error records). On Linux there is no system DirectStorage, so the test
//! title's `DirectStorage:` check gets a synchronous headless subset: `DStorageGetFactory`
//! hands a static factory whose queue executes uncompressed file→memory copies in
//! `Submit` order, plus `DestinationIsPhysicalPages` via the `kernelx_linux` page
//! homes. Compression/BCPack/swizzle/memory-source/physical-source requests fail
//! closed (`E_DECOMPRESSION`/`E_SOURCE_TYPE`); fences signal through the
//! `d3d12x_linux` null-device fence object (same `Signal` slot the Windows path
//! uses). Nothing is rendered; this only moves the title's bytes.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::util::{from_wide_ptr, E_INVALIDARG, E_NOINTERFACE, E_POINTER, HRESULT, S_OK};

const DLL: &str = "xcrdapi.dll";

// DirectStorage HRESULTs (same values as the Windows module).
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as HRESULT;
const E_PENDING: HRESULT = 0x8000_000Au32 as HRESULT;
const E_ABORT: HRESULT = 0x8000_4004u32 as HRESULT;
const E_QUEUE_CAPACITY: HRESULT = 0x8924_0003u32 as HRESULT;
const E_END_OF_FILE: HRESULT = 0x8924_0007u32 as HRESULT;
const E_ACCESS_VIOLATION: HRESULT = 0x8924_0009u32 as HRESULT;
const E_DESTINATION_SIZE: HRESULT = 0x8924_000Fu32 as HRESULT;
const E_QUEUE_CLOSED: HRESULT = 0x8924_0010u32 as HRESULT;
const E_QUEUE_PRIORITY: HRESULT = 0x8924_0013u32 as HRESULT;
const E_FILE_HANDLE: HRESULT = 0x8924_0017u32 as HRESULT;
const E_SOURCE_TYPE: HRESULT = 0x8924_001Bu32 as HRESULT;
const E_DECOMPRESSION: HRESULT = 0x8924_0030u32 as HRESULT;
const E_QUEUE_TYPE: HRESULT = 0x8924_00F5u32 as HRESULT;

/// Console `DSTORAGE_REQUEST_OPTIONS` bits the title uses (Xbox `dstorage_xs.h`):
/// bit 0 `ZlibDecompress`, bits 1–4 `BcpackMode`, bits 5–20 `SwizzleMode`,
/// bit 21 `DestinationIsPhysicalPages`, bit 22 `SourceType` (0 file / 1 memory),
/// bit 23 `SourceIsPhysicalPages`.
const ZLIB_BIT: u64 = 1 << 0;
const BCPACK_MASK: u64 = 0xF << 1;
const SWIZZLE_MASK: u64 = 0xFFFF << 5;
const DEST_PHYS_PAGES_BIT: u64 = 1 << 21;
const SOURCE_MEMORY_BIT: u64 = 1 << 22;
const SOURCE_PHYS_PAGES_BIT: u64 = 1 << 23;
const REQUEST_BYTES: usize = 0x48;
/// `ID3D12Fence::Signal` slot (same as the Windows path).
const FENCE_SIGNAL: usize = 10;
const FENCE_ADDREF: usize = 1;
const FENCE_RELEASE: usize = 2;

// ---------------------------------------------------------------------------------------
// GUIDs (same values as the Windows module; factory + X1 accepted)
// ---------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const fn guid_from_u128(v: u128) -> Guid {
    Guid {
        data1: (v >> 96) as u32,
        data2: (v >> 80) as u16,
        data3: (v >> 64) as u16,
        data4: (v as u64).to_be_bytes(),
    }
}

const IID_IUNKNOWN: Guid = guid_from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_FACTORY: Guid = guid_from_u128(0xfb3b54ac_bf82_463a_b777_73195837864d);
const IID_FILE: Guid = guid_from_u128(0x43704f9f_27c6_4a9f_8815_d16527894df4);
const IID_QUEUE: Guid = guid_from_u128(0xa3f6649a_8c2c_4b3a_8b6e_10186a04b9bc);
const IID_FACTORY_X1: Guid = guid_from_u128(0x62959d11_d45f_4884_a0a3_f7862ea243ae);
const IID_QUEUE_X1: Guid = guid_from_u128(0x3c02cb0c_c050_4195_99a1_a1ce94ba33c5);
const IID_STATUS_ARRAY: Guid = guid_from_u128(0xcd66d752_2786_44f7_9070_e2a6fdad3f1b);

fn same(a: &Guid, b: &Guid) -> bool {
    a == b
}

// ---------------------------------------------------------------------------------------
// vtable helper (never NULL slots: unknown slots are logging stubs)
// ---------------------------------------------------------------------------------------

fn vtable(iface: &str, methods: &[usize], total: usize) -> &'static [usize] {
    let mut v = methods.to_vec();
    for i in methods.len()..total {
        let (stub, _) =
            crate::stubs::make_stub_info(DLL, &format!("{iface}::#{i}"), E_NOTIMPL as u32 as u64);
        v.push(stub);
    }
    Box::leak(v.into_boxed_slice())
}

#[repr(C)]
struct Object<T> {
    vtbl: *const usize,
    refs: AtomicU32,
    inner: Arc<T>,
}

unsafe fn new_object<T>(vtbl: &'static [usize], inner: Arc<T>) -> *mut c_void {
    Box::into_raw(Box::new(Object {
        vtbl: vtbl.as_ptr(),
        refs: AtomicU32::new(1),
        inner,
    })) as *mut c_void
}

unsafe fn inner<'a, T>(p: *mut c_void) -> &'a Arc<T> {
    &(*(p as *const Object<T>)).inner
}

unsafe fn add_ref<T>(p: *mut c_void) -> u32 {
    (*(p as *const Object<T>))
        .refs
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe fn release<T>(p: *mut c_void) -> u32 {
    let n = (*(p as *const Object<T>))
        .refs
        .fetch_sub(1, Ordering::AcqRel)
        - 1;
    if n == 0 {
        drop(Box::from_raw(p as *mut Object<T>));
    }
    n
}

unsafe fn no_ref(_p: *mut c_void) -> u32 {
    1
}

unsafe fn query(
    p: *mut c_void,
    own: &Guid,
    riid: *const Guid,
    out: *mut *mut c_void,
    add: unsafe fn(*mut c_void) -> u32,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if !riid.is_null() && (same(&*riid, own) || same(&*riid, &IID_IUNKNOWN)) {
        add(p);
        *out = p;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

// ---------------------------------------------------------------------------------------
// factory
// ---------------------------------------------------------------------------------------

/// `HRESULT DStorageGetFactory(REFIID riid, void** factory)`
pub unsafe extern "win64" fn DStorageGetFactory(
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    static FACTORY: OnceLock<usize> = OnceLock::new();
    let f = *FACTORY.get_or_init(|| {
        let v = vtable(
            "IDStorageFactoryX",
            &[
                factory_qi as usize,
                static_ref as usize,
                static_ref as usize,
                factory_create_queue as usize,
                factory_open_file as usize,
                factory_create_status_array as usize,
                factory_set_cpu_affinity as usize,
                factory_set_debug_flags as usize,
                factory_set_staging_buffer_size as usize,
                factory_unknown as usize,
                factory_create_queue1 as usize,
            ],
            16,
        );
        Box::into_raw(Box::new(v.as_ptr())) as usize
    });
    factory_qi(f as *mut c_void, riid, out)
}

unsafe extern "win64" fn static_ref(_p: *mut c_void) -> u32 {
    1
}

unsafe extern "win64" fn factory_qi(
    p: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if !riid.is_null() && same(&*riid, &IID_FACTORY_X1) {
        return query(p, &IID_FACTORY_X1, riid, out, no_ref);
    }
    query(p, &IID_FACTORY, riid, out, no_ref)
}

unsafe extern "win64" fn factory_unknown(_p: *mut c_void) -> HRESULT {
    E_NOTIMPL
}

/// Slot 10 of the newer factory: 0x20-byte desc (0x18 of `factory_create_queue`
/// plus a trailing u64); the extra word is accepted and ignored.
unsafe extern "win64" fn factory_create_queue1(
    p: *mut c_void,
    desc: *const u8,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    factory_create_queue(p, desc, riid, out)
}

unsafe extern "win64" fn factory_set_cpu_affinity(_p: *mut c_void, _mask: u64) -> HRESULT {
    S_OK
}

unsafe extern "win64" fn factory_set_debug_flags(_p: *mut c_void, _flags: u32) {}

unsafe extern "win64" fn factory_set_staging_buffer_size(_p: *mut c_void, _bytes: u32) -> HRESULT {
    S_OK
}

// ---------------------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------------------

struct FileInner {
    id: u32,
    path: Vec<u16>,
    file: Mutex<Option<std::fs::File>>,
}

impl FileInner {
    fn handle_locked(&self) -> Option<std::fs::File> {
        self.file
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.try_clone().ok())
            .flatten()
    }
}

fn file_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageFileX",
            &[
                file_qi as usize,
                file_add_ref as usize,
                file_release as usize,
                file_close as usize,
                file_get_handle as usize,
                file_unknown as usize,
                file_unknown as usize,
                file_get_id as usize,
            ],
            12,
        )
    })
}

fn win32_hresult(code: i32) -> HRESULT {
    (0x8007_0000u32 | (code as u32 & 0xFFFF)) as HRESULT
}

unsafe extern "win64" fn factory_open_file(
    _p: *mut c_void,
    path: *const u16,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let Some(name) = from_wide_ptr(path) else {
        return E_INVALIDARG;
    };
    let file = match std::fs::File::open(&name) {
        Ok(f) => f,
        Err(e) => {
            crate::xlog_warn!("DirectStorage: OpenFile '{name}': {e}");
            return e.raw_os_error().map(win32_hresult).unwrap_or(E_INVALIDARG);
        }
    };
    let inner = Arc::new(FileInner {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        path: name.encode_utf16().collect(),
        file: Mutex::new(Some(file)),
    });
    let o = new_object(file_vtbl(), inner);
    let hr = file_qi(o, riid, out);
    file_release(o);
    hr
}

unsafe extern "win64" fn file_qi(
    p: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    query(p, &IID_FILE, riid, out, add_ref::<FileInner>)
}

unsafe extern "win64" fn file_add_ref(p: *mut c_void) -> u32 {
    add_ref::<FileInner>(p)
}

unsafe extern "win64" fn file_release(p: *mut c_void) -> u32 {
    release::<FileInner>(p)
}

unsafe extern "win64" fn file_close(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    inner::<FileInner>(p).file.lock().unwrap().take();
}

unsafe extern "win64" fn file_get_handle(_p: *mut c_void) -> *mut c_void {
    // No host handle to hand out; the title never calls this (fail closed, never NULL crash).
    std::ptr::null_mut()
}

unsafe extern "win64" fn file_unknown(_p: *mut c_void) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "win64" fn file_get_id(p: *mut c_void) -> u32 {
    if p.is_null() {
        return u32::MAX;
    }
    let f = inner::<FileInner>(p);
    if f.file.lock().unwrap().is_some() {
        f.id
    } else {
        u32::MAX
    }
}

// ---------------------------------------------------------------------------------------
// status arrays
// ---------------------------------------------------------------------------------------

struct StatusInner {
    slots: Box<[AtomicI32]>,
}

fn status_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageStatusArrayX",
            &[
                status_qi as usize,
                status_add_ref as usize,
                status_release as usize,
                status_is_complete as usize,
                status_get_hresult as usize,
            ],
            10,
        )
    })
}

unsafe extern "win64" fn factory_create_status_array(
    _p: *mut c_void,
    capacity: u32,
    _name: *const u8,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if capacity == 0 || capacity > 1 << 20 {
        return E_INVALIDARG;
    }
    let inner = Arc::new(StatusInner {
        slots: (0..capacity).map(|_| AtomicI32::new(S_OK)).collect(),
    });
    let o = new_object(status_vtbl(), inner);
    let hr = status_qi(o, riid, out);
    status_release(o);
    hr
}

unsafe extern "win64" fn status_qi(
    p: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    query(p, &IID_STATUS_ARRAY, riid, out, add_ref::<StatusInner>)
}

unsafe extern "win64" fn status_add_ref(p: *mut c_void) -> u32 {
    add_ref::<StatusInner>(p)
}

unsafe extern "win64" fn status_release(p: *mut c_void) -> u32 {
    release::<StatusInner>(p)
}

unsafe extern "win64" fn status_is_complete(p: *mut c_void, index: u32) -> bool {
    if p.is_null() {
        return true;
    }
    inner::<StatusInner>(p)
        .slots
        .get(index as usize)
        .is_some_and(|s| s.load(Ordering::Acquire) != E_PENDING)
}

unsafe extern "win64" fn status_get_hresult(p: *mut c_void, index: u32) -> HRESULT {
    if p.is_null() {
        return E_INVALIDARG;
    }
    inner::<StatusInner>(p)
        .slots
        .get(index as usize)
        .map_or(E_INVALIDARG, |s| s.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Request {
    options: u64,
    destination: u64,
    destination_page_offset: u16,
    destination_size: u32,
    source: u64,
    offset: u64,
    source_page_offset: u16,
    source_size: u32,
    intermediate_size: u32,
    tag: u64,
}

impl Request {
    fn parse(b: &[u8; REQUEST_BYTES]) -> Request {
        let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let d = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let w = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        let options = q(0);
        let dest_phys = options & DEST_PHYS_PAGES_BIT != 0;
        let memory = options & SOURCE_MEMORY_BIT != 0;
        let src_phys = memory && (options & SOURCE_PHYS_PAGES_BIT != 0);
        Request {
            options,
            destination: q(8),
            destination_page_offset: if dest_phys { w(0x10) } else { 0 },
            destination_size: d(0x18),
            source: q(0x20),
            offset: if memory { 0 } else { q(0x28) },
            source_page_offset: if src_phys { w(0x28) } else { 0 },
            source_size: d(0x30),
            intermediate_size: d(0x34),
            tag: q(0x38),
        }
    }

    fn bcpack(&self) -> u8 {
        ((self.options & BCPACK_MASK) >> 1) as u8
    }

    fn swizzle(&self) -> u16 {
        ((self.options & SWIZZLE_MASK) >> 5) as u16
    }

    /// Title verbs only: uncompressed file→memory (or physical pages). Anything else
    /// fails closed so later slices can port it from `hle/dstorage.rs` when needed.
    fn check(&self, source_memory: bool) -> Result<(), HRESULT> {
        if self.options & SOURCE_MEMORY_BIT != 0 || self.options & SOURCE_PHYS_PAGES_BIT != 0 {
            return Err(E_SOURCE_TYPE);
        }
        if source_memory {
            return Err(E_SOURCE_TYPE);
        }
        if self.options & ZLIB_BIT != 0 || self.bcpack() != 0 || self.swizzle() != 0 {
            return Err(E_DECOMPRESSION);
        }
        if self.destination == 0 {
            return Err(E_ACCESS_VIOLATION);
        }
        if self.destination_size != self.source_size
            || (self.intermediate_size != 0 && self.intermediate_size != self.destination_size)
        {
            return Err(E_DESTINATION_SIZE);
        }
        Ok(())
    }
}

/// Snapshot a `PULONG_PTR` physical page array at enqueue time (callers often stack-allocate it).
unsafe fn snapshot_page_array(ptr: u64, offset: u16, byte_len: u32) -> Result<Vec<usize>, HRESULT> {
    if byte_len == 0 {
        return Ok(Vec::new());
    }
    if ptr == 0 {
        return Err(E_ACCESS_VIOLATION);
    }
    let Some(span) = (offset as usize).checked_add(byte_len as usize) else {
        return Err(E_DESTINATION_SIZE);
    };
    const PHYS_PAGE: usize = 0x1_0000;
    let count = span.div_ceil(PHYS_PAGE);
    if count == 0 || count > 1024 {
        return Err(E_DESTINATION_SIZE);
    }
    let mut pages = vec![0usize; count];
    unsafe {
        std::ptr::copy_nonoverlapping(ptr as *const usize, pages.as_mut_ptr(), count);
    }
    Ok(pages)
}

use std::os::unix::fs::FileExt as UnixFileExt;

fn read_exact_at(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<(), HRESULT> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => return Err(E_END_OF_FILE),
            Ok(n) => done += n,
            Err(e) => return Err(e.raw_os_error().map(win32_hresult).unwrap_or(E_ABORT)),
        }
    }
    Ok(())
}

/// Run one validated request; the destination gets the bytes or nothing.
unsafe fn execute(
    r: &Request,
    file: Option<&FileInner>,
    dest_pages: Option<&[usize]>,
) -> Result<(), HRESULT> {
    let n = r.destination_size as usize;
    let f = file.ok_or(E_FILE_HANDLE)?;
    let src = {
        let guard = f.file.lock().unwrap();
        let handle = guard.as_ref().ok_or(E_FILE_HANDLE)?;
        let mut src = vec![0u8; r.source_size as usize];
        read_exact_at(handle, r.offset, &mut src)?;
        src
    };
    if r.dest_phys_pages() {
        let pages = dest_pages.ok_or(E_ACCESS_VIOLATION)?;
        if !crate::hle::kernelx_linux::write_physical_pages(
            pages,
            r.destination_page_offset as usize,
            &src[..n.min(src.len())],
        ) {
            return Err(E_ACCESS_VIOLATION);
        }
        if n != src.len() {
            return Err(E_DESTINATION_SIZE);
        }
    } else {
        if r.destination == 0 {
            return Err(E_ACCESS_VIOLATION);
        }
        if n > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), r.destination as *mut u8, n);
            }
        }
    }
    Ok(())
}

trait DestPhys {
    fn dest_phys_pages(&self) -> bool;
}

impl DestPhys for Request {
    fn dest_phys_pages(&self) -> bool {
        self.options & DEST_PHYS_PAGES_BIT != 0
    }
}

// ---------------------------------------------------------------------------------------
// queues (synchronous Submit)
// ---------------------------------------------------------------------------------------

enum Entry {
    Request {
        req: Request,
        file: Option<Arc<FileInner>>,
        dest_pages: Option<Vec<usize>>,
    },
    /// Refused at enqueue: reported in Submit order (unless cancelled, like a request).
    Refused {
        hr: HRESULT,
        req: Request,
    },
    Status(Arc<StatusInner>, u32),
    /// A fence (one reference held) and the value it gets.
    Signal(usize, u64),
}

struct QueueInner {
    desc: [u8; 0x18],
    source_memory: bool,
    capacity: usize,
    pending: Mutex<Vec<Entry>>,
    closed: AtomicBool,
}

impl QueueInner {
    fn submit(self: &Arc<Self>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let batch = std::mem::take(&mut *self.pending.lock().unwrap());
        if batch.is_empty() {
            return;
        }
        // The first failure of the requests since the last status entry.
        let mut since_status = S_OK;
        for entry in batch {
            if self.closed.load(Ordering::Acquire) {
                if let Entry::Status(s, i) = entry {
                    if let Some(slot) = s.slots.get(i as usize) {
                        slot.store(E_QUEUE_CLOSED, Ordering::Release);
                    }
                }
                continue;
            }
            match entry {
                Entry::Request {
                    req,
                    file,
                    dest_pages,
                } => {
                    if let Err(hr) =
                        unsafe { execute(&req, file.as_deref(), dest_pages.as_deref()) }
                    {
                        if since_status >= 0 {
                            since_status = hr;
                        }
                    }
                }
                Entry::Refused { hr, .. } => {
                    if since_status >= 0 {
                        since_status = hr;
                    }
                }
                Entry::Status(s, i) => {
                    if let Some(slot) = s.slots.get(i as usize) {
                        slot.store(since_status, Ordering::Release);
                    }
                    since_status = S_OK;
                }
                Entry::Signal(fence, value) => unsafe {
                    signal_fence(fence, value);
                },
            }
        }
    }
}

unsafe fn signal_fence(fence: usize, value: u64) {
    if fence == 0 {
        return;
    }
    let vt = unsafe { *(fence as *const *const usize) };
    if vt.is_null() {
        return;
    }
    // Same `Signal` slot the Windows path uses (null-device fence slot 10).
    let signal: unsafe extern "win64" fn(*mut c_void, u64) -> HRESULT =
        unsafe { std::mem::transmute(*vt.add(FENCE_SIGNAL)) };
    signal(fence as *mut c_void, value);
    let release: unsafe extern "win64" fn(*mut c_void) -> u32 =
        unsafe { std::mem::transmute(*vt.add(FENCE_RELEASE)) };
    release(fence as *mut c_void);
}

fn queue_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageQueueX",
            &[
                queue_qi as usize,
                queue_add_ref as usize,
                queue_release as usize,
                queue_enqueue_request as usize,
                queue_enqueue_status as usize,
                queue_enqueue_signal as usize,
                queue_submit as usize,
                queue_cancel as usize,
                queue_close as usize,
                queue_get_error_event as usize,
                queue_retrieve_error_record as usize,
                queue_query as usize,
            ],
            18,
        )
    })
}

unsafe extern "win64" fn factory_create_queue(
    _p: *mut c_void,
    desc: *const u8,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if desc.is_null() {
        return E_INVALIDARG;
    }
    let raw: [u8; 0x18] = unsafe { (desc as *const [u8; 0x18]).read_unaligned() };
    let source = u32::from_le_bytes(raw[0..4].try_into().unwrap());
    let capacity = u16::from_le_bytes(raw[8..10].try_into().unwrap()) as usize;
    let priority = raw[10] as i8;
    if source > 1 {
        return E_QUEUE_TYPE;
    }
    if !(-1..=2).contains(&priority) || (source == 1 && priority != 2) {
        return E_QUEUE_PRIORITY;
    }
    if capacity < 2 {
        return E_QUEUE_CAPACITY;
    }
    let inner = Arc::new(QueueInner {
        desc: raw,
        source_memory: source == 1,
        capacity,
        pending: Mutex::new(Vec::new()),
        closed: AtomicBool::new(false),
    });
    let o = new_object(queue_vtbl(), inner);
    let hr = queue_qi(o, riid, out);
    queue_release(o);
    hr
}

unsafe fn queue_of(p: *mut c_void) -> Arc<QueueInner> {
    Arc::clone(inner::<QueueInner>(p))
}

unsafe extern "win64" fn queue_qi(
    p: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if !riid.is_null() && same(&*riid, &IID_QUEUE_X1) {
        return query(p, &IID_QUEUE_X1, riid, out, add_ref::<QueueInner>);
    }
    query(p, &IID_QUEUE, riid, out, add_ref::<QueueInner>)
}

unsafe extern "win64" fn queue_add_ref(p: *mut c_void) -> u32 {
    add_ref::<QueueInner>(p)
}

unsafe extern "win64" fn queue_release(p: *mut c_void) -> u32 {
    release::<QueueInner>(p)
}

unsafe extern "win64" fn queue_enqueue_request(p: *mut c_void, req: *const u8) {
    if p.is_null() || req.is_null() {
        return;
    }
    let q = queue_of(p);
    if q.closed.load(Ordering::Acquire) {
        return;
    }
    let bytes = unsafe { *(req as *const [u8; REQUEST_BYTES]) };
    let r = Request::parse(&bytes);
    if let Err(hr) = r.check(q.source_memory) {
        q.pending
            .lock()
            .unwrap()
            .push(Entry::Refused { hr, req: r });
        maybe_auto_submit(&q);
        return;
    }
    let file = if r.options & SOURCE_MEMORY_BIT != 0 {
        None
    } else {
        let f = r.source as *mut c_void;
        if f.is_null() {
            q.pending.lock().unwrap().push(Entry::Refused {
                hr: E_FILE_HANDLE,
                req: r,
            });
            maybe_auto_submit(&q);
            return;
        }
        let vt = unsafe { *(f as *const *const usize) };
        if vt != file_vtbl().as_ptr() {
            q.pending.lock().unwrap().push(Entry::Refused {
                hr: E_FILE_HANDLE,
                req: r,
            });
            maybe_auto_submit(&q);
            return;
        }
        Some(unsafe { Arc::clone(inner::<FileInner>(f)) })
    };
    let dest_pages = if r.dest_phys_pages() {
        match unsafe {
            snapshot_page_array(r.destination, r.destination_page_offset, r.destination_size)
        } {
            Ok(pages) => Some(pages),
            Err(hr) => {
                q.pending
                    .lock()
                    .unwrap()
                    .push(Entry::Refused { hr, req: r });
                maybe_auto_submit(&q);
                return;
            }
        }
    } else {
        None
    };
    q.pending.lock().unwrap().push(Entry::Request {
        req: r,
        file,
        dest_pages,
    });
    maybe_auto_submit(&q);
}

fn maybe_auto_submit(q: &Arc<QueueInner>) {
    // As on the console: a half-full queue submits by itself.
    let n = q.pending.lock().unwrap().len();
    if n >= (q.capacity / 2).max(1) {
        q.submit();
    }
}

unsafe extern "win64" fn queue_enqueue_status(p: *mut c_void, array: *mut c_void, index: u32) {
    if p.is_null() || array.is_null() {
        return;
    }
    let q = queue_of(p);
    if q.closed.load(Ordering::Acquire) {
        return;
    }
    let vt = unsafe { *(array as *const *const usize) };
    if vt != status_vtbl().as_ptr() {
        return;
    }
    let s = unsafe { Arc::clone(inner::<StatusInner>(array)) };
    if let Some(slot) = s.slots.get(index as usize) {
        slot.store(E_PENDING, Ordering::Release);
    } else {
        return;
    }
    q.pending.lock().unwrap().push(Entry::Status(s, index));
    maybe_auto_submit(&q);
}

unsafe extern "win64" fn queue_enqueue_signal(p: *mut c_void, fence: *mut c_void, value: u64) {
    if p.is_null() || fence.is_null() {
        return;
    }
    let q = queue_of(p);
    if q.closed.load(Ordering::Acquire) {
        return;
    }
    let vt = unsafe { *(fence as *const *const usize) };
    if vt.is_null() {
        return;
    }
    // Hold one fence reference across the queued signal (released after signalling).
    let add: unsafe extern "win64" fn(*mut c_void) -> u32 =
        unsafe { std::mem::transmute(*vt.add(FENCE_ADDREF)) };
    add(fence);
    q.pending
        .lock()
        .unwrap()
        .push(Entry::Signal(fence as usize, value));
    maybe_auto_submit(&q);
}

unsafe extern "win64" fn queue_submit(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    queue_of(p).submit();
}

/// Requests enqueued before this call whose tag matches (`tag & mask == value`)
/// are skipped without error; their destinations stay untouched.
unsafe extern "win64" fn queue_cancel(p: *mut c_void, mask: u64, value: u64) {
    if p.is_null() {
        return;
    }
    let q = queue_of(p);
    let mut pend = q.pending.lock().unwrap();
    pend.retain(|e| !matches!(e, Entry::Request { req, .. } | Entry::Refused { req, .. } if req.tag & mask == value));
}

unsafe extern "win64" fn queue_close(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let q = queue_of(p);
    q.closed.store(true, Ordering::Release);
    let drained = std::mem::take(&mut *q.pending.lock().unwrap());
    for e in drained {
        match e {
            Entry::Status(s, i) => {
                if let Some(slot) = s.slots.get(i as usize) {
                    slot.store(E_QUEUE_CLOSED, Ordering::Release);
                }
            }
            Entry::Signal(fence, _) => {
                // Drop the held reference without signalling.
                let vt = unsafe { *(fence as *const *const usize) };
                if !vt.is_null() {
                    let release: unsafe extern "win64" fn(*mut c_void) -> u32 =
                        unsafe { std::mem::transmute(*vt.add(FENCE_RELEASE)) };
                    release(fence as *mut c_void);
                }
            }
            _ => {}
        }
    }
}

unsafe extern "win64" fn queue_get_error_event(_p: *mut c_void) -> *mut c_void {
    // Fail closed: no host event to hand out (nothing calls this in the title).
    std::ptr::null_mut()
}

unsafe extern "win64" fn queue_retrieve_error_record(_p: *mut c_void, _out: *mut u8) {
    // Fail closed: error records stay Phase-D work (nothing calls this in the title).
}

unsafe extern "win64" fn queue_query(_p: *mut c_void, _out: *mut u8) {
    // Fail closed: queue-info stays a stub (nothing calls this in the title).
}

pub fn exports() -> Vec<(&'static str, usize)> {
    vec![("DStorageGetFactory", DStorageGetFactory as usize)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(obj: *mut c_void, i: usize) -> usize {
        unsafe { *(*(obj as *const *const usize)).add(i) }
    }

    unsafe fn make_queue(capacity: u16) -> *mut c_void {
        let mut factory: *mut c_void = std::ptr::null_mut();
        assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut factory), S_OK);
        assert!(!factory.is_null());
        type Create = unsafe extern "win64" fn(
            *mut c_void,
            *const u8,
            *const Guid,
            *mut *mut c_void,
        ) -> HRESULT;
        let create: Create = std::mem::transmute(slot(factory, 3));
        let mut desc = [0u8; 0x18];
        desc[8..10].copy_from_slice(&capacity.to_le_bytes());
        let mut q: *mut c_void = std::ptr::null_mut();
        assert_eq!(create(factory, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
        assert!(!q.is_null());
        q
    }

    unsafe fn make_status(factory: *mut c_void, capacity: u32) -> *mut c_void {
        type Status = unsafe extern "win64" fn(
            *mut c_void,
            u32,
            *const u8,
            *const Guid,
            *mut *mut c_void,
        ) -> HRESULT;
        let create: Status = std::mem::transmute(slot(factory, 5));
        let mut s: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            create(
                factory,
                capacity,
                std::ptr::null(),
                &IID_STATUS_ARRAY,
                &mut s
            ),
            S_OK
        );
        assert!(!s.is_null());
        s
    }

    #[test]
    fn factory_qi_accepts_factory_and_x1() {
        unsafe {
            let mut f: *mut c_void = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            assert!(!f.is_null());
            let mut fx: *mut c_void = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY_X1, &mut fx), S_OK);
            assert!(!fx.is_null());
            // Unknown IID is refused with a cleared out pointer.
            let unknown = guid_from_u128(0xdead_beef_0000_4000_8000_001122334455);
            let mut p = 1usize as *mut c_void;
            assert_eq!(DStorageGetFactory(&unknown, &mut p), E_NOINTERFACE);
            assert!(p.is_null());
            // NULL out fails closed.
            assert_eq!(
                DStorageGetFactory(&IID_FACTORY, std::ptr::null_mut()),
                E_POINTER
            );
            // Factory vtable: title slots are real, never NULL.
            for i in 0..16 {
                assert_ne!(slot(f, i), 0, "factory slot {i} must not be NULL");
            }
        }
    }

    #[test]
    fn open_missing_and_status_bounds_fail_closed() {
        unsafe {
            let mut factory: *mut c_void = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut factory), S_OK);
            type Open = unsafe extern "win64" fn(
                *mut c_void,
                *const u16,
                *const Guid,
                *mut *mut c_void,
            ) -> HRESULT;
            let open: Open = std::mem::transmute(slot(factory, 4));
            let missing: Vec<u16> = "/tmp/xcl-dstorage-definitely-missing-12345.bin\0"
                .encode_utf16()
                .collect();
            let mut file: *mut c_void = std::ptr::null_mut();
            let hr = open(factory, missing.as_ptr(), &IID_FILE, &mut file);
            assert_ne!(hr, S_OK);
            assert!(file.is_null());
            // NULL out / NULL path fail closed.
            assert_eq!(
                open(factory, missing.as_ptr(), &IID_FILE, std::ptr::null_mut()),
                E_POINTER
            );
            assert_eq!(
                open(factory, std::ptr::null(), &IID_FILE, &mut file),
                E_INVALIDARG
            );
            // Status array bounds: 0 and huge capacities refuse.
            type Status = unsafe extern "win64" fn(
                *mut c_void,
                u32,
                *const u8,
                *const Guid,
                *mut *mut c_void,
            ) -> HRESULT;
            let create: Status = std::mem::transmute(slot(factory, 5));
            let mut s: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                create(factory, 0, std::ptr::null(), &IID_STATUS_ARRAY, &mut s),
                E_INVALIDARG
            );
            assert!(s.is_null());
            assert_eq!(
                create(
                    factory,
                    1 << 21,
                    std::ptr::null(),
                    &IID_STATUS_ARRAY,
                    &mut s
                ),
                E_INVALIDARG
            );
            // Out-of-range status index fails closed.
            let s = make_status(factory, 1);
            type GetHr = unsafe extern "win64" fn(*mut c_void, u32) -> HRESULT;
            let get: GetHr = std::mem::transmute(slot(s, 4));
            assert_eq!(get(s, 7), E_INVALIDARG);
            type Complete = unsafe extern "win64" fn(*mut c_void, u32) -> bool;
            let complete: Complete = std::mem::transmute(slot(s, 3));
            assert!(complete(s, 0));
        }
    }

    #[test]
    fn file_queue_status_signal_submit_flow() {
        unsafe {
            // A temp file with known bytes stands in for the title executable.
            let dir = std::env::temp_dir().join(format!(
                "xcl-dstorage-{}-{}",
                std::process::id(),
                line!()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("blob.bin");
            let want: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
            std::fs::write(&path, &want).unwrap();
            let wpath: Vec<u16> = path.to_string_lossy().encode_utf16().chain([0]).collect();

            let mut factory: *mut c_void = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut factory), S_OK);
            type Open = unsafe extern "win64" fn(
                *mut c_void,
                *const u16,
                *const Guid,
                *mut *mut c_void,
            ) -> HRESULT;
            let open: Open = std::mem::transmute(slot(factory, 4));
            let mut file: *mut c_void = std::ptr::null_mut();
            assert_eq!(open(factory, wpath.as_ptr(), &IID_FILE, &mut file), S_OK);
            assert!(!file.is_null());
            // File vtable: QI/AddRef/Release/Close/GetId real, handle fail-closed, never NULL.
            type GetId = unsafe extern "win64" fn(*mut c_void) -> u32;
            let get_id: GetId = std::mem::transmute(slot(file, 7));
            assert_ne!(get_id(file), u32::MAX);
            for i in 0..12 {
                assert_ne!(slot(file, i), 0, "file slot {i} must not be NULL");
            }

            let q = make_queue(16);
            let s = make_status(factory, 1);
            // Enqueue file→memory copy + status + fence signal, then Submit.
            let mut dst = vec![0u8; 512];
            let mut req = [0u8; REQUEST_BYTES];
            req[8..16].copy_from_slice(&(dst.as_mut_ptr() as u64).to_le_bytes());
            req[0x18..0x1C].copy_from_slice(&512u32.to_le_bytes());
            req[0x20..0x28].copy_from_slice(&(file as u64).to_le_bytes());
            req[0x30..0x34].copy_from_slice(&512u32.to_le_bytes());
            type EnqReq = unsafe extern "win64" fn(*mut c_void, *const u8);
            type EnqStatus = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32);
            type EnqSignal = unsafe extern "win64" fn(*mut c_void, *mut c_void, u64);
            type Submit = unsafe extern "win64" fn(*mut c_void);
            let enq: EnqReq = std::mem::transmute(slot(q, 3));
            let enq_st: EnqStatus = std::mem::transmute(slot(q, 4));
            let enq_sig: EnqSignal = std::mem::transmute(slot(q, 5));
            let submit: Submit = std::mem::transmute(slot(q, 6));
            enq(q, req.as_ptr());
            enq_st(q, s, 0);
            // A fake fence object with the null-device layout (slots 1/2/10 real).
            let fence = fake_fence();
            enq_sig(q, fence, 1);
            submit(q);
            type GetHr = unsafe extern "win64" fn(*mut c_void, u32) -> HRESULT;
            let get: GetHr = std::mem::transmute(slot(s, 4));
            assert_eq!(get(s, 0), S_OK);
            assert_eq!(&dst[..], &want[..]);
            assert_eq!(fake_value(fence), 1);
            // Compressed / swizzle / memory-source requests fail closed.
            let mut req_z = req;
            req_z[0] = 1; // ZlibDecompress
            enq(q, req_z.as_ptr());
            enq_st(q, s, 0);
            submit(q);
            assert_eq!(get(s, 0), E_DECOMPRESSION);
            let mut req_mem = req;
            req_mem[0..8].copy_from_slice(&(SOURCE_MEMORY_BIT).to_le_bytes());
            enq(q, req_mem.as_ptr());
            enq_st(q, s, 0);
            submit(q);
            assert_eq!(get(s, 0), E_SOURCE_TYPE);

            // Release clamp: static factory stays callable; queue/file/status release.
            type Release = unsafe extern "win64" fn(*mut c_void) -> u32;
            let release: Release = std::mem::transmute(slot(q, 2));
            let _ = release(q);
            let release_f: Release = std::mem::transmute(slot(file, 2));
            let _ = release_f(file);
            let release_s: Release = std::mem::transmute(slot(s, 2));
            let _ = release_s(s);
            fake_free(fence);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn cancel_filters_tagged_request_and_phys_pages_work() {
        unsafe {
            let dir = std::env::temp_dir().join(format!(
                "xcl-dstorage-cancel-{}-{}",
                std::process::id(),
                line!()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("blob.bin");
            let want: Vec<u8> = (0..512u32).map(|i| (i * 7 % 251) as u8).collect();
            std::fs::write(&path, &want).unwrap();
            let wpath: Vec<u16> = path.to_string_lossy().encode_utf16().chain([0]).collect();

            let mut factory: *mut c_void = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut factory), S_OK);
            type Open = unsafe extern "win64" fn(
                *mut c_void,
                *const u16,
                *const Guid,
                *mut *mut c_void,
            ) -> HRESULT;
            let open: Open = std::mem::transmute(slot(factory, 4));
            let mut file: *mut c_void = std::ptr::null_mut();
            assert_eq!(open(factory, wpath.as_ptr(), &IID_FILE, &mut file), S_OK);

            // One physical page for the DestinationIsPhysicalPages read.
            let mut count: usize = 1;
            let mut page = [0u64; 1];
            assert_eq!(
                crate::hle::kernelx_linux::XMemAllocatePhysicalPages(
                    0,
                    &mut count,
                    page.as_mut_ptr()
                ),
                1
            );
            let page_table = [page[0] as usize];

            // Use a large queue so nothing auto-submits before the cancel.
            let q = make_queue(64);
            let s = make_status(factory, 1);
            type EnqReq = unsafe extern "win64" fn(*mut c_void, *const u8);
            type EnqStatus = unsafe extern "win64" fn(*mut c_void, *mut c_void, u32);
            type Submit = unsafe extern "win64" fn(*mut c_void);
            type Cancel = unsafe extern "win64" fn(*mut c_void, u64, u64);
            let enq: EnqReq = std::mem::transmute(slot(q, 3));
            let enq_st: EnqStatus = std::mem::transmute(slot(q, 4));
            let submit: Submit = std::mem::transmute(slot(q, 6));
            let cancel: Cancel = std::mem::transmute(slot(q, 7));

            let mut req_phys = [0u8; REQUEST_BYTES];
            req_phys[0..8].copy_from_slice(&(DEST_PHYS_PAGES_BIT).to_le_bytes());
            req_phys[8..16].copy_from_slice(&(page_table.as_ptr() as u64).to_le_bytes());
            req_phys[0x10..0x12].copy_from_slice(&0x80u16.to_le_bytes());
            req_phys[0x18..0x1C].copy_from_slice(&512u32.to_le_bytes());
            req_phys[0x20..0x28].copy_from_slice(&(file as u64).to_le_bytes());
            req_phys[0x30..0x34].copy_from_slice(&512u32.to_le_bytes());
            let mut cancelled_dst = vec![0u8; 512];
            let mut req_cancel = [0u8; REQUEST_BYTES];
            req_cancel[8..16].copy_from_slice(&(cancelled_dst.as_mut_ptr() as u64).to_le_bytes());
            req_cancel[0x18..0x1C].copy_from_slice(&512u32.to_le_bytes());
            req_cancel[0x20..0x28].copy_from_slice(&(file as u64).to_le_bytes());
            req_cancel[0x30..0x34].copy_from_slice(&512u32.to_le_bytes());
            req_cancel[0x38..0x40].copy_from_slice(&0xCAFE_0001u64.to_le_bytes());
            enq(q, req_phys.as_ptr());
            enq(q, req_cancel.as_ptr());
            cancel(q, 0xFFFF_0000, 0xCAFE_0000);
            enq_st(q, s, 0);
            submit(q);
            type GetHr = unsafe extern "win64" fn(*mut c_void, u32) -> HRESULT;
            let get: GetHr = std::mem::transmute(slot(s, 4));
            assert_eq!(get(s, 0), S_OK);
            assert!(
                cancelled_dst.iter().all(|&b| b == 0),
                "cancelled request must not write"
            );
            // Physical pages hold the file bytes at offset 0x80.
            let mut back = vec![0u8; 512];
            assert!(crate::hle::kernelx_linux::read_physical_pages(
                &page_table,
                0x80,
                &mut back
            ));
            assert_eq!(&back[..], &want[..]);

            type Release = unsafe extern "win64" fn(*mut c_void) -> u32;
            let release: Release = std::mem::transmute(slot(q, 2));
            let _ = release(q);
            let release_f: Release = std::mem::transmute(slot(file, 2));
            let _ = release_f(file);
            let release_s: Release = std::mem::transmute(slot(s, 2));
            let _ = release_s(s);
            assert_eq!(
                crate::hle::kernelx_linux::XMemFreePhysicalPages(1, page.as_mut_ptr()),
                1
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // A minimal fence object with the null-device fence layout (slots 1/2/10 real).
    struct FakeFence {
        vtbl: *const usize,
        refs: AtomicU32,
        value: std::sync::atomic::AtomicU64,
    }

    unsafe extern "win64" fn fake_add(_p: *mut c_void) -> u32 {
        1
    }
    unsafe extern "win64" fn fake_release(_p: *mut c_void) -> u32 {
        1
    }
    unsafe extern "win64" fn fake_signal(p: *mut c_void, v: u64) -> HRESULT {
        let f = &*(p as *const FakeFence);
        f.value.store(v, Ordering::Release);
        S_OK
    }

    unsafe fn fake_fence() -> *mut c_void {
        static VT: OnceLock<Vec<usize>> = OnceLock::new();
        let vt = VT.get_or_init(|| {
            let mut v = vec![0usize; 11];
            v[FENCE_ADDREF] = fake_add as usize;
            v[FENCE_RELEASE] = fake_release as usize;
            v[FENCE_SIGNAL] = fake_signal as usize;
            v
        });
        let f = Box::new(FakeFence {
            vtbl: vt.as_ptr(),
            refs: AtomicU32::new(1),
            value: std::sync::atomic::AtomicU64::new(0),
        });
        Box::into_raw(f) as *mut c_void
    }

    unsafe fn fake_value(f: *mut c_void) -> u64 {
        unsafe { (*(f as *const FakeFence)).value.load(Ordering::Acquire) }
    }

    unsafe fn fake_free(f: *mut c_void) {
        unsafe { drop(Box::from_raw(f as *mut FakeFence)) };
    }
}
