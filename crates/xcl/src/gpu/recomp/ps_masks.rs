//! Pixel shaders (#2083): which compare masks need each helper lane's bit from its quad.
//!
//! On the console a pixel shader's masks hold helper lanes' bits; here `WaveActiveBallot`
//! leaves them out, so `Lw::ballot_ps_quad` rebuilds every quad's four bits from quad reads
//! of the compare's operands. That costs four ballots and up to eight quad reads per compare.
//! Most masks need less. With `WaveActiveBallot(c) | own bit` (HLSL `ballot_px_local`) every
//! lane's copy of the mask has the live lanes' bits and its own bit right, so EXEC
//! predication, `v_cndmask` selectors and carry-ins stay exact in helper lanes, and so do the
//! values they compute; only another helper lane's bit can be wrong. This forward analysis
//! frees a compare when nothing reads such a bit:
//! - bitwise SALU (and, or, xor, andn2, not, mov, cselect, saveexec, wrexec) carries the mask on;
//! - any other SALU read, SCC or VCCZ of it, a VALU / SMEM scalar operand refuse it;
//! - so do cross-lane instructions while EXEC carries it: DPP, `ds_swizzle` / `bpermute`,
//!   permlane read their source lane's bit of the reader's EXEC, `v_readfirstlane` the
//!   first set one (waterfall loops);
//! - a branch on such an EXEC (EXECZ, or SCC of a write to EXEC) lets copies of the mask take
//!   different paths: lanes that skip have their own bit 0, but the SGPRs and SCC the region
//!   writes differ after it (refused if read), and so does EXEC if the region widens it;
//! - a register known to be a subset of the entry EXEC (the live lanes: the kill mask) has
//!   no helper bits at all, so it is exact whatever it was computed from.
//! A compare that keeps the per-quad form under such an EXEC ORs in its own bit, which the
//! quad's other lanes may not have seen (`Lw::vopc`).

use crate::gpu::rdna2::{self, Decoded, Variant, F_CONDITIONAL, F_INDIRECT, F_TERMINATOR};
use super::x64_masks;
use std::collections::BTreeMap;

const VCC: u128 = 3 << 106;
const EXEC: u128 = 3 << 126;

fn fv(d: &Decoded, f: &str) -> Option<u64> {
    rdna2::field_value(d.enc, &d.words, f)
}
fn b64(n: &str) -> bool {
    n.ends_with("_B64") || n.contains("_B64_")
}
fn regs(code: u64, n: u64) -> u128 {
    if code < 128 { ((1u128 << n) - 1) << code } else { 0 }
}
fn is_salu(d: &Decoded) -> bool {
    matches!(d.enc.name, "ENC_SOP1" | "ENC_SOP2" | "ENC_SOPK" | "ENC_SOPC")
}
pub fn is_site(d: &Decoded) -> bool {
    let n = d.inst.name;
    n.starts_with("V_CMP") && d.variant == Variant::Plain && !n.starts_with("V_CMP_F_") && !n.starts_with("V_CMPX_F_")
}
fn bitwise(n: &str) -> bool {
    n.contains("_SAVEEXEC_")
        || n.contains("_WREXEC")
        || ["S_AND_B", "S_OR_B", "S_XOR_B", "S_ANDN2_B", "S_ORN2_B", "S_NAND_B", "S_NOR_B", "S_XNOR_B", "S_NOT_B", "S_MOV_B32", "S_MOV_B64", "S_CMOV_B", "S_CSELECT_B"]
            .iter()
            .any(|p| n.starts_with(p))
}
fn writes_scc(n: &str) -> bool {
    (bitwise(n) && !n.starts_with("S_MOV") && !n.starts_with("S_CMOV") && !n.starts_with("S_CSELECT"))
        || ["S_CMP", "S_BITCMP", "S_ADD_", "S_ADDC_", "S_SUB_", "S_SUBB_", "S_MIN_", "S_MAX_", "S_LSHL", "S_LSHR", "S_ASHR", "S_BFE_", "S_ABSDIFF", "S_WQM", "S_BCNT", "S_QUADMASK", "S_ABS_", "S_ADDK", "S_CMPK"]
            .iter()
            .any(|p| n.starts_with(p))
}
fn reads_scc(n: &str) -> bool {
    n.starts_with("S_CSELECT") || n.starts_with("S_CMOV") || n.starts_with("S_ADDC") || n.starts_with("S_SUBB")
}
/// every SGPR the instruction writes (also the partial writes `uses` leaves out)
fn writes(d: &Decoded) -> u128 {
    let n = d.inst.name;
    if n.starts_with("S_MOVRELD") {
        return !0;
    }
    let (_, mut w) = x64_masks::uses(d);
    if is_salu(d) {
        if let Some(s) = fv(d, "SDST") {
            w |= regs(s, if b64(n) { 2 } else { 1 });
        }
    }
    w
}
fn per_lane(d: &Decoded) -> u128 {
    let n = d.inst.name;
    if !(matches!(n, "V_CNDMASK_B32" | "V_ADD_CO_CI_U32" | "V_SUB_CO_CI_U32" | "V_SUBREV_CO_CI_U32") || n.starts_with("V_DIV_FMAS")) {
        return 0;
    }
    match fv(d, "SRC2") {
        Some(v) if d.enc.name.contains("VOP3") => regs(v, 2),
        _ => VCC,
    }
}
fn xlane(d: &Decoded) -> bool {
    matches!(d.variant, Variant::Dpp16 | Variant::Dpp8)
        || matches!(d.inst.name, "DS_SWIZZLE_B32" | "DS_BPERMUTE_B32" | "DS_PERMUTE_B32" | "V_PERMLANE16_B32" | "V_PERMLANEX16_B32" | "V_READFIRSTLANE_B32")
}

