# Upstream sync log (linux branch)

Durable in-repo mirror of the hourly upstream-sync scans. The primary
triage archive lives at `/home/anon/xcl-notes/upstream-prs/` (`index.md`
first); this file records what each scan verified so a stale triage tag
never sends a future session to re-port landed work. One entry per scan;
newest at the bottom.

## 2026-11-26 — step-113 (task-1795712219-0eef): scan empty, stale tags corrected

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Per-PR sequential probe `refs/pull/$i/head` #2832..#2850 + spot
  #2855/#2860/#2865: #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` (SHAs match the step-109-fix pins, all three still
  OPEN), #2835+ ALL MISSING — scan complete.
- Triage: nothing new. #2832 SKIP stands (d3d12x-only `pso_async`,
  zero Linux twin refs); #2833/#2834 already landed in HEAD.
- Stale-tag corrections (verified against the tree this scan; the
  triage `index.md` "Most actionable" section carried pre-landing text):
  - #1143 LANDED `cae046e5` (`contiguous_home_in` in `kernelx_linux.rs`).
  - #1705 LANDED `f0f9ea34` (poisoned locks taken + minimal report).
  - #1610 LANDED `27d80dbd` (Linux COM identity for XCL vtable objects).
  - #2136/#2148/#2134 LANDED `a3dec47f`/`c2a1dd36`/`dda30f62`
    (fidelity 119/119 + 172/172 + 118/118 vs live heads, step-112 audit).
    Prior LATER tag superseded — fixes are live on the Linux-run
    offline recomp path (`xcl shaders --recomp`).
  - K-D3D-01/02 LANDED `e1bec690` (#2499 watchdog) + `52e9014e`
    (dispatch waits at submit, not bind; arrived via upstream sync).
  - Still OPEN: K-D3D-06 (measured console slot map checklist).
- Gates on HEAD `a8ce8887` (doc-only change): both `cargo check`s
  0 errors; lib 780 passed / 0 failed / 1 ignored; title e2e 4 passed,
  1 failed by design, 12 unique FAILs all D3D12.X GPU-only, 0 non-GPU;
  retail fresh-root SIGSEGV `exe+0x2237320` stubs 20 xgr exactly
  XAsync/XNetworking/XUser — HOLDS.

## 2026-11-26 — step-114 (task-1795712511-e241): scan found #2836, triaged LATER-by-shape

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout (same tip as steps 110–113).
- Per-PR sequential probe `refs/pull/$i/head` #2832..#2900 (widened past
  the first gap — see lesson): #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` (SHAs match the step-109-fix pins, all three still
  OPEN), #2835 MISSING, **#2836 `43a4eb9d` EXISTS (still OPEN)**,
  #2837..#2900 ALL MISSING — scan complete.
- CORRECTION to step-113 ("#2835+ ALL MISSING"): the probe stopped at the
  first missing ref and missed #2836 behind the gap. Lesson recorded as
  `mem-1795712765-78dc`: never stop a sequential probe at the first
  missing ref — always sweep the whole window, because PR numbers gap
  (#2835 missing while #2836 exists).
- #2836 triage (fresh hunk evidence, `upstream/pr-2836` vs merge-base):
  "XGameRuntime: drain active task queue monitors on unregister" —
  `Monitor` gains `Mutex<MonitorState>` + `Condvar`, `invoke`/`revoke`
  with a `MONITOR_DEPTH` thread-local reentrancy guard, `Queue.monitors`
  becomes `Vec<Arc<Monitor>>`, unregister waits for in-flight callbacks
  and skips revoked snapshot entries, +3 tests (~140 lines). Real race
  fix on a Linux-live path — but CANNOT land verbatim: our tree diverged
  via #2021 (`d1ce58c4`, temp queue handles on shared ports), so our
  `Monitor` has no `queue_handle` field
  (`hle/xgameruntime/taskqueue.rs:55-60`,
  `hle/xtaskqueue_linux.rs:57-62`) and `notify_monitors` is `Port`-level
  fan-out with synthesized temp handles (`taskqueue.rs:87-125`,
  `xtaskqueue_linux.rs:89-127`), not upstream's `Queue::notify_monitors`.
  The upstream hunks' context (struct literal with `queue_handle`,
  `Queue::notify_monitors`) does not exist here. Verdict: LATER —
  needs an adaptation design (Arc+state monitors, invoke-with-handle
  param, revoke/wait, remove-if-drained, in BOTH twins; Linux-live twin
  is `xtaskqueue_linux.rs` via `hle_linux.rs:32-33,225`), not a port.
  Seeded as `m5:step-115` (`task-1795712765-8368`), failing-first.
