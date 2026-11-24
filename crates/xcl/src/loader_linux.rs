//! Linux PE loader: maps console images at their PE link address with
//! `mmap(MAP_FIXED_NOREPLACE)`.
//!
//! Semantics kept identical to `loader.rs` (Windows): the image is mapped at its
//! `ImageBase`, headers and raw section bytes are copied, base relocations are applied (a
//! no-op while the base equals the link address), the import and delay-import tables are
//! parsed with the same name/ordinal rules, the TLS directory's callbacks are recorded in
//! order, and the entry RVA is preserved. An occupied link address is a hard error: there is
//! no relocation-based fallback, because the title's pointers assume the link address.
//!
//! Not ported yet (explicit Phase C/C steps, no silent stubs):
//! * import resolution/HLE/auto-stubs — step 10; the IAT stays empty for now,
//! * static TLS block installation and callbacks — step 7; the directory is parsed here,
//! * `.pdata` registration / signal handling — step 9,
//! * running DllMain/entry (Win64 ABI) — needs the Phase C host.

use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use crate::modules::{self, MappedModule};
use crate::pe::{self, Layout, OwnedExports, Pe};

/// `MAP_FIXED_NOREPLACE` (Linux 4.17+). Not exposed by this `libc` version for plain Linux,
/// and it must be a build-time constant of the kernel ABI.
const MAP_FIXED_NOREPLACE: libc::c_int = 0x10_0000;

static PAGE_SIZE: LazyLock<usize> = LazyLock::new(|| {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 {
        n as usize
    } else {
        4096
    }
});

fn page_size() -> usize {
    *PAGE_SIZE
}

fn round_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

fn section_protection(ch: u32) -> libc::c_int {
    let x = ch & pe::SCN_MEM_EXECUTE != 0;
    let r = ch & pe::SCN_MEM_READ != 0;
    let w = ch & pe::SCN_MEM_WRITE != 0;
    match (x, r, w) {
        (true, _, true) => libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        (true, true, false) => libc::PROT_READ | libc::PROT_EXEC,
        (true, false, false) => libc::PROT_EXEC,
        (false, _, true) => libc::PROT_READ | libc::PROT_WRITE,
        (false, true, false) => libc::PROT_READ,
        (false, false, false) => libc::PROT_NONE,
    }
}

/// Owns the anonymous mapping until the loader hands it to a `MappedModule`.
struct Mapping {
    base: *mut c_void,
    len: usize,
    armed: bool,
}

impl Mapping {
    fn new(base_addr: usize, len: usize) -> Result<Self, String> {
        let p = unsafe {
            libc::mmap(
                base_addr as *mut c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EEXIST) {
                return Err(format!(
                    "preferred address {base_addr:#x} is occupied ({e}); the image is mapped only at its link address (no relocation fallback)"
                ));
            }
            return Err(format!(
                "cannot map preferred address {base_addr:#x} ({len:#x} bytes): {e}"
            ));
        }
        Ok(Mapping {
            base: p,
            len,
            armed: true,
        })
    }

    fn addr(&self) -> usize {
        self.base as usize
    }

    /// The module owns the mapping from here on; `Drop` must not unmap it.
    fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                libc::munmap(self.base, self.len);
            }
        }
    }
}

/// Map an image at its link address. Does not run any code of the image.
pub fn map_image(path: &Path) -> Result<Arc<MappedModule>, String> {
    map_image_ex(path, false)
}

/// Map the title's main executable and register it as the main image.
pub fn load_main(path: &Path) -> Result<Arc<MappedModule>, String> {
    map_image_ex(path, true)
}

