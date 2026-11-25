# PROMPT.md — get the damn game launcher working (for another LLM)

You are on the `linux` branch of XCL. The goal is one thing: **`xcl run
"<game folder>"` boots the real Xbox title to its first frame on Linux.**
Everything else — the static musl exe, audio/input sinks, perf — is
packaging and polish that comes *after* the game boots. Read `AGENTS.md`
first (short contract); this file is the work plan. Details live in
`.ralph/specs/linux-port/` (`plan.md`, `decisions`, `progress.md`,
`context.md`) — read those before designing anything.

## Status snapshot (2026-11-25 — refresh when a milestone flips)

- `linux` @ `2553a45` (merge of upstream `main` @ `568e7db`, plus Linux
  work; pushed to `origin/linux`).
  Verify with `git log --oneline -3; git status -sb`.
  Upstream `6e1e2be` → `568e7db` is README/assets only (retail game
  status: about 60 %, ~5 fps in its world) — no code, no Linux fallout.
  The ralph loop was stopped 2026-11-25; this session drives (streams in
  `/tmp/opencode/stream-<x>`, merged by the driver). Stream C landed:
  `report.json` is now written with the crash site on SIGTRAP/SIGABRT
  (`6a563be`, lib tests 493 passed). Stream A landed 4 sync/sysinfo
  families (`bdafc1d`, lib tests 496 passed): retail now traps at
  `exe+0x5a91c22`, diagnosed as the game's `js`-to-`int3` abort after
  `0x1455e2a70` returns negative — its 4 `GetProcAddress`es for
  xgameruntime `QueryApiImpl`/`InitializeApiImpl*` all return NULL.
  Next family: those xgameruntime exports.
- Stream B landed 17d review (PASS, no changes) + step 16/selector
  verified already-in-tree + windowed present of the recompiler-driven
  triangle via raw X11/`dlopen` (`fb58739`, no `winit` — Apache-only;
  lib tests 502 passed). Stream A2 landed the GDK-thunk protocol
  (`98139c6`, lib tests 511 passed): the 4 xgameruntime resolves work,
  retail now completes GDK init and traps at `exe+0x2237179` (config-
  selection abort on an all-zero heap struct, no stub implicated).
  Stream E landed device routing (`150d8ef`, lib tests 517 passed):
  console device creation routes to Vulkan under the env override (null
  default; fixed an ash `Entry`-drop `dlclose` segfault); Vulkan command
  lists implement console slots #60/#61/#69-#72/#89/#93; raytracing
  surface slots 13/18/146/148. Title on Vulkan backend: 12→10 FAILs
  (command-list memory + raytracing surface flipped); remaining 10 need
  a real renderer. Retail unmoved at `0x2237179` with zero d3d12 calls —
  next retail step is game-RE on that abort, not more device stubs.
- Ported 6 upstream fixes as individual commits (each verified against
  the live PR head pre-commit; 2 force-pushes caught and ported fresh):
  #1610 COM identity, #1705/#240 report poison-lock guard, #1143 page-
  allocation boundary, #2136/#2148/#2134 recompiler soundness
  (lib tests 534 passed).
- **Game files are PRESENT on this host** (the drive is mounted):
  `/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/`
  (`game_scarlett_master.exe`, 120 MB, + `MicrosoftGame.Config`) and
  `/run/media/anon/12TB_B/leonida-work/` (`shaders/`, `exe-disasm/`,
  `*-carve/`, `intro_demo/`). **Retail runs are NOT skipped anymore.**
  Every session starts by confirming the mount (`ls` both dirs); if the
  drive is ever unmounted, retail steps are `SKIPPED` for that session
  only — never treat one absent-mount check as permanent.
