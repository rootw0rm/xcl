# Retail Step 72 — console `CopyDescriptorsSimple` (#24) on the Linux device

Date: 2026-11-28. Step 72. Base: `linux` @ `1d7b1189` (Step 71 docs).
Task `code-assist:xcl-first-frame:step-72:copy-descriptors-simple`.

## Result in one paragraph

The Linux null/Vulkan device now routes console slot `#24`
`CopyDescriptorsSimple(n, dst, src, type)`. Retail calls it 113 times in a
fresh-root run from `exe+0x4ecc884`, immediately after staging a raw-memory
SRV with console slot `#70`; the logging stub answered S_OK and copied
nothing, so every staged descriptor was lost. On Linux a descriptor handle
is a host address inside a heap from `Device_CreateDescriptorHeap` and every
slot is 32 bytes (`GetDescriptorHandleIncrementSize` returns 32 for every
type here), so the handler copies `n * 32` bytes between graphics-pool
ranges and answers `E_INVALIDARG` — without writing — when either range is
outside the pool or the length overflows. `#23 CopyDescriptors` (ranges) is
not called by the title and stays an honest logging stub; `#70`
`CreateShaderResourceViewAt` still needs a DXGI→GFX10 number table and is
not part of this step.

## The call site

`0x144ecc800` is the title's raw-memory SRV staging helper:

```
144ecc800  push rsi; sub rsp,0x20
144ecc805  mov rsi,r8                 ; cpu handle
144ecc808  mov r8,rdx                 ; view desc
144ecc80b  mov rdx,rcx                ; resource desc
144ecc80e  mov rcx,[device]           ; 0x148759d00
144ecc815  mov r9,rsi
144ecc818  mov rax,[rcx]
144ecc81b  call [rax+0x230]           ; slot 70 CreateShaderResourceViewAt
144ecc821  mov rcx,rsi
144ecc824  xor edx,edx
144ecc82b  jmp 0x144ecc830            ; cache-line writeback of [handle, handle+len)
```

The `#24` call is the cache-flushed branch of that same helper at
`0x144ecc830`:

```
144ecc830  sub rsp,0x28
144ecc834  mov r9,rcx                 ; src = the staged handle
144ecc837  test edx,edx
144ecc839  jne 0x144ecc85d
...
144ecc85d  mov rcx,[device]
144ecc864  mov r8,[0x1475e7010]       ; dest heap base, -0x10000 relative
144ecc86b  mov edx,1                  ; n = 1
144ecc870  mov rax,[rcx]
144ecc873  add r8,r9                  ; dst = base + src
144ecc876  mov dword [rsp+0x20],1     ; type = 1
144ecc87e  call [rax+0xc0]            ; slot 24 CopyDescriptorsSimple
```

The Step-70 report recorded the same site as 113 calls with `first_args`
`0x100b900000 0x100b910000` — two handles inside the title's 38 MiB
shader-visible heap (the console layout's slot numbering matches the twin's
`real.rs::console_slots` entry `(24, dev_copy_descriptors_simple_x)`).

## The Linux descriptor model

`Device_CreateDescriptorHeap` hands out `count * 32` bytes from
`title_descriptor_heap_memory` (the title's `__XMemAllocationHooks__`
allocator, range-checked into the graphics pool), and
`Device_GetDescriptorHandleIncrementSize` returns 32 for every heap type.
A handle the title passes back is therefore the host address of its slot;
there is no console→PC descriptor table to publish (the words in the heap
*are* the console words). The copy is the slot bytes:

```rust
let bytes = n as usize * 32;                     // checked
if !gfx_contains_range(dst, bytes) || !gfx_contains_range(src, bytes) {
    return E_INVALIDARG;                          // never write outside the pool
}
copy_nonoverlapping(src, dst, bytes);
```

`gfx_contains_range` is the graphics pool's own range check
(`kernelx_linux.rs`). The retail handles `0x100b900000`/`0x100b910000` pass
it; the 20 GiB pool is probed at `0x10_0000_0000` first, so the title's
arena base `0x1000000000` is inside it (both handles are 64-bit window-0
addresses, as `gfx_alloc_window0` requires).

## Tests

RED at the parent tree (only the behavioural test added, it names no new
symbol):

```
cargo test -p xcl --lib d3d12x_linux::tests::console_copy_descriptors_simple_copies_heap_slots
xcl: [STUB] d3d12_xs.dll!ID3D12Device::#24 first called from ... → returning 0x0
assertion `left == right` failed: two 32-byte slots copied
  left: [1515870810; 8]     (0x5A5A5A5A sentinel, nothing copied)
 right: [286331153, 572662306, ...]
test result: FAILED. 0 passed; 1 failed; 922 filtered out
```

GREEN at this tree:

- `console_copy_descriptors_simple_copies_heap_slots` — two graphics-pool
  ranges from `gfx_alloc_window0`; `n = 2` copies 64 bytes and leaves the
  slot past the range at its sentinel; a destination outside the pool is
  refused with `E_INVALIDARG` and left untouched.
- `console_copy_descriptors_simple_is_routed` — device vtable `#24` is
  `Device_CopyDescriptorsSimple`.

Focused module `d3d12x_linux::tests`: **45 passed / 0 failed** (43 before).
Full gates: `check -p xcl --tests` 0, `check --workspace` 0, lib
**923 passed / 0 failed / 1 ignored**, `launcher_e2e` **7 passed / 0
failed**.

## Retail delta

Fresh root `/tmp/xcl-step72-r1`, debug binary at this tree, one plain run,
no retries:

- first frame `presented 3840x2160` at **6.930 s** (Step 70: 6.954 s);
- **113** presented frames;
- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651`, pc
  `0x142219651` — the Step-67 allocator OOM unchanged;
- **50** called stubs; `ID3D12Device::#24` is gone, no
  "CopyDescriptorsSimple outside the descriptor heaps" warning (both retail
  handles are in the pool), and the copy introduces no new fault;
- remaining device stubs on the path: `#70` 688 calls
  (CreateShaderResourceViewAt), `#19` 385, `#61` 113, `#80` 87, `#20` 70,
  `#21` 20.

## Next consumer-visible input

`#70` is the missing half of this staging path: the title asks for an SRV
of raw memory 688 times per run and no Linux handler writes the descriptor
(the twin writes V# words for the BUFFER dimension only). Porting it needs
the DXGI-format number → GFX10 IMG_FORMAT mapping
the Windows twin gets from `gpu::desc::gfx10_format` (Windows-only), and a
`view+0x28` GPU-address read that must be checked against title memory
first. After that, `#23` (ranges) and the frame-pacing device slots
`#78–#81` (87/4/3 calls) are the next named console methods. The gate
frontier is still the title-side allocator trap; this step does not touch it.

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `Device_CopyDescriptorsSimple`,
  `v[24]`, the two tests.
