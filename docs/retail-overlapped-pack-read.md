# Retail M1 — the pack NULL was an ignored `OVERLAPPED` read offset

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-37:overlapped-read`.
Tip: `linux` @ `da63d986` + this change. Product code + one test.

## Result in one paragraph

The title's RPF9 pack reader issues its header/table reads through
`ReadFile` with a non-NULL `OVERLAPPED` whose `Offset` selects the file
position. XCL's `ReadFile` ignored the fifth argument and read from the
handle's shared position, which an earlier 0x1000-byte setup read had left at
`0x1000`. The 16-byte root header read (a special-cased direct read) was
correct, but the 0x350-byte body landed at file `0x1010` instead of `0x10`,
so the parsed pack kind byte `H[0x2e]` became `0x0b` (class 5, whose
registration key is the all-zero default and is refused) instead of the
file's real `0x09` (class 4, whose key is valid). The refused cache child
made the producer store NULL and the title dereferenced it at
`exe+0x2237320`. Honoring the `OVERLAPPED` offset makes the class-4
registration succeed and moves the boot ~22 MB of code further:
exit **133 SIGTRAP at `exe+0x3751e65`** (the `int3` at `0x3751e64`, reached
after a negative return), with 9 called stubs and the pack cache loaded.

## The read path

```
0x141fb2600  header/table parser
  read #1: reader(0x1478f5e80), ctx, off=0,     dst=H,      len=0x10    direct
  read #2: reader,              ctx, off=0x10,  dst=H+0x10, len=0x350   page path
0x140cdd930  stream read: page-aligns the offset through the page cache
0x140313fc0  page cache: inner read(aligned offset) into 0x1479fb000, then copy
0x140314200  device read: ReadFile(handle 0x1013e, buf, len, &got,
             &OVERLAPPED{Internal=0, InternalHigh=0, Offset=<aligned>, OffsetHigh=0})
```

Observed at `0x140314045`/`0x140313f37`:

```
ReadFile(h=0x1013e, n=0x1000, ov.off=0, ov.hi=0)   <- logical offset 0
```

Before the fix the returned page equalled `appdata0.rpf[0x1000..]` (the
handle position left by the preceding 0x1000-byte read), so the stitched
`H` was `file[0..0xf] + file[0x1010..]`; `H[0x2e] = 0x0b`. The file bytes
are `appdata0.rpf[0x2e] = 0x09` and `[0x102e] = 0x0b`.

## Fix

`crates/xcl/src/hle/kernelx_linux.rs`:

- `ReadFile` and `WriteFile` with a non-NULL `OVERLAPPED` now seek to
  `Offset | (OffsetHigh << 32)` before the transfer and publish a synchronous
  completion (`Internal = STATUS_SUCCESS`, `InternalHigh = transferred`).
  The handle is left at `offset + transferred`, matching the Windows
  non-`FILE_FLAG_OVERLAPPED` contract the title relies on.
- New test `overlapped_file_io_uses_the_explicit_offset` pins both calls.
  RED at the pre-fix tree: the read asked for offset 0 with the handle parked
  at 8 returned `[89 ab]` instead of `[30 31 32 33]`; GREEN at this tree and
  the write half replaces only the requested bytes.

## Verification

- `cargo test -p xcl --lib -- --test-threads=4`: **871 passed / 0 failed /
  1 ignored** (the new test included).
- `cargo check -p xcl --tests` and `cargo check --workspace`: exit 0.
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Retail, fresh root `/tmp/xcl-it3/retail2`: exit **133**, SIGTRAP at
  `game_scarlett_master.exe+0x3751e65`, pc `0x143751e65`, code bytes
  `48 81 c4 50 02 00 00 5e c3 cc cc …`; **9** called stubs.
- gdb at the registration gates (one run): the kind byte is now
  `bl = 0x09` → class 4, and `0x1408ec221` returns `al = 1` for every pack
  (six hits), i.e. the cache child initializes instead of being refused.

## New frontier (next step, in this order)

The crash is the title's own `int3` at `0x3751e64` after
`0x140938790(2, &struct)` returned negative; the log immediately before it
records `QueryApiImpl 03f0fe74-fdd9-4e5c-b630-f9339c47acc5/35f07670-… →
XDisplay: missing` and `XErrorReport 0x89240107: XGameRuntime is missing
required dependencies`. New called stubs since the pack loaded:

| Stub | Calls | First caller |
|---|---|---|
| `isspace` | 204 | `exe+0x28d80ab` |
| `__stdio_common_vsscanf` | 6 | `exe+0x12da567` |
| `GetUserDefaultLocaleName` | 2 | `exe+0x2217584` |
| `atoi` | 2 | `exe+0x28c77f8` |
| `_stricmp` | 2 | `exe+0x128bbc7` |
| `qsort` | 1 | `exe+0x227b245` |
| `srand` | 1 | `exe+0x23cefc8` |

`ws2_32#115` and `IGameInput::#9` are unchanged. Triage those seven as real
CRT semantics first (one stub pack, failing-first tests), then the missing
`XDisplay` `QueryApiImpl` object; the `int3` site's struct at `+0x24` is fed
by `0x143751e70`, so re-trace after the stubs are real before routing D3D12.
No d3d12 calls yet.

## Correction to earlier M1 docs

`docs/retail-packcache-seeder.md`, `docs/retail-inner-init.md`,
`docs/retail-fault-outslot.md`, `docs/retail-createobject-chain.md` and
`docs/retail-obj1-initializer.md` described the class-5 refusal as a
game-structural dead end. That branch analysis is accurate, but it was not
the boot path: the `H[0x2e] = 0x0b` premise came from the wrong file region
caused by this XCL read bug. The reopen trigger "a console-equivalent pack
whose byte 0x102e is not 0x0b" is moot — the byte the title reads is `0x09`.
Do not re-probe the class-5 chain; this document supersedes it.
