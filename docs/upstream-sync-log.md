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

## 2026-11-26 — step-125 (task-1795718722-3f65): #2830-new-head + #2844-new-head + #2850 + #2851 triaged, all SKIP, probe past #2851

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) tops out at #2851 (`7e6e2f09`) — NOTHING past
  #2851 in the batched listing. Per-PR sequential probe #2830..#2960
  (widened past the fifth gap per mem-1795712765-78dc): #2830 `d6cf64c1` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` / #2843 `932e689c` /
  #2844 `fc007a96` / #2845 `76b17593` / #2846 `a4421e06` /
  #2847 `4b1780e8` / #2848 `88e56f93` / #2849 `4e93173` /
  #2850 `cdff8f72` / #2851 `7e6e2f09` (all pins match batched +
  sequential, `merge-base --is-ancestor` exit 1 each — all still OPEN,
  unmerged); #2835, #2839..#2841, #2852..#2960 ALL MISSING — scan
  complete. Batched and sequential agree this time (no mid-window
  arrivals after the batched fetch).
- Push-race note (per mem-1795714025-21a1): step-124 critic handed off
  #2830-new-head + #2850 + #2851; all three pins match the handoff
  hashes. #2844 was force-pushed since step-123 (`25dfc2c0` →
  `fc007a96`, same 2-commit stack over `b84e0378`, reworked tip:
  `25dfc2c0..fc007a96` = uma.rs ±15, title_textures −58, uma_driver
  +67) — triaged below at the NEW head, not the stale pin.
- Triage (all SKIP, hunk evidence):
  - #2830 `d6cf64c1` (shaders.rs +204/−13 startup-preparation
    cancellation): entire diff lives under `#[cfg(windows)]` —
    `Pipelines` struct/impl (`shaders.rs:1302/1516`), incl.
    `make_with_cache_path_cancel` / `make_one` (windows
    `ID3D12Device::*` body) and the `#[cfg(windows)] precompile`
    (`:1794`); the `#[cfg(not(windows))] precompile` (`:2046`) stays a
    no-op stub. Only shared-file touches are the `AtomicBool` import
    (covered by `:9 cfg_attr(not(windows), allow(unused_imports))`) and
    the `is_cancelled` helper (dead-code-allowed on Linux, zero Linux
    callers). SKIP — no Linux consumer (no DXC/D3D12 device on Linux).
  - #2844 `fc007a96` (coherent UMA driver heaps on NVIDIA, 5 files):
    shared-file hunk is `gpu/uma.rs` +14 (`driver: AtomicU32`,
    `set/is_driver_window`) only; every producer/consumer sits in
    `hle/d3d12x` (`uma_driver.rs` new 249 lines, `real.rs` open_uma +
    `sanitize_coherent_buffer`, `title_textures.rs`
    `with_title_bytes`/`read_committed_into`) which never compiles on
    Linux (`lib.rs` swaps in `hle_linux`). Zero `uma_driver` /
    `set_driver_window` / `is_driver_window` / `title_bytes` refs in
    `hle_linux.rs` / `d3d12x_linux.rs`; our `uma.rs` stays driver-free
    (`grep driver` empty). `driver == 0` makes the `real.rs` branch a
    no-op. SKIP — base shapes match.
  - #2850 `cdff8f72` (vfs.rs +1: `WRITE_ACCESS` gains `0x0200_0000`
    MAXIMUM_ALLOWED): ALREADY IN TREE — `91e37a87` (port of upstream
    #2694) added the identical bit as named const `MAXIMUM_ALLOWED`
    (`vfs.rs:73/76`, same `0x0200_0000` value) with the
    overlay-routing test (`:2247-2264`, `MAXIMUM_ALLOWED +
    OPEN_EXISTING must be a write open`). SKIP — duplicate of landed
    semantics, nothing to port.
  - #2851 `7e6e2f09` (eh.rs +9/−4 unwind-while-suspended): moves
    `unwind()` inside the `SuspendThread`/`GetThreadContext` window;
    all three are windows-crate/windows-sys-only (`eh.rs:13+`,
    `#2689`/`#106` cites). `eh_linux.rs` is a separate
    copy-then-unwind implementation with no consumer of this change.
    SKIP — Windows-only.
  Earlier pins stand: #2830old/#2831/#2832 SKIP; #2833/#2834 landed;
  #2836 adapted in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim
  in `aa59e8ac`; #2843/#2845/#2846/#2847/#2848/#2849 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step125r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-126 (task-1795719142-50e4): #2852 triaged SKIP (already-landed superset), probe past #2852

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) tops out at #2852 (`69ebece8`) — NOTHING past
  #2852 in the batched listing. Per-PR sequential probe #2830..#2960
  (widened past the sixth gap per mem-1795712765-78dc): #2830 `d6cf64c1` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` / #2843 `932e689c` /
  #2844 `fc007a96` / #2845 `76b17593` / #2846 `a4421e06` /
  #2847 `4b1780e8` / #2848 `88e56f93` / #2849 `4e93173` /
  #2850 `cdff8f72` / #2851 `7e6e2f09` / #2852 `69ebece8` (all pins match
  batched + sequential, `merge-base --is-ancestor` exit 1 each — all still
  OPEN, unmerged); #2835, #2839..#2841, #2853..#2960 ALL MISSING — scan
  complete. Critic step-125 handoff pin #2852 `69ebece8` matches the live
  head (no force-push, no mid-window arrival — batched and sequential
  agree).
