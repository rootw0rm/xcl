# Retail M1 — Step 38: real CRT pack + XDisplay; frontier reaches D3D12

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-38:crt-stubs-xdisplay`.
Base: `linux` @ `b3941914` (Step 37). Product code + tests.

## Result in one paragraph

The Step-37 pack-read fix left the boot in the title's language/config init:
exit **133 SIGTRAP** at `exe+0x3751e65` (the `int3` at `0x3751e64`) with
**9 called stubs**, seven of them CRT imports and `XDisplay: missing` in
the GDK log. Making those seven real
(`isspace`, `atoi`, `_stricmp`, `srand`+`rand`, `qsort`,
`GetUserDefaultLocaleName`, `__stdio_common_vsscanf`) and answering the
XDisplay capability query (`03f0fe74…/35f07670…`) moves the title past the
language init and into window/device creation: **exit 139 SIGSEGV at
`exe+0x2478731`**, with **4 called stubs** and `XDisplay: XCL` (the
`XErrorReport 0x89240107` is gone). The D3D12.X fallback (null) device is
created at `0.174 s`; the new fault is a real XCL vtable bug on the D3D12
device, identified below.

## What changed

`crates/xcl/src/hle/crt_linux.rs` (C-locale UCRT semantics, no locale
backend, every export carries a real code address):

| Export | Contract now | Test |
|---|---|---|
| `isspace` | nonzero for `0x09`–`0x0D` and `0x20` only | `classify_parse_sort_and_rng_contracts_are_live` |
| `atoi` | leading space, optional sign, decimal digits, clamp to `i32` | same |
| `_stricmp` | ASCII-folded unbounded compare | same |
| `srand`/`rand` | MSVC LCG (`state*214013+2531011`, `(state>>16)&0x7FFF`); seed 1 → 41 | same |
| `qsort` | in-place heapsort, whole-element guest swaps, `O(n log n)`, no allocation, no panic across FFI | same |
| `__stdio_common_vsscanf` | `%d %i %u %o %x %X %p`, `%f/%e/%g/%a` (`l` = double), `%c %s %[ ] %n`, length/width/suppression, wide `%lc/%ls`, `count` bound, EOF on first-field input failure | `vsscanf_assigns_fields_through_the_va_list` |

`crates/xcl/src/hle/kernelx_linux.rs`: `GetUserDefaultLocaleName` returns
`L"en-US"` + NUL (6) and `ERROR_INSUFFICIENT_BUFFER`/0 for a short or null
buffer, next to `GetUserDefaultGeoName` (`user_default_locale_name_fills_en_us_and_reports_the_null`).

`crates/xcl/src/hle/xgameruntime_api_linux.rs`: the GDK-table class
`03f0fe74-fdd9-4e5c-b630-f9339c47acc5` / `35f07670-706e-4bfb-9476-090798c5ebf3`
now answers `QueryApiImpl` with an XCL object whose slot 4 is
`XDisplayTryEnableHdrMode`; it returns `S_OK` with
`XDisplayHdrModeInfo{result=Disabled, luminances=0}` (XCL's output is SDR) and
`E_INVALIDARG` for a null out-parameter
(`xdisplay_hdr_query_answers_with_sdr_capability`).

Failing-first: with the new export-table rows commented out the two CRT tests
failed at their wiring asserts (`isspace must be wired`,
`__stdio_common_vsscanf` absent from `exports()`); with the XDisplay class
removed the QueryApiImpl test failed (`REGDB_E_CLASSNOTREG`); restored trees
are green.

## Retail movement

| Run | Tree | Exit | Fault | Called stubs |
|---|---|---|---|---|
| r0 | `b3941914` | 133 | `int3` `exe+0x3751e64` | 9 |
| r1 | + CRT pack | 139 | SIGSEGV `exe+0x2478731`, `0x1000000d4` | 4 |
| r2 | + XDisplay | 139 | same | 4 |

r2 called stubs: `LocaleNameToLCID` ×2 (`exe+0x22175b9`), `ShowWindow` ×1
(`exe+0x2478559`), `ws2_32#115` ×1, `IGameInput::#9` ×1. GDK log:
`XDisplay: XCL`, `unimplemented={}`. r1/r2 fresh roots, real title folder.

`LocaleNameToLCID` and `ShowWindow` surfaced because the language init now
proceeds; both are honest logging stubs. `LocaleNameToLCID` returning 0 makes
the caller at `0x1422175bf` return early (it does not feed the fault);
`ShowWindow`'s BOOL is unused here.

## The new fault is XCL's D3D12 `GetCustomHeapProperties`

`D3D12XboxCreateDevice` (IAT `0x146793c18`, thunk `0x145f3fd80`) is called at
`0x1424785b7`; XCL creates the fallback null device and returns `S_OK` (log
`D3D12.X: creating a FALLBACK DEVICE`). Immediately after, the title calls
the device's vtable slot `+0xd0` three times (`0x14247870d`, `…731`, `…758`):

```
rcx = &D3D12_HEAP_PROPERTIES (stack sret buffer)
rdx = device
r8d = 1 (NodeMask), r9d = 1..3 (D3D12_HEAP_TYPE)
call qword ptr [[rdx]+0xd0]        ; GetCustomHeapProperties returns 20 bytes by sret
```

XCL's `Device_GetCustomHeapProperties(_t: *mut NullObj, ret: *mut u8, _m: u32, ty: u32)`
assumes a hidden-`this` signature: it treats `rcx` as the object and `rdx` as
the out pointer. The guest actually passes `rcx` = sret buffer and `rdx` =
object, so XCL **zeroes and writes the 20-byte heap-properties struct over the
NullObj itself**: gdb shows the device's first qword becoming
`0x00000001_00000004` (`D3D12_HEAP_TYPE_CUSTOM` 4, CPU-page property 1,
memory pool 1), refcount/kind clobbered with it. The first
`+0xd0` call corrupts the object; the second (`r9d=2`) reloads the same
pointer and faults reading the vtable word at `0x1000000d4`. GDB: after
`D3D12XboxCreateDevice` returns, the device's vtbl is a valid vtable; at the
fault `rdx` is that device and `[rdx] = 0x0000000100000004`.

Fix (next step, not this change): declare the slot as
`(ret: *mut u8, _this: *mut NullObj, node_mask: u32, ty: u32)` so the sret
pointer is the first Win64 argument and the device is `rdx`; add a test that
calls slot 26 with those four registers and asserts the object header is
untouched. The other sret-returning D3D12 methods on the same path need the
same audit (`GetResourceAllocationInfo` returns 32 bytes by sret on slot 25).

## Gates

At the final tree: `cargo check -p xcl --tests` exit 0, `cargo check
--workspace` exit 0, `cargo test -p xcl --lib -- --test-threads=4`
**875 passed / 0 failed / 1 ignored**, `cargo test -p xcl --test
launcher_e2e` **7 passed / 0 failed**. Retail r2 as above (no GPU test
skipped; Vulkan device present).
