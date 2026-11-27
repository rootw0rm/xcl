//! RDNA2 → SPIR-V backend for blit-class vertex shaders (Phase D step 14).
//!
//! In-process emitter for the same straight-line blit subset the GLSL backend
//! covers (`glsl.rs`): scalar ALU + a small VALU set + `exp pos`/`param` +
//! `s_endpgm`. Branches are rejected with their shape (`BranchFlow`): a
//! structured translation would have to model them, and the console VS
//! corpus says that is not this backend's blocker (see
//! `docs/record-vs-coverage.md`). The full-EXEC entry state is assumed
//! (documented, true of the blits gated here). Anything else is recorded in
//! `unsupported`/`ignored`, never a crash.
//!
//! The output is hand-assembled SPIR-V 1.0 words (`Capability Shader`,
//! `Logical GLSL450`, `Vertex` entry, `Position` builtin + ranked param outs,
//! `VertexIndex`/`InstanceIndex` builtins). No external compiler, no process
//! spawn on any path: the shipped `translate_vs_spirv` only builds words.
//! Tests may shell out to `spirv-val`/`spirv-dis` behind `#[cfg(test)]`.

use std::collections::BTreeMap;

use super::{elements_at, ranked, vs_param_register, VsInterface};
use crate::gpu::rdna2::{self, Decoded, Operand, Variant};

/// SPIR-V 1.0 translation of one blit-class vertex shader.
#[derive(Debug, Clone, Default)]
pub struct SpirvTranslation {
    /// Raw SPIR-V words, including the 5-word header.
    pub words: Vec<u32>,
    /// RDNA2 mnemonics seen but not translated (mnemonic → count).
    pub unsupported: BTreeMap<String, usize>,
    /// "mnemonic: reason" → count.
    pub unsupported_why: BTreeMap<String, usize>,
    /// Instructions without an effect in this model (waits, prefetches…).
    pub ignored: BTreeMap<String, usize>,
    /// Decoded instruction count.
    pub instructions: usize,
}

impl SpirvTranslation {
    /// True when every instruction translated (no `unsupported` entries).
    pub fn complete(&self) -> bool {
        self.unsupported.is_empty()
    }
}

/// The control flow a decoded program has beyond straight-line code.
///
/// The bring-up backend rejects every branch; this says what a structured
/// SPIR-V translation would have to model, so the refusal is measurable in
/// `xcl shaders --record-vs` instead of one opaque reason. Counts are
/// branches (one edge each), not instructions: `branch_flow` only sees
/// resolved direct branches (`rdna2::branch_target`, far jumps included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BranchFlow {
    /// `s_cbranch_execz` / `s_cbranch_execnz`: the branch tests the EXEC lane
    /// mask, i.e. a region guarded by `s_and_saveexec_b64`-class mask writes.
    pub lane: usize,
    /// Other conditionals: SCC (`s_cbranch_scc0/1`), VCC (`vccz/vccnz`) and
    /// the debug/system forms.
    pub predicate: usize,
    /// `s_branch` and resolved far jumps.
    pub unconditional: usize,
    /// Edges whose target is at or before the branch itself (loops,
    /// including a branch to its own pc).
    pub loops: usize,
}

impl BranchFlow {
    /// Branch instructions counted.
    pub fn branches(&self) -> usize {
        self.lane + self.predicate + self.unconditional
    }

    /// True for straight-line code (no direct branch).
    pub fn is_empty(&self) -> bool {
        self.branches() == 0
    }

    /// The shapes present as a reason suffix, e.g. `"exec-mask branches,
    /// scc/vcc branches, backward edges"`; empty for straight-line code.
    ///
    /// Unconditional branches are named only when no conditional branch is
    /// present (they are the whole shape), so a program whose only flow is
    /// `s_branch` or a resolved far jump does not render as `()`.
    pub fn shape(&self) -> String {
        let mut parts = Vec::new();
        if self.lane > 0 {
            parts.push("exec-mask branches");
        }
        if self.predicate > 0 {
            parts.push("scc/vcc branches");
        }
        if self.unconditional > 0 && self.lane == 0 && self.predicate == 0 {
            parts.push("unconditional branches");
        }
        if self.loops > 0 {
            parts.push("backward edges");
        }
        parts.join(", ")
    }
}

/// Classifies the resolved direct branches of a decoded program.
pub fn branch_flow(insts: &[Decoded]) -> BranchFlow {
    let mut flow = BranchFlow::default();
    for d in insts {
        let Some(target) = rdna2::branch_target(d) else {
            continue;
        };
        let name = d.inst.name.to_ascii_lowercase();
        match name.as_str() {
            "s_cbranch_execz" | "s_cbranch_execnz" => flow.lane += 1,
            n if n.starts_with("s_cbranch") => flow.predicate += 1,
            _ => flow.unconditional += 1,
        }
        if target <= d.pc {
            flow.loops += 1;
        }
    }
    flow
}

/// The SPIR-V backend behind the shared [`VsInterface`] contract.
pub struct SpirvVsBackend;

impl super::VsBackend for SpirvVsBackend {
    type Output = SpirvTranslation;
    fn translate_vs(code: &[u8], vs: &VsInterface, wave64: bool) -> Result<Self::Output, String> {
        translate_vs_spirv(code, vs, wave64)
    }
}

/// Translate blit-class VS bytes to SPIR-V words.
pub fn translate_vs_spirv(
    code: &[u8],
    vs: &VsInterface,
    wave64: bool,
) -> Result<SpirvTranslation, String> {
    let mut insts: Vec<Decoded> = Vec::new();
    let mut off = 0usize;
    while off + 4 <= code.len() {
        let d = rdna2::decode(&code[off..], off as u64)
            .map_err(|e| format!("+{off:#x}: cannot decode ({e:?})"))?;
        off += d.len;
        insts.push(d);
    }
    rdna2::resolve_far_jumps(&mut insts);
    if let Some(d) = insts
        .iter()
        .find(|d| rdna2::branch_target(d).is_some())
    {
        let shape = branch_flow(&insts).shape();
        return Err(format!(
            "+{:#x}: branches are not supported by the SPIR-V bring-up backend ({shape})",
            d.pc
        ));
    }
    let mut em = SpirvEm::new(vs.clone(), wave64);
    let mut ended = false;
    for d in &insts {
        if ended {
            break;
        }
        ended = em.step(d)?;
    }
    let (words, unsupported, unsupported_why, ignored) = em.finish();
    Ok(SpirvTranslation {
        words,
        unsupported,
        unsupported_why,
        ignored,
        instructions: insts.len(),
    })
}

/// M2 draw DXBC S2: VS words for a stashed record container: the S1 console
/// interface (inputs/locations, param table) with the FSSM's own program
/// bytes and wave size (`fssm_wave64`), through the same `translate_vs_spirv`
/// the S3 blit path uses. A non-VS stage, a missing program, or anything the
/// backend rejects fails closed and the caller keeps today's triangle
/// fallback.
pub fn record_container_vs_words(blob: &[u8]) -> Result<SpirvTranslation, String> {
    let sh = crate::gpu::xbox::parse(blob).ok_or_else(|| "not a console VS container".to_string())?;
    if sh.stage != "VS" {
        return Err(format!("console stage {} is not VS", sh.stage));
    }
    let f = sh
        .fssm
        .iter()
        .find(|f| f.stages.first().is_some_and(|s| s.hw == 4))
        .ok_or_else(|| "no hardware VS variant".to_string())?;
    let vs = match super::interface(blob, &sh, f) {
        Some(super::Mode::Vertex(vs)) => vs,
        _ => return Err("unknown VS interface".into()),
    };
    let p = f.programs.first().ok_or_else(|| "FSSM without a program".to_string())?;
    let code = &f.data[p.offset..p.offset + p.len];
    translate_vs_spirv(code, &vs, super::fssm_wave64(f, code))
}

/// M2 draw S3: VS words for a stashed draw record. A console VS container
/// (S2) translates with its own interface (inputs/locations, param table)
/// and wave size; raw bytes keep the minimal VertexIndex-only bring-up
/// interface (no vertex inputs, Position-only output, `attr_base` 4).
/// Empty bytes or anything the backend rejects (PC DXBC, non-blit) fails
/// closed and the caller falls back to the hand-assembled title triangle.
pub fn record_vs_words(code: &[u8]) -> Result<SpirvTranslation, String> {
    if code.is_empty() {
        return Err("empty VS bytes: no instructions to translate".into());
    }
    // S2: a stashed console container carries its own interface — use it.
    if crate::gpu::xbox::is_xbox(code) {
        return record_container_vs_words(code);
    }
    let vs = VsInterface {
        inputs: vec![],
        outputs: vec![crate::gpu::dxbc::SigElement {
            name: "SV_Position".into(),
            index: 0,
            system_value: 1,
            component_type: 3,
            register: 0,
            mask: 0xF,
            rw_mask: 0xF,
            stream: 0,
        }],
        attr_base: 4,
        params: None,
    };
    translate_vs_spirv(code, &vs, true)
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

fn out_comps(e: &crate::gpu::dxbc::SigElement) -> usize {
    if matches!(e.system_value, 2 | 3) {
        (8 - e.mask.leading_zeros()) as usize
    } else {
        4
    }
}

// SPIR-V opcode / enum literals (per `spirv-headers`, unified1).
const OP_NAME: u16 = 5;
const OP_DECORATE: u16 = 71;
const OP_CAPABILITY: u16 = 17;
const OP_MEMORY_MODEL: u16 = 14;
const OP_ENTRY_POINT: u16 = 15;
const OP_TYPE_VOID: u16 = 19;
const OP_TYPE_BOOL: u16 = 20;
const OP_TYPE_INT: u16 = 21;
const OP_TYPE_FLOAT: u16 = 22;
const OP_TYPE_VECTOR: u16 = 23;
const OP_TYPE_POINTER: u16 = 32;
const OP_TYPE_FUNCTION: u16 = 33;
const OP_CONSTANT: u16 = 43;
const OP_FUNCTION: u16 = 54;
const OP_VARIABLE: u16 = 59;
const OP_LOAD: u16 = 61;
const OP_STORE: u16 = 62;
const OP_ACCESS_CHAIN: u16 = 65;
const OP_COMPOSITE_EXTRACT: u16 = 81;
const OP_COMPOSITE_CONSTRUCT: u16 = 80;
const OP_CONVERT_F_TO_U: u16 = 109;
const OP_CONVERT_F_TO_S: u16 = 110;
const OP_CONVERT_S_TO_F: u16 = 111;
const OP_CONVERT_U_TO_F: u16 = 112;
const OP_BITCAST: u16 = 124;
const OP_IADD: u16 = 128;
const OP_FADD: u16 = 129;
const OP_ISUB: u16 = 130;
const OP_FSUB: u16 = 131;
const OP_IMUL: u16 = 132;
const OP_FMUL: u16 = 133;
const OP_LOGICAL_OR: u16 = 167;
const OP_SELECT: u16 = 169;
const OP_I_EQUAL: u16 = 170;
const OP_I_NOT_EQUAL: u16 = 171;
const OP_U_LESS_THAN: u16 = 176;
const OP_FORD_LESS_THAN: u16 = 184;
const OP_SHIFT_R_LOGICAL: u16 = 194;
const OP_SHIFT_R_ARITH: u16 = 195;
const OP_SHIFT_L_LOGICAL: u16 = 196;
const OP_BITWISE_OR: u16 = 197;
const OP_BITWISE_XOR: u16 = 198;
const OP_BITWISE_AND: u16 = 199;
const OP_NOT: u16 = 200;
const OP_BIT_COUNT: u16 = 205;
const OP_LABEL: u16 = 248;
const OP_RETURN: u16 = 253;
const OP_FUNCTION_END: u16 = 56;
const OP_EXT_INST_IMPORT: u16 = 11;
const OP_EXT_INST: u16 = 12;
const OP_U_CONVERT: u16 = 113;
const OP_F_CONVERT: u16 = 115;
const OP_FDIV: u16 = 136;
const OP_U_GREATER_THAN: u16 = 172;
const OP_S_GREATER_THAN: u16 = 173;
const OP_U_GREATER_THAN_EQUAL: u16 = 174;
const OP_S_GREATER_THAN_EQUAL: u16 = 175;
const OP_S_LESS_THAN: u16 = 177;
const OP_U_LESS_THAN_EQUAL: u16 = 178;
const OP_S_LESS_THAN_EQUAL: u16 = 179;
const OP_FORD_EQUAL: u16 = 180;
const OP_FUNORD_EQUAL: u16 = 181;
const OP_FUNORD_NOT_EQUAL: u16 = 183;
const OP_FUNORD_LESS_THAN: u16 = 185;
const OP_FORD_GREATER_THAN: u16 = 186;
const OP_FUNORD_GREATER_THAN: u16 = 187;
const OP_FORD_LESS_THAN_EQUAL: u16 = 188;
const OP_FUNORD_LESS_THAN_EQUAL: u16 = 189;
const OP_FORD_GREATER_THAN_EQUAL: u16 = 190;
const OP_FUNORD_GREATER_THAN_EQUAL: u16 = 191;
/// Capability value (`SpvCapabilityInt64`), not an opcode.
const CAP_INT64: u32 = 11;
const CAP_FLOAT16: u32 = 9;
/// GLSL.std.450 instruction numbers this backend emits.
const GLSL_TRUNC: u32 = 3;
const GLSL_FLOOR: u32 = 8;
const GLSL_SIN: u32 = 13;
const GLSL_COS: u32 = 14;
const GLSL_EXP2: u32 = 29;
const GLSL_LOG2: u32 = 30;
const GLSL_SQRT: u32 = 31;
const GLSL_INVERSE_SQRT: u32 = 32;
/// `FindILsb`: index of the least-significant 1-bit, 0xFFFF_FFFF when zero.
const GLSL_FIND_I_LSB: u32 = 73;
/// `FClamp`.
const GLSL_F_CLAMP: u32 = 43;

const EXEC_MODEL_VERTEX: u32 = 0;
const ADDR_LOGICAL: u32 = 0;
const MEM_GLSL450: u32 = 1;
const CAP_SHADER: u32 = 1;
const STORAGE_INPUT: u32 = 1;
const STORAGE_OUTPUT: u32 = 3;
const STORAGE_FUNCTION: u32 = 7;
const DECOR_LOCATION: u32 = 30;
const DECOR_BUILT_IN: u32 = 11;
const BUILT_IN_POSITION: u32 = 0;
const BUILT_IN_VERTEX_INDEX: u32 = 42;
const BUILT_IN_INSTANCE_INDEX: u32 = 43;

fn inst(out: &mut Vec<u32>, opcode: u16, operands: &[u32]) {
    out.push(((operands.len() as u32 + 1) << 16) | opcode as u32);
    out.extend_from_slice(operands);
}

fn string_words(s: &str) -> Vec<u32> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// The extension encoding (SDWA/DPP fields in the second dword) of a VOP
/// instruction, or None for a plain form or an encoding that carries none.
fn ext_encoding(d: &Decoded) -> Option<&'static rdna2::Encoding> {
    let base = match d.enc.name {
        "ENC_VOP1" => "VOP1",
        "ENC_VOP2" => "VOP2",
        "ENC_VOPC" => "VOPC",
        _ => return None,
    };
    let name = match (d.variant, base) {
        (Variant::Sdwa, "VOPC") => "VOPC_VOP_SDWA_SDST_ENC".to_string(),
        (Variant::Sdwa, b) => format!("{b}_VOP_SDWA"),
        (Variant::Dpp16, b) => format!("{b}_VOP_DPP16"),
        (Variant::Dpp8, b) => format!("{b}_VOP_DPP8"),
        _ => return None,
    };
    rdna2::encoding(&name)
}

/// SDWA operand fields of a VOP1/VOP2 instruction: which byte/word of each
/// source is read and how, and where in the destination the result lands.
struct Sdwa {
    src0: u64,
    src0_scalar: bool,
    src0_sel: u32,
    src0_sext: bool,
    src0_abs: bool,
    src0_neg: bool,
    src1: u64,
    src1_scalar: bool,
    src1_sel: u32,
    src1_sext: bool,
    dst_sel: u32,
    dst_unused: u32,
    omod: u32,
    clamp: bool,
}

impl Sdwa {
    fn of(d: &Decoded) -> Option<Sdwa> {
        if d.variant != Variant::Sdwa {
            return None;
        }
        let e = ext_encoding(d)?;
        let f = |n: &str| rdna2::field_value(e, &d.words, n).unwrap_or(0);
        let bit = |n: &str| f(n) != 0;
        Some(Sdwa {
            src0: f("VSRC0"),
            src0_scalar: bit("S0"),
            src0_sel: f("SRC0_SEL") as u32,
            src0_sext: bit("SRC0_SEXT"),
            src0_abs: bit("SRC0_ABS"),
            src0_neg: bit("SRC0_NEG"),
            src1: f("VSRC1"),
            src1_scalar: bit("S1"),
            src1_sel: f("SRC1_SEL") as u32,
            src1_sext: bit("SRC1_SEXT"),
            dst_sel: f("DST_SEL") as u32,
            dst_unused: f("DST_UNUSED") as u32,
            omod: f("OMOD") as u32,
            clamp: bit("CLAMP"),
        })
    }
}

struct InputVar {
    id: u32,
    register: u32,
    component_type: u32,
    vec_ty: u32,
}

struct OutputVar {
    id: u32,
    out_index: usize,
}

