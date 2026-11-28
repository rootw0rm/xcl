# Retail Winsock name resolution (Step 85)

Date: 2026-11-28. Base: `44b8aef2` (Step 84). Task key:
`code-assist:xcl-first-frame:step-85:ws2-name-resolution`.

The title's LAN probe asked Winsock for its own node name, resolved it and
freed the list. Both calls were auto-stubs: `gethostname` returned 0 with an
empty buffer and `getaddrinfo` returned 0 with `*res` untouched, so the probe
saw a null list and every later address-dependent path stayed dead. The shim
now answers both through libc, translates the result into the Windows
`ADDRINFOA` layout the title reads, and refuses names it cannot honestly
resolve.

## Call-site map

`game_scarlett_master.exe+0x41d3170` (the probe; the function runs at
8.06–8.09 s in the retail log):

| site | call | evidence |
|---|---|---|
| `+0x41d3170` | entry | zeroes 0x100 bytes at `rsp+0x50`, `edx = 0x100` |
| `+0x41d31d6` | `[IAT 0x146793cf0]` | ordinal 57 (`gethostname(buf, 0x100)`); a non-zero return jumps to the failure log at `+0x41d329b` |
| `+0x41d31e4` | build hints | `r8 = rsp+0x150`; `[r8] = 0x2_0000_0000` → `ai_family = AF_INET` (2), flags 0 |
| `+0x41d321d` | `[IAT 0x146793ce0]` | `getaddrinfo(node, NULL, hints, &res@rsp+0x48)` |
| `+0x41d322b` | consume | loads `*res`; if non-null reads `[rax+0x20]` = `ai_addr` |
| `+0x41d3240` | `0x1419f1a00` | checks `sockaddr.sa_family == 2`, then `0x1441d3460` (`movbe [rcx], edx`) copies the IPv4 dword at `sockaddr+4` |
| `+0x41d3284` | `[IAT 0x146793cd8]` | `freeaddrinfo(*res)` when the list is non-null |

`0x1419f1a00` is the consumer: family must be 2 (AF_INET) and the address is
read from `sockaddr+4`. That fixes the layout the shim must produce.

## Ordinal 57 is `gethostname`

`objdump -p game_scarlett_master.exe`, WS2_32 import table:

```
06793cd8  <none>  00a5  freeaddrinfo
06793ce0  <none>  00a6  getaddrinfo
06793ce8     52  <none> <none>
06793cf0     57  <none> <none>
```

`0x146793cf0 = image base + 0x6793cf0`; ordinal 57 is `gethostname` in the
Winsock ordinal table (`ordlookup/ws2_32.py`), and the call site passes
`(buffer, 0x100)`. Ordinal 111 (`WSAGetLastError`, IAT `0x146793ca0`) is
imported too but only read on the failure branches; it stays a stub and was
not called in this run.

## Contract

New `crates/xcl/src/hle/ws2_linux.rs`, routed from `hle_linux.rs` as
`ws2_32.dll` (module name = route key), ordinals 57 and 111.

| export | behaviour |
|---|---|
| `gethostname(name, len)` | real host node name via `hle::net_names::hostname`; NUL-terminated, truncated to `len-1`, never writes past `len`; 0 on success. Null/`len <= 0` → `SOCKET_ERROR` with `WSAEFAULT` (10014), nothing written |
| `getaddrinfo` | the local-name policy (`net_names::local_name`) plus numeric literals (`IpAddr::parse`) go through libc with the caller's family/socktype/protocol and the common `AI_*` bits; everything else → `WSAHOST_NOT_FOUND` (11001), `*res = null` when non-null and no other write |
| `GetAddrInfoW` | UTF-16 → the same path (`ADDRINFOW` has the `ADDRINFOA` layout) |
| `GetAddrInfoExW` | synchronous form of the same path; extra blob/provider fields stay zero |
| `freeaddrinfo` | frees exactly a list this module handed out (registry keyed by head); null/foreign/already-freed is a no-op |
| `WSAGetLastError` / `WSASetLastError` | per-thread Winsock error (separate from `GetLastError`) |

