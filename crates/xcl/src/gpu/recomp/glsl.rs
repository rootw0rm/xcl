//! RDNA2 → GLSL 450 backend for blit-class vertex shaders (Phase D step 13).
//!
//! The HLSL emitter (`emit.rs`) stays the reference for register/export semantics; this
//! module is the second backend behind the [`VsBackend`] trait. It covers the straight-line
//! blit subset only (scalar ALU + a small VALU set + `exp pos`/`param` + `s_endpgm`) and
//! assumes the full-EXEC entry state a VS starts with (no divergent masks, no cross-lane
//! reads — true of the blit shaders gated here). Anything else is recorded in
//! `unsupported`/`ignored` and emitted as a comment so the output still compiles.
//!
//! Bring-up only: tests may shell out to `glslc` (behind `#[cfg(test)]`). The shipped path
//! never spawns a compiler; in-process SPIR-V emission is step 14's slice.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::{elements_at, ranked, vs_param_register, VsInterface};
pub use super::{HlslVsBackend, VsBackend};
use crate::gpu::rdna2::{self, Decoded, Operand, Variant};

/// The GLSL 450 path in this module.
pub struct GlslVsBackend;

impl VsBackend for GlslVsBackend {
    type Output = GlslTranslation;
    fn translate_vs(code: &[u8], vs: &VsInterface, wave64: bool) -> Result<Self::Output, String> {
        translate_vs_glsl(code, vs, wave64)
    }
}

/// GLSL 450 translation of one blit-class vertex shader.
#[derive(Debug, Clone, Default)]
pub struct GlslTranslation {
    /// The full `GLSL 450` vertex shader source.
    pub glsl: String,
    /// RDNA2 mnemonics seen but not translated (mnemonic → count); still compiles.
    pub unsupported: BTreeMap<String, usize>,
    /// "mnemonic: reason" → count.
    pub unsupported_why: BTreeMap<String, usize>,
    /// Instructions without an effect in this model (waits, prefetches…).
    pub ignored: BTreeMap<String, usize>,
    /// Decoded instruction count.
    pub instructions: usize,
}

impl GlslTranslation {
    /// True when every instruction translated (no `unsupported` entries).
    pub fn complete(&self) -> bool {
        self.unsupported.is_empty()
    }
}

/// Translate blit-class VS bytes to GLSL 450 with the HLSL path's register/export
/// semantics (fetch-shader VGPRs, `exp pos0` → `gl_Position`, `exp param N` → out param).
pub fn translate_vs_glsl(
    code: &[u8],
    vs: &VsInterface,
    wave64: bool,
) -> Result<GlslTranslation, String> {
    let mut insts: Vec<Decoded> = Vec::new();
    let mut off = 0usize;
    while off + 4 <= code.len() {
        let d = rdna2::decode(&code[off..], off as u64)
            .map_err(|e| format!("+{off:#x}: cannot decode ({e:?})"))?;
        off += d.len;
        insts.push(d);
    }
    rdna2::resolve_far_jumps(&mut insts);
    for d in &insts {
        if rdna2::branch_target(d).is_some() || d.far_target.is_some() {
            return Err(format!(
                "+{:#x}: branches are not supported by the GLSL bring-up backend",
                d.pc
            ));
        }
    }
    let mut em = GlslEm::new(vs.clone(), wave64);
    em.max_vgpr = insts
        .iter()
        .flat_map(|d| {
            rdna2::vgpr_uses(d)
                .0
                .into_iter()
                .chain(rdna2::vgpr_uses(d).1)
        })
        .max()
        .unwrap_or(0);
    let mut ended = false;
    for d in &insts {
        if ended {
            break;
        }
        ended = em.step(d)?;
    }
    let glsl = em.finish();
    Ok(GlslTranslation {
        glsl,
        unsupported: em.unsupported,
        unsupported_why: em.unsupported_why,
        ignored: em.ignored,
        instructions: insts.len(),
    })
}

/// Instructions without an effect in the bring-up execution model.
const NOOPS: &[&str] = &[
    "s_nop",
    "s_waitcnt",
    "s_waitcnt_vscnt",
    "s_waitcnt_vmcnt",
    "s_waitcnt_expcnt",
    "s_waitcnt_lgkmcnt",
    "s_waitcnt_depctr",
    "s_inst_prefetch",
    "s_clause",
    "s_code_end",
    "s_setprio",
    "s_sleep",
    "s_icache_inv",
    "s_dcache_inv",
    "s_gl1_inv",
    "s_ttracedata",
    "s_ttracedata_imm",
    "v_nop",
    "s_denorm_mode",
    "s_sendmsg",
    "s_sendmsghalt",
    "s_set_gpr_idx_off",
    "s_version",
    "buffer_gl0_inv",
    "buffer_gl1_inv",
    "ds_nop",
];

fn hex(v: u32) -> String {
    format!("0x{v:08X}u")
}