- #2832 SKIP stands (d3d12x-only `pso_async`, zero Linux twin refs);
  #2833/#2834 already landed in HEAD.
- Gates on HEAD `3879a621` (doc-only change): both `cargo check`s
  0 errors (22 lib + 28 lib-test pre-existing warnings); lib 780 passed
  / 0 failed / 1 ignored; title e2e 4 passed, 1 failed by design, 12
  unique FAILs all D3D12.X GPU-only, 0 non-GPU; retail fresh-root
  SIGSEGV `exe+0x2237320` stubs 20 xgr exactly
  XAsync/XNetworking/XUser — HOLDS.

## 2026-11-26 — step-115 (task-1795712765-8368): adapted #2836 monitor-drain to Port-fan-out shape, both twins

- Upstream #2836 `43a4eb9d` (still OPEN) re-verified pre-commit: fetch
  shows the same single-commit head; base `upstream/main` still
  `8b957bec`, is-ancestor HEAD exit 0 — ZERO sync fallout.
- Adaptation (verbatim impossible — #2021 shape divergence stands):
  `Monitor` (+`MonitorState{revoked,running}` +`idle` Condvar
  +`MONITOR_DEPTH` guard, `invoke(handle,port)->drained`,
  `revoke()->drained`) in BOTH `hle/xgameruntime/taskqueue.rs`
  (Windows twin, `extern "system"`) and `hle/xtaskqueue_linux.rs`
  (Linux-live twin, `extern "win64"`); `Queue.monitors` becomes
  `Vec<Arc<Monitor>>` + `remove_monitor`; `Port::notify_monitors`
  keeps its fan-out (temp handle per monitor, terminated checks) and
  drops drained registrations via `invoke`; register pushes
  `Arc<Monitor>`; unregister clones-then-revokes without holding the
  queue lock, removes when drained. `XTaskQueueTerminate`'s `clear()`
  unchanged (registry refs drop; in-flight snapshot Arcs survive).
- Failing-first: 3 upstream tests ported per twin (adapted to temp
  handles + per-twin ABI). Linux twin pre-fix: 2 failed as predicted
  (`waits_for_an_active_invocation` returned before release;
  `skips_a_revoked_snapshot_entry` ran the revoked monitor),
  1 passed as regression pin (self-unregister); post-fix 3/3 green.
  Windows-twin tests mirror them but cannot execute on this Linux box
  (module not compiled here) — rustfmt parse-verified only, noted in
  the commit message.
- Gates: both `cargo check`s 0 errors (22 lib + 28 lib-test
  pre-existing warnings); lib 783 passed / 0 failed / 1 ignored (+3
  new monitor tests); title e2e 31 passed / 19 failed with FAIL+PASS
  sets byte-identical to the stashed baseline (bare-folder
  files:/argv FAILs are environmental, equal both sides);
  `target/debug/xcl` symbol-verified to contain
  `xtaskqueue_linux::Monitor::{invoke,revoke}` +
  `Queue::remove_monitor`; retail fresh-root exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20 (all
  first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS. Lock
  audit: 0 new `lock()` held across an `if let` body (named bindings
  + statement-local temporaries only); `diff --check` clean.

## 2026-11-26 — step-116 (task-1795713518-79a3): scan found #2837, SKIP (Windows-only test CI fix on an aggregate fork stack)

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Per-PR sequential probe `refs/pull/$i/head` #2832..#2910 (widened past
  the first gap per mem-1795712765-78dc): #2832 `e2ecdfa9` / #2833
  `f5a352c3` / #2834 `653f8f13` / #2836 `43a4eb9d` (all pins match,
  all four still OPEN — `merge-base --is-ancestor <head>
  upstream/main` exit 1 each); #2835 MISSING; #2837 `9d967cf8` EXISTS
  (OPEN, exit 1 — NEW since step-114, which spot-probed it MISSING);
  #2838..#2910 ALL MISSING — scan complete.
