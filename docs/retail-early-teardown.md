# Retail M1 — Step 64: the ~7 s "teardown" is a failed TLS certificate init

Date: 2026-11-28. Step 64. Base: `linux` @ `60e012b2` (Step 63 docs).
Task `code-assist:xcl-first-frame:step-64:early-teardown`.

## Result in one paragraph

The title never tears its runtime down by itself. `0x14220f3b0` — the
callback Step 63 blamed — is **never called**. What runs at ~7 s is the
engine runtime initializer `0x143da1560`. All eight host subsystem inits
(`0x143da1a80` slots 0 and `+0x28`…`+0x58`) return `1`, then its TLS
certificate-store check `0x145aebb20` returns `0`. The certificate
loader feeds PEM blobs from `0x1465473e0` (`-----BEGIN CERTIFICATE-----`)
through `0x1421688a9`; the certificate date validation at `0x14216662b`
calls `_gmtime64`, whose XCL auto-stub returned NULL, so `0x142166670`
rejected the date and `0x1421793f4` failed with `-151` (`0xffffff69`).
`0x143da1560` logs a fatal at source line 255 (hash `0x406a17af`), rolls
back through `0x143da2880` → `0x143da2fb0`, and the caller continues into
its next global initializer `0x142208ca0`; that dispatches through
`0x14502b2e0` on the host whose first qword is now zero and faults at
`exe+0x502b2f1`.

## Why Step 63 read it as a teardown

- `0x143da19e6` is a tail `jmp 0x143da2880` after the epilogue pops
  `r14/rsi/rdi/rbx`; the return address `0x143da2880` sees is therefore
  the *original* caller `0x1422085b2`, not an address inside
  `0x143da1560`. The two functions are one init/rollback unit.
- The retail PE is stripped and gdb does not consume its `.pdata`, so
  `bt` fabricated frames (`#2 0x10`, `#3 0x8`, …). Only the stop PC and
  the stack word at entry are trustworthy.
- A breakpoint at `0x14220f3b0` (the `lea` target at `0x1458a2409`)
  never fires in `/tmp/xcl-step64-g4`, `-g6`, or the clean runs; that
  registry is not involved in this exit.

## Evidence chain

| stop | value | meaning |
|---|---|---|
| `/tmp/xcl-step64-g4` entry to `0x143da1560` | `ret=0x1422085b2`, `rcx` = stack config | runtime init called from `0x1422085ad` |
| `0x143da1b38` … `0x143da1c0a` | `al=1` ×8 | every host subsystem init succeeds |
| `0x143da1748` (`0x145aebb20` return) | `al=0` | the first post-host check fails |
| `0x143da19cd` | line `255`, hash `0x406a17af` | fatal path of `0x143da1560` |
| `/tmp/xcl-step64-g5` | same single failure | no window/message/exit stub is involved |
| `/tmp/xcl-step64-g6` `0x141d3e9a3` | `eax=-151` | `0x1421793f4` fails inside `0x145aebb20` |
| `/tmp/xcl-step64-g7` `0x1421688a9` | 13 certificate chunks from `0x1465473e0` | TLS CA-bundle load, PEM text |
| static `0x14216662b` | `call [0x146794590]` = `_gmtime64(&time)` | certificate date validation |
| static `0x142166670` | rejects a null `struct tm` | stub return invalidates every date |
| `/tmp/xcl-step64-g2` | `0x143da2fb0` then SIGSEGV on thread `xcl-game-*` | partial host rollback precedes the crash |

`0x142208ca0` then runs (Step 63 saw this initializer); at its
`0x142208ddc` call the debugger read `host=0x7ff3aa09b550` with
`[host]=0`, and `0x14502b2e0` faulted at `[rax+0x40]` with `rax=0`.
The crash is a consequence of the failed init, not a shutdown that the
title requested.

## Fix

`hle/crt_linux.rs` now implements `_gmtime64` for real:

- C-locale UTC breakdown via `libc::gmtime_r`, copied into the MSVC
  nine-`int` `struct tm` (`tm_isdst` included).
- Per-thread result buffer, like the MSVC CRT: the pointer stays valid
  until the next `_gmtime64` on the same thread.
- NULL input and out-of-range values return NULL and set the Windows
  `EINVAL` (22) through the CRT `errno` cell.
- Export table carries `_gmtime64`, so `api-ms-win-crt-time-l1-1-0.dll`
  resolves to code instead of the auto-stub.

## Tests (RED first)

`gmtime64_returns_a_valid_utc_tm`:

- RED with a null-returning skeleton: **0 passed / 1 failed**,
  `epoch 0 must convert`.
- GREEN: epoch `0` → `1970-01-01 00:00:00` (`yday 0`, `year 70`),
  `1_709_210_096` → `2024-02-29 12:34:56` (`mday 29`, `mon 1`,
  `year 124`), same-thread pointer stability, NULL input and `i64::MAX`
  return NULL with `EINVAL`.
- Gates: `check -p xcl --tests` exit 0, `check --workspace` exit 0,
  lib **912 passed / 0 failed / 1 ignored**, launcher **7/0**.

## Retail

- `/tmp/xcl-step64-r1`: first frame at 6.9 s; five presented
  `3840x2160` frames (`6.9 s`, `11.9 s`, `16.9 s` ×3). The TLS check
  returns `1`, the fatal-init line is gone, and `+0x502b2f1` never
  occurs. Exit 139 at `exe+0x18e10c7` at 19.8 s.
- `/tmp/xcl-step64-r2` (fresh root, final binary): first frame at
  6.8 s; exit 139 at the same `exe+0x18e10c7` at 7.9 s. The crash time
  varies with which thread reaches the next stub first; the PC does not.
- The exit is a new frontier, not the old one: the title is no longer
  rolling back the runtime host.

## Next frontier

`exe+0x18e10c7`: `api-ms-win-crt-convert-l1-1-0.dll!strtoll` is still an
auto-stub. The caller (`exe+0x18e109a`) passes `&endptr`; the stub
leaves `endptr` NULL, and the caller then executes
`cmp byte ptr [rax], 0` with `rax=0` (report code bytes
`80 38 00 0f 94 c0 84 c8 …`, fault `0x0`). Implement `strtoll` with the
same contract as the landed `_strtoui64`/`strtod`: longest valid subject
sequence, `endptr` advanced (or the original string), base `0`/`2..=36`,
signed `LLONG_MIN`/`LLONG_MAX` clamping with Windows `ERANGE`, and
`EINVAL` on an invalid base.

## Commits

`crates/xcl/src/hle/crt_linux.rs` (code + test) and this doc.
