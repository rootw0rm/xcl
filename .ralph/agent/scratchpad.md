
## 2026-11-25 planner revise (vfprintf)
- 12c closed with review (373 tests, ExitProcess 0xe / 14 known fails). Queue otherwise blocked behind SKIPPED retail-smoke.
- Audit: only remaining MISS UCRT import in both synthetic binaries is __stdio_common_vfprintf (stdio contract; mingw format! path; verbs %s/%g/%p/%llx observed).
- Created task-1795565262-e15d (linux:crt-vfprintf, plan.md step 12d): va_list format engine into CrtFile, %n never honored, + resolver checks + unit test + title e2e gate. Docs (plan/context/progress) updated; commit + tasks.ready next.

## 2026-11-25 critic review (vfprintf 12d)
- task-1795565262-e15d PASS, closed. Re-verified: crt 21 passed, full lib 374 passed, workspace check 0 errors, clippy 0 errors, title ExitProcess(0xe)/14 known fails with ucrtbase 75, llvm-readobj 0 MISS UCRT in both binaries.
- No blocking findings; two minors filed as progress.md follow-ups (unknown-verb prefix discard, I32/I64 test gap). Queue still blocked behind SKIPPED retail-smoke, so next is planner triage (plan.revise).

## 2026-11-25 planner revise (format-message 12e)
- 12d closed with review (374 tests, title ExitProcess(0xe)/14 fails, 0 MISS UCRT re-verified). Queue otherwise blocked behind SKIPPED retail-smoke (`ls /run/media/...` absent), `task ready` empty.
- Triage (fresh run here, no code change): `llvm-readobj` shows all CRT imports in both synthetic binaries resolve (0 MISS); title e2e with fresh XCL_DATA_ROOT reaches `ExitProcess(0xe)`/14 fails, `called_stubs: 34` — ~27 Phase-D D3D12 slots + FormatMessageW x1 + DStorageGetFactory x1 + MF* x3 + XclDefinitelyMissingFunction x1 (expected probe).
- New task `linux:format-message` (plan.md step 12e, DEC-017): FormatMessageW in kernelx_linux (Hle(kernelx) via kernel32 route), FROM_SYSTEM/FROM_STRING/inserts/ALLOCATE_BUFFER/truncation + unit test + resolver check + title e2e gate (FormatMessageW gone from called_stubs). Docs (plan/context/decisions) updated; `tasks.ready` next.

## 2026-11-25 builder (format-message 12e)
- task-1795566077-cb3a DONE, commit 43bdabc. FormatMessageW/A in kernelx_linux (FROM_SYSTEM table + HRESULT decode + unknown fallback, FROM_STRING %1 inserts + !specs + %%/%0, ARGUMENT_ARRAY vs va_list, ALLOCATE_BUFFER via HeapAlloc, truncation→122, NULL→87), exports + resolver + hle spot list + kernelx unit test.
- Verify: kernelx format test 1 passed, full lib 375 passed, workspace check 0 errors, clippy 0 errors, title fresh XCL_DATA_ROOT → ExitProcess(0xe)/14 fails, called_stubs 33 (FormatMessageW gone; PresentX FAIL line now shows formatted text). No refactors, Linux-only files + progress.md.

