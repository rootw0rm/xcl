# Retail Step 88 — the XGameSave provider, wired on Linux

Date: 2026-11-28. Base: Step 87 (`270af598`). Task key:
`code-assist:xcl-first-frame:step-88:xgamesave-provider`.

## Input (Step-87 artifact)

`/tmp/xcl-step87-r1/.../logs/report.json` recorded the live M1.6 trigger:

```json
"unimplemented": {
  "XGameSaveInitializeProviderAsync": 1,
  "XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync": 1,
  "XSpeechSynthesizerCreate": 1
}
```

`xcl.log` at 12.458 s had `XGameRuntime: XGameSaveInitializeProviderAsync –
not supported (E_NOTIMPL)` immediately after
`QueryApiImpl 704c3f58-e629-4cc2-b197-30511b996fe2/1bfff3af-f14a-40a3-8e35-9ada906593f9
→ XGameSave: XCL`. The three `XGameSaveFiles*` rows were the only members of
`xgamesave()`, so the 27 provider rows (GDK table slots 3..=29) were
unimplemented stubs and the title abandoned its save path.

## Static chain (title)

The two class identities sit next to the other GDK API GUIDs in `.rdata`:

```
0x145f41040  XAsync          073b7dcb-1fcf-4030-94be-e3c9eb623428
0x145f41050  XGameRuntimeFeature  8836fe87-edb9-4fe3-8dad-05f0d2cd5b40
0x145f41060  XGameEvent       bbfbdcc7-bfe7-409b-a5ca-edf054960b4d
0x145f41070  XGame            973a344e-24bf-4d0f-8457-56c534892b29
0x145f41080  XGame (iid)      2549f142-6419-4a06-97b5-931aab7c2f34
0x145f41090  XDisplay         1b339674-328d-4283-a200-3171f18d3639
0x145f410a0  XGameSave        704c3f58-e629-4cc2-b197-30511b996fe2
0x145f410b0  XGameSave (iid)  1bfff3af-f14a-40a3-8e35-9ada906593f9
0x145f410c0  XGameUi          dfcd4649-4ff8-4043-ba07-35d607df98b0
```

The title's thunk wrapper for the provider init is `0x1455df210`
(one of a run of same-shaped wrappers at `0x1455df210..0x1455df6fb`):

```
1455df231  mov  rdi, r9              ; async block
1455df234  mov  sil, r8b             ; syncOnDemand
1455df237  mov  rbp, rdx             ; configuration id
1455df23a  mov  r14, rcx             ; XUser handle
1455df242  lea  r8, [rax-0x18]       ; QueryApiImpl out
1455df246  lea  rdx, [0x145f410b0]   ; iid
1455df24d  lea  rcx, [0x145f410a0]   ; clsid
1455df254  call 0x1455de048          ; GDK QueryApiImpl entry
1455df25d  js   0x1455df291          ; class refused: return, no vtable call
1455df25f  mov  rcx, [rsp+0x30]      ; the XCL object
1455df267  mov  [rsp+0x20], rdi      ; async block = 5th argument
1455df26c  mov  r9b, sil             ; syncOnDemand
1455df26f  mov  r8, rbp              ; configuration id
1455df272  mov  rdx, r14             ; XUser handle
1455df275  mov  rax, [rax+0x20]      ; vtable slot 4
1455df279  call 0x1456a2070          ; InitializeProviderAsync
1455df288  mov  rax, [rax+0x10]      ; vtable slot 2 (Release)
```

`0x20/8 = 4` is the GDK table's `XGameSaveInitializeProviderAsync` row
(`XGameSaveInitializeProvider` would be slot 3 at `+0x18`). The wrapper
then Release()s the class object on both paths.

Its only caller is `0x1428d3ed2`, in the title's save-manager init
`0x1428d3e80`:

```
1428d3ec5  lea  rdx, [0x1479f7830]   ; configuration id (BSS string)
1428d3ecc  xor  r8d, r8d             ; syncOnDemand = false
1428d3ecf  mov  r9, rdi              ; 0x38-byte XAsyncBlock
1428d3ed2  call 0x1455df210
1428d3ed9  jns  0x1428d3ef7
1428d3edb  mov  dword [rsi+0xa0], 3  ; init failed: state = 3
1428d3eea  mov  edx, 0x38
1428d3ef2  call 0x144d9c130          ; free the async block
```

`rcx` is the user handle read from the save manager (`[rsi+0xa8]`); a
failure only changes the manager's state and frees the 56-byte
`XAsyncBlock` (queue/context/callback + 32-byte internal), so a wrong
class answer silently stops saves.

The configuration id is built once by `0x145b1c980` from the title id:
`sprintf(buf, "00000000-0000-0000-0000-0000%08x", 0x7952987a)` with the
format at `0x146778dd0` and a `0x26`-byte buffer. `0x7952987A` is the
`<TitleId>` in the shipped `MicrosoftGame.Config`, so the real save root
is `gamesave/00000000-0000-0000-0000-00007952987a`.

## Linux wiring

New `crates/xcl/src/hle/gamesave_linux.rs`:

- `#[path = "xgameruntime/gamesave.rs"] mod core;` — the real, already
  tested cross-platform provider (`<data>/gamesave/<configuration id>/`,
  atomic all-or-nothing updates, 256 MiB quota). Not copied.
- `pub mod xasync` re-exports the Linux engine
  (`hle/xuser_linux.rs`: `begin_with`, `XAsyncBlock`, `XAsyncGetStatus`,
  `XAsyncGetResultSize`, `XAsyncGetResult`); `pub mod taskqueue`
  re-exports `hle/xtaskqueue_linux.rs`. The provider's async work and the
  title's own `XAsyncGetStatus`/`GetResult` therefore share one engine.
