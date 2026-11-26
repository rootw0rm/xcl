//! Which loads of a program read at a code address: the constant data the console compiler puts
//! after the program (`Options::rodata`), which the program finds with `s_getpc_b64` plus an
//! offset. A load whose address does not come from `s_getpc_b64` needs no look at the table,
//! and one whose address comes only from it needs no memory access.

use std::collections::BTreeMap;

use super::cfg::Cfg;
use crate::gpu::rdna2::{self, Decoded};

/// `code_loads`: how a load's address relates to the code region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodeLoad {
    /// on every path the address's high dword comes from `s_getpc_b64` (with offsets added):
    /// the dwords are the table's
    Code,
    /// it may or may not: the address decides at run time
    Maybe,
}

/// What a register dword holds, as far as the code region goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Data,
    Code,
    Maybe,
}

fn join(a: K, b: K) -> K {
    if a == b { a } else { K::Maybe }
}

/// s0..s127, then v0..v255
const REGS: usize = 128 + 256;

/// The loads (`s_load*` / `s_buffer_load*`, FLAT / GLOBAL) whose address may be a code address,
/// by pc; the others read the memory. A program whose control flow is not known (an indirect
/// jump) has every such load `Maybe`.
pub fn code_loads(insts: &[Decoded]) -> BTreeMap<u64, CodeLoad> {
    let mut out = BTreeMap::new();
    let Some(cfg) = Cfg::new(insts) else {
        for d in insts.iter().filter(|d| address(d).is_some()) {
            out.insert(d.pc, CodeLoad::Maybe);
        }
        return out;
    };
    // forward dataflow over the blocks: the registers' kinds at each block's start
    let nb = cfg.start.len();
    let mut at: Vec<Option<Vec<K>>> = vec![None; nb];
    if nb == 0 {
        return out;
    }
    at[0] = Some(vec![K::Data; REGS]);
    let mut work = vec![0usize];
    while let Some(b) = work.pop() {
        let mut st = at[b].clone().expect("a reached block");
        for d in &insts[cfg.range(b)] {
            step(&mut st, d);
        }
        for &s in &cfg.succ[b] {
            let changed = match &mut at[s] {
                Some(t) => {
                    let mut c = false;
                    for (x, y) in t.iter_mut().zip(&st) {
                        let j = join(*x, *y);
                        c |= j != *x;
                        *x = j;
                    }
                    c
                }
                None => {
                    at[s] = Some(st.clone());
                    true
                }
            };
            if changed && !work.contains(&s) {
                work.push(s);
            }
        }
    }
    for b in 0..nb {
        let Some(mut st) = at[b].clone() else { continue };
        for d in &insts[cfg.range(b)] {
            if let Some((hi, lo)) = address(d) {
                let kind = match st[hi] {
                    K::Code => Some(CodeLoad::Code),
                    K::Maybe => Some(CodeLoad::Maybe),
                    K::Data => lo.iter().any(|&r| st[r] != K::Data).then_some(CodeLoad::Maybe),
                };
                if let Some(k) = kind {
                    out.insert(d.pc, k);
                }
            }
            step(&mut st, d);
        }
    }
    out
}

/// The registers of a load's address: (the high dword's, the low dword's).
fn address(d: &Decoded) -> Option<(usize, Vec<usize>)> {
    let f = |n: &str| rdna2::field_value(d.enc, &d.words, n);
    let name = d.inst.name;
    match d.enc.name {
        "ENC_SMEM" if name.starts_with("S_LOAD_") || name.starts_with("S_BUFFER_LOAD_") => {
            let b = (f("SBASE")? * rdna2::register_scale("SBASE")) as usize;
            Some((b + 1, vec![b]))
        }
        "ENC_FLAT" | "ENC_FLAT_GLBL" if name.contains("_LOAD_") => {
            let (v, s) = (128 + f("ADDR").unwrap_or(0) as usize, f("SADDR").unwrap_or(0x7F) as usize);
            if d.enc.name == "ENC_FLAT_GLBL" && s != 0x7D && s != 0x7F {
                Some((s + 1, vec![s, v]))
            } else {
                Some((v + 1, vec![v]))
            }
        }
        _ => None,
    }
}

