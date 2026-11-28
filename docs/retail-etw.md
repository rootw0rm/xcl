# Retail Step 87 — ETW no-op provider, the carried locale constant, and the pool-fill record

Date: 2026-11-28. Base: Step 86 (`6154a8f4`). Task key:
`code-assist:xcl-first-frame:step-87:etw-locale-record`.

## Input (Step-86 artifact)

`/tmp/xcl-step86-r1/.../logs/report.json` records one `advapi32.dll`
provider registration from the shipped `libHttpClient.GDK.dll`:

| import | calls | first caller | what the auto-stub did |
|---|---|---|---|
| `EventRegister` | 1 | `libhttpclient.gdk.dll+0x44401` | returned 0, left the `REGHANDLE` at `0x1800de078` unwritten |
| `EventSetInformation` | 1 | `libhttpclient.gdk.dll+0x44422` | returned 0, discarded the traits |
| `EventWriteTransfer` | 0 | — | imported, never called in the sample |
| `EventUnregister` | 0 | — | imported, never called in the sample |

The report's `first_args` are r8/r9: for `EventRegister` they are
`0x7ff31d3e8058` (callback context `0x1800de058`) and `0x7ff31d3e8078`
(the out handle `0x1800de078`); for `EventSetInformation` they are the
traits pointer and its length `0x10`. Both calls looked successful to the
caller, but the provider was never registered: the handle stayed zero and
the traits call received it.

## Static chain (`libHttpClient.GDK.dll`)

`objdump -p` pins the ADVAPI32 imports:

```
000e9000  EventRegister
000e9008  EventSetInformation
000e9010  EventWriteTransfer
000e9038  EventUnregister
```

The registration site is `0x1800443c6`–`0x180044422`:

```
1800443c6  mov  rax, [0x1800de060]      ; traits blob pointer (0x1800c36d9)
1800443cd  lea  r9,  [0x1800de078]      ; PREGHANDLE out
1800443d7  lea  r8,  [0x1800de058]      ; callback context
1800443de  lea  rdx, [0x180005830]      ; PENABLECALLBACK
1800443e5  lea  rcx, [rsp+0x20]         ; LPCGUID (copied from rax-0x10)
1800443ea  movups xmm0, [rax-0x10]
1800443f6  movups [rsp+0x20], xmm0
1800443fb  call [0x1800e9000]           ; EventRegister
180044401  test eax, eax
180044403  jne  0x180044386             ; failure path
180044405  mov  r8,  [0x1800de060]      ; traits
18004440c  mov  edx, 2                  ; EventProviderSetTraits
180044411  mov  rcx, [0x1800de078]      ; the handle EventRegister must write
180044418  movzx r9d, word [r8]         ; trait length = 0x10
18004441c  call [0x1800e9008]           ; EventSetInformation
```

The provider GUID at `0x1800c36c8` is
`{6274B204-9F2B-6F15-4EB8-000D17009E14}`; the 16-byte traits blob at
`0x1800c36d9` carries the `libHttpClient` provider name.

`EventWriteTransfer` has two static call sites: `0x18000e2c0` (function
`0x18000e220`) and `0x180043e5e` (three user-data descriptors). Neither
ran in the sample. The cleanup helper `0x1800a8210` zeroes the callback
context and handle globals at `0x1800de058`/`0x1800de078` and jumps to
`EventUnregister`; it did not run either. A real provider that answers
"registered" but leaves the handle zero is exactly the silent failure
mode this step removes.

## Contract implemented

`hle/kernelx_linux.rs` exports all four names (advapi32 already routes to
the kernelx module, so no route change was needed):

- `EventRegister(provider, enable_callback, callback_context, &handle)`:
  a null provider GUID or null out pointer returns `ERROR_INVALID_PARAMETER`
  (87) and writes nothing; otherwise it stores a non-null opaque cookie
  (`0x5843_4C00_0000_0000 | generation`) and returns 0.
- `EventSetInformation(handle, class, info, len)` and
  `EventWriteTransfer(handle, descriptor, activity, related, count,
  data)`: a live cookie returns 0 and the data is dropped; a null,
  foreign, or already-unregistered handle returns
  `ERROR_INVALID_HANDLE` (6).
