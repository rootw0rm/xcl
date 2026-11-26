//! Determinism of the direct translator (same input → byte-identical containers, in
//! separate translator instances and threads) and builder misuse reported as errors.

use super::kernel::{self, KernelOptions};
use crate::gpu::dxbc::SigElement as S;
use crate::gpu::rdna2::encode;
use crate::gpu::recomp::{CsInterface, PsInterface, VsInterface};

fn prog(list: &[(&str, &str, &[(&str, u64)], Option<u32>)]) -> Vec<u8> {
    let mut v = Vec::new();
    for (n, e, f, l) in list {
        v.extend(encode(n, e, f, *l).unwrap_or_else(|x| panic!("{n}: {x}")));
    }
    v.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
    v
}

fn el(name: &str, index: u32, sv: u32, ct: u32, reg: u32, mask: u8) -> S {
    S { name: name.into(), index, system_value: sv, component_type: ct, register: reg, mask, rw_mask: mask, stream: 0 }
}

/// Programs touching many registers (hash-map keyed state), branches, guarded memory,
/// LDS, cross-lane reads, and VS / PS signatures.
fn translate_all() -> Vec<Vec<u8>> {
    const V: u64 = 256;
    let branchy = prog(&[
        ("S_MOV_B32", "ENC_SOP1", &[("SDST", 0), ("SSRC0", 128)], None),
        ("S_MOV_B32", "ENC_SOP1", &[("SDST", 1), ("SSRC0", 138)], None),
        ("S_ADD_U32", "ENC_SOP2", &[("SDST", 0), ("SSRC0", 0), ("SSRC1", 1)], None),
        ("S_SUB_U32", "ENC_SOP2", &[("SDST", 1), ("SSRC0", 1), ("SSRC1", 129)], None),
        ("S_CMP_LG_U32", "ENC_SOPC", &[("SSRC0", 1), ("SSRC1", 128)], None),
        ("S_CBRANCH_SCC1", "ENC_SOPP", &[("SIMM16", 0xFFFC)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 1), ("SRC0", 0)], None),
        ("V_CMP_GT_U32", "ENC_VOPC", &[("SRC0", 160), ("VSRC1", 0)], None),
        ("S_AND_SAVEEXEC_B64", "ENC_SOP1", &[("SDST", 20), ("SSRC0", 106)], None),
        ("GLOBAL_LOAD_DWORD", "ENC_FLAT_GLBL", &[("VDST", 5), ("ADDR", 0), ("SADDR", 0x7D), ("OFFSET", 8)], None),
        ("V_ADD_F32", "ENC_VOP2", &[("VDST", 10), ("SRC0", V + 5), ("VSRC1", 2)], None),
        ("V_READLANE_B32", "ENC_VOP3", &[("VDST", 30), ("SRC0", V + 10), ("SRC1", 129)], None),
        ("S_MOV_B64", "ENC_SOP1", &[("SDST", 126), ("SSRC0", 20)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 12), ("SRC0", 30)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 13), ("SRC0", 31)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 14), ("SRC0", 32)], None),
    ]);
    let vs_code = prog(&[
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 0), ("SRC0", V + 4)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 1), ("SRC0", V + 5)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 2), ("SRC0", 128)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 3), ("SRC0", 242)], None),
        ("EXP", "ENC_EXP", &[("TGT", 12), ("EN", 0xF), ("VSRC0", 0), ("VSRC1", 1), ("VSRC2", 2), ("VSRC3", 3), ("DONE", 1)], None),
        ("EXP", "ENC_EXP", &[("TGT", 32), ("EN", 0xF), ("VSRC0", 8), ("VSRC1", 9), ("VSRC2", 10), ("VSRC3", 11)], None),
        ("EXP", "ENC_EXP", &[("TGT", 33), ("EN", 0x3), ("VSRC0", 0), ("VSRC1", 1), ("VSRC2", 0), ("VSRC3", 0)], None),
    ]);
    let vsi = VsInterface {
        inputs: vec![el("POSITION", 0, 0, 3, 0, 3), el("COLOR", 0, 0, 3, 1, 0xF)],
        outputs: vec![el("SV_Position", 0, 1, 3, 0, 0xF), el("COLOR", 0, 0, 3, 1, 0xF), el("TEXCOORD", 3, 0, 3, 2, 0x3)],
        attr_base: 4,
        params: None,
    };
    let ps_code = prog(&[
        ("V_INTERP_P2_F32", "ENC_VINTRP", &[("VDST", 4), ("VSRC", 1), ("ATTR", 0), ("ATTRCHAN", 0)], None),
        ("V_INTERP_P2_F32", "ENC_VINTRP", &[("VDST", 5), ("VSRC", 1), ("ATTR", 0), ("ATTRCHAN", 1)], None),
        ("V_INTERP_P2_F32", "ENC_VINTRP", &[("VDST", 6), ("VSRC", 1), ("ATTR", 1), ("ATTRCHAN", 0)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 7), ("SRC0", 242)], None),
        ("EXP", "ENC_EXP", &[("TGT", 0), ("EN", 0xF), ("VSRC0", 4), ("VSRC1", 5), ("VSRC2", 6), ("VSRC3", 7), ("DONE", 1), ("VM", 1)], None),
    ]);
    let psi = PsInterface {
        inputs: vec![el("SV_Position", 0, 1, 3, 0, 0xF), el("COLOR", 0, 0, 3, 1, 0xF), el("TEXCOORD", 3, 0, 3, 2, 0x3)],
        outputs: vec![el("SV_Target", 0, 64, 3, 0, 0xF)],
        input_ena: 0x302,
        attrs: None,
    };
    let cs = CsInterface { threads: [64, 1, 1], user_sgprs: 4, tgid: [true, false, false], tg_size: false, tid_components: 1 };
    let o = |wave64, split| KernelOptions { wave64, split, mem_windows: 1, sampler_base: 0 };
    let mut out = Vec::new();
    for (w, s) in [(false, false), (true, true), (true, false)] {
        out.push(kernel::translate(&branchy, o(w, s)).expect("kernel"));
        out.push(kernel::translate_compute(&branchy, o(w, s), &cs).expect("compute").dxil);
    }
    out.push(kernel::translate_vertex(&vs_code, o(true, false), &vsi).expect("vs").dxil);
    out.push(kernel::translate_pixel(&ps_code, o(true, false), &psi).expect("ps").dxil);
    out
}

