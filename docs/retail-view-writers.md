# Retail Step 74 — console UAV/RTV/DSV writers (#19/#20/#21) on the Linux device

Date: 2026-11-28. Step 74. Base: `linux` @ `ee172483` (Step 73 fix).
Task `code-assist:xcl-first-frame:step-74:console-view-writers`.

## Result in one paragraph

The Linux null/Vulkan device now routes console slots `#19`
`CreateUnorderedAccessView`, `#20` `CreateRenderTargetView` and `#21`
`CreateDepthStencilView` to real writers. Retail called them 385 / 70 / 20
times in the Step-73 run while the logging stubs returned immediately, so
every RTV/DSV/UAV heap slot the title filled stayed garbage. Each writer
reads the resource's 56-byte `D3D12_RESOURCE_DESC` and GPU address from
XCL's `NullObj`, then writes the console words into the title's 32-byte
descriptor slot: a V# for BUFFER views and a T# for textures, with
`BASE_ADDRESS`, the GFX10 format, view mip/array range and `Image`'s
absolute-slice depth convention. A NULL resource with a descriptor is
D3D12's null view (retail's UAV wrapper passes it while unbinding) and now
writes valid zero-address words instead of being refused. The Step-73
minors are folded in: 1D-array / MSAA-array depth tests for #70 and a logged
resource-desc refusal.

## Call sites

| slot | retail caller | shape |
| --- | --- | --- |
| `#19` | `0x1416475f0` wrapper → `[device+0x98]` at `exe+0x1647616` | `(res, NULL counter, desc, cpu)`; CPU handle on the stack |
| `#20` | builder `0x140cc2960` → `[device+0xa0]` at `exe+0xcc29ea` | `(res, desc, cpu)` |
| `#21` | builder `0x1414a6940` → `[device+0xa8]` at `exe+0x14a69e8` | `(res, desc, cpu)` |

`#19` is the PC 5-argument shape: `rcx` device, `rdx` resource, `r8` counter,
`r9` view descriptor, fifth argument (the CPU handle) at `[rsp+0x20]` in the
wrapper. The wrapper clears `r8` and moves its own `r8` (the handle) to the
stack, then jumps to `0x144ecc830`. The win64 shim declares the fifth
parameter, so the Rust prologue reads it from the caller's stack slot.

The `#20`/`#21` builders construct a **PC-layout** view descriptor:
`Format` u32 @0, `ViewDimension` u32 @4, then the RTV/UAV union @8 (with
`Shader4ComponentMapping` in that gap for an RTV) or the DSV `Flags` @8 and
union @0xC. That is not the console SRV-at layout of Step 73 (union @0x10,
GPU address @0x28); these slots take D3D12's view structs.

## What the handlers write

`v[19]`, `v[20]` and `v[21]` point at `Device_CreateUnorderedAccessView`,
`Device_CreateRenderTargetView` and `Device_CreateDepthStencilView` on both
`Kind::Device` and `Kind::VulkanDevice`. A CPU handle must be 32-byte aligned
and inside the graphics pool (the heap slots Step 72/73 already use); the
resource must be one of our `Resource`/`PlacedResource` objects; a non-NULL
view descriptor must be readable. Anything else writes nothing and logs one
warning per eight calls.

The resource side comes from `NullObj`: its 56-byte resource desc
(`Dimension`, `Width`, `Height`, `DepthOrArraySize`, `MipLevels`, `Format`,
`SampleDesc.Count`) and its `value` GPU address. The view side is the PC
descriptor above.

- BUFFER (#19/#20): `buffer_words` from the view's `FirstElement`,
  `NumElements`, `StructureByteStride` and RAW flag (UAV) or just the RTV
  element range; `BASE_ADDRESS = va + FirstElement * element_size`.
  Unknown/typeless formats keep the Windows twin's R32_FLOAT (22) fallback.
- Textures: `console_image_words` with `BASE_ADDRESS = va`, GFX10 format from
  the view's `Format` (falling back to the resource's), the resource extents
  and `SampleDesc.Count`. Array views store `FirstArraySlice + ArraySize` in
  `Image::depth` (the repo-wide absolute-last-slice convention), mip views
  store one level (`base == last`), MSAA views store `log2(samples)` as the
  Windows `view_shape_desc` does, and `DST_SEL` is the identity `[4,5,6,7]`
  (these views carry no `Shader4ComponentMapping`).
- 3D views use `WSize` as the T# depth and no base array.
- DSV: the union starts at @0xC (after `Flags`); dimensions 1–6 are
  TEXTURE1D, 1DARRAY, 2D, 2DARRAY, 2DMS, 2DMSARRAY. The view's `Format`
  wins over the resource's; a NULL/UNKNOWN view maps a typeless depth
  resource to its family's concrete depth format (`R32_TYPELESS` → `D32`).
  Depth formats have no colour GFX10 number, so the T# format is 0; the
  shape, mips and slices are still the view's.
