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
