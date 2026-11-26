//! Minimal, allocation-light PE32+ (x64) parser.
//!
//! Works on two layouts:
//! * `Layout::File`   – raw bytes of a file on disk (RVA → file offset via section table),
//! * `Layout::Mapped` – an image already mapped in memory (RVA == offset).
//!
//! Everything is read with explicit little-endian accessors and bounds checks, so a
//! malformed file produces a `PeError` instead of undefined behaviour.

use std::fmt;

pub const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
pub const IMAGE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
pub const IMAGE_FILE_DLL: u16 = 0x2000;

pub const DIR_EXPORT: usize = 0;
pub const DIR_IMPORT: usize = 1;
pub const DIR_RESOURCE: usize = 2;
pub const DIR_EXCEPTION: usize = 3;
pub const DIR_BASERELOC: usize = 5;
pub const DIR_TLS: usize = 9;
pub const DIR_IAT: usize = 12;
pub const DIR_DELAY_IMPORT: usize = 13;

pub const IMAGE_REL_BASED_ABSOLUTE: u16 = 0;
pub const IMAGE_REL_BASED_HIGHLOW: u16 = 3;
pub const IMAGE_REL_BASED_DIR64: u16 = 10;

pub const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const SCN_MEM_READ: u32 = 0x4000_0000;
pub const SCN_MEM_WRITE: u32 = 0x8000_0000;
pub const SCN_ALIGN_MASK: u32 = 0x00F0_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeError {
    Truncated(&'static str),
    BadMagic(&'static str),
    NotX64(u16),
    BadRva(u32),
    /// A directory-derived write (IAT slot, delay-load HMODULE, TLS index) lands in the headers.
    HeaderWriteTarget(u32),
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::Truncated(w) => write!(f, "file truncated/corrupt ({w})"),
            PeError::BadMagic(w) => write!(f, "invalid signature {w} (not a PE file)"),
            PeError::NotX64(m) => write!(f, "architecture 0x{m:04x} is not x64 (0x8664)"),
            PeError::BadRva(r) => write!(f, "RVA 0x{r:x} outside the image"),
            PeError::HeaderWriteTarget(r) => {
                write!(f, "RVA 0x{r:x} is a write target inside the headers")
            }
        }
    }
}

