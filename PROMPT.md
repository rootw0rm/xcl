# PROMPT.md — get the damn game launcher working (for another LLM)

You are on the `linux` branch of XCL. The goal is one thing: **`xcl run
"<game folder>"` boots the real Xbox title to its first frame on Linux.**
Everything else — the static musl exe, audio/input sinks, perf — is
packaging and polish that comes *after* the game boots. Read `AGENTS.md`
first (short contract); this file is the work plan. Details live in
`.ralph/specs/linux-port/` (`plan.md`, `decisions`, `progress.md`,
`context.md`) — read those before designing anything.

## Status snapshot (2026-11-25 — refresh when a milestone flips)

- `linux` @ `8c62384`, pushed to `origin/linux`. Upstream `main` @
  `568e7db` merged in. Verify: `git log --oneline -3; git status -sb`.
  No ralph loop running — this session drives (worktrees in
  `/tmp/opencode/<name>`, merged by the driver, cherry-picked one
  commit per item).
- **Retail launcher (the priority): the game boots to `exe+0x2237179`.**
  DllMains → entry → CRT init → sysinfo → semaphores → GDK init
  (`InitializeApiImplEx2` served; `XAsync`/`XNetworking`/`XUser`/`XPLS`
  via `QueryApiImpl`) all survive. The trap is a game-side
  config-selection abort reading an all-zero heap struct, with zero
  d3d12 calls in the log — device routing cannot move it. Next step is
  game-RE on that abort (disassemble the fault function like the
  `0x5a91c22` diagnosis: find the failing check, see what data it
  wanted), not more device stubs. `report.json` now carries crash +
  stubs on trap paths (`write_crash_with_code`); poisoned-lock fallback
  landed.
- **Synthetic title:** `ExitProcess(0xc)` / 12 FAILs default, 10 FAILs
  with `XCL_VULKAN_BACKEND=vulkan` (command-list memory + raytracing
  surface flipped). All remaining FAILs need a real renderer with PSO
  execution (D3DCompile echoes HLSL — no honest SPIR-V without a
  compiler, which is banned) or the VRAM memory model. Zero non-GPU
  gaps; `called_stubs` = console-GPU slots + 1 probe.
- **Phase D core landed:** GLSL backend, in-process SPIR-V, ash compute
  runner (Arc A770, `subgroupSize 32`), headless blit draw, hand-built
  + recompiler-driven triangles, X11 windowed present via
  `libc::dlopen` (no `winit`), Vulkan device routing + command lists +
  raytracing surface. lib tests 534 passed, 0 failed, 1 ignored.
- **Notes archive:** `/home/anon/xcl-notes/upstream-prs/` (index.md +
  9 keep + 3 skip files, ~9,200 lines, no URLs) triages all 1213 open
  upstream PRs. 6 ports landed as individual commits. Still actionable:
  K-D3D slot map + PSO-wait contract (Vulkan scheduling checklist),
  #2157/#2135 coherent-fallback (competing — take one), media/input
  harness items (Phase E), recompiler LATER items (backend expansion).
- **Game files PRESENT** (drive mounted):
  `/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/`
  + `/run/media/anon/12TB_B/leonida-work/`. Retail runs are NOT skipped.
  Mount check (`ls` both) first every session; an unmounted drive means
  `SKIPPED` for that session only.
- Build green on `x86_64-unknown-linux-gnu`: both `cargo check`s
  0 errors; `cargo test -p xcl --lib` 534/0/1. Re-verify after tree
  changes; never quote stale counts.

## 0. Ground rules (non-negotiable)

1. Game files stay **in place, read-only**. Never copy into the repo,
   never commit: game data/dumps, console SDK headers/binaries, keys,
   DRM circumvention. Sample tests use `XCL_SAMPLE_*` env vars and
   no-op + pass when unset.