struct SpirvEm {
    vs: VsInterface,
    #[allow(dead_code)]
    wave64: bool,
    next: u32,
    // Cached type ids.
    void_ty: u32,
    bool_ty: u32,
    uint_ty: u32,
    int_ty: u32,
    float_ty: u32,
    v4uint_ty: u32,
    v4int_ty: u32,
    v4float_ty: u32,
    ptr_input_uint: u32,
    ptr_input_v4uint: u32,
    ptr_input_v4int: u32,
    ptr_input_v4float: u32,
    ptr_output_v4float: u32,
    ptr_output_v4uint: u32,
    ptr_output_v4int: u32,
    ptr_output_float: u32,
    ptr_output_uint: u32,
    ptr_output_int: u32,
    ptr_func_uint: u32,
    func_ty: u32,
    func_id: u32,
    label_id: u32,
    /// GLSL.std.450 instruction-set id (declared once, used by unary float ops).
    glsl_id: u32,
    /// 64-bit unsigned type, declared lazily by `v_mul_hi_u32`.
    u64_ty: u32,
    /// Whether the module uses 64-bit integers (gates `Capability Int64`).
    need_int64: bool,
    /// 16-bit float type and its 2-lane vector, declared lazily by the SDWA
    /// f16 conversions.
    half_ty: u32,
    v2half_ty: u32,
    /// Whether the module uses 16-bit floats (gates `Capability Float16`).
    need_float16: bool,
    // Module sections.
    names: Vec<u32>,
    decor: Vec<u32>,
    types_consts: Vec<u32>,
    global_vars: Vec<u32>,
    body: Vec<u32>,
    uint_consts: BTreeMap<u32, u32>,
    int_consts: BTreeMap<i32, u32>,
    // Globals.
    inputs: Vec<InputVar>,
    outputs: Vec<OutputVar>,
    pos_var: u32,
    vertex_index_var: u32,
    instance_index_var: u32,
    entry_interface: Vec<u32>,
    // Function-scope register homes (var ids).
    vgpr_vars: BTreeMap<u64, u32>,
    sgpr_vars: BTreeMap<u64, u32>,
    scc_var: Option<u32>,
    unsupported: BTreeMap<String, usize>,
    unsupported_why: BTreeMap<String, usize>,
    ignored: BTreeMap<String, usize>,
}

impl SpirvEm {
    fn new(vs: VsInterface, wave64: bool) -> Self {
        let mut em = SpirvEm {
            vs,
            wave64,
            next: 1,
            void_ty: 0,
            bool_ty: 0,
            uint_ty: 0,
            int_ty: 0,
            float_ty: 0,
            v4uint_ty: 0,
            v4int_ty: 0,
            v4float_ty: 0,
            ptr_input_uint: 0,
            ptr_input_v4uint: 0,
            ptr_input_v4int: 0,
            ptr_input_v4float: 0,
            ptr_output_v4float: 0,
            ptr_output_v4uint: 0,
            ptr_output_v4int: 0,
            ptr_output_float: 0,
            ptr_output_uint: 0,
            ptr_output_int: 0,
            ptr_func_uint: 0,
            func_ty: 0,
            func_id: 0,
            label_id: 0,
            glsl_id: 0,
            u64_ty: 0,
            need_int64: false,
            half_ty: 0,
            v2half_ty: 0,
            need_float16: false,
            names: Vec::new(),
            decor: Vec::new(),
            types_consts: Vec::new(),
            global_vars: Vec::new(),
            body: Vec::new(),
            uint_consts: BTreeMap::new(),
            int_consts: BTreeMap::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            pos_var: 0,
            vertex_index_var: 0,
            instance_index_var: 0,
            entry_interface: Vec::new(),
            vgpr_vars: BTreeMap::new(),
            sgpr_vars: BTreeMap::new(),
            scc_var: None,
            unsupported: BTreeMap::new(),
            unsupported_why: BTreeMap::new(),
            ignored: BTreeMap::new(),
        };
        em.declare_types();
        em.declare_globals();
        em
    }

