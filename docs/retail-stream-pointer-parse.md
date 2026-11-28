# Retail M1 — the stream record hash: `%p` width and plain-reservation physical maps

Date: 2026-11-28. Steps 45–46. Base: `linux` @ `fa34a672` (Step 44 docs).
Product code + tests: two commits (CRT scan and kernelx physical maps).

## Result in one paragraph

The Step-44 frontier (`game_scarlett_master.exe+0x4db9277`, a 992-byte hash
over an unmapped `0x4688e13c`) was XCL's `sscanf` writing `%p` at 32-bit width.
The title round-trips a static address as the text `"0x14688e110,14532,0:"`
and re-parses it with `%p`; the truncated `0x4688e110` fell outside every
mapped guest range. With `%p` stored pointer-width, retail reaches the next
contract: a 6 GiB physical-page window is mapped into a plain `VirtualAlloc`
reservation, which Linux's `XMemMapPhysicalPages` refused because it only
accepted graphics-pool VAs. Both are fixed; retail now reaches
`game_scarlett_master.exe+0xabfe1c` (a console `GetResourceAllocationInfo2`
assert) instead of faulting on the stream hash.

## Frontier 1 — the 992-byte record hash (`+0x4db9277`)

The Step-44 crash site: `0x144db9277` hashes 992 bytes at
`rcx = 0x4688e13c`. The caller is the record wrapper `0x14247f590`, called
from `0x14247b6c0` (dispatch on a stream-record type byte), which itself is
reached through the string→object cache `0x141adcc20`.

gdb at `0x14247f6b4` (the hash call), on a fresh root:

```
rcx (hash target) = 0x4688e13c   rdx (size) = 0x3e0
r13 = 0x4688e13c   r14 (stream base) = 0x2c
rsi (record offset) = 0x4688e110
container = 0x1478d57e0  [c+0x18] = 0  [c+0x20] = 0x40c (cursor after advance)
```