#[derive(Clone, Copy, PartialEq, Default)]
struct St {
    t: u128,
    a: u128,
    scc: u8,
}
impl St {
    fn join(self, o: St) -> St {
        St { t: self.t | o.t, a: self.a | o.a, scc: self.scc.max(o.scc) }
    }
}

/// SGPRs known to be a subset of a reference mask after `d`, from those before it
/// (`set`; EXEC's bit 126 in it: EXEC is). AND / ANDN2 keep a bounded first operand's
/// bound, OR / XOR / CSELECT need both, a compare's mask is a subset of EXEC.
fn subset_after(d: &Decoded, set: u128) -> u128 {
    let n = d.inst.name;
    let w = writes(d);
    let mut out = set & !w;
    let e_in = set >> 126 & 1 != 0;
    if n.starts_with("V_CMP") {
        if e_in {
            out |= w;
        }
        return out;
    }
    if !is_salu(d) {
        return out;
    }
    let wd = if b64(n) { 2 } else { 1 };
    let isb = |c: Option<u64>| c.is_some_and(|c| c == 128 || { let r = regs(c, wd); r != 0 && r & !set == 0 });
    let (s0, s1, sd) = (fv(d, "SSRC0"), fv(d, "SSRC1"), fv(d, "SDST"));
    let sdr = sd.map_or(0, |v| regs(v, wd));
    if n.contains("_SAVEEXEC_") {
        if e_in {
            out |= sdr;
        }
        let ex = if n.starts_with("S_AND_SAVEEXEC") {
            isb(s0) || e_in
        } else if n.starts_with("S_ANDN1_SAVEEXEC") {
            e_in
        } else if n.starts_with("S_ANDN2_SAVEEXEC") {
            isb(s0)
        } else if n.starts_with("S_OR_SAVEEXEC") || n.starts_with("S_XOR_SAVEEXEC") {
            isb(s0) && e_in
        } else {
            false
        };
        if ex {
            out |= EXEC;
        }
        return out;
    }
    let b = if n.starts_with("S_ANDN2_WREXEC") {
        isb(s0)
    } else if n.starts_with("S_ANDN1_WREXEC") {
        isb(s1)
    } else if n.starts_with("S_MOV_B") {
        isb(s0)
    } else if n.starts_with("S_AND_B") {
        isb(s0) || isb(s1)
    } else if n.starts_with("S_ANDN2_B") {
        isb(s0)
    } else if n.starts_with("S_OR_B") || n.starts_with("S_XOR_B") || n.starts_with("S_CSELECT_B") {
        isb(s0) && isb(s1)
    } else {
        false
    };
    if b {
        out |= w;
    }
    out
}

