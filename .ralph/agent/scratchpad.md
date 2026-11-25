
## 2026-11-25 planner revise (vfprintf)
- 12c closed with review (373 tests, ExitProcess 0xe / 14 known fails). Queue otherwise blocked behind SKIPPED retail-smoke.
- Audit: only remaining MISS UCRT import in both synthetic binaries is __stdio_common_vfprintf (stdio contract; mingw format! path; verbs %s/%g/%p/%llx observed).
- Created task-1795565262-e15d (linux:crt-vfprintf, plan.md step 12d): va_list format engine into CrtFile, %n never honored, + resolver checks + unit test + title e2e gate. Docs (plan/context/progress) updated; commit + tasks.ready next.
