# Console-VS S1-S4 fallback coverage (M2.2)

Date: 2026-11-27. Branch: `m22-vs-fallback-coverage` (base `f590d27d`).

`xcl shaders <dir> --record-vs` classifies every sampled record offline: S1 is
`recomp::record_vs_interface`, S2/S3 is `recomp::spirv::record_vs_words` on the record's own
bytes. No device, no DXC, no GPU.

| outcome | meaning |
|---|---|
| non-VS | console shader that is not a VS, or a VS without an S1 interface |
| reject | not a console shader container, or S2/S3 returned an error |
| incomplete | S2/S3 words exist but `unsupported` is non-empty (ranked `unsupported_why` keys) |
| complete | every instruction translated; device validation is not implied |

Reasons use `gpu::dxil::coverage::reason_key` (operands and numbers collapse). A reason is
counted once per record, so an incomplete record can contribute several keys.

## Corpus

`/run/media/anon/12TB_B/leonida-work/shaders/dxbc` — 26,503 `.dxbc` console containers.
The directory also holds `disasm/` (26,503 `.s` text listings); `xcl shaders <dir>` recurses,
so a sweep of the parent counts those as rejects. Measure records from a directory that
contains only `.dxbc`:

```bash
mkdir -p /tmp/xcl-vs-dxbc
find /run/media/anon/12TB_B/leonida-work/shaders/dxbc -maxdepth 1 -type f -name '*.dxbc' \
  -exec ln -sf -t /tmp/xcl-vs-dxbc -- {} +
./target/debug/xcl shaders /tmp/xcl-vs-dxbc --record-vs --sample 0 --threads 4
```

## Result (all 26,503 records, seed 1, 46.6 s)

| outcome | records | share of all | share of VS |
|---|---:|---:|---:|
| non-VS | 24,267 | 91.6% | — |
| reject | 1,204 | 4.5% | 53.8% |
| incomplete | 1,022 | 3.9% | 45.7% |
| complete | 10 | 0.0% | 0.4% |
| all | 26,503 | 100% | (VS: 2,236) |

Only 10 of 2,236 console VS records translate completely today.

Top 10 reasons (records):

```
1204  N branches are not supported by the SPIR-V bring-up backend
 833  s_load_dwordx4: encoding
 804  v_fmac_f32: valu
 771  s_swappc_b64: salu
 390  s_buffer_load_dwordx4: encoding
 308  s_buffer_load_dwordx16: encoding
 237  buffer_load_dwordx4: encoding
 232  v_cvt_f32_ubyte0: valu
 137  s_buffer_load_dwordx8: encoding
 109  s_buffer_load_dword: encoding
```

Every reject (1,204) is the branch refusal; all other reasons come from incomplete
translations. The branch reason is 54% of all console VS records and dominates.

The exact command on the parent directory (`--sample 100 --threads 4`) gives 100 records:
43 non-VS, 53 reject, 4 incomplete, 0 complete. 50 of the 53 rejects are `disasm/*.s` text
(`not a console shader container`), not shader records.

## Branch shapes and the control-flow decision (step-08, 2026-11-27)

The single branch refusal is now split by `recomp::spirv::branch_flow`, the pure classifier the
backend uses in its reject message. Same corpus, `--sample 0`, seed 1; the outcome counts are
unchanged, only the branch reason is broken into its shapes:

| reject shape | records |
|---|---:|
| exec-mask + scc/vcc branches | 877 |
| exec-mask + scc/vcc branches, backward edges | 169 |
| scc/vcc branches | 117 |
| exec-mask branches | 28 |
| exec-mask branches, backward edges | 9 |
| scc/vcc branches, backward edges | 4 |
| all branch rejects | 1,204 |

`exec-mask` is `s_cbranch_execz/execnz`; `scc/vcc` is every other conditional; `backward edges`
are targets at or before the branch (loops). The categories use words only, so `reason_key`
keeps them apart; the 1,204 total matches the cross-tab of the decoded programs.

**Decision: the SPIR-V bring-up backend does not emit structured control flow for these
records.** Three measurements support it.