2. License: **GPL-2.0-only**. New deps must be GPL-2.0-compatible AND
   static-musl-friendly (MIT/BSD/zlib or dual MIT-or-Apache-2.0 under
   the MIT grant — `ash`, `cpal`, `evdev` are fine; pure-Apache
   or GPL-3-only are out). No `winit` (Apache-2.0-only — windowing goes
   through `libc` `dlopen`/`dlsym`). No WinDurango ports. **vkd3d-proton is
   reference-only, never a dependency** (PC D3D12 ≠ console D3D12.X;
   LGPL-2.1 breaks static-musl; no driver internals) — its
   `vkd3d-shader` is the per-op oracle for `backend_spirv`. Never track
   its master.
3. Small diffs, one logical change per commit. No drive-by refactors,
   renames, or whole-tree `cargo fmt` (tree is not rustfmt-clean; format
   only touched lines). Reuse the module's existing helpers.
   Competing upstream PRs land once, never separately (see index).
4. Porting upstream: read the notes entry first, then **re-verify the
   live PR head pre-commit** (fetch the `.diff`; force-pushes are
   routine — port the new version; merged-with-changes → port the
   merged form; closed/superseded → stop and report). Never trust the
   notes' hunks blind.
4. Upstream sync (when asked): merge, never rebase — but **never re-add
   `CLAUDE.md` / `CONTRIBUTING.md`** (stay deleted) and **never
   overwrite this branch's `AGENTS.md`** with the Windows version.
5. Evidence before claims: run what you say you ran, report exact
   command + passed/failed/ignored/skipped. `SKIPPED <test>: <reason>`
   is not coverage. If it wasn't built/tested, say so.

## 1. The launcher critical path (in order — no skipping ahead)