## 2026-11-25 critic review (format-message 12e)
- task-1795566077-cb3a PASS, already closed. Re-verified: format test 1 passed, full lib 375 passed, workspace check 0 errors, clippy 0 errors, title ExitProcess(0xe)/14 fails, called_stubs 33 with zero FormatMessage entries (was 34).
- No blocking findings; one minor filed as progress.md follow-up (%%0 truncation ordering). Queue has open tasks but `task ready` empty (all blocked), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (mfplat 12f)
- 12e closed with review (375 tests, title ExitProcess(0xe)/14 fails, called_stubs 33 zero FormatMessage, PresentX FAIL now formatted). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready` empty.
- Triage (fresh run here, no code change): title log + report.json `called_stubs: 33` = ~27 Phase-D D3D12 slots + DStorageGetFactory x1 + mfplat trio (MFStartup/MFCreateDXGIDeviceManager/MFShutdown x1 each; MFResetDXGIDeviceManagerX imported but never reached) + XclDefinitelyMissingFunction probe x1. Kernel32/oleaut32 leftovers are import-only, zero calls — stay stubs per §4.7b.
- New task `linux:mfplat-headless` (plan.md step 12f, DEC-018): headless mfplat manager in mfplat_linux (Startup/Shutdown/Create/Reset+token + manager vtable, GameInput/XAudio2 null-object pattern) + unit test + resolver checks + title e2e gate (MFPlat PASS, called_stubs 33→30). DStorage stays provisional after (larger surface). Docs (plan/context/decisions) updated; `tasks.ready` next.

## 2026-11-25 critic review (mfplat 12f)
- task-1795566796-4e2a PASS, closed. Re-verified: mfplat 3 passed, full lib 378 passed, workspace check 0 errors, clippy 0 errors, git diff --check clean, title ExitProcess(0xd)/13 fails with [PASS] MFPlat, called_stubs 30 with zero mfplat entries (was 33), zero [STUB] mfplat lines.
- No blocking findings; one minor filed as progress.md follow-up (manual_dangling_ptr on fake handle 1 — deliberate, warning-only). Task text said ExitProcess(0xe) but 0xd/13 is correct after the FAIL→PASS flip; noted in review.
- Queue: task ready empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise) — likely DStorage family next per plan.md 12f rationale.

## 2026-11-25 planner revise (dstorage 12g)
- 12f closed with review (378 tests, title ExitProcess(0xd)/13 fails, called_stubs 30 zero mfplat, [PASS] MFPlat). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready` empty.
- Triage (fresh run here, no code change): title fresh XCL_DATA_ROOT reaches `ExitProcess(0xd)`/13 fails, `called_stubs: 30` = ~28 Phase-D D3D12 slots + DStorageGetFactory x1 + XclDefinitelyMissingFunction probe x1 (expected). Full `hle/dstorage.rs` is 2665 lines + windows_sys surface — too large to port whole; title only uses uncompressed file→memory + phys-pages + cancel + GetHResult + fence signal.
- New task `linux:dstorage-read` (plan.md step 12g, DEC-019): Linux dstorage_linux file-read subset (factory/queue/file/status + sync Submit + fence signal via null-device fence, compression/BCPack/swizzle refused) + unit test + resolver Hle(xcrdapi) check + title e2e gate (DirectStorage PASS, called_stubs 30→29). Docs (plan/context/decisions) updated; `tasks.ready` next.

## 2026-11-25 critic review (dstorage 12g)
- task-1795567260-be32 PASS, closed. Re-verified: dstorage 5 passed, full lib 383 passed, workspace check 0 errors, clippy 0 errors, rustfmt/diff clean, title ExitProcess(0xc)/12 fails with [PASS] DirectStorage, called_stubs 29 with zero xcrdapi entries (was 30).
- No blocking findings; no minor filed (NULL/QI/fence paths fail closed, vtables never NULL). Queue otherwise blocked behind SKIPPED retail-smoke, so next is planner triage (plan.revise).

