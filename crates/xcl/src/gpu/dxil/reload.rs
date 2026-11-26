//! Scalar memory loads a program repeats: `s_load_dword*` of an address it has already
//! loaded, with nothing in between that could have changed the registers the address is made
//! of or the memory.
//!
//! The console's compiler reloads a descriptor from its table before each use, wherever its
//! registers were needed for something else meanwhile. Every load is a value of its own to
//! the translator, so what is derived from a descriptor is derived again per load: a retail
//! title's `ActivateRays` loads one buffer descriptor 2 399 times from one table slot, and the
//! decode of that one typed buffer's format (183 IR instructions per `buffer_load_format_x`,
//! 1 889 of them) was 57 % of its module. With the loads known to be repeats, the registers
//! keep the first load's values and the caches keyed on them (`Lw::fmt_desc_cache`) hit.
//!
//! This is the classic available-expressions analysis over the program's CFG: a load is
//! *available* at a point when on every path to it the same load has run and since then
//! - no instruction wrote the SGPRs its address is made of (base pair, offset register),
//! - no instruction of this program wrote memory (stores and atomics of any kind) and no
//!   `s_barrier` let another thread's writes become visible.
//!
//! An available load is not emitted: its registers get the earlier values back through SSA
//! variables (a phi where different earlier loads meet).

use std::collections::HashMap;

use super::cfg::{Cfg, Term};
use crate::gpu::rdna2::{self, Decoded};

/// The first SSA variable of the loads' values (16 a load); above `lower_mem::VAR_TMP`, so one
/// never written on a path reads 0 there (and is never read: the load is not available).
pub const VAR_RELOAD: u32 = 1 << 20;

/// What the lowering does with one `s_load_dword*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reload {
    /// its values live in variables `VAR_RELOAD + 16 * slot ..`
    pub slot: u32,
    /// the same load has run on every path here and nothing changed since: take the values
    pub available: bool,
}

/// One address a program loads from: (base register, offset register or 125, offset, dwords).
type Key = (u64, u64, u64, u64);

#[derive(Default)]
pub struct Reloads {
    /// (block, instruction index) → the load's slot and whether it is a repeat there
    at: HashMap<(usize, usize), Reload>,
}

impl Reloads {
    pub fn get(&self, block: usize, inst: usize) -> Option<Reload> {
        self.at.get(&(block, inst)).copied()
    }

    /// (loads with a slot, of them repeats)
    pub fn counts(&self) -> (usize, usize) {
        (self.at.len(), self.at.values().filter(|r| r.available).count())
    }
}

/// The key of a load this analysis follows: an `s_load_dword*` without GLC / DLC (a coherent
/// load asks for memory as it is now) whose destination is none of its own address registers.
fn key(d: &Decoded) -> Option<Key> {
    let n = match d.inst.name {
        "S_LOAD_DWORD" => 1,
        "S_LOAD_DWORDX2" => 2,
        "S_LOAD_DWORDX4" => 4,
        "S_LOAD_DWORDX8" => 8,
        "S_LOAD_DWORDX16" => 16,
        _ => return None,
    };
    let f = |name: &str| rdna2::field_value(d.enc, &d.words, name);
    if f("GLC") == Some(1) || f("DLC") == Some(1) {
        return None;
    }
    let (sbase, sdata) = (f("SBASE")? * rdna2::register_scale("SBASE"), f("SDATA")?);
    let soff = f("SOFFSET").unwrap_or(125);
    if sbase > 123 || (soff > 124 && soff != 125) {
        return None;
    }
    let k = (sbase, soff, f("OFFSET").unwrap_or(0), n);
    (sdata..sdata + n).all(|r| !reads(&k, r)).then_some(k)
}

/// Whether the address of `k` is made of SGPR `r`.
fn reads(k: &Key, r: u64) -> bool {
    r == k.0 || r == k.0 + 1 || r == k.1
}