- `EventUnregister(handle)`: removes exactly that cookie and returns 0;
  null/foreign/already-unregistered is `ERROR_INVALID_HANDLE`.

Cookies are tracked in a process-wide live set, so an address that looks
like a handle cannot be used after unregistration. The enable callback is
never invoked: XCL has no ETW consumer and no session can enable the
provider, which is what a no-op provider means. ETW events are
diagnostics, so dropping them is truthful; returning success with an
unwritten handle was not.

## Carried locale constant

`LocaleNameToLCID` now accepts `LOCALE_ALLOW_NEUTRAL_NAMES` as
`0x0800_0000` (WinNls.h `558`; mingw and the Windows SDK agree), not
`0x80`. `0x80` is not a valid flag for this API — `LOCALE_NOUSEROVERRIDE`
is `0x8000_0000` — so the plan/task parenthetical naming it as
`LOCALE_NOUSEROVERRIDE` is wrong. Neutral `en` returns `0x0009` only with
`0x0800_0000`; `0x80` alone still returns 0 with
`ERROR_INVALID_PARAMETER`. Retail passes `dwFlags = 0`, so this changes no
frame behavior; `docs/retail-lcid.md` was corrected with the code.

## Pool-fill record

`docs/retail-pool-fill.md` now records the pool-size provenance and the
corrected 500 MiB consumer:

- `0x141e1ced0` copies a static nine-qword row selected by
  `ConsoleMode` (`0x146186250` for Series S, `0x1461862a0` otherwise, stride
  `0x50`); copied entry 2 (raw row qword 3) is `0x4fd00000` / `0x89700000`,
  and the `+disableSharedPools` adjust never touches it.
- the consumer at `0x14022cb16` is the BVH prebuild (`0x1456abf80` →
  `0x1456ac100`, IAT `0x146793f98`) → alloc (`0x1456abf90` →
  `0x140075230` → `jmp 0x140075440`) → build (`0x1456abfb0`) loop, not
  `GetResourceAllocationInfo`; the other live caller `0x145c0c469` is the
  same BVH prebuild path.
- reopen triggers 1–2 (allocator/size provenance, alloc/free trace) are
  marked spent by Steps 67/69; the only open input is the console/Windows
  first-frame sample.

## Tests

RED: `hle::tests::etw_provider_is_wired` failed on the parent
(`EventRegister must be wired`, 0 passed / 1 failed / 967 filtered).

GREEN:

- `etw_provider_is_a_tracked_no_op`: cookie non-null, distinct per
  registration, accepted by the info/write calls, invalidated by
  unregister; null/foreign/already-unregistered handles fail with 6, null
  GUID/out with 87 and no write.
- `locale_name_to_lcid_maps_the_fixed_table`: `0x80` alone → 0 with 87,
  `0x0800_0000` → `0x0009`.
- `hle::tests::etw_provider_is_wired` → 2/0 for the ETW pair.

Gates:

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **968 passed / 0 failed /
  1 ignored** (966 before + 2 net new).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Retail evidence

ONE plain fresh-root run, debug binary at HEAD, no gdb:
`/tmp/xcl-step87-r1`.

- first frame `presented 3840x2160` at **6.892 s**
- **208** presents (tokens `0x1`–`0xd0`) through **14.428 s**
- exit **133** SIGTRAP at `game_scarlett_master.exe+0x2219651`
  (`pc 0x142219651`, fault 0) — the same parked title pool OOM
- `called_stubs` 21 → **19**; the set difference is exactly
  `advapi32.dll!EventRegister` and `advapi32.dll!EventSetInformation`
- `stubbed_imports` 333 → **329**: all four Event names leave the stub
  list (two were called, two are only imported)
- no `EventRegister`/`EventSetInformation`/`EventWriteTransfer`/
  `EventUnregister` line remains in `xcl.log`, and no new warning appears

The other per-call stub counts scale down with the shorter run (208 vs
340 presents); there is no causality claim between the ETW shim and the
exit, the pc, or the run length. Retail still reaches the title's own
fixed-pool OOM, which is title-side and documented in
`docs/retail-pool-fill.md`.