fn map_image_ex(path: &Path, is_main: bool) -> Result<Arc<MappedModule>, String> {
    let file = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let pe = Pe::parse(&file, Layout::File).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = crate::util::dll_key(
        &path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let size = pe.size_of_image as usize;
    if size == 0 {
        return Err(format!("{name}: SizeOfImage is 0"));
    }
    let page = page_size();
    let base_addr = pe.image_base as usize;
    if base_addr % page != 0 {
        return Err(format!(
            "{name}: image base {base_addr:#x} is not page aligned"
        ));
    }
    if base_addr.checked_add(size).is_none() {
        return Err(format!(
            "{name}: image base {base_addr:#x} + size {size:#x} overflows"
        ));
    }
    let map_len = round_up(size, page);

    // 1. reserve + commit at the link address. No fallback: a relocated image would break
    // every pointer the title holds. The anonymous mapping is zero-filled, like a PE's
    // zero-filled section tails.
    let mapping = Mapping::new(base_addr, map_len).map_err(|e| format!("{name}: {e}"))?;
    let base = mapping.addr();
    debug_assert_eq!(base, base_addr);

    // 2. headers + sections
    unsafe {
        let hdr = (pe.size_of_headers as usize).min(file.len()).min(size);
        std::ptr::copy_nonoverlapping(file.as_ptr(), base as *mut u8, hdr);
        for s in &pe.sections {
            let raw = (s.raw_size as usize).min(s.mapped_size() as usize);
            if raw == 0 {
                continue;
            }
            let src = s.raw_ptr as usize;
            if src + raw > file.len() || s.va as usize + raw > size {
                return Err(format!("{name}: section {} outside the file/image", s.name));
            }
            std::ptr::copy_nonoverlapping(
                file.as_ptr().add(src),
                (base + s.va as usize) as *mut u8,
                raw,
            );
        }
    }

    // 3. relocations. The image is at its link address, so this is a no-op today; the same
    // fixups run should a future backend map elsewhere (never silently: that is an error).
    let delta = base as i64 - pe.image_base as i64;
    apply_relocations(&pe, base, delta).map_err(|e| format!("{name}: {e}"))?;

    let exports = OwnedExports::from_pe(&pe).unwrap_or_default();

    // 4. imports (normal + delay-load). Parse with the same parser as Windows first: a broken
    // table must fail the load. Resolution is step 10 and has no business faking addresses.
    let imports = pe
        .imports()
        .map_err(|e| format!("{name}: import table: {e}"))?;
    let delay = match pe.delay_imports() {
        Ok(v) => v,
        Err(e) => {
            crate::xlog_warn!("{name}: delay-imports: {e}");
            Vec::new()
        }
    };
    let import_entries: usize = imports.iter().map(|d| d.entries.len()).sum();
    let delay_entries: usize = delay.iter().map(|d| d.entries.len()).sum();
    if !imports.is_empty() || !delay.is_empty() {
        // LINUX-TODO: Phase C step-10 — resolve imports through the HLE registry/game DLLs
        // and write the IAT slots; keep name/ordinal/delay ordering identical.
        crate::xlog_info!(
            "{name}: import tables parsed ({} DLLs/{} functions, delay {} DLLs/{} functions); resolution is Phase C step-10",
            imports.len(),
            import_entries,
            delay.len(),
            delay_entries
        );
    }

    // 5. static TLS: keep the directory's callbacks in order, then (for the main image)
    // install the per-thread blocks. DLL TLS install follows the game-DLL load path —
    // LINUX-TODO: Phase C step-10.
    let mut tls_callbacks = Vec::new();
    let mut tls_dir = None;
    match pe.tls() {
        Ok(Some(t)) => {
            if t.address_of_callbacks != 0 {
                let cb_rva = checked_rva(&pe, t.address_of_callbacks, 8, "TLS callbacks")?;
                let mut at = cb_rva as usize;
                loop {
                    if at + 8 > size {
                        return Err(format!(
                            "{name}: TLS callback list is not terminated inside the image"
                        ));
                    }
                    let cb = unsafe { std::ptr::read_unaligned((base + at) as *const u64) };
                    if cb == 0 {
                        break;
                    }
                    if pe
                        .va_to_rva(cb)
                        .map(|r| r as usize)
                        .filter(|r| *r < size)
                        .is_none()
                    {
                        return Err(format!("{name}: TLS callback {cb:#x} outside the image"));
                    }
                    tls_callbacks.push(cb as usize);
                    at += 8;
                }
            }
            tls_dir = Some(t);
        }
        Ok(None) => {}
        Err(e) => return Err(format!("{name}: TLS directory: {e}")),
    }

    let tls_count = tls_callbacks.len();
    if let Some(t) = tls_dir {
        let template_len = t.template_len() as usize;
        let template = if template_len == 0 {
            0
        } else {
            let rva = checked_rva(&pe, t.start_raw, template_len, "TLS template")?;
            base + rva as usize
        };
        let index_ptr = if t.address_of_index == 0 {
            0
        } else {
            let rva = checked_rva(&pe, t.address_of_index, 4, "TLS index")?;
            base + rva as usize
        };
        crate::xlog_info!(
            "{name}: TLS directory: template {template_len} B, zero-fill {} B, {tls_count} callbacks",
            t.size_of_zero_fill
        );
        if is_main {
            unsafe {
                crate::tls::install(crate::tls::ImageSpec {
                    name: name.clone(),
                    template,
                    template_len,
                    total_len: t.total_len(),
                    index_ptr,
                    hinst: base,
                    callbacks: tls_callbacks.clone(),
                    main: true,
                })
            }
            .map_err(|e| format!("{name}: static TLS: {e}"))?;
        } else {
            // LINUX-TODO: Phase C step-10 — the game-DLL load path (DllMain/init_dll) does
            // not exist yet; install the DLL's blocks there, before running its code.
            crate::xlog_info!(
                "{name}: non-main image: static TLS installs on the game-DLL load path (Phase C step-10)"
            );
        }
    }

    let module = Arc::new(MappedModule {
        name: name.clone(),
        path: path.to_path_buf(),
        base,
        size,
        entry_rva: pe.entry_rva,
        is_dll: pe.is_dll(),
        exports,
        tls_callbacks: Mutex::new(tls_callbacks),
        no_thread_calls: std::sync::atomic::AtomicBool::new(false),
        initialized: std::sync::atomic::AtomicBool::new(false),
        stack_reserve: pe.stack_reserve,
    });
    mapping.keep();
    modules::register_mapped(module.clone());
    if is_main {
        modules::set_main(module.clone());
    }
    crate::xlog_info!(
        "loaded {name} @ {base:#x} (size {size:#x}, preferred {:#x})",
        pe.image_base
    );

    // 6. page protections (headers read-only, sections from their characteristics) and
    // transparent huge pages for large sections.
    protect_image(&pe, base, size);

    // 7. exception data: LINUX-TODO: Phase C step-09 — translate `.pdata` for the signal
    // unwinder instead of passing it to __register_frame.
    let ex = pe.dir(pe::DIR_EXCEPTION);
    if ex.rva != 0 && ex.size != 0 {
        crate::xlog_debug!(
            "{name}: .pdata {} B at {:#x} (signal unwind is Phase C step-09)",
            ex.size,
            ex.rva
        );
    }
    Ok(module)
}

/// Apply base relocations for a non-zero delta, mirroring `loader.rs`.
fn apply_relocations(pe: &Pe<'_>, base: usize, delta: i64) -> Result<(), String> {
    if delta == 0 {
        return Ok(());
    }
    let relocs = pe.relocations().map_err(|e| format!("relocations: {e}"))?;
    for r in relocs {
        let width = match r.kind {
            pe::IMAGE_REL_BASED_DIR64 => 8,
            pe::IMAGE_REL_BASED_HIGHLOW => 4,
            k => {
                crate::xlog_warn!("unsupported relocation type {k} @ {:#x}", r.rva);
                continue;
            }
        };
        if r.rva as usize + width > pe.size_of_image as usize {
            return Err(format!("relocation RVA {:#x} outside the image", r.rva));
        }
        let p = base + r.rva as usize;
        unsafe {
            match r.kind {
                pe::IMAGE_REL_BASED_DIR64 => {
                    let v = std::ptr::read_unaligned(p as *const u64);
                    std::ptr::write_unaligned(p as *mut u64, (v as i64 + delta) as u64);
                }
                pe::IMAGE_REL_BASED_HIGHLOW => {
                    let v = std::ptr::read_unaligned(p as *const u32);
                    std::ptr::write_unaligned(p as *mut u32, (v as i64 + delta) as u32);
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

fn checked_rva(pe: &Pe<'_>, va: u64, len: usize, what: &str) -> Result<u32, String> {
    let rva = pe
        .va_to_rva(va)
        .ok_or_else(|| format!("{what} address {va:#x} outside the image"))?;
    if rva as usize + len > pe.size_of_image as usize {
        return Err(format!("{what} address {va:#x} outside the image"));
    }
    Ok(rva)
}

fn mprotect(base: usize, len: usize, prot: libc::c_int) {
    if len == 0 {
        return;
    }
    // mprotect rounds the length up to whole pages; the address must already be aligned.
    let r = unsafe { libc::mprotect(base as *mut c_void, len, prot) };
    if r != 0 {
        crate::xlog_warn!(
            "mprotect({base:#x}, {len:#x}, {prot:#x}): {}",
            std::io::Error::last_os_error()
        );
    }
}

fn protect_image(pe: &Pe<'_>, base: usize, size: usize) {
    let page = page_size();
    let hdr = (pe.size_of_headers as usize).min(size);
    mprotect(base, hdr, libc::PROT_READ);
    for s in &pe.sections {
        let start = (s.va as usize) & !(page - 1);
        let end = (s.va as usize)
            .saturating_add(s.mapped_size() as usize)
            .min(size);
        if end <= start {
            continue;
        }
        mprotect(
            base + start,
            end - start,
            section_protection(s.characteristics),
        );
        if end - start >= 2 * 1024 * 1024 {
            // Large sections benefit from transparent huge pages when the kernel supports
            // them; a refusal (e.g. THP disabled) is not a load failure.
            unsafe {
                libc::madvise(
                    (base + start) as *mut c_void,
                    end - start,
                    libc::MADV_HUGEPAGE,
                );
            }
        }
    }
    // x86_64 keeps the instruction cache coherent with data writes; the Windows
    // FlushInstructionCache call has no Linux twin to make here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p16(f: &mut [u8], at: usize, v: u16) {
        f[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn p32(f: &mut [u8], at: usize, v: u32) {
        f[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A PE32+ DLL with two sections: .text at RVA 0x1000 (0x200 raw bytes at file 0x200,
    /// execute+read; entry point there) and .data at RVA 0x2000 (0x200 raw bytes at file
    /// 0x400, read+write; the TLS directory lives there). Preferred base `image_base`, one
    /// empty base relocation block at RVA 0x1100. `import_dir`: (RVA, size) if an import
    /// directory is wanted.
    fn tiny_dll(image_base: u64, code: &[u8], import_dir: Option<(u32, u32)>) -> Vec<u8> {
        let mut f = vec![0u8; 0x600];
        f[0..2].copy_from_slice(b"MZ");
        p32(&mut f, 0x3C, 0x40);
        f[0x40..0x44].copy_from_slice(b"PE\0\0");
        p16(&mut f, 0x44, 0x8664);
        p16(&mut f, 0x46, 2);
        p16(&mut f, 0x54, 0xF0);
        p16(&mut f, 0x56, 0x2022);
        let o = 0x58;
        p16(&mut f, o, 0x20B);
        p32(&mut f, o + 0x10, 0x1000); // entry point
        p32(&mut f, o + 0x14, 0x1000); // base of code
        f[o + 0x18..o + 0x20].copy_from_slice(&image_base.to_le_bytes());
        p32(&mut f, o + 0x20, 0x1000); // section alignment
        p32(&mut f, o + 0x24, 0x200); // file alignment
        p16(&mut f, o + 0x28, 6);
        p16(&mut f, o + 0x30, 6);
        p32(&mut f, o + 0x38, 0x3000); // size of image
        p32(&mut f, o + 0x3C, 0x200); // size of headers
        p16(&mut f, o + 0x44, 3); // console subsystem
        f[o + 0x48..o + 0x50].copy_from_slice(&0x10_0000u64.to_le_bytes());
        f[o + 0x50..o + 0x58].copy_from_slice(&0x1000u64.to_le_bytes());
        f[o + 0x58..o + 0x60].copy_from_slice(&0x10_0000u64.to_le_bytes());
        f[o + 0x60..o + 0x68].copy_from_slice(&0x1000u64.to_le_bytes());
        p32(&mut f, o + 0x6C, 16); // data directories
        if let Some((rva, size)) = import_dir {
            p32(&mut f, o + 0x70 + 8, rva);
            p32(&mut f, o + 0x70 + 12, size);
        }
        // base relocations: one empty block (page 0x1000, 8 bytes) at RVA 0x1100
        p32(&mut f, o + 0x70 + 5 * 8, 0x1100);
        p32(&mut f, o + 0x70 + 5 * 8 + 4, 8);
        p32(&mut f, 0x300, 0x1000);
        p32(&mut f, 0x304, 8);
        let s = o + 0xF0;
        f[s..s + 5].copy_from_slice(b".text");
        p32(&mut f, s + 8, 0x200);
        p32(&mut f, s + 12, 0x1000);
        p32(&mut f, s + 16, 0x200);
        p32(&mut f, s + 20, 0x200);
        p32(&mut f, s + 36, 0x6000_0020);
        // .data: RVA 0x2000, file 0x400
        let d = s + 0x28;
        f[d..d + 5].copy_from_slice(b".data");
        p32(&mut f, d + 8, 0x200);
        p32(&mut f, d + 12, 0x2000);
        p32(&mut f, d + 16, 0x200);
        p32(&mut f, d + 20, 0x400);
        p32(&mut f, d + 36, 0xC000_0040);
        f[0x200..0x200 + code.len()].copy_from_slice(code);
        f
    }

    fn set_relocs(f: &mut [u8], entries: &[(u16, u16)]) {
        let o = 0x58;
        let block = 8 + 2 * entries.len();
        p32(f, 0x300, 0x1000);
        p32(f, 0x304, block as u32);
        for (i, &(kind, off)) in entries.iter().enumerate() {
            p16(f, 0x308 + 2 * i, (kind << 12) | off);
        }
        p32(f, o + 0x70 + 5 * 8, 0x1100);
        p32(f, o + 0x70 + 5 * 8 + 4, block as u32);
    }

    /// TLS directory at RVA 0x2000 (.data), callback list at 0x2040, 8-byte template at
    /// 0x2080 (first byte `template`, already-relocated zeros after it) and the image's
    /// `_tls_index` at 0x2088. The callbacks are two distinct `ret`s in .text, so a thread
    /// that attaches later can run them for real. The index starts at a poison value: an
    /// install must overwrite it.
    fn set_tls(f: &mut [u8], image_base: u64, template: u8) {
        let o = 0x58;
        let callbacks = image_base + 0x2040;
        let template_va = image_base + 0x2080;
        p32(f, o + 0x70 + 9 * 8, 0x2000);
        p32(f, o + 0x70 + 9 * 8 + 4, 40);
        let d = 0x400; // RVA 0x2000
        f[d..d + 8].copy_from_slice(&template_va.to_le_bytes());
        f[d + 8..d + 16].copy_from_slice(&(template_va + 8).to_le_bytes());
        f[d + 16..d + 24].copy_from_slice(&(image_base + 0x2088).to_le_bytes());
        f[d + 24..d + 32].copy_from_slice(&callbacks.to_le_bytes());
        // callbacks: two distinct entries and the null terminator
        let c = 0x440; // RVA 0x2040
        f[c..c + 8].copy_from_slice(&(image_base + 0x1000).to_le_bytes());
        f[c + 8..c + 16].copy_from_slice(&(image_base + 0x1001).to_le_bytes());
        f[0x480] = template; // RVA 0x2080
        p32(f, 0x488, 0xDEAD_BEEF); // RVA 0x2088, poison until installed
    }

    fn write(name: &str, bytes: &[u8]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("xcl-loader-linux-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn perms_at(addr: usize) -> Option<String> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        for line in maps.lines() {
            let mut it = line.split_whitespace();
            let range = it.next()?;
            let perms = it.next()?.to_string();
            let (a, b) = range.split_once('-')?;
            let (a, b) = (
                usize::from_str_radix(a, 16).ok()?,
                usize::from_str_radix(b, 16).ok()?,
            );
            if addr >= a && addr < b {
                return Some(perms);
            }
        }
        None
    }

    #[test]
    fn maps_a_synthetic_pe_at_its_link_address() {
        let base = 0x6E51_0000_0000u64;
        let code = [0x31, 0xC0, 0xC3];
        let p = write("xcl_map_ok.dll", &tiny_dll(base, &code, None));
        let m = map_image(&p).expect("synthetic PE maps");
        assert_eq!(m.base, base as usize);
        assert_eq!(m.size, 0x3000);
        assert_eq!(m.entry_rva, 0x1000);
        assert!(m.is_dll);
        assert_eq!(m.name, "xcl_map_ok.dll");
        assert_eq!(m.path, p);
        assert_eq!(
            unsafe { std::slice::from_raw_parts((m.base + 0x1000) as *const u8, code.len()) },
            code
        );
        assert!(modules::find_mapped_by_base(m.base).is_some());
    }

    #[test]
    fn rejects_a_non_pe() {
        let p = write("xcl_not_a_pe.bin", b"this is not a PE file");
        let e = map_image(&p).err().expect("image must be rejected");
        assert!(e.contains("not a PE"), "{e}");
    }

    #[test]
    fn rejects_an_occupied_link_address() {
        let len = page_size() * 2;
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        let base = p as u64;
        let path = write("xcl_occupied.dll", &tiny_dll(base, &[0xC3], None));
        let e = map_image(&path).err().expect("image must be rejected");
        assert!(e.contains(&format!("{base:#x}")), "{e}");
        assert!(e.contains("occupied"), "{e}");
        unsafe {
            libc::munmap(p, len);
        }
    }

    #[test]
    fn applies_relocations_when_the_base_differs() {
        let image_base = 0x4000_0000u64;
        let mut file = tiny_dll(image_base, &[0xC3], None);
        set_relocs(&mut file, &[(10, 0x010), (3, 0x018)]);
        let pe = Pe::parse(&file, Layout::File).unwrap();
        let mut image = vec![0u8; 0x2000];
        let at = image.as_mut_ptr() as usize;
        unsafe {
            std::ptr::write_unaligned((at + 0x1010) as *mut u64, image_base + 0x1000);
            std::ptr::write_unaligned((at + 0x1018) as *mut u32, (image_base + 0x1000) as u32);
        }
        apply_relocations(&pe, at, 0x1000).unwrap();
        unsafe {
            assert_eq!(
                std::ptr::read_unaligned((at + 0x1010) as *const u64),
                image_base + 0x2000
            );
            assert_eq!(
                std::ptr::read_unaligned((at + 0x1018) as *const u32),
                (image_base + 0x2000) as u32
            );
        }
    }

    #[test]
    fn tls_directory_callbacks_are_preserved_in_order() {
        let base = 0x6E52_0000_0000u64;
        let mut file = tiny_dll(base, &[0xC3, 0xC3], None);
        set_tls(&mut file, base, 7);
        let p = write("xcl_tls.dll", &file);
        let m = map_image(&p).expect("synthetic TLS image maps");
        assert_eq!(
            *m.tls_callbacks.lock().unwrap(),
            vec![base as usize + 0x1000, base as usize + 0x1001]
        );
        // A DLL installs its blocks with the game-DLL load path (step 10), not at map.
        assert_eq!(
            unsafe { std::ptr::read_volatile((m.base + 0x2088) as *const u32) },
            0xDEAD_BEEF
        );
    }

    /// Read byte 0 of the current thread's block for `index` through the TEB pointer, as
    /// PE `__declspec(thread)` code does.
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

    #[test]
    fn main_image_installs_slot_zero_and_blocks_for_new_threads() {
        let base = 0x6E55_0000_0000u64;
        let mut file = tiny_dll(base, &[0xC3, 0xC3], None);
        set_tls(&mut file, base, 7);
        let p = write("xcl_tls_main.dll", &file);
        let m = load_main(&p).expect("synthetic main image loads");
        let index = unsafe { std::ptr::read_volatile((m.base + 0x2088) as *const u32) };
        assert_eq!(index, 0, "the main image takes TLS slot 0");

        let a = crate::tls::spawn("tls-main-a", move || unsafe {
            let first = read_tls_byte(index);
            *slots_for(index) = 8;
            (first, read_tls_byte(index))
        })
        .expect("spawn a");
        let b = crate::tls::spawn("tls-main-b", move || unsafe {
            let first = read_tls_byte(index);
            *slots_for(index) = 8;
            (first, read_tls_byte(index))
        })
        .expect("spawn b");
        assert_eq!(a.join().unwrap(), (7, 8));
        assert_eq!(b.join().unwrap(), (7, 8));
    }

    unsafe fn slots_for(index: u32) -> *mut u8 {
        let slots: *mut *mut u8;
        core::arch::asm!(
            "mov {}, gs:[0x58]",
            out(reg) slots,
            options(nostack, readonly, preserves_flags)
        );
        *slots.add(index as usize)
    }

    #[test]
    fn sections_get_their_page_protections() {
        let base = 0x6E53_0000_0000u64;
        let p = write("xcl_prot.dll", &tiny_dll(base, &[0xC3], None));
        let m = map_image(&p).unwrap();
        assert_eq!(perms_at(m.base).as_deref(), Some("r--p"), ".headers");
        assert_eq!(perms_at(m.base + 0x1000).as_deref(), Some("r-xp"), ".text");
    }

    #[test]
    fn a_broken_import_table_fails_before_registration() {
        let base = 0x6E54_0000_0000u64;
        let mut code = vec![0xC3];
        code.resize(0x20, 0xCC);
        for v in [0x1000u32, 0, 0, 0x7FFF_0000, 0x1000] {
            code.extend_from_slice(&v.to_le_bytes());
        }
        let p = write(
            "xcl_bad_import.dll",
            &tiny_dll(base, &code, Some((0x1010, 40))),
        );
        let e = map_image(&p).err().expect("image must be rejected");
        assert!(e.contains("import table"), "{e}");
        assert!(modules::find_mapped_by_base(base as usize).is_none());
    }

    /// Headless harness for the retail exe: `XCL_SAMPLE_EXE=<game_scarlett_master.exe>`
    /// cargo test -p xcl --lib loader::tests::maps_the_retail_exe_when_sample_set -- --nocapture
    #[test]
    fn maps_the_retail_exe_when_sample_set() {
        let Ok(path) = std::env::var("XCL_SAMPLE_EXE") else {
            println!("XCL_SAMPLE_EXE not set – skipping");
            return;
        };
        let m = map_image(Path::new(&path)).expect("retail exe maps at its link address");
        let bytes = std::fs::read(&path).unwrap();
        let pe = Pe::parse(&bytes, Layout::File).unwrap();
        assert_eq!(m.base, pe.image_base as usize);
        assert_eq!(m.size, pe.size_of_image as usize);
        assert_eq!(m.entry_rva, pe.entry_rva);
        assert!(!m.is_dll, "game_scarlett_master.exe is an EXE");
        let callbacks = m.tls_callbacks.lock().unwrap().clone();
        assert!(
            callbacks.iter().all(|cb| m.contains(*cb)),
            "TLS callbacks inside the image"
        );
        println!(
            "mapped {} at {:#x}: size {:#x}, entry +{:#x}, {} sections, {} TLS callbacks, perms .text {:?}",
            m.name,
            m.base,
            m.size,
            m.entry_rva,
            pe.sections.len(),
            callbacks.len(),
            perms_at(m.base + pe.sections.iter().find(|s| s.name == ".text").map(|s| s.va as usize).unwrap_or(0x1000))
        );
    }
}