- Triage (one new PR):
  - #2852 `69ebece8` (D3D12.X: forget command list before its final
    release, single commit on `8b957bec`, cites #2690 address-reuse race):
    `real.rs` +11/−3 replaces the `n = f(this); if n == 0 { trio }`
    tail of `cl_release` with an AddRef probe
    (`last = add(this) == 2`), the `list_x::on_release` + `bind::forget`
    + `dred::forget_list` trio BEFORE the final release, and an undo
    `f(this)` of the probe AddRef. SKIP — already-landed SUPERSET in
    tree via `3e466e16` (port of upstream #2520 + #2691): our
    `cl_release` (`real.rs:8155`) already probes `add(this) == 2`,
    forgets the identical trio (factored as `forget_command_list`,
    `:8170-8174`) while the count is still 1 (same before-release
    ordering), AND keeps the race fallback the upstream PR dropped
    (`if n == 0 && !last`, same comment shape as upstream's own
    "two last-but-one releases" caveat). Base shape matches
    (upstream pre-image = the `n == 0` tail ours replaced in
    `3e466e16`). Windows-only file regardless: whole `hle` is
    `#[cfg(windows)]` (`lib.rs:30-34`, `hle_linux` swap on Linux), and
    zero `on_release` / `bind::forget` / `dred::forget` /
    `forget_command_list` refs in `d3d12x_linux.rs` /
    `vulkan_linux.rs` / `hle_linux.rs` (the two `list_x` hits in
    `d3d12x_linux.rs:1437-1439` are a local test type alias `ListX`,
    not the module). Nothing to port.
  Earlier pins stand: #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed;
  #2836 adapted in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim
  in `aa59e8ac`; #2843/#2844-new-head/#2845/#2846/#2847/#2848/#2849/#2850/
  #2851 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step126r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-127 (task-1795719862-9e56): #2853 + #2854 + #2855 + #2856 triaged, all SKIP, probe past #2855

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) tops out at #2856 (`e29306f4`) — one past the
  critic step-126 handoff top (#2855). Per-PR sequential probe #2830..#2970
  (widened past the sixth gap per mem-1795712765-78dc): #2830 `d6cf64c1` /
  #2831 `965806a0` / #2832 `e2ecdfa9` / #2833 `f5a352c3` /
  #2834 `653f8f13` / #2836 `43a4eb9d` / #2837 `9d967cf8` /
  #2838 `ef36b7df` / #2842 `344ce7b5` / #2843 `932e689c` /
  #2844 `fc007a96` / #2845 `76b17593` / #2846 `a4421e06` /
  #2847 `4b1780e8` / #2848 `88e56f93` / #2849 `4e93173` /
  #2850 `cdff8f72` / #2851 `7e6e2f09` / #2852 `69ebece8` /
  #2853 `bee0dab9` / #2854 `080e973d` / #2855 `651fc050` /
  #2856 `e29306f4` (all 23 pins match batched + sequential,
  `merge-base --is-ancestor` exit 1 each — all still OPEN, unmerged);
  #2835, #2839..#2841, #2857..#2970 ALL MISSING — scan complete.
  All four new heads predate this task window (task created 19:04:22 UTC;
  #2853 18:59 UTC / #2854 18:52 UTC / #2855 18:20 UTC / #2856 18:49:52 UTC),
  so no push-race in this window — three are the critic step-126 handoff,
  #2856 is the one genuinely new arrival since the critic's scan.
- Triage (four new PRs, all SKIP with hunk evidence):
  - #2853 `bee0dab9` (D3D12.X: StorePipeline gives the driver the
    pipeline behind a stand-in; single commit on `8b957bec`):
    `pso_async.rs` +23 (`Built` enum + `built()` non-waiting probe),
    `real.rs` +153 (`pipeline_to_store` / `store_pipeline` /
    `lib_store_pipeline` + `store_pipeline_tests`), `standin.rs` +8 —
    ALL three files under `hle/d3d12x/`. SKIP — Windows-only: whole
    `hle` is `#[cfg(windows)]` (`lib.rs:30-34`, `hle_linux` swap on
    Linux), zero `Built::` / `lib_store_pipeline` / `is_shell` refs in
    `hle_linux.rs` / `d3d12x_linux.rs`. Nothing to port.
  - #2854 `080e973d` (thread-limit tests run on a wave32-only GPU;
    stacked mega-branch, 410 commits, merge-base `b49be183` — the tip
    commit itself is 2 files +4/−2): `gpu/recomp/tests.rs` adds
    `wave64: false` to the "HLSL wave32" form, `tests/gpu_compute_e2e.rs`
    `XCL_DIRECT_SPLIT_INSTRUCTIONS` 12→16 with comment. SKIP — blocked
    on the unlanded `f9bcafd` thread-limit stack: the lib-test target
    `gpu_compute_thread_limits_leave_the_lanes_past_them_out` is ABSENT
    from the tree (grep count 0; no `thread limit` commit in log), and
    our e2e region matches the upstream PRE-image exactly, so the 12→16
    bump (which compensates entry-block growth from thread-limit code
    we do not have) would mis-tune our test. Revisit when `f9bcafd`
    lands.
  - #2855 `651fc050` (Direct DXIL: saturate `v_pk_*_i16` CLAMP with masked
    select; single commit stacked on `e120925e`): `lower_wide.rs` +185/−4
    replaces `IMax(IMin(t, 0x7FFF), -32768) & 0xFFFF` with
    SGt/SLt-compare + `select` (NVIDIA Ampere 610.88/616.56 drops the
    `& 0xFFFF`, clobbering bits [31:16]). Our tree holds the PRE-image
    (`lower_wide.rs:182-185` still `imin`/`imax`/`andc`). SKIP —
    DXIL-emit-side driver workaround with no Linux driver consumer:
    Linux device execution is Vulkan/SPIR-V (`hle/vulkan_linux`), and
    Direct-DXIL bytes on Linux go only to offline stats/dumps
    (`shaders.rs:450` form counters) — no driver ever compiles them
    here. The SPIR-V emitter has no parallel pattern (it lowers
    `v_pk_add/sub_i16` CLAMP via the `clamp()` builtin,
    `emit.rs:3321-3322`). The new unit test vacuous-passes on Linux
    (`validator_supported()` false → early return) and the GPU test
    skips without a wave32 device — porting is unobservable here.
  - #2856 `e29306f4` (D3D12.X: keep logic ops on UINT render targets;
    single commit stacked on `e120925e`, sibling of #2855):
    `hle/d3d12x/real.rs` +143/−15 — `probe_logic_op` caches
    `OutputMergerLogicOp`, `sanitize_integer_rt_blend` gains the
    `logic_op_supported` param (7 `windows::Win32` / `ID3D12Device`
    refs in the diff). SKIP — Windows-only file and API: whole `hle`
    is `#[cfg(windows)]`, zero `logic_op` / `sanitize_integer` /
    `is_uint_dxgi` refs in `hle_linux.rs` / `d3d12x_linux.rs`.
    Nothing to port.
  Earlier pins stand: #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed;
  #2836 adapted in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim
  in `aa59e8ac`; #2843/#2844-new-head/#2845/#2846/#2847/#2848/#2849/#2850/
  #2851/#2852 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step127r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-128 (task-1795720425-15cc): #2858/#2859/#2860 triaged SKIP, probe past #2860

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) first topped at #2858 (`971f766c`) — the critic
  step-127 handoff. Per-PR sequential probe #2830..#2980 (widened past
  the sixth gap per mem-1795712765-78dc) found 25 PRESENT
  (#2830..#2860 minus #2835/#2839-#2841/#2857 gaps) — INCLUDING #2859
  (`83b95c4f`) the batched listing missed (replication lag, NOT
  rate-limit: the ref committed 18:36 UTC, ~40 min before the batch).
  A second batched listing then showed #2860 (`6efa82fa`); targeted
  sequential #2857/#2861..#2870 (only #2858/#2859/#2860 PRESENT) plus
  #2981..#3020 (ALL MISSING) closed the window — scan complete, top is
  #2860. #2858 `971f766c` matches the handoff pin (no force-push).
  All 25 pins `merge-base --is-ancestor` exit 1 (all OPEN); #2857 and
  #2861..#3020 MISSING. All three new heads predate this task window
  (task created 19:13:45 UTC; #2858 19:06:41 UTC in-window-commit but
  handed off pre-push, #2859 18:36 UTC, #2860 17:46 UTC) — late-replication
  arrivals, not missed fallout (per mem-1795714025-21a1).
- Triage (three new PRs, all SKIP with hunk evidence):
  - #2858 `971f766c` (D3D12.X: the depth bias of a 16-bit depth buffer
    is in 24-bit units; SINGLE commit on `e120925e`, parent already in
    main): `device_x.rs` +78 (`pc_depth_bias` x256 for console DSV 118
    -> PC 45, `DepthBias::for_dsv`, `base_dsv_format`,
    `depth_bias_pso` re-emit with PC DSV format), `list_x.rs` +44
    (`try_native_depth_bias` + `on_set_pipeline_state` re-scale on DSV
    change), `real.rs` +101 (`stream_dsv_format` /
    `pc_stream_depth_bias` for RASTERIZER 10/27/28 subobjects, GS-PSO
    rasterizer + fallback-stream paths). SKIP — Windows-only: all three
    files under `hle/d3d12x/` (`lib.rs:30-34` `#[cfg(windows)]` gate,
    `hle_linux` swap on Linux), zero `pc_depth_bias` / `for_dsv` /
    `base_dsv_format` refs in `hle_linux.rs` / `d3d12x_linux.rs` /
    `vulkan_linux.rs` (grep exit 1). The scale lives entirely in the
    D3D12.X PSO-translation layer; the Linux device path (Vulkan/SPIR-V
    via null `d3d12x_linux`) has no consumer. Nothing to port.
  - #2859 `83b95c4f` (D3D12.X: pipeline library blob has a readable
    dword after its end; 276-commit stack, tip is 1 file +36/-1):
    `hle/d3d12x/pso_library.rs` gains the `copy: Vec<u8>` fallback
    (files ending <4 bytes before a page boundary are read into memory
    + 4 zero bytes, since AMD's driver over-reads the mapped end) plus
    a `VirtualQuery`-gated readability test. SKIP — blocked on
    unlanded stack AND Windows-only: `pso_library.rs` does not exist in
    the tree (the whole module is part of the unlanded stack), and the
    diff is `windows_sys`/CreateFileMappingW/UnmapViewOfFile-only with
    zero Linux-shim refs. Revisit when the module lands.
  - #2860 `6efa82fa` (Shaders: separate Series S and Series X used
    manifests; 410-commit stack on `b49be183`, tip is 4 files +96/-16):
    `README.md` doc line, `gfx_records.rs` +3/-2 (`path()` joins
    `used_file_name`), `pso_async.rs` +52/-35
    (`profile_is_series_s` boot->`XCL_DEVICE_TYPE` fallback,
    `used_file_name`/`used_pipelines_name`/`used_shaders_name`,
    `-lockhart` manifest variants), `shaders.rs` +34/-12 (new
    `pipeline_worker_command` forwarding `XCL_DEVICE_TYPE`, workers and
    `--used-only` println use the per-profile names). SKIP — blocked on
    the unlanded stack: the tip's callees (`worker_command`) do not
    exist in the tree's `shaders.rs`, and every production hunk sits in
    Windows-gated code (`precompile` real impl `#[cfg(windows)]`
    `shaders.rs:1794`, Linux stub `:2046`; `earlier_used_keys` call
    site under `#[cfg(windows)]` `:1043`; `hle/d3d12x/` files) with zero
    `pso_async` / `profile_is_series_s` / `used_pipelines_name` refs in
    the Linux shims (grep exit 1). Revisit when `f9bcafd`/preparation
    stack lands.
  Earlier pins stand: #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed;
  #2836 adapted in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim
  in `aa59e8ac`; #2843/#2844-new-head/#2845/#2846/#2847/#2848/#2849/#2850/
  #2851/#2852/#2853/#2854/#2855/#2856 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step128r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-129 (task-1795721259-f1ec): scan empty past #2860, all pins re-verified

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout
  (re-fetched pre-commit 19:39 UTC).
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) was flaky this round: the first full listing
  topped at #2821 (truncated fetch, 2345 refs) while a re-list's raw
  tail showed #2860 — sequential is ground truth, same lesson as
  step-128's batch-miss in the other direction. Per-PR sequential probe
  #2830..#3040 (widened past the sixth gap per mem-1795712765-78dc)
  found 26 PRESENT (#2830..#2860 minus #2835/#2839-#2841/#2857 gaps);
  all pins match step-128 (no force-push), all 26 `merge-base
  --is-ancestor` exit 1 (all OPEN); #2861..#3040 ALL MISSING — scan
  complete, top is still #2860. No new PRs since step-128, no push
  race (per mem-1795714025-21a1): nothing committed in-window that was
  not already triaged.
- Triage: nothing new to triage. Earlier pins stand:
  #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed; #2836 adapted
  in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim in `aa59e8ac`;
  #2843/#2844-new-head/#2845/#2846/#2847/#2848/#2849/#2850/#2851/#2852/
  #2853/#2854/#2855/#2856/#2858/#2859/#2860 SKIP.
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step129r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-130 (task-1795722019-4a48): #2861/#2862/#2864 triaged SKIP, probe past #2864

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) flaky again: full listing topped at #2822
  (truncated fetch, 2347 refs) — sequential is ground truth, same
  lesson as step-128/129. Per-PR sequential probe #2830..#3060
  (widened past the sixth gap per mem-1795712765-78dc) found 29
  PRESENT (#2830..#2864 minus #2835/#2839-#2841/#2857/#2863 gaps);
  all pins match prior steps (no force-push), all 29 `merge-base
  --is-ancestor` exit 1 (all OPEN); #2865..#3060 ALL MISSING — scan
  complete, top is #2864.
- Replication-lag handoff (per mem-1795714025-21a1, NOT builder
  fault): #2862 committed 19:40:07 UTC (12s before task creation
  19:40:19 UTC) and #2864 committed 19:31:59 UTC both predate the
  window yet were MISSING from the step-129 critic scan to #3060 —
  same lag shape as step-128's batch-miss. Triaged in-window now.