/// SGPRs that are a subset of the live lanes (the entry EXEC) after each instruction:
/// their helper-lane bits are 0, so the local form leaves them exact
fn live_subsets(insts: &[Decoded], succ: &[Vec<usize>]) -> Vec<u128> {
    let n = insts.len();
    let mut zin = vec![!0u128; n];
    let mut seen = vec![false; n];
    zin[0] = EXEC;
    seen[0] = true;
    let mut work = vec![0usize];
    let mut queued = vec![false; n];
    queued[0] = true;
    while let Some(i) = work.pop() {
        queued[i] = false;
        let o = subset_after(&insts[i], zin[i]);
        for &j in &succ[i] {
            let nz = if seen[j] { zin[j] & o } else { o };
            if !seen[j] || nz != zin[j] {
                seen[j] = true;
                zin[j] = nz;
                if !queued[j] {
                    queued[j] = true;
                    work.push(j);
                }
            }
        }
    }
    (0..n).map(|i| if seen[i] { subset_after(&insts[i], zin[i]) } else { !0 }).collect()
}

/// A conditional branch's region (forward: up to its target; backward: the loop) and what
/// differs after it when copies of the mask disagree on taking it.
struct Region {
    joins: Vec<usize>,
    /// SGPRs it writes
    w: u128,
    scc: bool,
    /// it widens EXEC, or runs something EXEC does not mask (v_writelane, scalar stores,
    /// messages) that the lanes skipping it would miss
    bad: bool,
}

/// The form each pixel-shader compare's mask can take, by address (compares not in it
/// keep the per-quad form of `Lw::ballot_ps_quad`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// never read whole: the lane's own bit
    Own,
    /// read whole, but no other lane's helper bit is: `WaveActiveBallot | own bit`
    Local,
    /// both operands scalar / constant: `cond ? EXEC : 0`
    Uniform,
}

pub fn forms(insts: &[Decoded]) -> BTreeMap<u64, Form> {
    let n = insts.len();
    let mut out = BTreeMap::new();
    if n == 0 {
        return out;
    }
    let pcs: std::collections::BTreeMap<u64, usize> = insts.iter().enumerate().map(|(i, d)| (d.pc, i)).collect();
    let mut indirect = vec![false; n];
    let succ: Vec<Vec<usize>> = insts
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let next = if i + 1 < n { Some(i + 1) } else { None };
            if let Some(t) = d.far_target {
                return pcs.get(&t).copied().into_iter().collect();
            }
            if d.inst.flags & F_INDIRECT != 0 || d.inst.name == "S_SETPC_B64" || d.inst.name == "S_SWAPPC_B64" {
                indirect[i] = true;
                return Vec::new();
            }
            let mut v = Vec::new();
            if let Some(t) = rdna2::branch_target(d) {
                v.extend(pcs.get(&t).copied());
                if d.inst.flags & F_CONDITIONAL != 0 {
                    v.extend(next);
                }
            } else if d.inst.flags & F_TERMINATOR == 0 {
                v.extend(next);
            }
            v
        })
        .collect();
    let z = live_subsets(insts, &succ);
    // conditional branches: the region whose execution may differ between copies
    let mut region: Vec<Option<Region>> = (0..n).map(|_| None).collect();
    for (b, d) in insts.iter().enumerate() {
        if d.inst.flags & F_CONDITIONAL == 0 {
            continue;
        }
        let Some(t) = rdna2::branch_target(d).and_then(|t| pcs.get(&t).copied()) else { continue };
        let (lo, hi, mut joins) = if t > b { (b + 1, t, vec![t]) } else { (t, b + 1, vec![b + 1]) };
        if t <= b {
            for k in lo..hi {
                for &s in &succ[k] {
                    if (s < lo || s >= hi) && !joins.contains(&s) {
                        joins.push(s);
                    }
                }
            }
        }
        let (mut w, mut scc, mut bad) = (0u128, false, false);
        let mut bound: u128 = EXEC;
        for k in lo..hi.min(n) {
            let dk = &insts[k];
            w |= writes(dk) & !EXEC;
            scc |= writes_scc(dk.inst.name);
            bound = subset_after(dk, bound);
            let nk = dk.inst.name;
            bad |= writes(dk) & EXEC != 0 && bound >> 126 & 1 == 0
                || nk == "V_WRITELANE_B32"
                || nk.starts_with("S_SENDMSG")
                || dk.enc.name == "ENC_SMEM" && (nk.contains("STORE") || nk.contains("ATOMIC"));
        }
        region[b] = Some(Region { joins, w, scc, bad });
    }
    let live = x64_masks::whole_live_out(insts, false);
    for site in 0..n {
        let d = &insts[site];
        if !is_site(d) {
            continue;
        }
        let dst = writes(d);
        if dst == 0 {
            continue;
        }
        if live[site] & dst == 0 && !d.inst.name.starts_with("V_CMPX") {
            out.insert(d.pc, Form::Own);
            continue;
        }
        // both operands scalar / constant: the mask is `cond ? EXEC : 0`, no ballot needed
        let uniform = d.enc.name.contains("VOP3") && fv(d, "SRC0").is_some_and(|v| v < 256) && fv(d, "SRC1").is_some_and(|v| v < 256);
        if uniform {
            out.insert(d.pc, Form::Uniform);
        } else if run(insts, &succ, &indirect, &z, &region, site).is_ok() {
            out.insert(d.pc, Form::Local);
        }
    }
    out
}

