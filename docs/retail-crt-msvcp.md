# Retail Step 86 — the CRT calls `msvcp140` makes (`rand_s`, `_get_stream_buffer_pointers`)

Date: 2026-11-28. Base: Step 85 (`e1e41076`). Task key:
`code-assist:xcl-first-frame:step-86:crt-msvcp-fake-success`.

The shipped `msvcp140.dll` calls two CRT imports from its own DLL init.
Both resolved to the generic auto-stub: `rand_s` returned 0 without
writing the word (so its caller returned an uninitialized stack dword as
"random"), and `_get_stream_buffer_pointers` returned 0 without writing
the three out slots. The latter never crashed only because the caller
pre-zeroes those slots; the contract was still wrong.

## Input (Step-85 artifact)

`/tmp/xcl-step85-r1/GTA6.Xbox.v1.0.2.1-CYBERLEEK/logs/report.json`,
`called_stubs` (23 names), both entries logged on `ThreadId(1)` during
`msvcp140.dll` `DllMain`:

| import | calls | first caller | log |
|---|---|---|---|
| `api-ms-win-crt-utility-l1-1-0.dll!rand_s` | 1 | `msvcp140.dll+0x38f1f` | 0.143 s |
| `api-ms-win-crt-stdio-l1-1-0.dll!_get_stream_buffer_pointers` | 4 | `msvcp140.dll+0x6417` | 0.045 s |

## Call sites inside `msvcp140.dll`

`objdump -p msvcp140.dll` pins the IAT slots:

```
DLL Name: api-ms-win-crt-stdio-l1-1-0.dll
 000555c0  <none>  0039  _get_stream_buffer_pointers
DLL Name: api-ms-win-crt-utility-l1-1-0.dll
 000556d8  <none>  001c  rand_s
```

`rand_s` is called from the small helper at `0x180038f10`, which returns
the drawn word in `eax` and takes a fatal path on a non-zero `errno_t`:

```
180038f10  sub   rsp, 0x28
180038f14  lea   rcx, [rsp+0x30]      ; unsigned int *value
180038f19  call  qword ptr [0x1800556d8]
180038f1f  test  eax, eax
180038f21  jne   0x180038f2c          ; error path -> 0x18003acc0
180038f23  mov   eax, [rsp+0x30]
180038f27  add   rsp, 0x28
180038f2b  ret
```

The auto-stub's "0 with no write" therefore looked successful and handed
back whatever the caller's stack slot held, not a random value.

`_get_stream_buffer_pointers` is called from the `filebuf`/stream
initializer at `0x180006388` with `(FILE*, &base, &ptr, &count)`:

```
1800063ee  test  rdi, rdi             ; stream
1800063f1  je    0x18000643e
1800063f3  and   qword ptr [r11+0x8], 0     ; base out zeroed
1800063fc  and   qword ptr [r11+0x10], 0    ; pointer out zeroed
180006405  and   qword ptr [r11+0x20], 0    ; count out zeroed
18000640e  mov   rcx, rdi
180006411  call  qword ptr [0x1800555c0]
180006417  mov   rax, [rsp+0x30]      ; base slot
180006424  mov   rax, [rsp+0x38]      ; pointer slot
180006431  mov   rax, [rsp+0x48]      ; count slot
```

`r11` is the entry `rsp`, so `rsp+0x30/0x38/0x48` are the three out slots
after the push/sub. The caller copies each into the stream object's
buffer fields (`[rbx+0x18]/+0x38/+0x50`).

## Contract implemented

The UCRT header (`winsdk-10` `ucrt/stdio.h:73-78`) declares the real
signature as `char*** _Base, char*** _Pointer, int** _Count` — three
pointer-sized out slots, not the plan's `char**`/`int**` pair. Each store
is 8 bytes.

`hle/crt_linux.rs`:

| export | behaviour |
|---|---|
| `rand_s(unsigned int *value)` | fills `*value` from `os_entropy_u32`, falling back to the existing monotonic-clock `entropy_fallback` (the same convention as `std::_Random_device`; no second RNG), and returns 0. Null → 22, `errno = 22`, no write |
| `_get_stream_buffer_pointers(FILE*, char***, char***, int**)` | XCL's `CrtFile` is unbuffered (`fread`/`fwrite` go straight to the host file), so the true answer is a null triple: writes null base, null pointer, null count and returns 0. Null stream or any null out slot → 22, `errno = 22`, nothing written |

Both are registered in `crt_linux::exports()`, which is the single HLE
route for every `api-ms-win-crt-*`/`ucrtbase` import.

## Tests (RED first)

`crt_linux::tests::rand_s_and_stream_buffer_pointers_follow_the_ucrt_contract`
asserts both names are in `exports()`, then calls them through
`extern "win64"` transmuted pointers.

- RED with only the test grafted: `rand_s must be wired`, **0/1**.
- GREEN at the fix: **1/0**; `hle::crt_linux` module **35/0**.

Pins: two `rand_s` draws differ; null value is 22 with `errno = 22`; a
real `fopen`ed stream yields three nulls and 0; null stream and each null
out slot are 22 with sentinels intact.

Gates: `cargo check -p xcl --tests` 0, `cargo check --workspace` 0, lib
**966 passed / 0 failed / 1 ignored** (`--test-threads=4`), `launcher_e2e`
**7/0**.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step86-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step86-r1`, debug build, no gdb):

| | Step 85 | Step 86 |
|---|---|---|
| first present | 7.701 s (token 0x1) | 7.050 s (token 0x1) |
| presents | 562 (through 0x232, 27.038 s) | **340** (through 0x154, 19.528 s) |
| exit | 133 SIGTRAP `+0x2219651` | **133 SIGTRAP `+0x2219651`** (pc `0x142219651`) |
| called stubs | 23 names | **21 names** |

- The `called_stubs` set difference is exactly
  `api-ms-win-crt-utility-l1-1-0.dll!rand_s` and
  `api-ms-win-crt-stdio-l1-1-0.dll!_get_stream_buffer_pointers`; both also
  leave `stubbed_imports`.
- The `[STUB]` warning diff is exactly those two removed lines; no new
  warning. The run is otherwise unchanged: same parked pool-OOM trap,
  same pc, no fault in `msvcp140`'s init.
- Present counts/run length vary with the pool-OOM timing (Step 81: 144,
  Step 82: 601, Step 83: 265, Step 84: 148, Step 85: 562); no causality
  claim.

## Semantics summary

| name | this branch | UCRT contract |
|---|---|---|
| `rand_s` | live OS entropy, 0; null → 22 | `errno_t`, 0 on success, EINVAL on null |
| `_get_stream_buffer_pointers` | null/null/null, 0; null arg → 22 | returns the stream's buffer trio; unbuffered stream has none |