fn f32_const(code: u64) -> Option<f32> {
    Some(match code {
        240 => 0.5,
        241 => -0.5,
        242 => 1.0,
        243 => -1.0,
        244 => 2.0,
        245 => -2.0,
        246 => 4.0,
        247 => -4.0,
        248 => 0.159_154_94,
        _ => return None,
    })
}

/// Components of an output element: clip/cull distances by their mask, the rest 4-vectors.
/// Mirrors `emit::out_comps` so the GLSL params alias the HLSL ones.
fn out_comps(e: &crate::gpu::dxbc::SigElement) -> usize {
    if matches!(e.system_value, 2 | 3) {
        (8 - e.mask.leading_zeros()) as usize
    } else {
        4
    }
}

fn glsl_vec_type(component_type: u32, n: usize) -> &'static str {
    if n == 1 {
        match component_type {
            1 => "uint",
            2 => "int",
            _ => "float",
        }
    } else {
        match component_type {
            1 => "uvec4",
            2 => "ivec4",
            _ => "vec4",
        }
    }
}

struct GlslEm {
    vs: VsInterface,
    wave64: bool,
    out: Vec<String>,
    vgprs: BTreeSet<u64>,
    sgprs: BTreeSet<u64>,
    unsupported: BTreeMap<String, usize>,
    unsupported_why: BTreeMap<String, usize>,
    ignored: BTreeMap<String, usize>,
    max_vgpr: u64,
}

impl GlslEm {
    fn new(vs: VsInterface, wave64: bool) -> Self {
        Self {
            vs,
            wave64,
            out: Vec::new(),
            vgprs: BTreeSet::new(),
            sgprs: BTreeSet::new(),
            unsupported: BTreeMap::new(),
            unsupported_why: BTreeMap::new(),
            ignored: BTreeMap::new(),
            max_vgpr: 0,
        }
    }

    fn line(&mut self, s: &str) {
        self.out.push(format!("  {s}"));
    }

    fn comment(&mut self, s: &str) {
        self.out.push(format!("  // {s}"));
    }

    fn unsupported(&mut self, d: &Decoded, why: &str) {
        let m = rdna2::mnemonic(d);
        *self
            .unsupported_why
            .entry(format!("{m}: {why}"))
            .or_default() += 1;
        *self.unsupported.entry(m).or_default() += 1;
        self.comment(&format!("UNSUPPORTED ({why}): {}", rdna2::format(d)));
    }

    fn vreg(&mut self, n: u64) -> String {
        self.vgprs.insert(n);
        format!("v{n}")
    }

    fn sreg(&mut self, code: u64) -> String {
        match code {
            0..=124 | 126 | 127 => {
                self.sgprs.insert(code);
                format!("s{code}")
            }
            _ => "0u".into(),
        }
    }

    fn code32(&mut self, code: u64, lit: Option<u32>) -> Option<String> {
        Some(match code {
            0..=124 | 126 | 127 => self.sreg(code),
            125 => "0u".into(),
            128..=192 => format!("{}u", code - 128),
            193..=208 => hex((-((code - 192) as i64)) as u32),
            240..=248 => hex(f32_const(code)?.to_bits()),
            // Whole-wave masks and SCC-count shorthands: the bring-up backend runs with a
            // full EXEC, so masks read all-ones here (documented limitation, true of blits).
            251 | 252 => "0xFFFFFFFFu".into(),
            253 => "scc".into(),
            255 => hex(lit?),
            256..=511 => self.vreg(code - 256),
            235..=238 => "0u".into(),
            239 => "0x7FFFFFFFu".into(),
            _ => return None,
        })
    }

    fn field(d: &Decoded, f: &str) -> u64 {
        rdna2::field_value(d.enc, &d.words, f).unwrap_or(0) * rdna2::register_scale(f)
    }

    fn ops(d: &Decoded) -> (Vec<&'static Operand>, Vec<&'static Operand>) {
        let mut outs = Vec::new();
        let mut ins = Vec::new();
        for o in d.ienc.operands {
            if o.implicit && !(o.field.is_none() && o.ty == "OPR_VCC") {
                continue;
            }
            if o.output {
                outs.push(o);
            } else if o.input {
                ins.push(o);
            }
        }
        (outs, ins)
    }

    fn read(&mut self, d: &Decoded, o: &Operand) -> Option<String> {
        match o.field {
            Some("LITERAL") => Some(hex(d.literal?)),
            Some(f) => {
                let v = Self::field(d, f);
                let width = d.enc.field_width(f);
                if o.ty.starts_with("OPR_VGPR") && width <= 8 {
                    return Some(self.vreg(v));
                }
                match o.ty {
                    "OPR_SIMM16" => Some(format!("{v}u")),
                    "OPR_SIMM32" => Some(hex(d.literal?)),
                    _ => self.code32(v, d.literal),
                }
            }
            None => match o.ty {
                "OPR_VCC" => Some(self.sreg(106)),
                "OPR_EXEC" | "OPR_SDST_EXEC" => Some(self.sreg(126)),
                "OPR_SDST_M0" => Some(self.sreg(124)),
                "OPR_SSRC_SPECIAL_SCC" => Some("scc".into()),
                _ => None,
            },
        }
    }

