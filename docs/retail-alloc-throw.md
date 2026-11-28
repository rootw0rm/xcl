# Retail Step 67 — the `+0x2219651` trap is the title's own 2.15 GiB aligned-allocator OOM

Date: 2026-11-28. Step 67. Base: `linux` @ `131fa7d1` (Step 66 docs).
Task `code-assist:xcl-first-frame:step-67:allocator-refusal`.

## Result in one paragraph

The failure after the first frame is not a stubbed HLE allocator. The title's
own `operator new(size, align)` path asks its fixed virtual allocator for
`0x1560000` bytes (21.4 MiB) aligned to `0x10000`; that allocator's region is
a title-sized `0x89700000`-byte (2.15 GiB) `XMemVirtualAlloc` block, and its
free-run scan finds nothing that large. The trap is the title's fatal OOM
diagnostic: the allocator's slot 0 returns 0, the new-handler flag is set, and
the diagnostic virtual slot returns, taking the `int3` after it. No
`_CxxThrowException` and no `.pdata` frame is involved, so the missing Linux
unwinder is **not** this frontier. The 2.15 GiB size is the title's static
Anaconda constant (XCL's `ConsoleMode` answer selects the same row); neither
XCL's registry answer nor any HLE call is between the request and the refusal.
No product code changed.

## Sample (one instrumented fresh-root gdb run)

`/tmp/xcl-step67-g1`, debug binary at HEAD, fresh root, breakpoints planted
after `xcl::loader::run_main`:

| probe | hits | values |
|---|---|---|
| `0x144fac3f1` (slot 0 returned 0) | 1 | `rbx=0x1471c2808`, `rdi=size=0x1560000`, `rsi=align=0x10000`, `[rbx+0x18]=1` (new-handler flag) |
| `0x142219620` (throw path) | 1 | `rcx=0x1471c2808`, `rdx=0x1560000`, `r8=0x10000`, vtable `0x1461883a0`, slot 0 `0x14221a620`, `[vtable+0x30]=0x14221a690`, `[allocator+0x18]=1` |
| `0x14221964d` (indirect call) | 1 | `rax=0x1461883a0`, target `0x14221a690` |
| `0x14221a690` (target) | 1 | `rcx=0x1471c2808`, `rdx=0x1560000`, `r8=0x10000`, return `0x142219650` → `int3` |
| `xcl::hle::d3d12x_linux::VulList_TimestampBottom` | 0 | this run takes the allocator frontier |

The caller of the aligned `new` is `0x140f7da34` (function `0x140f7d7c0`),
reached through `0x140cbb020` (= `jmp 0x14221ad20`), the title's throwing
aligned-`new` shim. At `0x142219620` entry the caller return is at `[rsp+0x50]`:
`0x14221ad20` restores its own `rsp` before the tail `jmp`, so `0x144fac3c0`'s
frame is the only one above it and `[rsp+0x48]` is a saved register, not a
return address. `bt` is not useful on these guest frames (no frame pointers);
the offset is the reliable datum.

The plain gate run on the same tree (no gdb): first frame `presented
3840x2160`, exit **133** SIGTRAP at `+0x2219651`, `pc 0x142219651`, fault 0,
**44** called stubs — identical to Step 66's r2. The reproducible gate
frontier is the allocator, not the `va=0x1` write.

## Static map

```
0x141e1cf60  init the title's per-class allocator table
0x141e1ced0  copy the size row selected by device class
0x140075140  ConsoleMode == 6 (Series S)? 0x1455deb88 reads
             HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM\ConsoleMode
             and maps 0x60..0x68; 0x64 (Series X) is not 6
0x146186250  static size row 0 (Series S): 9 qwords; [3] = 0x4fd00000
0x1461862a0  static size row 1 (not Series S): 9 qwords; [3] = 0x89700000
0x141e1d350  XMemVirtualAlloc(0, size, 0x20003000, 0x183, 0x22004)
             (IAT 0x146793a58)  ; region base 0x7ff315500000
0x141e1d53d  construct allocator 0x1471c2808: region size, base,
             block 0x1471c28b0, granule 0x20000
0x14aad45b0  class table row 0; class 2 entry 0x14aad45c8 = 0x1471c2808

0x1461883a0  allocator vtable
  slot 0  = 0x14221a620   allocate(size, align)
  slot 2  = 0x1453ea000   free(ptr, size)
  slot 6  = 0x14221a690   OOM diagnostic (the +0x30 call)
0x14221a620  -> 0x1410ca6d0(allocator+0x38, size, max(align,4))
0x1410ca6d0  region free-run scan; returns 0xffffffff on no fit

0x144fac3c0  new(allocator, size, align):
             slot 0 == 0 -> if [allocator+0x18] -> 0x140005970() (the
             new-handler check) -> 0x142219620(allocator, size, align)
0x142219620  reference the static exception object 0x1471be5d8, call
             vtable[0x30] with (allocator, size, align), then int3
0x14221a690  diagnostic: count = [allocator+0x4c] = 0x22d,
             table = [allocator+0x40] = 0x1471c38b0,
             test [table + 20*0x22d + 8] & 0x3ffffff (nonzero -> ret)
```

The throw path is a fatal assert: `int3` is the unreachable marker after the
diagnostic call, and the `mov rcx,r14; …; jmp 0x144d9dab0` after it would only
release the diagnostic object. There is no exception dispatch, so Windows
would trap here too unless the allocation had succeeded.

## Allocator state at the refusal

Sampled object `0x1471c2808` (outer, 0xa0 bytes) and its in-line region state
at `outer+0x38`:

| field | value |
|---|---|
| `[+0x20]` region base | `0x7ff315500000` |
| `[+0x28]` metadata block | `0x1471c28b0` |
| `[+0x30]` region size | `0x89700000` (2.15 GiB) |
| `[+0x38]` region state base | `0x1471c28b0` |
| `[+0x40]` block table | `0x1471c38b0` |
| `[+0x48]` head / count | `0x0000_022c` / `0x0000_022d` (556 / 557) |
| `[+0x50]` granule | `0x20000` |
| `[+0x58]` cumulative used bytes | `0x8946e6d0` |
| `[+0x60]` region size (inner) | `0x89700000` |
| `[+0x68]` | `0x10` |
| `[+0x78]` size-guard field | `0` (guard off; the refusal is the free-run scan) |

The scan's used-byte counter (`add [region+0x20], size`) is
`0x8946e6d0` of `0x89700000`, so the pool holds only
`0x291930` = 2.57 MiB free in total (0.12 %) — less than the 21.4 MiB
request before any fragmentation is considered. The other three early
failure branches are excluded by the sampled state (size-guard `0`, block
count `0x22d` nonzero, entry `0x22d` flags nonzero), so the scan's no-fit
return (`0x1410ca8d6` -> `0x1410cabbf`) is the one taken. The 557-entry
free list holds no run that large. The allocator is the title's own
2.15 GiB aligned-allocation pool, nearly full at the first frame.

## Why this is not an XCL seam

- The region is title-owned memory obtained through `XMemVirtualAlloc`
  (`0x141e1d350`); XCL only supplies a valid host block of the requested
  size. The size comes from the title's static row, not from a query.
- The row selection is `ConsoleMode != 0x60`. XCL answers Series X as `0x64`
  and Series S as `0x60` (`hle/kernelx.rs::console_mode`), which selects the
  same rows as the console: 2.15 GiB (Anaconda) / 1.25 GiB (Lockhart). XCL's
  answer cannot explain a refusal; the Series X row is the larger one.
- The free/alloc decisions are title code; XCL makes no HLE call between the
  caller's request and slot 0's refusal.
- The fatal path is an assert, not a C++ throw: no `_CxxThrowException`, no
  `.pdata` lookup, so the unimplemented Linux unwinder is not involved.

Do not patch the `int3`, do not fake a non-null allocation, and do not grow
the pool: the console uses the same constants and the same code.

## Second frontier: `VulList_TimestampBottom(va=0x1)`

Step 66's post-fix gdb run (`/tmp/xcl-step66-g4`) aborted much later at
`hle/d3d12x_linux.rs:2338` with `va = 0x1` (Rust misaligned-pointer panic in
a debug build). This gate run does not reach it: the allocator trap fires at
~12 s, before the timestamp path's frame cadence.

The plain run's report adds a new consumer-visible input for that frontier:
the title calls newer-layout command-list slots that Linux leaves unset —

```
ID3D12GraphicsCommandList::#139  calls=2  first_caller 0x2474cd4  args 0x0 0x0
ID3D12GraphicsCommandList::#74   calls=1  first_caller 0x2484b12  args 0x7 0x1
```

(Linux `v[10]` is the PC Reset; `#139` is a runtime stub. `#74` is a runtime
stub; the Windows twin maps newer `#74` -> older `#72` Wait64BitValueX and
newer `#139` -> older `#10` Reset via `NEWER_LIST_SLOTS`.) So this title's
command list is on the newer layout, exactly like its queue (Step 51's
`Queue_Slot16`). Under that layout Linux's `v[70] = VulList_TimestampBottom`
is the wrong method: newer `#70` is older `#68`, OMSetDepthBounds. That
explains a non-address first argument but does not by itself resolve what the
title passes; the guest call site for `#70` was not sampled in this step.

Do not add a blind `va < page` guard. The next unit must sample the guest call
site (slot `#70`) in a run that presents several frames, then either adopt
the newer-list remap or resolve the small argument per that call site.

## Reopen triggers (no work queued)

1. A console/Windows sample at the same `0x140f7da2f` request showing a free
   run >= `0x1560000` in `0x1471c2808+0x38` (then XCL's HLE changed the
   title's memory state; compare the free list and the used/free counters).
2. An allocation/free trace of this pool showing a title-side free that the
   console performs but XCL's D3D12/fence emulation prevents.
3. The `#70` guest call site from a multi-frame run, plus a decision on
   `NEWER_LIST_SLOTS` for the Linux command-list vtable.

## Gates

At `131fa7d1` (no product code touched):

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **915 passed / 0 failed /
  1 ignored**.
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Retail evidence

Fresh root `/tmp/xcl-step67-r1`, debug binary at HEAD, no gdb:

- first frame `presented 3840x2160` at 6.803 s;
- exit **133** SIGTRAP at `game_scarlett_master.exe+0x2219651`,
  `pc 0x142219651`, fault 0, code bytes
  `4c 89 f1 48 83 c4 28 5b 5f 5e 41 5e e9 4e 44 b8`;
- **44** called stubs, identical set/frontier to Step 66 r2;
- the gdb run `/tmp/xcl-step67-g1` also presented the frame (line
  `D3D12.X Vulkan: presented 3840x2160`).

Launch budget: two fresh-root runs (one instrumented, one plain gate), no
timing retries.
