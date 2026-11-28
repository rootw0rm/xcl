# Retail command-list per-frame slots (#14, #33/#35, #75/#126/#142)

Step 76. Base `762e5da4`. One plain fresh-root retail run, no gdb.

## Retail input

`/tmp/xcl-step75-r1` (45 called stubs) listed the per-frame command-list
slots:

| slot | calls | first caller | report arguments (r8/r9) |
|---|---|---|---|
| `#75` | 105 | `exe+0x32820` | `0x10 0x559f128373f0` |
| `#126` | 105 | `exe+0x3263e` | `&stack 0x559f128373f0` |
| `#142` | 105 | `exe+0x329e9` | `0x10 0x559f128373f0` |
| `#33` | 18 | `exe+0x248c379` | `0x10 0x559f128373f0` |
| `#14` | 18 | `exe+0x1236d2` | `0x5 0x21` |
| `#35` | 36 | `exe+0x248c46b` | `0x1000a1e180 0x40` (first) |

The calls come from the title's own state encoder. `exe+0x32520` is one
large "apply dirty graphics state" function that writes inline command
records into the list object (`+0x18` write pointer, `+0x28`/`+0x30`
ends) and falls back to a vtable call when a record does not fit.
`exe+0x248c2a0` is a per-draw-item encoder doing the same. `exe+0x1236d2`
is the return address of the wrapper's caller, not a direct vtable call
(the wrapper tail-jumps, see below).

## Named slots

### `#14 Dispatch(x, y, z)`

`0x140056a20` is the dispatch wrapper:

- `r14 = [rcx+0x127c8]` (the command list), `ebx/edi/esi = edx/r8d/r9d`;
- it checks the inline dispatch window `[r14+0x18] < [r14+0x28]` and writes
  a 16-byte dispatch record when there is room;
- otherwise `0x140056b2b` loads `[rax+0x70]` (vtable byte offset, slot 14)
  and `0x140056b3f` tail-jumps with `rcx = list`, `edx = ebx`,
  `r8d = edi`, `r9d = esi`.

The tail jump preserves the wrapper's return address, which is why the
report names the caller `exe+0x1236d2` (the `call 0x140056a20` at
`0x1401236cd`). The reported `r8 = 5`, `r9 = 0x21` are the group counts.

`#14` is PC `Dispatch` at the same index in both layouts (no entry in
`NEWER_LIST_SLOTS`). The Linux null list left it as the generic logging
stub, so the 5 dispatches returned 0 without touching any list state.

### `#33 SetComputeRootSignature`

The console remap block (`real.rs::console_remap`, mirrored by
`console_list_base_slot`) maps console 32..55 to PC 28..51, so console
`#33` is PC `#29`. In the Windows-rs `ID3D12GraphicsCommandList_Vtbl`
order (the same as `d3d12.h`) flat slot 29 is `SetComputeRootSignature`.

Static confirmation in `0x14248c2a0`:

- `0x14248c35d` loads `rdx = [r13+0x118]` (a root-signature object);
- `0x14248c373` calls `[rax+0x108]` (slot 33);
- afterwards `[rsi+0x32150]` shadows the object and
  `or byte [rsi+0x1252c], 0x70` marks the root state dirty.

The plan's first guess (#33 = PC 29 `EndQuery`) was wrong: PC
`EndQuery`/`SetPredication` are flat 57/59, outside the 28..51 remap
block. The argument shape (one object pointer) is the compute root
signature.

### `#35 SetComputeRootDescriptorTable`

Console `#35` is PC `#31` = `SetComputeRootDescriptorTable`. In the same
function:

- `0x14248c423` gets the small root-parameter index
  (`[r13+rbp-0x8e34]`, masked to a byte);
- `0x14248c462` sets `r8 = r15`, a GPU descriptor address;
- `0x14248c465` calls `[rax+0x118]` (slot 35) with
  `(list, parameter, address)`.

That is exactly `SetComputeRootDescriptorTable(param, base)`; the
fallback is taken when the inline root-view record did not fit.

## Honest negatives (`#75`, `#126`, `#142`)

These three console-only slots are still called once per frame. None has
a public GDK page, an entry in the Windows twin's observed console set
(`list_x.rs`, `#60`–`#148`), or a static name; they stay logging stubs.

### `#75` — console boolean setter

`0x140032809` compares the shadow `[rdi+0x47a88]` with `bl`, where
`bl = (([framebuffer+0x10]+0x55 >> 6) & 1)` and `framebuffer =
[rdi+0x12328]`. On a change, `0x140032811` loads `[r14]` and calls
`[rax+0x258]` with `edx = bl`, then stores the shadow at `0x140032820`.
There is no inline record for this state anywhere in the function.
A one-bit console graphics state is consistent with a hint/toggle, but
no name is provable from the title or the public reference; guessing a
handler would hide an unknown state change.