`hle::net_names` holds the one local-name policy (`""`, `localhost`,
`127.*`, `::1`, this machine's name) used by both the Windows
`XCL_NETWORK=0` route (`hle::netoff`) and this Linux route. `netoff`'s
`orig_getaddrinfo[_w]` are Windows hook trampolines and are not callable on
Linux; the shared piece is the policy, not the trampoline.

libc's list is translated before it is handed out: on this host
`ai_addr`/`ai_canonname` are swapped relative to `ADDRINFOA` and `ai_addrlen`
is 32-bit. Each Windows node is a zeroed 128-byte header plus a copied
`sockaddr`; `AF_INET6` is renumbered 10 → 23, `ai_addrlen` is 16/28, and
`ai_canonname` is null. The title's two consumed fields (`ai_addr` at 0x20,
`ai_next` at 0x28) are therefore correct.

No socket is created, connected or written. `WSAStartup` (#115) keeps the
contained-success stub, and the socket ordinals stay stubs. The same
`gethostname` implementation is exported by name so a `GetProcAddress`-style
lookup cannot land on a logging stub.

## Tests

RED first, test grafted on the parent tree: the registry test failed
`gethostname must be wired` (**0/1**); GREEN at the fix **1/0**.

- `hle::ws2_linux::tests::gethostname_fills_and_truncates_without_overrun` —
  non-empty NUL-terminated name; `len = 2` writes one byte plus the NUL and
  leaves bytes beyond untouched; null/zero length is `-1` + `WSAEFAULT` with
  no write.
- `getaddrinfo_resolves_localhost_into_the_windows_layout` — `localhost`
  with `ai_family = AF_INET` yields family 2, `ai_addrlen` 16, a loopback
  `sockaddr` at offset 0x20, walkable `ai_next`; `freeaddrinfo` twice and on
  null is safe.
- `getaddrinfo_refuses_unknown_names_and_writes_only_the_result_slot` —
  `no-such-host.invalid` is 11001 with a null result slot and sentinels on
  both sides untouched; `GetAddrInfoW` mirrors both answers.
- `numeric_names_resolve_without_a_lookup` — `127.0.0.1` resolves to
  127/8 without a DNS lookup.
- `hle::tests::ws2_name_resolution_is_wired` — every export is real code in
  the `ws2_32.dll` module, and ordinals 57/111 name `gethostname` /
  `WSAGetLastError`.

Gates: `cargo check -p xcl --tests` 0, `cargo check --workspace` 0, lib
**965 passed / 0 failed / 1 ignored** (`--test-threads=4`), `launcher_e2e`
**7/0**.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step85-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step85-r1`, debug build, no gdb):

| | Step 84 | Step 85 |
|---|---|---|
| first present | 6.871 s (token 0x1) | 7.701 s (token 0x1) |
| presents | 148 (through 0x94, 12.455 s) | **562** (through 0x232, 27.038 s) |
| exit | 133 SIGTRAP `+0x2219651` | **133 SIGTRAP `+0x2219651`** (pc `0x142219651`) |
| called stubs | 25 names | **23 names** |
| `hle:ws2_32.dll` imports | 0 | **3** (`gethostname`, `getaddrinfo`, `freeaddrinfo`) |

- `ws2_32.dll!getaddrinfo` and `ws2_32.dll!#57` left `called_stubs` (2 calls
  each in Step 84) and `stubbed_imports`; the set difference between the two
  called-stub lists is exactly those two names.
- The warning diff against Step 84 is exactly the two `[STUB]` lines for
  `getaddrinfo`/`#57`; no new warning. The `#23`/`#21`/`#2`/`#6`/`#3`
  socket-probe calls (`socket`, `setsockopt`, `bind`, `getsockname`,
  `closesocket`) and `#115` are unchanged from Step 84 and still stubs.
- The probe now receives a real name and a non-null list: the consumer at
  `+0x41d3240` reads `ai_addr`, sees family 2, copies the IPv4 dword, and
  `+0x41d3284` calls `freeaddrinfo` on the translated list. No fault at
  `0x1419f1a00`, `0x1441d3460` or in the shim; the crash is the same parked
  pool-OOM trap at `+0x2219651`.
- Present counts/run length vary with the pool-OOM timing (Step 81: 144,
  Step 82: 601, Step 83: 265, Step 84: 148); no causality claim.