/// The kinds after `d`: `s_getpc_b64` gives code addresses, a load gives data, a select joins
/// what it selects from, and any other instruction gives a code address when it computes with
/// one (an offset added to it).
fn step(st: &mut [K], d: &Decoded) {
    let (sr, sw) = rdna2::sgpr_uses(d);
    let (vr, vw) = rdna2::vgpr_uses(d);
    let reads = sr.iter().map(|&r| r as usize).chain(vr.iter().map(|&r| 128 + r as usize)).filter(|&r| r < REGS);
    let k = if d.inst.name == "S_GETPC_B64" {
        K::Code
    } else if matches!(d.enc.name, "ENC_SMEM" | "ENC_MUBUF" | "ENC_MTBUF" | "ENC_FLAT" | "ENC_FLAT_GLBL" | "ENC_FLAT_SCRATCH" | "ENC_DS" | "ENC_MIMG") {
        K::Data
    } else if matches!(d.inst.name, "V_CNDMASK_B32" | "S_CSELECT_B32" | "S_CSELECT_B64") {
        reads.map(|r| st[r]).reduce(join).unwrap_or(K::Data)
    } else {
        let mut k = K::Data;
        for r in reads {
            match st[r] {
                K::Maybe => k = K::Maybe,
                K::Code if k == K::Data => k = K::Code,
                _ => {}
            }
        }
        k
    };
    for w in sw.iter().map(|&r| r as usize).chain(vw.iter().map(|&r| 128 + r as usize)).filter(|&r| r < REGS) {
        st[w] = k;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insts(prog: &[(&str, &str, &[(&str, u64)], Option<u32>)]) -> Vec<Decoded> {
        let mut code = Vec::new();
        for (inst, enc, fields, lit) in prog {
            code.extend(rdna2::encode(inst, enc, fields, *lit).unwrap());
        }
        let mut out = Vec::new();
        let mut off = 0;
        while off < code.len() {
            let d = rdna2::decode(&code[off..], off as u64).unwrap();
            off += d.len;
            out.push(d);
        }
        out
    }

    /// The pattern of a retail LUT bake: the code address plus a scaled index in VGPRs, read
    /// with a GLOBAL load; a descriptor table pointer from the user SGPRs reads memory, and a
    /// pointer that is a code address on one path only is decided at run time.
    #[test]
    fn loads_from_s_getpc_read_the_code_and_the_others_memory() {
        let p = insts(&[
            ("S_GETPC_B64", "ENC_SOP1", &[("SDST", 0)], None),
            ("S_ADD_U32", "ENC_SOP2", &[("SDST", 0), ("SSRC0", 0), ("SSRC1", 255)], Some(0x293C)),
            ("S_ADDC_U32", "ENC_SOP2", &[("SDST", 1), ("SSRC0", 1), ("SSRC1", 128)], None),
            ("V_ADD_CO_U32", "VOP3_SDST_ENC", &[("VDST", 5), ("SDST", 106), ("SRC0", 256 + 5), ("SRC1", 0)], None),
            ("V_ADD_CO_CI_U32", "ENC_VOP2", &[("VDST", 6), ("SRC0", 1), ("VSRC1", 6)], None),
            ("GLOBAL_LOAD_DWORDX3", "ENC_FLAT_GLBL", &[("VDST", 5), ("ADDR", 5), ("SADDR", 0x7D), ("OFFSET", 0)], None),
            ("S_LOAD_DWORDX4", "ENC_SMEM", &[("SDATA", 8), ("SBASE", 5), ("OFFSET", 0x40), ("SOFFSET", 125)], None),
            ("S_LOAD_DWORD", "ENC_SMEM", &[("SDATA", 20), ("SBASE", 0), ("OFFSET", 4), ("SOFFSET", 125)], None),
            // s[0:1] is data after the load, a code address again on one path of the branch
            ("S_LOAD_DWORDX2", "ENC_SMEM", &[("SDATA", 0), ("SBASE", 5), ("OFFSET", 0), ("SOFFSET", 125)], None),
            ("S_CBRANCH_SCC0", "ENC_SOPP", &[("SIMM16", 1)], None),
            ("S_GETPC_B64", "ENC_SOP1", &[("SDST", 0)], None),
            ("S_LOAD_DWORD", "ENC_SMEM", &[("SDATA", 21), ("SBASE", 0), ("OFFSET", 0), ("SOFFSET", 125)], None),
            ("S_ENDPGM", "ENC_SOPP", &[], None),
        ]);
        let m = code_loads(&p);
        assert_eq!(m, BTreeMap::from([(p[5].pc, CodeLoad::Code), (p[7].pc, CodeLoad::Code), (p[11].pc, CodeLoad::Maybe)]));
    }
}
