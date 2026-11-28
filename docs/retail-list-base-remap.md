# Retail Step 70 — the console command list's base slot layout

Date: 2026-11-28. Step 70. Base: `linux` @ `1db157ba` (Step 69 docs).
Task `code-assist:xcl-first-frame:step-70:list-base-remap`.

## Result in one paragraph

The Linux null command list now numbers the PC methods the way the console
does. The Windows twin's `hle/d3d12x/real.rs::console_remap(Kind::CommandList)`
base block is mirrored (`console_list_base_slot`): the console list keeps
slots 0–27 and 56–59 in PC order, holds the PC query methods 52–55 at 28–31,
and the PC methods 28–51 four slots further at 32–55. The handlers the null
backend implements for that block are placed at the **console** slot
(`console_base_handler`): `#32` SetDescriptorHeaps, `#34` root signature,
`#36` root descriptor table, `#44` root SRV, `#47` index buffer, `#48`
vertex buffers, `#50` render targets, `#52` ClearRenderTargetView; `#23`/
`#24` OMSetBlendFactor/StencilRef keep the PC index. A PC method without a
handler leaves its console slot the honest logging stub. Retail keeps
presenting `3840x2160`, no longer logs `List::#52/#48/#36/#44/#23/#24` as
stubs, and still exits 133 at the Step-67 allocator trap `+0x2219651`.
`#131`/`#132` remain unidentified console extensions and stay stubs.

## The remap table

`hle/d3d12x/real.rs:899` (`console_remap`), cross-checked by the twin's own
comments: `bind.rs:248` “#36 SetGraphicsRootDescriptorTable (PC #32)” and
`list_x.rs:4813` “command list #48 IASetVertexBuffers … (PC #44)”.

```
console 28..=31 → PC 52..=55   (the query methods move right after ExecuteBundle)
console 32..=55 → PC 28..=51   (the PC methods 28–51 move up by four)
0..=27, 56..=59  → PC order
```

Console slots the retail title calls and their PC methods:

| console | PC | method | handler |
|---|---|---|---|
| `#23` | `#23` | `OMSetBlendFactor` | `List_OMSetBlendFactor` (store) |
| `#24` | `#24` | `OMSetStencilRef` | `List_OMSetStencilRef` (store) |
| `#36` | `#32` | `SetGraphicsRootDescriptorTable` | `List_SetGraphicsRootDescriptorTable` (store) |
| `#44` | `#40` | `SetGraphicsRootShaderResourceView` | `List_SetGraphicsRootShaderResourceView` (store) |
| `#48` | `#44` | `IASetVertexBuffers` | `List_IASetVertexBuffers` (store) |
| `#52` | `#48` | `ClearRenderTargetView` | `List_ClearRenderTargetView` (store) |

Already routed at their console slots before this step (same table, so the
placement is now mechanical): `#32` SetDescriptorHeaps, `#34` root
signature, `#47` index buffer, `#50` OMSetRenderTargets; the query methods
`#28`–`#30` stay at 28–30 (= PC 52–54). PC methods in the shifted block
without a handler (`29/31/33..39/41/42/45/47/49/50/51`) keep the stub —
e.g. console `#33` = PC `#29` SetComputeRootSignature, `#35` = PC `#31`
SetComputeRootDescriptorTable (both still called in retail) and `#51` =
PC `#47` ClearDepthStencilView. Never the wrong method.

## Call-site evidence

- `#52`: `0x1450a62d8` `call qword ptr [rax + 0x1a0]` (slot 52) with
  `rcx` = the list, `rdx` = a descriptor handle from `[rax+0x20]`,
  `r8` = a pointer into the resource's colour area,
  `r9` = `NumRects` = 0, `[rsp+0x20]` = `pRects` = 0 — the
  `ClearRenderTargetView(rtv, colour, 0, NULL)` shape. The Step-68 report
  recorded the same site as 101 calls with `r8` a guest-stack pointer and
  `r9` = 0.
