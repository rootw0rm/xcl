
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
