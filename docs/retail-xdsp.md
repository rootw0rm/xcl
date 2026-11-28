# Retail M1 — Step 60: the chad DSP, and the audio init is stub-free

Date: 2026-11-28. Step 60. Base: `linux` @ `fce84055` (Step 59 docs).
Task `code-assist:xcl-first-frame:step-60:xdsp`.

## Result in one paragraph

The audio init's last stub is gone. `XDspConnectWithMaximumStreamLimit`
(chad.dll, called at `exe+0x5b5262e`) now returns a real handle, so
`0x145b52510` returns success and the enclosing init at `exe+0x43ad...`
returns to its caller without the `int3` at `exe+0x43ad94e`. Retail logs
the DSP connect (128 streams) and continues into the game. The next fault
is elsewhere: SIGSEGV at `exe+0x1b332ea` (`call [rdx+8]`, fault `0x0`)
after a table slot is loaded as an object pointer.

## Fix

`hle/acphal_linux.rs` gains the Windows `hle/acphal.rs` XDsp slice with
`extern "win64"`:

- `XDspConnectWithMaximumStreamLimit(a, b, c, streams, flags, &handle)`:
  a boxed `XDsp { streams }`; a null out pointer is `E_INVALIDARG`.
- `XDspDisconnect`: drops the box.
- `XDspActivate`, `XDspDeactivate`, `XDspSubmitCommand`: acknowledge a
  non-null handle (`S_OK`); a null handle is `E_INVALIDARG`. The first
  submitted command is logged once. The DSP runs nothing in M1.
- All five names are wired through `acphal_exports`; the acphal route is
  now fully implemented for the console audio surface (APU, SHAPE,
  AcpHal, XApu, XDsp). `resolver_linux`'s stub probe moves to
  `d3d12_xs.dll!ID3D12Device::#70`, still stub metadata.

## Tests (RED first)

- `xdsp_entry_points_are_wired`: the five names are real.
- `xdsp_connects_acknowledges_and_disconnects`: retail's connect shape
  (`a`, `b`, `streams = 0x80`, `flags = 1`) yields a handle; activate,
  deactivate and submit acknowledge it; disconnect frees it; null
  handles/out pointer are `E_INVALIDARG`.

RED (the five names removed from the export merge): **10 passed /
2 failed** ("XDspConnectWithMaximumStreamLimit must be implemented, not
stub metadata"). GREEN: **12/12**. Full gates: checks 0, lib **908 passed
/ 0 failed / 1 ignored**, launcher **7/0**.

## Retail

`/tmp/xcl-step60-r1` (fresh root):

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 7.0 s.
- Log adds `acphal: DSP connected (128 streams) – its commands are taken,
  nothing runs`; the audio init completes.
- Report exit **139** (SIGSEGV) at `exe+0x1b332ea`, fault `0x0`, code
  bytes `ff 52 08 48 8b 06 …` (`call [rdx+8]`). The `+0x43ad94f` trap is
  gone. 40 called stubs.

gdb `/tmp/xcl-step60-g1`: at `0x141b332b0` entry `rcx` is a stack object,
`rdx = 0x14678dab0`; `[rcx] = 0x145f42e00` and the indexing dword at
`[[rcx]+4]` is `136`. The fault site loads
`rax = [rsi + 0x88 + 0x48] = 0x1441f6c20` (inside `.text`, not an object),
then `[rax] = 0x20c483482078c085` (code bytes) and calls `[rdx+8]` with
`rdx = [rax]`. The only code return address on the stack is
`0x1456143db`.

## Next frontier

`exe+0x1b332ea` and its caller `0x1456143db`: find which table slot holds
the bogus pointer, what fills it, and whether an earlier HLE answer (or
the now-succeeding audio init path) leaves it unset. Step 61 samples the
caller, the indexed table, and the object type before routing anything.
