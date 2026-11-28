# Retail M1 — Step 42: descriptor heaps from the title's allocation hook

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-42:descriptor-heap-title-memory`.
Base: `linux` @ `455d817d` (Step 41 docs). Product code + test @ this commit.

## Result in one paragraph

The Linux null device now allocates each `ID3D12DescriptorHeap` through the
title's exported `__XMemAllocationHooks__` allocator (the same path the Windows
device already uses), so the retail game's first shader-visible heap starts at
its own placeholder-arena base `0x1000000000` instead of a fresh graphics-pool
range. The Step-41 trap at `exe+0x1cf336d` (title `int3` comparing
`GetCPUDescriptorHandleForHeapStart` with its arena base) is gone. The title now
creates five heaps from its own memory and goes on to device slots #19/#20/#70/
#127 before trapping at `exe+0x247af2e`, a different arena-size assertion.

## The disasm chain (why the title expects its own memory)

`exe+0x1cf3280` creates a view heap through device slot 14 and then:

```
0x1cf334c  mov rcx, [rsp+0x28]          ; the new heap
0x1cf3351  mov [rsi+0x18], rcx
0x1cf3355  mov rax, [rcx]
0x1cf3358  call [rax+0x48]              ; slot 9 GetCPUDescriptorHandleForHeapStart
0x1cf335e  mov [rsi+0x20], rax
0x1cf3362  test rbx, rbx
0x1cf3365  je   0x1cf336d
0x1cf3367  cmp  r14, rbx
0x1cf336a  je   0x1cf336d
0x1cf336c  int3
```

`rbx` = `[0x1475df188]` = the base the arena helper `0x141e86440` stored at
`0x141e864ac` after its `MEM_RESERVE_PLACEHOLDER` splits; Step 41's gdb read it
as `0x1000000000`, while XCL returned `0x100b900000` from `gfx_alloc_window0`.

The console D3D runtime gets descriptor-heap memory from the title, not from the
host: the retail exe exports the data symbol `__XMemAllocationHooks__`
(`llvm-readobj --coff-exports`: RVA `0x6844380`; the `.data` bytes at file
`0x6842f80` are `{0x144deb420, 0x144e38ec0, 0x146188660, 0}` = `{alloc, free,
…}`). `0x144deb420` dispatches on the `XALLOC_ATTRIBUTES` fields to the title's
registered allocator (handler table at `0x146843370`, `bextr eax, esi, 0x810`
selects allocator/object class) and the class-4/allocator-0x80 handler returns
the arena memory. Windows' real device already goes through this table
(`hle/d3d12x/console_heap.rs::title_memory` →
`kernelx::runtime_alloc(bytes, D3D_DESCRIPTOR_HEAP)`); Linux never adopted it.

## Fix

`hle/kernelx_linux.rs` gains the Windows twin (`hle/kernelx.rs:653-696`):

- `TITLE_HOOKS: [AtomicUsize; 2]`, `adopt_title_hooks(&MappedModule)`: reads the
  `__XMemAllocationHooks__` data export and accepts `{alloc, free}` only when
  both pointers lie inside `main.base .. main.base + main.size`; logs
  `XMem: the title has allocation hooks (…)`.
- `D3D_DESCRIPTOR_HEAP = 4 | 1 << 14 | 0x80 << 16 | 16 << 24 | 1 << 29`
  (object type 4, 64 KiB pages/alignment, allocator 0x80, graphics memory) with
  the same comment as Windows.
- `runtime_alloc(size, attrs)` / `runtime_free(p, attrs)` call the adopted
  pointers as `unsafe extern "win64" fn` (the console allocator uses the
  Windows x64 convention; guest code runs natively through the loader).
- `title_descriptor_heap_memory(bytes)`: calls the title's allocator first and
  accepts the answer only when the whole range is inside the graphics pool
  (`gfx_contains_range`) **and** readable; otherwise it calls the title's free
  hook and returns `None`. `gfx_range_readable` answers from `/proc/self/maps`
  (commit `mprotect`s the range `PROT_READ | PROT_WRITE`). It deliberately does
  not reuse `gpu::shader::readable`: that function is a refusal on Linux
  because the crash handler calls it inside a signal context, where reading
  `/proc` would be async-signal-unsafe.
- `hle/d3d12x_linux.rs::Device_CreateDescriptorHeap`: `bytes = n.max(1) * 32`
  is passed to the title's allocator uncapped; only the existing
  `gfx_alloc_window0` fallback keeps its 64 MiB cap. The heap's `value`
  (handle start, slots #9/#10) is the title's pointer when accepted.
- `host_linux::run_boot`: `adopt_title_hooks(&main)` after `load_main` and
  before `run_main`, mirroring `host.rs:177`.

No other behaviour changes; a title without the export keeps the old
`gfx_alloc_window0` path.

## Test (RED first)

New `hle::d3d12x_linux::tests::descriptor_heap_memory_prefers_the_title_allocation_hook`
installs a synthetic title allocator (`set_title_hooks_for_test`, test-only)
that hands out a real committed window-0 pool block for a 7-slot heap:

- with the hook installed, slot 14 → slot 9 start == that block;
- with the hook returning memory outside the pool, the block is freed through
  the hook (`FREED == the rejected pointer`) and the heap falls back to a
  nonzero window-0 range that is not the rejected pointer.

RED on the same tree with `Device_CreateDescriptorHeap` reverted to the direct
`gfx_alloc_window0` path (hooks and helper kept, so the failure is behavioural,
not a compile error):

```
assertion `left == right` failed: the heap starts in the title's memory
  left: 68719542272
 right: 68719476736
