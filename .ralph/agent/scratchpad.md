
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
