# Retail M1 — Step 54: the page-table tail is a title-side short copy

Date: 2026-11-28. Step 54. Base: `linux` @ `a90f2bae` (Step 53 docs).
Task `code-assist:xcl-first-frame:step-54:audio-table-tail`. Docs only: the
measured stop is title-side, so no product code and no zeroing workaround.

## Result in one paragraph

The `exe+0x25a087d` walk over the 3627-entry offset table does not fail on a
dirty XCL commit. The table is allocated by the title's own CRT heap (the
phase-2 `new(count * 4)` returns the address of the phase-1 `new(count * 8)`
table it just freed), and the title's page-cached stream then fills **only the
first 0x1FF8 bytes (2046 of 3627 dwords)** while reporting the full 14508-byte
read. Every XCL `ReadFile` on the backing pack is byte-exact and returns its
full count, including the 8192-byte refill that immediately follows the
partial copy. The tail is therefore never written: pre-filling it with a
sentinel proves every remaining dword keeps the sentinel (1579 from `0x2000`,
1581 from `0x1FF8`). Entries
`0..=0x7fd` are a coherent strictly increasing offset run (`0x9d8`, `0x9e0`,
… `0x16f00`); entry `0x800` (`0xdb098fcc`) is heap leftovers, and
`base + off - 8` faults. The measured stop is inside the title's stream
layer; zeroing the allocation would not unblock the walk: `off = 0` gives
`r9d = 0xfffffff8`, so the access is `base + 4 GiB - 8`, still outside the
mapped base.

## File attribution correction

Step 53 blamed `G:\audio.rpf` from "last reads at the fault". A
`CreateFileW` → handle trace gives:

| handle | path |
|---|---|
| `0x10145` | `G:\data.rpf` (460 MB) |
| `0x1014a` | `G:\audio.rpf` (28 GB) |

