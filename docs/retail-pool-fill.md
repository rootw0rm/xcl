# Retail pool fill provenance (Step 69)

Step 68 left the retail title presenting `3840x2160` and trapping in its
aligned `new` at `game_scarlett_master.exe+0x2219651` (SIGTRAP, exit 133).
The allocator is the title's own fixed virtual pool at `0x1471c2808`; the
pool reported `0x8946e6d0` used of `0x89700000` at the Step-67 sample. This
step answers the Step-67 reopen trigger: **who consumes the pool, and did
XCL change that?**

Verdict: the fill is title-side. Every byte committed in the sampled run
came from seven title `.text` call sites; XCL's 18 `__XMemAllocationHooks__`
allocations (103.18 MiB) were answered from a different title heap
(`0x1000000000…`), not this pool. No free was refused. No XCL seam exists
between the requests and the refusal.

Evidence: one gdb-instrumented fresh-root run
(`XCL_DATA_ROOT=/tmp/xcl-step69-g1`, debug binary at `76faea82`), followed
by the static map below. The launch budget (≤2) allowed no second run.

## Static map

### The allocator object

`0x1471c2808` is built by `0x141e1cf60` as class index 2 of the title's
allocator-row table (`0x14aad45b0`; row `0x14aad45c8`). The constructor
writes the row at `0x141e1d3f2`; the class-2 branch is
`0x141e1d418`–`0x141e1d542` and calls `0x145b1a7d0`:

| offset | field | sample |
|---|---|---|
| `+0x00` | vtable | `0x1461883a0` |
| `+0x20` | region base VA | `0x7ff315500000` |
| `+0x28` | block table | `0x1471c28b0` |
| `+0x30` | region size | `0x89700000` |
| `+0x38` | region state | `0x1471c28b0` |
| `+0x40` | live block table | `0x1471c38b0` |
| `+0x48` | `{head,count}` | `0x22d_0000022c` |
| `+0x50` | granule | `0x20000` |
| `+0x58` | used counter | sampled below |

The vtable's pool methods:

| slot | target | role |
|---|---|---|
| 0 | `0x14221a620` | allocate `(size, align)` |
| 1 | `0x140bb7060` | |
| 2 | `0x1453ea000` | free `(ptr)` |
| 6 | `0x14221a690` | failed-alloc new-handler (ends in `int3`) |

`0x145b1a7d0` stores the base and region and calls
`0x14554f680(region = allocator+0x38, size, granule, table)`. The region
constructor writes `[region+0x20] = 0` at `0x14554f721`, so the **used
counter starts at zero**; the 2.18 GiB is real allocations, not seeded
state.

### Pool size provenance

The class-2 region size is a title static row, not a query. `0x141e1ced0`
asks `0x140075140` whether `ConsoleMode == 6` (Series S), flips that byte
(`xor al, 1`) and selects row `0x146186250 + 0x50 * index`: row 0
`0x146186250` for Series S, row 1 `0x1461862a0` for every other mode. It
copies the nine qwords at `row + 0x08` into `0x1471bf9b0` (two 32-byte
`vmovdqu`/`vmovups` plus one qword); the copied entry 2 — raw row qword 3
— is `0x4fd00000` (1,338,978,304 B) in row 0 and `0x89700000`
(2,305,818,624 B) in row 1. XCL answers `ConsoleMode 0x64` (Series X), so
retail takes row 1.

`+disableSharedPools` (`0x1422103b0`) is consulted only after that copy
(`0x141e1cf18`). When set, the adjust at `0x141e1cf33` subtracts
`0xdc00000` from copied entry 7 (`0x1471bf9e8`) and adds
`0x1461597a0` to entries 4/5 (the `vpaddq` at `0x1471bf9d0`); entry 2 is
never touched. No host query or dynamic resize sits behind `0x89700000`.