## 2026-11-25 planner revise (glsl-backend 13)
- 12g closed with review (383 tests, title ExitProcess(0xc)/12 fails, called_stubs 29 zero-xcrdapi, DirectStorage PASS). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready` empty.
- Triage (fresh run here, no code change): title fresh XCL_DATA_ROOT reaches `ExitProcess(0xc)`/12 fails, `called_stubs: 29` = 28 d3d12_xs console slots + XclDefinitelyMissingFunction probe x1 (expected), zero non-GPU calls. All 12 FAILs are null-device D3D12 categories (triangle/Present/command-list/sparse/coherent/descriptor/depth-bias/raytracing/XG-textures); kernel32/oleaut32 leftovers have zero calls and stay stubs per §4.7b. Stub-triage loop exhausted — §4.7 synthetic-ExitProcess gate met (retail leg vacuous, no game files on host).
- New task `linux:glsl-backend` (plan.md step 13, DEC-020): backend-neutral emitter + GLSL 450 backend for blit-class VS (synthetic/in-tree gate, XCL_SAMPLE_RPF opt-in, glslc bring-up only, no shipped shell-out). Docs (plan/context/decisions) updated; `tasks.ready` next.

## 2026-11-25 critic review (glsl-backend 13)
- task-1795567789-dc8b PASS, closed. Re-verified: glsl 2 passed (real glslc 2026.4 compile, not skip), full lib 387 passed, workspace check 0 errors, clippy 0 errors (2 unused-var warnings pre-existing kernelx_linux), rustfmt clean on glsl.rs, git diff --check clean, shipped path spawns nothing (all glslc under #[cfg(test)]), title ExitProcess(0xc)/12 fails/called_stubs 29 matching 12g baseline with DirectStorage + MFPlat PASS holding.
- No blocking findings; two minors filed as progress.md follow-ups (VsBackend placement when step-14 adds SPIR-V backend, TRANSLATOR_SOURCES revisit when caching lands). Queue: `task ready` empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (spirv-inproc 14)
- 13 closed with review (387 tests, title ExitProcess(0xc)/12 fails, called_stubs 29, real glslc 2026.4 gate, shipped path spawns nothing). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready --all` empty.
- Triage: title baseline unchanged (0xc/12 fails, 29 = 28 d3d12_xs slots + probe, zero non-GPU calls); stub-triage exhausted, only Phase-D frontier runnable. GLSL follow-ups (VsBackend placement, TRANSLATOR_SOURCES/caching) folded into step-14 work, not new tasks.
- New task `linux:spirv-inproc` (task-1795568511-058d, plan.md step 14, DEC-021): in-process SPIR-V backend for same blit VS (synthetic/in-tree gate, XCL_SAMPLE_RPF opt-in, spirv-val-or-structural, no shell-out, dep license verified GPL-2.0-compatible or hand-roll). Docs (plan/context/decisions/progress) committed as 6e26aa5; `tasks.ready` emitted.

