# Retail M1 — Step 41: `VirtualFree` placeholder split; frontier moves to the descriptor-heap arena

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-41:vfree-placeholder`.
Base: `linux` @ `054405cb` (Step 40 docs). Product code + test @ this commit.

## Result in one paragraph

`VirtualFree(base, size, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)` now splits a
reservation without giving its address space back, and a carved range can be
re-allocated at its original VA (`MEM_REPLACE_PLACEHOLDER`, then
`VirtualAlloc(base, size, MEM_COMMIT)`). Retail's arena helper at
`exe+0x1e86440` (`0x145c5ff00` → it) now completes its five placeholder
splits and five in-place replacements, and the title goes on to allocate and
map its physical pages (`physical pages 0.9 GiB` in the run log). The Step-40
trap at `exe+0x19f4f59` is gone. The new frontier is the shader-visible
descriptor heap: the title asserts that
`GetCPUDescriptorHandleForHeapStart` equals the base of the VA arena it just
reserved, while XCL's null device hands out a fresh graphics-pool range
(`exe+0x1cf336d`, Step 42).

## Fix and test

`hle/kernelx_linux.rs` gains a placeholder-region table keyed by range start
(`PLACEHOLDERS`, `PlaceholderRegion { end, placeholder, backing }`), being the
Linux twin of `hle/kernelx/placeholders.rs`:

- `MEM_RESERVE_PLACEHOLDER` (`0x40000`): the reservation comes from the
  graphics pool (`gfx_alloc(len, GFX_GRAIN, false)`, `PROT_NONE`), not the
  plain `mmap` path. The same VAs are the title's physical-page space and
  `MapTitlePhysicalPages` requires `gfx_contains`, so a plain reservation
  would only move the failure back to `0x1e86741`; the retail run proves the
  pool choice (the page map calls succeed).
- `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER` (`0x8002`): splits the containing
  region into `[start, base)`, `[base, end)`, `[end, region.end)`, every piece
  still reserved. On a replaced (non-placeholder) range it returns it to a
  placeholder and decommits its pages.
- `MEM_REPLACE_PLACEHOLDER` (`0x4000`, with `MEM_RESERVE`): accepts only the
  exact VA range of one placeholder (`base + size == region.end`), commits in
  place when `MEM_COMMIT` is set, and returns the original VA.
- `VirtualAlloc(base, size, MEM_COMMIT)`: commits a range of a replaced
  placeholder in place (`mprotect` RW), matching the Windows twin's test.
- `MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS` (`0x8001`) merges adjacent
  placeholder regions of one backing; `MEM_RELEASE size == 0` drops a region
  and returns the whole graphics allocation to the pool when its last region
  goes.

The ordinary plain-memory `VirtualFree(base, nonzero, MEM_RELEASE)` rejection
is unchanged; placeholder handling runs before both the graphics-pool and
plain release paths.

New test `placeholder_split_and_replacement_keep_the_va_reserved`
(`hle::kernelx_linux::tests`) mirrors retail: reserve three grains through
`XMemVirtualAlloc(null, 3*grain, 0x42000, 7, PAGE_NOACCESS)`, split twice with
`0x8002`, replace one region with `0x20406000`, reject a replacement spanning
two carved ranges (`ERROR_INVALID_ADDRESS`), commit the replacement with
`MEM_COMMIT | PAGE_READWRITE`, write and read back a word, check the other
ranges stay `---p`, return the replacement to a placeholder, coalesce, and
release. RED on the tree with the two dispatch hooks disabled:

```
assertion `left == right` failed: split a placeholder without releasing its address space
  left: 0
 right: 1
test result: FAILED. 0 passed; 1 failed; 878 filtered out
```

GREEN at HEAD: 1 passed.

## Retail evidence

Fresh root `/tmp/xcl-step41-r1`, debug binary at this commit:

```
exit 133, crash SIGTRAP (signal 5) at game_scarlett_master.exe+0x1cf336d
       pc 0x141cf336d, fault 0x0
called_stubs: d3d12_xs ID3D12Device::#133 ×1 (0x1e86853), list #131/#132 ×5,
              device #78 ×1 / #79 ×2, IDXGIOutput::#21 ×1,
              LocaleNameToLCID ×2, ShowWindow ×1, ws2_32#115 ×1,
              IGameInput::#9 ×1
```

The former trap at `+0x19f4f59` (title `int3` after `0x142478440` returned 0)
is gone. The arena helper at `0x141e86440` called `VirtualFree(...)` with
`0x8002` five times and `XMemVirtualAlloc(base, size, 0x20406000, 0x27, …)`
five times, then `XMemAllocatePhysicalPages` /
`XMemMapPhysicalPages` completed (`exe+0x1e8684c` reaches the placed-resource
call #133).

### Next blocker (Step 42): descriptor-heap handles vs the title arena

The trap is `0x141cf3280`, reached from `0x142478eb5` after the arena helper
returns. It creates the shader-visible descriptor heap on the null device
(slot 14 `CreateDescriptorHeap`; IID at `0x1461e0628` =
`8efb471d-616c-4f49-90f7-127bb763fa51`, `ID3D12DescriptorHeap`), then:

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

gdb at `0x141cf336d`:

```
rbx (expected) = 0x1000000000   ; [0x1475df188], the placeholder arena base
r14 (actual)   = 0x100b900000   ; XCL window-0 range from Device_CreateDescriptorHeap
rsi            = 0x1475e7378    ; the title's heap-manager object
```

So the title expects a shader-visible heap's CPU descriptor handles to live in
the VA arena it reserved at `0x145c5ff00` (base `0x1000000000`), not in a
fresh XCL graphics-pool allocation. The same arena places an
`ID3D12Resource` buffer at `base + segment1` through device #133 (IID
`696442be-…`, `CreatePlacedResourceX`); #133 is still an unrouted null stub,
so the Step-42 question is which part of the reservation protocol makes the
console's `CreateDescriptorHeap` return the title's arena base (the placement
call, slot 14 itself, or the heap-start accessor), and to pin it with a RED
test before changing the null device.

## Gates

`cargo check -p xcl --tests` exit 0, `cargo check --workspace` exit 0,
`cargo test -p xcl --lib -- --test-threads=4` **878 passed / 0 failed /
1 ignored**, `cargo test -p xcl --test launcher_e2e` **7 passed / 0 failed**,
fresh-root retail exit 133 at `exe+0x1cf336d` as above (no regression on the
stub surface; #133 is the only new stub).