- NULL resource + descriptor (#19 in retail): D3D12's null view. The slot
  gets a valid zero-address descriptor (buffer: the view's element range;
  texture: 1×1, no mips), so a shader sees an empty binding instead of the
  previous slot's stale words. A NULL resource with a NULL descriptor, or a
  real resource with a zero GPU address, is refused.

## Tests

RED at `ee172483` in an isolated worktree with the three vtable routes
deleted (implementation otherwise present), so the tests exercise the
behaviour, not just the code's absence:

```
cargo test -p xcl --lib 'hle::d3d12x_linux::tests::console_create_' -- --test-threads=1
test result: FAILED. 10 passed; 4 failed
  console_create_uav_writes_buffer_v_words
  console_create_rtv_writes_texture_t_sharp
  console_create_rtv_with_null_desc_uses_resource_shape
  console_create_dsv_writes_view_shape
```

The UAV failure is the `0x5A5A5A5A` sentinel decoded as a V#; the RTV/DSV
failures are `None` from `desc::image` (no T#). GREEN at this tree:

- `console_create_uav_writes_buffer_v_words` — R32_FLOAT buffer, FirstElement
  16, 64 elements: V# address `va + 64`, stride 4, 64 records, format 22,
  identity DST_SEL, upper slot words zeroed.
- `console_create_uav_null_resource_writes_null_v_words` — NULL resource with
  the same view writes a zero-base V# (address 64 after the element offset)
  instead of leaving the slot untouched.
- `console_create_rtv_writes_texture_t_sharp` — 256×128 RGBA8, 8 slices,
  5 mips; mip 1, slices 2..3 of count 3: T# `va`, format 56, D2Array, depth
  5, base array 2, levels 1/1/4.
- `console_create_rtv_with_null_desc_uses_resource_shape` — NULL desc: the
  resource's 64×32 D2 shape, format 56, mip 0, max mip 2.
- `console_create_dsv_writes_view_shape` — R24G8_TYPELESS resource, D24S8
  view, slices 1..3: D2Array, depth 3, base array 1, format 0 (no colour
  GFX10 depth number).
- `console_view_format_prefers_the_view` — the DSV word builder uses the
  view's R32_FLOAT (GFX10 22) rather than the typeless resource's format.
- `console_view_writers_refuse_bad_slots_resources_and_views` — out-of-pool,
  unaligned, foreign-resource, unreadable-view and null-GPU-address calls
  write nothing.
- `console_view_writers_are_routed` — device `#19/#20/#21` are the three
  handlers.
- `console_create_srv_at_1d_and_msaa_array_depth` (Step-73 minor) —
  TEXTURE1DARRAY and TEXTURE2DMSARRAY with FirstArraySlice 2, count 3 decode
  to base 2, depth 5.
- `console_create_srv_at_refuses_unreadable_resource_desc` (Step-73 minor) —
  NULL / `0x10` resource descs write no T# (and log a refusal).

Focused module `hle::d3d12x_linux::tests`: **62 passed / 0 failed**. Full
gates at this tree: `check -p xcl --tests` 0; `check --workspace` 0; lib
**942 passed / 0 failed / 1 ignored** at `--test-threads=4`; `launcher_e2e`
**7 passed / 0 failed**.

## Retail delta

Two launches were made, both plain fresh-root debug runs of the same title:

1. `/tmp/xcl-step74-r1` (pre-null-view fix): exit **133** SIGTRAP
   `+0x2219651`, **538** presents, 46 stubs, `#19/#20/#21` gone — but the
   log showed five `CreateUnorderedAccessView refused … resource 0x0`
   warnings in the first 0.5 s. That is retail's null UAV descriptor, not a
   bad call; it is the reason the NULL-resource path above exists.
2. `/tmp/xcl-step74-r2` (recorded, post-fix): exit **133** SIGTRAP
   `game_scarlett_master.exe+0x2219651`, **136** presented frames, 46
   called stubs, `#19/#20/#21` absent, and **no** UAV/RTV/DSV/SRV-at refusal
   or null-address warning. The allocator trap is unchanged.

The 385/70/20 view calls now write words instead of returning through a
logging stub. The frontier is still the title-side allocator trap
(`docs/retail-alloc-throw.md`), not descriptor creation.

## Next consumer-visible input

`#61 CreateSampler` (113) is the next descriptor writer, then the per-frame
list slots (`#75/#126/#142` 109 each) and device `#78–#81`. `#23
CopyDescriptors` remains an honest stub until called (0 calls).

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `ConsoleViewKind`,
  `console_view_words`, `buffer_view_words`, `dsv_resource_format`,
  `write_console_view`, the three handlers, `v[19..21]`, the ten tests, and
  unaligned `approx_size` reads (a stack/byte-array resource desc was a
  misaligned-dereference abort in the new tests).
