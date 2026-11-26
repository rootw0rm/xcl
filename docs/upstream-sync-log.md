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
