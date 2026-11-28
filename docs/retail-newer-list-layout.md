# Retail Step 68 — newer console command-list layout on the Linux backend

Date: 2026-11-28. Step 68. Base: `linux` @ `0d796341` (Step 67 docs).
Task `code-assist:xcl-first-frame:step-68:newer-list-layout`.

## Result in one paragraph

The Linux null backend now numbers the console command-list methods the way
the title's runtime does. `NEWER_LIST_SLOTS` (the same 22 pairs as the
Windows twin) drives a call-time slot thunk: while the layout byte is unset a
slot keeps the older meaning, and the first newer-only call (queue `#16/#17`
with a title fence, device `#108/#127/#133`, or list `#139`) flips it for
every live and future list. Missing handlers were added for `#32`
`SetDescriptorHeaps`, `#68`/newer-`#70` `OMSetDepthBounds`, newer `#61`
`ExecuteIndirect` and newer `#139` Reset; the implemented console methods
(`CopyMemoryX`, `FillMemoryWith32BitValueX`, the write/wait/timestamp family,
`Write64BitValueTopOfPipeX`, `KickoffX`, `RSSetDepthBiasX`, `DrawIndexedX`)
are reachable at both numbering schemes. Retail keeps presenting
`3840x2160`, no longer panics in `VulList_TimestampBottom`, and still exits
133 at the Step-67 allocator trap `+0x2219651`.

## Evidence (already in the Step-67 artifacts; no new gdb)

`/tmp/xcl-step67-r1` (plain fresh root, pre-fix):

- list `#139` ×2 (`exe+0x2474cd4`), `#74` ×1 (`exe+0x2484b12`, args
  `0x7 0x1`), `#32` ×3, `#52` ×1, `#131/#132` ×5 — all runtime logging
  stubs;
- `#74`'s `(0x7, 0x1)` shape is `Wait64BitValueX(function=GREATER_EQUAL,
  value=1)`, not the older layout's `#74`;
- `/tmp/xcl-step66-g4.out:16238` panicked at `d3d12x_linux.rs:2338`,
  `VulList_TimestampBottom` writing a u64 to `va=0x1`. `rdx=1` is the unused
  rdx of a **float-argument** call, i.e. the title's newer `#70` is
  `OMSetDepthBounds(min, max)`, not `Write64BitValueBottomOfPipeX`.

`hle/d3d12x/real.rs` (Windows twin, not compiled on Linux) supplies the
reference: `console_slots(Kind::CommandList)` at `:1019-1087`,
`NEWER_LIST_SLOTS` at `:8275`, and `QUEUE_SHIFTED` set by queue `#16/#17`
(`is_fence`), device `#108/#127/#133` (`note_newer_device_method`) and read
by the layout split thunks (`guard::make_layout_split`).

## Slot map

`NEWER_LIST_SLOTS` is copied byte-for-byte; the test pins the length and
every pair:

```
(61,59) (62,60) (63,61) (64,62) (65,63) (70,68) (71,69) (72,70)
(73,71) (74,72) (80,78) (83,81) (85,83) (86,84) (87,85) (90,88)
(91,89) (92,90) (95,93) (96,94) (129,127) (139,10)
```

Slots this backend implements on either side get a thunk
(`legacy_list_handler` / `newer_list_handler`):

| slot | older layout (flag clear) | newer layout (flag set) |
|---|---|---|
| `#61` | `FillMemoryWith32BitValueX` | `ExecuteIndirectX` |
| `#62` | stub (`Fill64` unimplemented) | `CopyMemoryToMemoryX` |
| `#63` | stub (`Fill128` unimplemented) | `FillMemoryWith32BitValueX` |
| `#70` | `Write64BitValueBottomOfPipeX` (timestamp) | `OMSetDepthBounds` |
| `#71` | `Wait32BitValueX` | `Write32BitValueBottomOfPipeX` |
| `#72` | `Wait64BitValueX` | `Write64BitValueBottomOfPipeX` |
| `#73` | stub | `Wait32BitValueX` |
| `#74` | stub | `Wait64BitValueX` |
| `#83` | stub | `DrawIndexedX` (draw counter) |
| `#91` | stub | `Write64BitValueTopOfPipeX` |
| `#95` | stub | `KickoffX` |
| `#96` | stub | `RSSetDepthBiasX` |
| `#139` | newer-only entry: a call latches the layout, then Reset | 3-arg console Reset |

The remaining table pairs (`64/65/80/85/86/87/90/92/129`) have no Linux
handler on either side and stay the honest logging stub — never the wrong
older method. `#32` is console `SetDescriptorHeaps(count, heaps)` in both
layouts (S_OK; the null backend binds no heaps), and `#68` is
`OMSetDepthBounds` in both (store-only).

`VulList_TimestampBottom` is therefore reachable only at the timestamp slot
(older `#70` / newer `#72`). There is no `va < page` guard and no
small-address skip.

## Mechanism

