# Retail M1 — Step 62: the real msvcp140 loads, the logger crash is gone

Date: 2026-11-28. Step 62. Base: `linux` @ `5a760d80` (Step 61 docs).
Task `code-assist:xcl-first-frame:step-62:real-msvcp`.

## Result in one paragraph

The title's C++ runtime is now the shipped real `msvcp140.dll`, not the
UCRT HLE stubs. Its `DllMain` builds its `ctype<char>` table from the
UCRT primitives added here (`__pctype_func`, `___lc_codepage_func`,
`___lc_locale_name_func`, `___mb_cur_max_func`, `setlocale`,
`_lock_locales`/`_unlock_locales`) and initializes the locales, so the
iostream/`std::mutex` objects the logger uses are real. The
`XblInitialize` telemetry call no longer crashes at `exe+0x1b332ea`: the
first frame still presents (6.9 s) and retail continues into the game.
The next fault is a dangling global at `exe+0x502b2f1`
(`mov rax,[rax+0x40]`, fault `0x40`): a singleton's vtable is zeroed after
it is stored in `0x1498c0f20`, and the block is later reused.

## Fix

- `hle_linux.rs`: `msvcp140.dll` is no longer routed to UCRT. The loader
  maps the shipped DLL; its imports of `vcruntime140`/`api-ms-win-crt-*`
  still resolve through the existing HLE routes. `vcruntime140`,
  `concrt140`, `vcomp140`, `vccorlib140` stay routed for now.
- `hle/crt_linux.rs`:
  - `__pctype_func` — static 256-entry `u16` C-locale table
    (`_UPPER/_LOWER/_DIGIT/_SPACE/_PUNCT/_CONTROL/_BLANK/_HEX/_ALPHA`,
    high bytes unclassified).
  - `___lc_codepage_func` — `0` (`CP_ACP`); XCL's ANSI page is UTF-8 and
    1252 is not an XCL conversion code page.
  - `___lc_locale_name_func` — six `"C"` entries (one per LC_* category).
  - `___mb_cur_max_func` — `1` (C locale is single-byte).
  - `_lock_locales`/`_unlock_locales` — no-ops (immutable C-locale tables).
  - `setlocale` — C-locale contract: `NULL` → `"C"`, `""`/`"C"`/`"POSIX"`
    accepted, other names and out-of-range categories → `NULL`.
- `hle/kernelx_linux.rs`: `InitializeCriticalSectionEx` (flags accepted,
  spin count used), `EncodePointer`/`DecodePointer` (identity; XCL has no
  pointer cookies).

`msvcp140`'s DllMain still calls `_get_stream_buffer_pointers` and
`rand_s`, both stubs; DllMain now completes, so they stay deferred.

## Tests (RED first)

- `c_locale_ctype_table_and_setlocale_match_ucrt`: letter/digit/hex/space/
  control flags, high bytes unclassified, codepage `0`, single-byte
  `mb_cur_max`, six `"C"` locale names, `setlocale` C contract.
- `critical_section_ex_and_pointer_cookies`: `InitializeCriticalSectionEx`
  + enter/leave/delete, pointer identity.
- Route tests: `msvcp140.dll` must **not** route to UCRT; the other four
  CRT DLLs still do. RED with `msvcp140` re-added to the route list:
  **0 passed / 2 failed** (both `bundled_crt_dlls_*` tests). GREEN after
  the fix.
- Full gates: checks 0, lib **910 passed / 0 failed / 1 ignored**,
  launcher **7/0**.

## Retail

First attempts on the way:

- `/tmp/xcl-step62-r1`: the DLL loads, DllMain still faults at
  `msvcp140+0x2f64` on `___lc_locale_name_func` returning NULL.
- `/tmp/xcl-step62-r2`: `___lc_locale_name_func` fixed; DllMain passes;
  crash moves to `exe+0x502b2f1`.
- `/tmp/xcl-step62-r3` (final): `presented 3840x2160` at 6.9 s;
  `loaded msvcp140.dll @ 0x180000000`; DllMain completes; the
  `+0x1b332ea` logger crash is gone; report exit **139** SIGSEGV at
  `exe+0x502b2f1`, fault `0x40`, 40 stubs.

gdb (`/tmp/xcl-step62-g1`…`-g3`):

- `0x1419db790` is a singleton getter (cache `0x1498c1040`, 0x98 bytes);
  its constructor `0x145800410` sets vtable `0x14654cf30`; the init at
  `0x143da1b1c` stores the object at `0x1498c0f20`.
- At the dispatch thunk `0x14502b2e0` the global is the same object but
  its vtable is `0`: the object was destroyed/freed while the global
  still points at it.
- A hardware watch on the object's first qword catches the first write
  from `0x1455aa52a` (`mov [rcx+8*rax+0x30],rdx`, a vector-style
  insertion), i.e. the block has been freed and reused.

## Next frontier

`exe+0x502b2f1`: the lifetime of the singleton behind
`0x1498c1040`/`0x1498c0f20`. Step 63 samples the factory, its owner, the
delete call, and the dispatch's caller `0x142208de1`; if an HLE failure
(HCInitialize / trace setup) makes the owner release the object while the
global remains, fix that call; otherwise close as a title-side
use-after-free with the allocator-reuse evidence.
