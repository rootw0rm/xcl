# Retail Step 73 — console `CreateShaderResourceViewAt` (#70) on the Linux device

Date: 2026-11-28. Step 73. Base: `linux` @ `24d7c981` (Step 72 docs).
Task `code-assist:xcl-first-frame:step-73:create-srv-at`.

## Result in one paragraph

The Linux null/Vulkan device now routes console slot `#70`
`CreateShaderResourceViewAt(resource_desc, view, cpu)`, the writer half of
the raw-memory SRV staging path Step 72 made copyable. Retail calls it from
`exe+0x4ecc81b` inside the staging helper `0x144ecc800`; the logging stub
returned immediately and left the title's 32-byte descriptor slot untouched,
so `#24` copied staging garbage. The handler writes the console descriptor
into that slot: a V# for the BUFFER dimension (the raw-memory case the
sampled call sites pass), a T# carrying the GPU address and the view's
format/mips for texture dimensions, the BVH words for a ray-tracing view.
A slot outside the graphics pool, a slot that is not 32-byte aligned, an
unreadable view or a null GPU address is refused without a write; the GPU
address is encoded, never dereferenced. This needs the
DXGI-number → GFX10 table that previously existed only behind the Windows
`DXGI_FORMAT` type (`gpu::desc::gfx10_format_num`, 61 pairs, pinned to
`gfx10_format` on Windows by an exhaustive walk of the Windows decoder).

## The call site

`0x144ecc800` (Step 72) is the title's staging helper:

```
144ecc800  push rsi; sub rsp,0x20
144ecc805  mov rsi,r8                 ; cpu handle
144ecc808  mov r8,rdx                 ; view desc
144ecc80b  mov rdx,rcx                ; resource desc
144ecc80e  mov rcx,[device]           ; 0x148759d00
144ecc815  mov r9,rsi
144ecc818  mov rax,[rcx]
144ecc81b  call [rax+0x230]           ; slot 70 CreateShaderResourceViewAt
```

The title's caller at `0x144ec5b76` builds the arguments:

```
144ec5b2b  ...                                   ; resource size → rbx desc +0x10
144ec5b34  mov dword [rbx],1                     ; D3D12_RESOURCE_DESC.Dimension = BUFFER
144ec5b4c  mov qword [rbx+0x18],0x1000100000001
144ec5b50  vmovups [rbx+0x20],xmm0
144ec5b71  call 0x144ecc890                      ; build the console view desc
144ec5b76  mov rax,[rsi+0x38]                    ; GPU address
144ec5b7a  mov rcx,rbx                           ; resource desc
144ec5b7d  mov rdx,r14                           ; view desc
144ec5b80  mov r8,rdi                            ; destination handle
144ec5b83  mov [r14+0x28],rax                    ; view + 0x28 = GPU address
144ec5b87  call 0x144ecc800
```

The view builder `0x144ecc890` writes Format and ViewDimension from the
source dwords, copies a dword at `+8` (the source built `0x1688`, D3D12's
identity `Shader4ComponentMapping`), then dispatches on `dimension - 1` to
handlers that copy the union starting at `+0x10`. So the console view is:

| offset | field |
| --- | --- |
| `0x00` | `Format` (DXGI number) |
| `0x04` | `ViewDimension` |
| `0x08` | `Shader4ComponentMapping` (`0x1688` = identity) |
| `0x10` | dimension union (`FirstElement`/`NumElements`/`Stride`/`Flags` for BUFFER, mip/slice fields for textures, `Location` for RTAS) |
| `0x28` | GPU address (u64) |

This is the layout `Vulkan_CreateShaderResourceView` already reads for
RTAS (`Location` @ `0x10`), not the PC
`D3D12_SHADER_RESOURCE_VIEW_DESC` (union @ `0x08`) that the Windows twin
`real.rs::dev_create_srv_at_x` assumes. The Linux handler follows the
console bytes the title actually stages.

## What the Linux handler writes