### `#126` — enum + 0x20-byte state structure

`0x1400325c0` reads the packed state word `[rdi+0x12380]`, compares it
with the shadow `[rdi+0x12400]`, and on a change:

- broadcasts the four state bytes into `rsp+0x110..0x11f`;
- copies words `[rdi+0x12378]`/`[rdi+0x1236e]`, bytes
  `[rdi+0x1236c]`/`[rdi+0x1237a]&0x7f`, a topology byte, the qword at
  `[rdi+0x12410]`, and the byte at its `+8` into the same 0x20-byte
  struct;
- calls `[rax+0x3f0]` (slot 126) with `edx = first state byte`,
  `r8 = &struct`;
- stores `[rdi+0x12400] = [rdi+0x12380]`.

The struct mixes pipeline and render-target-derived fields; without a
name for the slot, writing it to a handler would invent semantics.

### `#142` — resource pointer with a `-1` sentinel

`0x1400329a1` gates on dirty bit 21 of the title's state mask, reads
`rdx = [rdi+0x12360]` and compares it with the shadow `[rdi+0x123e0]`.
On a change, `0x1400329e3` calls `[rax+0x470]` (slot 142) with that
pointer, then stores the shadow at `0x1400329d9`.

`[rdi+0x12360]` is set from a source object's `+0x28` (seen in the
setters at `0x1455bce40`, `0x14175e32f`, `0x1435a637c`,
`0x144e58029`), cleared to `-1` by resets, and also sets bit `0x2000000`
in the inline index-buffer command word at `0x1400327cb`. That is a
resource binding, not an index buffer (the index view is `[rdi+0x12638]`
at `0x140032830`). It looks like predication, but console `#85` is
already `SetPredicationBufferX` and no observed table moves it to 142;
the `-1` sentinel and the extra header bit alone are not a name. Kept a
stub until a call site or a GDK reference names it.

## Implementation

`crates/xcl/src/hle/d3d12x_linux.rs`:

- `ListDrawRecord` gains
  - `compute_root_sig: usize`,
  - `compute_root_table: Option<(u32, u64)>`,
  - `dispatches: u64`;
- `List_SetComputeRootSignature(rs)` stores the pointer;
- `List_SetComputeRootDescriptorTable(param, base)` stores the pair;
- `List_Dispatch(x, y, z)` counts the call only — it never creates a
  `DrawOp`, never increments `draws`, and therefore never feeds the
  `#28/#29` occlusion-query bank;
- `console_base_handler` routes PC `29`/`31` (console `#33`/`#35`);
- `v[14] = List_Dispatch` on both `CommandList` and `VulkanList` (the
  layout has no shift for `#14`).

`#75`, `#126`, `#142` keep the table’s logging stub; their first-call
warning and `called_stubs` entry remain the honest negative.

## Tests

- `console_list_base_slots_match_the_windows_twin` pins the routed
  console slots as `32, 33, 34, 35, 36, 44, 47, 48, 50, 52`.
- `console_base_slots_store_blend_stencil_roots_and_vertices` calls
  `#33`/`#35` through the vtable and checks `compute_root_sig` /
  `compute_root_table`.
- `console_dispatch_counts_without_drawing` pins `slot(14)`, calls
  `Dispatch(5, 0x21, 1)`, and checks `dispatches == 1`, `draws == 0`,
  `ops.is_empty()`.
- `per_frame_console_only_slots_stay_honest_stubs` pins
  `slot(75|126|142) == list_stub(...)`.

RED: with the `console_base_handler` 29/31 arms and `v[14]` removed
(the rest of the change kept), both behavioural tests fail at the
vtable identity assertions. GREEN: focused module
`hle::d3d12x_linux::tests` **66 passed / 0 failed**.

Full gates at this tree: `cargo check -p xcl --tests` 0 errors,
`cargo check --workspace` 0 errors, lib **946 passed / 0 failed / 1
ignored** at `--test-threads=4`, `launcher_e2e` **7 passed / 0 failed**.

## Retail delta

ONE plain fresh-root debug run, `/tmp/xcl-step76-r1`, same title and
config as Step 75:

- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651` — the
  Step-67 title-side allocator trap, unchanged;
- first frame `3840x2160` at **6.833 s**, **134** presented frames
  (Step 75: 130), last at 11.917 s;
- **42** called stubs (Step 75: 45); `ID3D12GraphicsCommandList::#14`,
  `#33` and `#35` are gone;
- the honest negatives remain: `#75`/`#126`/`#142` at 108 calls each,
  `#131`/`#132` at 6/5;
- no new warning from the routed handlers.

The frontier is still the title-side allocator trap
(`docs/retail-alloc-throw.md`). Next called-stub inputs after this step:
`#75/#126/#142` until something names them, then device `#78–#81`,
queue `#31`, allocator `#9`, and the CRT math stubs.