- Triage (all SKIP with hunk evidence): #2861 `a3b9394e` SKIP,
  single commit on `8b957bec` (parent IS in main, exit 0), 1 file
  `hle/d3d12x/real.rs` +156/-2 (CreateCommandList-behind-standin:
  `dev_create_command_list_driver` rename + `pso_async::resolve`
  wrap incl. `create_raw_pc_command_list` path + 147-line standin
  test module); whole `hle` `cfg(windows)`, zero
  `pso_async`/`standin`/`dev_create_command_list` refs in the Linux
  shims (sole `d3d12x_linux.rs` hit is an unrelated fence-wait
  comment). #2862 `3f0ed9b8` SKIP, single commit on `8b957bec`,
  2 files +54/-3 (`gpu/texture.rs` staging-capacity counters +
  `Drop` bucket + `host.rs` +5 periodic log line);
  `gpu::texture` is `#[cfg(windows)]` (`windows::Win32` throughout),
  `host.rs` is `cfg(windows)` with a `host_linux.rs` twin holding
  zero staging refs. #2864 `aea330e1` SKIP, single commit on
  `8b957bec`, 3 files +566/-5 (README `XCL_DSTORAGE_CACHE` docs +
  `hle/dstorage.rs` opt-in unpack-cache hooks + new 482-line
  `dstorage/unpack_cache.rs`: Adler-checked on-disk store, 32 GiB
  cap, own writer thread); whole `hle` `cfg(windows)`
  (`std::os::windows`, `windows_sys` in the diff), and the Linux
  twin `dstorage_linux.rs` is a headless synchronous subset that
  fails closed on zlib/BCPack (`E_DECOMPRESSION`) with zero
  unpack/cache refs — decoded-request caching has no Linux
  consumer; opt-in perf env, not correctness. Earlier pins stand:
  #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed; #2836 adapted
  in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim in
  `aa59e8ac`; #2843-#2860 SKIP (minus #2857 gap).
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step130r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-131 (task-1795722975-275e): #2865/#2868/#2870/#2871 triaged SKIP, probe past #2871

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18) topped at #2868 (2350 refs) — truncated again:
  it MISSED #2870/#2871, which the sequential probe found. Sequential
  is ground truth. Per-PR sequential probe #2830..#3110 (widened past
  the sixth gap per mem-1795712765-78dc) found 33 PRESENT
  (#2830..#2871 minus #2835/#2839-#2841/#2857/#2863/#2866/#2867/#2869
  gaps); all pins match prior steps (no force-push), all 33
  `merge-base --is-ancestor` exit 1 (all OPEN); #2872..#3110 ALL
  MISSING — scan complete, top is #2871.