1. **Branches are not what blocks these records.** A throwaway local patch (not shipped) removed
   the branch check and counted branch instructions as ignored, then re-ran the same sweep:
   still 10 complete, 2,226 incomplete, and **0 of the 1,204 branch records complete**. Each one
   still carries at least 28 untranslated instructions (mean 344) across at least 7 distinct
   reasons; `s_load_dwordx4` appears in all 1,204, `s_buffer_load_dword` in 1,187 and
   `v_fmac_f32` in 1,155. `resolve_record_vs` accepts the words only on `complete()`, so branch
   support alone would change no draw.
2. **The shapes are not straight-line uniform `if`s.** 1,083 of the 1,204 use EXEC-mask
   branches, opened by mask saves (`s_and_saveexec_b64`: 2,834 uses) — lane-divergent regions
   where the bring-up model assumes full EXEC. The existing `recomp::lane_masks` analysis (the
   only module that models per-lane masks; "nothing translates with this yet") accepts
   570 of the 1,204 and refuses 634 (617 "a branch on whether any lane of a mask is set, with
   lanes in EXEC without its bit", 17 indirect jump). Of the 570, 481 have lane branches and
   only 89 are all-uniform — and those 89 still carry other untranslated instructions (the
   branch-free sweep made none of the 1,204 complete).
3. **The blockers rank differently.** Descriptor/buffer loads and VALU/SDWA dominate the
   untranslated instructions; control flow is neither first nor independently sufficient.
   Emitting `OpSelectionMerge`/`OpLoopMerge` around a body the emitter still cannot translate
   would add risk without a complete shader.

So the next coverage measurement should target the load/VALU reasons, not branches. Branch
shapes stay visible in the sweep so a future decision can be re-made on data.

## Pure-register VALU widening (step-09, 2026-11-27)

The emitter now translates the self-contained VALU families the sweep named; every one of the
39 in-scope reason kinds below is gone from the corpus. Same command and corpus as above
(26,503 records, seed 1, release build):

| outcome | records | share of all | share of VS |
|---|---:|---:|---:|
| non-VS | 24,267 | 91.6% | — |
| reject | 1,204 | 4.5% | 53.8% |
| incomplete | 899 | 3.4% | 40.2% |
| complete | 133 | 0.5% | 5.9% |
| all | 26,503 | 100% | (VS: 2,236) |

Reason kinds fell 102 → 63. **123 records flipped incomplete → complete** (10 → 133); each
had only in-scope reasons left. The proof is the reason table: every remaining reason's record
count is byte-identical to the step-08 sweep, and no new kind appeared. A record that still hit
any other blocker would have decremented that reason's count. Rejects are unchanged (1,204
branch refusals); non-VS is unchanged.

Top remaining reasons (records):

```
 877  N branches are not supported ... (exec-mask branches, scc/vcc branches)
 833  s_load_dwordx4: encoding
 771  s_swappc_b64: salu
 390  s_buffer_load_dwordx4: encoding
 308  s_buffer_load_dwordx16: encoding
 237  buffer_load_dwordx4: encoding
 169  N branches are not supported ... (+ backward edges)
 137  s_buffer_load_dwordx8: encoding
 117  N branches are not supported ... (scc/vcc branches)
 109  s_buffer_load_dword: encoding
  86  buffer_load_dword: encoding
  68  s_mul_i32: salu
  65  s_mul_hi_u32: salu
```

Translated families (semantics pinned by `gpu/recomp/emit.rs` / `dxil/lower_valu.rs`, one test
per family through `translate_vs_spirv` plus `spirv-val`):

- `v_fmac_f32` (VOP2/VOP3): `vdst = src0 * src1 + vdst(old)` — the test proves the addend is an
  `OpLoad` of the destination variable, not a constant.
- `v_cvt_f32_ubyte0..3`; `v_rcp_f32`/`v_rcp_iflag_f32` (`1.0/x`); `v_rsq_f32`, `v_sqrt_f32`
  (GLSL.std.450 `InverseSqrt`/`Sqrt`); `v_exp_f32`/`v_log_f32` (**exp2/log2**); `v_sin_f32`/
  `v_cos_f32` (input in turns, multiplied by 2π); `v_trunc_f32`; `v_fract_f32`
  (`x - floor(x)` with the `>= 1.0` clamp); `v_med3_f32` (`max(min(a,b), min(max(a,b),c))`).
