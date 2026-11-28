# Retail Step 84 — real f64 `log2` and `_fdtest`

`objdump -p game_scarlett_master.exe` lists both names under
`api-ms-win-crt-math-l1-1-0.dll`; with no HLE handler they resolved to
generic auto-stubs. The Step-83 retail artifact (`/tmp/xcl-step83-r1`)
records each being called twice (`called_stubs`, `first_caller`
`exe+0x328f692` for `log2`, `exe+0x5ebe275` for `_fdtest`).

## IAT slots and call sites

| name | IAT slot | calls (Step 83) | first return address |
|---|---|---|---|
| `_fdtest` | `0x1467945e8` | 2 | `exe+0x5ebe275` |
| `log2` | `0x146794698` | 2 | `exe+0x328f692` |

- `log2` (double, `xmm0` in / `xmm0` out) is reached through two helpers:
  the IAT trampoline `0x145f40900` (`jmp qword ptr [0x146794698]`) and the
  integer helper `0x145bcf820`:

  ```
  0x5bcf820  mov  eax, ecx              ; zero-extend the u32 argument
  0x5bcf822  vcvtsi2sd xmm0, xmm0, rax
  0x5bcf827  jmp  0x145f40900           ; -> IAT 0x146794698
  ```

  The retail caller is `0x14328f680`: `sar ecx, 3` → `call log2`,
  `ecx = 6` → `call log2`, subtract, `vroundsd` truncate, and clamp the
  result at 7 — a mip-count computation. Both arguments are positive, so
  the C99 edges are not exercised on that path but must still be honest.
  The same helper is called at `0x143286a6e`/`0x143286aa5`
  (`log2(size << 8) - 8` → `vcvttsd2si`) and `0x141dcda61`.
- `_fdtest` is `short _fdtest(const float *value)`: the caller stores the
  value in memory and passes its address in `rcx`. The reproduced call
  site is `0x145ebe260`:

  ```
  0x5ebe260  sub  rsp, 0x28
  0x5ebe264  lea  rcx, [rsp + 0x24]
  0x5ebe269  mov  dword ptr [rcx], 0x7fc00000   ; quiet NaN (f32)
  0x5ebe26f  call qword ptr [0x1467945e8]
  0x5ebe275  cmp  ax, 2
  0x5ebe279  jne  0x5ebe280                     ; != 2 -> error reporter
  0x5ebe27b  ret
  ```

  So the return value is a 16-bit category in `ax`, and NaN must be `2`.
  Other call sites (e.g. `0x14028ad5e`, `0x1407fa828`) compare the same
  `ax == 2` to take a scalar fallback in vectorized `sqrt` code.

## Contract implemented

UCRT's `_fdtest` is `fpclassify` for `float`; the codes are the `FP_*`
macros in `corecrt_math.h` (`_DENORM -2`, `_FINITE -1`, `_INFCODE 1`,
`_NANCODE 2`, `FP_ZERO 0`). The plan's `_FD_INVALID/...` names and its
"pointed-to double" are both wrong; the disassembly pins a 4-byte store
and the `ax == 2` NaN test.

`hle/crt_linux.rs`:

- `log2(x: f64)` is `f64::log2`. C99 edges: `±0 → -inf`, negative and
  `-inf` → NaN, `+inf → +inf`, NaN propagates, and a denormal keeps its
  exact exponent (`log2(2^-1074) = -1074`).
- `_fdtest(value: *const f32) -> i16` classifies the pointed-to value:

  | input | code |
  |---|---|
  | NaN (quiet or payload) | `FP_NAN` = 2 |
  | `±inf` | `FP_INFINITE` = 1 |
  | `±0` | `FP_ZERO` = 0 |
  | normal | `FP_NORMAL` = -1 |
  | subnormal | `FP_SUBNORMAL` = -2 |

  A null pointer faults in UCRT; the HLE guards it and returns `FP_NAN`
  (an invalid value must not be reported as a valid zero). Both functions
  are registered at the end of `exports()`.

## Tests (RED first)

`crt_linux::tests::f64_log2_and_fdtest_follow_ucrt_categories` asserts
both names are in `exports()`, then calls them through `extern "win64"`
transmuted pointers. RED with only the test grafted: `log2 must be
wired`, 0 passed / 1 failed. GREEN at the fix: 1/0.

`log2` covers 8/0.5/1/3/10, `±0`, `-1`, `±inf`, NaN and
`f64::from_bits(1) = -1074`. `_fdtest` pins every code with literals —
NaN (`f32::NAN` and a payload NaN), `±inf`, `±0`, normal (1, -1,
`f32::MAX`, `MIN_POSITIVE`), subnormal (`±2^-149`, `1e-40`) — plus the
null guard.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step84-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step84-r1`, debug build):

- First present `3840x2160` at 6.871 s, 148 presents through token
  `0x94` at 12.455 s; the present window is 5.58 s (Step 83: 265 presents
  over 16.59 s — the parked pool-OOM trap fires at a data-dependent
  point, so counts are not comparable).
- Exit 133 SIGTRAP at `game_scarlett_master.exe+0x2219651` (same parked
  pool OOM, pc `0x142219651`, unchanged).
- `called_stubs` 27 → 25: the set difference is exactly
  `api-ms-win-crt-math-l1-1-0.dll!log2` and `!_fdtest`. Both also leave
  `stubbed_imports` (both are present there in the Step-83 report), so
  they now resolve to the HLE implementations rather than a logging stub.
- No new handler warning; no `log2`/`_fdtest` line in `xcl.log`.
- No causality claim for the pool fill.

## Semantics summary

| name | this branch | UCRT/IEEE eval |
|---|---|---|
| `log2` | `f64::log2` | `±0→-inf`, negative→NaN, `+inf→+inf`, denormal exact exponent |
| `_fdtest` | classify `*const f32` | `NaN 2`, `inf 1`, `zero 0`, `normal -1`, `subnormal -2` |
