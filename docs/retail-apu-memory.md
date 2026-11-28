# Retail M1 — Step 57: the APU memory slice, then the SHAPE contexts

Date: 2026-11-28. Step 57. Base: `linux` @ `2d681e83` (Step 56 docs).
Task `code-assist:xcl-first-frame:step-57:apu-memory`.

## Result in one paragraph

`ApuCreateHeap` no longer asserts. Retail's audio init called it with
`(0x1c700000, 0x226e00)` at `exe+0x43ad933`; the generic auto-stub returned
`E_NOTIMPL`, the caller's `js` took it to the title's `int3` at
`exe+0x43ad94e`, and the run stopped after its first frame. The Linux APU
memory slice now implements the whole family the console's audio engine
uses to address its buffers — `ApuCreateHeap`, `ApuAlloc`, `ApuFree`,
`ApuMapVirtualAddress`, `ApuMapApuAddress`, `ApuIsVirtualAddressValid`,
`ApuHeapGetState` — with the Windows semantics (`hle/acphal.rs` is the
source of truth). Retail still stops at the same `int3`, but the failing
call moved: the APU heap succeeds and the next check,
`AcpHalAllocateShapeContexts`, is still an auto-stub.

## Fix

New `hle/acphal_linux.rs`, wired through `hle_linux::acphal_exports` (the
`gameinput_linux` pattern) and therefore through the existing
`acphal.dll`/`xapu.dll`/`chad.dll` routes:

- `ApuCreateHeap(size, flags)` → `S_OK`; the console's APU heap is the
  process's memory, as in the Windows twin (logged, nothing allocated).
- `ApuAlloc(&va, &apu, size, alignment, flags)` → a zeroed allocation via
  `kernelx_linux::VirtualAlloc` at the requested power-of-two alignment
  (above 64 KiB: reserve/align/re-release, the same dance as the Windows
  implementation) plus a non-overlapping 32-bit APU address in
  `[0x10000000, 4 GiB)`.
- `ApuFree(va)` releases only ranges this module owns;
  `ApuMapVirtualAddress` gives the title's own memory an APU address;
  `ApuMapApuAddress` resolves it back; `ApuIsVirtualAddressValid` → 1;
  `ApuHeapGetState` writes the five counters.
- The 4 GiB wrap/overlap logic, including the "owned allocation gets its
  own entry even when an unowned mapping covers the VA" case, is the
  Windows code ported with `extern "win64"`.

SHAPE contexts, `AcpHalCreate`/the channel, and the XApu/XDsp command
channels remain stub metadata; the next measured consumer is the SHAPE
call.

## Tests (RED first)

The tests resolve the names through `crate::hle::registry()` (the
`acphal.dll` route), so the pre-fix failure is "ApuCreateHeap must be
implemented, not stub metadata" rather than a compile error.

- `apu_entry_points_are_wired`: all seven names have real addresses.
- `apu_memory_maps_both_ways`: `ApuCreateHeap(0x1c700000, 0x226e00)`;
  `ApuAlloc(0x3000, align 0x20000)` is aligned; both mapping calls
  round-trip; `ApuHeapGetState` sees the block; `ApuFree` removes the
  range; null/invalid args are `E_INVALIDARG` without writing outputs.
- `apu_address_prevents_overflow_overlap_and_rejects_null_va`: the
  Windows boundary test, unchanged.

RED (merge loop removed from `acphal_exports`, all else present):
**1 passed / 2 failed**, "ApuCreateHeap must be implemented, not stub
metadata" `left: 0 right: 0`. GREEN: **3/3**. Full gates: checks 0, lib
**899 passed / 0 failed / 1 ignored**, launcher **7/0**.

## Retail

`/tmp/xcl-step57-r1` (fresh root):

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 7.1 s.
- `acphal: ApuCreateHeap(0x1c700000, 0x226e00) – the audio processor's
  memory is the process's`.
- Report exit **133** (SIGTRAP) at `exe+0x43ad94f`; shell exit 139.
  `ApuCreateHeap` is gone from the called stubs.

gdb `/tmp/xcl-step57-g1`: the APU check at `exe+0x43ad93a` now sees
`eax = 0` and calls `0x1443aec40`; that function returns 0, so
`exe+0x43ad943 je` reaches the same `int3` (`al = 0`, `rax = 0` at the
trap). The call inside is `exe+0x43aec73 → 0x145f40160`, resolved from
the IAT as `AcpHalAllocateShapeContexts`; the rest of the function is the
AcpHal setup and the XApu connect:

- `0x1443aed98` `AcpHalCreate(&global)` (thunk `0x145f40170`);
- `0x1443aedba` `handle->vtable[0](handle, 0x1000, 0x1000)` (Connect);
- `0x1443aedcd` `CreateEventA`;
- `0x1443aee2f` `handle->vtable[0x10]` = SubmitCommand with
  `(kind 2, id 0xf0f0f0f0, frame 0xffffffff, data → {0xc0}, size 0)`;
- `0x1443aee50` → `XApuConnect(&in, &out, &handle)` (thunk `0x145f40110`).

## Next frontier

`exe+0x43ad94e` is the same assert site, now reached because
`AcpHalAllocateShapeContexts` (`exe+0x43aec73`) is a stub. Step 58 ports
the SHAPE-context allocation and the `AcpHalCreate` object/channel; the
following `XApuConnect` is Step 59. Both exist in the Windows
`hle/acphal.rs`; the XApu decoder can answer silence where Linux has no
Opus decoder (the Windows path already does).
