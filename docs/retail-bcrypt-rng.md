# Retail Step 82 — Linux CNG randomness (`bcrypt.dll` RNG triple)

The Step-81 artifact (`/tmp/xcl-step81-r1`) calls
`bcrypt.dll!BCryptOpenAlgorithmProvider` (IAT `0x1467942a8`),
`!BCryptGenRandom` (`0x1467942a0`) and `!BCryptCloseAlgorithmProvider`
(`0x146794298`) — 5 times each, from `exe+0x216b436`/`+0x216b44f`/
`+0x216b45f`. The auto-stub returned 0 without writing the algorithm
handle or the 52-byte buffer, so the "random" nonce was stale caller
stack memory.

## The caller

`0x14216b40b` is the title's nonce helper (callers `0x14216abeb` and
`0x14216ade5`):

```
0x216b412  mov  edi, r8d           ; fill length (52)
0x216b415  mov  rbx, rdx           ; caller buffer
0x216b418  lea  rcx, [rsp+0x28]    ; &hAlgorithm (zeroed)
0x216b41d  lea  rdx, [rip+...]     ; L"RNG" @ 0x14678d2ae
0x216b424  xor  r8d, r8d           ; pszImplementation = NULL
0x216b427  xor  r9d, r9d           ; dwFlags = 0
0x216b431  call 0x145f40410        ; jmp [0x1467942a8] Open
0x216b436  test eax, eax ; js -> 0xffffff99
0x216b43a  mov  rcx, [rsp+0x28]    ; hAlgorithm
0x216b43f  mov  rdx, rbx
0x216b442  mov  r8d, edi           ; 52
0x216b445  xor  r9d, r9d
0x216b44a  call 0x145f40420        ; jmp [0x1467942a0] GenRandom
0x216b44f  test eax, eax ; js -> 0xffffff98
0x216b453  mov  rcx, [rsp+0x28]
0x216b458  xor  edx, edx
0x216b45a  call 0x145f40430        ; jmp [0x146794298] Close
```

All three thunks are plain IAT jumps (`objdump -p` lists the imports in
that order). Only a non-negative `NTSTATUS` counts as success, and the
helper zeroes the handle slot before `Open`, so the stub's constant 0
looked like a successful open with handle `0` and an unwritten buffer.

## Contract implemented

New `hle/bcrypt_linux.rs`, registered as the `bcrypt.dll` HLE module with
a Linux-only route (`hle_linux.rs::build`); Windows keeps resolving the
real system DLL. The route also keeps a stray host/title `bcrypt.dll`
from shadowing the shim.

- `BCryptOpenAlgorithmProvider(ph, algId, implementation, flags)`:
  accepts `RNG` and `System preferred RNG`, case-insensitively, reading at
  most 64 UTF-16 units; writes the opaque non-null provider cookie.
  An unknown algorithm is `STATUS_NOT_SUPPORTED` (`0xC00000BB`), a null
  out pointer or null algorithm id is `STATUS_INVALID_PARAMETER`
  (`0xC000000D`). The out slot is zeroed first, so a refused open never
  leaves a handle behind. `pszImplementation`/`dwFlags` are ignored: the
  shim serves one provider regardless of the requested implementation
  name.
- `BCryptGenRandom(h, buf, len, flags)`: fills the buffer through
  `crt_linux::fill_os_entropy` — the same `getrandom` → `/dev/urandom`
  → monotonic-clock-mix convention as `std::_Random_device` and
  `CoCreateGuid`, never a constant and never a buffer left untouched.
  A NULL handle is accepted only with
  `BCRYPT_USE_SYSTEM_PREFERRED_RNG` (`0x2`); any other handle is
  `STATUS_INVALID_HANDLE` (`0xC0000008`), a NULL buffer with a non-zero
  length is `STATUS_INVALID_PARAMETER`, and a zero-length fill is
  `STATUS_SUCCESS` without touching the pointer.
- `BCryptCloseAlgorithmProvider(h, flags)`: `STATUS_SUCCESS` for the
  cookie `Open` issued, `STATUS_INVALID_HANDLE` otherwise.
- All three are in `exports()`.

## Tests

Four tests in `hle/bcrypt_linux` plus the registry test:

- `open_accepts_rng_names_and_sets_a_handle` pins `RNG` / `System
  preferred RNG` in lower, upper and mixed case → non-null handle and a
  successful close.
- `open_refuses_unknown_algorithm_and_bad_pointers` pins `SHA256` →
  `STATUS_NOT_SUPPORTED` with the slot cleared, null out/algorithm id →
  `STATUS_INVALID_PARAMETER`, and an unterminated 96-unit identifier →
  `STATUS_NOT_SUPPORTED` (bounded read, no over-run).
- `gen_random_fills_the_callers_buffer` pins two 52-byte draws that differ
  and are written, plus the NULL-handle + `BCRYPT_USE_SYSTEM_PREFERRED_RNG`
  form.
- `gen_random_rejects_null_buffer_and_foreign_handles` pins the null
  buffer, zero-length NULL buffer, NULL handle without the flag, and
  never-issued handles for both `GenRandom` and `Close`.
- `hle::tests::bcrypt_rng_resolves_to_real_exports` (added first, failed
  `BCryptOpenAlgorithmProvider missing` → RED 0/1) pins that all three
  names resolve through the registry to real code in the `bcrypt.dll`
  module.

Gates: `cargo check -p xcl --tests` 0, `cargo check --workspace` 0,
`hle::bcrypt_linux` + registry **5/0**, lib **958 passed / 0 failed /
1 ignored** (`--test-threads=4`), `launcher_e2e` **7/0**.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step82-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step82-r1`):

| | Step 81 | Step 82 |
|---|---|---|
| first present | 6.964 s | 7.685 s |
| presents | 144 | **601** |
| run length | 12.37 s | 28.33 s |
| exit | 133 SIGTRAP `exe+0x2219651` | 133 SIGTRAP `exe+0x2219651` |
| called stubs | 31 | **28** |

- The three BCrypt names left `called_stubs`, and the log has no BCrypt
  line at all (the Step-81 log had three `[STUB] bcrypt.dll!…` warnings).
  No new handler warning.
- The only other artifact change is scale: the run reached the same
  parked pool-OOM trap (`+0x2219651`) 16 s later and presented 601 frames
  instead of 144, so `PeekMessageA`/allocator `#9`/list
  `#75`/`#126`/`#142` counts are 574/574/573 each instead of 118/118/117.
  The stub-name set is otherwise byte-identical. No causality claim for
  the longer run or the pool fill; that frontier stays parked on the
  console sample (`docs/retail-pool-fill.md`).
- Remaining called stubs by count: `PeekMessageA` 574 (honest null-window
  answer), allocator `#9` 574, list `#75`/`#126`/`#142` 573 each,
  `LocaleNameToLCID` 12, queue `#31` 19, `#131` 6, `#132` 5,
  `__uncaught_exception` 4, `_get_stream_buffer_pointers` 4,
  `IDXGIOutput::#21` 3, `log2`/`_fdtest`/`rand_s`/`getaddrinfo`/`ws2_32!#57`/
  `IGameInput::#9` 2 each.