- #2837 = "Tests: sparse tests take CI's Basic Render Driver for a
  software device" (DzXAnt22, tip `9d967cf8`). Verdict SKIP, two
  independent reasons: (a) the tip commit itself is 3+/2- inside
  `mod tests` in `hle/d3d12x/sparse.rs` (`software()` helper gains a
  `(VendorId, DeviceId) == (0x1414, 0x8c)` clause for WARP/Basic Render
  Driver) — Windows-only (`windows::Win32::Graphics::Dxgi`,
  `ID3D12Device`), test-only, CI-only; `hle/d3d12x/*` never compiles
  on Linux (Linux-live is `d3d12x_linux.rs` null/Vulkan), so there is
  no Linux consumer and no Linux twin to mirror. Hunk context absent
  in our tree: our `sparse.rs` (unified-memory sparse-windows design)
  has `mod tests` at line 1865 but no `software()` fn and no
  `0x1414`/Basic-Render-Driver refs (grep exit 1/0 hits for the
  negative/positive halves). (b) The PR branch as a whole is an
  88-file aggregate fork work stack (`8b957bec..9d967cf8`: +33541/-3546
  across `wm17/prs` + `nvidia-dev` merges) — not one logical change,
  so verbatim port is out; any portable pieces arrive as focused
  upstream PRs and get triaged then. #2832 SKIP stands (still zero
  `pso_async`/`need_by_submit` refs on the Linux path);
  #2833/#2834 landed; #2836 adapted in `7645afeb`.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design, 24 `[FAIL]` lines = 12 unique, all `D3D12.X`,
  0 non-GPU; retail fresh-root `XCL_DATA_ROOT=/tmp/xcl-step116-r`
  exit 139 SIGSEGV `exe+0x2237320` fault `0x0` bytes `48 8b 03`
  stubs 20 (all first_args) xgr exactly XAsync/XNetworking/XUser —
  HOLDS (report.json verified, fresh root removed after). Lock audit:
  0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-117 (task-1795713741-13e2): ported #2838 (G16 packed gradients)

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Per-PR sequential probe `refs/pull/$i/head` #2832..#2920 (widened past
  the first gap per mem-1795712765-78dc): #2832 `e2ecdfa9` /
  #2833 `f5a352c3` / #2834 `653f8f13` / #2836 `43a4eb9d` /
  #2837 `9d967cf8` / #2838 `ef36b7df` (all pins match, all still OPEN,
  `merge-base` exit 1 each); #2835 MISSING; #2839..#2920 ALL MISSING —
  scan complete. Race note: #2838's head commit is 2026-11-26 17:13:59 UTC
  but step-116 (window ending ~17:22 UTC) probed it MISSING — a push race,
  not builder fault (per mem-1795714025-21a1); the step-117 probe caught it.
- #2838 = "Recompiler: decode packed G16 gradients in direct DXIL"
  (single-commit `ef36b7df`, +162/-2: `lower_image.rs` G16 gradient-shape
  fix + `sample_grad_program` helper + 3 tests in `recomp/tests.rs`).
  Verdict KEEPER, verbatim-ported in this step: the fix hunk's context
  (`for _ in 0..2 * grad_n { shape.push((32, true)); }`) is byte-identical
  in our tree (`lower_image.rs:990-993`); `g16` detection via
  `mods.contains(&"g16")` works unchanged (`base = "sample_d_g16"` ->
  `mods = ["d", "g16"]`); downstream `compf(i_grad + k)` /
  `compf(i_grad + grad_n + k)` indexing is preserved (dx block then dy
  block). All four additions verified byte-identical to upstream head
  (`git apply --check` clean, post-apply `awk`-extract diff empty).
  Failing-first (static, Windows-gated): the non-GPU test
  `direct_sample_grad_g16_widens_packed_gradients` asserts
  `legacyF16ToF32` counts of 3/4/7/9 for G16 cases; pre-fix code emits
  only `(32, true)` gradient slots so the count is 0 — fails pre-fix by
  construction. The `recomp/tests.rs` module is
  `#[cfg(all(test, windows))]` (as are all dxil lowering test modules),
  so the new tests cannot execute on this Linux box: ported verbatim,
  `rustfmt`-parse-verified (remaining `--check` diffs are upstream's own
  over-length lines, kept verbatim per the no-reformat rule), execution
  waits for Windows CI. The `lower_image.rs` fix itself is Linux-live
  platform-independent code (only `VIEW_HEAP_MAX_SRV` is cfg-gated) and
  is compiled + regression-covered by the Linux gates below.
  #2832 SKIP stands; #2833/#2834 landed; #2836 adapted in `7645afeb`;
  #2837 SKIP stands (step-116 evidence re-verified: still OPEN).