- Push-race note (per mem-1795714025-21a1, NOT builder fault): #2868
  committed 19:56:46 UTC (~30s after task creation 19:56:15 UTC) and
  #2870/#2871 committed ~21:03-21:04 UTC mid-window — all postdate the
  step-130 critic scan to #3080. Triaged in-window now.
- Triage (all SKIP with hunk evidence): #2865 `2f4b4082` SKIP,
  STACKED tip on `71b8e6e0` (NOT in main, exit 1; 98-file stack diff
  vs main), tip-only 3 files +46/-2 (`gpu/dxil/kernel.rs` +
  `gpu/recomp/mod.rs` + `gpu/recomp/tests.rs`) gating per-region loop
  budgets to compute/kernel only; `budget_regions` occurs NOWHERE in
  the linux tree (`grep -rl` count 0 — the PR #2793 per-region-budget
  callee is itself unlanded), so the tip has no Linux consumer.
  #2868 `8edc0e03` SKIP, STACKED on the same `71b8e6e0` base, 2 files
  +19/-6 (`hle/d3d12x/pso_async.rs` `avail_commit_bytes` pub(super)
  + `hle/d3d12x/real.rs` commit-not-RAM throttle + test); whole
  `hle` `cfg(windows)` (`windows_sys::Win32` in the diff), zero
  `avail_commit_bytes`/`big_compile`/`pso_async` refs in the Linux
  shims. #2870 `ba0df574` SKIP, STACKED (parent `b0d2fb81` NOT in
  main), 3 files +149/-0 scripts-only (README `XCL_SHADER_DIAG`
  bundle docs + new `scripts/analyze_shader.py` + its test, both
  absent from the linux tree) — dev-only diagnostic tooling, no
  launcher behavior. #2871 `57a80785` SKIP, STACKED on #2870 itself
  (parent `ba0df574` NOT in main), 4 files +196/-0 scripts-only
  (README RGA section + `analyze_shader.py` RGA hooks + new
  `scripts/shader_rga.py` + its test). Earlier pins stand:
  #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed; #2836 adapted
  in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim in
  `aa59e8ac`; #2843-#2864 SKIP (minus #2857/#2863 gaps).
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step131r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-132 (task-1795724164-1571): #2872/#2873 triaged SKIP, #2864 re-pinned after force-push, probe past #2873

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18): 2354 refs. Per-PR sequential probe #2830..#3150
  (widened past the sixth gap per mem-1795712765-78dc) found 35 PRESENT
  (#2830..#2873 minus #2835/#2839-#2841/#2857/#2863/#2866/#2867/#2869
  gaps), matching pins on all previously seen heads except #2864 (see
  below); all 35 `merge-base --is-ancestor` exit 1 (all OPEN);
  #2874..#3150 ALL MISSING — scan complete, top is #2873.