    /// One decoded instruction; returns true when it ends the program (`s_endpgm`).
    fn step(&mut self, d: &Decoded) -> Result<bool, String> {
        let name = d.inst.name.to_ascii_lowercase();
        self.comment(&format!("{:04x}: {}", d.pc, rdna2::format(d)));
        if NOOPS.contains(&name.as_str()) {
            *self.ignored.entry(name).or_default() += 1;
            return Ok(false);
        }
        if d.variant != Variant::Plain && d.enc.name != "ENC_EXP" {
            self.unsupported(d, "SDWA/DPP in the bring-up backend");
            return Ok(false);
        }
        // Narrow VertexIndex-indexed select pattern (scalar-element only): VOP3 carries
        // an explicit lane selector and V_CMPX writes EXEC, so both stay unsupported.
        if name == "v_cndmask_b32" && d.enc.name != "ENC_VOP2" {
            self.unsupported(d, "valu");
            return Ok(false);
        }
        match d.enc.name {
            "ENC_SOPP" => match name.as_str() {
                "s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" => Ok(true),
                "s_barrier" => {
                    self.comment("s_barrier: no-op in the bring-up VS (single wave)");
                    Ok(false)
                }
                _ => {
                    self.unsupported(d, "sopp");
                    Ok(false)
                }
            },
            "ENC_SOP1" | "ENC_SOP2" | "ENC_SOPC" | "ENC_SOPK" => {
                self.salu(d, &name);
                Ok(false)
            }
            "ENC_EXP" => {
                self.export(d);
                Ok(false)
            }
            e if e.starts_with("ENC_VOP") || e.starts_with("VOP") => {
                self.valu(d, &name);
                Ok(false)
            }
            _ => {
                self.unsupported(d, "encoding");
                Ok(false)
            }
        }
    }

    fn salu(&mut self, d: &Decoded, name: &str) {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let dst_code = f("SDST");
        // SOPK carries a signed 32-bit literal instead of SSRC0.
        let src0 = if d.enc.name == "ENC_SOPK" {
            hex(d.literal.unwrap_or(0))
        } else {
            match self.code32(f("SSRC0"), d.literal) {
                Some(s) => s,
                None => {
                    self.unsupported(d, "op");
                    return;
                }
            }
        };
        let src1 = if d.enc.name == "ENC_SOP2" || d.enc.name == "ENC_SOPC" {
            match self.code32(f("SSRC1"), d.literal) {
                Some(s) => s,
                None => {
                    self.unsupported(d, "op");
                    return;
                }
            }
        } else {
            String::new()
        };
        // Narrow VertexIndex-indexed select pattern: SOPC writes SCC (this encoding
        // carries no SDST, so it must not fall through to the destination logic below).
        if d.enc.name == "ENC_SOPC" {
            if name != "s_cmp_eq_u32" {
                self.unsupported(d, "salu");
                return;
            }
            self.line(&format!("scc = (({src0} == {src1}) ? 1u : 0u);"));
            return;
        }
        let val: Option<String> = match name {
            "s_mov_b32" => Some(src0),
            "s_movk_i32" => Some(hex(d.literal.unwrap_or(0))),
            "s_add_u32" | "s_add_i32" => Some(format!("{src0} + {src1}")),
            "s_sub_u32" | "s_sub_i32" => Some(format!("{src0} - {src1}")),
            "s_and_b32" => Some(format!("{src0} & {src1}")),
            "s_or_b32" => Some(format!("{src0} | {src1}")),
            "s_xor_b32" => Some(format!("{src0} ^ {src1}")),
            "s_not_b32" => Some(format!("~{src0}")),
            "s_min_u32" => Some(format!("min({src0}, {src1})")),
            "s_max_u32" => Some(format!("max({src0}, {src1})")),
            // Codebase/GPU convention (lower.rs, emit.rs): SCC-true picks SSRC0.
            "s_cselect_b32" => Some(format!("((scc != 0u) ? {src0} : {src1})")),
            _ => None,
        };
        match val {
            Some(v) => {
                if dst_code == 125 {
                    // null: result discarded (SCC effects are out of scope for blits)
                } else if dst_code > 127 {
                    self.unsupported(d, "sdst");
                } else {
                    let s = self.sreg(dst_code);
                    self.line(&format!("{s} = {v};"));
                }
            }
            None => self.unsupported(d, "salu"),
        }
    }