#[test]
fn translation_is_byte_identical_across_runs_and_threads() {
    let first = translate_all();
    for _ in 0..3 {
        assert!(translate_all() == first, "a second translation differs");
    }
    let threads: Vec<_> = (0..4).map(|_| std::thread::Builder::new().stack_size(64 << 20).spawn(translate_all).unwrap()).collect();
    for t in threads {
        assert!(t.join().unwrap() == first, "a translation on another thread differs");
    }
}

// ---- builder misuse: recorded as the first error, finish fails instead of panicking

use super::container::Stage;
use super::module::{BinOp, FnAttrs, Module};
use super::shader::Shader;

/// A compute shader with `misuse` applied fails to finish with an error containing `want`.
fn finish_fails(want: &str, misuse: impl FnOnce(&mut Shader)) {
    let mut s = Shader::new(Stage::Compute, (6, 6));
    misuse(&mut s);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.try_finish([1, 1, 1], None)));
    let e = r.expect("try_finish panicked").expect_err("misuse was not reported");
    assert!(e.contains(want), "error {e:?} does not name {want:?}");
}

#[test]
fn forward_reference_outside_a_phi_is_an_error() {
    finish_fails("forward reference", |s| {
        let later = s.m.new_block();
        let entry = s.m.current_block();
        s.m.set_block(later);
        let one = s.m.i32c(1);
        let two = s.m.i32c(2);
        let v = s.m.bin(BinOp::Add, one, two);
        s.m.ret();
        s.m.set_block(entry);
        let _ = s.m.bin(BinOp::Add, v, one);
        s.m.br(later);
    });
}

#[test]
fn instruction_after_a_terminator_is_an_error() {
    finish_fails("after a terminator", |s| {
        s.m.ret();
        let one = s.m.i32c(1);
        let _ = s.m.bin(BinOp::Add, one, one);
    });
}

#[test]
fn unknown_overload_suffix_is_an_error() {
    finish_fails("overload suffix", |s| {
        let i32 = s.m.i32();
        let arr = s.m.array(4, i32);
        let _ = s.type_suffix(arr);
    });
}