The allocation core is `0x1410ca6d0(region, size, align)`; it commits at
`0x1410cab64` (`add [r14+0x20], r11`, `r11` = committed block size) and
returns the block offset. The free core is `0x1453ea050(region, offset)`;
it unlinks the live entry and subtracts at `0x1453ea0d3`
(`sub [rcx+0x20], rdi`, `rdi = [entry+0x10]`). Entry layout, 20 bytes:
`+0x8` flags/next (bit 26 = live), `+0xc` block offset, `+0x10` size. The
`+0x58` used counter is therefore exactly the sum of live entry sizes.

### Allocation routes

`0x144fac3c0` (aligned `new`) has **305 static references**: 278 `call` and
27 `jmp` thunks. It looks up `[allocator]` slot 0 for nonzero sizes
(`0x140104070`), then on a null result runs the new-handler check and calls
`0x142219620` (the `int3` throw path). `0x14221ad20` and `0x142474690` are
alignment wrappers that select the allocator first and tail-jump to
`0x144fac3c0`.

The instrumented run shows the pool is reached only through
`0x14221a620`; `0x1410cab64` had **548 commits and 0 direct-core commits**
(`r14 = 0x1471c2840`). No other wrapper fed this region.

### The XMem hook side

The exe exports `__XMemAllocationHooks__` at RVA `0x6844380` with
`{0x144deb420, 0x144e38ec0}` (confirmed from the export table; export
ordinal 1, name at `0x6791a78`). XCL adopts that pair
(`hle/kernelx_linux.rs::adopt_title_hooks`).

- `0x144deb420` → `0x144deb430` takes the allocator byte
  (`attrs` bits 16..23, `bextr eax, esi, 0x810`), walks the hook chain at
  `0x146843370[byte]`, and calls the registered allocator. The static
  default node `0x1461889b0` is `{0x144deb540, 0, -1}` and
  `0x144deb540` jumps to IAT `0x146793a08` = `XMemAllocDefault`.
- `0x144e38ec0` → `0x144e38ed0` does the same through `0x146843b80`;
  the default free node is `0x1461889d0` → `0x144e38fa0`.
- Titles register their own GPU-memory classes with `0x145c8f990` /
  `0x145c8fa00` (nodes tagged by the `0x144de8620` class). The pool is
  class 2 of the separate allocator-row table (`0x14aad45c8`), not an XMem
  hook node; the XMem hook path never selects it in this title.

## Instrumented sample

One breakpoint set after `xcl::loader::run_main`; aggregation in gdb
Python, summary only. Every pool allocation was captured (548 successes;
the last request is the fatal `0x1560000`), so no sampling threshold was
needed.

At the trap:

- used `[+0x58]` = `0x8846e6d0` = 2,286,347,984 B = **2180.43 MiB**
- region `0x89700000` = 2,305,818,624 B = 2199 MiB
- free = 0x1291930 = **18.57 MiB (0.84 %)**
- 548 successful allocations, 1 failed (`0x1560000`, align `0x10000`,
  caller `0x140f7da34`), 542 live blocks, 6 frees (0.26 MiB)
- total requested 2,286,615,744 B; committed 2,286,616,272 B;
  freed 268,288 B; live 2,286,347,984 B — used matches live exactly

### Consumers (live bytes at the trap)

| consumer site | containing function | n live | live bytes | share | largest |
|---|---|---|---|---|---|
| `0x14193e248` | `0x14193de10` (resource path, aligned-alloc `0x142474690`) | 156 | 1,262,878,720 B (1204.38 MiB) | 55.2 % | `0x5560000` |
| `0x140075452` | `0x140075440` / fail-fast `0x1456abf90`, `dl=1` | 3 | 524,292,096 B (500.00 MiB) | 22.9 % | `0x1f400000` |
| `0x14248ee63` | `0x14248e070` (`alloc [rsi+0x5b4]` → `[rsi+0x50]`) | 1 | 240,894,464 B (229.73 MiB) | 10.5 % | `0xe5bc200` |
| `0x14006dce7` | `0x14006dc20` (resource path, `0x142474690`) | 328 | 220,796,112 B (210.57 MiB) | 9.7 % | `0x5a00000` |
| `0x140f7da34` | `0x140f7d7c0` (surface-size alignment) | 1 | 22,413,312 B (21.38 MiB) | 1.0 % | `0x1560000` |
| `0x1456feb11` | `0x1456feaf0` (`0x140cbb020` → `0x14221ad20`) | 1 | 11,534,336 B (11.00 MiB) | 0.5 % | `0xb00000` |
| `0x14193df3c` | `0x14193de10` (second call site) | 52 | 3,538,944 B (3.38 MiB) | 0.2 % | `0x30000` |