- Replication-lag note (per mem-1795714025-21a1, NOT builder fault): #2873
  committed 19:50:00 UTC, before task creation (20:16:04 UTC), yet MISSING
  from the step-131 critic scan to #3130 — same handoff treatment as
  #2862/#2864 in step-130. Triaged in-window now.
- Force-push on #2864 mid-window: live head moved `aea330e1` -> `723f5a19`
  (old NOT ancestor of new, exit 1; new committed 20:24:26 UTC, parent
  still `8b957bec`). Tip-only delta is a closed-file guard
  (`f.handle()?` -> E_FILE_HANDLE in `unpack_cache_entry`) + the
  `a_closed_file_is_not_answered_from_the_unpack_cache` regression test +
  `flush` visibility (`fn` -> `pub(super) fn`); same 3 files
  (README + `hle/dstorage.rs` + `hle/dstorage/unpack_cache.rs`, whole
  `hle` `cfg(windows)`), Linux twin `dstorage_linux.rs` untouched —
  SKIP stands, pin updated.
- Triage (all SKIP with hunk evidence): #2872 `b780ec78` SKIP,
  single-commit on `8b957bec` (parent IS in main, exit 0), 1 file +13/-7
  (`gpu/recomp/emit.rs`): `ballot_px_quad`'s `[unroll]` loop factored into
  `ballot_px_member` x4 + `the_pixel_prelude_has_no_loop_for_dxc_to_unroll`
  test — HLSL PRELUDE_PIXEL only (DXC compile-time perf), zero
  `PRELUDE_PIXEL`/`ballot_px` refs outside `recomp/emit.rs`, so no
  GLSL/SPIR-V consumer; held for the SPIR-V port like the other
  emit-side-only keepers. #2873 `1f3d506d` SKIP, single-commit (parent
  `e120925e` IS in main, exit 0; 2-behind tip), tip-only 1 file +308
  (`hle/d3d12x/list_x.rs`): `footprint_plan` routing unaligned
  CopyTextureRegion footprints through scratch — whole `hle`
  `cfg(windows)`, zero `footprint_plan`/`copy_texture_region_x`/
  `UnrestrictedBufferTextureCopyPitch` refs in `hle_linux.rs`,
  `d3d12x_linux.rs`, `vulkan_linux.rs` (grep exit 1). Earlier pins stand:
  #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed; #2836 adapted
  in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim in
  `aa59e8ac`; #2843-#2871 SKIP (minus #2857/#2863 gaps).
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step132r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.

