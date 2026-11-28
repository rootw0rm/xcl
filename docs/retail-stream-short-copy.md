# Retail M1 — Step 55: the short pack copy was a refused DirectStorage zlib request

Date: 2026-11-28. Step 55. Base: `linux` @ `35d8df83` (Step 54 docs).
Task `code-assist:xcl-first-frame:step-55:stream-short-copy`.

## Result in one paragraph

The `10416`-byte request at `exe+0x25a087d` is not a title-side copy bug.
The stream's first fragment (`0xFFC`) is the residue of a partially consumed
block, fine. The remaining `6324` bytes are one pack block whose decoded size
matches the remainder exactly, so the stream hands it whole to its bulk
reader `0x1413307e0`. That reader builds a **DirectStorage request with
`ZlibDecompress` set** and enqueues it on an XCL `IDStorageQueueX`
(`hle::dstorage_linux`). The Linux slice refused every zlib request with
`E_DECOMPRESSION`, so the destination was never written; the title's wait
path did not turn that refusal into a short `xsgetn`, which credited the full
`10416` and returned. Porting the Windows zlib stage into the Linux
DirectStorage slice fixes the copy: retail's first frame still presents and
the frontier moves from `exe+0x25a087d` to `exe+0x43aea86`.

## Sample (bounded gdb)

Five physical fresh roots (`/tmp/xcl-step55-g1`…`-g5`), over the task's
"<=2" guideline: `-g2` (breakpoint condition read a caller-saved register)
and `-g3` (Python `MemoryError` in a stop handler) were harness failures
with no new state, and the remaining three were needed to go from the stream
state (`-g1`) to the request contents (`-g4`) to the host target identity
(`-g5`). All artifacts are kept for review rather than hidden.

At the `xsgetn` with `r9d = 10416` (stream `0x7ff3a0196e80`, reader
`0x147956b10`, dest `table+0xFFC`):

- reader `+0x10008 = 0xd0011`, `+0x10010 = 0x5f004`,
  `+0x10018 = 0x24f30`; inner state `+0x5c = 5` (current block),
  `+0x60 = 0x9e18` (staged mode), `+0x68 = 0xf004` (consumed in block 5).
- `[rbp+0x60] != 0` takes the staging path at `0x1413306c2`: block 5's
  decode is `{+0=0xa30e, +4=2, +8=0x10000, +0xc=0xa310, +0x10=3}`, so
  `r9d = 0x10000 - 0xf004 = 0xFFC`; `0x1413308e0` copies those `0xFFC`
  bytes and **returns 4092** (not 10416).
- The map loop then runs once for block 6 (`r13 = 6`, `edi = 6324`):
  `0x14550db40` decodes `{+0=0x836, +4=0xa, +8=0x18b4, +0xc=0x840,
  +0x10=3}`. `0x18b4 = 6324` is exactly the remaining byte count, so the
  loop leaves its copy path and calls the bulk writer `0x1413307e0` with
  `dest = table + 0xFFC + 0xFFC`, `size = 6324`, block count 1.
- `0x1413307e0` builds the request at `0x1428c9960`: options `+0 = 1`
  (`ZlibDecompress`), destination `+8 = table + 0x1FF8`,
  destination size `+0x18 = 0x18B4`, then calls
  `[ [0x1479f6d70] + 0x18 ] -> vtable[+0x18]`.

That last call is **XCL code**: the runtime target was
`0x555555d11630`, which is `xcl::hle::dstorage_linux::queue_enqueue_request`
at the host PIE base plus `0x7bd630`. The object at `[0x1479f6d70] + 0x18` is
an XCL DirectStorage queue created for the pack reader. The older
`[global] + 0x10` object at `0x1478f5e80` (vtable `0x1462fc550`) is the
title's own pack manager; its `+0x230` (`0x1428c97c0`) is what drives the
per-block decode and eventually enqueues the DStorage request.

## Why nothing was written

`hle/dstorage_linux.rs::Request::check` refused any request with bit 0 set:

```rust
if self.options & ZLIB_BIT != 0 || self.bcpack() != 0 || self.swizzle() != 0 {
    return Err(E_DECOMPRESSION);
}
```

The entry was queued as `Refused`, `Submit` executed nothing, and the
destination kept its previous bytes (the `0xAAAA…` sentinel from Step 54
survived at exactly `dest + 0xFFC`). The stream nevertheless saw the block as
delivered: the map loop accounts `[out+8]` (the decoded size) as copied,
sets `edi = 0`, and the epilogue returns `ebx - eax = 10416`. This is why
Step 54 measured a "successful" full read that stopped at `0x1FF8`.

## Fix

`hle/dstorage_linux.rs` now implements the Windows slice's zlib stage
(`hle/dstorage.rs::execute` is the source of truth):

- `Request::check` accepts `ZlibDecompress` and applies the same size rules:
  with `intermediate_size == 0` the destination must be larger than the
  compressed source; otherwise `source_size < intermediate_size <=
  destination_size`.
- `execute` reads `source_size` bytes at `offset` and inflates with
  `miniz_oxide::inflate::decompress_to_vec_zlib_with_limit` to
  `intermediate_size` (or the destination size when it is 0), requiring the
  exact inflated length and zero-padding up to the destination size. A bad
  stream or a length mismatch returns `E_ZLIB_DATA` and writes nothing.
- BCPack, swizzle and memory sources still fail closed with the same
  `E_DECOMPRESSION`/`E_SOURCE_TYPE` values.

## Evidence

- RED (test grafted onto `35d8df83` in `/tmp/xcl-step55-red`): status slot
  held `E_DECOMPRESSION` (`0x89240030`, -1994129360) where the test expects
  `S_OK`; 0 passed / 1 failed.
- GREEN at the fixed tree: 893 passed / 0 failed / 1 ignored (lib),
  launcher 7/0; `check -p xcl --tests` and `check --workspace` exit 0.
- Retail `/tmp/xcl-step55-r1` (fresh root, debug binary):
  `D3D12.X Vulkan: presented 3840x2160`, exit 139, report SIGSEGV at
  `game_scarlett_master.exe+0x43aea86` (`pc 0x1443aea86`, fault `0x0`,
  code bytes `48 8b 01 ff 50 30 85 c0 0f 88 8e 01 00 00 c7 44` — a null
  `this` before `call [rax+0x30]`). The `+0x25a087d` walk is gone.
- New frontier: `exe+0x43aea86`. Not queued here; the planner owns the next
  step.

## Static map used for the sample

- `0x141330560` outer `xsgetn`; `[rbp+0x60] != 0` → staged residue path
  `0x1413306c2`, else the map loop `0x141330621`.
- `0x14550db40` block-descriptor decoder; flag 3 yields
  `o8 = 0x10000 - (desc & 0xffff)` (a `0xF004` remainder is the `0xFFC`
  fragment).
- `0x1413307e0` bulk writer; `0x1428cfa70` request wrapper; the queue call is
  `[ [0x1479f6d70] + 0x18 ] -> vtable[+0x18]`.
- `0x144f657a0` is the title's queue-completion wait.