- Gates (fix + tests): both `cargo check`s 0 errors (22 lib + 28 lib-test
  pre-existing warnings); lib 783 passed / 0 failed / 1 ignored
  (`--test-threads=4`; count correctly unchanged — new tests are
  Windows-gated); title e2e 4 passed / 1 failed by design
  (`test_title_runs_and_game_folder_is_untouched`), 24 `[FAIL]` lines =
  12 unique, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step117-r` exit 139 SIGSEGV `exe+0x2237320`
  fault `0x0` unmapped bytes `48 8b 03` stubs 20 (all first_args) xgr
  exactly XAsync/XNetworking/XUser — HOLDS (report.json verified, fresh
  root removed after; binary mtime verified newer than the source edit).
  Lock audit: 0 new `lock()` in the added code; `diff --check` clean.

## 2026-11-26 — step-118 (task-1795714398-8c9c): empty scan, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Per-PR sequential probe `refs/pull/$i/head` #2832..#2930 (widened past
  the first gap per mem-1795712765-78dc): #2832 `e2ecdfa9` /
  #2833 `f5a352c3` / #2834 `653f8f13` / #2836 `43a4eb9d` /
  #2837 `9d967cf8` / #2838 `ef36b7df` (all pins match, all still OPEN,
  `merge-base` exit 1 each); #2835 MISSING; #2839..#2930 ALL MISSING —
  scan complete. Nothing new beyond #2838 (ported in `aa59e8ac`).
- Triage: nothing new. #2832 SKIP stands (d3d12x-only `pso_async`,
  zero Linux twin refs); #2833/#2834 landed; #2836 adapted in `7645afeb`;
  #2837 SKIP stands (Windows-only Dxgi/ID3D12Device test-only CI fix);
  #2838 landed verbatim in `aa59e8ac`.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design, 24 `[FAIL]` lines = 12 unique, all `D3D12.X`,
  0 non-GPU; retail fresh-root `XCL_DATA_ROOT=/tmp/xcl-step118-r`
  exit 139 SIGSEGV `exe+0x2237320` fault `0x0` bytes `48 8b 03`
  stubs 20 (all first_args) xgr exactly XAsync/XNetworking/XUser —
  HOLDS (report.json verified, fresh root removed after). Out-of-gate
  note: `launcher_e2e` shows the same 2 pre-existing platform-bug
  failures as step-117 (`only_xcl_copy` asserts `xcl.exe`, Linux emits
  `xcl`; plus `unreadable_shader_pack`) — untouched by this step.
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-119 (task-1795714798-9e4f): empty scan, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Per-PR sequential probe `refs/pull/$i/head` #2832..#2950 (widened past
  the first gap per mem-1795712765-78dc): #2832 `e2ecdfa9` /
  #2833 `f5a352c3` / #2834 `653f8f13` / #2836 `43a4eb9d` /
  #2837 `9d967cf8` / #2838 `ef36b7df` (all pins match, all still OPEN,
  `merge-base` exit 1 each); #2835 MISSING; #2839..#2950 ALL MISSING —
  scan complete. Nothing new beyond #2838 (ported in `aa59e8ac`).
- Triage: nothing new. #2832 SKIP stands (d3d12x-only `pso_async`,
  zero Linux twin refs); #2833/#2834 landed; #2836 adapted in `7645afeb`;
  #2837 SKIP stands (Windows-only Dxgi/ID3D12Device test-only CI fix);
  #2838 landed verbatim in `aa59e8ac`.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines = 12 unique, all `D3D12.X`, 0 non-GPU; retail
  fresh-root `XCL_DATA_ROOT=/tmp/xcl-step119-retail` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` unmapped bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after). Note: a first retail
  attempt with a `--data-root` CLI flag wrote nothing to the temp root
  (flag is not the launcher convention) — redone with `XCL_DATA_ROOT`
  per the launcher contract; the stray empty temp root was removed.
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-120 (task-1795715310-4af0): #2830/#2831/#2842 triaged, all SKIP

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) + per-PR sequential probe #2830..#2970 (widened
  past the first gap per mem-1795712765-78dc): #2830 `069292ca` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` (all pins match, all still OPEN,
  `merge-base --is-ancestor` exit 1 each); #2835, #2839..#2841,
  #2843..#2970 ALL MISSING — scan complete. #2830/#2831 sit below the
  prior #2832 window floor and were never triaged before; #2842 is the
  step-119 critic handoff (push-race per mem-1795714025-21a1 stands:
  head 2026-11-26 17:42:11 UTC vs the step-119 builder window).