## 2026-11-25 critic review (spirv-inproc 14)
- task-1795568511-058d PASS, closed. Re-verified: spirv 4 passed (real spirv-val 2026.4 gate, no skip), glsl 2 passed, full lib 391 passed, workspace check 0 errors, clippy 0 errors, git diff --check clean, rustfmt clean on spirv.rs/glsl.rs (mod.rs check shows only pre-existing dxc.rs drift), shipped path spawns nothing (all Command/process under #[cfg(test)]), no new deps, title ExitProcess(0xc)/12 fails/called_stubs 29 matching 12g/13 baseline with DirectStorage + MFPlat PASS holding.
- No blocking findings; no minor filed (wave64 dead_code cosmetic noted in progress.md follow-ups; TRANSLATOR_SOURCES revisit deferred to caching work). Queue: `task ready` and `task ready --all` both empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (vulkan-runner 15)
- 14 closed with review (391 lib tests, title ExitProcess(0xc)/12 fails, called_stubs 29, real spirv-val 2026.4 gate, shipped path spawns nothing, no new deps). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready` empty.
- Triage (fresh run here, no code change): title reaches `ExitProcess(0xc)`/12 fails, `called_stubs: 29` = 28 d3d12_xs console slots + XclDefinitelyMissingFunction probe (expected), zero non-GPU calls. All 12 FAILs are null-device D3D12 categories; stub-triage loop exhausted. Host has real Vulkan devices (Arc A770 / RTX 3090 Ti / Raphael, vulkaninfo 1.4.363, subgroupSize 32) so compute dispatch can run for real.
- New task `linux:vulkan-runner` (plan.md step 15, DEC-022): ash compute runner for caller-supplied SPIR-V compute words (embedded trivial copy/add kernel gate, not VS Vertex words; Kernel-mode recompiler emission deferred as follow-up), SKIPPED pattern when no device, 64 work-items pinned with subgroupSize logged (32/64 selection stays step-16), no window/graphics/routing change. Docs (plan/context/decisions) updated; `tasks.ready` next.

## 2026-11-25 builder (vulkan-runner 15)
- task-1795569389-dcf9 DONE, commit a157022. ash compute runner for caller-supplied SPIR-V words (embedded trivial copy/add kernel gate, 64 work-items pinned, subgroupSize logged).
- Verify: vulkan_runner 3 passed (real Arc A770 dispatch, subgroupSize 32), full lib 394 passed, workspace check 0 errors, clippy 0 errors, title ExitProcess(0xc)/12 fails/called_stubs 29 with DirectStorage + MFPlat PASS.

## 2026-11-25 critic review (vulkan-runner 15)
- task-1795569389-dcf9 PASS, closed. Re-verified: vulkan_runner 3 passed (real Arc A770 dispatch, subgroupSize 32), full lib 394 passed, workspace check 0 errors, clippy 0 errors, rustfmt/diff clean, ash MIT + libloading ISC with runtime dlopen only (ldd/nm clean), shipped path spawns nothing, title ExitProcess(0xc)/12 fails/called_stubs 29 (28 d3d12_xs + probe) with DirectStorage + MFPlat PASS.
- No blocking findings; three minors filed as progress.md follow-ups (skip_test arm collapse, error-path teardown leak, unused _entry param). Queue: `task ready` empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (subgroup-width 16)
- 15 closed with review (394 tests, title ExitProcess(0xc)/12 fails, called_stubs 29, real Arc A770 0..64→*2+1 with subgroupSize 32 logged). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready --all` empty.
- Triage (fresh run here, no code change): `cargo test -p xcl --lib gpu::recomp::vulkan_runner` → 3 passed; `vulkaninfo` subgroupSize 32 + subgroupSizeControl true. Runner pins WORK_ITEMS=64 and only logs subgroupSize — no lane selection yet. Recompiler already owns the 64-vs-split-32 decision (upper_lanes/x64_masks/mesh_wave, runner::split_for/split_wave64).
- New task `linux:subgroup-width` (task-1795570615-1ff3, plan.md step 16, DEC-023): subgroup-width-aware dispatch in vulkan_runner (device subgroupSize via VK_EXT_subgroup_size_control where available, mirror 64-vs-split-32 selection, forced-both-widths unit test) + fold 15 review follow-ups (skip_test arms, teardown leak, unused _entry). Docs (plan/context/decisions/progress) updated; `tasks.ready` next.

## 2026-11-25 critic review (subgroup-width 16)
- task-1795570615-1ff3 PASS, closed. Re-verified: vulkan_runner 5 passed (pure both-widths + on-device agreement + trivial kernel on Arc A770 subgroupSize 32 -> split-32), full lib 396 passed, workspace check 0 errors, clippy 0 errors, rustfmt/diff clean on runner file, shipped path spawns nothing, title ExitProcess(0xc)/12 fails/called_stubs 29 matching 12g/13/14/15 baseline with DirectStorage + MFPlat PASS holding.
- No blocking findings; two minors filed as progress.md follow-ups (_lane binding unused at dispatch, device_subgroup_size throwaway instance). Queue: `task ready` and `task ready --all` both empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (vulkan-backend 17a)
- 16 closed with review (396 tests, title ExitProcess(0xc)/12 fails, called_stubs 29, Arc A770 subgroupSize 32 -> split-32). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready --all` empty.
- Triage (fresh run here, no code change): `cargo test -p xcl --lib gpu::recomp::vulkan_runner` → 5 passed, no skip; `vulkaninfo` still Arc A770 / RTX 3090 Ti / Raphael. Runner selects Native64 vs Split32; step-15 follow-ups folded. Full step-17 present too large + windowed smoke unrunnable headless, so next is selector skeleton.
- New task `linux:vulkan-backend` (plan.md step 17a, DEC-024): Backend::{Null,Vulkan} + select() in front of null D3D12.X device (env override, null default, no-device fallback, consumes device_lane_path(), folds both step-16 follow-ups; no winit/swapchain/window/graphics change, no new deps). Docs (plan/context/decisions/progress) updated; commit + `tasks.ready` next.

## 2026-11-25 critic review (vulkan-backend 17a)
- task-1795571368-affd PASS, already closed. Re-verified: vulkan_linux 7 passed, vulkan_runner 5 passed, full lib 471 passed, workspace check 0 errors, clippy 0 errors, diff-check/rustfmt clean, no new deps, shipped path spawns nothing.
- Title (fresh game + fresh data): default 12 fails/ExitProcess(0xc)/called_stubs 29 with DirectStorage + MFPlat PASS; XCL_VULKAN_BACKEND=vulkan → split-32 lane on Arc A770, still 12/0xc/29 (null device returned); bogus → stays null, 12/0xc/29.
- No blocking findings; two minors filed as progress.md follow-ups (per-create_device probe spam, fresh-XCL_DATA_ROOT methodology note).
- Queue: `task ready` and `task ready --all` both empty; remaining open tasks are code-assist step-13..20 stubs chained behind SKIPPED retail-smoke. Next is planner triage (plan.revise).

## 2026-11-25 planner revise (vulkan-graphics-headless 17b)
- 17a closed with review (471 lib tests, title ExitProcess(0xc)/12 fails, called_stubs 29, env override vulkan→split-32 still 12/0xc/29, bogus→null). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready --all` empty.
- Triage (fresh run here, no code change): `hle::vulkan_linux` 7 passed, `vulkan_runner` 5 passed no skip; `vulkaninfo` still Arc A770 / RTX 3090 Ti / Raphael; `/run/media/anon` absent. Selector routes nothing to Vulkan yet; steps 13/14 VS words never executed on GPU (compute-only runner). PROMPT §5 cheapest-GPU-proof rule fires.
- New task `linux:vulkan-graphics-headless` (task-1795572137-d004, plan.md step 17b, DEC-025): headless offscreen blit-VS draw + readback, SKIPPED when no device, folds 17a probe-spam follow-up. Docs (plan/context/decisions/progress) updated; `tasks.ready` next.

## 2026-11-25 critic review (vulkan-graphics-headless 17b)
- task-1795572137-d004 PASS, closed. Re-verified: vulkan_graphics 5 passed (real Arc A770 dispatch, subgroupSize 32 -> split-32, real spirv-val 2026.4 gate, no skip), vulkan_runner 5 passed, vulkan_linux 8 passed (7 + cache stability), gpu::recomp 163 passed (glsl/spirv gates green), full lib 477 passed, workspace check 0 errors, clippy 0 errors, git diff --check clean, rustfmt clean on vulkan_graphics.rs itself, no Cargo.toml change (no new deps), shipped path spawns nothing (Command/process only under #[cfg(test)]), title ExitProcess(0xc)/12 fails/called_stubs 29 (28 d3d12_xs + probe, zero mfplat/xcrdapi/FormatMessage) with DirectStorage + MFPlat PASS holding; vulkan env → single backend line split-32 still 12/0xc/29, bogus → null 12/0xc/29.
- No blocking findings; two minors filed as progress.md follow-ups (mod ordering + let-else fmt nits, Scope teardown order on error path). Queue: `task ready` empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (vulkan-triangle-vertexid 17c)
- 17b closed with review (477 lib tests, title ExitProcess(0xc)/12 fails, called_stubs 29, real Arc A770 blit draw split-32, spirv-val 2026.4 gate, shipped path spawns nothing, no new deps). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready --all` empty.
- Triage (fresh run here, no code change): `gpu::recomp::vulkan_graphics` → 5 passed, `hle::vulkan_linux` → 8 passed, no skip; `vulkaninfo` still Arc A770 / RTX 3090 Ti / Raphael; `/run/media/anon` absent so retail stays SKIPPED. Selector + cached query route nothing to Vulkan yet; 17b blit path proven via vertex-buffer fetch passthrough, but title `render_triangle` primitive (SV_VertexID (-0.5,-0.5)/(0,0.5)/(0.5,-0.5) + constant green, center-green/corner-blue) never executed on GPU. Full present (winit/swapchain/window + D3D12.X routing) still too large + headless-unverifiable in one slice.
- New task `linux:vulkan-triangle-vertexid` (task-1795572956-6511, plan.md step 17c, DEC-026): hand-assembled VertexIndex triangle VS + constant-green FS through existing 64x64 offscreen path with title pixel asserts, SKIPPED when no device, folds 17b fmt/teardown follow-ups. Docs (plan/context/decisions) updated; `tasks.ready` next.

## 2026-11-25 critic review (vulkan-triangle-vertexid 17c)
- task-1795572956-6511 PASS, closed. Re-verified: vulkan_graphics 9 passed (real Arc A770 dispatch, real spirv-val 2026.4 gate, no skip), full lib 481 passed, recomp 167 passed, selector+runner 13 passed, workspace check 0 errors, clippy 0 errors, diff-check/rustfmt clean, no Cargo.toml/lock/build.rs change (no new deps, no TRANSLATOR_SOURCES), shipped path spawns nothing (Command hits are CommandPool/Buffer only), title ExitProcess(0xc)/12 fails/called_stubs 29 (28 d3d12_xs + probe) with DirectStorage + MFPlat PASS; vulkan env → single split-32 line still 12/0xc/29, bogus → null 12/0xc/29.
- No blocking findings; two minors filed as progress.md follow-ups (draw_offscreen/draw_triangle duplication to factor on third draw, exact-green asserts vs tolerance for future recompiler VS). Both 17b fmt + teardown follow-ups confirmed folded.
- Queue: `task ready` empty (remaining open tasks chained behind SKIPPED retail-smoke, no game files on host), so next is planner triage (plan.revise).

## 2026-11-25 planner revise (recomp-triangle 17d)
- 17c closed with review (481 lib tests, title ExitProcess(0xc)/12 fails, called_stubs 29, real Arc A770 VertexIndex triangle + spirv-val 2026.4, no new deps). Queue otherwise blocked behind SKIPPED retail-smoke, `task ready --all` empty.
- Triage (fresh run here, no code change): `gpu::recomp::vulkan_graphics` → 9 passed, `hle::vulkan_linux` → 8 passed, `vulkan_runner` → 5 passed, no skip; `vulkaninfo` still Arc A770 / RTX 3090 Ti / Raphael; `/run/media/anon` absent so retail stays SKIPPED; fresh title run → ExitProcess(0xc)/12 fails/called_stubs 29 (12 known null-device D3D12 categories, DirectStorage + MFPlat PASS). Offscreen triangle proven but only with hand-assembled SPIR-V — backends cover just the blit subset (no compare/select arms at RDNA2 level), so the title's VertexIndex-indexed constant-select pattern never translated through the recompiler. PROMPT §5 orders emission proof before mesh/GS/pack/present.
- New task `linux:recomp-triangle` (plan.md step 17d, DEC-027): synthetic-RDNA2 VertexIndex-indexed triangle through both backends (narrow compare + cndmask/select, scalar only) with GLSL/SPIR-V parity + offscreen draw with title pixel asserts; folds 17c draw-duplication follow-up (third draw factors common helpers), tolerance follow-up stays deferred. Docs (plan/context/decisions/progress) updated; commit + `tasks.ready` next.
