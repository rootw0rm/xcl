
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