The table stream is built on **handle `0x10145` = `G:\data.rpf`**: the first
`ReadFile` after the partial copy, and every refill for this reader, is on
`0x10145`. The content is audio because the resource is the
`audio:/config/audio_common.dat62.rle` data (that path string sits on the
walk's stack); it is stored in `data.rpf`. `audio.rpf` reads at the fault are
a concurrent thread, not this table.

## The read that stops early

Phase 1 of the function reads `1907` 8-byte records (15256 bytes); phase 2
reads `3627` 4-byte entries (14508 bytes). Both `new` calls return the **same
address**, e.g. `0x7ff39fa94cd0`, so the phase-2 table starts as the freed
phase-1 bytes.

At the phase-2 read (`exe+0x25a0840`, helper `0x1451d2260`):

```
reader:  [0x10]=page base, [0x18]=0x5e004,
         [0x20]=4 (count consumed), [0x24]=0x1000, [0x28]=0x1000;
         [0x8] is the 0x1000-byte page buffer handed to `xsgetn`
source:  [reader+0] -> object with vtable 0x1462fd480,
         xsgetn = [vtable+0x50] = 0x140314330, fast path 0x141330560
```

- The helper's initial `memcpy` copies `0x1000 - 4 = 0xFFC` bytes from its page
  buffer (`crt_linux.rs:676`; gdb backtrace shows XCL's `memcpy`).
- It then asks the source for the remaining **10416** bytes at
  `table + 0xFFC`.
- The fast path copies exactly one more `0xFFC` fragment and returns `10416`.
- The helper returns `0xFFC + 10416 = 14508`; the reader's virtual offset
  advances `0x5e004 → 0x618b4` (14512 bytes).

The writes stop at table byte `0xFFC + 0xFFC = 0x1FF8`. The table content is
consistent with that: dwords `0..=0x7fd` are the coherent offset run
(`0x9d8, 0x9e0, …, 0x16f00`, 2046 entries); everything from `0x7fe` on is
pre-existing heap data.

### Static map of the helper's branch

`0x1451d2260` is an `sgetn` over a page-cached reader (`rcx` = reader,
`rdx` = destination, `r8d` = count; fields `+0x0` inner source, `+0x8` page
buffer, `+0x10` page base, `+0x18` stream offset, `+0x20` consumed, `+0x24`
page length, `+0x28` page capacity):

- `0x1451d2290`: `count <= page length - consumed` → copy and return.
- `0x1451d22e2` / `0x1451d237a`: otherwise copy the `page length - consumed`
  residue (`0xFFC` here), advance the offset by the full page and shrink
  `count`; fall into the direct/buffered decision.
- `0x1451d2302`: `cmp count, [reader+0x28]`. `count >= capacity` takes the
  direct path at `0x1451d2346`; otherwise it refills `[reader+0x8]` and
  copies from the page.
- `0x1451d2346`: calls the inner source's `xsgetn` (inner vtable `+0x50`, the
  `0x140314330` → `0x141330560` stream) with `(inner, [reader+0x8],
  destination, count)` and the still-unconsumed `10416`; adds the return to
  the copied total and zeroes consumed/length at `0x1451d236d`.

The capacity is `0x1000`, so the `10416`-byte remainder can never take the
buffered branch: it is handed whole to the title's pack stream one call
after the `0xFFC` residue copy. The next consumer-visible inputs are inside
that stream: the block table read via `0x14550db40` in the loop at
`0x141330621` and the writers at `0x1413307e0` / `0x1413308e0`.

## Sentinel proof that the tail is never written

At `exe+0x25a0840`, gdb pre-fills the table tail with a sentinel, then
continues to the walk:

| fill start | sentinel dwords still intact at the fault |
|---|---|
| `0x2000` | 1579 / 1579 dwords |
| `0x1FF8` | 1581 / 1581 dwords (the whole tail, incl. `0x7fe`/`0x7ff`) |

So the read neither wrote nor used the tail; the coherent offsets stop at
`0x1FF8`, not at the crash index. In a clean run the stale dwords at
`0x7fe`/`0x7ff` happen to be plausible offsets (`0x118b4ffd`, `0x10`), which
is why Step 53 read entries `0..=0x7ff` as "valid"; they are leftovers and
fault as soon as one is out of the mapped base at `0x800`.

## XCL read verification

The refills are not short on the XCL side. A gdb `FinishBreakpoint` compared
the destination buffer of **every** `ReadFile` on handle `0x10145` with a
non-NULL `OVERLAPPED` against `pread` of `data.rpf` at the same
`Offset | OffsetHigh << 32`: **0 mismatches**, and the `done` count equalled
the request each time (the refill right after the partial copy is an
`8192`-byte request with `done=8192`). Sample offsets are valid page offsets
(`0`, `0x1000`, `0xf000`, `0x10000`, `0x1b236000`, …).

The allocation is also not XCL's: the title's `operator new`
(`0x144f97130`, i.e. `exe+0x4f97130` → `0x144d9c130` → `0x140003a10`) goes to
the title's own heap object at `gs:[0x58]`; it reuses the freed phase-1 block.
XCL's `VirtualAlloc`/`XMemAlloc` commit zeroing is not on this path, and a
fill would not help (`off = 0` indexes `base + 4 GiB - 8`).

## Verdict and next input

Title-side for the page-cached helper, but only at that layer: the source
`xsgetn` returns success because its block decode goes through DirectStorage,
and XCL's Linux slice refused the block's `ZlibDecompress` request. Step 55
(`docs/retail-stream-short-copy.md`) found and fixed that refusal; this
document's title-side wording covers the helper only. The next consumer-visible input
is that stream's refill/copy contract; the backing `ReadFile`s already carry
the full 8192 bytes.

With the table tail manually zeroed, the walk completes and retail moves to
`exe+0x9dfd93` (`mov rcx,[r8]`). That is the next frontier to sample **if**
the tail is removed as a factor; do not add an XCL fill workaround for it.

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- Retail `/tmp/xcl-step54-r1` (fresh root, debug binary at this tree): first
  frame `presented 3840x2160`, exit 133 in the shell, report SIGSEGV
  `exe+0x25a087d`, **26** called stubs — unchanged from Step 53.
- Re-confirmed at `c5318bcd` (fresh root `/tmp/xcl-step54-r2`): same frame,
  same crash PC/code bytes, 26 stubs; `check -p xcl --tests` 0,
  `check --workspace` 0, lib **892/0/1**, launcher **7/0**.

No product code changed; the gdb fills above were diagnostic only.
