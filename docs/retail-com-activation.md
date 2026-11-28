# Retail M1 — Step 56: CoCreateInstance was a fake success, then ApuCreateHeap

Date: 2026-11-28. Step 56. Base: `linux` @ `32117105` (Step 55 docs).
Task `code-assist:xcl-first-frame:step-56:com-activation`.

## Result in one paragraph

Step 55's frontier `exe+0x43aea86` was the generic auto-stub answering
`CoCreateInstance` with `S_OK` and never writing the out pointer. The title's
audio init asks for `CLSID_MMDeviceEnumerator` / `IID_IMMDeviceEnumerator`, so
the next instruction loaded the NULL enumerator and called its
`GetDevice`-shaped slot. Linux has no COM class registry, so the honest result
is `REGDB_E_CLASSNOTAVAILABLE` with `*ppv = NULL`; the title checks the
HRESULT and skips the device path. Retail now gets past that call and stops at
the title's own `int3` at `exe+0x43ad94e` because `ApuCreateHeap` (acphal.dll)
is an auto-stub returning `E_NOTIMPL`.

## The call site

`exe+0x43aea20` is the title's audio-device init. Static map
(`leonida-work/exe-disasm/full.asm`):

- `0x1443aea4d` `lea rcx,[rip+0x2245f5c]` → `0x1465f49b0`
  `bcde0395-e52f-467c-8e3d-c4579291692e` = `CLSID_MMDeviceEnumerator`.
- `0x1443aea54` sets `r9 = 0x1465f49a0`
  `a95664d2-9614-4f35-a746-de8db63617e6` = `IID_IMMDeviceEnumerator`;
  `0x1443aea5b` `xor edx,edx` (`pUnkOuter`), `0x1443aea5d` `mov r8d,0x17`
  (`CLSCTX_ALL`), and `0x1443aea65` stores `rsi+0x3ce28` (`ppv`) at
  `[rsp+0x20]`.
- `0x1443aea6a` calls the import thunk `[0x146793dc8]`; the stub database
  recorded the caller as `exe+0x43aea70` (the return address).
- `0x1443aea70 test eax,eax; js 0x1443aec24` is the failure path; on success
  `0x1443aea78` loads `[rsi+0x3ce28]` and `0x1443aea86` calls
  `[rax+0x30]` (fault `0x0` while the out slot was NULL).

`CoInitializeEx` (thunk `0x146793dd0`, caller `exe+0x43ad90a`) is the same
auto-stub family and is called by the enclosing init.

## Fix

`hle/kernelx_linux.rs` gains a small COM core, routed through the existing
`api-ms-win-core-*` → kernelx route:

- `CoInitializeEx`: per-thread apartment. `S_OK` on the first call,
  `S_FALSE` for a repeat in the same mode, `RPC_E_CHANGED_MODE` for a
  different mode, `E_INVALIDARG` for bits outside
  `COINIT_APARTMENTTHREADED | DISABLE_OLE1DDE | SPEED_OVER_MEMORY` (`0xE`).
- `CoUninitialize`: drops one init from the calling thread (no-op below 0).
- `CoCreateInstance`: clears `*ppv` on every failure, `E_INVALIDARG` for a
  NULL `ppv`/`rclsid`/`riid`, otherwise `REGDB_E_CLASSNOTAVAILABLE`
  (`0x80040154`) and one log line per distinct CLSID. This is the single
  seam if XCL ever registers a COM class.

No fake object: the title's `js` path handles the failure. The fake-ok
family here was the generic auto-stub (`stubs_linux.rs:102-106`), not an
acphal/d3d12 stub.

## Tests

Three tests resolve the names through `exports()` so they compile on the
pre-fix tree (the RED is a clean assertion, not a compile error):

- `com_entry_points_are_wired` — RED on `35d8df83`: "CoInitializeEx must be
  wired" (0 passed / 3 failed with the two behaviour tests).
- `com_activation_fails_closed_and_clears_the_out_pointer` — the retail
  CLSID/IID pair returns `REGDB_E_CLASSNOTAVAILABLE` and clears a poisoned
  out pointer; NULL `ppv`/`rclsid`/`riid` are `E_INVALIDARG`.
- `com_apartments_follow_the_windows_init_contract` — on a fresh thread:
  `S_OK`, `S_FALSE`, `RPC_E_CHANGED_MODE`, balanced `CoUninitialize`, then a
  fresh `S_OK`.

RED re-derived in a throwaway worktree of `35d8df83` with only the test
block grafted: 0 passed / 3 failed. GREEN at the fixed tree: 3/3; full lib
**896 passed / 0 failed / 1 ignored**; launcher **7/0**; both checks 0.

## Retail

`/tmp/xcl-step56-r1` (fresh root, debug binary):

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 6.9 s — first frame
  unchanged.
- `COM: no class is registered on Linux; CoCreateInstance CLSID
  [95, 03, de, bc, 2f, e5, 7c, 46, 8e, 3d, c4, 57, 92, 91, 69, 2e] →
  REGDB_E_CLASSNOTAVAILABLE`.
- report exit **133** (SIGTRAP) at `exe+0x43ad94f`; shell exit 139.
  `CoInitializeEx`/`CoCreateInstance` are gone from the called stubs; the
  log adds `acphal.dll!ApuCreateHeap first called from
  exe+0x43ad938 → returning 0x80004001`.
- gdb sample `/tmp/xcl-step56-g2`: `0x1443ad929` returns `0x1c7`;
  `0x1443ad92c` `shl eax,0x14`; `0x1443ad933` calls the `ApuCreateHeap`
  thunk with `size = 0x1c700000`, `flags = 0x226e00`; the stub returns
  `0x80004001`; `0x1443ad93a js 0x1443ad94e` reaches the title's `int3`
  (RIP reported past it at `0x1443ad94f`).

## Next frontier

`exe+0x43ad94e`: the audio init asserts because `ApuCreateHeap` failed.
The console APU memory slice (`ApuCreateHeap`, `ApuAlloc`, `ApuFree`,
`ApuMapVirtualAddress`, `ApuMapApuAddress`, `ApuIsVirtualAddressValid`,
`ApuHeapGetState`) is implemented in the Windows `hle/acphal.rs`; on Linux it
has no twin, so the names are stub metadata. That is Step 57.
