# Retail M1 — Step 39: D3D12 device hidden-result buffers; frontier reaches DXGI

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-39:d3d12-sret`.
Base: `linux` @ `b4d38ad8` (Step 38). Product code + test.

## Result in one paragraph

The Step-38 fault at `exe+0x2478731` was XCL writing a D3D12 struct over the
null device because two device methods take their result through a hidden
pointer. Both now accept the retail title's C-order `(ret, this, …)` and
windows-rs's `(this, ret, …)`, writing only 4-byte-aligned fields of the
caller's buffer and leaving the `NullObj` header untouched. The boot advances
past the three `GetCustomHeapProperties` calls, creates command queues/lists,
and faults later at **exit 139 SIGSEGV `exe+0x2481d37`, fault `0x0`**, where
it dereferences the adapter pointer that `IDXGIDevice::GetAdapter` (slot 7)
should have written. That is the next defect (Step 40): the null device's
`QueryInterface(IID_IDXGIDevice)` answers with the device itself, so slot 7
resolves to `Device_GetNodeCount`, and the title's adapter stays NULL.

## The bug and the fix

Retail calls the hidden-result methods the C way: `rcx` = result buffer,
`rdx` = device. Disassembly (`leonida-work/exe-disasm/full.asm`):

```
0x14247870d  lea rcx,[rsp+0xd0] ; rdx = [0x148759d00] device ; r8d=1 ; r9d=1
             call qword ptr [rax+0xd0]      ; GetCustomHeapProperties (20 B)
0x142478731  same with r9d=2
0x142478758  same with r9d=3
0x14034413f  lea rcx,[rsp+0x38] ; rdx = device ; r8d=1 ; r9d=1 ; [rsp+0x20]=desc
             call qword ptr [rax+0xc8]      ; GetResourceAllocationInfo (16 B)
0x140344182  same (retry path)
0x141a0dce9  lea rcx,[rsp+0x38] ; rdx = device ; r8d=1 ; r9d=1 ; [rsp+0x20]=desc
             call qword ptr [rax+0xc8]
```

windows-rs 0.62.2 (the test title) declares the same slots as
`(this, result__, …)`: `D3D12/mod.rs:9426-9438` and `:9617`, i.e. `this` in
`rcx`, result in `rdx`. So both orders are live in this repo's callers; the
fix mirrors the Windows twin's `this_and_ret` (`hle/d3d12x/null.rs:270-278`):

- `this_and_ret(a, b)` checks whether `a`/`b` points at one of XCL's vtables
  (the leaked tables are now a module-level `VTABLES`); the object is `this`,
  the other pointer is the result buffer.
- `Device_GetResourceAllocationInfo` (slot 25) and
  `Device_GetCustomHeapProperties` (slot 26) take `(a, b, mask, n/ty)` as
  `*mut c_void` and write through the resolved buffer with
  `write_unaligned` (the 20-byte struct's buffer is only 4-byte aligned;
  `D3D12_RESOURCE_ALLOCATION_INFO` is 16 bytes, the Step-38 doc's "32" was
  a miscount).
- Both return the result buffer in `rax`, as sret requires.

Test `hle::d3d12x_linux::tests::heap_properties_and_allocation_info_use_a_hidden_result_buffer`
drives slots 25 and 26 in both orders and asserts the returned buffer, the
struct contents (`CUSTOM`/page property/pool for 26; size/alignment for 25),
the device's first vtable word, and the object kind.

RED (fix temporarily reverted, `--test-threads=1`):

```
assertion `left == right` failed: slot 26 returned the wrong buffer
  left: 0x7f679840d9f0          (device)
 right: 0x7f679fbfdc7c          (caller buffer)
test result: FAILED. 0 passed; 1 failed
```

GREEN at HEAD: the same test passes; the retail fault moved (below).

## Audit of the other routed by-value / hidden returns

| slot | method | shape | action |
|---|---|---|---|
| 25 | `GetResourceAllocationInfo` | 16-byte struct, hidden pointer | fixed (auto-order) |
| 26 | `GetCustomHeapProperties` | 20-byte struct, hidden pointer | fixed (auto-order) |
| 43 | `GetAdapterLuid` | `LUID` (8 B) | windows-rs passes an explicit out pointer (second arg) and reads it back; retail has no reachable slot-43 call before this frontier. Unchanged; a native `rax`-return caller is not observed. |
| queue 18 / resource 10 / heap 8 | `GetDesc` | real out-parameter in the signature (`this, out`) | correct as-is (`Hidden_ZeroDesc16/40`, `Resource_GetDesc`) |
| DXGI adapter 8 / output 7 | `GetDesc` | real out-parameter | correct as-is |
| device 38 | `GetCopyableFootprints` | void + out-params | correct as-is |
| device 108 | `GetResourceAllocationInfo2` (Device8) | 16-byte struct | not routed: logging stub returns 0 without writing (honest, no corruption). Route with its own RED test when a title reaches it. |

## New frontier: null device must answer `IDXGIDevice`

Retail, fresh root (`XCL_DATA_ROOT=/tmp/xcl-step39-r1`):

```
crash: SIGSEGV at game_scarlett_master.exe+0x2481d37, fault 0x0
       code bytes 48 8b 01 48 83 ec 20 31 d2 ff 50 38
called_stubs: ID3D12GraphicsCommandList::#131 ×5 (0x2478bc5)
              ID3D12GraphicsCommandList::#132 ×5 (0x2478ba4)
              LocaleNameToLCID ×2, ShowWindow ×1, ws2_32#115 ×1, IGameInput::#9 ×1
xg.unimplemented {}, XDisplay: XCL
```

gdb (break after `run_main`, retail VA fixed at `0x140000000`):

```
0x142481cd5  call device->QueryInterface(0x1461ece88, &pp)
             riid = fa 77 ec 54 77 13 e6 44 8c 32 88 fd 5f 44 c8 4c  (IID_IDXGIDevice)
0x142481cf2  rcx=[rbp-0x10] = 0x7ff2783f7890  (XCL NullObj, kind Device, refs 3)
             vtbl[7] = Device_GetNodeCount   (info symbol)
0x142481d08  call [rax+0x38]  ; GetAdapter(out) -> rax = node count, out stays NULL
0x142481d37  mov rax,[rcx]    ; rcx = NULL adapter -> SIGSEGV
```

`QueryInterface` (`d3d12x_linux.rs:455`) only builds a `DxgiDevice` wrapper
for `Kind::VulkanDevice`; the null device answers every IID with itself
(same as the Windows twin). Step 40: extend the wrapper to the null device so
`GetAdapter` → `DxgiAdapter` → `EnumOutputs` is real, with a failing-first
test, and give the console command-list slots #131/#132 semantics once the
caller is identified (their args are `(0x700000,0,0,0xffffffffff00)` and
`(0xc3c0,0x80020000,0,0)`; returns ignored).

## Gates

At the final tree: `cargo check -p xcl --tests` exit 0, `cargo check
--workspace` exit 0, `cargo test -p xcl --lib -- --test-threads=4`
**876 passed / 0 failed / 1 ignored**, `cargo test -p xcl --test launcher_e2e`
**7 passed / 0 failed**. Retail as above (Vulkan device present, no skips).