**M1 — the real exe presents a frame (THE PRIORITY).** The triage loop:
```bash
timeout 120 ./target/debug/xcl run "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
# triage: ~/.local/share/XCL/<title>/logs/{xcl.log,report.json}
# report.json -> crash + called_stubs (+ xgameruntime.ApiStats); xcl.log [STUB] lines
# trap/segv with no clear stub: gdb backtrace + objdump disassembly of the fault
# function (loaded base 0x140000000; RIP-past-int3 means the cc byte before executed)
```
Current frontier is `exe+0x2237179` (game-RE needed — see snapshot).
Standing playbook, in order:
1. Game-RE on the current abort: disassemble the fault function and its
   callers, identify the failing check and the data it wanted, implement
   exactly that (one family per commit: unit test + resolver check +
   title e2e gate). The `0x5a91c22` diagnosis is the template (IAT
   slots → import names → strings → the game's own check). Families
   nothing calls stay stubs. Never fake-success past a check you don't
   understand — report it instead.
2. Console-slot routing checklist (`keep-d3d12-behavior.md` K-D3D-06):
   route the next device/list/slot the game actually calls, behind the
   existing env override (null stays default). Competing PR pairs land
   once, never separately (#2157 vs #2135, #1615/#1593 vs #1610).
3. Present path: adapter QI + frame clock (#78–#81) + `PresentX` #30,
   reusing `vulkan_present`; then the PSO-wait contract (K-D3D-01/02:
   draws never wait, bounded compute waits) so titles don't hit
   watchdogs. Then mesh/GS dispatch, descriptor heaps, committed
   textures, streaming textures, full shader-pack translation.
4. Gate: `xcl run "<game>"` → window → first presented frame.
Console integrity/tamper checks are out of scope until the log shows
them: report, don't hunt.

**M2 — synthetic title fully green.** At 0xc/12 default, 0xa/10 on the
Vulkan backend. Only GPU work may flip FAILs; a new non-GPU FAIL is a
regression — fix it first. The remaining 10 need a real renderer with
PSO execution or the VRAM memory model (see M1.3 — both fronts converge
there). Gate: `ExitProcess` + 0 FAILs.

**M3 — behind us.** Emitter split, GLSL + in-process SPIR-V, ash
compute, subgroup select, backend selector, headless + windowed
triangles all landed and green. Remaining Phase-D work lives in M1 now.

**M4 — after the first real frame:** real audio sink (`cpal`, MIT),
real input (`evdev`, MIT), static musl `release-final` profile
(`opt-level=3`, `lto="fat"`, `codegen-units=1`, `strip`, `debug=0`,
`panic="unwind"` — never `abort`), perf work (profile first: recompiler
emit buffers, tiling decode tables, VFS readahead, PSO/batch caching).
Any M4 commit before M1 lands gets reverted.

**M5 — standing backlog (never idle).** When M1 is blocked waiting on a
decision: (a) notes-archive NOW items (`xcl-notes/upstream-prs/`,
index first; K-D3D slot/contract checklists, #2157-or-#2135,
media/input harness); (b) LATER items when their precondition lands
(recompiler expansion past the blit subset, mesh/GS full pack);
(c) hourly upstream sync fallout (changed files vs the notes keepers).
M5 work follows the same gates (tests + title e2e + retail no-regress).

## 2. Session protocol

1. State branch + commit (`git branch --show-current` = `linux`,
   `git log --oneline -3`), then `ls` both game dirs — mount check
   first, every session.
2. Pick the smallest next step on the earliest unfinished milestone
   (M1 > M2 > M4; M5 when blocked). Narrowest test first
   (`cargo test -p xcl --lib <filter>`), then the widening ring
   (`--workspace`, title e2e, one real game file). Sample N=1/5/50
   before minutes-long runs (full-pack `--recomp`, whole suite).
   Read `/home/anon/xcl-notes/upstream-prs/index.md` before designing
   anything the notes cover — then re-verify the live PR head anyway.
3. Report: what changed, exact commands + counts, what was skipped and
   why, what remains. Paste tool output — no hand-written counts.
4. Snapshot freshness is a P0 bug: when a milestone flips, update the
   Status snapshot above in the same commit. A snapshot describing
   landed work as "the frontier" (or claiming files absent when the
   drive is mounted) wastes every session after it — fix it before
   writing more code.
5. Anti-stall (learned 2026-11-24): every iteration lands new behaviour
   (code, test, or doc commit) or terminates with a handoff. Pure
   re-verification twice in a row is forbidden — if two consecutive
   iterations produce no diff, stop and report instead of a third.

## 3. Pointers (read before touching)

- Loader/OS core: `loader.rs` + `loader_linux.rs`, `pe.rs`, `tls*.rs`,
  `eh.rs` + `eh_linux.rs` (sigaction handler; `.pdata` must be
  *translated*, never passed through), `resolver*.rs`, `vfs*.rs`,
  `state.rs`, `config.rs`, `launcher*.rs` (`XCL_DATA_ROOT` first, then
  XDG `~/.local/share/XCL`).
- Offline tools (done, keep green): `inspect`, `disasm`, `shaders`
  (+`--recomp`; keep `build.rs` `TRANSLATOR_SOURCES` accurate when
  moving emitter files), `rpf`.
- GPU: `gpu/recomp/emit.rs`, `glsl.rs`, `spirv.rs`,
  `vulkan_runner.rs`, `vulkan_graphics.rs`, `vulkan_present.rs`,
  `dxc.rs`, `runner.rs` (both Windows-only — do not extend),
  `hle/d3d12x/*` (Windows-only — read-only reference for slot layout),
  `hle/vulkan_linux.rs`, `hle/d3d12x_linux.rs` (null + Vulkan device).
- System services: `xaudio2.rs` + `opus.rs`/`xma2/` (decode done,
  sink headless), `gameinput.rs` + `kbm.rs`, `mfplat*.rs`,
  `dstorage*.rs`, `hle/xgameruntime_api_linux.rs` (GDK-thunk protocol),
  XGameRuntime offline paths.
- System services (M4+): `xaudio2.rs` + `opus.rs`/`xma2/` (decode done,
  sink headless), `gameinput.rs` + `kbm.rs`, `mfplat*.rs`,
  `dstorage*.rs`, `XGameRuntime` offline paths.