`v[70] = Device_CreateShaderResourceViewAt` on both `Kind::Device` and
`Kind::VulkanDevice`. A Linux descriptor handle is a host address in a
32-byte slot of a heap from `Device_CreateDescriptorHeap`; the heap words
*are* the console descriptor words (Step 72). The handler:

- requires `cpu` to be 32-byte aligned and pass
  `gfx_contains_range(cpu, 32)`, and `view` to pass `gfx_range_readable` —
  otherwise it writes nothing (host memory is never touched) and logs one
  refusal per 8 calls;
- reads the GPU address at `view + 0x28`; `0` is refused with one warning
  per 8 calls;
- BUFFER (dimension 1): V# from `FirstElement`, `NumElements`,
  `StructureByteStride` and the RAW flag, address `va + FirstElement *
  element_size` (checked multiply/add), records in bytes for raw views and
  in elements otherwise — the same shape as the Windows twin's
  `buffer_words_at`, including its R32_FLOAT (22) fallback for an
  unknown/typeless format;
- TEXTURE1D/…/TEXTURECUBEARRAY (2–10): a T# with BASE_ADDRESS via the new
  `gpu::desc::console_image_words` — GFX10 format, non-zero extent,
  image type, base/last/highest mip (MSAA: `log2(samples)`, as the Windows
  `view_shape_desc`), first array slice/face and DST_SEL decoded from
  `Shader4ComponentMapping`. The T# depth is the absolute last slice + 1,
  so an array view stores `FirstArraySlice + ArraySize` (cube array:
  `First2DArrayFace + NumCubes * 6`), not the slice count. Mapping fields
  0–3 (FROM_MEMORY) become GFX x..w = 4–7, and FORCE_VALUE_0/1 (4/5)
  become GFX 0/1;
- RTAS (11): the BVH words the slot-18 handler writes;
- any other dimension: no write.

`console_image_words` composes `xcl_image_for_view` and then replaces word 0
with `address >> 8`, puts `address >> 40` into word 1, and patches the view
fields; the result decodes back through `desc::image`.

The format table: `gpu::desc::gfx10_format_num(dxgi_number)` is the
platform-free inverse of `dxgi_format` (61 pairs, ascending GFX10 so the
first match is the same one `gfx10_format` returns). On Windows a test
asserts every pair against `gfx10_format(DXGI_FORMAT(n))` **and** walks
every GFX10 value `dxgi_format` maps, so a missing row (the old table had
58 and dropped B5G6R5/B5G5R5A1/B4G4R4A4) can no longer pass silently.

## Tests

RED at the parent tree, behavioural tests only (the refusal test passes
trivially against a stub that writes nothing):

```
cargo test -p xcl --lib console_create_srv_at
test console_create_srv_at_writes_buffer_v_words ... FAILED
  left: (99344109427290, 6746, 1515870810, 37)   ; 0x5A5A5A5A sentinel
test console_create_srv_at_writes_texture_t_sharp ... FAILED
  a console T#                                    ; slot is None
test result: FAILED. 1 passed; 2 failed
```

GREEN at this tree:

- `console_create_srv_at_writes_buffer_v_words` — BUFFER view, FirstElement
  16, 64 elements, RAW, address `va + 64`, records 256, format 22
  (R32_FLOAT), identity DST_SEL, the upper 16 bytes zeroed.
- `console_create_srv_at_writes_texture_t_sharp` — 256×128 RGBA8, 8 mips,
  view mips 1..3: T# decodes to address `va`, format 56, base/last/max mip
  1/3/7, type D2.
- `console_create_srv_at_array_depth_ends_at_first_plus_count` —
  TEXTURE2DARRAY FirstArraySlice 2, ArraySize 3 on a 12-slice resource
  decodes to base 2, depth 5; cube array First2DArrayFace 6, NumCubes 2 to
  base 6, depth 18.
- `console_create_srv_at_decodes_force_value_mapping` — mapping `0x68C`
  (FORCE_0, FROM_MEMORY_1/2/3) decodes to DST_SEL `[0, 5, 6, 7]`.