The 500 MiB site is one block of `0x1f400000` plus two blocks summing
4 KiB. Its direct consumer is the `dl=1` branch of the fail-fast helper
`0x1456abf90` (`0x140075230` → `jmp 0x140075440` → aligned `new`
`0x14221ad20`); that helper's static callers are `0x14022cb16`,
`0x145b1a71a` (size `rcx` into `0x145b1a6e0`) and `0x145c0c469` (size
`[rax]` after `0x144ee1860`).

`0x14022cb16` is the **BVH prebuild → alloc → build loop**, not
`GetResourceAllocationInfo`:

```
14022cb0c  call 0x1456abf80        ; prebuild: jmp 0x1456ac100
14022cb11  mov  rcx, [rsp+0x28]    ; ResultDataMaxSizeInBytes
14022cb16  call 0x1456abf90        ; alloc(size) + assert
14022cb21  call 0x1456abfb0        ; build the node
```

`0x1456abf80` is `jmp 0x1456ac100`, which calls
`XboxBvhBuilder::GetRaytracingAccelerationStructurePrebuildInfo` through
IAT `0x146793f98` and stores `ResultDataMaxSizeInBytes` /
`ScratchDataSizeInBytes`. `0x1456abfb0` then builds the node from that
allocation. The third caller `0x145c0c469` is the other BVH prebuild
path (`0x144ee1860` → `jmp 0x140075480` → the same IAT slot), and it was
the live caller in the Step-66 gdb sample
(`docs/retail-failfast-helper.md`). Whichever of the two callers emitted
the block, the 500 MiB belongs to the BVH builder, not to resource
allocation info; the Step-69 run records the helper, not which caller,
and the launch budget forbade a second run to split it.

`0x145b1a6e0` also allocates `0x51000` at `0x145b1a73d` on its first
call; that block is absent from the 548 allocations and from the live
table, so this caller did not run.

Requested-size shape (all sites): `0x10000` ×111, `0x12c0000` ×32,
`0x20` ×16, `0x80` ×14, `0x4` ×12, `0x10` ×11, `0x8000` ×11,
`0x1560000` ×10, `0x60000` ×10, `0x20000` ×10, `0x30000` ×9,
`0x4b0000` ×9, `0x4000` ×9.

Live block-size buckets (542 blocks): `<16K` 198, `16K–64K` 37,
`64K–256K` 163, `256K–1M` 32, `1M–4M` 30, `4M–16M` 29, `16M–64M` 49,
`64M–256M` 3, `≥256M` 1. Sum = `0x8846e6d0`.

### Fill trajectory

`Queue_PresentX` sampled `[0x1471c2808+0x58]`; 65 presents before the trap.

| present | used |
|---|---|
| 0 | 231.37 MiB |
| 1 | 463.63 MiB |
| 2–19 | 466.76 MiB |
| 20–49 | 470.98 MiB |
| 50–59 | 471.14 MiB |
| 60 | 687.91 MiB |
| 62 | 1339.29 MiB |
| 63 | 1498.40 MiB |
| 64 | 2080.60 MiB |
| trap | 2180.43 MiB |

The pool is flat at ~471 MiB through the first 50 presents and then takes
~1.7 GiB in the burst at presents 60–64. The fill is a title load/stream
burst, not first-frame display-target or descriptor-heap memory.

### XCL's own allocations

