# M1 step-05 — full dispatcher inputs: `G:/appdata0.rpf` + `packroot:/appdata0.rpf`

Date: 2026-11-27. Tip: `linux` @ `3261cb33` (step-04 commit).
Depends on: `docs/retail-frontier.md` (step-01), `docs/retail-step-02.md`
(step-02), `docs/retail-step-04.md` (step-04). Task key:
`code-assist:launcher-first-frame:step-05:dispatcher-inputs`.

## What this slice is

Step-04 named the dispatcher's scheme probes but logged only the `n`-byte
prefixes (`G:/app…`, `packro…`) and only the `strncmp` half of the
dispatcher (`0x140968480` issues most of its probes through `_strnicmp`,
IAT slot `0x1467947b8`). This step extends the probe to full NUL-terminated
inputs on both shims and runs the first fully-instrumented traced boot.

## Code (one logical change, `hle/crt_linux.rs` only)

- New `strncmp_cstr_full` (cap `STRNCMP_FULL_MAX` = 256, NUL-stopped, lossy):
  the traced log now carries `a_full`/`b_full` beside the step-04 `n`-byte
  prefixes. It reads past the `n` bytes the call itself compares — sound
  only for the NUL-terminated inputs the title passes (the dispatcher runs a
  `strlen`-like first; the Havok-style keys are C strings) — and runs only
  under the triage env var (the default-off path never reads past `n`).
- New `trace_strnicmp` under the same `XCL_TRACE_STRNCMP` gate with its own
  `STRNICMP_LOGGED` 512-call cap (same `OnceLock` flag + atomic-counter
  idiom, no locks); `_strnicmp` restructured to a single `result` (semantics
  preserved, covered by the pre-existing `string_compare_and_search_match_libc`
  drive-prefix cases).
- Failing-first test: `strncmp_full_reports_beyond_n` — failed to compile
  (`E0425: cannot find function strncmp_cstr_full` / `STRNCMP_FULL_MAX`)
  before the helper landed, passes after.
- Reversibility: default-off, zero behavior change when unset (control run
  below reproduces the frontier with 0 probe lines); the whole feature deletes
  cleanly.
- Deadlock shape (`if let` + `.lock()` guard temporary): N/A — no locks
  added (atomics + `OnceLock` only; grepped the diff).
- Deliberately NOT in this step: per-call game call-site logging. A first
  revision scanned stack words for mapped-image addresses, but two live
  retail runs proved that misattributes: the win64 home space spills the key
  pointer onto our stack ahead of any return address, and HLE trampoline
  frames add more spilled data words, so the "caller" field reported
  `.rdata`/`.data` operands, not code. Real unwind PCs would need
  `Backtrace::frames` (unstable on this toolchain) or a new unwind dependency
  for a default-off probe. Dropped in favor of static `(n, key)`→site mapping
  (below), which answers the same question. Future iterations: do not re-add
  stack-word caller attribution without executable-range tracking.

## Bounded runs (fresh roots throughout)

Traced run:

```bash
XCL_DATA_ROOT=/tmp/xcl-step05-retail XCL_TRACE_STRNCMP=1 timeout 120 \
  ./target/debug/xcl run "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
```

- Exit **139 (SIGSEGV)** — same `exe+0x2237320` (`mov rax,[rbx]`, rbx NULL,
  fault `0x0`, bytes `48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b`),
  `called_stubs` **20** (identical list), xgr exactly XAsync+XNetworking+XUser,
  `vfs.unmapped_paths` [], **0** d3d12/DXGI/PresentX hits. Frontier HOLDS.
- **182** probe lines (3 `[STRNCMP]` tcpip + 1 `[STRNCMP]` memory: + 178
  `[STRNICMP]`). Full inputs quoted verbatim:

```
[STRNCMP] n=6 a="G:/app" a_full="G:/appdata0.rpf" b="tcpip:" b_full="tcpip:" -> -45
[STRNCMP] n=6 a="packro" a_full="packroot:/appdata0.rpf" b="tcpip:" b_full="tcpip:" -> -4
[STRNCMP] n=6 a="packro" a_full="packroot:/appdata0.rpf" b="tcpip:" b_full="tcpip:" -> -4
[STRNCMP] n=7 a="packroo" a_full="packroot:/appdata0.rpf" b="memory:" b_full="memory:" -> 3
[STRNICMP] n=2 a="G:" a_full="G:/appdata0.rpf" b="g:" b_full="g:" -> 0
[STRNICMP] n=3 a="G:/" a_full="G:/appdata0.rpf" b="xg:" b_full="xg:" -> -17
[STRNICMP] n=3 a="G:/" a_full="G:/appdata0.rpf" b="xd:" b_full="xd:" -> -17
[STRNICMP] n=3 a="pac" a_full="packroot:/appdata0.rpf" b="dlc" b_full="dlc" -> 12
[STRNICMP] n=8 a="packroot" a_full="packroot:/" b="packroot" b_full="packroot:/appdata0.rpf" -> 0
```

Control run (`XCL_DATA_ROOT=/tmp/xcl-step05-ctrl`, var unset): exit 139,
same crash bytes, `called_stubs` 20, **0** probe lines — tracing is fully
off by default.

## Reading (stated confidence, with disassembly refs)

