# Retail M1 — Step 61: the post-audio fault is the routed msvcp140.dll

Date: 2026-11-28. Step 61. Base: `linux` @ `af79ea3a` (Step 60 docs).
Task `code-assist:xcl-first-frame:step-61:post-audio-table`.

## Sample

Retail `/tmp/xcl-step60-r1` presents the first frame and then SIGSEGVs at
`exe+0x1b332ea` (`call [rdx+8]`, fault `0x0`, 40 stubs). The crash is on
the thread that logs the `XblInitialize` event right after
`libhttpclient.gdk.dll!HCInitialize` returns `E_NOTIMPL`.

gdb `/tmp/xcl-step61-g1` (entry breakpoint at `0x141b332b0`, the logger
call):

- `rcx = 0x7ff2bcff6ff8` (a stack object), `rdx = 0x14678dab0`
  (`"XblInitialize"`). `[rcx] = 0x145f42e00`, whose dword `+4` is `136`;
  the callee therefore treats `rcx + 0x88` as an object field offset and
  loads the slot at `rcx + 0x88 + 0x48` (i.e. `rcx + 0xd0`).
- That slot is `0x1441f6c20`, the **return address of the
  `call 0x1455dd794` at `0x1441f6c1b`** — stale stack content, not an
  object. `[0x1441f6c20]` is code (`0x20c483482078c085`), so
  `call [[rax]+8]` faults.
- The log immediately before the crash is the title thread's
  `msvcp140.dll!??0?$basic_ios…`, `??0?$basic_iostream…`,
  `??0?$basic_streambuf…`, `_Mtx_init_in_situ`, `_Mtx_lock`,
  `_Mtx_unlock` calls — all auto-stubs returning `0`. XCL routes
  `msvcp140.dll` to the UCRT HLE module (`hle_linux.rs` route list), which
  does not implement the MSVC C++ runtime; the constructor stubs return
  `S_OK` without initializing the objects, leaving the logger's embedded
  C++ object with stale stack content.
- Zeroing the slot at the entry breakpoint (`/tmp/xcl-step61-g2`) moves
  the crash to `exe+0x55ea74d` (`mov [rax],rcx`, fault 0), still inside
  the same `0x1455e…` logging subsystem: the object stays uninitialized.

## Why the logger is broken

The title ships a real `msvcp140.dll` (plus `vcruntime140`, `concrt140`,
`vccorlib140`) and imports the C++ runtime from it. XCL deliberately
routes those DLLs to `crt_linux` to avoid the bundled DLLs; that makes
every C++ runtime export a stub. `crt_linux` implements the C CRT
primitives and operator new/delete, but not the MSVC C++ ABI (iostreams,
`std::mutex`, locales). Reimplementing that ABI by hand is not tractable;
the shipped DLL is the implementation.

Loading the real DLLs is close: removing `msvcp140.dll` from the UCRT
route (`/tmp/xcl-step61-exp2`) loads the bundled DLL, whose imports then
reach the CRT HLE. Its `DllMain` faults at `msvcp140.dll+0x2efd`: it calls
`__pctype_func` and copies 0x200 bytes from the returned pointer, but the
stub returns NULL. The same DllMain imports `_lock_locales`,
`_unlock_locales`, `setlocale`, `___lc_codepage_func`,
`_get_stream_buffer_pointers`, `InitializeCriticalSectionEx` and
`EncodePointer`; all are stubs today.

## Verdict

XCL-side. The crash is not a title uninitialized-read that happens to be
harmless on a console: the title's C++ runtime objects are stubs because
XCL intercepts `msvcp140.dll`. The faithful fix is to load the bundled
real CRT DLLs (as Windows does) and implement the CRT primitives their
DllMain imports. Step 62 starts that slice; no product code changed in
Step 61.

## Next frontier

Step 62: unroute `msvcp140.dll`, load the shipped DLL, and implement
`__pctype_func` (256-entry `u16` C-locale ctype table),
`___lc_codepage_func`, `_lock_locales`/`_unlock_locales`, `setlocale`,
`InitializeCriticalSectionEx`, `EncodePointer`/`DecodePointer`, and
`_get_stream_buffer_pointers` when the next fault names it. Tests pin the
contracts; retail must pass the logger call without the stale-slot crash.