/// What an instruction may change: the SGPRs it may write (a bit each), and whether it may
/// write memory or let other threads' writes be seen. Wider than what it does write wherever
/// that is not plain from its encoding.
fn changes(d: &Decoded) -> (u128, bool) {
    let name = d.inst.name;
    let f = |name: &str| rdna2::field_value(d.enc, &d.words, name);
    // s[x + m0] is written: any register
    if name.contains("MOVREL") {
        return (u128::MAX, false);
    }
    let pair = |r: u64, n: u64| (r..(r + n.max(2)).min(128)).fold(0u128, |m, r| m | 1 << r);
    let mut regs = 0u128;
    let mut scalar_out = false;
    for o in d.ienc.operands.iter().filter(|o| o.output) {
        match o.ty {
            "OPR_SDST" | "OPR_SREG" | "OPR_EXEC" | "OPR_VCC" | "OPR_SDST_EXEC" => scalar_out = true,
            "OPR_VGPR" | "OPR_SRC_VGPR" | "OPR_DSMEM" | "OPR_GPUMEM" | "OPR_SSRC_SPECIAL_SCC" | "OPR_PC" | "OPR_TGT" => {}
            // an operand type this does not know (OPR_HWREG among them: `s_setreg`): any register
            _ => return (u128::MAX, true),
        }
        if matches!(o.ty, "OPR_EXEC" | "OPR_VCC" | "OPR_SDST_EXEC") {
            regs |= pair(106, 2) | pair(126, 2);
        }
    }
    if scalar_out {
        // wherever the encoding names the destination (a VOP3 compare's and a lane read's is
        // in VDST), over the operand's dwords and at least a pair
        let dwords = d.ienc.operands.iter().filter(|o| o.output).map(|o| (o.size as u64).div_ceil(32)).max().unwrap_or(1);
        let dwords = match name.strip_prefix("S_LOAD_DWORDX").or_else(|| name.strip_prefix("S_BUFFER_LOAD_DWORDX")) {
            Some(n) => n.parse().unwrap_or(16),
            None => dwords,
        };
        for field in ["SDST", "SDATA", "VDST"] {
            if let Some(r) = f(field).filter(|&r| r < 128) {
                regs |= pair(r, dwords);
            }
        }
        // an SDWA compare with SD set writes the pair in the extension dword's SDST (125: none)
        if d.variant == rdna2::Variant::Sdwa {
            if let Some(e) = super::lower::ext_encoding(d).filter(|e| rdna2::field_value(e, &d.words, "SD") == Some(1)) {
                if let Some(r) = rdna2::field_value(e, &d.words, "SDST").filter(|&r| r < 125) {
                    regs |= pair(r, 2);
                }
            }
        }
    }
    let writes = name.contains("_STORE") || name.contains("_ATOMIC");
    let memory = matches!(name, "S_BARRIER" | "S_DCACHE_INV" | "S_GL1_INV" | "BUFFER_GL0_INV" | "BUFFER_GL1_INV" | "DS_GWS_BARRIER")
        || (writes && matches!(d.enc.name, "ENC_MUBUF" | "ENC_MTBUF" | "ENC_FLAT" | "ENC_FLAT_GLBL" | "ENC_FLAT_SCRATCH" | "ENC_MIMG" | "ENC_SMEM"));
    (regs, memory)
}

/// A set of keys, a bit each.
#[derive(Clone, PartialEq, Eq)]
struct Set(Vec<u64>);

impl Set {
    fn new(n: usize, full: bool) -> Set {
        Set(vec![if full { u64::MAX } else { 0 }; n.div_ceil(64).max(1)])
    }
    fn has(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 != 0
    }
    fn set(&mut self, i: usize, on: bool) {
        if on {
            self.0[i / 64] |= 1 << (i % 64);
        } else {
            self.0[i / 64] &= !(1 << (i % 64));
        }
    }
    fn meet(&mut self, o: &Set) {
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            *a &= b;
        }
    }
}