- Integer/bit: `v_lshl_add_u32`, `v_lshl_or_b32`, `v_or3_b32`, `v_add3_u32`, `v_bfe_u32`,
  `v_bfi_b32`, `v_perm_b32` (4/8-byte selector table incl. the sign/0/all-ones classes),
  `v_mul_hi_u32` (widened 64-bit product; declares `Capability Int64`), `v_readfirstlane_b32`
  (copy — every VGPR is a scalar in this model).
- Compares: `v_cmp_{eq,ne,lt,le,gt,ge}_u32`, `v_cmp_{eq,ne,lt,le,gt,ge}_i32`,
  `v_cmp_{lt,eq,le,gt,ge,lg}_f32` and their `n*` unordered forms; VOPC writes the VCC pair,
  VOP3 the explicit `SDST` pair. `v_cmp_class_f32` emits the IEEE class index
  (0 sNaN … 9 +inf) and tests the mask bit. `v_cndmask_b32` VOP3 uses its explicit `SRC2`
  mask; VOP2 keeps the implicit VCC.

Known approximations, documented not hidden: `v_rcp_iflag_f32` does not model the hardware
denormal flush (same as the HLSL backend's `rcp`); `v_rcp`/`v_rsq` are exact division/sqrt
rather than the hardware approximation; `v_mul_hi_u32` needs `shaderInt64` on the device.

### Non-goals (unchanged, with evidence)

- **SMEM/MUBUF/MTBUF/IMG/global/tbuffer loads.** The record path has no user data to feed them:
  `record_vs_words(&[u8])` → `translate_vs_spirv(code, &vs, true)` passes only bytes + the S1
  interface (`gpu/recomp/spirv.rs:124-147`); `VsInterface` has no `user_sgprs`
  (`gpu/recomp/mod.rs:134-142`); `PsoRecord` stashes no root args (`hle/d3d12x_linux.rs:562-582`);
  the Linux command list has `SetGraphicsRootSignature` but no
  `SetGraphicsRoot32BitConstant`/`SetGraphicsRootDescriptorTable` (`:302-340`); `ReplayDraw`
  has no user data (`vulkan_graphics.rs:751-767`). Zero-filled bindings would be fake success.
- **Branches and `s_swappc_b64`.** The step-08 decision stands: with branches ignored, 0 of the
  1,204 branch records complete.
- **EXEC/VCC b64 mask ops** (`s_mov_b64`, `s_or_b64`, `s_and_saveexec_b64`, …) and `v_cmpx_*`:
  the bring-up model has no per-lane masks.
- **SDWA/DPP**, f16 value class (`v_fma_mix*`, `v_cvt_f16_f32`, `v_pack_b32_f16`), cube ops and
  `v_add_co_ci_u32`: separate follow-ups; `s_setreg_imm32_b32` stays unsupported until the
  emitter models the MODE register.

### Next dominant self-contained family

SALU/SOPC, the step-10 slice: `s_mul_i32` (68), `s_mul_hi_u32` (65), `s_lshl_b32` (17),
`s_bfe_i32` (13), `s_lshr_b32` (8), `s_addk_i32` (3), `s_mulk_i32` (2), `s_addc_u32` (1),
`s_bcnt1_i32_b32` (1), `s_ff1_i32_b32` (1), `s_bfe_u32` (1), `s_cmp_lg_u32` (1) = 181
record-instances. It reads only SGPRs and is the largest remaining block that is not blocked on
draw-time user data, control flow, or `MODE`.

## Follow-up

`code-assist:xcl-first-frame:step-08:record-vs-branches` (task-1795794902-51cb) is resolved as
the documented decision above; the classifier and its tests live in `gpu/recomp/spirv.rs`
(`BranchFlow`, `branch_flow`). No behavior change beyond the reject-reason shape.

Step-09 (`code-assist:xcl-first-frame:step-09:record-vs-valu`, commit on
`m22-vs-valu-coverage`) implements the VALU slice above; the SALU/SOPC gap is queued as
`code-assist:xcl-first-frame:step-10:record-vs-salu`.