- Triage: nothing portable. #2830 SKIP ("Shaders: stop startup shader
  preparation promptly when cancelled", single-file +202/-13 in
  `shaders.rs`): `Pipelines`, `make_with_cache_path[_cancel]`,
  `make_one`, the real `precompile`, and both new tests are all
  `#[cfg(windows)]` (stub `precompile` on `not(windows)`); no Linux
  consumer. Hunk context diverged anyway — our `make_one` carries an
  extra `record: Option<u64>` param the PR base lacks
  (`shaders.rs:1594`). #2831 SKIP ("D3D12.X: keep allocator recordings
  held on device removal", +28/-6 in `hle/d3d12x/alloc_pool.rs`):
  Windows-D3D12-only device-loss semantics, no Linux consumer.
  #2842 SKIP ("D3D12.X: a timed-out readback_now keeps its job until
  the GPU copy has run"): root-commit aggregate fork snapshot (371
  files); the `IN_FLIGHT_RB_JOBS` / `take_readback_job` /
  `FREE_RB_JOBS` pool infra is absent from our tree (our
  `readback_now` at `sparse.rs:1342` is the simpler pool-less shape) —
  hunk context absent, Windows-D3D12-only. Earlier pins stand: #2832
  SKIP; #2833/#2834 landed; #2836 adapted in `7645afeb`; #2837 SKIP
  stands; #2838 landed verbatim in `aa59e8ac`.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines = 12 unique, all `D3D12.X`, 0 non-GPU; retail
  fresh-root `XCL_DATA_ROOT=/tmp/xcl-step120-retail` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-121 (task-1795716023-7d79): #2843 triaged SKIP, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) + per-PR sequential probe #2830..#3040 (widened
  past the first gap per mem-1795712765-78dc): #2830 `069292ca` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` / #2843 `932e689c` (all pins
  match, `merge-base --is-ancestor` exit 1 each; merge refs present for
  all except #2832, which has no merge ref — still unmerged, not in
  main); #2835, #2839..#2841, #2844..#3040 ALL MISSING — scan complete.
  #2843 is new since step-120 (single commit "D3D12.X: title fence sync
  entries own their ID3D12Fence (#2840)", head 2026-11-26 18:05:17 UTC,
  parent = `upstream/main` `8b957bec` — a fresh PR, not a push-race).
  The "(#2840)" in its title is an issue reference: `refs/pull/2840`
  remains MISSING.
- Triage: nothing portable. #2843 SKIP ("D3D12.X: title fence sync
  entries own their ID3D12Fence", 2 files, +185/-20 in
  `hle/d3d12x/list_x.rs` + `hle/d3d12x/real.rs`): `TitleFenceSync`
  takes its own `ID3D12Fence` reference via the `windows` crate so
  `prune_title_fence` never calls `GetCompletedValue` on a title-freed
  fence; synthetic-COM-fence tests fail without the owned reference.
  Windows-D3D12-only — `d3d12x` never compiles on Linux (`hle_linux.rs`
  swaps in the null `d3d12x_linux` + Vulkan; zero `title_fence` /
  `TitleFenceSync` / `SYNC_GRAPH` refs in `d3d12x_linux.rs` /
  `hle_linux.rs`), no Linux consumer. Hunk context is the old shape in
  our tree (`prune_title_fence(fence_raw, tf)` at `list_x.rs:1418`,
  pool-less entry construction) but there is nothing to port it to.
  Earlier pins stand: #2830/#2831/#2832 SKIP; #2833/#2834 landed;
  #2836 adapted in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim
  in `aa59e8ac`.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step121-retail` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-122 (task-1795716845-3db8): #2844/#2845/#2846/#2847/#2848 triaged, all SKIP, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) + per-PR sequential probe #2830..#2875 (widened
  past the second gap per mem-1795712765-78dc): #2830 `069292ca` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` / #2843 `932e689c` /
  #2844 `25dfc2c0` / #2845 `76b17593` / #2846 `a4421e06` /
  #2847 `4b1780e8` / #2848 `88e56f93` (all pins match batched +
  sequential, `merge-base --is-ancestor` exit 1 each — all still OPEN,
  unmerged); #2835, #2839..#2841, #2849..#2875 ALL MISSING — scan
  complete. #2844 is the step-121 critic handoff (push-race per
  mem-1795714025-21a1 stands: 2-commit stack `b84e0378` +
  `25dfc2c0`, merge ref `adbc9a65` present); #2845/#2846/#2847/#2848
  are NEW since (single commits, each parented on `upstream/main`
  `8b957bec` — fresh PRs, not push-races; the re-fetched batched
  listing tops out at #2848, and the sequential tail #2849..#2875 is
  empty).