    fn valu(&mut self, d: &Decoded, name: &str) {
        // Narrow VertexIndex-indexed select pattern (scalar-element only): V_CMP_EQ_U32
        // in VOPC form writes the VCC homes (s106/s107). The VOPC encoding carries no
        // VDST, so this must not fall through to the destination logic below.
        if name == "v_cmp_eq_u32" {
            if d.enc.name != "ENC_VOPC" {
                self.unsupported(d, "valu");
                return;
            }
            let (_, ins) = Self::ops(d);
            if ins.len() != 2 {
                self.unsupported(d, "op");
                return;
            }
            let (a, b) = match (self.read(d, &ins[0]), self.read(d, &ins[1])) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    self.unsupported(d, "op");
                    return;
                }
            };
            // Single-wave bring-up: the lane bit lives in s106 (s107 stays 0).
            self.line(&format!("s106 = (({a} == {b}) ? 0xFFFFFFFFu : 0u);"));
            self.line("s107 = 0u;");
            return;
        }
        let (outs, ins) = Self::ops(d);
        let mut u = Vec::new();
        for o in &ins {
            match self.read(d, o) {
                Some(s) => u.push(s),
                None => {
                    self.unsupported(d, "op");
                    return;
                }
            }
        }
        let dst = outs.first().map(|o| match o.field {
            Some(f) => {
                let v = Self::field(d, f);
                if o.ty.starts_with("OPR_VGPR") {
                    Dst::V(v)
                } else {
                    Dst::S(v)
                }
            }
            None => Dst::Null,
        });
        // Re-resolve without the awkward path: VGPR dest ↔ vreg, scalar dest ↔ sreg/null.
        let a = u.first().cloned().unwrap_or_else(|| "0u".into());
        let b = u.get(1).cloned().unwrap_or_else(|| "0u".into());
        let c = u.get(2).cloned().unwrap_or_else(|| "0u".into());
        let float = |e: String| format!("uintBitsToFloat({e})");
        let to_bits = |e: String| format!("floatBitsToUint({e})");
        let res: Option<String> = match name {
            "v_mov_b32" | "v_mov_fed_b32" => Some(a.clone()),
            "v_not_b32" => Some(format!("~{a}")),
            // Codebase/GPU convention (lower.rs F32_COMPARES): VCC-true picks VSRC1.
            "v_cndmask_b32" => Some(format!("((s106 != 0u) ? {b} : {a})")),
            "v_and_b32" => Some(format!("{a} & {b}")),
            "v_or_b32" => Some(format!("{a} | {b}")),
            "v_xor_b32" => Some(format!("{a} ^ {b}")),
            "v_lshlrev_b32" => Some(format!("{b} << ({a} & 31u)")),
            "v_lshrrev_b32" => Some(format!("{b} >> ({a} & 31u)")),
            "v_ashrrev_i32" => Some(format!("uint(int({b}) >> ({a} & 31u))")),
            "v_add_nc_u32" | "v_add_u32" | "v_add_co_u32" => Some(format!("{a} + {b}")),
            "v_sub_nc_u32" | "v_sub_u32" => Some(format!("{a} - {b}")),
            "v_subrev_nc_u32" | "v_subrev_u32" => Some(format!("{b} - {a}")),
            "v_mul_lo_u32" => Some(format!("{a} * {b}")),
            "v_min_u32" => Some(format!("min({a}, {b})")),
            "v_max_u32" => Some(format!("max({a}, {b})")),
            "v_add_f32" => Some(to_bits(format!(
                "{} + {}",
                float(a.clone()),
                float(b.clone())
            ))),
            "v_sub_f32" => Some(to_bits(format!(
                "{} - {}",
                float(a.clone()),
                float(b.clone())
            ))),
            "v_subrev_f32" => Some(to_bits(format!(
                "{} - {}",
                float(b.clone()),
                float(a.clone())
            ))),
            "v_mul_f32" | "v_mul_legacy_f32" => Some(to_bits(format!(
                "{} * {}",
                float(a.clone()),
                float(b.clone())
            ))),
            "v_min_f32" => Some(to_bits(format!(
                "min({}, {})",
                float(a.clone()),
                float(b.clone())
            ))),
            "v_max_f32" => Some(to_bits(format!(
                "max({}, {})",
                float(a.clone()),
                float(b.clone())
            ))),
            "v_fma_f32" | "v_mad_f32" | "v_fma_legacy_f32" | "v_mad_legacy_f32" => {
                Some(to_bits(format!(
                    "fma({}, {}, {})",
                    float(a.clone()),
                    float(b.clone()),
                    float(c.clone())
                )))
            }
            "v_cvt_f32_u32" => Some(to_bits(format!("float({a})"))),
            "v_cvt_f32_i32" => Some(to_bits(format!("float(int({a}))"))),
            "v_cvt_u32_f32" => Some(format!("uint({})", float(a.clone()))),
            "v_cvt_i32_f32" => Some(format!("uint(int({}))", float(a.clone()))),
            _ => None,
        };
        match (res, dst) {
            (Some(v), Some(Dst::V(n))) => {
                let out = self.vreg(n);
                // Full EXEC at VS entry: direct assignment (documented bring-up assumption).
                self.line(&format!("{out} = {v};"));
            }
            (Some(v), Some(Dst::S(n))) => {
                if n == 125 {
                    // null
                } else if n > 127 {
                    self.unsupported(d, "sdst");
                } else {
                    let out = self.sreg(n);
                    self.line(&format!("{out} = {v};"));
                }
            }
            (Some(_), _) => {}
            (None, _) => self.unsupported(d, "valu"),
        }
    }

    fn export(&mut self, d: &Decoded) {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let (en, tgt, compr) = (f("EN"), f("TGT"), f("COMPR") != 0);
        let srcs: Vec<String> = (0..4).map(|i| self.vreg(f(&format!("VSRC{i}")))).collect();
        let comp = |c: usize| {
            if compr {
                let r = &srcs[c / 2];
                if c.is_multiple_of(2) {
                    format!("({r} & 0xFFFFu)")
                } else {
                    format!("({r} >> 16)")
                }
            } else {
                srcs[c].clone()
            }
        };
        let enabled = |c: usize| {
            if compr {
                en >> (c / 2 * 2) & 3 != 0
            } else {
                en >> c & 1 == 1
            }
        };
        match tgt {
            12 => {
                for c in 0..4 {
                    if enabled(c) {
                        let chan = ["x", "y", "z", "w"][c];
                        self.line(&format!(
                            "gl_Position.{chan} = uintBitsToFloat({});",
                            comp(c)
                        ));
                    }
                }
            }
            13..=15 => {
                self.comment(
                    "exp misc vector: no GLSL output (clip distances come from their param)",
                );
            }
            32..=63 => {
                let k = (tgt - 32) as usize;
                let at = vs_param_register(&self.vs, k)
                    .map(|r| elements_at(&self.vs.outputs, true, r))
                    .unwrap_or_default();
                if at.is_empty() {
                    self.comment("exp param without an OSG1 element (dummy)");
                    return;
                }
                // Clone the (index, component type, component count) triples first so the
                // `ranked` borrow ends before `self.line` mutably borrows `self`.
                let params: Vec<(usize, u32, usize)> = ranked(&self.vs.outputs, true)
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| at.contains(j))
                    .map(|(j, e)| (j, e.component_type, out_comps(e)))
                    .collect();
                for (j, component_type, n) in params {
                    for c in 0..n {
                        if !enabled(c) {
                            continue;
                        }
                        let raw = comp(c);
                        let v = if component_type == 3 {
                            format!("uintBitsToFloat({raw})")
                        } else if component_type == 2 {
                            format!("int({raw})")
                        } else if component_type == 1 {
                            raw
                        } else {
                            format!("uintBitsToFloat({raw})")
                        };
                        if n == 1 {
                            self.line(&format!("out_p{j} = {v};"));
                        } else {
                            self.line(&format!("out_p{j}[{c}] = {v};"));
                        }
                    }
                }
            }
            _ => self.unsupported(d, "export target"),
        }
    }

    fn finish(&self) -> String {
        let mut h = String::new();
        let _ = writeln!(h, "// generated by XCL (RDNA2 -> GLSL), do not edit");
        h.push_str("#version 450\n");
        // Inputs: one location per non-system ISG1 element, in register order.
        let inputs: Vec<&crate::gpu::dxbc::SigElement> = self
            .vs
            .inputs
            .iter()
            .filter(|e| e.system_value == 0 && !e.name.starts_with("SV_"))
            .collect();
        for (k, e) in inputs.iter().enumerate() {
            let ty = match e.component_type {
                1 => "uvec4",
                2 => "ivec4",
                _ => "vec4",
            };
            let _ = writeln!(h, "layout(location = {k}) in {ty} in_a{};", e.register);
        }
        // Outputs: one location per ranked non-system OSG1 element; position is gl_Position.
        for (k, e) in ranked(&self.vs.outputs, true).iter().enumerate() {
            let ty = glsl_vec_type(e.component_type, out_comps(e));
            let _ = writeln!(h, "layout(location = {k}) out {ty} out_p{k};");
        }
        h.push_str("void main() {\n");
        // The wave's console lane; blits need no cross-lane reads.
        h.push_str("  uint lane_id = 0u; // bring-up: full-EXEC blit, no lane exchange\n");
        h.push_str("  uint scc = 0u;\n");
        // SGPRs: EXEC all-ones (VS entry state), the rest zero.
        let mut sg: BTreeSet<u64> = self.sgprs.clone();
        sg.extend([106, 107, 124, 126, 127]);
        for &c in &sg {
            let init = if c == 126 || c == 127 {
                "0xFFFFFFFFu"
            } else {
                "0u"
            };
            let _ = writeln!(h, "  uint s{c} = {init};");
        }
        // VGPRs: vertex/index ids, fetch-shader attributes, else zero.
        for &v in &self.vgprs {
            let sv_at = self.vs.inputs.iter().find(|e| {
                matches!(e.system_value, 6 | 8) && self.vs.input_vgpr(e.register) == Some(v)
            });
            let init = if v == 0 {
                "uint(gl_VertexIndex)".to_string()
            } else if v == 3 {
                "uint(gl_InstanceIndex)".to_string()
            } else if let Some(e) = sv_at {
                if e.system_value == 6 {
                    "uint(gl_VertexIndex)".to_string()
                } else {
                    "uint(gl_InstanceIndex)".to_string()
                }
            } else {
                inputs
                    .iter()
                    .find_map(|e| {
                        self.vs
                            .input_vgpr(e.register)
                            .filter(|&b| v >= b && v < b + 4)
                            .map(|b| {
                                let chan = ["x", "y", "z", "w"][(v - b) as usize];
                                match e.component_type {
                                    1 => format!("in_a{}.{chan}", e.register),
                                    2 => format!("uint(in_a{}.{chan})", e.register),
                                    _ => format!("floatBitsToUint(in_a{}.{chan})", e.register),
                                }
                            })
                    })
                    .unwrap_or_else(|| "0u".into())
            };
            let _ = writeln!(h, "  uint v{v} = {init};");
        }
        // Silence an unused-lane warning on drivers that check: keep the read.
        h.push_str("  lane_id = lane_id & 63u;\n");
        for l in &self.out {
            h.push_str(l);
            h.push('\n');
        }
        h.push_str("}\n");
        let _ = self.wave64;
        h
    }
}

