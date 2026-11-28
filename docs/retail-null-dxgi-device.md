# Retail M1 — Step 40: null-device DXGI device wrapper; frontier reaches memory placeholders

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-40:null-dxgi-device`.
Base: `linux` @ `0c9c873c` (Step 39 docs). Product code + test @ `a6bf1559`.

## Result in one paragraph

`QueryInterface(IID_IDXGIDevice/1/2, IID_IDXGIObject)` on the null device now
returns the same `DxgiDevice` wrapper the Vulkan device already used, whose
`value` is the device. Retail's `GetAdapter` (slot 7) therefore resolves to
`Dxgi_GetAdapter` instead of `Device_GetNodeCount`, and the
`DxgiAdapter → GetDesc(LUID) → EnumOutputs(0)` chain works. The former
Step-39 fault (`exe+0x2481d37`, NULL adapter) is gone: boot advances through
the DXGI chain, records two more console command-list extension calls, and
stops at **exit 133 SIGTRAP `exe+0x19f4f59`**. gdb shows that trap is a
title `int3` after `0x142478440` returns false, because
`VirtualFree(addr, 0xa00000, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER)` at
`0x141e864cd` returns 0. That is the next defect (Step 41), not the list
slots.

## Fix and test

- `QueryInterface` (`hle/d3d12x_linux.rs:454`) builds `Kind::DxgiDevice` for
  `matches!(kind, Kind::Device | Kind::VulkanDevice)` with
  `dxgi_device_iid(riid)` (`IID_IDXGIObject`, `IID_IDXGIDevice`,
  `…1`, `…2`). Every other IID still gets the object itself, so existing
  callers see no change.
- `Dxgi_QI` already round-trips `IID_ID3D12DEVICE` to the wrapper's device and
  keeps the DXGI identity; `Dxgi_GetAdapter` returns a `DxgiAdapter` whose
  `GetDesc` carries the device LUID (constant `0x58434C00`, equal to
  `Device_GetAdapterLuid`). The adapter name now falls back to the honest
  `"XCL console adapter"` when no Vulkan device backs it (a Vulkan device
  still uses its real adapter name).
- New test `null_device_dxgi_chain_serves_adapter_and_output`
  (`hle::d3d12x_linux::tests`) drives retail order: QI `IID_IDXGIDevice` →
  wrapper (not the device) → DXGI1 answers itself → `IID_ID3D12DEVICE`
  round-trips → slot 7 `GetAdapter` → slot 8 `GetDesc` (LUID == slot 43) →
  slot 7 `EnumOutputs(0)` is a `DxgiOutput`, `(1)` is
  `DXGI_ERROR_NOT_FOUND`. The existing Vulkan test was updated for the new
  null-device contract and still asserts non-DXGI IIDs return the device.

RED (wrapper condition temporarily reverted to `Kind::VulkanDevice`):

```
assertion `left != right` failed: DXGI QI must not hand back the device
  left: 0x7f502000b380
 right: 0x7f502000b380
test result: FAILED. 0 passed; 1 failed; 877 filtered out
```

GREEN at HEAD: the same test passes.

## Retail evidence

Fresh root `/tmp/xcl-step40-r1`, debug binary at `a6bf1559`:

```
exit 133, crash SIGTRAP (signal 5) at game_scarlett_master.exe+0x19f4f59
       pc 0x1419f4f59, fault 0x0
called_stubs: list #131 ×5 (0x2478bc5), list #132 ×5 (0x2478ba4),
              ID3D12Device::#79 ×2 (0x247ac02), ID3D12Device::#78 ×1
              (0x247abde), IDXGIOutput::#21 ×1 (0x2481d9d),
              LocaleNameToLCID ×2, ShowWindow ×1, ws2_32#115 ×1,
              IGameInput::#9 ×1
```

The old SIGSEGV is replaced by the DXGI chain working end to end (the output
object is created and `#21` is called on it). `ID3D12Device::#78/#79` are the
null device's unrouted `SetFrameIntervalX` / `ScheduleFrameEventX` console
slots, called from `0x14247abde/0x247ac02` with `(0x8235, 3)` and `(0, 0)`;
both return 0. Each call is followed by `test eax,eax; js` to the common
`ret`, so a failure only skips the rest of that frame helper — no trap. They
are not this frontier.

### Console list #131 / #132 identified

`0x142478440` (title window + D3D12 setup) creates an allocator with
`IID_ID3D12CommandAllocator` (`0x1461e0658`) and a list via device #73 with
`IID_ID3D12GraphicsCommandList6` (`0x1461e0668`), then calls on that fresh
list, in this order:

```
0x142478b9e  edx=0x700000, r8d=0, r9d=0, [rsp+0x20]=0xffffffffff00
             call [rax+0x420]              ; console list #132
0x142478bbf  edx=0xc3c0, r8d=0x80020000, r9d=0, [rsp+0x20]=0xffffffffff00
             call [rax+0x418]              ; console list #131
0x142478bcd  call [rax+0x48]               ; list #9 Close
```

Both returns are ignored (no `test eax` before the next call), so the stubs
do not gate boot. The arguments look like GPU VAs / handles but no consumer
needs their semantics yet; they stay honest logging stubs.

### Next blocker (Step 41): `MEM_PRESERVE_PLACEHOLDER`

The trap is reached through the title's `0x145c5ff00` → `0x141e86440`
allocation helper; gdb breakpoints after each check show:

```
after RegisterClassExA  ax=1
after CreateWindowExA   rax=0x20000
after 0x145f3fd80       edi=0
after device slot 8     eax=0
VirtualFree(base=0x7ff2282fd000, size=0xa00000, ftype=0x8002) -> eax=0
frontier test (0x1419f4f54) al=0
```

`0x8002` is `MEM_RELEASE (0x8000) | MEM_PRESERVE_PLACEHOLDER (0x2)`.
`kernelx_linux::VirtualFree` (`hle/kernelx_linux.rs:265-269`) rejects every
`MEM_RELEASE` with `size != 0`; the title's placeholder carving therefore
fails, `0x141e86440` returns 0, `0x142478440` returns 0, and retail executes
`int3` at `0x1419f4f58` (reported as SIGTRAP at `+0x19f4f59`).

Step 41 must implement placeholders: `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`
splits a reservation without losing the VA range, so the following
`VirtualAlloc(base, size, MEM_RESERVE | MEM_COMMIT, …)` calls at
`0x141e8659e/5d0/600/630/66c` can re-commit each carved range. A test must
drive the split-and-reallocate pattern, not just the return code.

## Gates

At `a6bf1559`: `cargo check -p xcl --tests` exit 0, `cargo check --workspace`
exit 0, `cargo test -p xcl --lib -- --test-threads=4` **877 passed / 0 failed
/ 1 ignored**, `cargo test -p xcl --test launcher_e2e` **7 passed / 0 failed**.
Retail fresh root as above (Vulkan adapter present, no skips).