- `console_create_srv_at_buffer_unknown_format_is_r32_float` — a
  UNKNOWN/typeless buffer format gives GFX10 22 and stride 4, not 0.
- `console_create_srv_at_refuses_bad_slots_and_null_addresses` — a slot
  outside the pool is untouched, an in-pool but unaligned address is
  untouched; a null GPU address writes nothing.
- `console_create_srv_at_is_routed` — device vtable `#70` is
  `Device_CreateShaderResourceViewAt`.
- `gpu::desc::tests::gfx10_format_numbers_match_the_wire_values` — wire
  numbers 28/41/10/2/99 map to 56/22/71/77/182, the packed 85/86/115 map
  to 133/134/136, UNKNOWN 0 and R24G8_TYPELESS 44 are `None`.
- `gpu::desc::tests::console_image_words_carry_address_and_view_levels` —
  address, base/last/max mip, base array, DST_SEL round-trip.

Focused module `hle::d3d12x_linux::tests`: **52 passed / 0 failed** (49
before this review fix). Full gates at this tree: `check -p xcl --tests` 0,
`check --workspace` 0, lib **932 passed / 0 failed / 1 ignored**,
`launcher_e2e` **7 passed / 0 failed**.

## Review fix (after Step 73 rejection)

The first Step-73 tree was rejected with three reproduced defects and four
minors; all are fixed here.

1. `GFX10_TO_DXGI` had 58 of the 61 rows `dxgi_format` defines, so
   133/134/136 (DXGI 85/86/115: B5G6R5, B5G5R5A1, B4G4R4A4) produced
   GFX10 0. The Windows pin could not see that: it iterated the table. The
   test now walks every GFX10 value `dxgi_format` maps.
2. Array/cube-array T# depth was the view's count; `Image::depth` is the
   absolute last slice + 1 repo-wide, so FirstArraySlice 2 + ArraySize 3
   decoded as one slice. The handler now adds first + count (cube array:
   first + 6 × NumCubes).
3. `Shader4ComponentMapping` 4/5 (FORCE_VALUE_0/1) decoded to GFX 4/5
   (x/y) instead of 0/1; `0x68C` now decodes to `[0, 5, 6, 7]`.

Minors: the GFX10-44 comment is 24_8 (depth), not 2_10_10_10, and the
DXGI-44 comment names R24G8_TYPELESS; an unknown/typeless buffer format
now uses the Windows twin's R32_FLOAT fallback; `view`/`rdesc` are checked
with `gfx_range_readable` before any read; `cpu` must be 32-byte aligned
as well as inside the pool (descriptor slots are 32-byte increments).

## Retail delta

Fresh root `/tmp/xcl-step73b-r1`, debug binary at this tree, one plain run,
no retries:

- `presented 3840x2160` (first present then repeated; the run's log does
  not carry wall timestamps), **142** presented frames;
- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651`, pc
  `0x142219651` — the Step-67 allocator OOM unchanged;
- **49** called stubs; `ID3D12Device::#70` is gone, and no
  "CreateShaderResourceViewAt refused", "with a null GPU address" or
  "outside the descriptor heaps" warning was logged, so every sampled call
  passed the alignment/pool/readable checks and had a non-null address;
- remaining device stubs on the path: `#19` 385, `#61` 113, `#80` 115,
  `#20` 70, `#21` 20; command-list `#75`/`#126`/`#142` at 115 each.

## Next consumer-visible input

`#23 CopyDescriptors` (ranges; uncalled, honest stub) and the frame-pacing
device slots `#78–#81`. `#19`
(385, CreateUnorderedAccessView) and `#20` (70, CreateRenderTargetView) are
the next descriptor writers with named call sites. The gate frontier is
still the title-side allocator trap; this step does not touch it.

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `Device_CreateShaderResourceViewAt`,
  `v[70]`, the seven tests and the shared view builder.
- `crates/xcl/src/gpu/desc.rs` — `GFX10_TO_DXGI`, `gfx10_format_num`,
  `console_image_words`, the three tests.
