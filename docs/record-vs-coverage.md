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

## Pure-scalar SALU/SOPC widening (step-10, 2026-11-27)

The emitter now translates the remaining self-contained scalar ALU and the 32-bit SOPC compare
family. Same command and corpus (26,503 records, seed 1, release):

| outcome | records | share of all | share of VS |
|---|---:|---:|---:|
| non-VS | 24,267 | 91.6% | — |
| reject | 1,204 | 4.5% | 53.8% |
| incomplete | 899 | 3.4% | 40.2% |
| complete | 133 | 0.5% | 5.9% |
| all | 26,503 | 100% | (VS: 2,236) |

Outcome counts are unchanged from step-09. **No record flipped complete**: every record that
carried an in-scope reason also carries at least one reason outside this slice. Reason kinds
fell **63 → 51**; the 12 in-scope kinds disappeared with exactly their step-09 record counts,
no other reason's count changed, and no new kind appeared. Total reason instances fell
**4,568 → 4,387 (−181)** — exactly the in-scope sum:

| reason | records (step-09 → step-10) |
|---|---:|
| `s_mul_i32` | 68 → 0 |
| `s_mul_hi_u32` | 65 → 0 |
| `s_lshl_b32` | 17 → 0 |
| `s_bfe_i32` | 13 → 0 |
| `s_lshr_b32` | 8 → 0 |
| `s_addk_i32` | 3 → 0 |
| `s_mulk_i32` | 2 → 0 |
| `s_addc_u32` | 1 → 0 |
| `s_bcnt1_i32_b32` | 1 → 0 |
| `s_bfe_u32` | 1 → 0 |
| `s_cmp_lg_u32` | 1 → 0 |
| `s_ff1_i32_b32` | 1 → 0 |
| total | 181 → 0 |

Translated families (one test per family through `translate_vs_spirv` plus `spirv-val`):

- `s_mul_i32` low product; `s_mul_hi_u32` widened 64-bit product's high word (declares
  `Capability Int64`); `s_mulk_i32` `dst * sign_extended(SIMM16)`.
- `s_lshl_b32`/`s_lshr_b32` shift by `src1 & 31`; `s_bfe_u32`/`s_bfe_i32` decode
  `offset = c[4:0]`, `width = c[22:16]`, width 0 → 0, width ≥ 32 → the shifted word, with
  sign extension for the `i32` form.
- `s_addk_i32` adds the destination old value and the sign-extended immediate; signed overflow
  materialises to SCC. `s_addc_u32` loads SCC as carry-in (two `OpIAdd`s), computes carry-out
  as `(a+b < a) || (a+b+carry < a+b)`.
- `s_bcnt1_i32_b32` is `OpBitCount`; `s_ff1_i32_b32` is GLSL.std.450 `FindILsb` (0xFFFF_FFFF
  for zero); both leave SCC as the ISA specifies (`s_ff1` writes no SCC).
- All 12 32-bit `s_cmp_{eq,lg,gt,ge,lt,le}_{i32,u32}` write SCC as `0`/`1`; signed relations
  bitcast to the signed integer type and use the signed compare opcodes.
- SCC on the already-translated scalar ops now follows the ISA too: carry for `s_add_u32`,
  signed overflow for `s_add_i32`/`s_sub_i32`, borrow for `s_sub_u32`, result-nonzero for
  bitwise/not/shift/bfe/bcnt, and the comparison result for `s_min_u32`/`s_max_u32`. This
  changes no outcome count.
- SOPK immediates decode from the 16-bit signed `SIMM16` field (the shared path also fixes
  `s_movk_i32`, which previously read a literal dword instead of the field).

The unmodified `s_bfe_*` field decoding and SCC model mirror `gpu/recomp/emit.rs` and
`dxil/lower.rs`; `s_mul_hi_u32` carries the same `shaderInt64` device requirement as the VALU
form.

### Residual after step-10

All 4,387 remaining reason instances are in four groups (counts are record instances, so they
overlap and do not sum to the 899 incomplete records):

- **Memory / descriptors — 2,263.** `s_load_dwordx4` 833, `s_buffer_load_dwordx4` 390,
  `s_buffer_load_dwordx16` 308, `buffer_load_dwordx4` 237, `s_buffer_load_dwordx8` 137,
  `s_buffer_load_dword` 109, `buffer_load_dword` 86, `buffer_load_dwordx3` 49,
  `s_buffer_load_dwordx2` 48, `buffer_load_format_x` 18, `buffer_load_dwordx2` 16,
  `image_load` 13, `s_load_dwordx8` 13, `tbuffer_load_format_xyzw` 3,
  `buffer_load_format_xyzw` 1, `global_load_dword` 1, `global_load_dwordx2` 1. Gated on
  draw-time root arguments/user SGPRs (see Step 9 evidence); still rejected.