- Build green on `x86_64-unknown-linux-gnu`: `cargo check -p xcl --tests`
  + `cargo check --workspace` 0 errors; `cargo test -p xcl --lib`
  490 passed, 0 failed, 1 ignored (last full run after the `6e1e2be`
  sync — re-verify after tree changes, don't quote stale counts).
- **Synthetic title (`tests/testtitle` + `tests/testgamedll`) is one step
  from done:** reaches `ExitProcess(0xc)` with 12 FAILs, ALL of them
  null-device D3D12 categories, `called_stubs` = 28 console-GPU slots +
  1 expected probe. Zero non-GPU calls missing. The stub-triage loop on
  synthetic is exhausted — do not invent more synthetic stub work.
- **Phase D is halfway landed:** step 13 GLSL backend (`6c09624`),
  step 14 in-process SPIR-V (`d64f32c`), step 15 ash compute runner
  (`a157022`, verified on a real Arc A770, `subgroupSize 32`). Queued:
  step 16 subgroup-width selection, step 17 Vulkan backend selector +
  headless graphics smoke → triangle → recompiler-driven triangle →
  window + present.
- A ralph loop (`primary-20261124-234921`) is working this tree — do not
  touch its files, do not stash, verify merges in an isolated worktree.

## 0. Ground rules (non-negotiable)

1. Game files stay **in place, read-only**. Never copy into the repo,
   never commit: game data/dumps, console SDK headers/binaries, keys,
   DRM circumvention. Sample tests use `XCL_SAMPLE_*` env vars and
   no-op + pass when unset.
2. License: **GPL-2.0-only**. New deps must be GPL-2.0-compatible AND
   static-musl-friendly (MIT/BSD/zlib or dual MIT-or-Apache-2.0 under
   the MIT grant — `ash`, `winit`, `cpal`, `evdev` are fine; pure-Apache
   or GPL-3-only are out). No WinDurango ports. **vkd3d-proton is
   reference-only, never a dependency** (PC D3D12 ≠ console D3D12.X;
   LGPL-2.1 breaks static-musl; no driver internals) — its
   `vkd3d-shader` is the per-op oracle for `backend_spirv`. Never track
   its master.
3. Small diffs, one logical change per commit. No drive-by refactors,
   renames, or whole-tree `cargo fmt` (tree is not rustfmt-clean; format
   only touched lines). Reuse the module's existing helpers.
4. Upstream sync (when asked): merge, never rebase — but **never re-add
   `CLAUDE.md` / `CONTRIBUTING.md`** (stay deleted) and **never
   overwrite this branch's `AGENTS.md`** with the Windows version.
5. Evidence before claims: run what you say you ran, report exact
   command + passed/failed/ignored/skipped. `SKIPPED <test>: <reason>`
   is not coverage. If it wasn't built/tested, say so.

## 1. The launcher critical path (in order — no skipping ahead)

**M1 — the real exe boots to its entry point.** Run it, read the first
crash, fix exactly that, repeat:
```bash
timeout 120 ./target/debug/xcl run "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
# triage: ~/.local/share/XCL/<title>/logs/{xcl.log,report.json}
# report.json -> crash + called_stubs; xcl.log [STUB] lines
```
Implement missing HLE **in the order the exe actually calls it**, one
family per commit (each: unit test + resolver check + title e2e gate).
Families nothing calls stay stubs. CRT forwarding (`hle/crt_linux.rs`
via `resolver_linux.rs`) is done — extend only with the next missing
family the log shows. Console integrity/tamper checks are out of scope
until the log shows them: report, don't hunt. Gate: the exe survives
every `DllMain` and reaches its entry point's first HLE calls.

**M2 — synthetic title fully green except GPU.** It is at 0xc/12 GPU
fails now. Only GPU work may flip those; if a non-GPU FAIL appears,
that is a regression — fix it before anything else. Gate:
`ExitProcess` + 0 non-GPU FAILs + `called_stubs` containing only
console-GPU slots and the expected probe.

**M3 — first pixels (Phase D remainder, the largest chunk).**
`plan.md` steps 16 → 17 in order, each with its acceptance gate, each
proven on the cheapest test first (blit VS → headless smoke → triangle
→ recompiler-driven triangle). Rules: emitter stays backend-agnostic
(`emit.rs` = RDNA2 semantics; HLSL/DXIL-isms behind `backend_hlsl`,
SPIR-V behind `backend_spirv`); wave64 selects 64 vs split-32 from
`VkPhysicalDeviceSubgroupProperties::subgroupSize`, tested at both
widths; `runner.rs` dispatches via `ash` compute, `skip_test(reason)`
without a device; `hle/d3d12x/real.rs` + `guard.rs` do NOT port — route
console D3D12.X to `hle/vulkan/`, `null.rs` stays the headless fallback;
`present.rs` becomes Vulkan swapchain + `winit` window, pacing
unchanged. Never ship a path that shells out to an external compiler
(`glslc`/`dxc -spirv`/`spirv-val` are `#[cfg(test)]` bring-up only).
Gate: offscreen triangle drawn from recompiled shaders, pixels match.

**M4 — the real game presents a frame.** `xcl run "<game>"` with the
Vulkan backend → window → first presented frame. Then, and only then:
real audio sink (`cpal`), real input (`evdev`), static musl
`release-final` profile, perf work. These are explicitly NOT M1–M3
work — any commit starting them before M4 lands gets reverted.

## 2. Session protocol

1. State branch + commit (`git branch --show-current` = `linux`,
   `git log --oneline -3`), then `ls` both game dirs — mount check
   first, every session.
2. Pick the smallest next step on the earliest unfinished milestone
   (M1 > M2 > M3 > M4). Narrowest test first
   (`cargo test -p xcl --lib <filter>`), then the widening ring
   (`--workspace`, title e2e, one real game file). Sample N=1/5/50
   before minutes-long runs (full-pack `--recomp`, whole suite).
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
  `vulkan_runner.rs`, `vulkan_graphics.rs`, `dxc.rs`, `runner.rs`,
  `hle/d3d12x/*`, `hle/vulkan_linux.rs`, `hle/d3d12x_linux.rs` (null).
- System services (M4+): `xaudio2.rs` + `opus.rs`/`xma2/` (decode done,
  sink headless), `gameinput.rs` + `kbm.rs`, `mfplat*.rs`,
  `dstorage*.rs`, `XGameRuntime` offline paths.