test result: FAILED. 0 passed; 1 failed; 879 filtered out
```

(`0x1000010000` = the fresh pool range the fallback picked vs `0x1000000000` =
the synthetic title block the hook returned.)
GREEN at HEAD: `1 passed; 0 failed; 879 filtered out`.

## Retail evidence (fresh root `/tmp/xcl-step42-r2`)

```
exit 133, crash SIGTRAP (signal 5) at game_scarlett_master.exe+0x247af2e
       pc 0x14247af2e, fault 0x0
```

Log lines (first adoption + five heap allocations from the title's memory):

```
XMem: the title has allocation hooks (game_scarlett_master.exe+0x4deb420, game_scarlett_master.exe+0x4e38ec0)
console descriptor heap: 0x2600000 bytes from the title's memory at 0x1000000000
console descriptor heap: 0x1000 bytes from the title's memory at 0x100b900000
console descriptor heap: 0x1000 bytes from the title's memory at 0x100b910000
console descriptor heap: 0x20000 bytes from the title's memory at 0x100b920000
console descriptor heap: 0x10000 bytes from the title's memory at 0x100b940000
```

The first (shader-visible, 38 MiB) heap is exactly the arena base the title
asserts; `+0x1cf336d` no longer fires. Compared with Step 41, device
`#133` (`CreatePlacedResourceX`) is still an unrouted stub, and the run now
reaches new device slots `#19` ×8 (`0x1647616`), `#20` ×2 (`0xcc29ea`,
second arg a descriptor address in the title's memory), `#127` ×2
(`0x193e1ba`) and `#70` ×1 (`0x4ecc821`, second arg `0x1000000220` — inside the
arena). CommandList `#131`/`#132` ×5, `#78`/`#79`, `IDXGIOutput::#21`,
`LocaleNameToLCID`, `ShowWindow`, `ws2_32#115`, `IGameInput::#9` are unchanged.

### New frontier (Step 43): arena-size assertion at `exe+0x247af2e`

`0x14247ad10` (callers `0x14247942b`, `0x14248d94f`) sets up a
`{0, 0x4000000 | 0x5200000}` struct (64 MiB, or 82 MiB when
`0x140075140()` is false), calls `0x145c8f990` to allocate/register a block,
and ends with:

```
          14247af14  mov rax, [rsp+0x78]
          14247af19  sub rax, [rsp+0x70]
          14247af1e  add rax, [rip+0x516c0cb]   # 0x1475e6ff0
          14247af25  cmp rax, 0x1fffff
          14247af2b  jbe 0x14247af2e
          14247af2d  int3
```

i.e. the size of that block, added to a global base `0x1475e6ff0`, must fit in
21 bits (2 MiB). The operands have not been sampled with gdb yet; the next step
is to read `[rsp+0x70]`, `[rsp+0x78]` and `[0x1475e6ff0]` at the trap and name
the protocol (descriptor-table window vs arena block) before changing anything.
Also still open from the same run: the unrouted console device slots
`#19/#20/#70/#127` and `#133`.

## Gates (this commit)

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **879 passed / 0 failed /
  1 ignored** (878 baseline + the new test).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Fresh-root retail: exit 133 at `exe+0x247af2e` (Step-41
  `+0x1cf336d` gone); no regression in the Step-40/41 stubs.
