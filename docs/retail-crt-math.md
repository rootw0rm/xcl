# Retail Step 78 — real `erff` / `fmodf` / `log2f` / `atan2`

The retail title imports the scalar math group from
`api-ms-win-crt-math-l1-1-0.dll`. Four of them had no HLE handler, so the
resolver installed the generic auto-stub: an integer thunk that returns 0 in
`rax` and never writes `xmm0`. The title consumes the Win64 float return, so it
read zero or stale `xmm0` on every call.

## Call sites and IAT slots

`objdump -p game_scarlett_master.exe` pins the 64-bit IAT
(`Import Address Table Directory` RVA `0x6793490`, so VA `0x146793490`):

| name | IAT slot | calls (Step 77 run) | return address in the report |
|---|---|---|---|
| `erff` | `0x146794650` | 4,800 | `exe+0x1cb636f` |
| `log2f` | `0x1467946a0` | 129 | `exe+0x56a733a` |
| `atan2` | `0x146794618` | 84 | `exe+0x59d337` |
| `fmodf` | `0x146794680` | 52 | `exe+0x5189080` |

The three f32 names are one-argument or two-argument xmm0/xmm1 calls; the
report's return address is the instruction after the call.

- `erff` is the hot path. The function at `0x141cb62e0` keeps the pointer in a
  register (`mov r15, qword ptr [0x146794650]` at `0x141cb6338`) and calls it
  four times per element loop (`call r15` at `0x141cb636c` and
  `0x141cb637c`), subtracting a pair of `erff` results at `0x141cb637f`. The
  loop is float ( `vaddss`/`vmulss`/`vsubss`), so the ABI is one f32 in
  `xmm0`, one f32 out.
- `log2f`: `call 0x145f408c0` at `exe+0x56a7335`; that thunk is
  `jmp qword ptr [0x1467946a0]`. The argument is
  `[rbx] * [rip+0xabc36f]` (`vmulss`), and the result feeds a `vfmadd213ss`.
- `fmodf`: `call 0x145f40820` at `exe+0x518907b`; the thunk is
  `jmp qword ptr [0x146794680]`. Arguments are the f32 pair
  `[rsi+0x258]`, `[rsi+0x25c]`; the result is stored back to `[rsi+0x258]`.
- `atan2` is the double function, but the caller is float code. The helper at
  `0x140623b20` promotes both low-lane f32 arguments with `vcvtss2sd` and
  `jmp 0x145f40750`, which is `jmp qword ptr [0x146794618]`. The helper swaps
  the lanes accordingly, so the exporter's `(y, x)` order is the UCRT one.
  `atan2f` (slot `0x146794620`) was already real and stays untouched.

## Implementation

All four live in `hle/crt_linux.rs` next to the existing f32 math slice and are
registered at the end of `exports()`:

- `log2f(x)` is `f32::log2`. Contract: `log2f(1) = 0`, `log2f(0.5) = -1`,
  `log2f(±0) = -inf`, negative → NaN, `+inf → +inf`, and denormals keep their
  exponent (`log2f(2^-149) = -149`).
- `fmodf(x, y)` is Rust's float `%`, i.e. LLVM `frem` → libm `fmodf`: the
  result has the dividend's sign, `x % ±inf = x` for finite `x`, and
  `x % 0`, `inf % y`, NaN input → NaN.
- `erff(x)` is `erf_f64(x as f64) as f32`. The f64 core splits at `|x| = 2`:
  - below 2, the power series
    `2/sqrt(pi) * sum((-1)^n x^(2n+1)/(n!(2n+1)))`. It keeps full relative
    accuracy near zero, where `1 - erfc` would cancel; the term recurrence
    stops at `1e-18` relative.
  - from 2 up, `erfc(x) = Q(1/2, x^2)` through the modified-Lentz continued
    fraction (300 iterations, `1e-16` delta); `erf = 1 - erfc` is safe there
    because `erf >= 0.995`. Measured worst relative error against
    `math.erf` over 200k points in `[-8, 8]` is `1.1e-15`, and the two
    branches agree at the switch point.
  - IEEE edges: `±0 → ±0` (sign preserved), `±inf → ±1`, NaN → NaN.
- `atan2(y, x)` is `f64::atan2`, matching UCRT quadrants, signed zeros and
  infinities.

No new dependency; the series and continued fraction are local `f64` helpers.

## Tests (RED first)

`crt_linux::tests::erf_fmod_log2_and_atan2_are_real` resolves the four names
through `exports()` and transmutes them to `extern "win64"` signatures, so it
compiles on the parent tree and fails at the export-table assert first.
RED at `0a36dfd2` with only the test grafted: `erff must be wired`,
`0 passed / 1 failed`. GREEN at the fix: 1/0.

The erff reference table is `math.erf(x as f64)` for the exact f32 inputs
(including `1e-6` and `1e-3`, so a cancellation-prone shortcut fails), plus
`±0`, `±inf`, NaN and a `-6..6` step `0.01` monotonicity sweep. `fmodf` covers
signs, `±0`, infinite divisor/dividend and a zero divisor; `log2f` covers
0/1/inf/NaN/negative and the `2^-149` denormal; `atan2` covers all four
quadrants, `±0`, `x = +inf` and NaN.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step78-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step78-r1`):

- First present `3840x2160` at 6.906 s (Step 77: 6.902 s), 130 presents, no
  stall change.
- Exit 133 SIGTRAP at `game_scarlett_master.exe+0x2219651` (pool OOM assert,
  unchanged; not caused by the math change).
- `called_stubs` 39 → 35; `erff`, `log2f`, `fmodf`, `atan2` are gone, no new
  stub class. `log2` (double, 2 calls) and `_fdtest` (2) were already present
  in the Step 77 artifact and remain candidates for a later step.

## Semantics summary

| name | this branch | UCRT/IEEE eval |
|---|---|---|
| `erff` | f64 series ≤2, continued-fraction erfc above | `±0→±0`, `±inf→±1`, NaN→NaN |
| `fmodf` | float `%` (frem) | sign of dividend, `x%inf=x`, `x%0`/`inf%y`=NaN |
| `log2f` | `f32::log2` | `0→-inf`, negative→NaN, denormal exact exponent |
| `atan2` | `f64::atan2` | q1–q4, signed zeros, NaN propagation |