- Triage: nothing portable. #2844 SKIP ("D3D12.X: coherent UMA windows
  in a driver heap at the title's address on NVIDIA", 5 files,
  +508/-42: `gpu/uma.rs` +25, `hle/d3d12x/mod.rs` +1,
  `real.rs` +24, `title_textures.rs` +290, new `uma_driver.rs` +210):
  the `gpu/uma.rs` hunk (new `driver: AtomicU32` bitmask,
  `set/is/has_driver_window`, `watch_regions` skipping driver
  windows) is inert on Linux — every producer/consumer lives in
  `hle/d3d12x` (`uma_driver::wanted/remake` needs `ID3D12Device` +
  `windows`/`windows_sys` + kernel32 `GetWriteWatch` + NVIDIA PCI
  vendor check; `real.rs` `open_uma` remake path; `title_textures`
  `queue_driver_window_changes`), which never compiles on Linux
  (`lib.rs` swaps `hle` for `hle_linux.rs`: null `d3d12x_linux` +
  Vulkan; zero `uma_driver` / `set_driver_window` /
  `is_driver_window` / `has_driver_windows` refs in
  `d3d12x_linux.rs` / `hle_linux.rs` / `gpu/uma.rs`). With `driver`
  stuck at 0 the `watch_regions` branch is a no-op. Our `uma.rs`
  base shape matches (`pool` at `:118`, `watch_regions` at `:582`).
  #2845 SKIP ("D3D12.X: clear the preparation screen when the
  presentation window is missing", +48/-3 in
  `hle/d3d12x/present/preparation.rs`): `activate()` refactor + 2
  tests around `ensure_window`/HWND; `hle/d3d12x` never compiles on
  Linux (zero `preparation`/`ensure_window` refs in `host_linux.rs` /
  `d3d12x_linux.rs`). COMPETES with #2847 (same file, same
  missing-window bug — land-once rule §3, and both SKIP anyway).
  #2846 SKIP ("D3D12.X: clear Flags in null heap GetDesc", +15/-3 in
  `hle/d3d12x/null.rs`): `Hidden_ZeroDesc40` → `Hidden_ZeroDesc48` +
  test; the Windows null backend only (Linux uses
  `d3d12x_linux.rs`); our base shape matches (`Hidden_ZeroDesc40`
  at `null.rs:178,287`). #2847 SKIP ("D3D12.X: clear ACTIVE/SCREEN
  when preparation window creation fails", +8/-1, same file as
  #2845): inline match + clear, no tests — the narrower twin of
  #2845's fix; Windows-only per #2845. #2848 SKIP ("Shaders: key
  DXIL toolchain identity off DLL bytes, not mtime", +9/-6 in
  `gpu/recomp/dxc.rs` `toolchain_identity`): `pub mod dxc` is
  `#[cfg(windows)]` (`recomp/mod.rs:56-57`); the DXIL cache identity
  (`dxcompiler.dll`/`dxil.dll` bytes, `XCL_DXIL_CACHE` /
  `%LOCALAPPDATA%`) has no Linux reader — Linux keys its shader
  disk cache on translator-source hash (`build.rs`) and emits
  SPIR-V. Our base shape matches (mtime-based code at
  `dxc.rs:657`). Earlier pins stand: #2830/#2831/#2832 SKIP;
  #2833/#2834 landed; #2836 adapted in `7645afeb`; #2837/#2842 SKIP;
  #2838 landed verbatim in `aa59e8ac`; #2843 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step122r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-123 (task-1795717751-4372): #2849 triaged SKIP, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) tops out at #2848 (`88e56f93`) — NOTHING past
  #2848 in the batched listing (older #2826–#2829 refs present below
  the window). Per-PR sequential probe #2830..#2900 (widened past the
  third gap per mem-1795712765-78dc) finds #2849 `4e93173` PRESENT —
  the sequential scan again catches what the batched listing missed
  (same shape as step-122, where sequential caught #2846–#2848 after
  batched topped out at #2845): #2830 `069292ca` / #2831 `965806a0` /
  #2832 `e2ecdfa9` / #2833 `f5a352c3` / #2834 `653f8f13` /
  #2836 `43a4eb9d` / #2837 `9d967cf8` / #2838 `ef36b7df` /
  #2842 `344ce7b5` / #2843 `932e689c` / #2844 `25dfc2c0` /
  #2845 `76b17593` / #2846 `a4421e06` / #2847 `4b1780e8` /
  #2848 `88e56f93` / #2849 `4e93173` (all pins match batched +
  sequential, `merge-base --is-ancestor` exit 1 each — all still OPEN,
  unmerged); #2835, #2839..#2841, #2850..#2900 ALL MISSING — scan
  complete. #2849 is NEW since step-122: single commit parented on
  `upstream/main` `8b957bec`, head stamped 15:31:38 -0300 (= 18:31
  UTC, minutes into this scan window) — a fresh PR that appeared
  mid-window, not a push-race on an older ref.
