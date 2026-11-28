# Retail Step 79 — real `strncpy_s`

`api-ms-win-crt-string-l1-1-0!strncpy_s` had no HLE handler, so the
resolver installed the generic auto-stub: the thunk returns 0 in `rax`
and never writes the destination. Retail calls it **35×** per fresh-root
run (`objdump -p` IAT `0x146794850`, ordinal `0x90`; the report's first
return address is `game_scarlett_master.exe+0x2748fd5`).

## Why a "success" that writes nothing is wrong

Three static sites call the import; each zeroes a 256-byte stack buffer,
asks for a bounded copy, then hashes the buffer and compares the hash to a
constant:

| site | destination | size | count | consumer after the call |
|---|---|---|---|---|
| `0x142748fcf` | `[rsp+0x120]` | `0xff` | `[r8+8]` | terminator at `[rsp+count+0x120]`; `0x144fc2180` hash; `cmp 0xfe25ef42` |
| `0x142748ff7` | `[rsp+0x20]` | `0xff` | `r9` | same hash/compare pair |
| `0x14375e3e7` | `[rbx]` | `0x100` | `r9` | `0x141b14050` hash of the copied string |

With the stub the destinations stay all-zero, so every name lookup hashed
the empty string and took the miss branch (`cmp ...; sete al` at
`0x1427490db`). This is a consumer-visible wrong answer, not a logging
gap.

## Contract implemented

`hle/crt_linux.rs::strncpy_s(dest, destsz, src, count)`:

| input | result |
|---|---|
| `dest == NULL` or `destsz == 0` | `EINVAL` (22), `errno = 22`, no write |
| `count == 0` | `dest[0] = 0`, returns 0 |
| `src == NULL` | `dest[0] = 0`, `EINVAL` |
| `count >= destsz` and `strlen(src) >= destsz` | `dest[0] = 0`, `EINVAL` — the source cannot fit, never truncated |
| `count >= destsz` and `strlen(src) < destsz` | copies the whole source plus NUL, returns 0 |
| `count < destsz` | copies `min(count, strlen(src))` plus NUL (`dest[count] = 0` on truncation), returns 0 |

Never writes past `destsz`, never pads the tail (unlike `strncpy`), and
does not invoke the invalid-parameter handler (XCL's handler is a no-op
unless the title installs one).

## Tests

`strncpy_s_copies_bounded_and_terminates` resolves the name through
`exports()` and transmutes it, so the parent tree fails with
`strncpy_s must be wired` instead of a compile error (RED 0/1 at
`6b574cfc`, GREEN 1/0). It pins: the short-source copy with the tail
sentinel intact, truncation at `count`, `count >= destsz` with a source
that fits (success) and one that does not (`EINVAL`, `dest[0] = 0`, no
byte past `destsz` written), `count == 0`, null source, zero `destsz`, null
`dest`, and the `errno` value.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step79-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step79-r1`):

- `strncpy_s` is gone from `called_stubs` (35 → 33 entries).
- First present `3840x2160` at **6.898 s**; **105** presents.
- Exit **134** SIGABRT (not the previous 133 SIGTRAP at
  `exe+0x2219651`): at 10.415 s, ThreadId(24) runs
  `XGameSaveInitializeProviderAsync → E_NOTIMPL`, then
  `CoCreateGuid` (`exe+0x55f3865`), `StringFromGUID2`
  (`exe+0x55f3883`), `__std_exception_copy` (`exe+0x55f2ea4`) and
  `_CxxThrowException`; XCL has no C++ unwinding and aborts.
- The GUID helper `0x1455f3820` writes the GUID through
  `CoCreateGuid` into a zeroed 16-byte buffer and then formats it with
  `StringFromGUID2` into an **unwritten** 50-unit stack buffer. Both
  imports are auto-stubs, so the string the title builds from that buffer
  is stack garbage (`0x1455f3893` assigns it to a `std::wstring`). The
  throw is the likely consequence of that garbage
  ([INFERENCE] — the run was the only launch allowed for this step);
  the deterministic defect is the two unset output parameters, and that
  is the next step (`CoCreateGuid` + `StringFromGUID2`).
- No causality claim for the parked pool-OOM frontier: this run never
  reached the `0x2219651` pool assert.
