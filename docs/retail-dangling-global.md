# Retail M1 — Step 63: the XClosed query, and the teardown that leaves a dangling host

Date: 2026-11-28. Step 63. Base: `linux` @ `eddc9d16` (Step 62 docs).
Task `code-assist:xcl-first-frame:step-63:dangling-global`.

## Result in one paragraph

The startup `QueryApiImpl(3e241977… / d722b373…)` no longer answers
"missing": XCL now serves the GDK's **XClosed** family (closed captions /
accessibility), so the title's `XErrorReport 0x89240107` ("missing required
dependencies") is gone. The remaining SIGSEGV at `exe+0x502b2f1` is not
that error path: it is the title's own global teardown at ~7 s. The teardown
destroys the runtime host (cache `0x1498c1040` cleared) but leaves the
separate raw global `0x1498c0f20` pointing at the destroyed object; a
global initializer then dispatches through `0x14502b2e0` and reads the
zeroed vtable.

## Fix

`hle/xgameruntime_api_linux.rs` gains the `XClosed` class
(`3e241977-6237-41e9-8559-102c2d9983f1`, the GDK table's accessibility
family) with the Windows twin's real semantics:

- `XClosedCaptionGetProperties(&p)` — the 28-byte
  `XClosedCaptionProperties`, C-locale defaults (`background` black, `font`
  white, `font_scale` 1.0), the current enabled flag.
- `XClosedCaptionSetEnabled(1/0)` — stores the flag; `S_OK`.
- `XHighContrastGetMode(&mode)` — `0` (`XHIGH_CONTRAST_MODE_OFF`): the
  host has no high-contrast scheme, so OFF is the real answer.

The rest of the XClosed class (speech) stays E_NOTIMPL stubs. Object is
built by the existing GDK table layout, so slot 3/4/5 match the PC table.

## Tests (RED first)

`xclosed_accessibility_query_answers_with_real_captions`: query the class
and the `d722b373…` interface, expect `S_OK` and `"XClosed: XCL"` in
`ApiStats`; GetProperties returns the defaults, SetEnabled(1) flips
`enabled`, HighContrast returns OFF, null out pointers are `E_INVALIDARG`,
and the struct is 28 bytes.

RED (the class entry removed): **0 passed / 1 failed** (the query answers
`REGDB_E_CLASSNOTREG`). GREEN after; full gates: checks 0, lib **911
passed / 0 failed / 1 ignored**, launcher **7/0**.

## Retail

`/tmp/xcl-step63-r1` (fresh root): first frame presented at 6.9 s; the log
now has `QueryApiImpl 3e241977… → XClosed: XCL` and **no**
`XErrorReport 0x89240107`. The crash is unchanged:
`exe+0x502b2f1` (`mov rax,[rax+0x40]`, fault `0x40`), report exit 139.

gdb (`/tmp/xcl-step63-g1`…`-g3`):

- The host singleton (`0x1419db790`, cache `0x1498c1040`, constructor
  `0x145800410`, vtable `0x14654cf30`) is created and stored at
  `0x1498c0f20`; all eight subsystem inits of `0x143da1a80` return 1.
- The global **teardown** `0x14220f3b0` (registered by the `lea` at
  `0x1458a2409`) calls `0x143da2880` → host shutdown `0x143da2fb0` on the
  same thread. The shutdown calls the host's vtable methods in reverse,
  then `0x143dadd00` clears `0x1498c1040` and frees the host; the separate
  global `0x1498c0f20` is **not** cleared.
- A hardware watch on the host's first qword shows the block later reused
  by a vector insertion (`0x1455aa52a`), and the dispatcher
  `0x14502b2e0` (called from the global initializer `0x142208ca2`) loads
  the stale host with vtable 0.

## Next frontier

Why does the title's global teardown run at ~7 s, immediately after the
first frame? Step 64 samples the caller of `0x14220f3b0` (the `lea` at
`0x1458a2409` registers it) and the message/window loop that returns; if
XCL's headless window/message or an HLE return makes `main` exit early,
that is the fix. The missing clear of `0x1498c0f20` is title-side; no
allocator quarantine or fake lifetime extension.