The dispatcher's two boot inputs are **`G:/appdata0.rpf`** and
**`packroot:/appdata0.rpf`** — full strings, no longer prefixes. Neither
byte sequence occurs anywhere in the exe file: both locations are
**runtime-built** (heap), assembled during boot, not static mount paths.

Per-input probe tables (all nonzero unless noted), each `(n, b_full)` mapping
to a unique `lea rdx,[rip+…]` site (checked by byte search + disassembly):

- `G:/appdata0.rpf`: `tcpip:`/6 ≠0 (`0x14096852b`); `globalroot`/10 sliding
  suffix scan ≠0 (inside `0x14003bd50`, the transform the dispatcher calls at
  `0x140968553` — a strstr-like loop over input offsets); `t:`/2,
  `\??\T:`/6, `/??/T:`/6, `\?\T:`/5, `/?/T:`/5 all ≠0 (`0x140968572…0x140968626`
  via slot `0x1467947b8`); **`g:`/2 == 0** (`0x14096860c`) → dispatches to the
  `0x141d8e470` table (`0x1478f5e80`). Then the drive-prefix pair `xg:`/`xd:`/3
  ≠0 at **`exe+0x54F8A70`** (`0x1454f8a7c`/`0x1454f8a96` via the same slot) —
  exactly the dump-path site the `_strnicmp` doc comment names; the game
  continues past both.
- `packroot:/appdata0.rpf` (×2 dispatcher passes + 1 more tcpip probe): same
  chain through `g:` ≠0, `R:`/2 ≠0 (`0x140968626`), then `dlc`/3 ≠0 (site
  `0x1453525e8`, same `0x140643cd0`-family input builder as the `0xfc2240`
  chain) and the scheme self-match `packroot`/8 == 0 (a=`packroot:/`, b the
  full location): the game confirms the `packroot` scheme, then still builds
  the default object.
- `memory:`/7 ≠0 against `packroot:/appdata0.rpf`: the key at `0x146788821`
  is the Havok rigid-body attr name (`elasticity:\0memory:\0inertiaBox:…`,
  12 code `lea` sites); the comparing call is one of the `0x1428c7xxx`
  shape-name matchers near the strip-prefix helper — a different table
  walk over the same location string, not the dispatcher.
- Ambient (benign, proves probe reach): 85× `_strnicmp`/25 sliding the host
  exe path (`/run/media/…/game_scarlett_master.exe`) past
  `+randomDebugMemoryFilling` — the game's own argv flag scan, boot continues.

Input-source chain (static, step-02 §`0xfc2240` extended): the dispatcher
call at `0x140fc2398` takes `rcx` = `[rsi+0x38]` (else the `0x14678d1bb`
telemetry default), which is composed by `0x140643cd0` + `0x14056eb30`
(dump-path formatter over the `D:/LocalDumps` wide strings) + the factory's
`rdi`, then `0x14079e350` + `0x1428cd1a0` — i.e. the location tokens flow
out of the game's own path-composition code, and every probe above resolves
them against game-internal tables. `tcpip:` is the one ambiguous key (second
site: the `0x1428c63c0` strip-prefix helper, ret `0x1428c63df` vs dispatcher
ret `0x140968541`); it does not affect the verdict since both sites consume
the same inputs.

## Verdict

**New evidence, no HLE slice.** The full inputs (`G:/appdata0.rpf`,
`packroot:/appdata0.rpf`, runtime-built) plus the per-key probe tables answer
step-04's Next: the "missing" value is not a mount XCL can provide — no
scheme matches except the in-game `g:` drive alias, `vfs.unmapped_paths` is
empty, zero storage/XVD imports, and the NULL still comes from the game's own
validity gate (`0xfc2530`, `[rcx+0x30] != 0`) on the default-branch object the
dispatcher builds for these locations. `strncmp`/`_strnicmp` are HLE-covered
and correct; no stub is implicated, so PresentX #30 / device #78–81 routing
stays out (0 d3d12 hits re-confirmed) and no upstream port is owed.

Pivot (own task, not this step): find what the default-table object's
`[rax+0x108]`/`[rax+0xf8]` vtable targets read for `packroot:`/`G:` inputs —
i.e. why field `+0x30` stays 0. Cheapest next evidence is a targeted probe at
that writer (extend the tracer to those two vtable slots, or a gdb watchpoint
on the `[rsi+0x30]` store), not another boot-wide log.

## Gates

- `cargo test -p xcl --lib strncmp_full` → fail-to-compile (`E0425`) before
  the helper landed, pass after (failing-first shown).
- `cargo test -p xcl --lib string_compare…` + all `crt_linux` →
  21 passed (semantics preserved, incl. the `G:/` vs `xg:`/`xd:` drive cases).
- `timeout 300 cargo check -p xcl --tests` → ok (pre-existing warnings only).
- `timeout 300 cargo check --workspace` → ok.
- `timeout 400 cargo test -p xcl --lib -- --test-threads=4` →
  **789 passed, 0 failed, 1 ignored** (+1 vs step-04: the new test).
- Title e2e (fresh root, `cargo test -p xcl --test e2e`) → 4 passed + 1 failed
  by design; 12 unique `[FAIL]`, ALL `D3D12.X` GPU-only, 0 non-GPU —
  baseline one-to-one, no regression.
- Retail traced + control runs (fresh roots, above) → frontier HOLDS,
  tracing default-off confirmed (182 vs 0 probe lines).
- Deadlock shape: no locks added (atomics + `OnceLock` only; grepped).
