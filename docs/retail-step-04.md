# M1 step-04 — runtime evidence: the dispatcher's scheme probes all miss

Date: 2026-11-27. Tip: `linux` @ `d84f8f4b`.
Depends on: `docs/retail-frontier.md` (step-01), `docs/retail-step-02.md`
(step-02). Task key:
`code-assist:launcher-first-frame:step-04:next-slice`.

## What this slice is

Step-02's pivot named the cheapest next evidence: runtime observation of the
`0x140968480` dispatch path — e.g. a temporary HLE `strncmp` argument log —
because grinding further static hops without runtime values was out of
bounds. This step builds exactly that: an env-gated `strncmp` tracer
(`XCL_TRACE_STRNCMP`, following the `XCL_TRACE_MEM` precedent in
`gpu/uma.rs`), plus the first traced retail run.

## Code (one logical change, `hle/crt_linux.rs` only)

- `strncmp` keeps exact UCRT semantics (loop restructured to a single
  `result`, verified by the pre-existing `string_compare_and_search_match_libc`
  test) and calls `trace_strncmp` on exit.
- Tracing is env-gated (`XCL_TRACE_STRNCMP` present = on, `OnceLock<bool>`,
  same idiom as `dumps_enabled`/`tracing`), capped at the first 512 calls
  per process (`AtomicUsize`, no locks — no deadlock-guard surface), each
  line carrying bounded arg previews (`min(n, 64)` bytes, NUL-terminated,
  lossy) plus `n` and the result. The preview never reads more than the
  `strncmp(a, b, n)` call itself reads.
- Failing-first test: `strncmp_trace_preview_renders_bounded_lossy_args`
  (cap, NUL stop, lossy bytes, zero length) — failed to compile
  (`E0425: cannot find function strncmp_arg_preview`) before the helper
  landed, passes after.
- Reversibility: default-off, zero behavior change when unset (control run
  below reproduces the frontier byte-for-byte in kind); the whole feature
  deletes cleanly if a better probe supersedes it.

## Bounded runs (fresh roots throughout)

Traced run:

```bash
XCL_DATA_ROOT=/tmp/xcl-step04-retail XCL_TRACE_STRNCMP=1 timeout 120 \
  ./target/debug/xcl run "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
```

- Exit **139 (SIGSEGV)** — same `exe+0x2237320` (`mov rax,[rbx]`, rbx NULL,
  fault `0x0`, bytes `48 8b 03 …`), `called_stubs` **20** (identical list),
  xgr exactly XAsync+XNetworking+XUser, `vfs.unmapped_paths` [], **0**
  d3d12/DXGI/PresentX hits. Frontier HOLDS.
- The whole pre-crash boot issues exactly **4** `strncmp` calls:

```
[STRNCMP] n=6 a="G:/app" b="tcpip:" -> -45
[STRNCMP] n=6 a="packro" b="tcpip:" -> -4
[STRNCMP] n=6 a="packro" b="tcpip:" -> -4
[STRNCMP] n=7 a="packroo" b="memory:" -> 3
```

Control run (`XCL_DATA_ROOT=/tmp/xcl-step04-ctrl`, var unset): exit 139,
same crash bytes, `called_stubs` 20, 0 d3d12 hits, **0** `[STRNCMP]` lines —
tracing is fully off by default.

## Reading (stated confidence, not overclaimed)

- Per step-02's static result, the only import on the
  `0xfc2240`/`0xfc2540`/`0x968490` bodies is `strncmp`, so these 4 calls —
  the only ones in the boot — are almost certainly the `0x96853b`
  dispatcher's scheme-prefix probes (high confidence on attribution by
  elimination; the log carries no caller address, so this is not proven).
- The probes classify a location string against URI-scheme-like keys:
  `tcpip:` (known from step-02's `.rdata` keys) and **`memory:` — new,
  not in step-02's static key list**. Candidates are `G:/app…` (n=6
  prefix; consistent with the `G:`-mapped game location, cf. the
  `_strnicmp` drive-prefix probe) and `packro…`/`packroo…` (n=6/7 prefixes).
  All four return nonzero — no scheme matches.
- This fits the NULL end-to-end: no scheme match → the dispatch builds the
  default object whose `+0x30` field stays 0 → predicate `0xfc2530`
  (`[rcx+0x30] != 0`) is false → producer `0xfc2490` takes the NULL-write
  branch (`0xfc2518`) → fault at `0x2237320`. The missing value is created
  inside game-built tables, as step-02 concluded — now with the runtime
  miss table to show for it.

## Verdict

**New evidence, no HLE slice yet.** The 4-probe miss table (`tcpip:` /
`memory:` vs `G:/app…` / `packro…`, all nonzero) is the runtime value
step-02 asked for, and `memory:` is a key static triage missed. But no stub
is implicated (`strncmp` is HLE-covered and correct; `called_stubs` is the
same benign 20), so there is still nothing to route or implement — PresentX
#30 / device #78–81 routing stays out (0 d3d12 hits re-confirmed), no
upstream port (notes not trawled, per M5).

Next (own task, not this step): the a-side prefixes are truncated by `n`
(`G:/app…`, `packro…`) and the log has no caller — capture full input
strings plus the dispatcher's input source (what feeds `0x968480`, and what
`memory:`/`packro…` mounts the game expects). That decides whether the miss
is a mount XCL should provide or a wrong input the game builds itself.

## Gates

- `cargo test -p xcl --lib strncmp_trace_preview` → fail-to-compile
  (`E0425`) before, pass after (failing-first shown).
- `cargo test -p xcl --lib string_compare…` + `crt_linux` → all pass
  (semantics preserved).
- `timeout 300 cargo check -p xcl --tests` → ok (pre-existing warnings only).
- `timeout 300 cargo check --workspace` → ok.
- `timeout 400 cargo test -p xcl --lib -- --test-threads=4` →
  **788 passed, 0 failed, 1 ignored** (+1 vs step-02/03: the new test).
- Title e2e (fresh root) → 4 passed + 1 failed by design; 24 `[FAIL]`
  lines = **12 unique, ALL `D3D12.X` GPU-only, 0 non-GPU** — baseline
  one-to-one, no regression.
- Retail traced + control runs (fresh roots, above) → frontier HOLDS,
  tracing default-off confirmed.
- Deadlock shape (`if let` + `.lock()` guard temporary): N/A — no locks
  added (`OnceLock` flag + `AtomicUsize` counter only; grepped the diff).
