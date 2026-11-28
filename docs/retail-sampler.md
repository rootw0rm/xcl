# Retail Step 75 — console sampler writer (#61) on the Linux device

Date: 2026-11-28. Step 75. Base: `linux` @ `0031a39d` (Step 74).
Task `code-assist:xcl-first-frame:step-75:console-sampler`.

## Result in one paragraph

The Linux null/Vulkan device now routes console device slot `#61`
`CreateSamplerX(desc, cpu)`. Retail calls it 113 times per fresh-root run
through the wrapper `0x1412b9800` (`[device+0x1e8]` at `exe+0x12b981e`),
which computes the destination as the title's sampler-heap base plus
`index * increment`; the logging stub returned immediately and left the
32-byte heap slot garbage. The handler writes the XCL-ABI S# the
translated shaders read — `desc::xcl_sampler_words` with
`index = (cpu - heap_base) / 32`, exactly the `[0, 0, 0, index]` form the
Windows twin's `dev_create_sampler_x` publishes. The slot must belong to a
descriptor heap registered by `Device_CreateDescriptorHeap`, be a whole
32-byte slot from that heap's base, and the 0x6C-byte console descriptor
must be readable; anything else writes nothing and logs one warning per
eight calls. The extended console sampler fields (address modes, LODs,
bias, anisotropy, comparison) are not modelled: the slot is shader input,
and the XCL ABI keeps the sampler state outside it.

## Call sites

`0x1412b9800` is the title's sampler wrapper:

```
1412b9800  push rsi; sub rsp,0x20
1412b9805  mov rsi,rdx                 ; cpu handle
1412b9808  mov rdx,rcx                 ; console sampler desc
1412b980b  mov rcx,[device]            ; 0x148759d00
1412b9812  mov r8,rsi                  ; cpu handle
1412b9815  mov rax,[rcx]
1412b9818  call [rax+0x1e8]            ; slot 61 CreateSamplerX
1412b981e  mov rcx,rsi                 ; (the plan's "at exe+0x12b981e")
1412b9821  mov edx,1
1412b9826  ...
1412b982b  jmp 0x144ecc830            ; stage the slot with #24, type 1
```

`0x144ecc830` is the Step-72 cache-writeback / copy helper: with
`edx = 1` it copies `n = 1` descriptor with `CopyDescriptorsSimple` type
`1` (SAMPLER) from the staged handle to `[0x1475e7010] + handle`, the
shader-visible heap. The wrapper is called from two places:

- `0x1412b96b0` (sampler 0..127 setup) builds the 0x6C-byte console
  descriptor in the caller's frame and calls the wrapper. The destination
  comes from the two globals it loads at `exe+0x12b96bf`:
  `r10 = [0x1475e7428]` (heap base), `rdx = [0x1475e7438]` (increment);
  `imul rdx, index`, `add rdx, r10`. `0x142475fbb` calls this builder in
  a loop over 128 sampler records (each 0x24 bytes).
- The other sampler callers (`0x1412b97d7` in the same builder) pass the
  same descriptor and wrapper.

The heap setup at `0x142478e80` creates four heaps through
`0x145a54700` → `0x141cf3280`: a 128-slot SAMPLER heap at
`0x1475e73c0` (shader visible), a 128-slot SAMPLER staging heap at
`0x1475e7408`, a 4096-slot RTV heap at `0x1475e7450`, and a 2048-slot DSV
heap at `0x1475e7498`. `0x141cf3280` asks the device for
`GetDescriptorHandleIncrementSize(type)` (`[device+0x78]`), creates the
heap (`[device+0x70]`), stores the CPU handle start at heap object
`+0x20` (`= 0x1475e7408 + 0x20 = 0x1475e7428`) and that increment at
`+0x30` (`= 0x1475e7438`). Both SAMPLER heaps are therefore
`base + index * 32` on this backend, 32-byte slots from a registered
range.

## The console sampler descriptor