- `NEWER_LIST_LAYOUT: AtomicBool` — the table's layout flag.
- `layout_thunk(older, newer, flag)` generates 40 bytes of code that clobber
  only the volatile `rax`/`r10`:

  ```
  mov rax, flag ; cmp byte [rax], 0
  mov rax, older ; mov r10, newer ; cmovne rax, r10 ; jmp rax
  ```

  All argument registers (`rcx/rdx/r8/r9`, `xmm0–15`) and the caller's stack
  arguments reach the target untouched; the flag is read at **each call**, so
  lists created before the runtime identifies itself route correctly. (A
  Rust dispatcher was rejected: calling a SysV function clobbers `rdi/rsi`
  and xmm, which are non-volatile in the title's win64 ABI.)
- `note_newer_list_layout()` latches and logs once. Called from
  `Queue_Slot16`/`Queue_Slot17` when `is_title_fence(a1)`, from
  `Device_GetResourceAllocationInfo2` (`#108`),
  `Device_CreateCommittedResourceLayoutX` (`#127`),
  `Device_CreatePlacedResourceLayoutX` (`#133`), and from
  `VulList_Slot139` before the console Reset.
- `List_ResetX` re-opens the null list (clears `closed`, records the NULL
  initial PSO the title passes); `List_OMSetDepthBounds` stores
  `[min, max]` in the list record like the existing dynamic depth bias.

## Tests

`cargo test -p xcl --lib d3d12x_linux::tests`: **41 passed / 0 failed**.

- `newer_list_slot_table_matches_the_windows_twin` — pins all 22 pairs and
  the exact set of wired slots (`61,62,63,70,71,72,73,74,83,91,95,96,139`).
- `newer_list_layout_selects_the_moved_methods` — newer `#70` is
  `List_OMSetDepthBounds` (not `VulList_TimestampBottom`), newer `#72` is
  `VulList_TimestampBottom`, `#74` `VulList_Wait64`, `#32`
  `List_SetDescriptorHeapsX`, `#139` the Reset entry; older `#70` is the
  timestamp, `#71` Wait32, `#72` Wait64, `#139` the stub.
- `newer_list_layout_routes_depth_bounds_timestamp_and_reset` — end to end
  through the vtable: older `#70` writes a timestamp; a `#139` call latches;
  newer `#70` stores depth bounds without touching the timestamp cell; `#72`
  writes the timestamp; `#74` Wait64 and `#32` return.
- `list_layout_thunk_reads_the_flag_at_each_call_and_preserves_arguments` —
  integer, stack and floating-point arguments survive the thunk and the
  target follows the flag across three calls.

RED at the parent tree (`0d796341`, temporary worktree): the parent's
`v[70]` is `VulList_TimestampBottom`, so
`assert_ne!(slot(list, 70), VulList_TimestampBottom)` fails
(**0 passed / 1 failed**, 916 filtered). The existing
`null_list_shares_the_console_memory_methods` test now holds the layout
serial lock and pins the older numbering; the queue `#16` signal tests hold
it too so their latch cannot interleave with the older-layout assertions.

## Retail delta

Fresh root `/tmp/xcl-step68-r1`, debug binary at this tree, one plain run,
no retries:

- first frame `presented 3840x2160` at **6.798 s** (Step 67: 6.803 s);
- **126** presented frames (the previous gate run reached the allocator trap
  before this many);
- the newer-layout log line appears once:
  `D3D12.X: the title's runtime uses the newer console command-list layout`;
- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651`,
  `pc 0x142219651`, fault 0 — the Step-67 allocator OOM unchanged;
- **57** called stubs instead of 44: `#139`, `#74` and `#32` are gone from
  `called_stubs`; the extra entries are the longer run's math (`erff` 4800),
  `ws2_32`/`bcrypt`/`EventRegister` and the still-unimplemented d3d12 slots
  (`Device::#70` 715, `List::#52` 101, `List::#131/#132`);
- no `d3d12x_linux.rs:2338` panic and no `VulList_TimestampBottom(va=1)`
  write.

## Next consumer-visible input

The gate frontier is now only the Step-67 allocator trap. Reopening it needs
the pool-fill provenance (Step 67 reopen trigger 2): an alloc/free trace of
the title pool `0x1471c2808` showing why only 2.57 MiB of `0x89700000` is
free at the first frames — a title-side free that the console performs and
XCL prevents, or a title leak. If that trap is cleared first, the next
missing list handlers are `#52` and `#131/#132` (stubs, not layout).

## Gates

At this tree:

- `cargo check -p xcl --tests`: exit 0 (warnings only).
- `cargo check --workspace`: exit 0 (warnings only).
- `cargo test -p xcl --lib -- --test-threads=4`: **919 passed / 0 failed /
  1 ignored**.
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Files

- `crates/xcl/src/hle/d3d12x_linux.rs` — `NEWER_LIST_SLOTS`,
  `layout_thunk`, slot routing, `List_SetDescriptorHeapsX`,
  `List_OMSetDepthBounds`, `VulList_ExecuteIndirect`, `List_ResetX`,
  `VulList_Slot139`, layout latches, `ListDrawRecord::depth_bounds`, tests.
