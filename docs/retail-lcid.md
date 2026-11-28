# Retail Step 83 — `LocaleNameToLCID` (deterministic locale table)

The Step-82 artifact (`/tmp/xcl-step82-r1`) calls
`api-ms-win-core-localization-l1-2-0.dll!LocaleNameToLCID` 12 times
(IAT `0x146793a98`, report return address `exe+0x22175b9`). The auto-stub
returned 0, so the title's locale init took its early return and never
converted the name it had just obtained from the real
`GetUserDefaultLocaleName` (`"en-US"`).

## The caller

`0x142217550` is the locale-init helper (wrapper `0x142217520`, called
from `0x14195fae4` and `0x1427ebd05`); it returns an LCID-derived value
through its `rcx` out pointer:

```
0x2217563  mov  rsi, rcx              ; out (u32)
0x2217566  mov  dword ptr [rcx], 0    ; out = 0
0x2217570  call 0x1408490d0           ; zero another local
0x2217575  lea  rcx, [rbp-0x80]       ; LOCALE_NAME_MAX_LENGTH buffer
0x2217579  mov  edx, 0x55
0x221757e  call [0x146793a90]         ; GetUserDefaultLocaleName(buf, 0x55)
0x221758e  je   0x221760c             ; failure -> GetLastError path
0x2217590  lea  rdx, [0x14678d2c2]    ; L"en-001"
0x2217597  lea  rcx, [rbp-0x80]
0x221759b  call [0x146794888]         ; wcscmp(buf, L"en-001")
0x22175a7  je   0x2217616             ; equal -> leave out = 0
0x22175ad  lea  rcx, [rbp-0x80]
0x22175b1  xor  edx, edx              ; dwFlags = 0
0x22175b3  call [0x146793a98]         ; LocaleNameToLCID(buf, 0)
0x22175bd  test eax, eax
0x22175bf  je   0x2217616             ; 0 -> leave out = 0
0x22175c9  mov  rcx, rdi              ; wcslen(buf)
0x22175cc  call [0x146794898]
0x22175d6  lea  eax, [rax+2*rax+3]    ; 3 * len + 3
0x22175e5  call 0x145647140           ; stack probe
0x22175ea  sub  rsp, rax              ; alloca
0x22175ed  mov  rbx, rsp
0x22175f4  mov  rcx, rbx              ; UTF-8 destination (r8d = 3*len+3)
0x22175f7  mov  rdx, rdi              ; UTF-16 source
0x22175fa  call 0x14221f710           ; UTF-16 -> UTF-8
0x22175ff  mov  rcx, rsi              ; out
0x2217602  mov  rdx, rbx              ; UTF-8 name
0x2217605  call 0x1406891d0           ; name hash + locale hook
```

So a non-zero LCID is the *only* condition for the conversion
continuation: `0x14221f710` is a bounded UTF-16→UTF-8 writer (one to
three bytes per unit, NUL-terminated, `r8d` = capacity), and
`0x1406891d0` hashes the UTF-8 string (`0x144fc21d0`, a djb2-style
hash) and invokes the locale hook table at `0x1474464c0` when present
(writes 0 when absent). The caller `0x142217520` then publishes the
out value through `0x1471b8c74`.

## Contract implemented

`hle/kernelx_linux.rs::LocaleNameToLCID(const wchar_t *name, u32 flags)`
maps a BCP-47 name to a Windows LCID with a fixed table and no OS locale
backend:

| name | LCID | notes |
|---|---|---|
| `en-US` | `0x0409` | user default (the real `GetUserDefaultLocaleName` answer) |
| `en-GB` | `0x0809` | |
| `en-001` | `0x2409` | the title's compare string |
| `""` (`LOCALE_NAME_INVARIANT`) | `0x007F` | |
| `!x-sys-default-locale` | `0x0800` | `LOCALE_NAME_SYSTEM_DEFAULT` |
| `en` | `0x0009` | neutral; only with `LOCALE_ALLOW_NEUTRAL_NAMES` (0x0800_0000) |
| null | `0x0409` | `LOCALE_NAME_USER_DEFAULT` = the fixed `en-US` user |

Names compare case-insensitively (`eq_ignore_ascii_case`), as Win32
locale names do. Flags outside 0x0800_0000, an unknown name, and a neutral name
without that flag return 0 with `ERROR_INVALID_PARAMETER`; successful calls
set `ERROR_SUCCESS` (module convention). Registered in `exports()`, so
the unrouted api-set import resolves through the cross-HLE name lookup
to the kernelx module instead of a logging stub.

## Tests

`locale_name_to_lcid_maps_the_fixed_table` starts with the export
assertion (parent tree failed with `LocaleNameToLCID must be wired`,
RED 0/1; GREEN 1/0). It pins all table rows, case-insensitivity, the
empty-string and `!x-sys-default-locale` names, the null user default,
neutral `en` refused without 0x0800_0000 and accepted with it, an unknown name
and a flag outside 0x0800_0000 both returning 0 with `ERROR_INVALID_PARAMETER`.

Gates: `cargo check -p xcl --tests` 0, `cargo check --workspace` 0,
focused **1/0**, lib **959 passed / 0 failed / 1 ignored**
(`--test-threads=4`), `launcher_e2e` **7/0**.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step83-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step83-r1`):

| | Step 82 | Step 83 |
|---|---|---|
| first present | 7.685 s | 7.158 s |
| presents | 601 | **265** |
| run length | 28.33 s | 16.59 s |
| exit | 133 SIGTRAP `exe+0x2219651` | 133 SIGTRAP `exe+0x2219651` |
| called stubs | 28 | **27** |

- `LocaleNameToLCID` left `called_stubs` (12 calls in Step 82) and the
  `[STUB] …!LocaleNameToLCID` line is gone. The called-stub set is
  otherwise byte-identical: the run is 27 names (the set difference is
  exactly `LocaleNameToLCID`).
- This is the first run with a non-zero LCID on that path (`en-US` is
  the exact buffer `GetUserDefaultLocaleName` wrote, so the table's
  0x0409 answer is unconditional). The UTF-16→UTF-8 conversion and the
  `0x1406891d0` hash/hook lookup are therefore executed; the run shows
  no fault at `0x14221f710`/`0x1406891d0` and no new warning.
- The crash is the same parked allocator/pool-OOM trap
  (`SIGTRAP` at `exe+0x2219651`, pc `0x142219651`, fault address 0).
  Present count and run length vary between runs (Step 81: 144/12.37 s,
  Step 82: 601/28.33 s) and are not attributed to this change. No
  causality claim for the pool fill; that frontier stays parked on the
  console sample (`docs/retail-pool-fill.md`).
- Remaining called stubs by count: `PeekMessageA` 240 (honest
  null-window answer), allocator `#9` 240, list `#75`/`#126`/`#142` 239
  each, queue `#31` 18, `#131` 6, `#132` 5, `__uncaught_exception` 4,
  `_get_stream_buffer_pointers` 4, `IDXGIOutput::#21` 3,
  `getaddrinfo`/`ws2_32!#57`/`_fdtest`/`log2`/`IGameInput::#9` 2 each.