The title's builder fills a 0x6C-byte console structure in its frame: PC
fields appear at the offsets the Windows twin's `create_sampler_x`
documents — address modes at `+0x18` (four bytes), min/max LOD at
`+0x24`/`+0x28`, bias at `+0x30`, max anisotropy at `+0x34`,
comparison at `+0x40`. The handler only requires the whole 0x6C bytes to
be readable (the twin's guard); it does not decode the state.

That is not an omission: the console slot is the S# memory a shader
loads, and XCL's shader ABI takes the sampler heap index from word 3
[11:0] (`gpu::recomp::emit`'s MIMG path,
`gpu::dxil::lower_image::mimg_sampler`). The Windows twin does the same:
its console `#61` builds a `D3D12_SAMPLER_DESC` from the extended fields,
creates the PC sampler, and publishes `[0, 0, 0, index]` into the console
slot. On Linux there is no PC sampler heap in this slice, so the slot
carries the ABI index and zeroes the other 16 bytes of the 32-byte slot.
Static check: the wrapper's only consumers are `#61` and the `#24`
stage to the shader-visible sampler heap; no title-side read of the S#
words was found, so the default ABI form is the honest one.

## The descriptor-heap range registry

`Device_CreateDescriptorHeap` now records `(base, allocated_bytes, type)`
in a process-global `DESCRIPTOR_HEAPS` list. `allocated_bytes` is the
range actually handed out: the title's hook allocation, the pool
allocation capped at 64 MiB, or the exhaustion fallback `Vec<u8>`.
Recording the capped size (not the requested `NumDescriptors * 32`) is
what keeps `#61` from writing past a short fallback allocation.

`descriptor_heap_at(cpu)` finds the heap whose `[base, base + bytes)`
contains the 32-byte slot. `#61` uses it; `#24`/`#70` keep the coarser
`gfx_contains_range` check, because every retail handle they have seen so
far is already inside a registered heap but tightening them is a separate
change.

## What the handler writes

`v[61] = Device_CreateSamplerX` on both `Kind::Device` and
`Kind::VulkanDevice`. The handler:

1. looks the slot up in `DESCRIPTOR_HEAPS`; `cpu` must not lie outside
   every heap;
2. requires `(cpu - base) % 32 == 0` (a whole slot from the heap base, not
   an absolute host-32 alignment: the exhaustion fallback is malloc-backed);
3. requires the descriptor pointer to pass `gfx_range_readable(desc, 0x6C)`;
4. writes `desc::xcl_sampler_words(&[0; 4], index)` into a zeroed 32-byte
   slot, so word 3 [11:0] is the slot index and no stale upper bytes stay.

Any failure writes nothing and logs one `CreateSamplerX refused` warning
per eight calls, with the alignment, heap and descriptor verdicts.

## Tests

RED at the parent route (the vtable line commented out, same tree): the
logging stub answered S_OK and left the sentinel:

```
cargo test -p xcl --lib console_create_sampler_writes_s_sharp_at_heap_slots
assertion `left == right` failed: S# index 0, rest zeroed
  left: [1515870810, ...]   ; 0x5A5A5A5A sentinel
 right: [0, 0, 0, 0, 0, 0, 0, 0]
test result: FAILED. 0 passed; 1 failed
```

GREEN at this tree (2 tests):

- `console_create_sampler_writes_s_sharp_at_heap_slots` — creates a
  16-slot SAMPLER heap through `Device_CreateDescriptorHeap`, calls `#61`
  with a `0x7F`-filled descriptor at slot 0 and slot 5 of that heap: the
  slots decode to `[0,0,0,0]` and `[0,0,0,5]` with the upper 16 bytes
  zeroed (proving the slot is not a byte copy of the descriptor). An
  unaligned handle (`base + 8`), a NULL descriptor and a raw pool range
  that no heap registered all leave their `0x5A` sentinels untouched.
- `console_create_sampler_is_routed` — `slot(dev, 61)` is
  `Device_CreateSamplerX`.

Focused module `hle::d3d12x_linux::tests`: **64 passed / 0 failed**.
Full gates at this tree: `check -p xcl --tests` 0, `check --workspace` 0,
lib **944 passed / 0 failed / 1 ignored** at `--test-threads=4`,
`launcher_e2e` **7 passed / 0 failed**.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step75-r1`, same title and config as
the Step-74 run:

- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651` — the
  Step-67 title-side allocator trap, unchanged;
- **130** presented frames, first at **6.938 s**, last at 11.769 s;
- **45** called stubs (Step 74: 46); `ID3D12Device::#61` is gone;
- no `CreateSamplerX refused` and no null/out-of-pool warning, so all 113
  staged sampler slots were registered, aligned, and readable;
- the remaining D3D12 stubs are the next inputs: list `#75/#126/#142`
  105 each, device `#80` 105, allocator `#9` 106, list `#35` 36,
  `#33/#14` 18 each, `#131/#132` 6/5, queue `#31` 11, device `#78/#79`
  3/4.

The frontier is still the title-side allocator trap
(`docs/retail-alloc-throw.md`), not descriptor creation.

## Next consumer-visible input

Per-frame command-list slots `#75/#126/#142` (105 each), `#33/#35`, `#14`,
then the frame clock/queue/allocator slots (`#78–#81`, queue `#31`,
allocator `#9`). `#23 CopyDescriptors` stays deferred on its call trigger;
the allocator-OOM frontier stays parked on its console-sample trigger.

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `DESCRIPTOR_HEAPS`,
  `descriptor_heap_at`, the registration in `Device_CreateDescriptorHeap`,
  `Device_CreateSamplerX`, `v[61]`, and the two tests.
