//! Windows x64 `.pdata` / `.xdata` parser plus single-frame unwinding.
//!
//! Phase C step-11f: the Linux host cannot call `RtlAddFunctionTable` (there is no
//! Windows loader), so the HLEs in `hle/kernelx_linux.rs` read the mapped image's
//! exception directory directly through this module. Only the operations the test
//! title needs are unwound here (`PUSH_NONVOL`, `ALLOC_SMALL`, `ALLOC_LARGE` with
//! `OpInfo==0`, `SET_FPREG`, plus ignored `SAVE_XMM128` restores); anything else
//! (`SAVE_NONVOL`, chained info, machine frames, epilog markers, 32-bit large
//! allocs) returns a clean `Unsupported` error so `RtlVirtualUnwind` can fail
//! closed instead of computing a wrong `RSP`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin_rva: u32,
    pub end_rva: u32,
    pub unwind_rva: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwindOp {
    PushNonvol { offset: u8, reg: u8 },
    AllocSmall { offset: u8, size: u32 },
    AllocLarge { offset: u8, size: u32 },
    SetFpreg { offset: u8 },
    SaveNonvol { offset: u8, reg: u8, stack_offset: u32 },
    SaveNonvolFar { offset: u8, reg: u8, stack_offset: u32 },
    SaveXmm { offset: u8, reg: u8, stack_offset: u32 },
    SaveXmmFar { offset: u8, reg: u8, stack_offset: u32 },
    PushMachframe { offset: u8, with_error: bool },
    Epilog { offset: u8 },
    Spare { offset: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindInfo {
    pub version: u8,
    pub flags: u8,
    pub prolog_size: u8,
    pub frame_reg: u8,
    pub frame_offset_scaled: u8,
    pub codes: Vec<UnwindOp>,
    pub handler_rva: Option<u32>,
}

pub const UNW_FLAG_EHANDLER: u8 = 0x01;
pub const UNW_FLAG_UHANDLER: u8 = 0x02;
pub const UNW_FLAG_CHAININFO: u8 = 0x04;

/// Integer registers needed for `RtlVirtualUnwind`. Numbers match the unwind-code
/// register encoding (`RAX==0 … R15==15`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntRegs {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
}

impl IntRegs {
    pub fn get(&self, reg: u8) -> Option<u64> {
        match reg {
            0 => Some(self.rax),
            1 => Some(self.rcx),
            2 => Some(self.rdx),
            3 => Some(self.rbx),
            4 => Some(self.rsp),
            5 => Some(self.rbp),
            6 => Some(self.rsi),
            7 => Some(self.rdi),
            8 => Some(self.r8),
            9 => Some(self.r9),
            10 => Some(self.r10),
            11 => Some(self.r11),
            12 => Some(self.r12),
            13 => Some(self.r13),
            14 => Some(self.r14),
            15 => Some(self.r15),
            _ => None,
        }
    }

    pub fn set(&mut self, reg: u8, val: u64) -> bool {
        match reg {
            0 => self.rax = val,
            1 => self.rcx = val,
            2 => self.rdx = val,
            3 => self.rbx = val,
            4 => self.rsp = val,
            5 => self.rbp = val,
            6 => self.rsi = val,
            7 => self.rdi = val,
            8 => self.r8 = val,
            9 => self.r9 = val,
            10 => self.r10 = val,
            11 => self.r11 = val,
            12 => self.r12 = val,
            13 => self.r13 = val,
            14 => self.r14 = val,
            15 => self.r15 = val,
            _ => return false,
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindOutcome {
    pub new_rip: u64,
    pub new_rsp: u64,
    pub establisher: u64,
    pub handler: Option<u64>,
}

/// Parse one `UNWIND_INFO` from the start of `data`. `data` must begin at the
/// unwind RVA; trailing handler / chain bytes must be present when the flags
/// require them.
pub fn parse_unwind_info(data: &[u8]) -> Result<UnwindInfo, String> {
    if data.len() < 4 {
        return Err("UNWIND_INFO truncated (header)".into());
    }
    let b0 = data[0];
    let version = b0 & 0x7;
    let flags = b0 >> 3;
    if version != 1 && version != 2 {
        return Err(format!("unsupported UNWIND_INFO version {version}"));
    }
    let prolog_size = data[1];
    let count = data[2] as usize;
    let frame_reg = data[3] & 0xF;
    let frame_offset_scaled = (data[3] >> 4) & 0xF;
    // Codes occupy `count` 2-byte slots starting at offset 4.
    let codes_end = 4usize.saturating_add(count.saturating_mul(2));
    if data.len() < codes_end {
        return Err("UNWIND_INFO truncated (codes)".into());
    }
    let mut codes = Vec::new();
    let mut slot = 0usize;
    while slot < count {
        let off = 4 + slot * 2;
        let code_offset = data[off];
        let op_byte = data[off + 1];
        let op = op_byte & 0xF;
        let info = (op_byte >> 4) & 0xF;
        match op {
            0 => {
                // PUSH_NONVOL (1 slot).
                if info > 15 {
                    return Err("PUSH_NONVOL bad register".into());
                }
                codes.push(UnwindOp::PushNonvol { offset: code_offset, reg: info });
                slot += 1;
            }
            1 => {
                // ALLOC_LARGE (2 or 3 slots).
                if info == 0 {
                    if slot + 1 >= count {
                        return Err("ALLOC_LARGE truncated".into());
                    }
                    let lo = 4 + (slot + 1) * 2;
                    let v = u16::from_le_bytes([data[lo], data[lo + 1]]) as u32;
                    codes.push(UnwindOp::AllocLarge { offset: code_offset, size: v * 8 });
                    slot += 2;
                } else if info == 1 {
                    if slot + 2 >= count {
                        return Err("ALLOC_LARGE (32-bit) truncated".into());
                    }
                    let lo = 4 + (slot + 1) * 2;
                    let v = u32::from_le_bytes([data[lo], data[lo + 1], data[lo + 2], data[lo + 3]]);
                    codes.push(UnwindOp::AllocLarge { offset: code_offset, size: v });
                    slot += 3;
                } else {
                    return Err(format!("ALLOC_LARGE bad OpInfo {info}"));
                }
            }
            2 => {
                codes.push(UnwindOp::AllocSmall {
                    offset: code_offset,
                    size: info as u32 * 8 + 8,
                });
                slot += 1;
            }
            3 => {
                codes.push(UnwindOp::SetFpreg { offset: code_offset });
                slot += 1;
            }
            4 => {
                if slot + 1 >= count {
                    return Err("SAVE_NONVOL truncated".into());
                }
                let lo = 4 + (slot + 1) * 2;
                let v = u16::from_le_bytes([data[lo], data[lo + 1]]) as u32;
                codes.push(UnwindOp::SaveNonvol {
                    offset: code_offset,
                    reg: info,
                    stack_offset: v * 8,
                });
                slot += 2;
            }
            5 => {
                if slot + 2 >= count {
                    return Err("SAVE_NONVOL_FAR truncated".into());
                }
                let lo = 4 + (slot + 1) * 2;
                let v = u32::from_le_bytes([data[lo], data[lo + 1], data[lo + 2], data[lo + 3]]);
                codes.push(UnwindOp::SaveNonvolFar {
                    offset: code_offset,
                    reg: info,
                    stack_offset: v,
                });
                slot += 3;
            }
            6 => {
                codes.push(UnwindOp::Epilog { offset: code_offset });
                slot += 1;
            }
            7 => {
                codes.push(UnwindOp::Spare { offset: code_offset });
                slot += 1;
            }
            8 => {
                if slot + 1 >= count {
                    return Err("SAVE_XMM128 truncated".into());
                }
                let lo = 4 + (slot + 1) * 2;
                let v = u16::from_le_bytes([data[lo], data[lo + 1]]) as u32;
                codes.push(UnwindOp::SaveXmm {
                    offset: code_offset,
                    reg: info,
                    stack_offset: v * 16,
                });
                slot += 2;
            }
            9 => {
                if slot + 2 >= count {
                    return Err("SAVE_XMM128_FAR truncated".into());
                }
                let lo = 4 + (slot + 1) * 2;
                let v = u32::from_le_bytes([data[lo], data[lo + 1], data[lo + 2], data[lo + 3]]);
                codes.push(UnwindOp::SaveXmmFar {
                    offset: code_offset,
                    reg: info,
                    stack_offset: v,
                });
                slot += 3;
            }
            10 => {
                if info > 1 {
                    return Err(format!("PUSH_MACHFRAME bad OpInfo {info}"));
                }
                codes.push(UnwindOp::PushMachframe {
                    offset: code_offset,
                    with_error: info == 1,
                });
                slot += 1;
            }
            _ => return Err(format!("unknown unwind op {op}")),
        }
    }
    // Language-specific data starts at the first DWORD-aligned slot past the codes.
    let data_slot = (count + 1) & !1;
    let data_off = 4 + data_slot * 2;
    let mut handler_rva = None;
    if flags & UNW_FLAG_CHAININFO != 0 {
        // Chained unwind info (3 ULONGs) is not followed here; the caller must
        // follow the chain. Fail closed so VirtualUnwind never invents a frame.
        return Err("chained UNWIND_INFO is not supported in this slice".into());
    }
    if flags & (UNW_FLAG_EHANDLER | UNW_FLAG_UHANDLER) != 0 {
        if data.len() < data_off + 4 {
            return Err("UNWIND_INFO truncated (handler)".into());
        }
        handler_rva = Some(u32::from_le_bytes([
            data[data_off],
            data[data_off + 1],
            data[data_off + 2],
            data[data_off + 3],
        ]));
    }
    Ok(UnwindInfo {
        version,
        flags,
        prolog_size,
        frame_reg,
        frame_offset_scaled,
        codes,
        handler_rva,
    })
}

/// Parse the `.pdata` table (`count` 12-byte entries) from `data`.
pub fn parse_runtime_functions(data: &[u8]) -> Result<Vec<RuntimeFunction>, String> {
    if data.len() % 12 != 0 {
        return Err("RUNTIME_FUNCTION table size is not a multiple of 12".into());
    }
    let mut out = Vec::with_capacity(data.len() / 12);
    for chunk in data.chunks_exact(12) {
        let begin = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let end = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        let unwind = u32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
        out.push(RuntimeFunction { begin_rva: begin, end_rva: end, unwind_rva: unwind });
    }
    Ok(out)
}

/// Binary-search lookup: the table is sorted by `BeginAddress` per the PE spec.
pub fn lookup<'a>(table: &'a [RuntimeFunction], control_rva: u32) -> Option<&'a RuntimeFunction> {
    let mut lo = 0usize;
    let mut hi = table.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let f = &table[mid];
        if control_rva < f.begin_rva {
            hi = mid;
        } else if control_rva >= f.end_rva {
            lo = mid + 1;
        } else {
            return Some(f);
        }
    }
    None
}

/// Unwind one frame. `read_u64` reads guest stack memory; `regs` holds the
/// current integer context (`rip` is the control PC). On success `regs` is
/// updated to the caller context and the outcome reports the return address,
/// the establisher frame and the language-specific handler (if any).
///
/// Only fixed frames without `SAVE_NONVOL` / machine frames are unwound here;
/// anything else returns `Err` so the HLE can fail closed. `SAVE_XMM128` is
/// ignored (it does not move `RSP` and the test-title panic path never checks
/// `XMM`).
pub fn unwind_frame(
    regs: &mut IntRegs,
    read_u64: &dyn Fn(u64) -> Option<u64>,
    image_base: u64,
    func: &RuntimeFunction,
    info: &UnwindInfo,
) -> Result<UnwindOutcome, String> {
    if info.flags & UNW_FLAG_CHAININFO != 0 {
        return Err("chained unwind is not supported in this slice".into());
    }
    let control_rva = regs
        .rip
        .checked_sub(image_base)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| "control PC outside its image".to_string())?;
    if control_rva < func.begin_rva || control_rva >= func.end_rva {
        return Err("control PC outside RUNTIME_FUNCTION".into());
    }
    let rip_offset = control_rva - func.begin_rva;
    // Starting RSP: for frame-register functions recover the post-prolog RSP
    // from the frame pointer (this also covers dynamic allocs); otherwise the
    // current RSP already is the post-prolog value for fixed frames.
    let start_rsp = if info.frame_reg != 0 {
        let fp = regs.get(info.frame_reg).ok_or_else(|| "bad frame register".to_string())?;
        fp.wrapping_sub(info.frame_offset_scaled as u64 * 16)
    } else {
        regs.rsp
    };
    let mut cur_rsp = start_rsp;
    let in_prolog = rip_offset < info.prolog_size as u32;
    for op in &info.codes {
        let code_offset = match op {
            UnwindOp::PushNonvol { offset, .. }
            | UnwindOp::AllocSmall { offset, .. }
            | UnwindOp::AllocLarge { offset, .. }
            | UnwindOp::SetFpreg { offset }
            | UnwindOp::SaveNonvol { offset, .. }
            | UnwindOp::SaveNonvolFar { offset, .. }
            | UnwindOp::SaveXmm { offset, .. }
            | UnwindOp::SaveXmmFar { offset, .. }
            | UnwindOp::PushMachframe { offset, .. }
            | UnwindOp::Epilog { offset }
            | UnwindOp::Spare { offset } => *offset,
        };
        // Inside the prolog only the instructions up to the control PC have
        // executed; later unwind codes must be skipped (cf. vmi-os-windows).
        if in_prolog && (code_offset as u32) > rip_offset {
            continue;
        }
        match op {
            UnwindOp::PushNonvol { reg, .. } => {
                let v = read_u64(cur_rsp).ok_or_else(|| "stack read failed (push)".to_string())?;
                if !regs.set(*reg, v) {
                    return Err("bad PUSH_NONVOL register".into());
                }
                cur_rsp = cur_rsp.wrapping_add(8);
            }
            UnwindOp::AllocSmall { size, .. } | UnwindOp::AllocLarge { size, .. } => {
                cur_rsp = cur_rsp.wrapping_add(*size as u64);
            }
            UnwindOp::SetFpreg { .. } => {
                // No RSP movement: FP was derived from the post-prolog RSP above.
            }
            UnwindOp::SaveXmm { .. } | UnwindOp::SaveXmmFar { .. } => {
                // XMM restores do not move RSP and the panic path never checks them.
            }
            UnwindOp::SaveNonvol { .. }
            | UnwindOp::SaveNonvolFar { .. }
            | UnwindOp::PushMachframe { .. }
            | UnwindOp::Epilog { .. }
            | UnwindOp::Spare { .. } => {
                return Err("unwind op not supported in this slice (SAVE_NONVOL/MACHFRAME/EPILOG)".into());
            }
        }
    }
    let new_rip = read_u64(cur_rsp).ok_or_else(|| "stack read failed (return address)".to_string())?;
    let new_rsp = cur_rsp.wrapping_add(8);
    let establisher = start_rsp;
    let handler = info.handler_rva.map(|rva| image_base.wrapping_add(rva as u64));
    regs.rsp = new_rsp;
    regs.rip = new_rip;
    Ok(UnwindOutcome { new_rip, new_rsp, establisher, handler })
}