fn run(insts: &[Decoded], succ: &[Vec<usize>], indirect: &[bool], z: &[u128], region: &[Option<Region>], site: usize) -> Result<(), &'static str> {
    let n = insts.len();
    let mut sin = vec![St::default(); n];
    let mut extra = vec![St::default(); n];
    let mut div = vec![false; n];
    let mut work = vec![site];
    let mut queued = vec![false; n];
    queued[site] = true;
    while let Some(i) = work.pop() {
        queued[i] = false;
        let st = sin[i].join(extra[i]);
        if st == St::default() && i != site {
            continue;
        }
        let d = &insts[i];
        let nm = d.inst.name;
        let (whole, _) = x64_masks::uses(d);
        let w = writes(d);
        if (whole | per_lane(d)) & st.a != 0 {
            return Err("read of a diverged register");
        }
        if indirect[i] && (st.t | st.a) != 0 {
            return Err("indirect jump");
        }
        let mut o = St { t: st.t & !w, a: st.a & !w, scc: st.scc };
        let mut branch_div = false;
        if d.enc.name == "ENC_SOPP" {
            if nm.starts_with("S_CBRANCH_EXEC") {
                branch_div = st.t & EXEC != 0;
            } else if nm.starts_with("S_CBRANCH_SCC") {
                if st.scc == 2 {
                    return Err("branch on SCC of a mask");
                }
                branch_div = st.scc == 1;
            } else if nm.starts_with("S_CBRANCH_VCC") {
                if st.t & VCC != 0 {
                    return Err("branch on VCCZ");
                }
            } else if whole & st.t != 0 {
                return Err("other SOPP read");
            }
        } else if is_salu(d) {
            let rt = whole & st.t != 0;
            if reads_scc(nm) && st.scc != 0 {
                return Err("SCC read (cselect / cmov / addc)");
            }
            if rt && !bitwise(nm) {
                return Err(if nm.starts_with("S_CMP") || nm.starts_with("S_BITCMP") {
                    "s_cmp / s_bitcmp"
                } else if nm.starts_with("S_WQM") {
                    "s_wqm"
                } else {
                    "other SALU"
                });
            }
            match () {
                _ if nm.contains("_SAVEEXEC_") || nm.contains("_WREXEC") => {
                    let et = st.t & EXEC != 0;
                    let srct = whole & !EXEC & st.t != 0;
                    let sd = w & !EXEC;
                    if et || srct {
                        o.t |= EXEC;
                    }
                    if nm.contains("_WREXEC") {
                        if et || srct {
                            o.t |= sd;
                        }
                    } else if et {
                        o.t |= sd;
                    }
                }
                _ => {
                    if rt {
                        o.t |= w;
                    }
                }
            }
            if writes_scc(nm) {
                // SCC of a bitwise op is its result != 0: exact when the result is live-only
                let exact = w & !EXEC & !z[i] == 0 && (w & EXEC == 0 || z[i] >> 126 & 1 != 0);
                o.scc = if rt && !exact { if w & EXEC != 0 { 1 } else { 2 } } else { 0 };
            }
        } else if d.enc.name == "ENC_SMEM" {
            if whole & st.t != 0 {
                return Err("SMEM operand");
            }
        } else {
            // VALU / VMEM / DS / export
            if st.a & EXEC != 0 {
                return Err("diverged EXEC");
            }
            let exec_data = d.variant == Variant::Plain && ["SRC0", "SRC1", "SRC2"].iter().any(|f| matches!(fv(d, f), Some(126 | 127)));
            if whole & !EXEC & st.t != 0 || exec_data && st.t & EXEC != 0 {
                return Err("VALU / VMEM scalar operand");
            }
            if st.t & EXEC != 0 && xlane(d) {
                return Err(match d.variant {
                    Variant::Dpp16 | Variant::Dpp8 => "DPP under the mask",
                    _ if nm == "V_READFIRSTLANE_B32" => "readfirstlane under the mask",
                    _ => "ds_swizzle / permute under the mask",
                });
            }
            if is_site(d) && (i == site || st.t & EXEC != 0) {
                o.t |= w;
            }
        }
        o.t &= !z[i];
        if branch_div && !div[i] {
            div[i] = true;
            if let Some(r) = &region[i] {
                if r.bad {
                    return Err("branch on the mask over EXEC changes or unmasked work");
                }
                for &j in &r.joins {
                    let e = St { t: 0, a: r.w, scc: if r.scc { 2 } else { 0 } };
                    let ne = extra[j].join(e);
                    if ne != extra[j] {
                        extra[j] = ne;
                        if !queued[j] {
                            queued[j] = true;
                            work.push(j);
                        }
                    }
                }
            }
        }
        for &j in &succ[i] {
            let ns = sin[j].join(o);
            if ns != sin[j] {
                sin[j] = ns;
                if !queued[j] {
                    queued[j] = true;
                    work.push(j);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::rdna2::encode;

    const V: u64 = 256;
    type I = (&'static str, &'static str, Vec<(&'static str, u64)>);

    /// The form of every compare in `prog` (wave64; `S_ENDPGM` appended), in order.
    fn forms_of(prog: &[I]) -> Vec<Option<Form>> {
        let mut code: Vec<u8> = prog.iter().flat_map(|(n, e, f)| encode(n, e, f, None).unwrap_or_else(|err| panic!("{n}: {err}"))).collect();
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        let mut insts = Vec::new();
        let mut off = 0;
        while off < code.len() {
            let d = rdna2::decode(&code[off..], off as u64).unwrap();
            off += d.len;
            insts.push(d);
        }
        rdna2::resolve_far_jumps(&mut insts);
        let f = forms(&insts);
        insts.iter().filter(|d| is_site(d)).map(|d| f.get(&d.pc).copied()).collect()
    }
    fn sop1(n: &'static str, d: u64, s: u64) -> I {
        (n, "ENC_SOP1", vec![("SDST", d), ("SSRC0", s)])
    }
    fn sop2(n: &'static str, d: u64, a: u64, b: u64) -> I {
        (n, "ENC_SOP2", vec![("SDST", d), ("SSRC0", a), ("SSRC1", b)])
    }
    fn cmp(src1: u64) -> I {
        ("V_CMP_LT_F32", "ENC_VOPC", vec![("SRC0", 128), ("VSRC1", src1)])
    }
    fn dpp(dst: u64, src: u64) -> I {
        ("V_MOV_B32", "VOP1_VOP_DPP16", vec![("VDST", dst), ("SRC0", 0xFA), ("VSRC0", src), ("DPP_CTRL", 0xB1), ("ROW_MASK", 0xF), ("BANK_MASK", 0xF)])
    }
    fn branch(n: &'static str, words: u64) -> I {
        (n, "ENC_SOPP", vec![("SIMM16", words)])
    }

    #[test]
    fn masks_read_lane_by_lane_or_restored_before_derivatives_take_the_cheap_forms() {
        let wqm = sop1("S_WQM_B64", 126, 126);
        // v_cndmask alone reads it: the own bit
        assert_eq!(forms_of(&[wqm.clone(), cmp(2), ("V_CNDMASK_B32", "ENC_VOP2", vec![("VDST", 4), ("SRC0", 128), ("VSRC1", 2)])]), [Some(Form::Own)]);
        // EXEC from it, restored before the DPP derivative: the local form
        let save = sop1("S_AND_SAVEEXEC_B64", 22, 106);
        let mov = ("V_MOV_B32", "ENC_VOP1", vec![("VDST", 7), ("SRC0", V + 3)]);
        let restore = sop1("S_MOV_B64", 126, 22);
        assert_eq!(forms_of(&[wqm.clone(), cmp(3), save.clone(), mov.clone(), restore.clone(), dpp(8, 7)]), [Some(Form::Local)]);
        // the DPP under that EXEC reads the source lane's bit of the reader's copy: kept
        assert_eq!(forms_of(&[wqm.clone(), cmp(3), save, mov, dpp(8, 7), restore]), [None]);
        // scalar operands: cond ? EXEC : 0
        let ucmp = ("V_CMP_LT_F32", "ENC_VOP3", vec![("VDST", 106), ("SRC0", 4), ("SRC1", 128)]);
        assert_eq!(forms_of(&[wqm, ucmp, sop1("S_AND_SAVEEXEC_B64", 22, 106), dpp(8, 7)]), [Some(Form::Uniform)]);
    }

    #[test]
    fn masks_read_across_lanes_keep_the_quad_form() {
        let wqm = sop1("S_WQM_B64", 126, 126);
        // s_cbranch_vccz: a helper lane's bit alone sends the live lanes into the block
        assert_eq!(forms_of(&[wqm.clone(), cmp(2), sop2("S_AND_B64", 106, 126, 106), branch("S_CBRANCH_VCCZ", 1), ("V_MOV_B32", "ENC_VOP1", vec![("VDST", 7), ("SRC0", 128)])]), [None]);
        // waterfall loop: v_readfirstlane picks the first lane of an EXEC that carries the mask
        let body: Vec<I> = vec![
            ("V_READFIRSTLANE_B32", "ENC_VOP1", vec![("VDST", 4), ("SRC0", V + 2)]),
            ("V_CMP_EQ_F32", "ENC_VOPC", vec![("SRC0", 4), ("VSRC1", 2)]),
            sop1("S_AND_SAVEEXEC_B64", 26, 106),
            sop2("S_XOR_B64", 126, 26, 126),
        ];
        let mut prog = vec![wqm, sop1("S_MOV_B64", 24, 126)];
        prog.extend(body);
        prog.push(branch("S_CBRANCH_EXECNZ", (-5i64) as u16 as u64));
        prog.push(sop1("S_MOV_B64", 126, 24));
        assert_eq!(forms_of(&prog), [None]);
    }

    #[test]
    fn a_kill_mask_of_live_lanes_stays_exact() {
        // s20 = the entry EXEC (live lanes only); a v_cmpx kill clears its lanes from it, and
        // the branch on SCC (any pixel left) sees no helper bit: the local form is enough
        let prog: Vec<I> = vec![
            sop1("S_MOV_B64", 20, 126),
            sop1("S_WQM_B64", 126, 126),
            ("V_CMPX_LT_F32", "ENC_VOPC", vec![("SRC0", 128), ("VSRC1", 2)]),
            sop2("S_ANDN2_B64", 20, 20, 126),
            branch("S_CBRANCH_SCC0", 2),
            sop1("S_WQM_B64", 2, 20),
            sop2("S_AND_B64", 126, 126, 2),
        ];
        assert_eq!(forms_of(&prog), [Some(Form::Local)]);
    }

    #[test]
    fn vop3_sdst_and_literal_encodings_are_recognized() {
        // VOP3_SDST_ENC: V_ADD_CO_CI_U32 and V_SUB_CO_CI_U32 read carry-in from SRC2, not VCC.
        for (op, src2) in [("V_ADD_CO_CI_U32", 20u64), ("V_SUB_CO_CI_U32", 22u64)] {
            let bytes = rdna2::encode(
                op,
                "VOP3_SDST_ENC",
                &[("VDST", 1), ("SDST", 106), ("SRC0", V + 2), ("SRC1", V + 3), ("SRC2", src2)],
                None,
            )
            .unwrap();
            let inst = rdna2::decode(&bytes, 0).unwrap();
            assert_eq!(inst.enc.name, "VOP3_SDST_ENC");
            assert_eq!(per_lane(&inst), regs(src2, 2));
        }

        // Scalar-vs-literal VOP3 compare is recognized as Form::Uniform.
        let mut bytes = Vec::new();
        bytes.extend(rdna2::encode("S_WQM_B64", "ENC_SOP1", &[("SDST", 126), ("SSRC0", 126)], None).unwrap());
        let cmp_pc = bytes.len() as u64;
        bytes.extend(
            rdna2::encode(
                "V_CMP_LT_F32",
                "ENC_VOP3",
                &[("VDST", 106), ("SRC0", 4), ("SRC1", 255)],
                Some(0x3F80_0000),
            )
            .unwrap(),
        );
        bytes.extend(rdna2::encode("S_AND_SAVEEXEC_B64", "ENC_SOP1", &[("SDST", 22), ("SSRC0", 106)], None).unwrap());
        bytes.extend(rdna2::encode("V_MOV_B32", "ENC_VOP1", &[("VDST", 7), ("SRC0", V + 3)], None).unwrap());
        bytes.extend(rdna2::encode("S_ENDPGM", "ENC_SOPP", &[("SIMM16", 0)], None).unwrap());
        let mut insts = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            let d = rdna2::decode(&bytes[off..], off as u64).unwrap();
            off += d.len;
            insts.push(d);
        }
        assert_eq!(forms(&insts).get(&cmp_pc), Some(&Form::Uniform));
    }
}