impl std::error::Error for PeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    File,
    Mapped,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DataDir {
    pub rva: u32,
    pub size: u32,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub va: u32,
    pub vsize: u32,
    pub raw_ptr: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

impl Section {
    /// Size the section occupies once mapped.
    pub fn mapped_size(&self) -> u32 {
        if self.vsize == 0 {
            self.raw_size
        } else {
            self.vsize
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportName {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl fmt::Display for ImportName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportName::Name { name, .. } => write!(f, "{name}"),
            ImportName::Ordinal(o) => write!(f, "#{o}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportEntry {
    /// RVA of the IAT slot that receives the resolved address.
    pub iat_rva: u32,
    pub what: ImportName,
}

#[derive(Debug, Clone)]
pub struct ImportDll {
    pub dll: String,
    pub delay: bool,
    pub entries: Vec<ImportEntry>,
    /// Delay-load only: RVA of the HMODULE slot.
    pub module_handle_rva: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reloc {
    pub rva: u32,
    pub kind: u16,
}

/// IMAGE_TLS_DIRECTORY64 – all address fields are absolute VAs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TlsDir {
    pub start_raw: u64,
    pub end_raw: u64,
    pub address_of_index: u64,
    pub address_of_callbacks: u64,
    pub size_of_zero_fill: u32,
    pub characteristics: u32,
}

impl TlsDir {
    pub fn template_len(&self) -> u64 {
        self.end_raw.saturating_sub(self.start_raw)
    }
    pub fn total_len(&self) -> u64 {
        self.template_len().saturating_add(self.size_of_zero_fill as u64)
    }
    /// Alignment in bytes encoded in `Characteristics` bits 20..23
    /// (`IMAGE_SCN_ALIGN_*BYTES`, nibble `1..=14` → `1..=8192` bytes), or `1`
    /// when unspecified (`0`) or invalid (`15`).
    pub fn alignment(&self) -> u32 {
        tls_alignment_from_characteristics(self.characteristics)
    }
    /// `Characteristics` masked down to a valid `IMAGE_SCN_ALIGN_*BYTES` value
    /// (nibble `1..=14`), clearing reserved bits and the invalid `0xF` mask value.
    pub fn align_characteristics(&self) -> u32 {
        sanitize_tls_characteristics(self.characteristics)
    }
}

/// Decode `IMAGE_SCN_ALIGN_*BYTES` (bits 20..23) into a byte alignment (`1..=8192`).
pub fn tls_alignment_from_characteristics(characteristics: u32) -> u32 {
    let nibble = (characteristics & SCN_ALIGN_MASK) >> 20;
    if (1..=14).contains(&nibble) {
        1u32 << (nibble - 1)
    } else {
        1
    }
}

/// Mask `IMAGE_TLS_DIRECTORY64.Characteristics` to a valid `IMAGE_SCN_ALIGN_*BYTES`
/// value (nibble `1..=14`), clearing reserved bits and the invalid `0xF` mask value.
pub fn sanitize_tls_characteristics(characteristics: u32) -> u32 {
    let nibble = (characteristics & SCN_ALIGN_MASK) >> 20;
    if (1..=14).contains(&nibble) {
        nibble << 20
    } else {
        0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    Rva(u32),
    Forwarder(String),
}

#[derive(Debug, Clone, Default)]
pub struct Exports {
    pub dll_name: String,
    pub ordinal_base: u32,
    /// Function RVAs, indexed by (ordinal - base).
    pub functions: Vec<u32>,
    /// (name, index into `functions`)
    pub names: Vec<(String, u16)>,
    pub dir: DataDir,
}

impl Exports {
    fn target(&self, rva: u32, fwd: Option<String>) -> Option<ExportTarget> {
        if rva == 0 {
            return None;
        }
        match fwd {
            Some(s) => Some(ExportTarget::Forwarder(s)),
            None => Some(ExportTarget::Rva(rva)),
        }
    }

    pub fn by_name_raw(&self, name: &str) -> Option<u32> {
        // names are sorted in valid PE files, but be tolerant of unsorted tables
        self.names
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, idx)| self.functions.get(*idx as usize).copied())
    }

    pub fn by_ordinal_raw(&self, ordinal: u32) -> Option<u32> {
        let idx = ordinal.checked_sub(self.ordinal_base)?;
        self.functions.get(idx as usize).copied()
    }

    pub fn is_forwarder_rva(&self, rva: u32) -> bool {
        self.dir
            .rva
            .checked_add(self.dir.size)
            .is_some_and(|end| rva >= self.dir.rva && rva < end)
    }
}

pub struct Pe<'a> {
    data: &'a [u8],
    layout: Layout,
    pub machine: u16,
    pub characteristics: u16,
    pub image_base: u64,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub entry_rva: u32,
    pub section_alignment: u32,
    pub file_alignment: u32,
    pub subsystem: u16,
    pub dll_characteristics: u16,
    pub stack_reserve: u64,
    pub stack_commit: u64,
    pub timestamp: u32,
    pub sections: Vec<Section>,
    pub dirs: [DataDir; 16],
}

fn rd_u16(d: &[u8], off: usize) -> Option<u16> {
    d.get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn rd_u32(d: &[u8], off: usize) -> Option<u32> {
    d.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn rd_u64(d: &[u8], off: usize) -> Option<u64> {
    d.get(off..off + 8).map(|b| {
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        u64::from_le_bytes(a)
    })
}

impl<'a> Pe<'a> {
    pub fn parse(data: &'a [u8], layout: Layout) -> Result<Self, PeError> {
        if rd_u16(data, 0).ok_or(PeError::Truncated("DOS"))? != 0x5A4D {
            return Err(PeError::BadMagic("MZ"));
        }
        let nt = rd_u32(data, 0x3C).ok_or(PeError::Truncated("e_lfanew"))? as usize;
        if rd_u32(data, nt).ok_or(PeError::Truncated("NT"))? != 0x0000_4550 {
            return Err(PeError::BadMagic("PE"));
        }
        let coff = nt + 4;
        let machine = rd_u16(data, coff).ok_or(PeError::Truncated("COFF"))?;
        let nsec = rd_u16(data, coff + 2).ok_or(PeError::Truncated("COFF"))? as usize;
        let timestamp = rd_u32(data, coff + 4).ok_or(PeError::Truncated("COFF"))?;
        let opt_size = rd_u16(data, coff + 16).ok_or(PeError::Truncated("COFF"))? as usize;
        let characteristics = rd_u16(data, coff + 18).ok_or(PeError::Truncated("COFF"))?;
        let opt = coff + 20;
        let magic = rd_u16(data, opt).ok_or(PeError::Truncated("optional header"))?;
        if magic != 0x20B {
            if machine != IMAGE_FILE_MACHINE_AMD64 {
                return Err(PeError::NotX64(machine));
            }
            return Err(PeError::BadMagic("PE32+ (0x20B)"));
        }
        if machine != IMAGE_FILE_MACHINE_AMD64 {
            return Err(PeError::NotX64(machine));
        }
        let t = |o: usize| PeError::Truncated(if o < 112 { "optional header" } else { "data directories" });
        let entry_rva = rd_u32(data, opt + 16).ok_or(t(16))?;
        let image_base = rd_u64(data, opt + 24).ok_or(t(24))?;
        let section_alignment = rd_u32(data, opt + 32).ok_or(t(32))?;
        let file_alignment = rd_u32(data, opt + 36).ok_or(t(36))?;
        let size_of_image = rd_u32(data, opt + 56).ok_or(t(56))?;
        let size_of_headers = rd_u32(data, opt + 60).ok_or(t(60))?;
        let subsystem = rd_u16(data, opt + 68).ok_or(t(68))?;
        let dll_characteristics = rd_u16(data, opt + 70).ok_or(t(70))?;
        let stack_reserve = rd_u64(data, opt + 72).ok_or(t(72))?;
        let stack_commit = rd_u64(data, opt + 80).ok_or(t(80))?;
        let ndirs = (rd_u32(data, opt + 108).ok_or(t(108))? as usize).min(16);
        let max_dirs = opt_size.saturating_sub(112) / 8;
        let mut dirs = [DataDir::default(); 16];
        for (i, d) in dirs.iter_mut().enumerate().take(ndirs.min(max_dirs)) {
            let o = opt + 112 + i * 8;
            d.rva = rd_u32(data, o).ok_or(t(112))?;
            d.size = rd_u32(data, o + 4).ok_or(t(112))?;
        }
        let mut sections = Vec::with_capacity(nsec);
        let sec0 = opt + opt_size;
        for i in 0..nsec {
            let o = sec0 + i * 40;
            let raw_name = data.get(o..o + 8).ok_or(PeError::Truncated("section table"))?;
            let end = raw_name.iter().position(|&c| c == 0).unwrap_or(8);
            sections.push(Section {
                name: String::from_utf8_lossy(&raw_name[..end]).into_owned(),
                vsize: rd_u32(data, o + 8).ok_or(PeError::Truncated("section table"))?,
                va: rd_u32(data, o + 12).ok_or(PeError::Truncated("section table"))?,
                raw_size: rd_u32(data, o + 16).ok_or(PeError::Truncated("section table"))?,
                raw_ptr: rd_u32(data, o + 20).ok_or(PeError::Truncated("section table"))?,
                characteristics: rd_u32(data, o + 36).ok_or(PeError::Truncated("section table"))?,
            });
        }
        Ok(Pe {
            data,
            layout,
            machine,
            characteristics,
            image_base,
            size_of_image,
            size_of_headers,
            entry_rva,
            section_alignment,
            file_alignment,
            subsystem,
            dll_characteristics,
            stack_reserve,
            stack_commit,
            timestamp,
            sections,
            dirs,
        })
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn is_dll(&self) -> bool {
        self.characteristics & IMAGE_FILE_DLL != 0
    }

    pub fn has_relocs(&self) -> bool {
        self.characteristics & IMAGE_FILE_RELOCS_STRIPPED == 0 && self.dirs[DIR_BASERELOC].size != 0
    }

    pub fn dir(&self, idx: usize) -> DataDir {
        self.dirs[idx]
    }

    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        match self.layout {
            Layout::Mapped => {
                if (self.size_of_image == 0 || rva < self.size_of_image) && (rva as usize) < self.data.len() {
                    Some(rva as usize)
                } else {
                    None
                }
            }
            Layout::File => {
                if rva < self.size_of_headers {
                    let off = rva as usize;
                    return (off < self.data.len()).then_some(off);
                }
                for s in &self.sections {
                    let size = s.mapped_size().max(s.raw_size);
                    if rva >= s.va && rva < s.va.saturating_add(size) {
                        let delta = rva - s.va;
                        if delta >= s.raw_size {
                            return None; // in zero-filled tail, not present in file
                        }
                        let off = (s.raw_ptr as usize).checked_add(delta as usize)?;
                        return (off < self.data.len()).then_some(off);
                    }
                }
                None
            }
        }
    }

    fn rva_tail(&self, rva: u32) -> Option<&'a [u8]> {
        match self.layout {
            Layout::Mapped => {
                if self.size_of_image != 0 && rva >= self.size_of_image {
                    return None;
                }
                let len = if self.size_of_image != 0 {
                    (self.size_of_image - rva) as usize
                } else {
                    self.data.len().saturating_sub(rva as usize)
                };
                self.data.get(rva as usize..).and_then(|s| s.get(..len.min(s.len())))
            }
            Layout::File => {
                if rva < self.size_of_headers {
                    let off = rva as usize;
                    let end = off.saturating_add((self.size_of_headers - rva) as usize).min(self.data.len());
                    return self.data.get(off..end);
                }
                for s in &self.sections {
                    let size = s.mapped_size().max(s.raw_size);
                    if rva >= s.va && rva < s.va.saturating_add(size) {
                        let delta = rva - s.va;
                        if delta >= s.raw_size {
                            return None; // in zero-filled tail, not present in file
                        }
                        let off = (s.raw_ptr as usize).checked_add(delta as usize)?;
                        let end = off.saturating_add((s.raw_size - delta) as usize).min(self.data.len());
                        return self.data.get(off..end);
                    }
                }
                None
            }
        }
    }

    pub fn bytes_at(&self, rva: u32, len: usize) -> Option<&'a [u8]> {
        self.rva_tail(rva)?.get(..len)
    }

    pub fn u16_at(&self, rva: u32) -> Option<u16> {
        let b = self.bytes_at(rva, 2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32_at(&self, rva: u32) -> Option<u32> {
        let b = self.bytes_at(rva, 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64_at(&self, rva: u32) -> Option<u64> {
        let b = self.bytes_at(rva, 8)?;
        Some(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    pub fn cstr_at(&self, rva: u32) -> Option<String> {
        let tail = self.rva_tail(rva)?;
        let end = tail.iter().take(4096).position(|&c| c == 0)?;
        Some(String::from_utf8_lossy(&tail[..end]).into_owned())
    }

    /// Convert an absolute VA (as stored in TLS directory, etc.) to an RVA.
    pub fn va_to_rva(&self, va: u64) -> Option<u32> {
        let r = va.checked_sub(self.image_base)?;
        u32::try_from(r).ok()
    }

    /// Refuse an RVA the loader would write into when it still needs the headers intact
    /// (IAT thunks, delay-load module handles, TLS `AddressOfIndex`). The whole write of
    /// `width` bytes must lie in `[size_of_headers, size_of_image)`.
    fn ensure_data_write_rva(&self, rva: u32, width: u32) -> Result<(), PeError> {
        if rva < self.size_of_headers {
            return Err(PeError::HeaderWriteTarget(rva));
        }
        let end = rva.checked_add(width).ok_or(PeError::BadRva(rva))?;
        if end > self.size_of_image {
            return Err(PeError::BadRva(rva));
        }
        Ok(())
    }

    fn thunks(&self, int_rva: u32, iat_rva: u32) -> Result<Vec<ImportEntry>, PeError> {
        let mut out = Vec::new();
        let names_rva = if int_rva != 0 { int_rva } else { iat_rva };
        if names_rva == 0 {
            return Ok(out);
        }
        for i in 0..65536u32 {
            let offset = i.checked_mul(8).ok_or(PeError::BadRva(names_rva))?;
            let slot = names_rva.checked_add(offset).ok_or(PeError::BadRva(names_rva))?;
            if self.size_of_image != 0 && slot.saturating_add(8) > self.size_of_image {
                return Err(PeError::BadRva(slot));
            }
            let v = self.u64_at(slot).ok_or(PeError::BadRva(slot))?;
            if v == 0 {
                break;
            }
            let what = if v & (1u64 << 63) != 0 {
                ImportName::Ordinal((v & 0xFFFF) as u16)
            } else {
                let hn = (v & 0x7FFF_FFFF) as u32;
                let hint = self.u16_at(hn).ok_or(PeError::BadRva(hn))?;
                // wrapping_add would turn a hint at the top of the address space into RVA 0
                // and read the DOS header as the import name
                let name_rva = hn.checked_add(2).ok_or(PeError::BadRva(hn))?;
                let name = self.cstr_at(name_rva).ok_or(PeError::BadRva(hn))?;
                ImportName::Name { hint, name }
            };
            let iat_offset = i.checked_mul(8).ok_or(PeError::BadRva(iat_rva))?;
            let entry_iat_rva = iat_rva.checked_add(iat_offset).ok_or(PeError::BadRva(iat_rva))?;
            if self.size_of_image != 0 && entry_iat_rva.saturating_add(8) > self.size_of_image {
                return Err(PeError::BadRva(entry_iat_rva));
            }
            self.ensure_data_write_rva(entry_iat_rva, 8)?;
            out.push(ImportEntry { iat_rva: entry_iat_rva, what });
        }
        Ok(out)
    }

    pub fn imports(&self) -> Result<Vec<ImportDll>, PeError> {
        let d = self.dirs[DIR_IMPORT];
        let mut out = Vec::new();
        if d.rva == 0 || d.size == 0 {
            return Ok(out);
        }
        if d.size < 20 || d.rva.checked_add(d.size).is_none() {
            return Err(PeError::BadRva(d.rva));
        }
        for i in 0..4096u32 {
            let pos = i.checked_mul(20).ok_or(PeError::BadRva(d.rva))?;
            if 20 > d.size - pos {
                return Err(PeError::BadRva(d.rva.checked_add(pos).unwrap_or(d.rva)));
            }
            let o = d.rva.checked_add(pos).ok_or(PeError::BadRva(d.rva))?;
            let g = |offset: u32| {
                let rva = o.checked_add(offset).ok_or(PeError::BadRva(o))?;
                self.u32_at(rva).ok_or(PeError::BadRva(rva))
            };
            let int_rva = g(0)?;
            let name_rva = g(12)?;
            let iat_rva = g(16)?;
            if int_rva == 0 && name_rva == 0 && iat_rva == 0 {
                break;
            }
            let dll = self.cstr_at(name_rva).ok_or(PeError::BadRva(name_rva))?;
            let entries = self.thunks(int_rva, iat_rva)?;
            out.push(ImportDll { dll, delay: false, entries, module_handle_rva: 0 });
        }
        Ok(out)
    }

    pub fn delay_imports(&self) -> Result<Vec<ImportDll>, PeError> {
        let d = self.dirs[DIR_DELAY_IMPORT];
        let mut out = Vec::new();
        if d.rva == 0 || d.size == 0 {
            return Ok(out);
        }
        if d.size < 32 || d.rva.checked_add(d.size).is_none() {
            return Err(PeError::BadRva(d.rva));
        }
        for i in 0..4096u32 {
            let pos = i.checked_mul(32).ok_or(PeError::BadRva(d.rva))?;
            if 32 > d.size - pos {
                return Err(PeError::BadRva(d.rva.checked_add(pos).unwrap_or(d.rva)));
            }
            let o = d.rva.checked_add(pos).ok_or(PeError::BadRva(d.rva))?;
            let g = |offset: u32| {
                let rva = o.checked_add(offset).ok_or(PeError::BadRva(o))?;
                self.u32_at(rva).ok_or(PeError::BadRva(rva))
            };
            let attrs = g(0)?;
            let mut name_rva = g(4)?;
            let mut hmod_rva = g(8)?;
            let mut iat_rva = g(12)?;
            let mut int_rva = g(16)?;
            if name_rva == 0 && iat_rva == 0 {
                break;
            }
            if attrs & 1 == 0 {
                // very old VA-based descriptors
                let fix = |v: u32| self.va_to_rva(v as u64).unwrap_or(v);
                name_rva = fix(name_rva);
                hmod_rva = fix(hmod_rva);
                iat_rva = fix(iat_rva);
                int_rva = fix(int_rva);
            }
            let dll = self.cstr_at(name_rva).ok_or(PeError::BadRva(name_rva))?;
            if hmod_rva != 0 {
                self.ensure_data_write_rva(hmod_rva, 8)?;
            }
            let entries = self.thunks(int_rva, iat_rva)?;
            out.push(ImportDll { dll, delay: true, entries, module_handle_rva: hmod_rva });
        }
        Ok(out)
    }

    pub fn relocations(&self) -> Result<Vec<Reloc>, PeError> {
        let d = self.dirs[DIR_BASERELOC];
        let mut out = Vec::new();
        if d.rva == 0 || d.size == 0 {
            return Ok(out);
        }
        if d.size < 8 || d.rva.checked_add(d.size).is_none() {
            return Err(PeError::BadRva(d.rva));
        }
        // the block sizes come from the file: a block past the directory's end (or one whose
        // offsets overflow) is a malformed image, not a panic or a 2^31-entry loop
        let mut pos = 0u32;
        while pos.saturating_add(8) <= d.size {
            let at = d.rva.checked_add(pos).ok_or(PeError::BadRva(d.rva))?;
            let page = self.u32_at(at).ok_or(PeError::BadRva(at))?;
            let block_rva = at.checked_add(4).ok_or(PeError::BadRva(at))?;
            let block = self.u32_at(block_rva).ok_or(PeError::BadRva(block_rva))?;
            if block < 8 {
                break;
            }
            if block > d.size - pos {
                return Err(PeError::BadRva(at));
            }
            let n = (block - 8) / 2;
            for i in 0..n {
                let entry_rva = at.checked_add(8).and_then(|rva| rva.checked_add(i * 2))
                    .ok_or(PeError::BadRva(at))?;
                let e = self.u16_at(entry_rva).ok_or(PeError::BadRva(entry_rva))?;
                let kind = e >> 12;
                if kind == IMAGE_REL_BASED_ABSOLUTE {
                    continue;
                }
                let reloc_rva = page.checked_add((e & 0x0FFF) as u32).ok_or(PeError::BadRva(at))?;
                if self.size_of_image != 0 && reloc_rva >= self.size_of_image {
                    return Err(PeError::BadRva(reloc_rva));
                }
                out.push(Reloc { rva: reloc_rva, kind });
            }
            pos += block;
        }
        Ok(out)
    }

    pub fn tls(&self) -> Result<Option<TlsDir>, PeError> {
        let d = self.dirs[DIR_TLS];
        if d.rva == 0 || d.size == 0 {
            return Ok(None);
        }
        let field = |o: u32| d.rva.checked_add(o).ok_or(PeError::BadRva(d.rva));
        let r = |o: u32| {
            let rva = field(o)?;
            self.u64_at(rva).ok_or(PeError::BadRva(rva))
        };
        let address_of_index = r(16)?;
        // The loader writes the TLS slot into AddressOfIndex; RVA 0 is "no index" (VA 0),
        // but image_base+0 (RVA 0) would overwrite the DOS header – refuse it too.
        if address_of_index != 0 {
            let index_rva = self.va_to_rva(address_of_index).ok_or(PeError::BadRva(0))?;
            self.ensure_data_write_rva(index_rva, 4)?;
        }
        Ok(Some(TlsDir {
            start_raw: r(0)?,
            end_raw: r(8)?,
            address_of_index,
            address_of_callbacks: r(24)?,
            size_of_zero_fill: self.u32_at(field(32)?).ok_or(PeError::BadRva(d.rva))?,
            characteristics: self.u32_at(field(36)?).ok_or(PeError::BadRva(d.rva))?,
        }))
    }

    pub fn exports(&self) -> Result<Option<Exports>, PeError> {
        let d = self.dirs[DIR_EXPORT];
        if d.rva == 0 || d.size == 0 {
            return Ok(None);
        }
        if d.size < 40 || d.rva.checked_add(d.size).is_none() {
            return Err(PeError::BadRva(d.rva));
        }
        let g = |o: u32| {
            let rva = d.rva.checked_add(o).ok_or(PeError::BadRva(d.rva))?;
            self.u32_at(rva).ok_or(PeError::BadRva(rva))
        };
        let name_rva = g(12)?;
        let base = g(16)?;
        let nfunc = g(20)?.min(65536);
        let nnames = g(24)?.min(65536);
        let afunc = g(28)?;
        let anames = g(32)?;
        let aords = g(36)?;
        let check_table = |rva: u32, size: u32| -> Result<(), PeError> {
            if rva >= d.rva && rva < d.rva.saturating_add(d.size) {
                let pos = rva - d.rva;
                if size > d.size - pos {
                    return Err(PeError::BadRva(rva));
                }
            }
            if self.size_of_image != 0 && rva.saturating_add(size) > self.size_of_image {
                return Err(PeError::BadRva(rva));
            }
            Ok(())
        };
        let func_table_size = nfunc.checked_mul(4).ok_or(PeError::BadRva(afunc))?;
        check_table(afunc, func_table_size)?;
        let name_table_size = nnames.checked_mul(4).ok_or(PeError::BadRva(anames))?;
        check_table(anames, name_table_size)?;
        let ord_table_size = nnames.checked_mul(2).ok_or(PeError::BadRva(aords))?;
        check_table(aords, ord_table_size)?;

        let mut functions = Vec::with_capacity(nfunc as usize);
        for i in 0..nfunc {
            let rva = i.checked_mul(4).and_then(|o| afunc.checked_add(o)).ok_or(PeError::BadRva(afunc))?;
            functions.push(self.u32_at(rva).ok_or(PeError::BadRva(rva))?);
        }
        let mut names = Vec::with_capacity(nnames as usize);
        for i in 0..nnames {
            let nr_rva = i.checked_mul(4).and_then(|o| anames.checked_add(o)).ok_or(PeError::BadRva(anames))?;
            let ord_rva = i.checked_mul(2).and_then(|o| aords.checked_add(o)).ok_or(PeError::BadRva(aords))?;
            let nr = self.u32_at(nr_rva).ok_or(PeError::BadRva(nr_rva))?;
            let idx = self.u16_at(ord_rva).ok_or(PeError::BadRva(ord_rva))?;
            if let Some(n) = self.cstr_at(nr) {
                names.push((n, idx));
            }
        }
        Ok(Some(Exports {
            dll_name: self.cstr_at(name_rva).unwrap_or_default(),
            ordinal_base: base,
            functions,
            names,
            dir: d,
        }))
    }

    /// Resolve an export to an RVA or a forwarder string ("DLL.Func" / "DLL.#12").
    /// An unreadable forwarder is not an export address.
    pub fn export_target(&self, ex: &Exports, rva: u32) -> Option<ExportTarget> {
        let fwd = if ex.is_forwarder_rva(rva) { Some(self.cstr_at(rva)?) } else { None };
        ex.target(rva, fwd)
    }
}

/// Owned export table with forwarders already decoded – safe to keep after the
/// source bytes are gone.
#[derive(Debug, Clone, Default)]
pub struct OwnedExports {
    pub by_name: std::collections::HashMap<String, ExportTarget>,
    pub by_ordinal: std::collections::HashMap<u32, ExportTarget>,
}

impl OwnedExports {
    pub fn from_pe(pe: &Pe<'_>) -> Result<Self, PeError> {
        let mut out = OwnedExports::default();
        if let Some(ex) = pe.exports()? {
            for (i, &rva) in ex.functions.iter().enumerate() {
                // the base comes from the file: an entry past u32::MAX has no ordinal
                let Some(ordinal) = ex.ordinal_base.checked_add(i as u32) else { break };
                if let Some(t) = pe.export_target(&ex, rva) {
                    out.by_ordinal.insert(ordinal, t);
                }
            }
            for (name, idx) in &ex.names {
                if let Some(&rva) = ex.functions.get(*idx as usize) {
                    if let Some(t) = pe.export_target(&ex, rva) {
                        out.by_name.insert(name.clone(), t);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_file(name: &str) -> Vec<u8> {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        std::fs::read(format!("{root}\\System32\\{name}")).expect("read system dll")
    }

    #[test]
    fn rejects_non_pe() {
        assert_eq!(Pe::parse(b"h", Layout::File).err(), Some(PeError::Truncated("DOS")));
        assert_eq!(Pe::parse(b"hello", Layout::File).err(), Some(PeError::BadMagic("MZ")));
        let mut buf = vec![0u8; 256];
        buf[0] = b'M';
        buf[1] = b'Z';
        buf[0x3C] = 0x40;
        assert_eq!(Pe::parse(&buf, Layout::File).err(), Some(PeError::BadMagic("PE")));
    }

    // A small file can map readable directory fields near the u32 RVA limit. Using an
    // unmapped high RVA would fail the first read and miss overflow in later fields.
    fn directory_at_rva(dir: usize, rva: u32, payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 0x200 + payload.len()];
        data[..2].copy_from_slice(b"MZ");
        data[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        data[0x40..0x44].copy_from_slice(b"PE\0\0");
        data[0x44..0x46].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
        data[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        data[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        let opt = 0x58;
        data[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        data[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
        data[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let d = opt + 112 + dir * 8;
        data[d..d + 4].copy_from_slice(&rva.to_le_bytes());
        data[d + 4..d + 8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let sec = opt + 240;
        data[sec + 8..sec + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        data[sec + 12..sec + 16].copy_from_slice(&rva.to_le_bytes());
        data[sec + 16..sec + 20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        data[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes());
        data[0x200..].copy_from_slice(payload);
        data
    }

    #[test]
    fn import_name_at_the_top_of_the_address_space_is_error() {
        // Hint/name RVA 0xFFFFFFFE is inside a section there; +2 must not wrap to RVA 0
        // (the DOS header) and be accepted as the import name.
        let base = 0xFFFF_FF00u32;
        let mut payload = vec![0u8; 0x100];
        let int_rva = base + 0x40;
        let dll_rva = base + 0x50;
        payload[0..4].copy_from_slice(&int_rva.to_le_bytes());
        payload[12..16].copy_from_slice(&dll_rva.to_le_bytes());
        payload[16..20].copy_from_slice(&int_rva.to_le_bytes());
        payload[0x40..0x48].copy_from_slice(&0xFFFF_FFFEu64.to_le_bytes());
        payload[0x50..0x52].copy_from_slice(b"k\0");
        payload[0xFE..0x100].copy_from_slice(&1u16.to_le_bytes());
        let data = directory_at_rva(DIR_IMPORT, base, &payload);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.u16_at(0xFFFF_FFFE), Some(1));
        assert!(matches!(pe.imports(), Err(PeError::BadRva(_))));
    }

    #[test]
    fn unterminated_forwarders_are_not_export_addresses() {
        for terminated in [true, false] {
            let mut payload = vec![0u8; 60];
            payload[16..20].copy_from_slice(&1u32.to_le_bytes()); // ordinal base
            payload[20..24].copy_from_slice(&1u32.to_le_bytes()); // functions
            payload[24..28].copy_from_slice(&1u32.to_le_bytes()); // names
            payload[28..32].copy_from_slice(&0x1028u32.to_le_bytes()); // function table
            payload[32..36].copy_from_slice(&0x102Cu32.to_le_bytes()); // name table
            payload[36..40].copy_from_slice(&0x1030u32.to_le_bytes()); // ordinal table
            payload[40..44].copy_from_slice(&0x103Cu32.to_le_bytes()); // forwarder
            payload[44..48].copy_from_slice(&0x1032u32.to_le_bytes()); // export name
            payload[50..60].copy_from_slice(b"Forwarded\0");
            payload.extend_from_slice(b"other.Real");
            if terminated {
                payload.push(0);
            }
            let file = directory_at_rva(DIR_EXPORT, 0x1000, &payload);
            let mut mapped = vec![0; 0x1000 + payload.len()];
            mapped[..0x200].copy_from_slice(&file[..0x200]);
            mapped[0x1000..].copy_from_slice(&payload);
            let expected = terminated.then(|| ExportTarget::Forwarder("other.Real".into()));
            for (data, layout) in [(&file[..], Layout::File), (&mapped[..], Layout::Mapped)] {
                let pe = Pe::parse(data, layout).unwrap();
                let exports = pe.exports().unwrap().unwrap();
                assert_eq!(pe.export_target(&exports, 0x103C), expected, "{layout:?}, terminated={terminated}");
                let owned = OwnedExports::from_pe(&pe).unwrap();
                assert_eq!(owned.by_name.get("Forwarded"), expected.as_ref());
                assert_eq!(owned.by_ordinal.get(&1), expected.as_ref());
            }
        }
    }

    /// The ordinal base comes from the file: an export past u32::MAX is no ordinal and is skipped
    /// (it panicked on overflow in debug builds and wrapped to ordinal 0 in release ones).
    #[test]
    fn export_ordinals_past_u32_max_are_skipped() {
        let rva = 0x1000u32;
        let mut payload = vec![0u8; 48];
        payload[16..20].copy_from_slice(&u32::MAX.to_le_bytes()); // Base
        payload[20..24].copy_from_slice(&2u32.to_le_bytes()); // NumberOfFunctions
        payload[28..32].copy_from_slice(&(rva + 40).to_le_bytes()); // AddressOfFunctions
        payload[40..44].copy_from_slice(&0x2000u32.to_le_bytes());
        payload[44..48].copy_from_slice(&0x2004u32.to_le_bytes());
        let data = directory_at_rva(DIR_EXPORT, rva, &payload);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        let ex = OwnedExports::from_pe(&pe).unwrap();
        assert_eq!(ex.by_ordinal.len(), 1);
        assert_eq!(ex.by_ordinal.get(&u32::MAX), Some(&ExportTarget::Rva(0x2000)));
        assert!(!ex.by_ordinal.contains_key(&0));
    }

    #[test]
    fn import_descriptor_field_rva_overflow_is_error() {
        for offset in [12u32, 16] {
            let rva = u32::MAX - offset + 1;
            let data = directory_at_rva(DIR_IMPORT, rva, &[0; 20]);
            let pe = Pe::parse(&data, Layout::File).unwrap();
            assert_eq!(pe.u32_at(rva), Some(0));
            assert!(matches!(pe.imports(), Err(PeError::BadRva(_))), "offset {offset}");
        }
    }

    #[test]
    fn delay_import_descriptor_field_rva_overflow_is_error() {
        for offset in [4u32, 8, 12, 16] {
            let rva = u32::MAX - offset + 1;
            let data = directory_at_rva(DIR_DELAY_IMPORT, rva, &[0; 32]);
            let pe = Pe::parse(&data, Layout::File).unwrap();
            assert_eq!(pe.u32_at(rva), Some(0));
            assert!(matches!(pe.delay_imports(), Err(PeError::BadRva(_))), "offset {offset}");
        }
    }

    #[test]
    fn relocation_block_field_rva_overflow_is_error() {
        let data = directory_at_rva(DIR_BASERELOC, 0xFFFF_FFFC, &[0; 8]);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.u32_at(0xFFFF_FFFC), Some(0));
        assert!(matches!(pe.relocations(), Err(PeError::BadRva(_))));
    }

    #[test]
    fn relocation_entry_rva_overflow_is_error() {
        for (rva, size) in [(0xFFFF_FFF8, 10u32), (0xFFFF_FFF0, 18)] {
            let mut payload = vec![0u8; size as usize];
            payload[4..8].copy_from_slice(&size.to_le_bytes());
            let data = directory_at_rva(DIR_BASERELOC, rva, &payload);
            let pe = Pe::parse(&data, Layout::File).unwrap();
            assert_eq!(pe.u32_at(rva + 4), Some(size));
            assert!(matches!(pe.relocations(), Err(PeError::BadRva(_))), "RVA {rva:#x}");
        }
    }

    #[test]
    fn tls_field_rva_overflow_is_error() {
        for offset in [8u32, 16, 24, 32, 36] {
            let rva = u32::MAX - offset + 1;
            let data = directory_at_rva(DIR_TLS, rva, &[0; 40]);
            let pe = Pe::parse(&data, Layout::File).unwrap();
            assert_eq!(pe.u64_at(rva), Some(0));
            assert!(matches!(pe.tls(), Err(PeError::BadRva(_))), "offset {offset}");
        }
    }

    #[test]
    fn high_rva_directories_without_overflow_are_readable() {
        for rva in [0x1000, 0xFFFF_FF00] {
            let data = directory_at_rva(DIR_IMPORT, rva, &[0; 20]);
            assert!(Pe::parse(&data, Layout::File).unwrap().imports().unwrap().is_empty());
            let data = directory_at_rva(DIR_DELAY_IMPORT, rva, &[0; 32]);
            assert!(Pe::parse(&data, Layout::File).unwrap().delay_imports().unwrap().is_empty());
            let mut reloc = [0u8; 10];
            reloc[4..8].copy_from_slice(&10u32.to_le_bytes());
            reloc[8..10].copy_from_slice(&((IMAGE_REL_BASED_DIR64 << 12) | 0x20).to_le_bytes());
            let data = directory_at_rva(DIR_BASERELOC, rva, &reloc);
            assert_eq!(Pe::parse(&data, Layout::File).unwrap().relocations().unwrap(),
                vec![Reloc { rva: 0x20, kind: IMAGE_REL_BASED_DIR64 }]);
            let data = directory_at_rva(DIR_TLS, rva, &[0; 40]);
            assert_eq!(Pe::parse(&data, Layout::File).unwrap().tls().unwrap(), Some(TlsDir::default()));
        }
    }

    #[cfg(windows)]
    #[test]
    fn parses_kernel32_exports_and_imports() {
        let data = system_file("kernel32.dll");
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(pe.is_dll());
        assert!(pe.has_relocs());
        let ex = OwnedExports::from_pe(&pe).unwrap();
        assert!(ex.by_name.contains_key("CreateFileW"));
        assert!(ex.by_name.contains_key("GetProcAddress"));
        // kernel32 forwards plenty of functions to NTDLL / api sets
        assert!(ex.by_name.values().any(|t| matches!(t, ExportTarget::Forwarder(_))));
        let imps = pe.imports().unwrap();
        assert!(imps.iter().any(|d| d.dll.to_ascii_lowercase().starts_with("api-ms-win-")
            || d.dll.eq_ignore_ascii_case("ntdll.dll")));
        assert!(pe.relocations().unwrap().iter().all(|r| r.kind == IMAGE_REL_BASED_DIR64));
    }

    /// Malformed directories are errors, not panics or runaway loops (#106): a relocation block
    /// claiming more than the directory holds, import descriptors at the end of the address space.
    #[cfg(windows)]
    #[test]
    fn malformed_relocations_and_imports_are_errors() {
        let mut data = system_file("kernel32.dll");
        let (reloc, import) = {
            let pe = Pe::parse(&data, Layout::File).unwrap();
            (pe.dir(DIR_BASERELOC), pe.dir(DIR_IMPORT))
        };
        let pe = Pe::parse(&data, Layout::File).unwrap();
        let first_block = pe.rva_to_offset(reloc.rva).unwrap();
        drop(pe);
        // the first block's size: larger than the whole directory
        data[first_block + 4..first_block + 8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.relocations(), Err(PeError::BadRva(_))));
        drop(pe);
        data[first_block + 4..first_block + 8].copy_from_slice(&(reloc.size + 8).to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.relocations(), Err(PeError::BadRva(_))));
        drop(pe);
        // the import directory at the top of the address space: its descriptors' offsets overflow
        let dirs_off = {
            let pe = Pe::parse(&data, Layout::File).unwrap();
            // the data directories follow the optional header: find the import entry by value
            let needle = [import.rva.to_le_bytes(), import.size.to_le_bytes()].concat();
            data[..pe.size_of_headers as usize].windows(8).position(|w| w == needle.as_slice()).unwrap()
        };
        data[dirs_off..dirs_off + 4].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(pe.imports().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn parses_own_test_binary_tls() {
        // The test harness itself is an x64 Rust binary with static TLS.
        let me = std::env::current_exe().unwrap();
        let data = std::fs::read(me).unwrap();
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(!pe.is_dll());
        let tls = pe.tls().unwrap().expect("rust binaries have a TLS directory");
        assert!(tls.end_raw >= tls.start_raw);
        assert!(tls.alignment().is_power_of_two());
        assert!(pe.va_to_rva(tls.address_of_index).is_some());
    }

    #[test]
    fn tls_characteristics_alignment_decoding() {
        assert_eq!(tls_alignment_from_characteristics(0), 1);
        assert_eq!(sanitize_tls_characteristics(0), 0);
        for nibble in 1..=14u32 {
            let expected_align = 1u32 << (nibble - 1);
            let raw = (nibble << 20) | (0xDEAD_BEEF & !SCN_ALIGN_MASK); // include reserved noise bits outside mask
            let dir = TlsDir { characteristics: raw, ..Default::default() };
            assert_eq!(dir.alignment(), expected_align, "nibble {nibble}");
            assert_eq!(dir.align_characteristics(), nibble << 20, "nibble {nibble}");
        }
        // Specific common PE alignment flags:
        assert_eq!(tls_alignment_from_characteristics(0x0060_0000), 32);
        assert_eq!(tls_alignment_from_characteristics(0x0070_0000), 64);
        assert_eq!(tls_alignment_from_characteristics(0x00D0_0000), 4096);
        assert_eq!(tls_alignment_from_characteristics(0x00E0_0000), 8192);
        // Invalid nibble 0xF (IMAGE_SCN_ALIGN_MASK itself) maps to 1 / 0:
        assert_eq!(tls_alignment_from_characteristics(SCN_ALIGN_MASK), 1);
        assert_eq!(sanitize_tls_characteristics(SCN_ALIGN_MASK), 0);
    }

    // Builds a carrier DLL: Windows-only (the builder lives in the
    // `cfg(windows)` TLS carrier module; same reason as the `system_file`
    // tests above).
    #[cfg(windows)]
    #[test]
    fn parses_tls_characteristics_from_pe() {
        let mut bytes = crate::tls::build_carrier_dll_with(
            0x100,
            crate::tls::CarrierOpts { characteristics: 0x0070_0000, ..Default::default() },
        );
        let pe = Pe::parse(&bytes, Layout::File).unwrap();
        let tls = pe.tls().unwrap().unwrap();
        assert_eq!(tls.characteristics, 0x0070_0000);
        assert_eq!(tls.alignment(), 64);
        assert_eq!(tls.align_characteristics(), 0x0070_0000);

        // Patch raw Characteristics field directly in the PE to verify Pe::tls reads offset +36 verbatim.
        let tls_off = pe.rva_to_offset(pe.dir(DIR_TLS).rva).unwrap();
        drop(pe);
        bytes[tls_off + 36..tls_off + 40].copy_from_slice(&0x1260_00ABu32.to_le_bytes());
        let pe = Pe::parse(&bytes, Layout::File).unwrap();
        let tls = pe.tls().unwrap().unwrap();
        assert_eq!(tls.characteristics, 0x1260_00AB);
        assert_eq!(tls.alignment(), 32);
        assert_eq!(tls.align_characteristics(), 0x0060_0000);
    }

    #[test]
    fn truncated_directories_are_rejected() {
        // Import directory with size smaller than one descriptor (20 bytes)
        let data = directory_at_rva(DIR_IMPORT, 0x1000, &[0; 10]);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.imports(), Err(PeError::BadRva(_))));

        // Delay-import directory with size smaller than one descriptor (32 bytes)
        let data = directory_at_rva(DIR_DELAY_IMPORT, 0x1000, &[0; 16]);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.delay_imports(), Err(PeError::BadRva(_))));

        // Relocation directory with size smaller than one block header (8 bytes)
        let data = directory_at_rva(DIR_BASERELOC, 0x1000, &[0; 4]);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.relocations(), Err(PeError::BadRva(_))));

        // Export directory with size smaller than export directory structure (40 bytes)
        let data = directory_at_rva(DIR_EXPORT, 0x1000, &[0; 30]);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.exports(), Err(PeError::BadRva(_))));
    }

    #[test]
    fn import_directory_without_null_terminator_exceeding_size_is_error() {
        // Non-null import descriptor of 20 bytes, followed by only 10 bytes (not enough for 20-byte null descriptor)
        let mut payload = vec![0u8; 30];
        payload[0..4].copy_from_slice(&0x1000u32.to_le_bytes()); // int_rva
        payload[12..16].copy_from_slice(&0x1000u32.to_le_bytes()); // name_rva
        payload[16..20].copy_from_slice(&0x1000u32.to_le_bytes()); // iat_rva
        let data = directory_at_rva(DIR_IMPORT, 0x1000, &payload);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.imports(), Err(PeError::BadRva(_))));
    }

    #[test]
    fn delay_import_without_null_terminator_exceeding_size_is_error() {
        let mut payload = vec![0u8; 40];
        payload[4..8].copy_from_slice(&0x1000u32.to_le_bytes()); // name_rva
        payload[12..16].copy_from_slice(&0x1000u32.to_le_bytes()); // iat_rva
        let data = directory_at_rva(DIR_DELAY_IMPORT, 0x1000, &payload);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.delay_imports(), Err(PeError::BadRva(_))));
    }

    #[test]
    fn export_tables_exceeding_directory_bounds_is_error() {
        // Export directory claiming 100 functions (400 bytes), but directory size is only 48 bytes
        let mut payload = vec![0u8; 48];
        payload[20..24].copy_from_slice(&100u32.to_le_bytes()); // nfunc = 100
        payload[28..32].copy_from_slice(&0x1028u32.to_le_bytes()); // afunc at offset 40 in directory
        let data = directory_at_rva(DIR_EXPORT, 0x1000, &payload);
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.exports(), Err(PeError::BadRva(_))));
    }

    #[test]
    fn thunks_with_out_of_bounds_iat_slot_is_error() {
        // Import descriptor with IAT pointing past size_of_image (0x2000)
        let mut payload = vec![0u8; 48];
        payload[0..4].copy_from_slice(&0x1028u32.to_le_bytes()); // int_rva -> points to non-zero thunk
        payload[12..16].copy_from_slice(&0x1020u32.to_le_bytes()); // name_rva
        payload[16..20].copy_from_slice(&0x7FFF_0000u32.to_le_bytes()); // iat_rva outside image
        // thunk entry at 0x1028: ordinal 1
        payload[40..48].copy_from_slice(&(1u64 | (1u64 << 63)).to_le_bytes());
        let mut data = directory_at_rva(DIR_IMPORT, 0x1000, &payload);
        // set size_of_image in optional header (opt = 0x58, size_of_image at opt + 56)
        data[0x58 + 56..0x58 + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert!(matches!(pe.imports(), Err(PeError::BadRva(_))));
    }

    /// Minimal PE32+ with `SizeOfHeaders = 0x400`, one section at 0x1000, and room for a
    /// directory payload. Used to pin IAT / TLS write targets inside the headers (#836).
    fn pe_with_headers(size_of_headers: u32, image_base: u64, payload_rva: u32, payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 0x400 + payload.len().max(0x100)];
        data[..2].copy_from_slice(b"MZ");
        data[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        data[0x40..0x44].copy_from_slice(b"PE\0\0");
        data[0x44..0x46].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
        data[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        data[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        let opt = 0x58;
        data[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        data[opt + 24..opt + 32].copy_from_slice(&image_base.to_le_bytes());
        data[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes()); // SizeOfImage
        data[opt + 60..opt + 64].copy_from_slice(&size_of_headers.to_le_bytes());
        data[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let sec = opt + 240;
        data[sec..sec + 5].copy_from_slice(b".text");
        data[sec + 8..sec + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        data[sec + 12..sec + 16].copy_from_slice(&payload_rva.to_le_bytes());
        data[sec + 16..sec + 20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        data[sec + 20..sec + 24].copy_from_slice(&size_of_headers.to_le_bytes());
        let off = size_of_headers as usize;
        if data.len() < off + payload.len() {
            data.resize(off + payload.len(), 0);
        }
        data[off..off + payload.len()].copy_from_slice(payload);
        data
    }

    #[test]
    fn import_address_table_in_headers_is_error() {
        // INT + name live past the headers; FirstThunk (IAT) at 0x80 overwrites them.
        let mut payload = vec![0u8; 0x80];
        // IMAGE_IMPORT_DESCRIPTOR at payload+0: OriginalFirstThunk, Name, FirstThunk
        let int_rva: u32 = 0x1000 + 0x20;
        let name_rva: u32 = 0x1000 + 0x40;
        let iat_rva = 0x80u32;
        payload[0..4].copy_from_slice(&int_rva.to_le_bytes());
        payload[12..16].copy_from_slice(&name_rva.to_le_bytes());
        payload[16..20].copy_from_slice(&iat_rva.to_le_bytes());
        // one ordinal thunk, then null
        payload[0x20..0x28].copy_from_slice(&(1u64 << 63 | 1).to_le_bytes());
        payload[0x40..0x44].copy_from_slice(b"a\0\0\0");
        let mut data = pe_with_headers(0x400, 0x140000000, 0x1000, &payload);
        // point the import directory at the descriptor
        let opt = 0x58usize;
        let d = opt + 112 + DIR_IMPORT * 8;
        data[d..d + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[d + 4..d + 8].copy_from_slice(&0x28u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.size_of_headers, 0x400);
        assert_eq!(pe.imports().unwrap_err(), PeError::HeaderWriteTarget(0x80));
    }

    #[test]
    fn tls_index_in_headers_is_error() {
        let image_base = 0x140000000u64;
        let mut payload = vec![0u8; 40];
        // AddressOfIndex = image_base + 0x80 (inside SizeOfHeaders = 0x400)
        payload[16..24].copy_from_slice(&(image_base + 0x80).to_le_bytes());
        let mut data = pe_with_headers(0x400, image_base, 0x1000, &payload);
        let opt = 0x58usize;
        let d = opt + 112 + DIR_TLS * 8;
        data[d..d + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[d + 4..d + 8].copy_from_slice(&40u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.tls(), Err(PeError::HeaderWriteTarget(0x80)));
    }

    #[test]
    fn delay_import_module_handle_in_headers_is_error() {
        // Delay-load HMODULE slot at 0x80; IAT/INT/name sit past the headers.
        let mut payload = vec![0u8; 0x80];
        let name_rva: u32 = 0x1000 + 0x40;
        let hmod_rva = 0x80u32;
        let iat_rva: u32 = 0x1000 + 0x50;
        let int_rva: u32 = 0x1000 + 0x60;
        // IMAGE_DELAYLOAD_DESCRIPTOR: attrs=1 (RVA-based), Name, ModuleHandle, IAT, INT
        payload[0..4].copy_from_slice(&1u32.to_le_bytes());
        payload[4..8].copy_from_slice(&name_rva.to_le_bytes());
        payload[8..12].copy_from_slice(&hmod_rva.to_le_bytes());
        payload[12..16].copy_from_slice(&iat_rva.to_le_bytes());
        payload[16..20].copy_from_slice(&int_rva.to_le_bytes());
        payload[0x40..0x44].copy_from_slice(b"c\0\0\0");
        // empty IAT/INT (null terminator only) – the HMODULE write is still refused
        let mut data = pe_with_headers(0x400, 0x140000000, 0x1000, &payload);
        let opt = 0x58usize;
        let d = opt + 112 + DIR_DELAY_IMPORT * 8;
        data[d..d + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[d + 4..d + 8].copy_from_slice(&0x40u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.delay_imports().unwrap_err(), PeError::HeaderWriteTarget(0x80));
    }

    #[test]
    fn import_address_table_straddling_image_end_is_error() {
        // FirstThunk at SizeOfImage - 4: start is in-range, the 8-byte write is not.
        let size_of_image = 0x2000u32;
        let iat_rva = size_of_image - 4;
        let mut payload = vec![0u8; 0x80];
        let int_rva: u32 = 0x1000 + 0x20;
        let name_rva: u32 = 0x1000 + 0x40;
        payload[0..4].copy_from_slice(&int_rva.to_le_bytes());
        payload[12..16].copy_from_slice(&name_rva.to_le_bytes());
        payload[16..20].copy_from_slice(&iat_rva.to_le_bytes());
        payload[0x20..0x28].copy_from_slice(&(1u64 << 63 | 1).to_le_bytes());
        payload[0x40..0x44].copy_from_slice(b"a\0\0\0");
        let mut data = pe_with_headers(0x400, 0x140000000, 0x1000, &payload);
        // pe_with_headers already sets SizeOfImage to 0x2000
        let opt = 0x58usize;
        let d = opt + 112 + DIR_IMPORT * 8;
        data[d..d + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[d + 4..d + 8].copy_from_slice(&0x28u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.size_of_image, size_of_image);
        assert_eq!(pe.imports().unwrap_err(), PeError::BadRva(iat_rva));
    }

    #[test]
    fn tls_index_straddling_image_end_is_error() {
        let image_base = 0x140000000u64;
        let size_of_image = 0x2000u32;
        let index_rva = size_of_image - 2; // 4-byte write past the end
        let mut payload = vec![0u8; 40];
        payload[16..24].copy_from_slice(&(image_base + index_rva as u64).to_le_bytes());
        let mut data = pe_with_headers(0x400, image_base, 0x1000, &payload);
        let opt = 0x58usize;
        let d = opt + 112 + DIR_TLS * 8;
        data[d..d + 4].copy_from_slice(&0x1000u32.to_le_bytes());
        data[d + 4..d + 8].copy_from_slice(&40u32.to_le_bytes());
        let pe = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(pe.tls().unwrap_err(), PeError::BadRva(index_rva));
    }

    #[test]
    fn write_targets_at_size_of_headers_are_accepted() {
        // Boundary: an IAT slot and a TLS index at exactly SizeOfHeaders must load (#836).
        let image_base = 0x140000000u64;
        let headers = 0x400u32;
        let mut mapped = vec![0u8; 0x2000];
        mapped[..2].copy_from_slice(b"MZ");
        mapped[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        mapped[0x40..0x44].copy_from_slice(b"PE\0\0");
        mapped[0x44..0x46].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
        mapped[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        mapped[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        let opt = 0x58usize;
        mapped[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        mapped[opt + 24..opt + 32].copy_from_slice(&image_base.to_le_bytes());
        mapped[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        mapped[opt + 60..opt + 64].copy_from_slice(&headers.to_le_bytes());
        mapped[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        let sec = opt + 240;
        mapped[sec..sec + 5].copy_from_slice(b".text");
        mapped[sec + 8..sec + 12].copy_from_slice(&0x1C00u32.to_le_bytes());
        mapped[sec + 12..sec + 16].copy_from_slice(&headers.to_le_bytes());
        mapped[sec + 16..sec + 20].copy_from_slice(&0x1C00u32.to_le_bytes());
        mapped[sec + 20..sec + 24].copy_from_slice(&headers.to_le_bytes());

        mapped[headers as usize..headers as usize + 8]
            .copy_from_slice(&(1u64 << 63 | 7).to_le_bytes());
        mapped[headers as usize + 0x10..headers as usize + 0x14].copy_from_slice(b"b\0\0\0");
        let desc = headers as usize + 0x20;
        mapped[desc..desc + 4].copy_from_slice(&headers.to_le_bytes());
        mapped[desc + 12..desc + 16].copy_from_slice(&(headers + 0x10).to_le_bytes());
        mapped[desc + 16..desc + 20].copy_from_slice(&headers.to_le_bytes());
        let di = opt + 112 + DIR_IMPORT * 8;
        mapped[di..di + 4].copy_from_slice(&(headers + 0x20).to_le_bytes());
        mapped[di + 4..di + 8].copy_from_slice(&0x28u32.to_le_bytes());

        let tls_rva = 0x1800u32;
        let tls_at = tls_rva as usize;
        mapped[tls_at + 16..tls_at + 24]
            .copy_from_slice(&(image_base + headers as u64).to_le_bytes());
        let dt = opt + 112 + DIR_TLS * 8;
        mapped[dt..dt + 4].copy_from_slice(&tls_rva.to_le_bytes());
        mapped[dt + 4..dt + 8].copy_from_slice(&40u32.to_le_bytes());

        let pe = Pe::parse(&mapped, Layout::Mapped).unwrap();
        let imps = pe.imports().unwrap();
        assert_eq!(imps.len(), 1);
        assert_eq!(imps[0].entries[0].iat_rva, headers);
        let tls = pe.tls().unwrap().unwrap();
        assert_eq!(pe.va_to_rva(tls.address_of_index), Some(headers));
    }

    #[test]
    fn tls_dir_total_len_saturates_instead_of_overflowing() {
        let dir = TlsDir {
            start_raw: 0,
            end_raw: u64::MAX,
            size_of_zero_fill: 16,
            ..Default::default()
        };
        assert_eq!(dir.total_len(), u64::MAX);
    }

    // Needs kernel32.dll from disk: Windows-only (same reason as the other
    // `system_file` tests above). The synthetic twin below pins the same
    // boundaries on every platform.
    #[cfg(windows)]
    #[test]
    fn rva_to_offset_in_file_layout_rejects_offsets_past_eof() {
        let mut data = system_file("kernel32.dll");
        let pe = Pe::parse(&data, Layout::File).unwrap();
        let sec = pe.sections.iter().find(|s| s.raw_size >= 64 && s.raw_ptr as usize >= pe.size_of_headers as usize).unwrap().clone();
        // Truncate inside the section so raw_ptr + 16 lands exactly on data.len().
        let exact_end = sec.raw_ptr as usize + 16;
        let header_end = pe.size_of_headers as usize;
        drop(pe);
        data.truncate(exact_end.min(data.len()));
        let mid = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(mid.rva_to_offset(sec.va + 15), Some(data.len() - 1));
        assert_eq!(
            mid.rva_to_offset(sec.va + 16),
            None,
            "rva_to_offset must return None when raw_ptr + delta == data.len()"
        );
        // Truncate right after the headers so the section start itself is past EOF.
        data.truncate(header_end.min(data.len()));
        let truncated = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(
            truncated.rva_to_offset(sec.va),
            None,
            "rva_to_offset must return None when section raw_ptr + delta >= data.len()"
        );
    }

    #[test]
    fn rva_to_offset_in_file_layout_rejects_offsets_past_eof_synthetic() {
        // Linux-runnable twin of the kernel32 truncation test above: `pe_with_headers`
        // builds a minimal PE32+ with SizeOfHeaders = 0x400 and one section
        // (va 0x1000, raw_ptr 0x400, 64 raw bytes); the truncation boundaries
        // are identical, plus a mid-headers truncation for the header branch.
        let headers = 0x400u32;
        let full = pe_with_headers(headers, 0x14000_0000, 0x1000, &[0xAB; 64]);
        // Truncate inside the section so raw_ptr + 16 lands exactly on data.len().
        let mut data = full.clone();
        data.truncate(headers as usize + 16);
        let mid = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(mid.rva_to_offset(0x1000 + 15), Some(data.len() - 1));
        assert_eq!(
            mid.rva_to_offset(0x1000 + 16),
            None,
            "rva_to_offset must return None when raw_ptr + delta == data.len()"
        );
        // Truncate right after the headers so the section start itself is past EOF.
        data.truncate(headers as usize);
        let truncated = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(
            truncated.rva_to_offset(0x1000),
            None,
            "rva_to_offset must return None when section raw_ptr + delta >= data.len()"
        );
        // Truncate mid-headers: an rva inside the headers but past EOF is rejected too.
        data.truncate(0x200);
        let short = Pe::parse(&data, Layout::File).unwrap();
        assert_eq!(short.rva_to_offset(0x300), None);
    }
}