#[test]
fn gep_and_extractvalue_into_wrong_types_are_errors() {
    finish_fails("gep into", |s| {
        let i32 = s.m.i32();
        let g = s.m.global("g", i32, 3, None, false, 4);
        let z = s.m.i32c(0);
        let _ = s.m.gep(g, &[z, z]);
    });
    finish_fails("gep: struct field", |s| {
        let i32 = s.m.i32();
        let st = s.m.named_struct("S", &[i32]);
        let g = s.m.global("g", st, 3, None, false, 4);
        let z = s.m.i32c(0);
        let k = s.m.i32c(5);
        let _ = s.m.gep_fields(g, &[z, k]);
    });
    finish_fails("not a pointer", |s| {
        let one = s.m.i32c(1);
        let _ = s.m.load(one, 4);
    });
    finish_fails("extractvalue", |s| {
        let one = s.m.i32c(1);
        let _ = s.m.extract(one, 0);
    });
}

#[test]
fn redeclared_function_and_bad_calls_are_errors() {
    finish_fails("redeclared", |s| {
        let v = s.m.void();
        let i32 = s.m.i32();
        let a = s.m.func_ty(v, &[]);
        let b = s.m.func_ty(v, &[i32]);
        s.m.declare("f", a, FnAttrs::None);
        s.m.declare("f", b, FnAttrs::None);
    });
    finish_fails("arguments", |s| {
        let v = s.m.void();
        let t = s.m.func_ty(v, &[]);
        let f = s.m.declare("f", t, FnAttrs::None);
        let one = s.m.i32c(1);
        s.m.call(f, &[one]);
    });
    finish_fails("second defined function", |s| {
        s.m.define_entry("other");
    });
}

#[test]
fn bad_constants_phis_and_stores_are_errors() {
    finish_fails("const_int", |s| {
        let f = s.m.f32();
        s.m.const_int(f, 1);
    });
    finish_fails("const_float", |s| {
        let i = s.m.i32();
        s.m.const_float(i, 1);
    });
    finish_fails("non-phi", |s| {
        let one = s.m.i32c(1);
        let v = s.m.bin(BinOp::Add, one, one);
        let b = s.m.current_block();
        s.m.add_incoming(v, one, b);
    });
    finish_fails("rawBufferStore of 5", |s| {
        let u = s.raw_uav(0, 0);
        let z = s.m.i32c(0);
        s.raw_store(u, z, &[z; 5]);
    });
}

#[test]
fn use_of_a_void_value_and_wrong_finish_are_errors() {
    finish_fails("void instruction", |s| {
        let u = s.raw_uav(0, 0);
        let z = s.m.i32c(0);
        s.raw_store(u, z, &[z]);
        // the store's (void) result as an operand
        let st = super::module::Value::Inst(s.m.block_insts(s.m.current_block()).last().copied().unwrap());
        let i32 = s.m.i32();
        let _ = s.m.cast(super::module::CastOp::BitCast, st, i32);
    });
    let s = Shader::new(Stage::Pixel, (6, 6));
    assert!(s.try_finish([1, 1, 1], None).is_err());
    let mut m = Module::new();
    m.ret();
    m.ret();
    assert!(m.try_to_bitcode("dxil-ms-dx", "").is_err());
}

/// `SKIPPED <test>: <reason>` straight to stderr: libtest captures `eprintln!`, so the
/// line would show only with `--nocapture` and `scripts/test.ps1` would not count it
/// (same contract as `e2e::debug_layer_skip_line`, K-TS-01).
fn x64_masks_skip_line(test: &str) -> String {
    format!("SKIPPED {test}: XCL_X64_ALL_MASKS set")
}

#[test]
fn x64_masks_skip_line_matches_test_ps1_pattern() {
    let line = x64_masks_skip_line("split_wave64_whole_live_elides_exchange_barriers_for_lane_local_masks");
    let rest = line.strip_prefix("SKIPPED ").expect("SKIPPED prefix");
    let (test, reason) = rest.split_once(": ").expect("colon separator");
    assert!(!test.is_empty() && !test.chars().any(char::is_whitespace), "test token must match \\S+: {line:?}");
    assert_eq!(reason, "XCL_X64_ALL_MASKS set");
}