`0x144deb430` was hit 18 times, all from host caller `0x555555ff003a`
(XCL's `runtime_alloc` wrapper), never from title `.text`:

- 16 × `attrs = 0x30804004` (`D3D_DESCRIPTOR_HEAP`), 2 × `0x30808000`
  (`D3D_DISPLAY_TARGET`); sizes `0x2600000`, `0x1ff8000` ×2, `0x18700` ×11,
  `0x20000`, `0x10000`, `0x1000` ×2
- total 108,195,072 B (103.18 MiB)
- every return was in `0x1000000000…0x100ba90000`, **none in the pool**
  `0x7ff315500000…0x7ff3fa700000`
- `0x144e38ed0` was not hit: XCL made no hook free in this run

So XCL's 103 MiB is real and comes from the title's XMem hooks, but from a
different title heap. It does not move the `0x1471c2808` used counter.

### Free side

- `0x1453ea000` entry: 6 calls (all non-null)
- `0x1453ea0d3` success: 6; 0 rejected (entry count == success count)
- freed 268,288 B (0.26 MiB), all mapped to live allocations:
  `0x14193e248` 4 × 262,144 B, `0x14006dce7` 2 × 6,144 B
- no large block was freed; the title retains 99.16 % of the pool

There is no evidence of XCL gating a title free: the free path is title
`.text` (`0x1453ea000` is vtable slot 2), XCL supplies no hook there, and
no free was refused.

## Reconciliation

- live table sum `0x8846e6d0` = used counter `0x8846e6d0` exactly
- 542 live entries, 15 free entries (`count 0x22d`), 0 unknown offsets
- committed 2,286,616,272 − freed 268,288 = live 2,286,347,984
- per-site live table equals the table dump; no unaccounted bytes
- the fatal request `0x1560000` (21.375 MiB) exceeds the 18.57 MiB free,
  so the title's pool slot 0 returns 0, the new-handler flag is set, and
  the title takes slot 6 (`0x14221a690` → `int3`)

## Verdict

**Title-side fill; honest negative.**

- All 548 pool allocations are title `.text`; XCL is not between the
  request and the pool.
- XCL's 103.18 MiB of title memory is a separate heap; it does not consume
  this pool.
- The title frees only 0.26 MiB; no free is refused.
- The refusal is the title's own fixed-pool OOM; do not grow the pool, fake
  the allocation, or patch the `int3`.

## Reopen trigger

The two local reopen triggers named when this pool became the frontier are
**spent**; do not re-propose them:

1. **Allocator identity and size provenance** — Step 67
   (`docs/retail-alloc-throw.md`) named the allocator/vtable and the
   static rows; the "Pool size provenance" section above records the
   row copy and the `+disableSharedPools` adjust.
2. **Alloc/free trace of the pool** — the Step-69 gdb run above (548
   allocations, 6 frees, used counter == live-table sum, zero refused
   frees) is that trace.

One new consumer-visible input remains: sample the same `[pool+0x58]` and
the same block table at the first frame **on the console/Windows build**.
If the console also sits at ~99 % used with the same seven consumers, this
is closed. If the console is materially lower, the delta names what XCL
should release; then re-run this breakpoint set once with the largest
blocks' free callers (currently none).

The title-side release trigger that would free the 500 MiB, 229.73 MiB or
19.66 MiB blocks is unknown; the sampled run's six frees are all small
blocks from the two resource sites.

## Folded follow-ups (Step 68 minors)

- `display_target_memory_comes_from_the_title_allocation_hook`,
  `console_resource_allocation_info2_writes_offsets_and_total` and
  `console_placed_resource_layout_slot133_writes_the_resource` now take
  `LayoutSerial` (the `layout_serial` lock plus a drop guard that clears
  `NEWER_LIST_LAYOUT`) because their device slots `#108/#127/#133` latch
  the process-wide layout.
- `docs/retail-newer-list-layout.md`: the emitted layout thunk is **39**
  bytes, not 40.
