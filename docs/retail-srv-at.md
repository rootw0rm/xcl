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
A slot outside the graphics pool or a null GPU address is refused without a
write; the GPU address is encoded, never dereferenced. This needs the
DXGI-number → GFX10 table that previously existed only behind the Windows
`DXGI_FORMAT` type (`gpu::desc::gfx10_format_num`, 58 pairs, pinned to
`gfx10_format` on Windows).

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

- requires `cpu` to pass `gfx_contains_range(cpu, 32)` — otherwise it
  writes nothing (host memory is never touched);
- reads the GPU address at `view + 0x28`; `0` is refused with one warning
  per 8 calls;
- BUFFER (dimension 1): V# from `FirstElement`, `NumElements`,
  `StructureByteStride` and the RAW flag, address `va + FirstElement *
  element_size` (checked multiply/add), records in bytes for raw views and
  in elements otherwise — the same shape as the Windows twin's
  `buffer_words_at`;
- TEXTURE1D/…/TEXTURECUBEARRAY (2–10): a T# with BASE_ADDRESS via the new
  `gpu::desc::console_image_words` — GFX10 format, non-zero extent,
  image type, base/last/highest mip (MSAA: `log2(samples)`, as the Windows
  `view_shape_desc`), first array slice/face, and DST_SEL decoded from
  `Shader4ComponentMapping` (channels 0–3 → GFX x..w = 4–7);
- RTAS (11): the BVH words the slot-18 handler writes;
- any other dimension: no write.

`console_image_words` composes `xcl_image_for_view` and then replaces word 0
with `address >> 8`, puts `address >> 40` into word 1, and patches the view
fields; the result decodes back through `desc::image`.

The format table: `gpu::desc::gfx10_format_num(dxgi_number)` is the
platform-free inverse of `dxgi_format` (58 pairs, ascending GFX10 so the
first match is the same one `gfx10_format` returns). On Windows a test
asserts every pair against `gfx10_format(DXGI_FORMAT(n))`, so the twins
cannot drift silently.

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
- `console_create_srv_at_refuses_bad_slots_and_null_addresses` — a slot
  outside the pool is untouched; a null GPU address writes nothing.
- `console_create_srv_at_is_routed` — device vtable `#70` is
  `Device_CreateShaderResourceViewAt`.
- `gpu::desc::tests::gfx10_format_numbers_match_the_wire_values` — wire
  numbers 28/41/10/2/99 map to 56/22/71/77/182, typeless 0 and 44 are
  `None`.
- `gpu::desc::tests::console_image_words_carry_address_and_view_levels` —
  address, base/last/max mip, base array, DST_SEL round-trip.

Focused module `hle::d3d12x_linux::tests`: **49 passed / 0 failed** (45
before). Full gates at this tree: `check -p xcl --tests` 0,
`check --workspace` 0, lib **929 passed / 0 failed / 1 ignored**,
`launcher_e2e` **7 passed / 0 failed**.

## Retail delta

Fresh root `/tmp/xcl-step73-r1`, debug binary at this tree, one plain run,
no retries:

- first frame `presented 3840x2160` at **6.972 s** (Step 72: 6.930 s);
- **106** presented frames;
- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651`, pc
  `0x142219651` — the Step-67 allocator OOM unchanged;
- **49** called stubs (Step 72: 50); `ID3D12Device::#70` is gone, and no
  "CreateShaderResourceViewAt with a null GPU address" or "outside the
  descriptor heaps" warning was logged, so every sampled call had a pooled
  slot and a non-null address;
- remaining device stubs on the path: `#19` 385, `#61` 113, `#80` 80,
  `#20` 70, `#21` 20, `#78` 3, `#79` 4.

## Next consumer-visible input

`#23 CopyDescriptors` (ranges; uncalled, honest stub) and the frame-pacing
device slots `#78–#81` (`#78` 3 and `#79` 4 calls in this run; `#80` is one
per present). `#19`
(385, CreateUnorderedAccessView) and `#20` (70, CreateRenderTargetView) are
the next descriptor writers with named call sites. The gate frontier is
still the title-side allocator trap; this step does not touch it.

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `Device_CreateShaderResourceViewAt`,
  `v[70]`, the four tests and the shared view builder.
- `crates/xcl/src/gpu/desc.rs` — `GFX10_TO_DXGI`, `gfx10_format_num`,
  `console_image_words`, the three tests.
