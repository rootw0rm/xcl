# Retail M1 — Step 59: the XApu decoder channel, then the DSP connect

Date: 2026-11-28. Step 59. Base: `linux` @ `b2771c12` (Step 58 docs).
Task `code-assist:xcl-first-frame:step-59:xapu-channel`.

## Result in one paragraph

The audio init's three checks all pass now: `ApuCreateHeap`, the
SHAPE/AcpHal object and `XApuConnect` return success. Retail logs
`xapu: decoder connected (process 1, 256 streams)` and keeps going. The
`int3` at `exe+0x43ad94e` is still the assertion reached later, because the
audio engine's DSP connect is still a stub: `0x145b52510` calls
`chad.dll!XDspConnectWithMaximumStreamLimit` at `exe+0x5b5262e`, gets
`E_NOTIMPL`, and returns false through its `sete al`.

## Fix

`hle/acphal_linux.rs` gains the Windows `hle/acphal.rs` XApu slice with
`extern "win64"`, silence decoder only (no Opus decoder is linked on Linux,
and the Windows path already defines that case):

- Packed `XApuCommand`, `XApuActivate`, `XApuProcess`, `XApuResult`,
  `XApuConnectInput`, `XApuConnectOutput`, byte-for-byte the title's
  layouts.
- `XApuConnect`: allocates `data_bytes` through the APU allocator
  (`0x104000` from retail), creates a ready event only when `options & 8`
  asks for one, allocates `streams.max(1)` stream slots (256 here), and
  writes `{data, ready}` + handle. Null arguments and allocation failures
  clear the outputs.
- `XApuEnqueueCommand`: Activate records 1/2 channels and active,
  Deactivate clears active, Process writes the asked frames of zeroed
  float32 (falling back to `frames_per_output`, then 960; capped by
  `output_bytes / (channels*4)`) and queues an `XApuResult` with state
  `1|2`, frames/output_bytes/pitch/input echoed and `S_OK`.
- `XApuDequeueResult` (`XAPU_NO_RESULTS` `0x8AC81001`), `XApuGetResultCount`,
  `XApuGetUtilization` (256/256), `XApuDisconnect` (frees data, closes the
  event). All six are wired through `acphal_exports`.
- `resolver_linux`'s stub probe moves to `XDspDisconnect`, the remaining
  acphal stub family.

## Tests (RED first)

Through `crate::hle::registry()`:

- `xapu_entry_points_are_wired`: the six names are real.
- `xapu_connect_process_and_dequeue`: retail-shaped connect (2 streams,
  data 0x100, frames_per_output 4, no event) leaves `data` non-null and
  `ready` null; Activate stereo returns an empty result; Process with
  `frames = 4` writes 32 zero bytes, queues a result with
  `(frames, output_bytes, state) = (4, 32, 3)` and the echoed output
  pointer/pitch; Dequeue then returns `XAPU_NO_RESULTS`; the count is 0;
  utilization is `[256, 256]`; null connect/enqueue/dequeue/`util`
  arguments are `E_INVALIDARG`; Disconnect succeeds and a null handle is
  rejected.

RED (the six names removed from the export merge, all else present):
**8 passed / 2 failed** ("XApuConnect must be implemented, not stub
metadata"). GREEN: **10/10**. Full gates: checks 0, lib **906 passed /
0 failed / 1 ignored**, launcher **7/0**.

## Retail

`/tmp/xcl-step59-r1` (fresh root):

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 7.0 s.
- Log adds `xapu: decoder connected (process 1, 256 streams) – Opus is not
  decoded on Linux: Process answers silence`.
- Report exit **133** (SIGTRAP) at `exe+0x43ad94f`; shell exit 139. The
  only remaining called stub on the path is
  `chad.dll!XDspConnectWithMaximumStreamLimit` (one call, first caller
  `exe+0x5b52633`).

gdb `/tmp/xcl-step59-g1`: the three init checks print success
(`exe+0x43ad93c` `eax = 0`, `exe+0x43ad945` `al = 1`,
`exe+0x43ad958` reached). Execution then reaches `exe+0x43ada4f` →
`0x145b52510`; that helper calls the `XDspConnectWithMaximumStreamLimit`
thunk at `0x145b5262e`, whose `E_NOTIMPL` makes `0x145b52633`'s
`sete al` false, and `exe+0x43ada59 je 0x1443ad94e` asserts.

## Next frontier

The DSP connect (`chad.dll`): `XDspConnectWithMaximumStreamLimit`,
`XDspDisconnect`, `XDspActivate`, `XDspDeactivate`, `XDspSubmitCommand`.
They are the last stub family in the Windows acphal twin (a `streams`
counter object and acknowledged commands); Step 60 ports them. After that
the audio init has no stub left on its path.