- **Control flow — 1,976.** Branch shapes 1,204 (877/169/117/28/9/4), `s_swappc_b64` 771,
  `s_getpc_b64` 1. The step-08 decision stands.
- **Documented non-goals — 148.** SDWA/DPP 80, b64/EXEC/VCC mask ops 27
  (`s_or_b64` 11, `s_mov_b64` 8, `s_and_saveexec_b64` 6, `s_cselect_b64` 1, `s_xor_b64` 1),
  `s_setreg_imm32_b32` 22, f16 value class 9, cube ops 3, `v_cmpx_*` EXEC writes 3, and the
  small VALU residual 4 (`v_add_co_ci_u32` 2, `v_mad_u32_u24` 1, `v_lshlrev_b64` 1).

In other words no self-contained 32-bit SALU/SOPC reason remains. The residual is memory,
control flow, or one of the documented out-of-scope classes — not an unclassified ALU gap.

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
- **DPP** (`quad_perm`, `row_*`, `dpp8`) reads another lane; the per-lane bring-up model has no
  cross-lane value and the corpus carries no DPP instance. Other SDWA instructions and VOPC
  SDWA compares stay refused; step-12 covers only the six measured SDWA forms.
- **f16 value class** is translated for the measured plain forms (step-13); cube ops and the
  small VALU residual (`v_add_co_ci_u32`, `v_mad_u32_u24`, `v_lshlrev_b64`) are separate
  follow-ups; `s_setreg_imm32_b32` stays unsupported until the emitter models the MODE register.

### Next dominant self-contained family

The cube ops (3 record-instances, one record) and the small VALU residual (4, four records) are
the next slice after step-13: `v_cubema_f32`/`v_cubesc_f32`/`v_cubetc_f32`, `v_add_co_ci_u32`,
`v_mad_u32_u24` and `v_lshlrev_b64`. They are pure VALU with no memory, mask or MODE
involvement. b64/EXEC/VCC masks (27) still need the per-lane model, `s_setreg` (22) the MODE
register and `v_cmpx_*` (3) the per-lane mask.

## Tooling polish (step-11, 2026-11-27)

Three debts from the step-07/08 reviews, no translation change:

- `--record-vs` is now listed in the `xcl --help` usage block for `xcl shaders`
  (`crates/xcl/src/main.rs`), including the `--sample`/`--seed`/`--top`/`--threads`/`--json`
  switches the sweep accepts.
- `BranchFlow::shape` names `unconditional branches` when a program's only flow is
  `s_branch` or a resolved far jump, instead of rendering the empty shape `()` (which
  collapsed every such record into one uninformative reason key). Mixed shapes are
  unchanged, so the corpus keys stay byte-identical.
- `far_jump_is_a_named_unconditional_branch_shape` pins the far-jump claim: synthetic
  `s_getpc_b64; s_add_u32 <literal 0x40000>; s_addc_u32; s_setpc_b64` resolves to the
  target `0x40004` (beyond the ±128 KB `s_branch` reach), classifies as one unconditional
  branch with that shape, and the refusal text names it.

Corpus re-run (26,503 records, seed 1, release): outcomes unchanged
(24,267 non-VS / 1,204 reject / 899 incomplete / 133 complete), 51 reason kinds; the six
branch keys remain 877 / 169 / 117 / 28 / 9 / 4 = 1,204. No corpus record had
unconditional-only flow, so no key moved.

## SDWA operand forms (step-12, 2026-11-27)

The emitter now translates the six SDWA forms the step-10 sweep named. Same command and corpus
(26,503 records, seed 1, release):

| outcome | records | share of all | share of VS |
|---|---:|---:|---:|
| non-VS | 24,267 | 91.6% | — |
| reject | 1,204 | 4.5% | 53.8% |
| incomplete | 899 | 3.4% | 40.2% |
| complete | 133 | 0.5% | 5.9% |
| all | 26,503 | 100% | (VS: 2,236) |

Outcome counts are unchanged from step-11. **No record flipped complete**: every record that
carried an in-scope reason still carries at least one reason outside this slice. Reason kinds
fell **51 → 45**; the six in-scope kinds disappeared with exactly their step-10 record counts,
no other reason's count changed, and no new kind appeared. Total reason instances fell
**4,387 → 4,307 (−80)** — exactly the in-scope sum:

