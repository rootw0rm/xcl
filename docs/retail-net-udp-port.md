# Retail Step 89 — the XNetworking UDP port, asynchronous on Linux

Date: 2026-11-28. Base: Step 88 (`824a4de3`). Task key:
`code-assist:xcl-first-frame:step-89:networking-port-async`.

The title's networking init asks for the multiplayer UDP port through the
`XNetworking` class's async pair. The sync query and both async/result
implementations already existed, but the Linux class list carried only the
sync/registration rows, so slot 4 (`...Async`) was an unimplemented stub that
answered `E_NOTIMPL`.

## Input (Step-88 artifact)

`/tmp/xcl-step88-r1/GTA6.Xbox.v1.0.2.1-CYBERLEEK/logs/report.json`:

```json
"unimplemented": {
  "XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync": 1,
  "XSpeechSynthesizerCreate": 1
}
```

`xcl.log` had the matching `XGameRuntime: XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync
– not supported (E_NOTIMPL)` line after `QueryApiImpl 37e56907-2f10-41e8-b72f-36edb185331a
→ XNetworking: XCL`.

## Static chain (title)

The `XNetworking` clsid sits in `.rdata` at `0x145f41170` (file
`0x5f3ff70`, `37e56907-2f10-41e8-b72f-36edb185331a`), its iid at
`0x145f41180`. The GDK thunk wrappers next to it call the class vtable:

- `0x1455e0aa8` — `XNetworkingQueryPreferredLocalUdpMultiplayerPort`
  (slot 3, `[rax+0x18]`), one out pointer.
- `0x1455e0b1c` — `...Async` (slot 4, `[rax+0x20]`), one arg (the
  `XAsyncBlock`).
- `0x1455e0b8c` — `...AsyncResult` (slot 5, `[rax+0x28]`), `rcx` =
  block, `rdx` = 2-byte out.

Each wrapper calls the GDK `QueryApiImpl` entry `0x1455de048`, then the
vtable slot through `0x1456a2070`, and `Release` (`+0x10`) on both paths.

Call sites:

- `0x1441f11ac` (in `0x1441f1150`) calls the `...Async` wrapper with the
  block at `[rdi+0x160]` and only inspects the begin HRESULT; a failure
  raises the title's error path (`0x1402675a0`, codes `0xd3915e`/`0x5833`).
- `0x1441f10b7` (in `0x1441f10a0`) calls the `...Result` wrapper with the
  block at `[rcx+0x20]` and a 2-byte stack slot, then stores that value to
  `[rsi+0x68]` and `[rsi+0x70]`.

## Change

`crates/xcl/src/hle/xtaskqueue_linux.rs`, next to the sync query:

- `XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync` — `begin_with`
  on the title's Linux XAsync engine (`hle/xuser_linux.rs`), fresh identity,
  payload = `DEFAULT_UDP_PORT` (3074) little-endian, 2 bytes.
- `XNetworkingQueryPreferredLocalUdpMultiplayerPortAsyncResult` — null
  `port` is `E_INVALIDARG` with nothing written or consumed; otherwise
  writes 0 first and asks `XAsyncGetResult` for the 2-byte payload.

Both are in `xtaskqueue_linux::exports()` (the `hle_exports!` name list in
`hle_linux.rs` already had them) and in `xgameruntime_api_linux::xnetworking()`,
so the class `layout()` puts them at GDK slots 4 and 5. This mirrors the
Windows `hle/xgameruntime/networking.rs:94/98`; the shared Windows module is
untouched. The result is one-shot on the XAsync engine: a second
`...Result` is `E_ILLEGAL_METHOD_CALL`, and a null-out rejection does not
consume the payload.

## Tests

- RED (parent tree): `networking_port_async_and_result_through_the_class_vtable`
  through `QueryApiImpl(XNETWORKING, bf2346b2-…)` → the slot is the
  unimplemented stub, 0 passed / 1 failed at `begin`:
  `E_NOTIMPL` (`0x80004001`, -2147467263), log line
  `XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync – not supported`.
- GREEN 1/0: async begin `S_OK`, status after wait `S_OK`, result writes
  `DEFAULT_UDP_PORT`; the second result is `E_ILLEGAL_METHOD_CALL`; a null
  result pointer is `E_INVALIDARG` and the payload stays consumable.
- `hle::xtaskqueue_linux` 20/0 (sync port, connect registration, queue
  semantics unchanged); `hle::xgameruntime_api_linux` 29/0; `cargo check
  -p xcl --tests` and `cargo check --workspace` 0; full lib **996 passed
  / 0 failed / 1 ignored** (`--test-threads=4`); launcher_e2e 7/0.

## Retail delta (one fresh-root run)

`XCL_DATA_ROOT=/tmp/xcl-step89-r1 ./target/debug/xcl run
/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK`, debug
build, no extra env:

- first present 6.805 s (`token 0x1`), **137** presents, last `token
  0x89` at 12.058 s; exit **133 SIGTRAP
  `game_scarlett_master.exe+0x2219651`** (pc 0x142219651, fault 0) — the
  same parked trap as Steps 83–88.
- `xgameruntime.unimplemented` loses the networking row and keeps only
  `XSpeechSynthesizerCreate: 1`; the log has no `XNetworking… not
  supported` line, and the warning-set difference versus Step 88 is
  exactly that line (plus pointer/signal addresses).
- `called_stubs` is the same 19-entry set as Step 88 (diff empty),
  `stubbed_imports` stays 329, shaders unchanged 106 submitted / 106
  rejected / 0 unique, five rootsig warnings. No frame or OOM causality
  is claimed for 137 vs 128 presents.