## 2026-11-26 — step-133 (task-1795725390-c461): #2875/#2877 triaged SKIP, probe past #2877

- `upstream/main` = `8b957bec`, `merge-base --is-ancestor upstream/main HEAD`
  exit 0, HEAD-ancestor-of-upstream exit 1 — ZERO sync fallout.
- Batched `git ls-remote upstream 'refs/pull/*/head'` cross-check (per
  mem-1795715658-5c18): 2356 refs, in-range set exactly
  #2830..#2877 minus the known gaps (showed #2875/#2877 that the
  step-132 sequential scan missed) — sequential still ground truth.
  Per-PR sequential probe #2830..#3200 (widened past the sixth gap per
  mem-1795712765-78dc) found 37 PRESENT (#2830..#2877 minus
  #2835/#2839-#2841/#2857/#2863/#2866/#2867/#2869/#2874/#2876 gaps),
  matching pins on all 35 previously seen heads (no force-push, #2864
  pin `723f5a19` holds); all 37 `merge-base --is-ancestor` exit 1
  (all OPEN); #2878..#3200 ALL MISSING — scan complete, top is #2877.
- Replication-lag note (per mem-1795714025-21a1, NOT builder fault): #2875
  committed 20:32:00 UTC mid-window of the step-132 run (task created
  20:16:04 UTC, scan to #3150) yet MISSING from it, and #2877 committed
  18:26:34 UTC pre-task-creation yet MISSING from both the step-132
  builder scan and the step-131 critic scan to #3130 — same handoff
  treatment as #2873 in step-132. Triaged in-window now.
- Triage (all SKIP with hunk evidence): #2875 `7876a7f6` SKIP,
  single-commit on `8b957bec` (parent IS in main, exit 0), 1 file +7/-2
  (`gpu/dxil/fuzz_tests.rs`): `run_seeds` keeps the `wave32` result and
  reports `runner::skip_test("no wave32"/"no GPU")` instead of silently
  skipping the GPU comparison — test-only reporting inside
  `#[cfg(all(test, windows))] mod fuzz_tests` (`gpu/dxil/mod.rs:63`,
  never compiled on Linux per the SPIR-V-backend LINUX-TODO above it).
  #2877 `c1c22863` SKIP, single-commit on `8b957bec` (parent IS in main,
  exit 0), 1 file +28/-2 (`hle/d3d12x/title_textures.rs`):
  `title_bytes_read` (`Cow::Owned(read_committed(...))` via
  `ReadProcessMemory`) for texture workers so a title-decommitted page
  can't fault a borrow (#2841) + `windows_sys` VirtualAlloc regression
  test — whole `hle`/`d3d12x` `cfg(windows)`, zero `title_bytes`/
  `read_committed`/`title_textures` refs in Linux-compiled files (sole
  hit is Windows-only `hle/kernelx.rs`). Earlier pins stand:
  #2830-new-head/#2831/#2832 SKIP; #2833/#2834 landed; #2836 adapted
  in `7645afeb`; #2837/#2842 SKIP; #2838 landed verbatim in
  `aa59e8ac`; #2843-#2873 SKIP (minus #2857/#2863 gaps).
- Gates (doc-only change, no code touched): both `cargo check`s 0 errors
  (22 lib + 28 lib-test pre-existing warnings); lib 783 passed /
  0 failed / 1 ignored (`--test-threads=4`); title e2e 4 passed /
  1 failed by design (`test_title_runs_and_game_folder_is_untouched`),
  24 `[FAIL]` lines, all `D3D12.X`, 0 non-GPU; retail fresh-root
  `XCL_DATA_ROOT=/tmp/xcl-step133r` exit 139 SIGSEGV
  `exe+0x2237320` fault `0x0` bytes `48 8b 03` stubs 20
  (all first_args) xgr exactly XAsync/XNetworking/XUser — HOLDS
  (report.json verified, fresh root removed after).
  Lock audit: 0 new `lock()` (no code touched); `diff --check` clean.