#[test]
fn split_wave64_whole_live_elides_exchange_barriers_for_lane_local_masks() {
    use super::bitreader_tests::parse;
    use super::cfg::{Cfg, Term};
    use super::graphics::part;
    use super::kernel::{compute_init, Entry};
    use super::lower::Lw;
    use super::module::{Block, InstInfo};

    if std::env::var_os("XCL_X64_ALL_MASKS").is_some() {
        // straight to stderr: libtest captures eprintln!, and scripts/test.ps1 counts this line
        use std::io::Write;
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("split_wave64_whole_live_elides_exchange_barriers_for_lane_local_masks");
        let _ = writeln!(std::io::stderr(), "{}", x64_masks_skip_line(test));
        return;
    }
    let lower_cs = |code: &[u8], wave64: bool, split: bool| -> (Vec<u8>, u64, usize) {
        let cfg = Cfg::build(code).unwrap();
        let leaders = cfg.blocks.iter().filter_map(|b| b.insts.first().map(|&i| cfg.insts[i].pc)).collect();
        let threads = [if wave64 { 64 } else { 32 }, 1, 1];
        let cs = CsInterface { threads, user_sgprs: 2, tgid: [false; 3], tg_size: false, tid_components: 1 };
        let mut lw = Lw::new(&cfg.insts, &leaders, KernelOptions { wave64, split, mem_windows: 1, sampler_base: 0 }, &Entry::Compute(cs.clone()));
        let (tid, gtid) = lw.setup_lanes_compute(cs.threads, false);
        compute_init(&mut lw, &cs, tid, gtid);
        let (h0, exit) = (lw.block(Some(1)), lw.block(Some(1)));
        lw.edge(Block(0), h0);
        lw.s.m.set_block(h0);
        for &i in &cfg.blocks[0].insts {
            lw.inst(&cfg.insts[i]).unwrap();
        }
        assert!(matches!(cfg.blocks[0].term, Term::End));
        lw.br(exit);
        lw.s.m.set_block(exit);
        lw.seal(exit);
        lw.s.m.ret();
        lw.s.m.set_block(Block(0));
        lw.s.m.br(h0);
        let tgsm_bytes = lw.s.m.global_bytes(3);
        let barriers = (0..lw.s.m.block_count())
            .flat_map(|b| lw.s.m.block_insts(Block(b as u32)).iter().copied())
            .filter(|&i| matches!(lw.s.m.inst_info(i), InstInfo::Call("dx.op.barrier", _)))
            .count();
        (lw.s.try_finish(threads, Some(32)).unwrap(), tgsm_bytes, barriers)
    };
    let tgsm_globals = |c: &[u8]| -> usize {
        // Skip the 24-byte `DxilProgramHeader` (`container::program_part`) before `BC\xC0\xDE`.
        let dxil = part(c, b"DXIL").unwrap();
        parse(&dxil[24..]).find(8).unwrap().records_with(7).into_iter().filter(|r| (r[1] >> 2) == 3).count()
    };
    let local = prog(&[
        ("V_CMP_GT_U32", "ENC_VOPC", &[("SRC0", 160), ("VSRC1", 0)], None),
        ("V_CNDMASK_B32", "ENC_VOP2", &[("VDST", 2), ("SRC0", 128), ("VSRC1", 0)], None),
        ("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 2), ("SADDR", 0)], None),
    ]);
    let whole = prog(&[
        ("V_CMP_GT_U32", "ENC_VOPC", &[("SRC0", 160), ("VSRC1", 0)], None),
        ("S_MOV_B32", "ENC_SOP1", &[("SDST", 4), ("SSRC0", 106)], None),
        ("V_MOV_B32", "ENC_VOP1", &[("VDST", 2), ("SRC0", 4)], None),
        ("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 2), ("SADDR", 0)], None),
    ]);
    // `setup_lanes_compute` uses 1 TGSM dword (`xcl_wt`, 4 B) and 1 barrier; lane-local `VCC`
    // (`v_cndmask_b32`) skips `xcl_x64_s`, while scalar `VCC` (`s_mov_b32`) adds `xcl_x64_s` (+8 B, +2 barriers).
    let (c_local, bytes_local, bar_local) = lower_cs(&local, true, true);
    assert_eq!((tgsm_globals(&c_local), bytes_local, bar_local), (1, 4, 1));
    let (c_whole, bytes_whole, bar_whole) = lower_cs(&whole, true, true);
    assert_eq!((tgsm_globals(&c_whole), bytes_whole, bar_whole), (2, 12, 3));

    // 64-bit integer compares lower to 32-bit dword `icmp`s, leaving `D3D_SHADER_REQUIRES_INT64_OPS` (`0x8000`) unset.
    let cmp64 = prog(&[
        ("V_CMP_LT_I64", "ENC_VOPC", &[("SRC0", 256), ("VSRC1", 2)], None),
        ("V_CNDMASK_B32", "ENC_VOP2", &[("VDST", 4), ("SRC0", 128), ("VSRC1", 1)], None),
        ("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 4), ("SADDR", 0)], None),
    ]);
    let (c_cmp64, _, _) = lower_cs(&cmp64, false, false);
    let sfi0 = u64::from_le_bytes(part(&c_cmp64, b"SFI0").unwrap().try_into().unwrap());
    assert_eq!(sfi0 & 0x8000, 0, "64-bit integer compare must not set Int64Ops in SFI0: {sfi0:#x}");
}

#[test]
fn pops_pixel_shader_gates_rov_sfi0_flag_on_surviving_uav_handles() {
    use super::cfg::Cfg;
    use super::graphics::part;
    use super::graphics::StageInfo;
    use super::kernel::Entry;
    use super::lower::Lw;
    use super::module::Block;

    let ps_sfi0 = |pops: bool, write_uav: bool| -> u64 {
        let mov_f: &[(&str, u64)] = if pops { &[("VDST", 3), ("SRC0", 239)] } else { &[("VDST", 3), ("SRC0", 242)] };
        let mut ops = vec![("V_MOV_B32", "ENC_VOP1", mov_f, None)];
        if write_uav {
            ops.push(("GLOBAL_STORE_DWORD", "ENC_FLAT_GLBL", &[("ADDR", 0), ("DATA", 3), ("SADDR", 0)][..], None));
        }
        ops.push(("EXP", "ENC_EXP", &[("TGT", 0), ("EN", 0xF), ("VSRC0", 3), ("VSRC1", 3), ("VSRC2", 3), ("VSRC3", 3), ("DONE", 1), ("VM", 1)][..], None));
        let code = prog(&ops);
        let cfg = Cfg::build(&code).unwrap();
        let leaders = cfg.blocks.iter().filter_map(|b| b.insts.first().map(|&i| cfg.insts[i].pc)).collect();
        let psi = PsInterface { inputs: vec![], outputs: vec![el("SV_Target", 0, 64, 3, 0, 0xF)], input_ena: 2, attrs: None };
        let mw = 2 | crate::gpu::recomp::HEAP_WINDOWS;
        let mut lw = Lw::new(&cfg.insts, &leaders, KernelOptions { wave64: true, split: false, mem_windows: mw, sampler_base: 0 }, &Entry::Pixel(psi.clone()));
        lw.setup_lanes_gfx();
        lw.setup_pixel(&psi, &Default::default());
        let h0 = lw.block(Some(1));
        lw.edge(Block(0), h0);
        lw.s.m.set_block(h0);
        for &i in &cfg.blocks[0].insts {
            lw.inst(&cfg.insts[i]).unwrap();
        }
        lw.finish_gfx();
        lw.s.m.ret();
        lw.s.m.set_block(Block(0));
        lw.s.m.br(h0);
        let g = lw.gfx.take().unwrap();
        let c = lw.s.try_finish_graphics(g.io, StageInfo::Pixel { early_depth_stencil: false }).unwrap();
        u64::from_le_bytes(part(&c, b"SFI0").unwrap().try_into().unwrap())
    };
    assert_eq!(ps_sfi0(false, true) & 0x1000, 0, "non-POPS PS must not set ROVs");
    assert_eq!(ps_sfi0(true, false) & 0x1000, 0, "POPS PS whose UAV handles are DCE'd must not set ROVs");
    assert_eq!(ps_sfi0(true, true) & 0x1000, 0x1000, "POPS PS with live UAV store must set ROVs");
}
