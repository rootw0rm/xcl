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
