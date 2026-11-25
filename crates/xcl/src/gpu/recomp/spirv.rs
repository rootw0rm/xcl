//! RDNA2 → SPIR-V backend for blit-class vertex shaders (Phase D step 14).
//!
//! In-process emitter for the same straight-line blit subset the GLSL backend
//! covers (`glsl.rs`): scalar ALU + a small VALU set + `exp pos`/`param` +
//! `s_endpgm`. Branches are rejected; the full-EXEC entry state is assumed
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
    for d in &insts {
        if rdna2::branch_target(d).is_some() || d.far_target.is_some() {
            return Err(format!(
                "+{:#x}: branches are not supported by the SPIR-V bring-up backend",
                d.pc
            ));
        }
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
const OP_SELECT: u16 = 169;
const OP_U_LESS_THAN: u16 = 176;
const OP_FORD_LESS_THAN: u16 = 184;
const OP_SHIFT_R_LOGICAL: u16 = 194;
const OP_SHIFT_R_ARITH: u16 = 195;
const OP_SHIFT_L_LOGICAL: u16 = 196;
const OP_BITWISE_OR: u16 = 197;
const OP_BITWISE_XOR: u16 = 198;
const OP_BITWISE_AND: u16 = 199;
const OP_NOT: u16 = 200;
const OP_LABEL: u16 = 248;
const OP_RETURN: u16 = 253;
const OP_FUNCTION_END: u16 = 56;

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
        if d.variant != Variant::Plain && d.enc.name != "ENC_EXP" {
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
        let src0 = if d.enc.name == "ENC_SOPK" {
            self.uint_const(d.literal.unwrap_or(0))
        } else {
            match self.code_value(f("SSRC0"), d.literal) {
                Some(s) => s,
                None => {
                    self.unsupported(d, "op");
                    return;
                }
            }
        };
        let src1 = if d.enc.name == "ENC_SOP2" {
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
        let res: Option<u32> = match name {
            "s_mov_b32" => Some(src0),
            "s_movk_i32" => Some(self.uint_const(d.literal.unwrap_or(0))),
            "s_add_u32" | "s_add_i32" => Some(self.tmp(uint_ty, OP_IADD, &[src0, src1])),
            "s_sub_u32" | "s_sub_i32" => Some(self.tmp(uint_ty, OP_ISUB, &[src0, src1])),
            "s_and_b32" => Some(self.tmp(uint_ty, OP_BITWISE_AND, &[src0, src1])),
            "s_or_b32" => Some(self.tmp(uint_ty, OP_BITWISE_OR, &[src0, src1])),
            "s_xor_b32" => Some(self.tmp(uint_ty, OP_BITWISE_XOR, &[src0, src1])),
            "s_not_b32" => Some(self.tmp(uint_ty, OP_NOT, &[src0])),
            "s_min_u32" => Some(self.umin(src0, src1)),
            "s_max_u32" => Some(self.umax(src0, src1)),
            _ => None,
        };
        match res {
            Some(v) => {
                if dst_code == 125 {
                    // null: discarded.
                } else if dst_code > 127 {
                    self.unsupported(d, "sdst");
                } else {
                    self.store_sgpr(dst_code, v);
                }
            }
            None => self.unsupported(d, "salu"),
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

    fn valu(&mut self, d: &Decoded, name: &str) {
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
            "v_add_nc_u32" | "v_add_u32" | "v_add_co_u32" => {
                Some(self.tmp(uint_ty, OP_IADD, &[a, b]))
            }
            "v_sub_nc_u32" | "v_sub_u32" => Some(self.tmp(uint_ty, OP_ISUB, &[a, b])),
            "v_subrev_nc_u32" | "v_subrev_u32" => Some(self.tmp(uint_ty, OP_ISUB, &[b, a])),
            "v_mul_lo_u32" => Some(self.tmp(uint_ty, OP_IMUL, &[a, b])),
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
            _ => None,
        };
        match (res, dst) {
            (Some(v), Some(Dst::V(n))) => self.store_vgpr(n, v),
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
        let mut mem: Vec<u32> = Vec::new();
        inst(&mut mem, OP_MEMORY_MODEL, &[ADDR_LOGICAL, MEM_GLSL450]);
        let bound = self.next;
        head[3] = bound;
        let mut out = head;
        out.extend_from_slice(&cap);
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
}
