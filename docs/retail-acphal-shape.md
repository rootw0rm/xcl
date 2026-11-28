# Retail M1 — Step 58: the SHAPE contexts and the AcpHal channel

Date: 2026-11-28. Step 58. Base: `linux` @ `53e77461` (Step 57 docs).
Task `code-assist:xcl-first-frame:step-58:acphal-shape`.

## Result in one paragraph

The audio init's second check passes. `exe+0x43aec40` first calls
`AcpHalAllocateShapeContexts` (retail's counts are `[0, 0, 0, 0, 350]`),
then `AcpHalCreate`, the channel's Connect (4096 commands / 4096 messages),
and SubmitCommand with the title's first command (kind 2, id
`0xf0f0f0f0`). All return success, so `exe+0x43ad943` no longer takes the
`int3`. The rerun stops at the same assert site via its last check instead:
`XApuConnect` (thunk `0x145f40110`, called from `exe+0x43aee69`) is still an
auto-stub and returns `E_NOTIMPL`.

## Fix

`hle/acphal_linux.rs` gains the Windows `hle/acphal.rs` SHAPE and AcpHal
slices, with `extern "win64"`:

- `ShapeContexts` (five counts, reserved, five pointers), 64-byte entries,
  a process-global `SHAPE` registry. Allocation goes through `ApuAlloc`;
  a partial failure frees the earlier arrays and nulls every pointer in the
  caller's struct; a successful call releases the previous arrays.
  `AcpHalReleaseShapeContexts` releases the registry (idempotent).
- `Channel`: queued messages, command acks for id != 0, a 128-sample audio
  frame every 2667 µs while connected, reconnect clearing stale messages.
- `Hal` object with the six-slot vtable (Connect, Disconnect, SubmitCommand,
  PopMessage, GetNumMessages, Release) and `AcpHalCreate`. The vtable is a
  process-global `LazyLock<[usize; 6]>`; the object is a boxed `Hal` freed
  by Release. The channel takes the same command shapes retail sends and
  logs each command kind once.
- Both are wired through `acphal_exports`; the XApu/XDsp channels stay stub
  metadata. `resolver_linux::routed_imports_resolve_to_callable_stubs` now
  probes `XApuConnect` (the next real stub) instead of `AcpHalCreate`.

No audio is mixed: the console's SHAPE mixer does not run, matching the
Windows twin's M1 contract.

## Tests (RED first)

Resolve the three names through `crate::hle::registry()`:

- `acphal_shape_and_channel_entry_points_are_wired`.
- `shape_contexts_allocate_release_and_roll_back`: counts `[2,1,0,0,0]`
  allocate two non-null arrays and three null; release is idempotent; a
  null argument is `E_INVALIDARG`; the internal allocator callback test
  fails on kind 1 and proves kind 0's allocation was freed and every
  pointer cleared.
- `channel_acknowledges_and_ticks` and
  `reconnect_drops_messages_queued_for_the_old_channel`: the Windows
  channel unit tests, unchanged.
- `hal_object_connects_acknowledges_and_releases`: drives the real vtable
  slots: Connect, SubmitCommand (kind 2, id `0xf0f0f0f0`, frame
  `0xffffffff`, size 0), PopMessage gets `MSG_COMMAND_DONE`, a null
  message pointer is refused, Disconnect queues `MSG_DISCONNECTED`, Release
  drops the object.

RED (the three new names removed from the module's export merge, all else
present): **5 passed / 3 failed**, "AcpHalAllocateShapeContexts must be
implemented, not stub metadata". GREEN: **8/8**. Full gates: checks 0, lib
**904 passed / 0 failed / 1 ignored**, launcher **7/0**.

## Retail

`/tmp/xcl-step58-r1` (fresh root):

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 6.9 s.
- Log adds `acphal: SHAPE contexts [0, 0, 0, 0, 350]`,
  `acphal: audio processor connected (4096 commands, 4096 messages)`,
  `acphal: audio processor command 0x2 first seen`.
- Report exit **133** (SIGTRAP) at `exe+0x43ad94f`; shell exit 139. No
  acphal stub is called any more.

gdb `/tmp/xcl-step58-g1`: `exe+0x43ad945` sees `al = 1` from
`exe+0x43aec40` (SHAPE + AcpHal succeeded). The last check calls
`XApuConnect(&input, &output, &handle)` from `exe+0x43aee69` with
`{process 1, streams 256, commands_per_stream 2, data_bytes 0x104000,
frames_per_output 0, options 1}`; the stub returns `E_NOTIMPL`, the
function's `setns al` leaves `al = 0`, and `exe+0x43ad94e` asserts.

## Next frontier

`XApuConnect` (and the XApu command channel behind it). The Windows
`hle/acphal.rs` already covers the whole decoder contract, including the
silence answer when no Opus decoder exists; Step 59 ports that channel with
silence output and the request/result structures retail polls. That is the
last stub before the audio init can return success.