/// Copy `len` bytes at guest `base + rva` (bounds-checked against `size`).
pub fn guest_bytes(base: usize, size: usize, rva: u32, len: usize) -> Option<Vec<u8>> {
    let end = (rva as usize).checked_add(len)?;
    if end > size {
        return None;
    }
    let src = (base.checked_add(rva as usize)?) as *const u8;
    if src.is_null() {
        return None;
    }
    let mut out = vec![0u8; len];
    unsafe {
        std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len);
    }
    Some(out)
}

pub fn guest_u16(base: usize, size: usize, rva: u32) -> Option<u16> {
    guest_bytes(base, size, rva, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

pub fn guest_u32(base: usize, size: usize, rva: u32) -> Option<u32> {
    guest_bytes(base, size, rva, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

pub fn guest_u64(base: usize, size: usize, addr: u64) -> Option<u64> {
    if addr < base as u64 || addr.checked_add(8)? > (base as u64).checked_add(size as u64)? {
        return None;
    }
    unsafe { Some(std::ptr::read_unaligned(addr as *const u64)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwind_bytes(codes: &[u8], version: u8, flags: u8, prolog: u8, frame: u8) -> Vec<u8> {
        let mut v = vec![(version & 0x7) | (flags << 3), prolog, codes.len() as u8 / 2, frame];
        v.extend_from_slice(codes);
        // Pad to an even slot count for handler alignment.
        if (codes.len() / 2) % 2 == 1 {
            v.extend_from_slice(&[0, 0]);
        }
        v
    }

    #[test]
    fn parses_alloc_small_and_pushes() {
        // Codes (unwind order): ALLOC_SMALL 56, PUSH RBX, PUSH RBP.
        let codes = [0x10u8, 0x62, 0x0C, 0x30, 0x0B, 0x50];
        let data = unwind_bytes(&codes, 1, 0, 16, 0);
        let info = parse_unwind_info(&data).unwrap();
        assert_eq!(info.version, 1);
        assert_eq!(info.codes.len(), 3);
        assert_eq!(info.codes[0], UnwindOp::AllocSmall { offset: 0x10, size: 56 });
        assert_eq!(info.codes[1], UnwindOp::PushNonvol { offset: 0x0C, reg: 3 });
        assert_eq!(info.codes[2], UnwindOp::PushNonvol { offset: 0x0B, reg: 5 });
        assert_eq!(info.handler_rva, None);
    }

    #[test]
    fn parses_handler_rva() {
        let codes = [0x04u8, 0xC2];
        let mut data = unwind_bytes(&codes, 1, UNW_FLAG_EHANDLER | UNW_FLAG_UHANDLER, 4, 0);
        data.extend_from_slice(&0x1_234u32.to_le_bytes());
        let info = parse_unwind_info(&data).unwrap();
        assert_eq!(info.handler_rva, Some(0x1234));
    }

    #[test]
    fn rejects_chained_info() {
        let codes = [0x04u8, 0xC2];
        let data = unwind_bytes(&codes, 1, UNW_FLAG_CHAININFO, 4, 0);
        assert!(parse_unwind_info(&data).is_err());
    }

    #[test]
    fn parses_runtime_table_and_looks_up() {
        let mut raw = Vec::new();
        for (b, e, u) in [(0x1000u32, 0x1100u32, 0x5000u32), (0x1100, 0x1200, 0x5100)] {
            raw.extend_from_slice(&b.to_le_bytes());
            raw.extend_from_slice(&e.to_le_bytes());
            raw.extend_from_slice(&u.to_le_bytes());
        }
        let table = parse_runtime_functions(&raw).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(lookup(&table, 0x1050).unwrap().begin_rva, 0x1000);
        assert_eq!(lookup(&table, 0x1150).unwrap().begin_rva, 0x1100);
        assert!(lookup(&table, 0x1200).is_none());
        assert!(lookup(&table, 0x0FFF).is_none());
    }

    #[test]
    fn unwinds_push_plus_alloc_to_caller() {
        // Prolog: push rbx; sub rsp, 56. Unwind order: ALLOC_SMALL 56, PUSH RBX.
        let codes = [0x10u8, 0x62, 0x0C, 0x30];
        let data = unwind_bytes(&codes, 1, 0, 16, 0);
        let info = parse_unwind_info(&data).unwrap();
        let func = RuntimeFunction { begin_rva: 0x1000, end_rva: 0x1100, unwind_rva: 0x5000 };
        // Fake stack: [saved rbx][return address] at start_rsp.
        let saved_rbx = 0x1122_3344_5566_7788u64;
        let ret = 0x1400_0020_00u64;
        let stack_base = 0x1_0000u64;
        let mem = [(stack_base + 56, saved_rbx), (stack_base + 64, ret)];
        let read = |addr: u64| mem.iter().find(|(a, _)| *a == addr).map(|(_, v)| *v);
        let mut regs = IntRegs { rsp: stack_base, rip: 0x1400_0010_50, rbx: 0, ..Default::default() };
        let image_base = 0x1400_0000_00u64;
        // Control PC must be inside [begin, end): 0x1050.
        regs.rip = image_base + 0x1050;
        let out = unwind_frame(&mut regs, &read, image_base, &func, &info).unwrap();
        assert_eq!(out.new_rip, ret);
        assert_eq!(out.new_rsp, stack_base + 72);
        assert_eq!(regs.rbx, saved_rbx);
        assert_eq!(regs.rsp, stack_base + 72);
        assert_eq!(regs.rip, ret);
        assert_eq!(out.establisher, stack_base);
        assert_eq!(out.handler, None);
    }

    #[test]
    fn unwind_skips_codes_past_a_prolog_pc() {
        // Same frame, but the control PC is at offset 0x0B (after PUSH RBP but
        // before PUSH RBX at 0x0C and ALLOC at 0x10): only PUSH RBP applies.
        let codes = [0x10u8, 0x62, 0x0C, 0x30, 0x0B, 0x50];
        let data = unwind_bytes(&codes, 1, 0, 16, 0);
        let info = parse_unwind_info(&data).unwrap();
        let func = RuntimeFunction { begin_rva: 0x1000, end_rva: 0x1100, unwind_rva: 0x5000 };
        let saved_rbp = 0x99u64;
        let ret = 0xABu64;
        let stack_base = 0x2_0000u64;
        let mem = [(stack_base, saved_rbp), (stack_base + 8, ret)];
        let read = |addr: u64| mem.iter().find(|(a, _)| *a == addr).map(|(_, v)| *v);
        let mut regs = IntRegs {
            rsp: stack_base,
            rip: 0x1400_0000_00 + 0x100B,
            rbp: 0,
            rbx: 0xDEAD,
            ..Default::default()
        };
        let out = unwind_frame(&mut regs, &read, 0x1400_0000_00, &func, &info).unwrap();
        assert_eq!(regs.rbp, saved_rbp);
        assert_eq!(regs.rbx, 0xDEAD);
        assert_eq!(out.new_rsp, stack_base + 16);
    }

    #[test]
    fn rejects_save_nonvol_in_this_slice() {
        // SAVE_NONVOL RBX at [rsp+0x10]: parsed, but unwinding fails closed.
        let codes = [0x08u8, 0x34, 0x02, 0x00];
        let data = unwind_bytes(&codes, 1, 0, 8, 0);
        let info = parse_unwind_info(&data).unwrap();
        assert!(matches!(info.codes[0], UnwindOp::SaveNonvol { reg: 3, stack_offset: 16, .. }));
        let func = RuntimeFunction { begin_rva: 0x1000, end_rva: 0x1100, unwind_rva: 0x5000 };
        let mut regs = IntRegs { rsp: 0x1000, rip: 0x1400_0000_00 + 0x1050, ..Default::default() };
        let read = |_: u64| Some(0u64);
        assert!(unwind_frame(&mut regs, &read, 0x1400_0000_00, &func, &info).is_err());
    }

    /// Headless harness for the retail/test PE: parses the real `.pdata` when
    /// `XCL_SAMPLE_EXE` points at a PE, otherwise no-ops.
    #[test]
    fn parses_the_sample_pdata_when_set() {
        let Ok(path) = std::env::var("XCL_SAMPLE_EXE") else {
            println!("XCL_SAMPLE_EXE not set – skipping");
            return;
        };
        let bytes = std::fs::read(&path).expect("read sample exe");
        let pe = crate::pe::Pe::parse(&bytes, crate::pe::Layout::File).expect("parse sample");
        let dir = pe.dir(crate::pe::DIR_EXCEPTION);
        assert!(dir.rva != 0 && dir.size != 0, "sample has no .pdata");
        assert_eq!(dir.size % 12, 0);
        let raw = pe.bytes_at(dir.rva, dir.size as usize).expect("pdata bytes");
        let table = parse_runtime_functions(raw).expect("parse .pdata");
        assert!(!table.is_empty());
        // Sorted by BeginAddress per the PE spec.
        assert!(table.windows(2).all(|w| w[0].begin_rva <= w[1].begin_rva));
        // The first function unwinds (no chain).
        let first = &table[0];
        let info_raw = pe.bytes_at(first.unwind_rva, 64).expect("first unwind bytes");
        let info = parse_unwind_info(info_raw).expect("parse first UNWIND_INFO");
        assert!(info.version == 1 || info.version == 2);
    }
}