| reason | records (step-10 → step-12) |
|---|---:|
| `v_cvt_f32_u32_sdwa` | 65 → 0 |
| `v_add_nc_u32_sdwa` | 4 → 0 |
| `v_lshlrev_b32_sdwa` | 4 → 0 |
| `v_or_b32_sdwa` | 4 → 0 |
| `v_cvt_f32_f16_sdwa` | 2 → 0 |
| `v_cvt_f16_f32_sdwa` | 1 → 0 |
| total | 80 → 0 |

Translated semantics (`spirv::tests::sdwa_*` through `translate_vs_spirv` plus `spirv-val`,
mirroring `gpu/recomp/emit.rs` and `gpu/dxil/lower_valu.rs` `sdwa_sel`/`sdwa_dst`/`mods`):

- **Sources.** `SRC0_SEL`/`SRC1_SEL` read BYTE_0..3 or WORD_0..1 and zero-extend, or
  sign-extend with `SEXT`. The `S0`/`S1` bit makes the field an SGPR or inline constant instead
  of a VGPR. `ABS`/`NEG` apply to the f32 source of `v_cvt_f16_f32_sdwa`.
- **Destination.** `DST_SEL` places the result's low 8/16/32 bits in the selected byte/word.
  `DST_UNUSED` PAD zeroes the rest, SEXT sign-extends the written value, and PRESERVE keeps the
  destination's old bits — the corpus pattern `v_cvt_f16_f32_sdwa …, dst_sel:WORD_1,
  dst_unused:UNUSED_PRESERVE` keeps the low half.
- **Output modifiers.** `OMOD` (x2/x4/÷2) and `CLAMP` apply in f32 to the
  `v_cvt_f32_u32_sdwa`/`v_cvt_f32_f16_sdwa` results and to the `v_cvt_f16_f32_sdwa` source
  before conversion. `CLAMP` on `v_add_nc_u32_sdwa` is unsigned saturation to 0xFFFF_FFFF (the
  DXIL backend's rule).
- **f16.** `v_cvt_f32_f16_sdwa` widens the selected half; `v_cvt_f16_f32_sdwa` narrows with
  round-to-nearest-even. To avoid a 16-bit integer type, the dword bitcasts to a `v2f16` and
  lane 0 is extracted/packed, so the module adds only `Capability Float16`. As with the
  existing `shaderInt64` requirement, a consumer must enable `shaderFloat16` at device creation
  (RDNA2 supports it; the record-VS path still has no retail consumer).
- **Refused.** DPP (`quad_perm`/`row_*`/`dpp8`) reads another lane and stays refused (no corpus
  instance); every other SDWA instruction and every VOPC SDWA compare stay refused with the
  same `SDWA/DPP in the bring-up backend` reason.

### Residual after step-12

All 4,307 remaining reason instances are the step-10 groups minus the SDWA forms (counts are
record instances, so they overlap and do not sum to the 899 incomplete records):

- **Memory / descriptors — 2,263.** Unchanged; still gated on draw-time root arguments.
- **Control flow — 1,976.** Unchanged; the step-08 decision stands.
- **Documented non-goals — 68.** b64/EXEC/VCC mask ops 27, `s_setreg_imm32_b32` 22, f16 value
  class 9, cube ops 3, `v_cmpx_*` EXEC writes 3, small VALU residual 4 (`v_add_co_ci_u32` 2,
  `v_mad_u32_u24` 1, `v_lshlrev_b64` 1).

## Plain f16 value class (step-13, 2026-11-27)

The emitter now translates the six measured plain f16 forms step-12 left. Same command and
corpus (26,503 records, seed 1, release):

| outcome | records | share of all | share of VS |
|---|---:|---:|---:|
| non-VS | 24,267 | 91.6% | — |
| reject | 1,204 | 4.5% | 53.8% |
| incomplete | 899 | 3.4% | 40.2% |
| complete | 133 | 0.5% | 5.9% |
| all | 26,503 | 100% | (VS: 2,236) |

Outcome counts are unchanged from step-12. **No record flipped complete**: the nine in-scope
instances live in two records (`(VS_FireVolumeFrontBack_24e49fd7`, `VS_Transform_c38bc63c`) that
still fail on `s_buffer_load_dwordx16`, `s_load_dwordx4` and `s_swappc_b64`. Reason kinds fell
**45 → 39**; total reason instances fell **4,307 → 4,298 (−9)** — exactly the in-scope sum, no
other reason's count changed and no new kind appeared:

| reason | records (step-12 → step-13) |
|---|---:|
| `v_fma_mix_f32` | 3 → 0 |
| `v_cvt_f32_f16` | 2 → 0 |
| `v_cvt_f16_f32` | 1 → 0 |
| `v_fma_mixhi_f16` | 1 → 0 |
| `v_fma_mixlo_f16` | 1 → 0 |
| `v_pack_b32_f16` | 1 → 0 |
| total | 9 → 0 |

Translated semantics (`spirv::tests::valu_*` through `translate_vs_spirv` plus `spirv-val`,
mirroring `gpu/recomp/emit.rs` and `gpu/dxil/lower_valu.rs`):

- **`v_cvt_f32_f16`.** `OP_SEL` bit 0 selects the half (0 = low, 1 = high); the dword bitcasts
  to a `v2f16` and the selected lane widens. The VOP3 `OMOD`/`CLAMP` apply to the f32 result.
- **`v_cvt_f16_f32`.** `ABS`/`NEG` (VOP3) and `OMOD`/`CLAMP` apply in f32 before a
  round-to-nearest-even narrowing (`OpFConvert`). The plain form keeps the destination half it
  does not write; `OP_SEL` bit 3 moves the write to the high half. SDWA keeps the step-12
  destination-selector behavior.
- **`v_pack_b32_f16`.** `OP_SEL` bit 0 picks src0's half for bits 15:0, bit 1 picks src1's half
  for bits 31:16.
- **`v_fma_mix*`.** `OP_SEL_HI`/`OP_SEL_HI_2` name which sources are f16, `OP_SEL` the half,
  `NEG_HI` the abs and `NEG` the negation per source. The product-add is `OpFMUL` + `OpFADD`
  and `CLAMP` saturates in f32. `v_fma_mix_f32` writes the f32 bits; `mixlo`/`mixhi` narrow
  toward zero — the codebase's `f32tof16` convention; `OpFConvert` rounds to nearest even, so a
  conversion that rounded away from zero is stepped back one magnitude — and keep the other
  destination half.
- **Half type.** Reuses step-12's `v2f16` + `Capability Float16`; the module declares no 16-bit
  integer type.

### Residual after step-13

All 4,298 remaining reason instances are the step-10 groups minus the SDWA and f16 forms
(counts are record instances, so they overlap and do not sum to the 899 incomplete records):

- **Memory / descriptors — 2,263.** Unchanged; still gated on draw-time root arguments.
- **Control flow — 1,976.** Unchanged; the step-08 decision stands.
- **Documented non-goals — 59.** b64/EXEC/VCC mask ops 27, `s_setreg_imm32_b32` 22, cube ops 3
  (`v_cubema_f32`/`v_cubesc_f32`/`v_cubetc_f32`), `v_cmpx_*` EXEC writes 3, and the small VALU
  residual 4 (`v_add_co_ci_u32` 2, `v_mad_u32_u24` 1, `v_lshlrev_b64` 1).

### Next dominant self-contained family

Cube ops (3, one record) and the small VALU residual (4, four records) are the next slice: they
are pure VALU with no memory, mask or MODE involvement. b64/EXEC/VCC masks (27) still need the
per-lane model, `s_setreg` (22) the MODE register and `v_cmpx_*` (3) the per-lane mask.

## Follow-up

`code-assist:xcl-first-frame:step-08:record-vs-branches` (task-1795794902-51cb) is resolved as
the documented decision above; the classifier and its tests live in `gpu/recomp/spirv.rs`
(`BranchFlow`, `branch_flow`). No behavior change beyond the reject-reason shape.

Step-09 (`code-assist:xcl-first-frame:step-09:record-vs-valu`, commit on
`m22-vs-valu-coverage`) implements the VALU slice above. Step-10
(`code-assist:xcl-first-frame:step-10:record-vs-salu`, commit on
`m22-vs-salu-coverage`) implements the SALU/SOPC slice; the SDWA/DPP gap is queued as the next
follow-up. Step-11 (`code-assist:xcl-first-frame:step-11:record-vs-polish`, commit on
`m22-vs-polish`) is the help/shape/far-jump polish above. Step-12
(`code-assist:xcl-first-frame:step-12:record-vs-sdwa`, commit on `m22-vs-sdwa-dpp`) implements
the measured SDWA forms. Step-13 (`code-assist:xcl-first-frame:step-13:record-vs-f16`, commit on
`m22-vs-f16`) implements the plain f16 value class; cube ops and the small VALU residual are
queued as the next follow-up.