`0x1428ca230` is just `[container+0x18] + [container+0x20]` (the paged
stream's window base offset + cursor, *not* a base lookup). The wrapper adds
the record pointer `rsi` to that offset: `r13 = rsi + 0x2c`.

The record pointer comes from the string cache. gdb at `0x14247b5d3` (after
the `%p` scan) caught the exact key:

```
rbx = stack key buffer
key text at rbx+8 = "0x14688e110,14532,0:"
parsed out (r8)  = 0x000000004688e110     <-- high 32 bits lost
count (r14d)     = 0x400
```

The text says `0x14688e110` (5,473,223,952 = inside the mapped exe image
`0x140000000..0x14b06d000`); the scan produced `0x4688e110`. So the correct
hash target would have been `0x14688e13c`, inside `.data`, not an unmapped
low address.

### Root cause and fix (CRT)

`hle/crt_linux.rs::__stdio_common_vsscanf` defaulted every integer
conversion to `ScanLen::Dword` and had no `%p` special case, so `%p` wrote 4
bytes. UCRT treats `%p` as pointer-width regardless of a length modifier.
The integer branch now selects `ScanLen::Qword` for `verb == b'p'`.

Test: the existing `vsscanf_assigns_fields_through_the_va_list` gained the
retail shape — input `0x14688e110`, format `%p`, destination pre-filled with
`0xDEAD_BEEF_0000_0000`. RED (before the fix):

```
left: 16045690982280782096   (0xDEAD_BEEF_4688_E110)
right: 5478342928           (0x1_4688_E110)
```

GREEN after the fix.

### Retail after fix 1

Fresh root `/tmp/xcl-step45-r3`: the `+0x4db9277` SIGSEGV is gone; exit
**133** SIGTRAP at `game_scarlett_master.exe+0x1e6c2a0`; physical pages
7.0 GiB (the stream's page window is now being mapped); new stubs
`ID3D12Device::#61` and `#24`.

## Frontier 2 — physical pages into a plain reservation (`+0x1e6c2a0`)

The int3 at `0x141e6c29f` fires when `XMemMapPhysicalPages` returns null:

```
141e6c273  call XMemVirtualAlloc(0, size, 0x20402000, 0x23, 0x2004)
141e6c278  store result -> [0x1471bfaa8]
141e6c28c  rcx = [0x1471bfaa8]; rdx = n; r8 = page array
141e6c295  call XMemMapPhysicalPages
141e6c29a  test rax, rax
141e6c29f  int3                      ; rax == 0
```

gdb at `0x141e6c29a` on the failing run:

```
rax = null
va  = 0x7fef1ce00000 (64 KiB aligned, from XMemVirtualAlloc)
n   = 0x185c0 pages (6 GiB)
size = 0x185c00000
```

`XMemVirtualAlloc` with `memory_type == 0` (attrs `0x23`: object type 0x23,
memory type 0) delegates to plain `VirtualAlloc` in **both** twins
(`hle/kernelx.rs::XMemVirtualAlloc` and `kernelx_linux.rs`). Windows'
`MapTitlePhysicalPages` maps into that reservation; Linux's `map_pages`
required every page to be inside the graphics pool (`gfx_contains`) and
returned `ERROR_INVALID_ADDRESS`.

### Fix (kernelx)

`hle/kernelx_linux.rs`:

- `plain_reservation_contains(addr, len)`: a range inside one live
  `VirtualAlloc` allocation (`ALLOCATIONS`).
- `commit_physical_page(at)`: graphics pages keep `gfx_commit_at`; a plain
  reservation is committed with `mprotect(PROT_READ | PROT_WRITE)`.
- `map_pages` accepts an explicit address when every 64 KiB page is in the
  graphics pool **or** a plain reservation, and commits through
  `commit_physical_page`.
- Plain `VirtualFree(MEM_RELEASE)` / `MEM_DECOMMIT` now call
  `unmap_away_range` first, so mapped physical pages go home instead of
  leaving stale away entries.

Test `physical_pages_map_into_a_plain_reservation`: allocate one physical
page, write `0x42` to its home, reserve `0x30000` via `VirtualAlloc`, map the
page at a 64 KiB-aligned address inside it, read `0x42`, unmap with a null
page array, release. RED on the parent (the explicit map returns null):

```
left: 0
right: 140137924132864  (the reservation address)
```

GREEN at the fix.

### Retail after fix 2

Fresh root `/tmp/xcl-step45-r5`, debug binary at both fixes:

```
exit 133, SIGTRAP (signal 5) at game_scarlett_master.exe+0xabfe1c
       (int3 at 0xabfe1b), fault 0x0
memory: process commit 17.9 GiB (working set 6.8 GiB); physical pages 7.0 GiB
called_stubs: device #57 x585, #67 x42, #70 x18, #19 x8, #86 x6,
              list #131/#132 x5, #20 x2, #24 x2, #61 x2, #79 x2, ...
              16 other one-shot stubs (#78, #108, #133, IDXGIOutput::#21, ...)
vfs.unmapped_paths: []      xgameruntime.unimplemented: {}
```

The old `+0x4db9277` and `+0x1e6c2a0` crashes are gone; the stream window is
mapped and the title now walks 585 pipeline-state/descriptor calls.

### New frontier (Step 47)

`0x140abfe1b` asserts that the 16-byte result of the device call at
`[device vtable + 0x360]` (= slot 108) has `[out+4] == 0`:

```
140abfdf3  lea rdi, [rsp + 0x40]
140abfe07  call qword ptr [rax + 0x360]   ; device #108
140abfe0d  vmovups xmm0, [rdi]
140abfe11  vmovups [rsi], xmm0            ; 16-byte allocation info
140abfe15  cmp dword ptr [rsi + 4], 0
140abfe1b  int3
```

Windows maps device #108 to `get_resource_allocation_info2_x`
(`real.rs:1004`), but Linux leaves it a stub, so the out struct is
uninitialised. The other new stubs have Windows console handlers too:
#57 / #67 = `deserialize_pso_x`, #86 = `create_pipeline_state_x`, #61 =
`create_sampler_x`, #24 = `dev_copy_descriptors_simple_x`
(`real.rs:977,985-989,1003`). Step 47 should route #108 (the assert) first
and then decide the pipeline-state/descriptor cluster from a second run.

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **882 passed / 0 failed /
  1 ignored** (881 baseline + the physical-map test; the `%p` case extended
  an existing test).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Retail fresh roots: `/tmp/xcl-step45-r3` (fix 1), `/tmp/xcl-step45-r5`
  (both fixes), 133 at `+0xabfe1c`; old frontier gone.
