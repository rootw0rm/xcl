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

## Follow-up

`code-assist:xcl-first-frame:step-08:record-vs-branches` (task-1795794902-51cb): console VS
control flow (the `N branches are not supported by the SPIR-V bring-up backend` key). Not
fixed in this measurement change.