- `#48`: the Step-68 report's 200 calls carry `r8` = `NumViews` = 1 and
  `r9` = `pViews`, the `IASetVertexBuffers(start, 1, views)` shape
  (PC #44 per the twin's `list_x.rs`).
- `#36`: 200 calls, `(param, base descriptor)` = SetGraphicsRootDescriptorTable
  (PC #32); `#44`: 100 calls, `(param, buffer location)` = the root SRV
  (PC #40); `#23`/`#24`: 100 calls each, the blend factor pointer / stencil
  ref.
- The twin's console-only override list (`real.rs::console_slots`) registers
  `#36`, `#42/#44/#46` and `#48` with exactly these methods; the Linux null
  handlers store the same arguments, so a later replay can read them back
  from `ListDrawRecord`.

## `#131` / `#132` — honest negative

`0x142478440` (window + D3D12 setup, the function of
`docs/retail-null-dxgi-device.md`) creates an allocator (device `#9`,
IID `{6102dee4-af59-4b09-b999-b44d73f09b24}`) and a list through device
`#73` with IID `{c3827890-e548-4cfa-96cf-5689a9370f80}` =
`ID3D12GraphicsCommandList6`, then calls on that fresh list, in order:

```
0x142478b9e  #132: edx=0x700000, r8=0,        r9=0, [rsp+0x20]=0xffffffffff00
0x142478bbf  #131: edx=0xc3c0,   r8=0x80020000,r9=0, [rsp+0x20]=0xffffffffff00
0x142478bcd  #9   Close
```

The `r13` loop that follows (`0x2478c1c`–`0x2478ce4`, block taken for
`r13` = 1..4) repeats the pair for four more lists, with `#132` edx =
`0x100000` and `#131` edx = `0xc380`, r8d = `0x20000`, then Close; that is
the report's five `#132` calls (the sixth `#131` comes from a later site).
Both results are ignored (no `test eax` before the next call), so the stubs
do not gate boot — the same conclusion as Step 40.

No semantics can be named: neither slot is in `NEWER_LIST_SLOTS`,
`hle/d3d12x/list_x.rs`, the twin's `console_slots`, or the public GDK
D3D12 reference (`docs.xbox.com` documents only the PC interfaces; no
`d3d12x*.h` header is present on this machine — searched all mounts). The
size-like/flag-like arguments (`0x700000`/`0xc3c0`, `0x80020000`, the
`0xffffffffff00` stack word) do not match any method the twin routes, so
they stay logging stubs per the step's rule. They are not acceptance
criteria.

## Mechanism

- `console_list_base_slot(console)` — the twin's base block, `None` outside
  28–55. The test pins all 28 pairs so a drift between the twins is caught.
- `console_base_handler(pc)` — PC slot → handler for what this backend
  implements; the vtable loop walks console 28–55 and installs the handler
  at the console slot.
- `ListDrawRecord` gained the stored state: `blend_factor`,
  `stencil_ref`, `root_table`, `root_srv`, `vb_start`/`vb_views`,
  `clear_rtv`. All store-only, like the existing S2 fields; the null backend
  has no target to clear or heap to bind.

## Tests

RED at the parent tree (`1db157ba`, temporary worktree, test only using
symbols that exist there):

```
cargo test -p xcl --lib d3d12x_linux::tests::console_base_slots_are_routed
assertion `left != right` failed: #52 ClearRenderTargetView
  left: 140096239110592  right: 140096239110592
test result: FAILED. 0 passed; 1 failed; 920 filtered out
```

GREEN at this tree:

- `console_list_base_slots_match_the_windows_twin` — pins the 28 pairs and
  the exact wired set `[32, 34, 36, 44, 47, 48, 50, 52]`.
- `console_base_slots_store_blend_stencil_roots_and_vertices` — calls every
  new slot through the vtable once and asserts the stored record; console
  `#49` (PC `#45` SOSetTargets) is still the honest stub.

Focused module `d3d12x_linux::tests`: **43 passed / 0 failed** (41 before).
Full gates: `check -p xcl --tests` 0, `check --workspace` 0, lib
**921 passed / 0 failed / 1 ignored**, `launcher_e2e` **7 passed / 0
failed**.

## Retail delta

Fresh root `/tmp/xcl-step70-r1`, debug binary at this tree, one plain run,
no retries:

- first frame `presented 3840x2160` at **6.954 s** (Step 68: 6.798 s);
- **132** presented frames;
- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651`, pc
  `0x142219651` — the Step-67 allocator OOM unchanged;
- **51** called stubs instead of 57; `List::#52`, `#48`, `#36`, `#44`,
  `#23`, `#24` are all gone (the Step-68 report's six slots);
- `List::#131` ×6 (`exe+0x2478bc5`, args `0x80020000 0x0`) and `#132` ×5
  (`exe+0x2478ba4`, args `0x0 0x0`) remain logging stubs, as decided;
- the longer run's new stub entries are the per-frame console slots `#75`,
  `#126`, `#142` (105 calls each, in neither twin table) plus `#14`, `#33`,
  `#35` and the usual math/network set. None is acceptance criteria.

## Next consumer-visible input

The gate frontier is still the Step-67 allocator trap. After the Step-71
upstream triage, the next d3d12 inputs are the per-frame console slots
`#75`/`#126`/`#142` (105 calls each; their call sites must name a semantics
before routing) and, if the trap is cleared, `#51` ClearDepthStencilView
(PC #47) and the compute root-signature/table slots `#33`/`#35`.

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `console_list_base_slot`,
  `console_base_handler`, the six handlers, `ListDrawRecord` fields, vtable
  routing, tests, module doc.