- Triage: nothing portable. #2849 SKIP ("D3D12.X: copy title bytes
  instead of borrowing in texture workers", 1 file
  `hle/d3d12x/title_textures.rs` +7/-3): `title_bytes` borrows only
  inside a UMA committed window now
  (`crate::gpu::uma::get().is_some_and(|u| u.in_committed_window(at,
  len))`, replacing the `readable(at, len)` borrow gate; everything
  else copies page-by-page via `read_committed`) plus the matching
  test flip (a plain committed page is now `Owned`, not `Borrowed`).
  The whole `hle` module is `#[cfg(windows)]` (`lib.rs` swaps in
  `hle_linux.rs` on Linux: null `d3d12x_linux` + Vulkan), so
  `hle/d3d12x/*` never compiles on Linux; the Linux `gpu::uma` shim
  is constants-only (`MAX_WINDOWS`/`MAX_HEAP_WINDOWS`, no `get()` /
  `in_committed_window`) and zero `title_bytes` refs exist in the
  Linux shims — the borrowed-view race on worker threads has no
  Linux consumer. Our base shape matches (`title_bytes` at
  `title_textures.rs:1819`, `readable` gate, `Borrowed` test at
  `:2738`). The `(#2841)` cite in the new comment is an issue ref —
  `refs/pull/2841/head` stays MISSING. Earlier pins stand:
  #2830/#2831/#2832 SKIP; #2833/#2834 landed; #2836 adapted in
  `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim in `aa59e8ac`;
  #2843/#2844/#2845/#2846/#2847/#2848 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step123r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-124 (task-1795718142-d9ab): scan empty, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) tops out at #2849 (`4e931734`) — NOTHING past
  #2849 in the batched listing (older #2820–#2829 refs present below
  the window). Per-PR sequential probe #2830..#2960 (widened past the
  fourth gap per mem-1795712765-78dc): #2830 `069292ca` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` / #2843 `932e689c` /
  #2844 `25dfc2c0` / #2845 `76b17593` / #2846 `a4421e06` /
  #2847 `4b1780e8` / #2848 `88e56f93` / #2849 `4e93173` (all pins match
  batched + sequential, `merge-base --is-ancestor` exit 1 each — all still
  OPEN, unmerged); #2835, #2839..#2841, #2850..#2960 ALL MISSING — scan
  complete, nothing new since step-123. This time batched and sequential
  agree (no mid-window PR appeared after the batched fetch).
- Triage: nothing to triage. Earlier pins stand: #2830/#2831/#2832 SKIP;
  #2833/#2834 landed; #2836 adapted in `7645afeb`; #2837/#2842 SKIP;
  #2838 landed verbatim in `aa59e8ac`; #2843/#2844/#2845/#2846/#2847/
  #2848/#2849 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step124r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.