- 27 thin `unsafe extern "win64"` wrappers over `core::*`, one per GDK
  row 3..=29, plus `exports()`.

The shared functions are `extern "system"` (win64 only on Windows); the
guest calls the class vtable with the Windows ABI, so the wrappers are
the actual export addresses. Enumeration callbacks are guest code too:
the wrappers take a `win64` function pointer and pass a one-call bridge
to the shared module's `system`-ABI callback type, forwarding the
caller's context and the callback's stop/continue bool.

`hle_linux.rs` overlays `gamesave_linux::exports()` in
`xgameruntime_exports()` (all 27 names were already in the export-name
list as `Stub(0)`, so the count stays 127) and the implemented-name test
pins six of them. `hle/xgameruntime_api_linux.rs::xgamesave()` adds the
27 wrappers by GDK name; slot lookup/`layout()` then places them at
slots 3..=29 of the `XGameSave` class.

## Corrections the Linux build forced

- The shared test module spelled `ASYNC_TEST_LOCK` through
  `crate::hle::xgameruntime::xasync::…`, a Windows-only path. It is now
  the bare name already in scope through `use super::*`; the Linux shim
  defines the static.
- `atomic_writes_preserve_xcltmp_blobs_and_skip_leftover_temps` called
  `apply(dir, None, &ops)` against the 4-argument `apply(dir, name,
  display, ops)` (three-argument form was stale). Compiling the shared
  tests on Linux caught it; the call now passes the container name.
- The store assumed NTFS case folding: `CreateContainer("slotmixed")`
  after `SlotMixed` aliased the same folder, and a blob write differing
  only in case replaced the same file. On a case-sensitive host the same
  calls produced two containers (two `XGameSaveGetContainerInfo` hits)
  and two blobs. `container_dir_name` and the new `blob_file` now reuse
  an existing folder/file that differs only in case; NTFS keeps its
  early path. `case_insensitive_blob_and_container_matching_on_ntfs`
  now passes on both hosts.

## Contract

- Provider root: `<XCL data>/gamesave/<enc(configuration id)>`; no cloud.
- Null provider/out pointers stay `E_INVALIDARG`; a failed async stays a
  failed status; `XGameSaveInitializeProviderResult` with a null out
  pointer returns `E_INVALIDARG` without consuming the one-shot payload
  (a valid retry still succeeds).
- Failed enumeration callback: `None` callback is `E_INVALIDARG`, a
  callback returning `false` stops the enumeration, `ctx` is the
  caller's.

## Tests

- RED (parent tree): the new class-level test
  `xgamesave_provider_async_and_blob_round_trip_through_the_class_vtable`
  fails 0/1 at the vtable call: `E_NOTIMPL` (`0x80004001`, -2147467263)
  from `unknown_slot`. GREEN 1/0 after wiring: async init completes on
  the Linux engine, Result writes the provider, quota is `QUOTA`
  (256 MiB, fresh configuration id), `CreateContainer` →
  `CreateUpdate` → `SubmitBlobWrite` → `SubmitUpdate` survives
  close/reopen through a second provider, `ReadBlobData` returns the
  bytes, and the null-out/retry contract holds.
- `gamesave_linux::tests` 2/0: `exports()` is exactly the 27 GDK rows
  with distinct real addresses; a `win64` callback is bridged with its
  context and `false` stops after one container; the blob callback uses
  the same bridge.
- Shared provider tests now run on Linux:
  `cargo test -p xcl --lib hle::gamesave` 26/0, including the NTFS
  case-folding test.
- `hle::xgameruntime_api_linux` 28/0, `hle::tests` 8/0; `cargo check -p
  xcl --tests` and `cargo check --workspace` 0; full lib **995 passed
  / 0 failed / 1 ignored** (`--test-threads=4`); launcher_e2e 7/0.

## Retail delta (one fresh-root run)

`XCL_DATA_ROOT=/tmp/xcl-step88-r1 ./target/debug/xcl run
/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK`, debug
build, no extra env:

- first present 6.811 s (`token 0x1`), 128 presents, last
  `token 0x80` at 11.776 s; exit **133 SIGTRAP
  `game_scarlett_master.exe+0x2219651`** (pc 0x142219651, fault 0) —
  the same parked trap as Steps 83–87.
- `xgameruntime.unimplemented` loses `XGameSaveInitializeProviderAsync`
  and keeps `XNetworkingQueryPreferredLocalUdpMultiplayerPortAsync: 1`
  and `XSpeechSynthesizerCreate: 1`. The log has no `XGameSave… not
  supported` line and no new warning; the only warning-set change is
  that line's removal.
- `gamesave/00000000-0000-0000-0000-00007952987a/` exists in the run's
  data root and is empty — the async work opened the provider with the
  title's real configuration id. No quota log and no container were
  written in this 15 s window, so the title stopped after the init in
  the sampled period.
- `called_stubs` is the same 19-entry set but with lower counts (e.g.
  `PeekMessageA` 183 → 103, list `#126`/`#142`/`#75` 182 → 102) because
  this run presented 128 frames instead of 208; `stubbed_imports` stays
  329 (the title imports the provider family through the GDK thunk, not
  as static imports). No frame or OOM causality is claimed for the
  fewer presents.
- Shaders unchanged: 106 submitted / 106 rejected / 0 unique, five
  rootsig warnings (Step 90 territory).