/// The repeated loads of `cfg`. None for a program with a call (its return address is written
/// by a branch, which no block's instructions hold).
pub fn analyze(cfg: &Cfg) -> Reloads {
    let mut out = Reloads::default();
    if cfg.insts.iter().any(|d| d.inst.name == "S_CALL_B64") {
        return out;
    }
    let mut slots: HashMap<Key, usize> = HashMap::new();
    let keys: Vec<Option<usize>> = cfg
        .insts
        .iter()
        .map(|d| {
            key(d).map(|k| {
                let n = slots.len();
                *slots.entry(k).or_insert(n)
            })
        })
        .collect();
    if slots.is_empty() {
        return out;
    }
    let mut by_slot: Vec<Key> = vec![(0, 0, 0, 0); slots.len()];
    for (k, &i) in &slots {
        by_slot[i] = *k;
    }
    let n = by_slot.len();
    // one instruction: kill what it changes, then its own load is there
    let step = |i: usize, s: &mut Set| {
        let (regs, memory) = changes(&cfg.insts[i]);
        if memory {
            *s = Set::new(n, false);
        } else if regs != 0 {
            for (j, k) in by_slot.iter().enumerate() {
                if (0..128).any(|r| regs >> r & 1 != 0 && reads(k, r)) {
                    s.set(j, false);
                }
            }
        }
        if let Some(j) = keys[i] {
            s.set(j, true);
        }
    };
    // a block's instructions, its branch included (it writes nothing this follows, but is
    // checked like the rest)
    let insts = |b: usize| {
        let blk = &cfg.blocks[b];
        let branch = match blk.term {
            Term::Cond(i, ..) => Some(i),
            _ => None,
        };
        blk.insts.iter().copied().chain(branch)
    };
    let reachable: Vec<bool> = {
        let mut r = vec![false; cfg.blocks.len()];
        for &b in &cfg.rpo {
            r[b] = true;
        }
        r
    };
    let entry = cfg.rpo.first().copied();
    let at_entry = |b: usize, outs: &[Set]| {
        let mut s = Set::new(n, Some(b) != entry);
        for &p in cfg.blocks[b].preds.iter().filter(|&&p| reachable[p]) {
            s.meet(&outs[p]);
        }
        s
    };
    // greatest fixpoint: every block's end starts as "everything", the entry's start as nothing
    let mut outs: Vec<Set> = vec![Set::new(n, true); cfg.blocks.len()];
    loop {
        let mut changed = false;
        for &b in &cfg.rpo {
            let mut s = at_entry(b, &outs);
            for i in insts(b) {
                step(i, &mut s);
            }
            if s != outs[b] {
                outs[b] = s;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for &b in &cfg.rpo {
        let mut s = at_entry(b, &outs);
        for i in insts(b) {
            if let Some(j) = keys[i] {
                out.at.insert((b, i), Reload { slot: j as u32, available: s.has(j) });
            }
            step(i, &mut s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type I = (&'static str, &'static str, &'static [(&'static str, u64)]);

    fn cfg(insts: &[I]) -> Cfg {
        let code: Vec<u8> = insts.iter().flat_map(|(n, e, f)| rdna2::encode(n, e, f, None).unwrap_or_else(|e| panic!("{n}: {e}"))).collect();
        Cfg::build(&code).unwrap()
    }

    /// (instruction index, available) of every followed load, in program order.
    fn loads(insts: &[I]) -> Vec<(usize, bool)> {
        let c = cfg(insts);
        let r = analyze(&c);
        let mut v: Vec<(usize, bool)> = r.at.iter().map(|(&(_, i), l)| (i, l.available)).collect();
        v.sort();
        v
    }

    // s_load_dwordx4 s[12:15], s[4:5], 0x40
    const LOAD: I = ("S_LOAD_DWORDX4", "ENC_SMEM", &[("SDATA", 12), ("SBASE", 2), ("OFFSET", 0x40), ("SOFFSET", 125)]);
    const OTHER: I = ("S_LOAD_DWORDX4", "ENC_SMEM", &[("SDATA", 12), ("SBASE", 2), ("OFFSET", 0x80), ("SOFFSET", 125)]);
    const ADD: I = ("S_ADD_U32", "ENC_SOP2", &[("SDST", 20), ("SSRC0", 12), ("SSRC1", 129)]);
    const END: I = ("S_ENDPGM", "ENC_SOPP", &[]);

    #[test]
    fn a_load_repeated_with_nothing_changed_is_available() {
        // the registers it wrote may be written meanwhile: the address is what counts
        let clobber: I = ("S_MOV_B32", "ENC_SOP1", &[("SDST", 12), ("SSRC0", 129)]);
        assert_eq!(loads(&[LOAD, ADD, clobber, LOAD, END]), [(0, false), (3, true)]);
        // another slot of the table is another load; each is a repeat of its own
        assert_eq!(loads(&[LOAD, OTHER, LOAD, OTHER, END]), [(0, false), (1, false), (2, true), (3, true)]);
    }

    #[test]
    fn a_write_to_the_address_registers_or_to_memory_ends_it() {
        let base_lo: I = ("S_MOV_B32", "ENC_SOP1", &[("SDST", 4), ("SSRC0", 129)]);
        let base_hi: I = ("S_ADD_U32", "ENC_SOP2", &[("SDST", 5), ("SSRC0", 5), ("SSRC1", 129)]);
        // a 64-bit write of s[3:4] reaches the base's low dword
        let wide: I = ("S_MOV_B64", "ENC_SOP1", &[("SDST", 3), ("SSRC0", 8)]);
        let lane: I = ("V_READFIRSTLANE_B32", "ENC_VOP1", &[("VDST", 5), ("SRC0", 256)]);
        let store: I = ("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 2)]);
        let atomic: I = ("BUFFER_ATOMIC_ADD", "ENC_MUBUF", &[("VDATA", 1), ("SRSRC", 2)]);
        let image: I = ("IMAGE_STORE", "ENC_MIMG", &[("DMASK", 15), ("VADDR", 0), ("VDATA", 4), ("SRSRC", 2)]);
        let barrier: I = ("S_BARRIER", "ENC_SOPP", &[]);
        for k in [base_lo, base_hi, wide, lane, store, atomic, image, barrier] {
            assert_eq!(loads(&[LOAD, k, LOAD, END]), [(0, false), (2, false)], "{}", k.0);
        }
        // a memory read, a vector instruction and a compare into VCC change nothing of it
        let read: I = ("GLOBAL_LOAD_DWORD", "ENC_FLAT_GLBL", &[("VDST", 2), ("ADDR", 1), ("SADDR", 125)]);
        let vmov: I = ("V_MOV_B32", "ENC_VOP1", &[("VDST", 5), ("SRC0", 129)]);
        let cmp: I = ("V_CMP_GT_U32", "ENC_VOPC", &[("SRC0", 144), ("VSRC1", 0)]);
        for k in [read, vmov, cmp] {
            assert_eq!(loads(&[LOAD, k, LOAD, END]), [(0, false), (2, true)], "{}", k.0);
        }
    }

    #[test]
    fn an_sdwa_compare_into_an_sgpr_pair_ends_it() {
        // v_cmp_lt_f32_sdwa s[x:x+1], v1, v2 (SD = 1): the SDST is in the extension dword, not in
        // the base words
        let code = |sdst: u32| -> Vec<u8> {
            let mut v = Vec::new();
            for i in [LOAD] {
                v.extend(rdna2::encode(i.0, i.1, i.2, None).unwrap());
            }
            for w in [0x7c02_04f9u32, 0x0606_8001 | sdst << 8] {
                v.extend(w.to_le_bytes());
            }
            for i in [LOAD, END] {
                v.extend(rdna2::encode(i.0, i.1, i.2, None).unwrap());
            }
            v
        };
        let avail = |sdst: u32| {
            let c = Cfg::build(&code(sdst)).unwrap();
            let mut v: Vec<(usize, bool)> = analyze(&c).at.iter().map(|(&(_, i), l)| (i, l.available)).collect();
            v.sort();
            v
        };
        // the base is s[4:5]: a write of s4 or of s[3:4] reaches it, one of s[6:7] does not
        assert_eq!(avail(4), [(0, false), (2, false)]);
        assert_eq!(avail(5), [(0, false), (2, false)]);
        assert_eq!(avail(6), [(0, false), (2, true)]);
        // SDST = 125 (null) writes nothing
        assert_eq!(avail(125), [(0, false), (2, true)]);
    }

    #[test]
    fn loads_that_cannot_be_followed_are_left_alone() {
        // a coherent load, and one that overwrites its own base
        let glc: I = ("S_LOAD_DWORDX4", "ENC_SMEM", &[("SDATA", 12), ("SBASE", 2), ("OFFSET", 0x40), ("SOFFSET", 125), ("GLC", 1)]);
        let own: I = ("S_LOAD_DWORDX4", "ENC_SMEM", &[("SDATA", 4), ("SBASE", 2), ("OFFSET", 0x40), ("SOFFSET", 125)]);
        assert_eq!(loads(&[glc, glc, own, own, END]), []);
        // an offset register is part of the address
        let off: I = ("S_LOAD_DWORDX2", "ENC_SMEM", &[("SDATA", 12), ("SBASE", 2), ("OFFSET", 0), ("SOFFSET", 9)]);
        let bump: I = ("S_ADD_U32", "ENC_SOP2", &[("SDST", 9), ("SSRC0", 9), ("SSRC1", 132)]);
        assert_eq!(loads(&[off, off, bump, off, END]), [(0, false), (1, true), (3, false)]);
    }

    #[test]
    fn it_takes_every_path() {
        let cmp: I = ("S_CMP_EQ_U32", "ENC_SOPC", &[("SSRC0", 8), ("SSRC1", 129)]);
        // (branch offsets in dwords: a load and a store are two, the rest one)
        let skip1: I = ("S_CBRANCH_SCC1", "ENC_SOPP", &[("SIMM16", 1)]);
        let skip2: I = ("S_CBRANCH_SCC1", "ENC_SOPP", &[("SIMM16", 2)]);
        // loaded on one arm only: not available after the join
        assert_eq!(loads(&[cmp, skip2, LOAD, LOAD, END]), [(2, false), (3, false)]);
        // loaded before the branch, a store on one arm: not available after the join either
        let store: I = ("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 2)]);
        assert_eq!(loads(&[LOAD, cmp, skip2, store, LOAD, END]), [(0, false), (4, false)]);
        // nothing on either arm: available
        assert_eq!(loads(&[LOAD, cmp, skip1, ADD, LOAD, END]), [(0, false), (4, true)]);
        // a loop: the load at its head is a repeat of the one before it and of itself around
        // the back edge; with a store in the body it is neither
        let back5: I = ("S_CBRANCH_SCC0", "ENC_SOPP", &[("SIMM16", 0xFFFB)]);
        assert_eq!(loads(&[LOAD, LOAD, ADD, cmp, back5, LOAD, END]), [(0, false), (1, true), (5, true)]);
        let back7: I = ("S_CBRANCH_SCC0", "ENC_SOPP", &[("SIMM16", 0xFFF9)]);
        assert_eq!(loads(&[LOAD, LOAD, ADD, store, cmp, back7, LOAD, END]), [(0, false), (1, false), (6, false)]);
    }

    /// The translator emits a repeated load once: its registers get the first load's values
    /// (here the sum of both loads' first dword is twice one value), and a load after a store
    /// is made again.
    // GPU/DXC-backed translation test; runner/validate are Windows-only
    // until the SPIR-V backend lands.
    #[cfg(windows)]
    #[test]
    fn the_translator_emits_a_repeated_load_once() {
        let buffer_loads = |insts: &[I]| {
            let code: Vec<u8> = insts.iter().flat_map(|(n, e, f)| rdna2::encode(n, e, f, None).unwrap()).collect();
            let k = crate::gpu::recomp::runner::direct_kernel_full(&code, false).expect("direct translation");
            super::super::validate::validate(&k.dxil).expect("valid DXIL");
            let dis = super::super::validate::disassemble(&k.dxil).expect("disassemble");
            dis.lines().filter(|l| l.contains("@dx.op.rawBufferLoad") && l.contains(" = call ")).count()
        };
        let sum: I = ("S_ADD_U32", "ENC_SOP2", &[("SDST", 21), ("SSRC0", 20), ("SSRC1", 12)]);
        let store: I = ("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 2), ("SADDR", 125)]);
        // (the kernel's own loads of its registers from `io` are counted in both)
        let once = buffer_loads(&[LOAD, ADD, END]);
        assert_eq!(buffer_loads(&[LOAD, ADD, LOAD, sum, END]), once, "the second load is the first one's values");
        assert_eq!(buffer_loads(&[LOAD, ADD, store, LOAD, sum, END]), buffer_loads(&[LOAD, ADD, store, sum, END]) + 1, "a store in between: loaded again");
    }

    /// Every output operand type of the instruction tables is one `changes` knows: a new one
    /// must be looked at here, not passed over.
    #[test]
    fn every_output_operand_type_is_accounted_for() {
        const KNOWN: &[&str] = &["OPR_SDST", "OPR_SREG", "OPR_EXEC", "OPR_VCC", "OPR_SDST_EXEC", "OPR_VGPR", "OPR_SRC_VGPR", "OPR_DSMEM", "OPR_GPUMEM", "OPR_SSRC_SPECIAL_SCC", "OPR_PC", "OPR_TGT", "OPR_HWREG"];
        for inst in rdna2::all_instructions() {
            for o in inst.encs.iter().flat_map(|e| e.operands.iter()).filter(|o| o.output) {
                assert!(KNOWN.contains(&o.ty), "{}: output operand type {}", inst.name, o.ty);
            }
        }
    }

    #[test]
    fn vopc_sdwa_sdst_invalidates_address_registers() {
        let sdwa = |sd: u64, sdst: u64| -> Vec<u8> {
            rdna2::encode("V_CMP_EQ_U32", "VOPC_VOP_SDWA_SDST_ENC", &[("SRC0", 0xF9), ("VSRC0", 0), ("VSRC1", 1), ("SRC0_SEL", 6), ("SRC1_SEL", 6), ("SD", sd), ("SDST", sdst)], None).unwrap()
        };
        let run = |ld: I, mid: Vec<u8>| {
            let enc = |(n, e, f): I| rdna2::encode(n, e, f, None).unwrap();
            let code: Vec<u8> = [enc(ld), mid, enc(ld), enc(END)].concat();
            let r = analyze(&Cfg::build(&code).unwrap());
            let mut v: Vec<(usize, bool)> = r.at.iter().map(|(&(_, i), l)| (i, l.available)).collect();
            v.sort();
            v
        };
        for sdst in [3, 4, 5] {
            assert_eq!(run(LOAD, sdwa(1, sdst)), [(0, false), (2, false)], "base SDST=s{sdst}");
        }
        let off: I = ("S_LOAD_DWORDX2", "ENC_SMEM", &[("SDATA", 12), ("SBASE", 2), ("OFFSET", 0), ("SOFFSET", 9)]);
        for sdst in [8, 9] {
            assert_eq!(run(off, sdwa(1, sdst)), [(0, false), (2, false)], "soffset SDST=s{sdst}");
        }
        assert_eq!(run(LOAD, sdwa(0, 4)), [(0, false), (2, true)], "SD=0 writes VCC");
        assert_eq!(run(LOAD, sdwa(1, 125)), [(0, false), (2, true)], "SDST=NULL writes nothing");
    }

    #[test]
    fn cache_invalidations_and_gws_barrier_end_available_loads() {
        let gl0: I = ("BUFFER_GL0_INV", "ENC_MUBUF", &[]);
        let gl1: I = ("BUFFER_GL1_INV", "ENC_MUBUF", &[]);
        let gws: I = ("DS_GWS_BARRIER", "ENC_DS", &[("ADDR", 0)]);
        for k in [gl0, gl1, gws] {
            assert_eq!(loads(&[LOAD, k, LOAD, END]), [(0, false), (2, false)], "{}", k.0);
        }
        let icache: I = ("S_ICACHE_INV", "ENC_SOPP", &[]);
        assert_eq!(loads(&[LOAD, icache, LOAD, END]), [(0, false), (2, true)], "S_ICACHE_INV");
    }
}
