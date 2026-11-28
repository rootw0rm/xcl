# Retail Step 80 — `CoCreateGuid` + `StringFromGUID2`

The Step-79 artifact (`/tmp/xcl-step79-r1`) died in `_CxxThrowException`
0.65 s after the XGameSave init answered `E_NOTIMPL`. On the same
thread, in the same millisecond, the title had called two auto-stubs that
both write an output parameter:

| import | IAT | report return address |
|---|---|---|
| `CoCreateGuid` | `0x146793dc0` | `exe+0x55f3865` |
| `StringFromGUID2` | `0x146793df0` | `exe+0x55f3883` |

`CoCreateGuid` returned 0 (S_OK) without writing the GUID; the caller had
zeroed the 16-byte slot, so it formatted an all-zero GUID.
`StringFromGUID2` returned 0 without writing the caller's 50-unit buffer,
and `0x1455f3893` immediately constructed a `std::wstring` from that
unwritten stack buffer. The unhandled throw is on that path; with both
out-parameters real the throw disappears (see the delta below).

## The caller

`0x1455f3820` (the title's GUID-to-string helper):

```
0x55f385f  call [0x146793dc0]        ; CoCreateGuid(&guid at rsp+0x48)
0x55f3865  test eax,eax ; js ...     ; HRESULT check
0x55f387d  call [0x146793df0]        ; StringFromGUID2(&guid, rsp+0x60, 0x32)
0x55f3883  test eax,eax ; js ...     ; <0 is the failure test
0x55f3893  call 0x1455f4010          ; assign the buffer to a std::wstring
0x55f38bf  cmp byte [rax], '{'       ; optional brace strip
```

The caller tests the `StringFromGUID2` result with `js` (signed), so 0 is
read as success; returning 0 without a write is the worst combination.

## Contract implemented

`hle/kernelx_linux.rs`:

- `CoCreateGuid(GUID *pguid)` — version 4 GUID: 16 bytes from the shared
  OS entropy helper (`crt_linux::fill_os_entropy`, also used by
  `std::_Random_device`), version nibble `0x4` in byte 7, RFC 4122
  variant bits in byte 8. `E_INVALIDARG` for a null pointer (no write).
  The GUID is written in memory order (Data1/2/3 little-endian), so
  `StringFromGUID2` and the title's own `memcmp`/hashes see the same
  bytes as on Windows.
- `StringFromGUID2(REFGUID, LPOLESTR, int cch)` — the brace form with
  uppercase hex, as the Win32 implementation:
  `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`. Returns **39** (38 UTF-16
  units + NUL) and writes the terminator in the last unit. `cch < 39`, a
  null buffer or a null GUID returns 0 and writes nothing.
- Both are registered in `exports()`.

## Tests

`guid_creation_and_formatting_follow_the_win32_contract` resolves both
names through `exports()` and transmutes (parent tree fails with
`CoCreateGuid must be wired`, RED 0/1; GREEN 1/0). It pins: null out
pointer, version/variant bits, two draws differ, a known GUID byte
pattern → the exact uppercase string, the 39/0 length boundary
(38 writes nothing, 39 writes the NUL), and null/zero-length arguments
leaving the buffer untouched.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step80-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step80-r1`):

| | Step 79 | Step 80 |
|---|---|---|
| first present | 6.898 s | 7.015 s |
| presents | 105 | **135** |
| exit | 134 SIGABRT (`_CxxThrowException`) | **133 SIGTRAP `exe+0x2219651`** |
| called stubs | 33 | **32** |

- `CoCreateGuid`/`StringFromGUID2` left `called_stubs`; the
  `__std_exception_copy` / `_CxxThrowException` abort is gone.
- The XGameSave `E_NOTIMPL` at 10.004 s is now non-fatal: the title
  continues for another 1.2 s and reaches the parked pool-OOM assert.
- Frontier returns to the known allocator trap (`+0x2219651`), with 5
  more presents than the Step-78 artifact. No causality claim for the
  pool fill; that frontier stays parked on the console sample
  (`docs/retail-pool-fill.md`).
- Next called stubs by count (this artifact): `PeekMessageA` 110 (honest
  null-window answer), `_wstat64` 9, `LocaleNameToLCID` 12, BCrypt RNG
  5 each, `log2`/`_fdtest` 2 each.