#[derive(Clone, Copy)]
enum Dst {
    V(u64),
    S(u64),
    Null,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::rdna2::encode;

    fn sig(
        name: &str,
        index: u32,
        sv: u32,
        ct: u32,
        reg: u32,
        mask: u8,
    ) -> crate::gpu::dxbc::SigElement {
        crate::gpu::dxbc::SigElement {
            name: name.into(),
            index,
            system_value: sv,
            component_type: ct,
            register: reg,
            mask,
            rw_mask: mask,
            stream: 0,
        }
    }

    fn blit_code() -> Vec<u8> {
        let mut code = Vec::new();
        for (vdst, src) in [(0u64, 260u64), (1, 261u64), (2, 128u64), (3, 242u64)] {
            code.extend(
                encode(
                    "V_MOV_B32",
                    "ENC_VOP1",
                    &[("VDST", vdst), ("SRC0", src)],
                    None,
                )
                .unwrap(),
            );
        }
        code.extend(
            encode(
                "EXP",
                "ENC_EXP",
                &[
                    ("TGT", 12),
                    ("EN", 0xF),
                    ("VSRC0", 0),
                    ("VSRC1", 1),
                    ("VSRC2", 2),
                    ("VSRC3", 3),
                    ("DONE", 0),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(
            encode(
                "EXP",
                "ENC_EXP",
                &[
                    ("TGT", 32),
                    ("EN", 0xF),
                    ("VSRC0", 0),
                    ("VSRC1", 1),
                    ("VSRC2", 2),
                    ("VSRC3", 3),
                    ("DONE", 1),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    fn blit_vs() -> VsInterface {
        VsInterface {
            inputs: vec![sig("POSITION", 0, 0, 3, 0, 0xF)],
            outputs: vec![
                sig("SV_Position", 0, 1, 3, 0, 0xF),
                sig("COLOR", 0, 0, 3, 1, 0xF),
            ],
            attr_base: 4,
            params: None,
        }
    }

    #[test]
    fn synthetic_blit_vs_emits_gl_position() {
        let code = blit_code();
        let vs = blit_vs();
        // Both backends complete on the blit subset with the same interface.
        let hlsl = HlslVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(hlsl.complete(), "{:?}\n{}", hlsl.unsupported, hlsl.hlsl);
        let t = GlslVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}\n{}", t.unsupported, t.glsl);
        assert!(t.glsl.contains("#version 450"), "{}", t.glsl);
        assert!(t.glsl.contains("gl_Position"), "{}", t.glsl);
        assert!(t.glsl.contains("layout(location"), "{}", t.glsl);
        assert!(t.glsl.contains("uintBitsToFloat"), "{}", t.glsl);
        assert!(!t.glsl.contains("WaveGetLaneIndex"), "{}", t.glsl);
        assert!(!t.glsl.contains("asfloat"), "{}", t.glsl);
        assert!(!t.glsl.contains("SV_Position"), "{}", t.glsl);
        // Position comes from the fetch VGPRs (v4/v5 → in_a0.xy), w/1.0 immediates.
        assert!(t.glsl.contains("in_a0"), "{}", t.glsl);
        assert!(
            t.glsl.contains("gl_VertexIndex") || t.glsl.contains("in_a0"),
            "{}",
            t.glsl
        );
        // glslc bring-up check where available, else the structural assertions above gate.
        let Ok(which) = std::process::Command::new("glslc")
            .arg("--version")
            .output()
        else {
            println!("glslc not available – skipping compile, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("glslc not available – skipping compile, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-glsl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("blit.vert");
        let spv = dir.join("blit.vert.spv");
        std::fs::write(&src, &t.glsl).unwrap();
        let out = std::process::Command::new("glslc")
            .arg("-fshader-stage=vert")
            .arg(&src)
            .arg("-o")
            .arg(&spv)
            .output()
            .expect("spawn glslc");
        assert!(
            out.status.success(),
            "glslc failed: {}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            t.glsl
        );
        assert!(
            std::fs::metadata(&spv)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "glslc produced no output"
        );
    }

    /// Same interface as the blit gate (position + one param), but selected by
    /// VertexIndex: the title `render_triangle` pattern (`p[id]` over constant corners).
    fn triangle_code() -> Vec<u8> {
        // v0 holds VertexIndex (bring-up init); y = is1 ? 0.5 : -0.5,
        // x = is0 ? -0.5 : (is1 ? 0.0 : 0.5), z = 0, w = 1.
        let mut code = Vec::new();
        // Materialize float-bit constants (inline-constant operand codes).
        for (vdst, src) in [
            (1u64, 241u64), // -0.5
            (2, 240u64),    // 0.5
            (3, 128u64),    // 0.0
            (4, 242u64),    // 1.0
        ] {
            code.extend(
                encode(
                    "V_MOV_B32",
                    "ENC_VOP1",
                    &[("VDST", vdst), ("SRC0", src)],
                    None,
                )
                .unwrap(),
            );
        }
        // vcc = (vid == 1).
        code.extend(
            encode(
                "V_CMP_EQ_U32",
                "ENC_VOPC",
                &[("SRC0", 129), ("VSRC1", 0)],
                None,
            )
            .unwrap(),
        );
        // v5 = vcc ? v2(0.5) : v1(-0.5)  → y.
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP2",
                &[("VDST", 5), ("SRC0", 257), ("VSRC1", 2)],
                None,
            )
            .unwrap(),
        );
        // v6 = vcc ? v3(0.0) : v2(0.5)  → x so far.
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP2",
                &[("VDST", 6), ("SRC0", 258), ("VSRC1", 3)],
                None,
            )
            .unwrap(),
        );
        // vcc = (vid == 0).
        code.extend(
            encode(
                "V_CMP_EQ_U32",
                "ENC_VOPC",
                &[("SRC0", 128), ("VSRC1", 0)],
                None,
            )
            .unwrap(),
        );
        // v6 = vcc ? v1(-0.5) : v6  → x.
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP2",
                &[("VDST", 6), ("SRC0", 262), ("VSRC1", 1)],
                None,
            )
            .unwrap(),
        );
        // pos = param0 = (x, y, 0, 1).
        for (tgt, done) in [(12u64, 0u64), (32, 1)] {
            code.extend(
                encode(
                    "EXP",
                    "ENC_EXP",
                    &[
                        ("TGT", tgt),
                        ("EN", 0xF),
                        ("VSRC0", 6),
                        ("VSRC1", 5),
                        ("VSRC2", 3),
                        ("VSRC3", 4),
                        ("DONE", done),
                    ],
                    None,
                )
                .unwrap(),
            );
        }
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    fn triangle_vs() -> VsInterface {
        VsInterface {
            inputs: vec![],
            outputs: vec![
                sig("SV_Position", 0, 1, 3, 0, 0xF),
                sig("COLOR", 0, 0, 3, 1, 0xF),
            ],
            attr_base: 4,
            params: None,
        }
    }

    /// SCC through the scalar pair: scc = (s0 == s1), sdst = scc ? ssrc0 : ssrc1.
    fn scalar_select_code() -> Vec<u8> {
        let mut code = Vec::new();
        for (sdst, src) in [(0u64, 135u64), (1, 135u64), (2, 136u64)] {
            code.extend(
                encode(
                    "S_MOV_B32",
                    "ENC_SOP1",
                    &[("SDST", sdst), ("SSRC0", src)],
                    None,
                )
                .unwrap(),
            );
        }
        // s0 == s1 (7 == 7): scc = 1, s3 = scc ? s0(7) : s2(8).
        code.extend(
            encode(
                "S_CMP_EQ_U32",
                "ENC_SOPC",
                &[("SSRC0", 0), ("SSRC1", 1)],
                None,
            )
            .unwrap(),
        );
        code.extend(
            encode(
                "S_CSELECT_B32",
                "ENC_SOP2",
                &[("SDST", 3), ("SSRC0", 0), ("SSRC1", 2)],
                None,
            )
            .unwrap(),
        );
        // s0 == s2 (7 == 8): scc = 0, s4 = scc ? s0(7) : s2(8).
        code.extend(
            encode(
                "S_CMP_EQ_U32",
                "ENC_SOPC",
                &[("SSRC0", 0), ("SSRC1", 2)],
                None,
            )
            .unwrap(),
        );
        code.extend(
            encode(
                "S_CSELECT_B32",
                "ENC_SOP2",
                &[("SDST", 4), ("SSRC0", 0), ("SSRC1", 2)],
                None,
            )
            .unwrap(),
        );
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    #[test]
    fn scalar_compare_select_completes() {
        let code = scalar_select_code();
        let vs = blit_vs();
        let t = GlslVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}\n{}", t.unsupported, t.glsl);
        assert!(
            t.glsl.contains("scc = ((s0 == s1) ? 1u : 0u);"),
            "{}",
            t.glsl
        );
        // Codebase convention (lower.rs, emit.rs): SCC-true picks SSRC0.
        assert!(
            t.glsl.contains("s3 = ((scc != 0u) ? s0 : s2);"),
            "{}",
            t.glsl
        );
        assert!(
            t.glsl.contains("scc = ((s0 == s2) ? 1u : 0u);"),
            "{}",
            t.glsl
        );
        assert!(
            t.glsl.contains("s4 = ((scc != 0u) ? s0 : s2);"),
            "{}",
            t.glsl
        );
    }

    #[test]
    fn triangle_vertexindex_select_emits_position() {
        let code = triangle_code();
        let vs = triangle_vs();
        let t = GlslVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}\n{}", t.unsupported, t.glsl);
        assert!(t.glsl.contains("#version 450"), "{}", t.glsl);
        assert!(t.glsl.contains("gl_Position"), "{}", t.glsl);
        assert!(t.glsl.contains("gl_VertexIndex"), "{}", t.glsl);
        assert!(t.glsl.contains("out_p0"), "{}", t.glsl);
        // The compare writes the VCC homes; the selects read them.
        assert!(
            t.glsl.contains("s106 = ((1u == v0) ? 0xFFFFFFFFu : 0u);"),
            "{}",
            t.glsl
        );
        assert!(
            t.glsl.contains("s106 = ((0u == v0) ? 0xFFFFFFFFu : 0u);"),
            "{}",
            t.glsl
        );
        assert!(t.glsl.contains("((s106 != 0u) ? v2 : v1)"), "{}", t.glsl);
        // glslc bring-up check where available, else the structural assertions gate.
        let Ok(which) = std::process::Command::new("glslc")
            .arg("--version")
            .output()
        else {
            println!("glslc not available – skipping compile, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("glslc not available – skipping compile, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-glsl-tri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("tri.vert");
        let spv = dir.join("tri.vert.spv");
        std::fs::write(&src, &t.glsl).unwrap();
        let out = std::process::Command::new("glslc")
            .arg("-fshader-stage=vert")
            .arg(&src)
            .arg("-o")
            .arg(&spv)
            .output()
            .expect("spawn glslc");
        assert!(
            out.status.success(),
            "glslc failed: {}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            t.glsl
        );
        assert!(
            std::fs::metadata(&spv)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "glslc produced no output"
        );
    }

    #[test]
    fn retail_blit_vs_glsl_opt_in() {
        let Ok(path) = std::env::var("XCL_SAMPLE_RPF") else {
            println!("XCL_SAMPLE_RPF not set – skipping");
            return;
        };
        let data = std::fs::read(&path).unwrap();
        let rec = crate::gpu::pack::scan(&data, 0x40)
            .into_iter()
            .find(|r| r.name == "VS_BlitGenAmbCube_0cfdb358_Wrapped")
            .expect("record");
        let blob = crate::gpu::pack::inflate(&data, &rec).unwrap();
        let sh = crate::gpu::xbox::parse(&blob).unwrap();
        let fs = &sh.fssm[0];
        let p = fs.programs[0];
        let code = &fs.data[p.offset..p.offset + p.len];
        let mode = super::super::interface(&blob, &sh, fs).expect("interface");
        let super::super::Mode::Vertex(vs) = mode else {
            panic!("expected a vertex shader, got {mode:?}")
        };
        let t = GlslVsBackend::translate_vs(code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}\n{}", t.unsupported_why, t.glsl);
        assert!(t.glsl.contains("gl_Position"), "{}", t.glsl);
        let Ok(which) = std::process::Command::new("glslc")
            .arg("--version")
            .output()
        else {
            println!("glslc not available – skipping compile");
            return;
        };
        if !which.status.success() {
            println!("glslc not available – skipping compile");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-glsl-retail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("retail.vert");
        let spv = dir.join("retail.vert.spv");
        std::fs::write(&src, &t.glsl).unwrap();
        let out = std::process::Command::new("glslc")
            .arg("-fshader-stage=vert")
            .arg(&src)
            .arg("-o")
            .arg(&spv)
            .output()
            .expect("spawn glslc");
        assert!(
            out.status.success(),
            "glslc failed: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            t.glsl
        );
    }

    #[test]
    fn shipped_path_spawns_no_compiler() {
        // The shipped `translate_vs_glsl` must never spawn a process: no
        // `Command` / `process` usage outside tests (tool names may appear in
        // docs/comments, so only the spawn mechanism itself is gated).
        let src = include_str!("glsl.rs");
        let shipped = src.split("#[cfg(test)]").next().unwrap_or(src);
        for needle in ["Command::new", "std::process"] {
            assert!(
                !shipped.contains(needle),
                "shipped GLSL path mentions {needle}"
            );
        }
    }
}
