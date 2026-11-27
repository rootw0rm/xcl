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

## Follow-up

`code-assist:xcl-first-frame:step-08:record-vs-branches` (task-1795794902-51cb) is resolved as
the documented decision above; the classifier and its tests live in `gpu/recomp/spirv.rs`
(`BranchFlow`, `branch_flow`). No behavior change beyond the reject-reason shape.