    fn alloc(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    fn declare_types(&mut self) {
        let void_ty = self.alloc();
        inst(&mut self.types_consts, OP_TYPE_VOID, &[void_ty]);
        let bool_ty = self.alloc();
        inst(&mut self.types_consts, OP_TYPE_BOOL, &[bool_ty]);
        let uint_ty = self.alloc();
        inst(&mut self.types_consts, OP_TYPE_INT, &[uint_ty, 32, 0]);
        let int_ty = self.alloc();
        inst(&mut self.types_consts, OP_TYPE_INT, &[int_ty, 32, 1]);
        let float_ty = self.alloc();
        inst(&mut self.types_consts, OP_TYPE_FLOAT, &[float_ty, 32]);
        let v4uint_ty = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_VECTOR,
            &[v4uint_ty, uint_ty, 4],
        );
        let v4int_ty = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_VECTOR,
            &[v4int_ty, int_ty, 4],
        );
        let v4float_ty = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_VECTOR,
            &[v4float_ty, float_ty, 4],
        );
        let ptr_input_uint = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_input_uint, STORAGE_INPUT, uint_ty],
        );
        let ptr_input_v4uint = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_input_v4uint, STORAGE_INPUT, v4uint_ty],
        );
        let ptr_input_v4int = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_input_v4int, STORAGE_INPUT, v4int_ty],
        );
        let ptr_input_v4float = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_input_v4float, STORAGE_INPUT, v4float_ty],
        );
        let ptr_output_v4float = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_output_v4float, STORAGE_OUTPUT, v4float_ty],
        );
        let ptr_output_v4uint = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_output_v4uint, STORAGE_OUTPUT, v4uint_ty],
        );
        let ptr_output_v4int = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_output_v4int, STORAGE_OUTPUT, v4int_ty],
        );
        let ptr_output_float = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_output_float, STORAGE_OUTPUT, float_ty],
        );
        let ptr_output_uint = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_output_uint, STORAGE_OUTPUT, uint_ty],
        );
        let ptr_output_int = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_output_int, STORAGE_OUTPUT, int_ty],
        );
        let ptr_func_uint = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_POINTER,
            &[ptr_func_uint, STORAGE_FUNCTION, uint_ty],
        );
        let func_ty = self.alloc();
        inst(
            &mut self.types_consts,
            OP_TYPE_FUNCTION,
            &[func_ty, void_ty],
        );
        let func_id = self.alloc();
        let label_id = self.alloc();
        let glsl_id = self.alloc();
        self.void_ty = void_ty;
        self.bool_ty = bool_ty;
        self.uint_ty = uint_ty;
        self.int_ty = int_ty;
        self.float_ty = float_ty;
        self.v4uint_ty = v4uint_ty;
        self.v4int_ty = v4int_ty;
        self.v4float_ty = v4float_ty;
        self.ptr_input_uint = ptr_input_uint;
        self.ptr_input_v4uint = ptr_input_v4uint;
        self.ptr_input_v4int = ptr_input_v4int;
        self.ptr_input_v4float = ptr_input_v4float;
        self.ptr_output_v4float = ptr_output_v4float;
        self.ptr_output_v4uint = ptr_output_v4uint;
        self.ptr_output_v4int = ptr_output_v4int;
        self.ptr_output_float = ptr_output_float;
        self.ptr_output_uint = ptr_output_uint;
        self.ptr_output_int = ptr_output_int;
        self.ptr_func_uint = ptr_func_uint;
        self.func_ty = func_ty;
        self.func_id = func_id;
        self.label_id = label_id;
        self.glsl_id = glsl_id;
    }

    fn global_var(&mut self, ptr_ty: u32, storage: u32) -> u32 {
        let id = self.alloc();
        inst(&mut self.global_vars, OP_VARIABLE, &[ptr_ty, id, storage]);
        id
    }

    fn decorate_location(&mut self, target: u32, loc: u32) {
        inst(&mut self.decor, OP_DECORATE, &[target, DECOR_LOCATION, loc]);
    }

    fn decorate_builtin(&mut self, target: u32, builtin: u32) {
        inst(
            &mut self.decor,
            OP_DECORATE,
            &[target, DECOR_BUILT_IN, builtin],
        );
    }

    fn op_name(&mut self, target: u32, name: &str) {
        let mut ops = vec![target];
        ops.extend(string_words(name));
        inst(&mut self.names, OP_NAME, &ops);
    }

    fn declare_globals(&mut self) {
        // Inputs: one location per non-system ISG1 element.
        let input_elems: Vec<(u32, u32)> = self
            .vs
            .inputs
            .iter()
            .filter(|e| e.system_value == 0 && !e.name.starts_with("SV_"))
            .map(|e| (e.register, e.component_type))
            .collect();
        for (k, (reg, ct)) in input_elems.iter().enumerate() {
            let (vec_ty, ptr_ty) = match ct {
                1 => (self.v4uint_ty, self.ptr_input_v4uint),
                2 => (self.v4int_ty, self.ptr_input_v4int),
                _ => (self.v4float_ty, self.ptr_input_v4float),
            };
            let id = self.global_var(ptr_ty, STORAGE_INPUT);
            self.decorate_location(id, k as u32);
            self.op_name(id, &format!("in_a{reg}"));
            self.entry_interface.push(id);
            self.inputs.push(InputVar {
                id,
                register: *reg,
                component_type: *ct,
                vec_ty,
            });
        }
        // Outputs: one location per ranked non-system OSG1 element.
        let ranked: Vec<(u32, u8)> = ranked(&self.vs.outputs, true)
            .iter()
            .map(|e| (e.component_type, out_comps(e) as u8))
            .collect();
        for (k, (ct, n)) in ranked.iter().enumerate() {
            let n = *n as usize;
            let ptr_ty = if n == 1 {
                match ct {
                    1 => self.ptr_output_uint,
                    2 => self.ptr_output_int,
                    _ => self.ptr_output_float,
                }
            } else {
                match ct {
                    1 => self.ptr_output_v4uint,
                    2 => self.ptr_output_v4int,
                    _ => self.ptr_output_v4float,
                }
            };
            let id = self.global_var(ptr_ty, STORAGE_OUTPUT);
            self.decorate_location(id, k as u32);
            self.op_name(id, &format!("out_p{k}"));
            self.entry_interface.push(id);
            self.outputs.push(OutputVar { id, out_index: k });
        }
        // Position builtin.
        let pos = self.global_var(self.ptr_output_v4float, STORAGE_OUTPUT);
        self.decorate_builtin(pos, BUILT_IN_POSITION);
        self.op_name(pos, "gl_Position");
        self.pos_var = pos;
        self.entry_interface.push(pos);
        // Vertex / instance index builtins.
        let vi = self.global_var(self.ptr_input_uint, STORAGE_INPUT);
        self.decorate_builtin(vi, BUILT_IN_VERTEX_INDEX);
        self.op_name(vi, "gl_VertexIndex");
        self.vertex_index_var = vi;
        self.entry_interface.push(vi);
        let ii = self.global_var(self.ptr_input_uint, STORAGE_INPUT);
        self.decorate_builtin(ii, BUILT_IN_INSTANCE_INDEX);
        self.op_name(ii, "gl_InstanceIndex");
        self.instance_index_var = ii;
        self.entry_interface.push(ii);
        self.op_name(self.func_id, "main");
    }

    fn uint_const(&mut self, v: u32) -> u32 {
        if let Some(&id) = self.uint_consts.get(&v) {
            return id;
        }
        let id = self.alloc();
        let ty = self.uint_ty;
        inst(&mut self.types_consts, OP_CONSTANT, &[ty, id, v]);
        self.uint_consts.insert(v, id);
        id
    }

    fn int_const(&mut self, v: i32) -> u32 {
        if let Some(&id) = self.int_consts.get(&v) {
            return id;
        }
        let id = self.alloc();
        let ty = self.int_ty;
        inst(&mut self.types_consts, OP_CONSTANT, &[ty, id, v as u32]);
        self.int_consts.insert(v, id);
        id
    }

    fn emit_body(&mut self, opcode: u16, operands: &[u32]) {
        inst(&mut self.body, opcode, operands);
    }

    fn tmp(&mut self, ty: u32, opcode: u16, operands: &[u32]) -> u32 {
        let id = self.alloc();
        let mut ops = vec![ty, id];
        ops.extend_from_slice(operands);
        self.emit_body(opcode, &ops);
        id
    }

    fn load(&mut self, ty: u32, ptr: u32) -> u32 {
        self.tmp(ty, OP_LOAD, &[ptr])
    }

    /// `OpBitcast` uint bits → f32.
    fn as_float(&mut self, bits: u32) -> u32 {
        let ty = self.float_ty;
        self.tmp(ty, OP_BITCAST, &[bits])
    }

    /// `OpBitcast` f32 → uint bits (the register model stores bit patterns).
    fn as_uint(&mut self, f: u32) -> u32 {
        let ty = self.uint_ty;
        self.tmp(ty, OP_BITCAST, &[f])
    }

    /// One-operand GLSL.std.450 instruction on an f32 bit pattern.
    fn glsl_unary_f32(&mut self, x: u32, opnum: u32) -> u32 {
        let ty = self.float_ty;
        let glsl = self.glsl_id;
        let fx = self.as_float(x);
        let r = self.tmp(ty, OP_EXT_INST, &[glsl, opnum, fx]);
        self.as_uint(r)
    }

    /// `v_fract_f32`: `x - floor(x)`, with RDNA's `>= 1.0` clamp to the
    /// largest f32 below one (NaN stays NaN; matches the HLSL backend).
    fn fract_f32(&mut self, x: u32) -> u32 {
        let ft = self.float_ty;
        let fx = self.as_float(x);
        let floor = self.tmp(ft, OP_EXT_INST, &[self.glsl_id, GLSL_FLOOR, fx]);
        let r = self.tmp(ft, OP_FSUB, &[fx, floor]);
        let one_bits = self.uint_const(0x3F80_0000);
        let one = self.as_float(one_bits);
        let below_bits = self.uint_const(0x3F7F_FFFF);
        let below_one = self.as_float(below_bits);
        let cond = self.tmp(self.bool_ty, OP_FORD_GREATER_THAN_EQUAL, &[r, one]);
        let sel = self.tmp(ft, OP_SELECT, &[cond, below_one, r]);
        self.as_uint(sel)
    }

    /// `v_med3_f32` = `max(min(a, b), min(max(a, b), c))` (HLSL `med3f`).
    fn med3_f32(&mut self, a: u32, b: u32, c: u32) -> u32 {
        let lo = self.fmin(a, b);
        let hi = self.fmax(a, b);
        let hi = self.fmin(hi, c);
        self.fmax(lo, hi)
    }

    /// `v_perm_b32`: four byte selectors in `sel`; 0-3 pick bytes of `b`
    /// (VSRC1), 4-7 bytes of `a` (VSRC0), 8-11 sign bits, 12 zero, 13+ all-ones.
    /// Same table as the HLSL backend's `perm_b32`.
    fn perm_b32(&mut self, a: u32, b: u32, sel: u32) -> u32 {
        let uint_ty = self.uint_ty;
        let bool_ty = self.bool_ty;
        let mut acc = self.uint_const(0);
        let c0 = self.uint_const(0);
        let c3 = self.uint_const(3);
        let c4 = self.uint_const(4);
        let c8 = self.uint_const(8);
        let c255 = self.uint_const(0xFF);
        for i in 0..4u32 {
            let shift = self.uint_const(8 * i);
            let k = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[sel, shift]);
            let k = self.tmp(uint_ty, OP_BITWISE_AND, &[k, c255]);
            // byte from b (k<4) / a (4<=k<8): the byte offset is k (mod 4).
            let off = self.tmp(uint_ty, OP_BITWISE_AND, &[k, c3]);
            let off = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[off, c3]);
            let bb = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[b, off]);
            let bb = self.tmp(uint_ty, OP_BITWISE_AND, &[bb, c255]);
            let ab = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[a, off]);
            let ab = self.tmp(uint_ty, OP_BITWISE_AND, &[ab, c255]);
            let lt4 = self.tmp(bool_ty, OP_U_LESS_THAN, &[k, c4]);
            let low = self.tmp(uint_ty, OP_SELECT, &[lt4, bb, ab]);
            // sign/constant classes 8..12.
            let bit_ff = |em: &mut Self, v: u32, sh: u32| -> u32 {
                let shc = em.uint_const(sh);
                let x = em.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[v, shc]);
                let one = em.uint_const(1);
                let x = em.tmp(uint_ty, OP_BITWISE_AND, &[x, one]);
                let zero = em.uint_const(0);
                let nz = em.tmp(bool_ty, OP_I_NOT_EQUAL, &[x, zero]);
                let ff = em.uint_const(0xFF);
                em.tmp(uint_ty, OP_SELECT, &[nz, ff, zero])
            };
            let s8 = bit_ff(self, b, 15);
            let s9 = bit_ff(self, b, 31);
            let s10 = bit_ff(self, a, 15);
            let s11 = bit_ff(self, a, 31);
            let mut high = c255;
            let mut pick = |em: &mut Self, n: u32, v: u32| {
                let nc = em.uint_const(n);
                let eq = em.tmp(bool_ty, OP_I_EQUAL, &[k, nc]);
                let sel = em.tmp(uint_ty, OP_SELECT, &[eq, v, high]);
                high = sel;
            };
            pick(self, 12, c0);
            pick(self, 11, s11);
            pick(self, 10, s10);
            pick(self, 9, s9);
            pick(self, 8, s8);
            let lt8 = self.tmp(bool_ty, OP_U_LESS_THAN, &[k, c8]);
            let byte = self.tmp(uint_ty, OP_SELECT, &[lt8, low, high]);
            let byte = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[byte, shift]);
            acc = self.tmp(uint_ty, OP_BITWISE_OR, &[acc, byte]);
        }
        acc
    }

    /// 64-bit unsigned type, declared on first use with `Capability Int64`.
    fn uint64_ty(&mut self) -> u32 {
        if self.u64_ty == 0 {
            let id = self.alloc();
            inst(&mut self.types_consts, OP_TYPE_INT, &[id, 64, 0]);
            self.u64_ty = id;
            self.need_int64 = true;
        }
        self.u64_ty
    }

    fn uint64_const(&mut self, v: u64) -> u32 {
        let ty = self.uint64_ty();
        let id = self.alloc();
        inst(&mut self.types_consts, OP_CONSTANT, &[ty, id, v as u32, (v >> 32) as u32]);
        id
    }

    /// `v_mul_hi_u32`: high 32 bits of the widened 64-bit product.
    fn mul_hi_u32(&mut self, a: u32, b: u32) -> u32 {
        let ty64 = self.uint64_ty();
        let ua = self.tmp(ty64, OP_U_CONVERT, &[a]);
        let ub = self.tmp(ty64, OP_U_CONVERT, &[b]);
        let p = self.tmp(ty64, OP_IMUL, &[ua, ub]);
        let sh = self.uint64_const(32);
        let hi = self.tmp(ty64, OP_SHIFT_R_LOGICAL, &[p, sh]);
        let ty = self.uint_ty;
        self.tmp(ty, OP_U_CONVERT, &[hi])
    }

    /// `v2f16` type, declared on first use with `Capability Float16`.
    fn half2_ty(&mut self) -> u32 {
        if self.v2half_ty == 0 {
            let h = self.alloc();
            inst(&mut self.types_consts, OP_TYPE_FLOAT, &[h, 16]);
            let v = self.alloc();
            inst(&mut self.types_consts, OP_TYPE_VECTOR, &[v, h, 2]);
            self.half_ty = h;
            self.v2half_ty = v;
            self.need_float16 = true;
        }
        self.v2half_ty
    }

    /// `v_cvt_f32_f16` widening: the low 16 bits of `bits` are an f16. The
    /// module deliberately declares no scalar 16-bit integer type, so the
    /// dword bitcasts to a `v2f16` and lane 0 is widened.
    fn f16_low_to_f32_bits(&mut self, bits: u32) -> u32 {
        let v2 = self.half2_ty();
        let hv = self.tmp(v2, OP_BITCAST, &[bits]);
        let h = self.half_ty;
        let half = self.tmp(h, OP_COMPOSITE_EXTRACT, &[hv, 0]);
        let f = self.tmp(self.float_ty, OP_F_CONVERT, &[half]);
        self.as_uint(f)
    }

    /// `v_cvt_f16_f32` narrowing: round to nearest even and return the f16
    /// pattern in bits 15:0, zero above (the SDWA destination selector then
    /// places it).
    fn f32_bits_to_f16_low(&mut self, bits: u32) -> u32 {
        let v2 = self.half2_ty();
        let f = self.as_float(bits);
        let half_ty = self.half_ty;
        let h = self.tmp(half_ty, OP_F_CONVERT, &[f]);
        let composed = self.alloc();
        inst(
            &mut self.body,
            OP_COMPOSITE_CONSTRUCT,
            &[v2, composed, h, h],
        );
        let packed = self.tmp(self.uint_ty, OP_BITCAST, &[composed]);
        let mask = self.uint_const(0xFFFF);
        self.tmp(self.uint_ty, OP_BITWISE_AND, &[packed, mask])
    }

    /// Read SDWA source `i` (0 or 1) from the extension dword: an SGPR or
    /// inline constant when its `S` bit is set, else the named VGPR, then the
    /// selected byte/word with its extension.
    fn sdwa_read(&mut self, s: &Sdwa, i: usize) -> Option<u32> {
        let (reg, scalar, sel, sext) = if i == 0 {
            (s.src0, s.src0_scalar, s.src0_sel, s.src0_sext)
        } else {
            (s.src1, s.src1_scalar, s.src1_sel, s.src1_sext)
        };
        let raw = if scalar {
            self.code_value(reg, None)?
        } else {
            self.load_vgpr(reg)
        };
        Some(self.sdwa_sel(raw, sel, sext))
    }

    /// SDWA source select: BYTE_0..3 / WORD_0..1 of `x`, zero-extended, or
    /// sign-extended when `sext` is set. A DWORD select is a no-op.
    fn sdwa_sel(&mut self, x: u32, sel: u32, sext: bool) -> u32 {
        if sel >= 6 {
            return x;
        }
        let (sh, w) = if sel < 4 {
            (8 * sel, 8u32)
        } else {
            (16 * (sel - 4), 16u32)
        };
        let uint_ty = self.uint_ty;
        let shc = self.uint_const(sh);
        let x = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[x, shc]);
        let mask = self.uint_const((1u32 << w) - 1);
        let x = self.tmp(uint_ty, OP_BITWISE_AND, &[x, mask]);
        if !sext {
            return x;
        }
        let int_ty = self.int_ty;
        let k = self.uint_const(32 - w);
        let l = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[x, k]);
        let li = self.tmp(int_ty, OP_BITCAST, &[l]);
        let int_ty = self.int_ty;
        let k = self.int_const((32 - w) as i32);
        let r = self.tmp(int_ty, OP_SHIFT_R_ARITH, &[li, k]);
        let uint_ty = self.uint_ty;
        self.tmp(uint_ty, OP_BITCAST, &[r])
    }

    /// SDWA destination select and the unused-bits policy: PAD zeroes the
    /// other parts, SEXT extends the written value's sign, PRESERVE keeps the
    /// destination's old bits. A DWORD select returns the result unchanged.
    fn sdwa_dst(&mut self, s: &Sdwa, r: u32, n: u64) -> u32 {
        if s.dst_sel >= 6 {
            return r;
        }
        let (sh, w) = if s.dst_sel < 4 {
            (8 * s.dst_sel, 8u32)
        } else {
            (16 * (s.dst_sel - 4), 16u32)
        };
        let uint_ty = self.uint_ty;
        let mask = ((1u32 << w) - 1) << sh;
        let shc = self.uint_const(sh);
        let v = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[r, shc]);
        let m = self.uint_const(mask);
        let v = self.tmp(uint_ty, OP_BITWISE_AND, &[v, m]);
        match s.dst_unused {
            2 => {
                let old = self.load_vgpr(n);
                let nm = self.uint_const(!mask);
                let kept = self.tmp(uint_ty, OP_BITWISE_AND, &[old, nm]);
                self.tmp(uint_ty, OP_BITWISE_OR, &[kept, v])
            }
            1 => {
                let top = sh + w;
                if top >= 32 {
                    return v;
                }
                let shc = self.uint_const(w - 1);
                let bit = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[r, shc]);
                let one = self.uint_const(1);
                let bit = self.tmp(uint_ty, OP_BITWISE_AND, &[bit, one]);
                let zero = self.uint_const(0);
                let neg = self.tmp(self.bool_ty, OP_I_NOT_EQUAL, &[bit, zero]);
                let ext = self.uint_const(u32::MAX << top);
                let ext = self.tmp(uint_ty, OP_SELECT, &[neg, ext, zero]);
                self.tmp(uint_ty, OP_BITWISE_OR, &[v, ext])
            }
            _ => v,
        }
    }

    /// SDWA output modifier (x2 / x4 / /2) then clamp to [0, 1] on an f32
    /// bit pattern (the DXIL backend's `fres`).
    fn sdwa_output(&mut self, omod: u32, clamp: bool, bits: u32) -> u32 {
        let float_ty = self.float_ty;
        let mut x = bits;
        if let Some(k) = match omod {
            1 => Some(2.0f32),
            2 => Some(4.0f32),
            3 => Some(0.5f32),
            _ => None,
        } {
            let fx = self.as_float(x);
            let kb = self.uint_const(k.to_bits());
            let fk = self.as_float(kb);
            let m = self.tmp(float_ty, OP_FMUL, &[fx, fk]);
            x = self.as_uint(m);
        }
        if clamp {
            let fx = self.as_float(x);
            let glsl = self.glsl_id;
            let zb = self.uint_const(0);
            let zero = self.as_float(zb);
            let ob = self.uint_const(0x3F80_0000);
            let one = self.as_float(ob);
            let c = self.tmp(float_ty, OP_EXT_INST, &[glsl, GLSL_F_CLAMP, fx, zero, one]);
            x = self.as_uint(c);
        }
        x
    }

    fn vgpr_var(&mut self, n: u64) -> u32 {
        if let Some(&id) = self.vgpr_vars.get(&n) {
            return id;
        }
        let id = self.alloc();
        self.vgpr_vars.insert(n, id);
        id
    }

    fn sgpr_var(&mut self, code: u64) -> u32 {
        if let Some(&id) = self.sgpr_vars.get(&code) {
            return id;
        }
        let id = self.alloc();
        self.sgpr_vars.insert(code, id);
        id
    }

    fn scc_home(&mut self) -> u32 {
        if let Some(id) = self.scc_var {
            return id;
        }
        let id = self.alloc();
        self.scc_var = Some(id);
        id
    }

    fn load_vgpr(&mut self, n: u64) -> u32 {
        let var = self.vgpr_var(n);
        let ty = self.uint_ty;
        self.tmp(ty, OP_LOAD, &[var])
    }

    fn load_sgpr(&mut self, code: u64) -> u32 {
        let var = self.sgpr_var(code);
        let ty = self.uint_ty;
        self.tmp(ty, OP_LOAD, &[var])
    }

    fn load_scc(&mut self) -> u32 {
        let var = self.scc_home();
        let ty = self.uint_ty;
        self.tmp(ty, OP_LOAD, &[var])
    }

    fn store_vgpr(&mut self, n: u64, val: u32) {
        let var = self.vgpr_var(n);
        self.emit_body(OP_STORE, &[var, val]);
    }

    fn store_sgpr(&mut self, code: u64, val: u32) {
        let var = self.sgpr_var(code);
        self.emit_body(OP_STORE, &[var, val]);
    }

    fn store_scc(&mut self, val: u32) {
        let var = self.scc_home();
        self.emit_body(OP_STORE, &[var, val]);
    }

    fn code_value(&mut self, code: u64, lit: Option<u32>) -> Option<u32> {
        match code {
            0..=124 | 126 | 127 => Some(self.load_sgpr(code)),
            125 => Some(self.uint_const(0)),
            128..=192 => Some(self.uint_const((code - 128) as u32)),
            193..=208 => Some(self.uint_const((-((code - 192) as i64)) as u32)),
            240..=248 => {
                let f = f32_const(code)?;
                Some(self.uint_const(f.to_bits()))
            }
            251 | 252 => Some(self.uint_const(0xFFFF_FFFF)),
            253 => Some(self.load_scc()),
            255 => Some(self.uint_const(lit?)),
            256..=511 => Some(self.load_vgpr(code - 256)),
            235..=238 => Some(self.uint_const(0)),
            239 => Some(self.uint_const(0x7FFF_FFFF)),
            _ => None,
        }
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

    fn read(&mut self, d: &Decoded, o: &Operand) -> Option<u32> {
        match o.field {
            Some("LITERAL") => Some(self.uint_const(d.literal?)),
            Some(f) => {
                let v = Self::field(d, f);
                let width = d.enc.field_width(f);
                if o.ty.starts_with("OPR_VGPR") && width <= 8 {
                    return Some(self.load_vgpr(v));
                }
                match o.ty {
                    "OPR_SIMM16" => Some(self.uint_const(v as u32)),
                    "OPR_SIMM32" => Some(self.uint_const(d.literal?)),
                    _ => self.code_value(v, d.literal),
                }
            }
            None => match o.ty {
                "OPR_VCC" => Some(self.load_sgpr(106)),
                "OPR_EXEC" | "OPR_SDST_EXEC" => Some(self.load_sgpr(126)),
                "OPR_SDST_M0" => Some(self.load_sgpr(124)),
                "OPR_SSRC_SPECIAL_SCC" => Some(self.load_scc()),
                _ => None,
            },
        }
    }

    fn unsupported(&mut self, d: &Decoded, why: &str) {
        let m = rdna2::mnemonic(d);
        *self
            .unsupported_why
            .entry(format!("{m}: {why}"))
            .or_default() += 1;
        *self.unsupported.entry(m).or_default() += 1;
    }

    /// One decoded instruction; returns true when it ends the program.
    fn step(&mut self, d: &Decoded) -> Result<bool, String> {
        let name = d.inst.name.to_ascii_lowercase();
        if NOOPS.contains(&name.as_str()) {
            *self.ignored.entry(name).or_default() += 1;
            return Ok(false);
        }
        // DPP reads another lane; the scalar bring-up model cannot express it.
        // SDWA stays on the same lane and is translated for the measured forms
        // in `valu`, which refuses every other SDWA instruction.
        if matches!(d.variant, Variant::Dpp16 | Variant::Dpp8) {
            self.unsupported(d, "SDWA/DPP in the bring-up backend");
            return Ok(false);
        }
        match d.enc.name {
            "ENC_SOPP" => match name.as_str() {
                "s_endpgm" | "s_endpgm_saved" | "s_endpgm_ordered_ps_done" => Ok(true),
                "s_barrier" => Ok(false),
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
        // SOPK's immediate is a 16-bit signed field, not a literal dword.
        let simm = Self::field(d, "SIMM16") as u16 as i16 as i32 as u32;
        let src0 = if d.enc.name == "ENC_SOPK" {
            self.uint_const(simm)
        } else {
            match self.code_value(f("SSRC0"), d.literal) {
                Some(s) => s,
                None => {
                    self.unsupported(d, "op");
                    return;
                }
            }
        };
        let src1 = if d.enc.name == "ENC_SOP2" || d.enc.name == "ENC_SOPC" {
            match self.code_value(f("SSRC1"), d.literal) {
                Some(s) => s,
                None => {
                    self.unsupported(d, "op");
                    return;
                }
            }
        } else {
            0
        };
        let uint_ty = self.uint_ty;
        // SOPC writes SCC and has no SDST, so it must not fall through to the
        // destination logic below.
        if d.enc.name == "ENC_SOPC" {
            match self.sopc_cond(name, src0, src1) {
                Some(cond) => {
                    let v = self.bit01(cond);
                    self.store_scc(v);
                }
                None => self.unsupported(d, "salu"),
            }
            return;
        }
        let zero = self.uint_const(0);
        let c31 = self.uint_const(31);
        // SOPK add/mul read the destination as their first source.
        let cur = if matches!(name, "s_addk_i32" | "s_mulk_i32") {
            if dst_code > 127 {
                self.unsupported(d, "sdst");
                return;
            }
            Some(if dst_code == 125 {
                zero
            } else {
                self.load_sgpr(dst_code)
            })
        } else {
            None
        };
        let mut scc: Option<u32> = None;
        let res: Option<u32> = match name {
            "s_mov_b32" => Some(src0),
            "s_movk_i32" => Some(self.uint_const(simm)),
            "s_add_u32" => {
                let t = self.tmp(uint_ty, OP_IADD, &[src0, src1]);
                let carry = self.tmp(self.bool_ty, OP_U_LESS_THAN, &[t, src0]);
                scc = Some(self.bit01(carry));
                Some(t)
            }
            "s_add_i32" => {
                let t = self.tmp(uint_ty, OP_IADD, &[src0, src1]);
                let x = self.tmp(uint_ty, OP_BITWISE_XOR, &[src0, t]);
                let y = self.tmp(uint_ty, OP_BITWISE_XOR, &[src1, t]);
                let x = self.tmp(uint_ty, OP_BITWISE_AND, &[x, y]);
                let x = self.tmp(uint_ty, OP_SHIFT_R_ARITH, &[x, c31]);
                scc = Some(self.nz(x));
                Some(t)
            }
            "s_sub_u32" => {
                let t = self.tmp(uint_ty, OP_ISUB, &[src0, src1]);
                let borrow = self.tmp(self.bool_ty, OP_U_GREATER_THAN, &[src1, src0]);
                scc = Some(self.bit01(borrow));
                Some(t)
            }
            "s_sub_i32" => {
                let t = self.tmp(uint_ty, OP_ISUB, &[src0, src1]);
                let x = self.tmp(uint_ty, OP_BITWISE_XOR, &[src0, src1]);
                let y = self.tmp(uint_ty, OP_BITWISE_XOR, &[src0, t]);
                let x = self.tmp(uint_ty, OP_BITWISE_AND, &[x, y]);
                let x = self.tmp(uint_ty, OP_SHIFT_R_ARITH, &[x, c31]);
                scc = Some(self.nz(x));
                Some(t)
            }
            "s_and_b32" => {
                let v = self.tmp(uint_ty, OP_BITWISE_AND, &[src0, src1]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_or_b32" => {
                let v = self.tmp(uint_ty, OP_BITWISE_OR, &[src0, src1]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_xor_b32" => {
                let v = self.tmp(uint_ty, OP_BITWISE_XOR, &[src0, src1]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_not_b32" => {
                let v = self.tmp(uint_ty, OP_NOT, &[src0]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_min_u32" => {
                let v = self.umin(src0, src1);
                let lt = self.tmp(self.bool_ty, OP_U_LESS_THAN, &[src0, src1]);
                scc = Some(self.bit01(lt));
                Some(v)
            }
            "s_max_u32" => {
                let v = self.umax(src0, src1);
                let gt = self.tmp(self.bool_ty, OP_U_GREATER_THAN, &[src0, src1]);
                scc = Some(self.bit01(gt));
                Some(v)
            }
            "s_mul_i32" => Some(self.tmp(uint_ty, OP_IMUL, &[src0, src1])),
            "s_mul_hi_u32" => Some(self.mul_hi_u32(src0, src1)),
            "s_mulk_i32" => Some(self.tmp(uint_ty, OP_IMUL, &[cur.unwrap_or(zero), src0])),
            "s_addk_i32" => {
                let cur = cur.unwrap_or(zero);
                let t = self.tmp(uint_ty, OP_IADD, &[cur, src0]);
                let x = self.tmp(uint_ty, OP_BITWISE_XOR, &[cur, t]);
                let y = self.tmp(uint_ty, OP_BITWISE_XOR, &[src0, t]);
                let x = self.tmp(uint_ty, OP_BITWISE_AND, &[x, y]);
                let x = self.tmp(uint_ty, OP_SHIFT_R_ARITH, &[x, c31]);
                scc = Some(self.nz(x));
                Some(t)
            }
            "s_lshl_b32" => {
                let sh = self.tmp(uint_ty, OP_BITWISE_AND, &[src1, c31]);
                let v = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[src0, sh]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_lshr_b32" => {
                let sh = self.tmp(uint_ty, OP_BITWISE_AND, &[src1, c31]);
                let v = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[src0, sh]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_bfe_u32" => {
                let v = self.sbfe(src0, src1, false);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_bfe_i32" => {
                let v = self.sbfe(src0, src1, true);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_bcnt1_i32_b32" => {
                let v = self.tmp(uint_ty, OP_BIT_COUNT, &[src0]);
                scc = Some(self.nz(v));
                Some(v)
            }
            "s_ff1_i32_b32" => {
                let glsl = self.glsl_id;
                Some(self.tmp(uint_ty, OP_EXT_INST, &[glsl, GLSL_FIND_I_LSB, src0]))
            }
            "s_addc_u32" => {
                let carry_in = self.load_scc();
                let t0 = self.tmp(uint_ty, OP_IADD, &[src0, src1]);
                let t = self.tmp(uint_ty, OP_IADD, &[t0, carry_in]);
                let c0 = self.tmp(self.bool_ty, OP_U_LESS_THAN, &[t0, src0]);
                let c1 = self.tmp(self.bool_ty, OP_U_LESS_THAN, &[t, t0]);
                let carry_out = self.tmp(self.bool_ty, OP_LOGICAL_OR, &[c0, c1]);
                scc = Some(self.bit01(carry_out));
                Some(t)
            }
            // Codebase/GPU convention (lower.rs, emit.rs): SCC-true picks SSRC0.
            "s_cselect_b32" => {
                let scc = self.load_scc();
                let cond = self.tmp(self.bool_ty, OP_I_NOT_EQUAL, &[scc, zero]);
                Some(self.tmp(uint_ty, OP_SELECT, &[cond, src0, src1]))
            }
            _ => None,
        };
        match res {
            Some(v) => {
                if dst_code == 125 {
                    // null: discarded.
                } else if dst_code > 127 {
                    self.unsupported(d, "sdst");
                    return;
                } else {
                    self.store_sgpr(dst_code, v);
                }
            }
            None => {
                self.unsupported(d, "salu");
                return;
            }
        }
        if let Some(v) = scc {
            self.store_scc(v);
        }
    }

    /// Materialise a boolean as the SCC convention's `0`/`1` word.
    fn bit01(&mut self, cond: u32) -> u32 {
        let one = self.uint_const(1);
        let zero = self.uint_const(0);
        self.tmp(self.uint_ty, OP_SELECT, &[cond, one, zero])
    }

    /// SCC for result-nonzero writes.
    fn nz(&mut self, v: u32) -> u32 {
        let zero = self.uint_const(0);
        let cond = self.tmp(self.bool_ty, OP_I_NOT_EQUAL, &[v, zero]);
        self.bit01(cond)
    }

    /// SOPC 32-bit compares: condition id for `s_cmp_*`, `None` unsupported.
    fn sopc_cond(&mut self, name: &str, a: u32, b: u32) -> Option<u32> {
        let (rel, signed) = match name {
            "s_cmp_eq_i32" | "s_cmp_eq_u32" => ("eq", false),
            "s_cmp_lg_i32" | "s_cmp_lg_u32" => ("lg", false),
            "s_cmp_gt_i32" => ("gt", true),
            "s_cmp_ge_i32" => ("ge", true),
            "s_cmp_lt_i32" => ("lt", true),
            "s_cmp_le_i32" => ("le", true),
            "s_cmp_gt_u32" => ("gt", false),
            "s_cmp_ge_u32" => ("ge", false),
            "s_cmp_lt_u32" => ("lt", false),
            "s_cmp_le_u32" => ("le", false),
            _ => return None,
        };
        let (a, b) = if signed {
            let int_ty = self.int_ty;
            let ai = self.tmp(int_ty, OP_BITCAST, &[a]);
            let bi = self.tmp(int_ty, OP_BITCAST, &[b]);
            (ai, bi)
        } else {
            (a, b)
        };
        let op = match rel {
            "eq" => OP_I_EQUAL,
            "lg" => OP_I_NOT_EQUAL,
            "gt" if signed => OP_S_GREATER_THAN,
            "ge" if signed => OP_S_GREATER_THAN_EQUAL,
            "lt" if signed => OP_S_LESS_THAN,
            "le" if signed => OP_S_LESS_THAN_EQUAL,
            "gt" => OP_U_GREATER_THAN,
            "ge" => OP_U_GREATER_THAN_EQUAL,
            "lt" => OP_U_LESS_THAN,
            "le" => OP_U_LESS_THAN_EQUAL,
            _ => return None,
        };
        Some(self.tmp(self.bool_ty, op, &[a, b]))
    }

    /// `s_bfe_u32`/`s_bfe_i32`: offset = c[4:0], width = c[22:16]; width 0
    /// gives 0, width >= 32 the shifted word, otherwise the masked field.
    fn sbfe(&mut self, v: u32, c: u32, signed: bool) -> u32 {
        let uint_ty = self.uint_ty;
        let int_ty = self.int_ty;
        let c31 = self.uint_const(31);
        let c16 = self.uint_const(16);
        let c32 = self.uint_const(32);
        let c7f = self.uint_const(0x7F);
        let zero = self.uint_const(0);
        let o = self.tmp(uint_ty, OP_BITWISE_AND, &[c, c31]);
        let w = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[c, c16]);
        let w = self.tmp(uint_ty, OP_BITWISE_AND, &[w, c7f]);
        let s = self.tmp(uint_ty, OP_ISUB, &[c32, w]);
        let s = self.tmp(uint_ty, OP_BITWISE_AND, &[s, c31]);
        let big = self.tmp(self.bool_ty, OP_U_GREATER_THAN_EQUAL, &[w, c32]);
        let wzero = self.tmp(self.bool_ty, OP_I_EQUAL, &[w, zero]);
        if signed {
            let vi = self.tmp(int_ty, OP_BITCAST, &[v]);
            let oi = self.tmp(int_ty, OP_BITCAST, &[o]);
            let x = self.tmp(int_ty, OP_SHIFT_R_ARITH, &[vi, oi]);
            let si = self.tmp(int_ty, OP_BITCAST, &[s]);
            let y = self.tmp(int_ty, OP_SHIFT_L_LOGICAL, &[x, si]);
            let y = self.tmp(int_ty, OP_SHIFT_R_ARITH, &[y, si]);
            let r = self.tmp(int_ty, OP_SELECT, &[big, x, y]);
            let izero = self.int_const(0);
            let r = self.tmp(int_ty, OP_SELECT, &[wzero, izero, r]);
            self.tmp(uint_ty, OP_BITCAST, &[r])
        } else {
            let x = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[v, o]);
            let all = self.uint_const(0xFFFF_FFFF);
            let mask = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[all, s]);
            let xm = self.tmp(uint_ty, OP_BITWISE_AND, &[x, mask]);
            let r = self.tmp(uint_ty, OP_SELECT, &[big, x, xm]);
            self.tmp(uint_ty, OP_SELECT, &[wzero, zero, r])
        }
    }

    fn umin(&mut self, a: u32, b: u32) -> u32 {
        let bool_ty = self.bool_ty;
        let uint_ty = self.uint_ty;
        let cmp = self.tmp(bool_ty, OP_U_LESS_THAN, &[a, b]);
        self.tmp(uint_ty, OP_SELECT, &[cmp, a, b])
    }

    fn umax(&mut self, a: u32, b: u32) -> u32 {
        let bool_ty = self.bool_ty;
        let uint_ty = self.uint_ty;
        let cmp = self.tmp(bool_ty, OP_U_LESS_THAN, &[a, b]);
        self.tmp(uint_ty, OP_SELECT, &[cmp, b, a])
    }

    fn fmin(&mut self, a: u32, b: u32) -> u32 {
        // `a`/`b` are uint bit patterns of floats.
        let float_ty = self.float_ty;
        let bool_ty = self.bool_ty;
        let uint_ty = self.uint_ty;
        let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
        let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
        let cmp = self.tmp(bool_ty, OP_FORD_LESS_THAN, &[fa, fb]);
        let fr = self.tmp(float_ty, OP_SELECT, &[cmp, fa, fb]);
        self.tmp(uint_ty, OP_BITCAST, &[fr])
    }

    fn fmax(&mut self, a: u32, b: u32) -> u32 {
        let float_ty = self.float_ty;
        let bool_ty = self.bool_ty;
        let uint_ty = self.uint_ty;
        let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
        let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
        let cmp = self.tmp(bool_ty, OP_FORD_LESS_THAN, &[fa, fb]);
        let fr = self.tmp(float_ty, OP_SELECT, &[cmp, fb, fa]);
        self.tmp(uint_ty, OP_BITCAST, &[fr])
    }

    /// `v_cmp_*`: one boolean per lane (single-lane bring-up model), written as
    /// an all-ones/all-zero mask into the destination SGPR pair. Ordered float
    /// forms are false on NaN, the `n`-prefixed forms true, matching the DXIL
    /// backend's `float_compare`.
    fn vopc(&mut self, d: &Decoded, rest: &str) {
        if !matches!(d.enc.name, "ENC_VOPC" | "ENC_VOP3") {
            self.unsupported(d, "valu");
            return;
        }
        let Some((opn, ty)) = rest.rsplit_once('_') else {
            self.unsupported(d, "op");
            return;
        };
        let (_, ins) = Self::ops(d);
        if ins.len() != 2 {
            self.unsupported(d, "op");
            return;
        }
        let (Some(a), Some(b)) = (self.read(d, &ins[0]), self.read(d, &ins[1])) else {
            self.unsupported(d, "op");
            return;
        };
        let cond = match ty {
            "u32" | "i32" => {
                let signed = ty == "i32";
                let op = match opn {
                    "eq" => OP_I_EQUAL,
                    "ne" | "lg" => OP_I_NOT_EQUAL,
                    "lt" => {
                        if signed {
                            OP_S_LESS_THAN
                        } else {
                            OP_U_LESS_THAN
                        }
                    }
                    "le" => {
                        if signed {
                            OP_S_LESS_THAN_EQUAL
                        } else {
                            OP_U_LESS_THAN_EQUAL
                        }
                    }
                    "gt" => {
                        if signed {
                            OP_S_GREATER_THAN
                        } else {
                            OP_U_GREATER_THAN
                        }
                    }
                    "ge" => {
                        if signed {
                            OP_S_GREATER_THAN_EQUAL
                        } else {
                            OP_U_GREATER_THAN_EQUAL
                        }
                    }
                    _ => {
                        self.unsupported(d, "valu");
                        return;
                    }
                };
                self.tmp(self.bool_ty, op, &[a, b])
            }
            "f32" => {
                if opn == "class" {
                    self.fclass_cond(a, b)
                } else {
                    let fa = self.as_float(a);
                    let fb = self.as_float(b);
                    let op = match opn {
                        "lt" => OP_FORD_LESS_THAN,
                        "eq" => OP_FORD_EQUAL,
                        "le" => OP_FORD_LESS_THAN_EQUAL,
                        "gt" => OP_FORD_GREATER_THAN,
                        "ge" => OP_FORD_GREATER_THAN_EQUAL,
                        // Unordered forms, true when either operand is NaN.
                        "ne" | "lg" => OP_FUNORD_NOT_EQUAL,
                        "ngt" => OP_FUNORD_LESS_THAN_EQUAL,
                        "nge" => OP_FUNORD_LESS_THAN,
                        "nlt" => OP_FUNORD_GREATER_THAN_EQUAL,
                        "nle" => OP_FUNORD_GREATER_THAN,
                        "neq" => OP_FUNORD_NOT_EQUAL,
                        "nlg" => OP_FUNORD_EQUAL,
                        _ => {
                            self.unsupported(d, "valu");
                            return;
                        }
                    };
                    self.tmp(self.bool_ty, op, &[fa, fb])
                }
            }
            _ => {
                self.unsupported(d, "valu");
                return;
            }
        };
        let all = self.uint_const(0xFFFF_FFFF);
        let zero = self.uint_const(0);
        let mask = self.tmp(self.uint_ty, OP_SELECT, &[cond, all, zero]);
        match d.enc.name {
            "ENC_VOPC" => {
                // Single-wave bring-up: the lane bit lives in VCC lo (sgpr 106).
                self.store_sgpr(106, mask);
                self.store_sgpr(107, zero);
            }
            _ => {
                let dst = Self::field(d, "VDST");
                if dst == 125 {
                    // null destination: discarded.
                } else if dst > 126 {
                    self.unsupported(d, "sdst");
                } else {
                    self.store_sgpr(dst, mask);
                    self.store_sgpr(dst + 1, zero);
                }
            }
        }
    }

    /// `v_cmp_class_f32` condition: `(mask >> class(value)) & 1 != 0`, with the
    /// RDNA class bits 0 sNaN, 1 qNaN, 2 -inf, 3 -normal, 4 -denorm, 5 -0,
    /// 6 +0, 7 +denorm, 8 +normal, 9 +inf (DXIL `fclass`).
    fn fclass_cond(&mut self, bits: u32, mask: u32) -> u32 {
        let ut = self.uint_ty;
        let bt = self.bool_ty;
        let c0 = self.uint_const(0);
        let c1 = self.uint_const(1);
        let c2 = self.uint_const(2);
        let c3 = self.uint_const(3);
        let c4 = self.uint_const(4);
        let c5 = self.uint_const(5);
        let c6 = self.uint_const(6);
        let c7 = self.uint_const(7);
        let c8 = self.uint_const(8);
        let c9 = self.uint_const(9);
        let c23 = self.uint_const(23);
        let c31 = self.uint_const(31);
        let c255 = self.uint_const(0xFF);
        let c_frac = self.uint_const(0x7F_FFFF);
        let c_quiet = self.uint_const(0x40_0000);
        let e = self.tmp(ut, OP_SHIFT_R_LOGICAL, &[bits, c23]);
        let e = self.tmp(ut, OP_BITWISE_AND, &[e, c255]);
        let m = self.tmp(ut, OP_BITWISE_AND, &[bits, c_frac]);
        let s = self.tmp(ut, OP_SHIFT_R_LOGICAL, &[bits, c31]);
        let n = self.tmp(bt, OP_I_NOT_EQUAL, &[s, c0]);
        let mnz = self.tmp(bt, OP_I_NOT_EQUAL, &[m, c0]);
        let q = self.tmp(ut, OP_BITWISE_AND, &[m, c_quiet]);
        let qn = self.tmp(bt, OP_I_NOT_EQUAL, &[q, c0]);
        let nanc = self.tmp(ut, OP_SELECT, &[qn, c1, c0]);
        let infc = self.tmp(ut, OP_SELECT, &[n, c2, c9]);
        let special = self.tmp(ut, OP_SELECT, &[mnz, nanc, infc]);
        let zc = self.tmp(ut, OP_SELECT, &[n, c5, c6]);
        let dc = self.tmp(ut, OP_SELECT, &[n, c4, c7]);
        let zero = self.tmp(ut, OP_SELECT, &[mnz, dc, zc]);
        let normal = self.tmp(ut, OP_SELECT, &[n, c3, c8]);
        let ez = self.tmp(bt, OP_I_EQUAL, &[e, c0]);
        let emx = self.tmp(bt, OP_I_EQUAL, &[e, c255]);
        let r = self.tmp(ut, OP_SELECT, &[ez, zero, normal]);
        let class = self.tmp(ut, OP_SELECT, &[emx, special, r]);
        let bit = self.tmp(ut, OP_SHIFT_R_LOGICAL, &[mask, class]);
        let bit = self.tmp(ut, OP_BITWISE_AND, &[bit, c1]);
        self.tmp(bt, OP_I_NOT_EQUAL, &[bit, c0])
    }

    fn valu(&mut self, d: &Decoded, name: &str) {
        // The SDWA slice is the measured one (step-12); every other SDWA form
        // and every VOPC SDWA compare stays refused.
        if d.variant == Variant::Sdwa
            && !matches!(
                name,
                "v_cvt_f32_u32"
                    | "v_add_nc_u32"
                    | "v_lshlrev_b32"
                    | "v_or_b32"
                    | "v_cvt_f32_f16"
                    | "v_cvt_f16_f32"
            )
        {
            self.unsupported(d, "SDWA/DPP in the bring-up backend");
            return;
        }
        // V_CMP/V_CMPX write a mask, not a register: VOPC has the implicit VCC
        // pair, VOP3 an explicit SGPR-pair destination. Neither may fall through
        // to the register-destination logic below.
        if let Some(rest) = name.strip_prefix("v_cmp_") {
            self.vopc(d, rest);
            return;
        }
        let (outs, ins) = Self::ops(d);
        let sdwa = Sdwa::of(d);
        let mut u = Vec::new();
        if let Some(s) = &sdwa {
            for i in 0..ins.len().min(2) {
                match self.sdwa_read(s, i) {
                    Some(v) => u.push(v),
                    None => {
                        self.unsupported(d, "op");
                        return;
                    }
                }
            }
        } else {
            for o in &ins {
                match self.read(d, o) {
                    Some(s) => u.push(s),
                    None => {
                        self.unsupported(d, "op");
                        return;
                    }
                }
            }
        }
        #[derive(Clone, Copy)]
        enum Dst {
            V(u64),
            S(u64),
            Null,
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
        let c31 = self.uint_const(31);
        let uint_ty = self.uint_ty;
        let int_ty = self.int_ty;
        let float_ty = self.float_ty;
        let a = u.first().copied().unwrap_or_else(|| self.uint_const(0));
        let b = u.get(1).copied().unwrap_or_else(|| self.uint_const(0));
        let c = u.get(2).copied().unwrap_or_else(|| self.uint_const(0));
        let res: Option<u32> = match name {
            "v_mov_b32" | "v_mov_fed_b32" => Some(a),
            "v_not_b32" => Some(self.tmp(uint_ty, OP_NOT, &[a])),
            // Codebase/GPU convention (lower.rs F32_COMPARES): mask-true picks
            // VSRC1. VOP2 reads the implicit VCC (`c`), VOP3 the explicit SRC2.
            "v_cndmask_b32" => {
                let zero = self.uint_const(0);
                let cond = self.tmp(self.bool_ty, OP_I_NOT_EQUAL, &[c, zero]);
                Some(self.tmp(uint_ty, OP_SELECT, &[cond, b, a]))
            }
            "v_and_b32" => Some(self.tmp(uint_ty, OP_BITWISE_AND, &[a, b])),
            "v_or_b32" => Some(self.tmp(uint_ty, OP_BITWISE_OR, &[a, b])),
            "v_xor_b32" => Some(self.tmp(uint_ty, OP_BITWISE_XOR, &[a, b])),
            "v_lshlrev_b32" => {
                let m = self.tmp(uint_ty, OP_BITWISE_AND, &[a, c31]);
                Some(self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[b, m]))
            }
            "v_lshrrev_b32" => {
                let m = self.tmp(uint_ty, OP_BITWISE_AND, &[a, c31]);
                Some(self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[b, m]))
            }
            "v_ashrrev_i32" => {
                let bi = self.tmp(int_ty, OP_BITCAST, &[b]);
                let m = self.tmp(uint_ty, OP_BITWISE_AND, &[a, c31]);
                let mi = self.tmp(int_ty, OP_BITCAST, &[m]);
                let r = self.tmp(int_ty, OP_SHIFT_R_ARITH, &[bi, mi]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[r]))
            }
            // SDWA CLAMP on the unsigned add saturates to 0xFFFF_FFFF
            // (the DXIL backend's `v_add_nc_u32` clamp arm).
            "v_add_nc_u32" if sdwa.as_ref().is_some_and(|s| s.clamp) => {
                let t = self.tmp(uint_ty, OP_IADD, &[a, b]);
                let over = self.tmp(self.bool_ty, OP_U_LESS_THAN, &[t, a]);
                let all = self.uint_const(0xFFFF_FFFF);
                Some(self.tmp(uint_ty, OP_SELECT, &[over, all, t]))
            }
            "v_add_nc_u32" | "v_add_u32" | "v_add_co_u32" => {
                Some(self.tmp(uint_ty, OP_IADD, &[a, b]))
            }
            "v_sub_nc_u32" | "v_sub_u32" => Some(self.tmp(uint_ty, OP_ISUB, &[a, b])),
            "v_subrev_nc_u32" | "v_subrev_u32" => Some(self.tmp(uint_ty, OP_ISUB, &[b, a])),
            "v_mul_lo_u32" => Some(self.tmp(uint_ty, OP_IMUL, &[a, b])),
            "v_mul_hi_u32" => Some(self.mul_hi_u32(a, b)),
            "v_lshl_add_u32" => {
                let m = self.tmp(uint_ty, OP_BITWISE_AND, &[b, c31]);
                let sh = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[a, m]);
                Some(self.tmp(uint_ty, OP_IADD, &[sh, c]))
            }
            "v_lshl_or_b32" => {
                let m = self.tmp(uint_ty, OP_BITWISE_AND, &[b, c31]);
                let sh = self.tmp(uint_ty, OP_SHIFT_L_LOGICAL, &[a, m]);
                Some(self.tmp(uint_ty, OP_BITWISE_OR, &[sh, c]))
            }
            "v_or3_b32" => {
                let x = self.tmp(uint_ty, OP_BITWISE_OR, &[a, b]);
                Some(self.tmp(uint_ty, OP_BITWISE_OR, &[x, c]))
            }
            "v_add3_u32" => {
                let x = self.tmp(uint_ty, OP_IADD, &[a, b]);
                Some(self.tmp(uint_ty, OP_IADD, &[x, c]))
            }
            // `o = b & 31`, `w = c & 31`; width 0 yields 0 (HLSL `bfe_u`).
            "v_bfe_u32" => {
                let o = self.tmp(uint_ty, OP_BITWISE_AND, &[b, c31]);
                let w = self.tmp(uint_ty, OP_BITWISE_AND, &[c, c31]);
                let c32 = self.uint_const(32);
                let zero = self.uint_const(0);
                let sh = self.tmp(uint_ty, OP_ISUB, &[c32, w]);
                let sh = self.tmp(uint_ty, OP_BITWISE_AND, &[sh, c31]);
                let full = self.uint_const(0xFFFF_FFFF);
                let mask = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[full, sh]);
                let v = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[a, o]);
                let v = self.tmp(uint_ty, OP_BITWISE_AND, &[v, mask]);
                let nz = self.tmp(self.bool_ty, OP_I_NOT_EQUAL, &[w, zero]);
                Some(self.tmp(uint_ty, OP_SELECT, &[nz, v, zero]))
            }
            // `(a & b) | (~a & c)` (HLSL `bfi`).
            "v_bfi_b32" => {
                let x = self.tmp(uint_ty, OP_BITWISE_AND, &[a, b]);
                let na = self.tmp(uint_ty, OP_NOT, &[a]);
                let y = self.tmp(uint_ty, OP_BITWISE_AND, &[na, c]);
                Some(self.tmp(uint_ty, OP_BITWISE_OR, &[x, y]))
            }
            "v_perm_b32" => Some(self.perm_b32(a, b, c)),
            "v_min_u32" => Some(self.umin(a, b)),
            "v_max_u32" => Some(self.umax(a, b)),
            "v_add_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
                let fr = self.tmp(float_ty, OP_FADD, &[fa, fb]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_sub_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
                let fr = self.tmp(float_ty, OP_FSUB, &[fa, fb]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_subrev_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
                let fr = self.tmp(float_ty, OP_FSUB, &[fb, fa]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_mul_f32" | "v_mul_legacy_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
                let fr = self.tmp(float_ty, OP_FMUL, &[fa, fb]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_min_f32" => Some(self.fmin(a, b)),
            "v_max_f32" => Some(self.fmax(a, b)),
            "v_fma_f32" | "v_mad_f32" | "v_fma_legacy_f32" | "v_mad_legacy_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
                let fc = self.tmp(float_ty, OP_BITCAST, &[c]);
                let m = self.tmp(float_ty, OP_FMUL, &[fa, fb]);
                let fr = self.tmp(float_ty, OP_FADD, &[m, fc]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_cvt_f32_u32" => {
                let fr = self.tmp(float_ty, OP_CONVERT_U_TO_F, &[a]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_cvt_f32_i32" => {
                let ai = self.tmp(int_ty, OP_BITCAST, &[a]);
                let fr = self.tmp(float_ty, OP_CONVERT_S_TO_F, &[ai]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_cvt_u32_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                Some(self.tmp(uint_ty, OP_CONVERT_F_TO_U, &[fa]))
            }
            "v_cvt_i32_f32" => {
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let ir = self.tmp(int_ty, OP_CONVERT_F_TO_S, &[fa]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[ir]))
            }
            // SDWA-only f16 conversions: the plain forms stay in the
            // documented f16 non-goal.
            "v_cvt_f32_f16" if sdwa.is_some() => Some(self.f16_low_to_f32_bits(a)),
            "v_cvt_f16_f32" if sdwa.is_some() => {
                let s = sdwa.as_ref().unwrap();
                let mut x = a;
                if s.src0_abs {
                    let m = self.uint_const(0x7FFF_FFFF);
                    x = self.tmp(uint_ty, OP_BITWISE_AND, &[x, m]);
                }
                if s.src0_neg {
                    let m = self.uint_const(0x8000_0000);
                    x = self.tmp(uint_ty, OP_BITWISE_XOR, &[x, m]);
                }
                let x = self.sdwa_output(s.omod, s.clamp, x);
                Some(self.f32_bits_to_f16_low(x))
            }
            "v_cvt_f32_ubyte0" | "v_cvt_f32_ubyte1" | "v_cvt_f32_ubyte2" | "v_cvt_f32_ubyte3" => {
                let k = (name.as_bytes()[name.len() - 1] - b'0') as u32;
                let x = if k == 3 {
                    let c24 = self.uint_const(24);
                    self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[a, c24])
                } else {
                    let shc = self.uint_const(8 * k);
                    let c255 = self.uint_const(0xFF);
                    let sh = self.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[a, shc]);
                    self.tmp(uint_ty, OP_BITWISE_AND, &[sh, c255])
                };
                let fr = self.tmp(float_ty, OP_CONVERT_U_TO_F, &[x]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_rcp_f32" | "v_rcp_iflag_f32" => {
                let fx = self.tmp(float_ty, OP_BITCAST, &[a]);
                let one_bits = self.uint_const(0x3F80_0000);
                let one = self.tmp(float_ty, OP_BITCAST, &[one_bits]);
                let fr = self.tmp(float_ty, OP_FDIV, &[one, fx]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            "v_rsq_f32" => Some(self.glsl_unary_f32(a, GLSL_INVERSE_SQRT)),
            "v_sqrt_f32" => Some(self.glsl_unary_f32(a, GLSL_SQRT)),
            "v_exp_f32" => Some(self.glsl_unary_f32(a, GLSL_EXP2)),
            "v_log_f32" => Some(self.glsl_unary_f32(a, GLSL_LOG2)),
            "v_trunc_f32" => Some(self.glsl_unary_f32(a, GLSL_TRUNC)),
            "v_fract_f32" => Some(self.fract_f32(a)),
            "v_med3_f32" => Some(self.med3_f32(a, b, c)),
            // RDNA sin/cos take the angle in turns (HLSL backend: x * 2pi).
            "v_sin_f32" | "v_cos_f32" => {
                let fx = self.tmp(float_ty, OP_BITCAST, &[a]);
                let tau_bits = self.uint_const(0x40C9_0FDB);
                let tau = self.tmp(float_ty, OP_BITCAST, &[tau_bits]);
                let x = self.tmp(float_ty, OP_FMUL, &[fx, tau]);
                let op = if name == "v_sin_f32" { GLSL_SIN } else { GLSL_COS };
                let xb = self.tmp(uint_ty, OP_BITCAST, &[x]);
                Some(self.glsl_unary_f32(xb, op))
            }
            "v_readfirstlane_b32" => Some(a),
            "v_fmac_f32" => {
                // VOP2/VOP3: vdst = src0 * src1 + vdst(old).
                let old = match &dst {
                    Some(Dst::V(n)) => self.load_vgpr(*n),
                    _ => self.uint_const(0),
                };
                let fa = self.tmp(float_ty, OP_BITCAST, &[a]);
                let fb = self.tmp(float_ty, OP_BITCAST, &[b]);
                let fc = self.tmp(float_ty, OP_BITCAST, &[old]);
                let m = self.tmp(float_ty, OP_FMUL, &[fa, fb]);
                let fr = self.tmp(float_ty, OP_FADD, &[m, fc]);
                Some(self.tmp(uint_ty, OP_BITCAST, &[fr]))
            }
            _ => None,
        };
        // SDWA output modifiers apply to the f32 result of the widening
        // conversions, before the destination selector.
        let res = match (sdwa.as_ref(), res) {
            (Some(s), Some(bits)) if matches!(name, "v_cvt_f32_u32" | "v_cvt_f32_f16") => {
                Some(self.sdwa_output(s.omod, s.clamp, bits))
            }
            (_, r) => r,
        };
        match (res, dst) {
            (Some(v), Some(Dst::V(n))) => {
                let v = match &sdwa {
                    Some(s) => self.sdwa_dst(s, v, n),
                    None => v,
                };
                self.store_vgpr(n, v);
            }
            (Some(v), Some(Dst::S(n))) => {
                if n == 125 {
                    // null
                } else if n > 127 {
                    self.unsupported(d, "sdst");
                } else {
                    self.store_sgpr(n, v);
                }
            }
            (Some(_), _) => {}
            (None, _) => self.unsupported(d, "valu"),
        }
    }

    fn export(&mut self, d: &Decoded) {
        let f = |n: &str| rdna2::field_value(d.enc, &d.words, n).unwrap_or(0);
        let (en, tgt, compr) = (f("EN"), f("TGT"), f("COMPR") != 0);
        let src_vars: Vec<u64> = (0..4).map(|i| f(&format!("VSRC{i}"))).collect();
        let src_vals: Vec<u32> = src_vars.iter().map(|&v| self.load_vgpr(v)).collect();
        let comp_val = |c: usize, vals: &[u32], em: &mut SpirvEm| -> u32 {
            if compr {
                let r = vals[c / 2];
                if c.is_multiple_of(2) {
                    let mask = em.uint_const(0xFFFF);
                    let uint_ty = em.uint_ty;
                    em.tmp(uint_ty, OP_BITWISE_AND, &[r, mask])
                } else {
                    let sh = em.uint_const(16);
                    let uint_ty = em.uint_ty;
                    em.tmp(uint_ty, OP_SHIFT_R_LOGICAL, &[r, sh])
                }
            } else {
                vals[c]
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
                        let raw = comp_val(c, &src_vals, self);
                        let float_ty = self.float_ty;
                        let fv = self.tmp(float_ty, OP_BITCAST, &[raw]);
                        let idx = self.int_const(c as i32);
                        let ptr_ty = self.ptr_output_float;
                        let pos = self.pos_var;
                        let ptr = self.tmp(ptr_ty, OP_ACCESS_CHAIN, &[pos, idx]);
                        self.emit_body(OP_STORE, &[ptr, fv]);
                    }
                }
            }
            13..=15 => {
                // Misc vector: clip distances come from their param; nothing to store.
            }
            32..=63 => {
                let k = (tgt - 32) as usize;
                let at = vs_param_register(&self.vs, k)
                    .map(|r| elements_at(&self.vs.outputs, true, r))
                    .unwrap_or_default();
                if at.is_empty() {
                    return;
                }
                let params: Vec<(usize, u32, usize)> = ranked(&self.vs.outputs, true)
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| at.contains(j))
                    .map(|(j, e)| (j, e.component_type, out_comps(e)))
                    .collect();
                // Resolve output var ids before emitting (borrow-safe clone).
                let out_ids: Vec<(usize, u32, usize, u32)> = params
                    .iter()
                    .map(|(j, ct, n)| {
                        let ov = self
                            .outputs
                            .iter()
                            .find(|o| o.out_index == *j)
                            .map(|o| o.id)
                            .unwrap_or(0);
                        (*j, *ct, *n, ov)
                    })
                    .collect();
                for (j, ct, n, ov) in out_ids {
                    if ov == 0 {
                        continue;
                    }
                    for c in 0..n {
                        if !enabled(c) {
                            continue;
                        }
                        let raw = comp_val(c, &src_vals, self);
                        if n == 1 {
                            let (val, val_ty_ok) = match ct {
                                1 => (raw, true),
                                2 => {
                                    let int_ty = self.int_ty;
                                    (self.tmp(int_ty, OP_BITCAST, &[raw]), true)
                                }
                                _ => {
                                    let float_ty = self.float_ty;
                                    (self.tmp(float_ty, OP_BITCAST, &[raw]), true)
                                }
                            };
                            let _ = val_ty_ok;
                            self.emit_body(OP_STORE, &[ov, val]);
                        } else {
                            let (val, ptr_ty) = match ct {
                                1 => (raw, self.ptr_output_uint),
                                2 => {
                                    let int_ty = self.int_ty;
                                    let iv = self.tmp(int_ty, OP_BITCAST, &[raw]);
                                    (iv, self.ptr_output_int)
                                }
                                _ => {
                                    let float_ty = self.float_ty;
                                    let fv = self.tmp(float_ty, OP_BITCAST, &[raw]);
                                    (fv, self.ptr_output_float)
                                }
                            };
                            let idx = self.int_const(c as i32);
                            let ptr = self.tmp(ptr_ty, OP_ACCESS_CHAIN, &[ov, idx]);
                            self.emit_body(OP_STORE, &[ptr, val]);
                        }
                    }
                    let _ = j;
                }
            }
            _ => self.unsupported(d, "export target"),
        }
    }

    fn emit_init_stores(&mut self) {
        // Clone the interface snapshot first so closures below do not hold a
        // `self` borrow across the mutable emits.
        let vs = self.vs.clone();
        // SGPRs: EXEC all-ones, the rest zero.
        let sgprs: Vec<u64> = self.sgpr_vars.keys().copied().collect();
        for code in sgprs {
            let init = self.uint_const(if code == 126 || code == 127 {
                0xFFFF_FFFF
            } else {
                0
            });
            let var = self.sgpr_var(code);
            self.emit_body(OP_STORE, &[var, init]);
        }
        if let Some(scc) = self.scc_var {
            let z = self.uint_const(0);
            self.emit_body(OP_STORE, &[scc, z]);
        }
        // VGPRs: vertex/index ids, fetch-shader attributes, else zero.
        let vgprs: Vec<u64> = self.vgpr_vars.keys().copied().collect();
        // Snapshot the input mapping (borrow-safe) before emitting loads.
        let input_map: Vec<(u32, u32, u32, u32)> = self
            .inputs
            .iter()
            .map(|i| (i.id, i.register, i.component_type, i.vec_ty))
            .collect();
        let uint_ty = self.uint_ty;
        let int_ty = self.int_ty;
        let float_ty = self.float_ty;
        let vi_var = self.vertex_index_var;
        let ii_var = self.instance_index_var;
        for v in vgprs {
            let mut done = false;
            if v == 0 {
                let li = self.load(uint_ty, vi_var);
                let var = self.vgpr_var(v);
                self.emit_body(OP_STORE, &[var, li]);
                done = true;
            } else if v == 3 {
                let li = self.load(uint_ty, ii_var);
                let var = self.vgpr_var(v);
                self.emit_body(OP_STORE, &[var, li]);
                done = true;
            } else {
                // System-value inputs aliased to this VGPR.
                let mut sv_done = false;
                for e in vs.inputs.iter() {
                    if matches!(e.system_value, 6 | 8) && vs.input_vgpr(e.register) == Some(v) {
                        let src_var = if e.system_value == 6 { vi_var } else { ii_var };
                        let src = self.load(uint_ty, src_var);
                        let var = self.vgpr_var(v);
                        self.emit_body(OP_STORE, &[var, src]);
                        sv_done = true;
                        break;
                    }
                }
                if sv_done {
                    done = true;
                }
            }
            if done {
                continue;
            }
            let mut placed = false;
            for (var_id, reg, ct, vec_ty) in &input_map {
                let base = match vs.input_vgpr(*reg) {
                    Some(b) => b,
                    None => continue,
                };
                if v >= base && v < base + 4 {
                    let chan = (v - base) as usize;
                    let loaded = self.load(*vec_ty, *var_id);
                    let comp_ty = match ct {
                        1 => uint_ty,
                        2 => int_ty,
                        _ => float_ty,
                    };
                    let comp = self.tmp(comp_ty, OP_COMPOSITE_EXTRACT, &[loaded, chan as u32]);
                    let as_uint = match ct {
                        1 => comp,
                        _ => self.tmp(uint_ty, OP_BITCAST, &[comp]),
                    };
                    let var = self.vgpr_var(v);
                    self.emit_body(OP_STORE, &[var, as_uint]);
                    placed = true;
                    break;
                }
            }
            if !placed {
                let z = self.uint_const(0);
                let var = self.vgpr_var(v);
                self.emit_body(OP_STORE, &[var, z]);
            }
        }
    }

    fn finish(
        mut self,
    ) -> (
        Vec<u32>,
        BTreeMap<String, usize>,
        BTreeMap<String, usize>,
        BTreeMap<String, usize>,
    ) {
        // Function header + label + register homes + init + body + return.
        let mut func: Vec<u32> = Vec::new();
        inst(
            &mut func,
            OP_FUNCTION,
            &[self.void_ty, self.func_id, 0, self.func_ty],
        );
        inst(&mut func, OP_LABEL, &[self.label_id]);
        let vgprs: Vec<u32> = self.vgpr_vars.values().copied().collect();
        let sgprs: Vec<u32> = self.sgpr_vars.values().copied().collect();
        let ptr = self.ptr_func_uint;
        for var in vgprs.iter().chain(sgprs.iter()) {
            inst(&mut func, OP_VARIABLE, &[ptr, *var, STORAGE_FUNCTION]);
        }
        if let Some(scc) = self.scc_var {
            inst(&mut func, OP_VARIABLE, &[ptr, scc, STORAGE_FUNCTION]);
        }
        // Init stores and translated body share `self.body`; emit init first by
        // swapping the translated body out, emitting init, then re-appending.
        let translated = std::mem::take(&mut self.body);
        self.emit_init_stores();
        self.body.extend_from_slice(&translated);
        func.extend_from_slice(&self.body);
        inst(&mut func, OP_RETURN, &[]);
        inst(&mut func, OP_FUNCTION_END, &[]);

        // Entry-point interface: every global Input/Output we declared.
        let mut entry: Vec<u32> = Vec::new();
        {
            let mut ops = vec![EXEC_MODEL_VERTEX, self.func_id];
            ops.extend(string_words("main"));
            ops.extend(self.entry_interface.iter().copied());
            entry.push(((ops.len() as u32 + 1) << 16) | OP_ENTRY_POINT as u32);
            entry.extend_from_slice(&ops);
        }
        let mut head: Vec<u32> = vec![0x0723_0203, 0x0001_0000, 0x584C_0001, 0, 0];
        let mut cap: Vec<u32> = Vec::new();
        inst(&mut cap, OP_CAPABILITY, &[CAP_SHADER]);
        if self.need_int64 {
            inst(&mut cap, OP_CAPABILITY, &[CAP_INT64]);
        }
        if self.need_float16 {
            inst(&mut cap, OP_CAPABILITY, &[CAP_FLOAT16]);
        }
        // GLSL.std.450 for the transcendental/rounding ops; always declared so
        // the import section does not depend on usage order.
        let mut imports: Vec<u32> = Vec::new();
        let mut glsl_ops = vec![self.glsl_id];
        glsl_ops.extend(string_words("GLSL.std.450"));
        inst(&mut imports, OP_EXT_INST_IMPORT, &glsl_ops);
        let mut mem: Vec<u32> = Vec::new();
        inst(&mut mem, OP_MEMORY_MODEL, &[ADDR_LOGICAL, MEM_GLSL450]);
        let bound = self.next;
        head[3] = bound;
        let mut out = head;
        out.extend_from_slice(&cap);
        out.extend_from_slice(&imports);
        out.extend_from_slice(&mem);
        out.extend_from_slice(&entry);
        out.extend_from_slice(&self.names);
        out.extend_from_slice(&self.decor);
        out.extend_from_slice(&self.types_consts);
        out.extend_from_slice(&self.global_vars);
        out.extend_from_slice(&func);
        (out, self.unsupported, self.unsupported_why, self.ignored)
    }
}

#[cfg(test)]
mod tests {
    use super::super::VsBackend;
    use super::*;
    use crate::gpu::rdna2::encode;
    use std::collections::BTreeSet;

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

    fn word_ops(words: &[u32]) -> Vec<(u16, Vec<u32>)> {
        let mut out = Vec::new();
        let mut i = 5;
        while i < words.len() {
            let w = words[i];
            let count = (w >> 16) as usize;
            let op = (w & 0xFFFF) as u16;
            assert!(count >= 1, "bad SPIR-V word count");
            out.push((op, words[i + 1..i + count].to_vec()));
            i += count;
        }
        out
    }

    #[test]
    fn synthetic_blit_vs_emits_valid_spirv() {
        let code = blit_code();
        let vs = blit_vs();
        let t = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported);
        let w = &t.words;
        assert!(w.len() > 5, "module too short");
        // Header: magic / version 1.x / bound / schema.
        assert_eq!(w[0], 0x0723_0203, "bad magic");
        assert_eq!(w[1] >> 16, 1, "major version must be 1, got {:#x}", w[1]);
        assert!(w[3] > 50, "bad bound {}", w[3]);
        assert!(
            (w[3] as usize) < w.len() + 100,
            "bound {} exceeds words {}",
            w[3],
            w.len()
        );
        assert_eq!(w[4], 0, "schema must be 0");
        let ops = word_ops(w);
        let has = |op: u16, pred: &dyn Fn(&[u32]) -> bool| {
            ops.iter().any(|(o, args)| *o == op && pred(args))
        };
        // Capability Shader + Logical GLSL450 memory model.
        assert!(
            has(OP_CAPABILITY, &|a| a == [CAP_SHADER]),
            "missing Capability Shader"
        );
        assert!(
            has(OP_MEMORY_MODEL, &|a| a == [ADDR_LOGICAL, MEM_GLSL450]),
            "missing Logical GLSL450 memory model"
        );
        // Vertex entry point naming "main".
        assert!(
            has(OP_ENTRY_POINT, &|a| !a.is_empty()
                && a[0] == EXEC_MODEL_VERTEX),
            "missing Vertex entry point"
        );
        // Position builtin + param location + vertex/index builtins.
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3
                && a[1] == DECOR_BUILT_IN
                && a[2] == BUILT_IN_POSITION),
            "missing Position builtin"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3 && a[1] == DECOR_LOCATION),
            "missing Location decoration for params"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3
                && a[1] == DECOR_BUILT_IN
                && a[2] == BUILT_IN_VERTEX_INDEX),
            "missing VertexIndex builtin"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3
                && a[1] == DECOR_BUILT_IN
                && a[2] == BUILT_IN_INSTANCE_INDEX),
            "missing InstanceIndex builtin"
        );
        // Real `spirv-val` gate where available, else the structural checks above gate.
        let val = std::process::Command::new("spirv-val")
            .arg("--version")
            .output();
        let Ok(which) = val else {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-spirv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spv = dir.join("blit.vert.spv");
        let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(&spv, &bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .arg(&spv)
            .output()
            .expect("spawn spirv-val");
        assert!(
            out.status.success(),
            "spirv-val failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    #[test]
    fn spirv_matches_glsl_position_and_param_contract() {
        use super::super::glsl::GlslVsBackend;
        use super::super::VsBackend;
        let code = blit_code();
        let vs = blit_vs();
        let g = GlslVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(g.complete(), "{:?}", g.unsupported);
        let s = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(s.complete(), "{:?}", s.unsupported);
        // Same instruction count gated on both backends.
        assert_eq!(s.instructions, g.instructions);
        // SPIR-V declares one param output (COLOR) plus Position; GLSL declares
        // one `out_p` plus `gl_Position` for the same interface.
        let ops = word_ops(&s.words);
        let locs: Vec<u32> = ops
            .iter()
            .filter(|(o, a)| *o == OP_DECORATE && a.len() == 3 && a[1] == DECOR_LOCATION)
            .map(|(_, a)| a[2])
            .collect();
        assert!(!locs.is_empty(), "no Location decorations");
        // One output location (COLOR); inputs share the numbering in Input class.
        let out_locs: BTreeSet<u32> = locs.into_iter().collect();
        assert!(out_locs.contains(&0), "param out location 0 missing");
        assert!(g.glsl.contains("gl_Position"), "{}", g.glsl);
        assert!(g.glsl.contains("out_p0"), "{}", g.glsl);
    }

    #[test]
    fn record_container_vs_uses_the_real_interface() {
        // S2: a console VS container translates with its S1 interface
        // (attr_base 4, FSSM param table) over its own program bytes/wave64 —
        // the whole blob is not raw code. spirv-val gates the words.
        use crate::gpu::xbox::tests::{fssm, sample};
        let blob = sample(1, &[fssm(&blit_code(), &[(4, 0)])]);
        let vs = super::super::record_vs_interface(&blob).expect("S1 interface");
        assert_eq!(vs.attr_base, 4);
        let t = super::record_vs_words(&blob).expect("container VS must translate");
        assert!(t.complete(), "{:?}", t.unsupported_why);
        let ops = word_ops(&t.words);
        let has = |op: u16, pred: &dyn Fn(&[u32]) -> bool| {
            ops.iter().any(|(o, a)| *o == op && pred(a))
        };
        assert!(
            has(OP_ENTRY_POINT, &|a| !a.is_empty()
                && a[0] == EXEC_MODEL_VERTEX),
            "missing Vertex entry point"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3
                && a[1] == DECOR_BUILT_IN
                && a[2] == BUILT_IN_POSITION),
            "missing Position builtin"
        );
        let val = std::process::Command::new("spirv-val")
            .arg("--version")
            .output();
        let Ok(which) = val else {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-spirv-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spv = dir.join("record.vert.spv");
        let bytes: Vec<u8> = t.words.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(&spv, &bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .arg(&spv)
            .output()
            .expect("spawn spirv-val");
        assert!(
            out.status.success(),
            "spirv-val failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    #[test]
    fn record_non_container_bytes_still_fail_closed() {
        // S2 keeps today's fallback contract: PC DXBC (the title's bytes) and
        // empty input never yield complete words — the caller stands the
        // title triangle in.
        use crate::gpu::xbox::tests::container;
        let pc = container(&[(b"ISG1", &[0u8; 8])]);
        let complete = super::record_vs_words(&pc).map(|t| t.complete()).unwrap_or(false);
        assert!(!complete, "PC DXBC must fail closed");
        assert!(super::record_vs_words(&[]).is_err(), "empty bytes must fail closed");
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
    fn scalar_compare_select_emits_select() {
        let code = scalar_select_code();
        let vs = blit_vs();
        let t = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported);
        let w = &t.words;
        let ops = word_ops(w);
        let has =
            |op: u16, pred: &dyn Fn(&[u32]) -> bool| ops.iter().any(|(o, a)| *o == op && pred(a));
        // Two compares (OpIEqual on integers) feeding two SCC selects.
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_I_EQUAL).count() >= 2,
            "missing integer compares"
        );
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_SELECT).count() >= 4,
            "missing SCC compare/select pairs"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3 && a[1] == DECOR_LOCATION),
            "missing Location decoration for params"
        );
    }

    #[test]
    fn triangle_vertexindex_select_emits_valid_spirv() {
        let code = triangle_code();
        let vs = triangle_vs();
        let t = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported);
        let w = &t.words;
        assert!(w.len() > 5, "module too short");
        assert_eq!(w[0], 0x0723_0203, "bad magic");
        assert_eq!(w[1] >> 16, 1, "major version must be 1, got {:#x}", w[1]);
        assert_eq!(w[4], 0, "schema must be 0");
        let ops = word_ops(w);
        let has =
            |op: u16, pred: &dyn Fn(&[u32]) -> bool| ops.iter().any(|(o, a)| *o == op && pred(a));
        assert!(
            has(OP_ENTRY_POINT, &|a| !a.is_empty()
                && a[0] == EXEC_MODEL_VERTEX),
            "missing Vertex entry point"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3
                && a[1] == DECOR_BUILT_IN
                && a[2] == BUILT_IN_POSITION),
            "missing Position builtin"
        );
        assert!(
            has(OP_DECORATE, &|a| a.len() == 3
                && a[1] == DECOR_BUILT_IN
                && a[2] == BUILT_IN_VERTEX_INDEX),
            "missing VertexIndex builtin"
        );
        // Two VertexIndex compares + three corner selects (VCC-true picks VSRC1).
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_I_EQUAL).count() >= 2,
            "missing VertexIndex compares"
        );
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_SELECT).count() >= 3,
            "missing corner selects"
        );
        // Real `spirv-val` gate where available, else the structural checks gate.
        let val = std::process::Command::new("spirv-val")
            .arg("--version")
            .output();
        let Ok(which) = val else {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-spirv-tri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spv = dir.join("tri.vert.spv");
        let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(&spv, &bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .arg(&spv)
            .output()
            .expect("spawn spirv-val");
        assert!(
            out.status.success(),
            "spirv-val failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    #[test]
    fn triangle_matches_glsl_position_and_param_contract() {
        use super::super::glsl::GlslVsBackend;
        use super::super::VsBackend;
        let code = triangle_code();
        let vs = triangle_vs();
        let g = GlslVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(g.complete(), "{:?}", g.unsupported);
        let s = SpirvVsBackend::translate_vs(&code, &vs, true).unwrap();
        assert!(s.complete(), "{:?}", s.unsupported);
        // Same instruction count gated on both backends.
        assert_eq!(s.instructions, g.instructions);
        // SPIR-V declares one param output (COLOR) plus Position; GLSL declares
        // one `out_p` plus `gl_Position` for the same interface.
        let ops = word_ops(&s.words);
        let locs: Vec<u32> = ops
            .iter()
            .filter(|(o, a)| *o == OP_DECORATE && a.len() == 3 && a[1] == DECOR_LOCATION)
            .map(|(_, a)| a[2])
            .collect();
        assert!(!locs.is_empty(), "no Location decorations");
        let out_locs: BTreeSet<u32> = locs.into_iter().collect();
        assert!(out_locs.contains(&0), "param out location 0 missing");
        assert!(g.glsl.contains("gl_Position"), "{}", g.glsl);
        assert!(g.glsl.contains("gl_VertexIndex"), "{}", g.glsl);
        assert!(g.glsl.contains("out_p0"), "{}", g.glsl);
    }

    fn decoded(code: &[u8]) -> Vec<Decoded> {
        let mut insts = Vec::new();
        let mut off = 0usize;
        while off + 4 <= code.len() {
            let d = rdna2::decode(&code[off..], off as u64).unwrap();
            off += d.len;
            insts.push(d);
        }
        rdna2::resolve_far_jumps(&mut insts);
        insts
    }

    /// The branch refusal carries the shape a structured translation would have to
    /// model, so `xcl shaders --record-vs` ranks the shapes separately.
    #[test]
    fn branch_flow_classifies_lane_predicate_and_loop_edges() {
        let branch = |name: &str, simm: u16| {
            encode(name, "ENC_SOPP", &[("SIMM16", simm as u64)], None).unwrap()
        };
        let mut code = Vec::new();
        code.extend(branch("S_CBRANCH_SCC0", 2)); // forward, predicate
        code.extend(branch("S_CBRANCH_EXECZ", 0xFFFF)); // target = own pc: lane loop
        code.extend(branch("S_BRANCH", 0xFFFF)); // unconditional self loop
        let flow = branch_flow(&decoded(&code));
        assert_eq!(
            flow,
            BranchFlow {
                lane: 1,
                predicate: 1,
                unconditional: 1,
                loops: 2,
            }
        );
        assert_eq!(flow.branches(), 3);
        assert!(!flow.is_empty());
        assert_eq!(
            flow.shape(),
            "exec-mask branches, scc/vcc branches, backward edges"
        );
    }

    #[test]
    fn branch_flow_is_empty_for_straight_line_code() {
        let flow = branch_flow(&decoded(&blit_code()));
        assert!(flow.is_empty());
        assert_eq!(flow.branches(), 0);
        assert_eq!(flow.shape(), "");
    }

    /// A resolved far jump (`s_getpc_b64` … `s_setpc_b64` with a 64-bit
    /// offset) is an unconditional branch even when its target is outside the
    /// `s_branch` ±128 KB reach, and the refusal names that shape instead of
    /// rendering `()`.
    #[test]
    fn far_jump_is_a_named_unconditional_branch_shape() {
        let mut code = Vec::new();
        for (name, enc, fields, literal) in [
            ("S_GETPC_B64", "ENC_SOP1", vec![("SDST", 0)], None),
            (
                "S_ADD_U32",
                "ENC_SOP2",
                vec![("SDST", 0), ("SSRC0", 0), ("SSRC1", 255)],
                Some(0x0004_0000u32),
            ),
            (
                "S_ADDC_U32",
                "ENC_SOP2",
                vec![("SDST", 1), ("SSRC0", 1), ("SSRC1", 128)],
                None,
            ),
            ("S_SETPC_B64", "ENC_SOP1", vec![("SSRC0", 0)], None),
        ] {
            code.extend(encode(name, enc, &fields, literal).unwrap());
        }
        code.extend(blit_code());
        let insts = decoded(&code);
        assert_eq!(
            rdna2::branch_target(&insts[3]),
            Some(0x0004_0000 + 4),
            "far target = getpc end + 0x40000"
        );
        let flow = branch_flow(&insts);
        assert_eq!(
            flow,
            BranchFlow {
                unconditional: 1,
                ..BranchFlow::default()
            }
        );
        assert_eq!(flow.shape(), "unconditional branches");
        let err = SpirvVsBackend::translate_vs(&code, &blit_vs(), true)
            .expect_err("a far jump must be rejected");
        assert!(
            err.contains("(unconditional branches)"),
            "refusal must name the shape, not (): {err}"
        );
    }

    #[test]
    fn translated_blit_rejection_names_the_branch_shape() {
        let mut code = Vec::new();
        code.extend(
            encode(
                "S_CBRANCH_EXECZ",
                "ENC_SOPP",
                &[("SIMM16", 2)],
                None,
            )
            .unwrap(),
        );
        code.extend(blit_code());
        let err = SpirvVsBackend::translate_vs(&code, &blit_vs(), true)
            .expect_err("a branch must be rejected");
        assert!(err.contains("branches are not supported"), "{err}");
        assert!(err.contains("exec-mask branches"), "{err}");
        assert!(err.contains("+0x0:"), "{err}");
    }

    #[test]
    fn shipped_path_spawns_no_compiler() {
        // The shipped `translate_vs_spirv` must never spawn a process: no
        // `Command` / `process` usage outside tests (tool names may appear in
        // docs/comments, so only gate the spawn mechanism itself).
        let src = include_str!("spirv.rs");
        let shipped = src.split("#[cfg(test)]").next().unwrap_or(src);
        for needle in ["Command::new", "std::process"] {
            assert!(
                !shipped.contains(needle),
                "shipped SPIR-V path mentions {needle}"
            );
        }
    }

    // ---- M2.2 step-09: pure-register VALU coverage -------------------------

    /// Translate a synthetic program with the blit interface and require every
    /// instruction to translate.
    fn translated(code: &[u8]) -> SpirvTranslation {
        let t = SpirvVsBackend::translate_vs(code, &blit_vs(), true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported_why);
        t
    }

    /// Position export + `s_endpgm`, so the module is a well-formed VS.
    fn end_program() -> Vec<u8> {
        let mut code = encode(
            "EXP",
            "ENC_EXP",
            &[
                ("TGT", 12),
                ("EN", 1),
                ("VSRC0", 0),
                ("VSRC1", 1),
                ("VSRC2", 2),
                ("VSRC3", 3),
                ("DONE", 1),
            ],
            None,
        )
        .unwrap();
        code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
        code
    }

    /// The same optional `spirv-val` gate the other module tests use.
    fn assert_valid_spirv(t: &SpirvTranslation, label: &str) {
        let val = std::process::Command::new("spirv-val")
            .arg("--version")
            .output();
        let Ok(which) = val else {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        };
        if !which.status.success() {
            println!("spirv-val not available – skipping, structural checks gate");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-spirv-valu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spv = dir.join(format!("{label}.vert.spv"));
        let bytes: Vec<u8> = t.words.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(&spv, &bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .arg(&spv)
            .output()
            .expect("spawn spirv-val");
        assert!(
            out.status.success(),
            "spirv-val failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// `v_fmac_f32` reads the destination register old value as the addend (it
    /// is not a three-source VOP3 instruction): the OpFAdd's second operand must
    /// be an OpLoad from the destination variable, never a constant.
    #[test]
    fn valu_fmac_f32_reads_the_destination_as_addend() {
        let mut code = encode(
            "V_MOV_B32",
            "ENC_VOP1",
            &[("VDST", 2), ("SRC0", 242)],
            None,
        )
        .unwrap();
        code.extend(
            encode(
                "V_FMAC_F32",
                "ENC_VOP2",
                &[("VDST", 2), ("SRC0", 256 + 1), ("VSRC1", 256 + 2)],
                None,
            )
            .unwrap(),
        );
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        let (add_idx, addend) = ops
            .iter()
            .enumerate()
            .find_map(|(i, (o, a))| (*o == OP_FADD && a.len() == 4).then(|| (i, a[3])))
            .expect("no OpFAdd");
        // The register model stores uint bits: the addend is a bitcast of the
        // old destination load, not the load instruction itself.
        let mut cur = addend;
        let ptr = loop {
            let (o, a) = ops
                .iter()
                .find(|(_, a)| a.len() >= 2 && a[1] == cur)
                .expect("addend has no defining instruction");
            match *o {
                OP_BITCAST => cur = a[2],
                OP_LOAD => break a[2],
                other => panic!("addend is not a destination load (opcode {other})"),
            }
        };
        assert!(
            ops.iter()
                .any(|(o, a)| *o == OP_VARIABLE && a.len() == 3 && a[1] == ptr && a[2] == STORAGE_FUNCTION),
            "addend load is not a function-scope register"
        );
        assert!(
            ops[..add_idx]
                .iter()
                .any(|(o, a)| *o == OP_STORE && a.first() == Some(&ptr)),
            "destination old value was not written before the fmac"
        );
        assert_valid_spirv(&t, "fmac");
    }

    /// Unary float and byte-conversion families: rcp/rsq/sqrt/exp2/log2/sin/cos,
    /// trunc, fract (x - floor(x) with the RDNA >=1 clamp), med3, ubyte0..3.
    #[test]
    fn valu_unary_float_and_ubyte_family_translates() {
        let mut code = Vec::new();
        for (dst, name) in [
            (5u64, "V_RCP_F32"),
            (6, "V_RCP_IFLAG_F32"),
            (7, "V_RSQ_F32"),
            (8, "V_SQRT_F32"),
            (9, "V_EXP_F32"),
            (10, "V_LOG_F32"),
            (11, "V_SIN_F32"),
            (12, "V_COS_F32"),
            (13, "V_TRUNC_F32"),
            (14, "V_FRACT_F32"),
            (16, "V_CVT_F32_UBYTE0"),
            (17, "V_CVT_F32_UBYTE1"),
            (18, "V_CVT_F32_UBYTE2"),
            (19, "V_CVT_F32_UBYTE3"),
        ] {
            code.extend(
                encode(name, "ENC_VOP1", &[("VDST", dst), ("SRC0", 256 + 1)], None).unwrap(),
            );
        }
        code.extend(
            encode(
                "V_MED3_F32",
                "ENC_VOP3",
                &[
                    ("VDST", 15),
                    ("SRC0", 256 + 1),
                    ("SRC1", 256 + 2),
                    ("SRC2", 256 + 3),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        let glsl: BTreeSet<u32> = ops
            .iter()
            .filter(|(o, a)| *o == OP_EXT_INST && a.len() >= 4)
            .map(|(_, a)| a[3])
            .collect();
        for (name, n) in [
            ("Sin", GLSL_SIN),
            ("Cos", GLSL_COS),
            ("Exp2", GLSL_EXP2),
            ("Log2", GLSL_LOG2),
            ("Sqrt", GLSL_SQRT),
            ("InverseSqrt", GLSL_INVERSE_SQRT),
            ("Trunc", GLSL_TRUNC),
            ("Floor", GLSL_FLOOR),
        ] {
            assert!(glsl.contains(&n), "missing GLSL.std.450 {name}");
        }
        assert!(
            ops.iter().any(|(o, _)| *o == OP_FDIV),
            "v_rcp_f32 must divide 1.0 by the source"
        );
        assert!(
            ops.iter().any(|(o, _)| *o == OP_CONVERT_U_TO_F),
            "v_cvt_f32_ubyte* must convert the extracted byte"
        );
        assert_valid_spirv(&t, "unary-float");
    }

    /// Integer VALU families: lshl_add/lshl_or/or3/add3, bfe, bfi, perm, mul_hi.
    #[test]
    fn valu_integer_family_translates() {
        let tri = |code: &mut Vec<u8>, name: &str, dst: u64| {
            code.extend(
                encode(
                    name,
                    "ENC_VOP3",
                    &[
                        ("VDST", dst),
                        ("SRC0", 256 + 1),
                        ("SRC1", 256 + 2),
                        ("SRC2", 256 + 3),
                    ],
                    None,
                )
                .unwrap(),
            );
        };
        let mut code = Vec::new();
        for (dst, name) in [
            (5u64, "V_LSHL_ADD_U32"),
            (6, "V_LSHL_OR_B32"),
            (7, "V_OR3_B32"),
            (8, "V_ADD3_U32"),
            (9, "V_BFE_U32"),
            (10, "V_BFI_B32"),
            (11, "V_PERM_B32"),
        ] {
            tri(&mut code, name, dst);
        }
        code.extend(
            encode(
                "V_MUL_HI_U32",
                "ENC_VOP3",
                &[("VDST", 12), ("SRC0", 256 + 1), ("SRC1", 256 + 2)],
                None,
            )
            .unwrap(),
        );
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter().any(|(o, a)| *o == OP_CAPABILITY && a.as_slice() == [CAP_INT64]),
            "v_mul_hi_u32 must declare Capability Int64"
        );
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_U_CONVERT).count() >= 3,
            "v_mul_hi_u32 must widen both sources and narrow the result"
        );
        assert!(
            ops.iter().any(|(o, _)| *o == OP_IMUL),
            "v_mul_hi_u32 must multiply the widened sources"
        );
        assert!(
            ops.iter().any(|(o, _)| *o == OP_SHIFT_L_LOGICAL),
            "v_lshl_* must shift left"
        );
        assert_valid_spirv(&t, "integer");
    }

    /// `v_readfirstlane_b32` writes an SGPR: every VGPR is a scalar in this
    /// model, so the value is a copy.
    #[test]
    fn valu_readfirstlane_copies_to_sgpr() {
        let mut code =
            encode("V_READFIRSTLANE_B32", "ENC_VOP1", &[("VDST", 0), ("SRC0", 256 + 2)], None)
                .unwrap();
        code.extend(end_program());
        let t = translated(&code);
        // The copy, the position export and `s_endpgm`; nothing unsupported.
        assert_eq!(t.instructions, 3, "{:?}", t.unsupported_why);
        assert_valid_spirv(&t, "readfirstlane");
    }

    /// The VOPC/VOP3 compare family (u32/i32/f32, ordered and unordered forms)
    /// and the VOP3 `v_cndmask_b32` with its explicit mask source.
    #[test]
    fn valu_compare_and_select_family_translates() {
        let vopc = |name: &str, a: u64| {
            encode(
                name,
                "ENC_VOPC",
                &[("SRC0", a), ("VSRC1", 256 + 1)],
                None,
            )
            .unwrap()
        };
        let mut code = Vec::new();
        for name in [
            "V_CMP_LT_U32",
            "V_CMP_LE_U32",
            "V_CMP_GT_U32",
            "V_CMP_GE_U32",
            "V_CMP_EQ_U32",
            "V_CMP_NE_U32",
            "V_CMP_LT_I32",
            "V_CMP_EQ_F32",
            "V_CMP_LE_F32",
            "V_CMP_NGT_F32",
            "V_CMP_LG_F32",
        ] {
            code.extend(vopc(name, 256 + 2));
        }
        // VOP3 forms write an explicit SGPR pair (VCC here).
        code.extend(
            encode(
                "V_CMP_EQ_U32",
                "ENC_VOP3",
                &[("VDST", 106), ("SRC0", 256 + 2), ("SRC1", 256 + 3)],
                None,
            )
            .unwrap(),
        );
        // class(v1) against +0 and +inf: mask 0x240 as a literal source.
        code.extend(
            encode(
                "V_CMP_CLASS_F32",
                "ENC_VOP3",
                &[("VDST", 106), ("SRC0", 256 + 1), ("SRC1", 255)],
                Some(0x240),
            )
            .unwrap(),
        );
        code.extend(
            encode(
                "V_CNDMASK_B32",
                "ENC_VOP3",
                &[
                    ("VDST", 5),
                    ("SRC0", 256 + 1),
                    ("SRC1", 256 + 2),
                    ("SRC2", 106),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        for (label, op) in [
            ("u32 less-than", OP_U_LESS_THAN),
            ("u32 less-or-equal", OP_U_LESS_THAN_EQUAL),
            ("u32 greater", OP_U_GREATER_THAN),
            ("u32 greater-or-equal", OP_U_GREATER_THAN_EQUAL),
            ("i32 less-than", OP_S_LESS_THAN),
            ("f32 ordered equal", OP_FORD_EQUAL),
            ("f32 ordered less-or-equal", OP_FORD_LESS_THAN_EQUAL),
            ("f32 unordered not-greater", OP_FUNORD_LESS_THAN_EQUAL),
            ("f32 unordered not-equal", OP_FUNORD_NOT_EQUAL),
        ] {
            assert!(
                ops.iter().any(|(o, _)| *o == op),
                "missing {label} compare"
            );
        }
        assert!(
            ops.iter().any(|(o, _)| *o == OP_SHIFT_R_LOGICAL),
            "v_cmp_class_f32 must test the class bit"
        );
        assert!(
            ops.iter().any(|(o, _)| *o == OP_SELECT),
            "v_cndmask_b32 must select between the sources"
        );
        assert_valid_spirv(&t, "compare-select");
    }

    #[test]
    fn retail_blit_vs_spirv_opt_in() {
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
        let t = SpirvVsBackend::translate_vs(code, &vs, true).unwrap();
        assert!(t.complete(), "{:?}", t.unsupported_why);
        let val = std::process::Command::new("spirv-val")
            .arg("--version")
            .output();
        let Ok(which) = val else {
            println!("spirv-val not available – skipping");
            return;
        };
        if !which.status.success() {
            println!("spirv-val not available – skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-spirv-retail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spv = dir.join("retail.vert.spv");
        let bytes: Vec<u8> = t.words.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(&spv, &bytes).unwrap();
        let out = std::process::Command::new("spirv-val")
            .arg(&spv)
            .output()
            .expect("spawn spirv-val");
        assert!(
            out.status.success(),
            "spirv-val failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // ---- M2.2 step-10: pure-scalar SALU/SOPC coverage ----------------------

    fn salu(name: &str, enc: &str, fields: &[(&str, u64)], literal: Option<u32>) -> Vec<u8> {
        encode(name, enc, fields, literal).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// Multiplying scalars: `s_mul_i32` low product, `s_mul_hi_u32` widened high
    /// product (`Capability Int64`), `s_mulk_i32` immediate multiply.
    #[test]
    fn salu_mul_family_translates() {
        let mut code = Vec::new();
        code.extend(salu(
            "S_MUL_I32",
            "ENC_SOP2",
            &[("SDST", 0), ("SSRC0", 128), ("SSRC1", 129)],
            None,
        ));
        code.extend(salu(
            "S_MUL_HI_U32",
            "ENC_SOP2",
            &[("SDST", 1), ("SSRC0", 128), ("SSRC1", 129)],
            None,
        ));
        code.extend(salu(
            "S_MULK_I32",
            "ENC_SOPK",
            &[("SDST", 2), ("SIMM16", 7)],
            None,
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_IMUL).count() >= 3,
            "mul_i32, the widened mul_hi product and mulk must multiply"
        );
        assert!(
            ops.iter()
                .any(|(o, a)| *o == OP_CAPABILITY && a.as_slice() == [CAP_INT64]),
            "s_mul_hi_u32 must declare Capability Int64"
        );
        assert!(
            ops.iter().any(|(o, _)| *o == OP_U_CONVERT),
            "s_mul_hi_u32 must widen/narrow through OpUConvert"
        );
        assert_valid_spirv(&t, "salu_mul");
    }

    /// `s_lshl_b32`/`s_lshr_b32` mask the shift to 5 bits; `s_bfe_u32`/
    /// `s_bfe_i32` decode offset/width from the packed control word and sign
    /// extend for the signed form.
    #[test]
    fn salu_shift_and_bfe_family_translates() {
        let mut code = Vec::new();
        code.extend(salu(
            "S_LSHL_B32",
            "ENC_SOP2",
            &[("SDST", 0), ("SSRC0", 128), ("SSRC1", 129)],
            None,
        ));
        code.extend(salu(
            "S_LSHR_B32",
            "ENC_SOP2",
            &[("SDST", 1), ("SSRC0", 128), ("SSRC1", 129)],
            None,
        ));
        let ctl = Some(0x0008_0004); // offset 4, width 8
        code.extend(salu(
            "S_BFE_U32",
            "ENC_SOP2",
            &[("SDST", 2), ("SSRC0", 128), ("SSRC1", 255)],
            ctl,
        ));
        code.extend(salu(
            "S_BFE_I32",
            "ENC_SOP2",
            &[("SDST", 3), ("SSRC0", 128), ("SSRC1", 255)],
            ctl,
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        for (op, what) in [
            (OP_SHIFT_L_LOGICAL, "s_lshl_b32"),
            (OP_SHIFT_R_LOGICAL, "s_lshr_b32"),
            (OP_SHIFT_R_ARITH, "s_bfe_i32 sign extension"),
        ] {
            assert!(ops.iter().any(|(o, _)| *o == op), "{what} missing op {op}");
        }
        assert!(
            ops.iter()
                .any(|(o, a)| *o == OP_CONSTANT && a.get(2) == Some(&0x7F)),
            "s_bfe must mask the width with the 7-bit field"
        );
        assert_valid_spirv(&t, "salu_shift_bfe");
    }

    /// `s_addc_u32` takes SCC as carry-in and leaves the carry-out; `s_add_u32`
    /// is the carry producer and `s_addk_i32` reports signed overflow. The
    /// test traces the addc operand back to an SCC load whose previous store is
    /// a select (the materialised SCC bit).
    #[test]
    fn salu_addk_addc_read_and_write_scc() {
        let mut code = Vec::new();
        code.extend(salu(
            "S_MOV_B32",
            "ENC_SOP1",
            &[("SDST", 0), ("SSRC0", 255)],
            Some(0xFFFF_FFFF),
        ));
        code.extend(salu(
            "S_MOV_B32",
            "ENC_SOP1",
            &[("SDST", 1), ("SSRC0", 129)],
            None,
        ));
        // 0xFFFF_FFFF + 1 carries out, 1 + 1 + carry = 3, addk uses the
        // destination old value and reports signed overflow.
        code.extend(salu(
            "S_ADD_U32",
            "ENC_SOP2",
            &[("SDST", 2), ("SSRC0", 0), ("SSRC1", 1)],
            None,
        ));
        code.extend(salu(
            "S_ADDC_U32",
            "ENC_SOP2",
            &[("SDST", 3), ("SSRC0", 1), ("SSRC1", 1)],
            None,
        ));
        code.extend(salu(
            "S_ADDK_I32",
            "ENC_SOPK",
            &[("SDST", 4), ("SIMM16", 5)],
            None,
        ));
        code.extend(salu(
            "S_CSELECT_B32",
            "ENC_SOP2",
            &[("SDST", 5), ("SSRC0", 0), ("SSRC1", 1)],
            None,
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_IADD).count() >= 4,
            "add, both addc additions and addk each emit OpIAdd"
        );
        // Carry-out of s_add_u32 and the two addc carries.
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_U_LESS_THAN).count() >= 3,
            "unsigned carry/borrow compares missing"
        );
        // Signed overflow of s_addk_i32.
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_BITWISE_XOR).count() >= 2
                && ops.iter().any(|(o, _)| *o == OP_SHIFT_R_ARITH),
            "s_addk_i32 overflow path missing"
        );
        // addc must consume a load from an SCC home whose previous store is a
        // materialised boolean.
        let loads: Vec<(usize, u32, u32)> = ops
            .iter()
            .enumerate()
            .filter_map(|(i, (o, a))| (*o == OP_LOAD && a.len() >= 3).then(|| (i, a[1], a[2])))
            .collect();
        let stores: Vec<(usize, u32, u32)> = ops
            .iter()
            .enumerate()
            .filter_map(|(i, (o, a))| (*o == OP_STORE && a.len() >= 2).then(|| (i, a[0], a[1])))
            .collect();
        let mut carry_in = 0usize;
        for &(li, load_id, ptr) in &loads {
            let used_by_add = ops[li + 1..].iter().any(|(o, a)| {
                *o == OP_IADD && a.len() == 4 && (a[2] == load_id || a[3] == load_id)
            });
            if !used_by_add {
                continue;
            }
            let Some(&(si, _, val)) = stores.iter().rev().find(|(si, p, _)| *si < li && *p == ptr)
            else {
                continue;
            };
            if ops[..si]
                .iter()
                .rev()
                .any(|(o, a)| *o == OP_SELECT && a.len() >= 2 && a[1] == val)
            {
                carry_in += 1;
            }
        }
        assert!(carry_in > 0, "s_addc_u32 did not load SCC as the carry-in");
        assert_valid_spirv(&t, "salu_addc");
    }

    /// `s_bcnt1_i32_b32` is OpBitCount and `s_ff1_i32_b32` is GLSL FindILsb.
    #[test]
    fn salu_bit_scan_family_translates() {
        let mut code = Vec::new();
        code.extend(salu(
            "S_BCNT1_I32_B32",
            "ENC_SOP1",
            &[("SDST", 0), ("SSRC0", 1)],
            None,
        ));
        code.extend(salu(
            "S_FF1_I32_B32",
            "ENC_SOP1",
            &[("SDST", 2), ("SSRC0", 3)],
            None,
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter().any(|(o, _)| *o == OP_BIT_COUNT),
            "s_bcnt1_i32_b32 must emit OpBitCount"
        );
        assert!(
            ops.iter()
                .any(|(o, a)| *o == OP_EXT_INST && a.get(3) == Some(&GLSL_FIND_I_LSB)),
            "s_ff1_i32_b32 must emit GLSL FindILsb"
        );
        assert_valid_spirv(&t, "salu_bit_scan");
    }

    /// The entire 32-bit SOPC compare family writes SCC (`0`/`1`), signed
    /// relations through signed compare opcodes.
    #[test]
    fn salu_sopc_compare_family_sets_scc() {
        let mut code = Vec::new();
        for name in [
            "S_CMP_EQ_I32",
            "S_CMP_LG_I32",
            "S_CMP_GT_I32",
            "S_CMP_GE_I32",
            "S_CMP_LT_I32",
            "S_CMP_LE_I32",
            "S_CMP_EQ_U32",
            "S_CMP_LG_U32",
            "S_CMP_GT_U32",
            "S_CMP_GE_U32",
            "S_CMP_LT_U32",
            "S_CMP_LE_U32",
        ] {
            code.extend(salu(
                name,
                "ENC_SOPC",
                &[("SSRC0", 128), ("SSRC1", 129)],
                None,
            ));
        }
        code.extend(salu(
            "S_CSELECT_B32",
            "ENC_SOP2",
            &[("SDST", 0), ("SSRC0", 128), ("SSRC1", 129)],
            None,
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        for (op, what) in [
            (OP_I_EQUAL, "eq_i32"),
            (OP_I_NOT_EQUAL, "lg_i32"),
            (OP_S_GREATER_THAN, "gt_i32"),
            (OP_S_GREATER_THAN_EQUAL, "ge_i32"),
            (OP_S_LESS_THAN, "lt_i32"),
            (OP_S_LESS_THAN_EQUAL, "le_i32"),
            (OP_U_GREATER_THAN, "gt_u32"),
            (OP_U_GREATER_THAN_EQUAL, "ge_u32"),
            (OP_U_LESS_THAN, "lt_u32 (and eq_u32's select)"),
            (OP_U_LESS_THAN_EQUAL, "le_u32"),
        ] {
            assert!(
                ops.iter().any(|(o, _)| *o == op),
                "SOPC {what} missing op {op}"
            );
        }
        assert_valid_spirv(&t, "salu_sopc");
    }

    // ---- M2.2 step-12: SDWA operand forms ---------------------------------

    /// The definition of a value id (an instruction's result is its second
    /// operand; stores carry no result).
    fn def<'a>(ops: &'a [(u16, Vec<u32>)], id: u32) -> Option<(u16, &'a [u32])> {
        ops.iter()
            .find_map(|(o, a)| (a.len() >= 2 && a[1] == id).then(|| (*o, a.as_slice())))
    }

    /// Like [`def`], but looks through any f32 bit-pattern bitcasts.
    fn def_unbits<'a>(ops: &'a [(u16, Vec<u32>)], id: u32) -> Option<(u16, &'a [u32])> {
        let mut id = id;
        loop {
            match def(ops, id) {
                Some((OP_BITCAST, a)) => id = a[2],
                other => return other,
            }
        }
    }

    /// An f32 bit pattern: the literal of `id`, or of the constant bitcast
    /// that defines it.
    fn const_bits(ops: &[(u16, Vec<u32>)], id: u32) -> Option<u32> {
        match def(ops, id) {
            Some((OP_BITCAST, a)) => const_value(ops, a[2]),
            _ => const_value(ops, id),
        }
    }

    /// The literal of the OpConstant that defines `id`.
    fn const_value(ops: &[(u16, Vec<u32>)], id: u32) -> Option<u32> {
        ops.iter()
            .find_map(|(o, a)| (*o == OP_CONSTANT && a.len() >= 3 && a[1] == id).then(|| a[2]))
    }

    fn sdwa(name: &str, vop2: bool, fields: &[(&str, u64)]) -> Vec<u8> {
        let enc = if vop2 {
            "VOP2_VOP_SDWA"
        } else {
            "VOP1_VOP_SDWA"
        };
        // SRC0 = 0xF9 is the instruction-word marker that selects the SDWA
        // extension dword.
        let mut all = vec![("SRC0", 0xF9)];
        all.extend_from_slice(fields);
        encode(name, enc, &all, None).unwrap()
    }

    /// Source selection reads the chosen byte/word out of the extension dword
    /// operand, zero-extends unless SEXT, and an `S` bit selects an SGPR /
    /// inline constant instead of a VGPR.
    #[test]
    fn sdwa_source_select_sext_and_scalar_sources() {
        // v_or_b32_sdwa v5, v1, v2: src0 BYTE_2 sign-extended, src1 BYTE_3.
        let mut code = sdwa(
            "V_OR_B32",
            true,
            &[
                ("VDST", 5),
                ("VSRC0", 1),
                ("VSRC1", 2),
                ("SRC0_SEL", 2),
                ("SRC0_SEXT", 1),
                ("SRC1_SEL", 3),
                ("DST_SEL", 6),
            ],
        );
        // v_add_nc_u32_sdwa v6, 1.0, v2: src0 is the inline constant 1.0
        // (S0 = 1, VSRC0 = 242), read without a sub-dword select.
        code.extend(sdwa(
            "V_ADD_NC_U32",
            true,
            &[
                ("VDST", 6),
                ("S0", 1),
                ("VSRC0", 242),
                ("VSRC1", 2),
                ("SRC0_SEL", 6),
                ("SRC1_SEL", 6),
                ("DST_SEL", 6),
            ],
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        for (value, what) in [(16, "BYTE_2 read"), (24, "BYTE_3 read")] {
            assert!(
                ops.iter().any(|(o, a)| *o == OP_SHIFT_R_LOGICAL
                    && a.len() == 4
                    && const_value(&ops, a[3]) == Some(value)),
                "{what}: no shift by {value}"
            );
        }
        assert!(
            ops.iter().any(|(o, a)| *o == OP_SHIFT_R_ARITH
                && a.len() == 4
                && const_value(&ops, a[3]) == Some(24)),
            "sext(BYTE_2) must shift back arithmetically by 24"
        );
        assert!(
            ops.iter().any(|(o, a)| *o == OP_BITWISE_AND
                && a.len() == 4
                && const_value(&ops, a[3]) == Some(0xFF)),
            "byte selection must mask the selected byte"
        );
        // The inline constant must enter the add as 1.0's bit pattern, not as
        // a VGPR load.
        let one = ops
            .iter()
            .find_map(|(o, a)| {
                (*o == OP_CONSTANT && a.len() >= 3 && a[2] == 0x3F80_0000).then(|| a[1])
            })
            .expect("1.0 inline constant missing");
        assert!(
            ops.iter()
                .any(|(o, a)| *o == OP_IADD && a.len() == 4 && (a[2] == one || a[3] == one)),
            "S0 source is not read as the scalar constant"
        );
        assert_valid_spirv(&t, "sdwa_select");
    }

    /// `v_cvt_f16_f32_sdwa` applies source abs/neg, output modifiers and clamp
    /// in f32 before the conversion, and PRESERVE keeps the destination half
    /// it does not write. `v_cvt_f32_f16_sdwa` reads the selected half.
    #[test]
    fn sdwa_f16_conversions_and_destination_preserve() {
        // v_cvt_f16_f32_sdwa v1, v9 dst_sel:WORD_1 dst_unused:PRESERVE
        //   -|v9| src0_sel:DWORD
        let mut code = sdwa(
            "V_CVT_F16_F32",
            false,
            &[
                ("VDST", 1),
                ("VSRC0", 9),
                ("SRC0_ABS", 1),
                ("SRC0_NEG", 1),
                ("SRC0_SEL", 6),
                ("DST_SEL", 5),
                ("DST_UNUSED", 2),
            ],
        );
        // v_cvt_f32_f16_sdwa v2, v3 dst_sel:DWORD src0_sel:WORD_1
        code.extend(sdwa(
            "V_CVT_F32_F16",
            false,
            &[("VDST", 2), ("VSRC0", 3), ("SRC0_SEL", 5), ("DST_SEL", 6)],
        ));
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter()
                .any(|(o, a)| *o == OP_CAPABILITY && a.as_slice() == [CAP_FLOAT16]),
            "f16 conversion must declare Capability Float16"
        );
        assert!(
            ops.iter().filter(|(o, _)| *o == OP_F_CONVERT).count() >= 2,
            "both conversion directions must emit OpFConvert"
        );
        assert!(
            ops.iter().any(|(o, a)| *o == OP_BITWISE_AND
                && a.len() == 4
                && const_value(&ops, a[3]) == Some(0x7FFF_FFFF)),
            "source abs missing"
        );
        assert!(
            ops.iter().any(|(o, a)| *o == OP_BITWISE_XOR
                && a.len() == 4
                && const_value(&ops, a[3]) == Some(0x8000_0000)),
            "source neg missing"
        );
        // PRESERVE: the stored value is (old & 0xFFFF0000) | (half << 16), and
        // the old value is a load of the destination variable.
        let (ptr, val) = ops
            .iter()
            .find_map(|(o, a)| {
                (*o == OP_STORE && def(&ops, a[1]).is_some_and(|(d, _)| d == OP_BITWISE_OR))
                    .then(|| (a[0], a[1]))
            })
            .expect("no preserving store");
        let (_, or_ops) = def(&ops, val).unwrap();
        let mut kept = false;
        let mut wrote_hi = false;
        for &x in &or_ops[2..] {
            let Some((OP_BITWISE_AND, a)) = def(&ops, x) else {
                continue;
            };
            match const_value(&ops, a[3]) {
                // PRESERVE keeps the halves outside WORD_1.
                Some(0x0000_FFFF) => {
                    let Some((OP_LOAD, l)) = def(&ops, a[2]) else {
                        panic!("preserved half is not a load")
                    };
                    kept = l[2] == ptr;
                }
                // The written half is the converted f16 shifted into WORD_1.
                Some(0xFFFF_0000) => {
                    let Some((OP_SHIFT_L_LOGICAL, sh)) = def(&ops, a[2]) else {
                        panic!("half write is not shifted")
                    };
                    assert_eq!(const_value(&ops, sh[3]), Some(16));
                    let Some((OP_BITWISE_AND, m)) = def(&ops, sh[2]) else {
                        panic!("half write is not masked")
                    };
                    assert_eq!(const_value(&ops, m[3]), Some(0xFFFF));
                    let Some((OP_BITCAST, b)) = def(&ops, m[2]) else {
                        panic!("half write is not a v2f16 bitcast")
                    };
                    let Some((OP_COMPOSITE_CONSTRUCT, c)) = def(&ops, b[2]) else {
                        panic!("half write does not construct a v2f16")
                    };
                    wrote_hi = matches!(def(&ops, c[2]), Some((OP_F_CONVERT, _)));
                }
                _ => {}
            }
        }
        assert!(kept, "UNUSED_PRESERVE must keep the old destination word");
        assert!(
            wrote_hi,
            "WORD_1 must write the converted half into bits 31:16"
        );
        assert_valid_spirv(&t, "sdwa_f16");
    }

    /// `v_cvt_f32_u32_sdwa v5, v3 src0_sel:WORD_1` widens the selected word;
    /// omod/clamp apply to the f32 result.
    #[test]
    fn sdwa_cvt_f32_u32_applies_omod_and_clamp() {
        let mut code = sdwa(
            "V_CVT_F32_U32",
            false,
            &[
                ("VDST", 5),
                ("VSRC0", 3),
                ("SRC0_SEL", 5),
                ("DST_SEL", 6),
                ("OMOD", 1),
                ("CLAMP", 1),
            ],
        );
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter().any(|(o, a)| *o == OP_SHIFT_R_LOGICAL
                && a.len() == 4
                && const_value(&ops, a[3]) == Some(16)),
            "WORD_1 source is not shifted down"
        );
        let mul = ops
            .iter()
            .find_map(|(o, a)| {
                (*o == OP_FMUL
                    && a.len() == 4
                    && def(&ops, a[2]).is_some()
                    && const_bits(&ops, a[3]) == Some(0x4000_0000))
                .then(|| a[1])
            })
            .expect("omod mul:2 missing");
        let Some((OP_CONVERT_U_TO_F, _)) = def_unbits(&ops, def(&ops, mul).unwrap().1[2]) else {
            panic!("omod does not follow the u32-to-f32 conversion")
        };
        let store = ops
            .iter()
            .find(|(o, a)| {
                *o == OP_STORE && def_unbits(&ops, a[1]).is_some_and(|(d, _)| d == OP_EXT_INST)
            })
            .expect("clamped result is not stored");
        let (OP_EXT_INST, e) = def_unbits(&ops, store.1[1]).unwrap() else {
            unreachable!()
        };
        assert_eq!(e[3], 43, "clamp must be GLSL.std.450 FClamp");
        assert!(
            matches!(def_unbits(&ops, e[4]), Some((OP_FMUL, f)) if f[1] == mul),
            "clamp must consume the omod result"
        );
        assert_valid_spirv(&t, "sdwa_cvt_omod");
    }

    /// `v_add_nc_u32_sdwa` with clamp saturates unsigned to 0xFFFF_FFFF.
    #[test]
    fn sdwa_add_nc_u32_clamp_saturates() {
        let mut code = sdwa(
            "V_ADD_NC_U32",
            true,
            &[
                ("VDST", 13),
                ("VSRC0", 23),
                ("VSRC1", 24),
                ("SRC0_SEL", 6),
                ("SRC1_SEL", 5),
                ("DST_SEL", 6),
                ("CLAMP", 1),
            ],
        );
        code.extend(end_program());
        let t = translated(&code);
        let ops = word_ops(&t.words);
        assert!(
            ops.iter().any(|(o, a)| *o == OP_SHIFT_R_LOGICAL
                && a.len() == 4
                && const_value(&ops, a[3]) == Some(16)),
            "src1 WORD_1 is not shifted down"
        );
        let all = ops
            .iter()
            .find_map(|(o, a)| {
                (*o == OP_CONSTANT && a.len() >= 3 && a[2] == 0xFFFF_FFFF).then(|| a[1])
            })
            .expect("saturation constant missing");
        assert!(
            ops.iter().any(|(o, _)| *o == OP_U_LESS_THAN),
            "clamped add must compare the wrapped sum"
        );
        let (_, store) = ops
            .iter()
            .find(|(o, a)| *o == OP_STORE && def(&ops, a[1]).is_some_and(|(d, _)| d == OP_SELECT))
            .expect("saturated result is not stored");
        let Some((OP_SELECT, s)) = def(&ops, store[1]) else {
            unreachable!()
        };
        assert!(
            s[3] == all || s[4] == all,
            "saturation must select 0xFFFF_FFFF on overflow"
        );
        assert_valid_spirv(&t, "sdwa_sat_add");
    }

    /// Only the measured SDWA forms translate: other SDWA instructions and
    /// every DPP form stay refused with the documented reason.
    #[test]
    fn sdwa_forms_outside_the_measured_slice_stay_unsupported() {
        let mut code = sdwa(
            "V_MOV_B32",
            false,
            &[("VDST", 1), ("VSRC0", 2), ("SRC0_SEL", 6), ("DST_SEL", 6)],
        );
        code.extend(
            encode(
                "V_MOV_B32",
                "VOP1_VOP_DPP16",
                &[
                    ("SRC0", 0xFA),
                    ("VDST", 2),
                    ("VSRC0", 3),
                    ("DPP_CTRL", 0x1b),
                    ("ROW_MASK", 0xF),
                    ("BANK_MASK", 0xF),
                ],
                None,
            )
            .unwrap(),
        );
        code.extend(end_program());
        let t = SpirvVsBackend::translate_vs(&code, &blit_vs(), true).unwrap();
        assert_eq!(t.unsupported.get("v_mov_b32_sdwa"), Some(&1));
        assert_eq!(t.unsupported.get("v_mov_b32_dpp"), Some(&1));
        for why in t.unsupported_why.keys() {
            assert!(
                why.contains("SDWA/DPP in the bring-up backend"),
                "unexpected refusal reason {why}"
            );
        }
    }
}
