# M1 steps 32–38 — mount registrar + pool filler (honest-negative series)

Date: 2026-11-27. Tip probed: `linux` @ `c2088262`–`60c51a70`
(all gdb/static runs). Landed at: `7a06393d` (frame-clock 1b).
Depends on: `docs/retail-step-07-31.md` (steps 07–31 chain).
No code changed in any step 32–38 (gdb probes + static reads only).

## Frontier (unchanged throughout)

Exit 139, `SIGSEGV at game_scarlett_master.exe+0x2237320`
(`mov rax,[rbx]`, rbx NULL, fault 0x0), `called_stubs` 20,
xgr exactly XAsync+XNetworking+XUser, `vfs.unmapped_paths` [], 0 d3d12.
HOLDS on every probed + control run.

## Step-32: mount-table consult succeeds (registrar healthy)

Table: mount registry `count=[0x1478f5ec4]`, entries `[0x1478f5ec8]`,
0x20-byte records with name inline. Consulted by `0x1453525C0`
(via `0x140643C10+0x1a` ← `0x14056f0fc`): rejects `dlc`-prefix via
`_strnicmp`, takes `strcspn(path,":")` as prefix len, `_strnicmp` per record.
Live (`packroot:/appdata0.rpf` ×4): `count=1`, `entry[0]="packroot:/"`,
`strcspn`→8, return `dil=1` every time. Missing entry: none.
IAT/HLE: `_strnicmp` + `strcspn` only, both HLE-covered
(`resolver_linux.rs:559-561`, `crt_linux.rs:787-815,1121-1141`).
Filler honest-negative: registration ran in-game before consult;
no XCL-side filler owed. NULL chain NOT caused by mount miss.

## Step-33: registrar xref (dead end for the fault)

Only count-writer: `0x1412c5792` (`mov [rip],ecx → 0x1478f5ec4`)
in registrar `0x1412c5670–0x1412c58fe`. Live: `oldcount 0 → newcount 1`
(the step-32 `count=1`); `entry[0]` filled via `0x140c1b970` copy.
Entry: `path='packroot:/'`, `r8d=1`, caller `0x141fb2210`-family
(same family as step-11 `0x141fb2600`). Ordering: registrar #1 →
writer → registrar #2 → first pool-consumer `0x140a34890`; pool qwords 0
at all three points. No shared init with pool filler. Zero indirect calls
in registrar; HLE feed NO. Registrar healthy (`dil=1`) — dead end.

## Steps 34–35: no pool writer in-image; filler runs nil

Static (`full.asm` 1.07 GB objdump): ZERO direct writers to pool six
`...060/078/07c/094/098/0b0` (rg for rip-stores → exit 1; all 16 xrefs are
reads/lea/cmp inside `0x140a349a0`, L2810948–20). `[rsi+0x20]`≡0 by
construction (`0x140fc2540` zeroes it; `0x141fb2600` never stores +0x8).
Live single-bp `*0x145ebdad0`: HITS (ret `0x140a34b06`, `rcx=pool060`)
but `pool060` still nil post-call — filler invoked with nil arg, no fill.
Runtime-fill DISPROVEN for this path.

## Steps 36–37: nil is hardcoded; filler zero-fills

`0x140a349a0` slow path builds the call explicitly: `lea rcx,[pool060]`
(`af0`), **`xor edx,edx`** (`af7`), `xor r8d,r8d` (`af9`) →
`call 0x145ebdad0` (`b01`). Gen check `eax=[0x1479f7078]` vs
`[tlsslot+4]` with uninit TLS garbage forces the slow path; post-filler
re-runs switch → `cl=5` → default `lea rax,[pool060]` (zeroed).
Filler body: 8 insns, straight-line, NO nil early-exit
(full.asm:26566041–48): `mov [rcx],edx` (0), `mov [rcx+4],r8w` (0),
`mov rax,rcx`, `vmovups [rcx+6],xmm0(zero)` then overwrite `[rcx+6]`
from `[r9]` — nil-path `r9` is xmm-zeroed (`:2811013–18`), so net zero.
First site (`:2809995`) passes REAL template `0x1462fd470` +
`edx=0x10000001` (`:2809991–94`) — never runs on our boot.

## Step-38: first site gated behind cl==4 + counters

Host `0x140a349a0–0x140a34b19`: entry guard
(`cmp eax,[rdx+4]; jg slow`), then `cmp cl,4; je …a45` →
`cmp cl,1; jne …ab8(nil-fallthrough)`. First site is one leaf behind
`cl==4` + `[0x1479f70b0]>[tlsslot+4]` + post-lookup `== -1`.
Our boot: `cl=5` stable (flag `0xb>>1`, caller garbage in high bytes) →
default/nil fallthrough always. Wrapper `0x140a348a0` pre-gate
(`0x140b85020; test al; je return-1`) passes; `cl=(byte[rcx+8+0x2e])>>1`.
All direct game→game calls, no import thunk. Our `cl≠4` (or unprimed
counters) keeps the real-template leaf cold. Volume kind comes from file
bytes (`RPF9` blob `[0x2e]=0xb`), not an XCL input.

## Verdict

Mount + registrar + loader/TLS + ReadFile backing all healthy/exonerated.
Pool `0x1479f7060` zeroed by design on the `cl=5` path; filler runs but
writes zeros; first-site template gated behind `cl==4` this boot never
takes. No HLE slice in 32–38 (or 07–31). Do not re-probe registration
without new evidence (a boot with `cl==4`, a pool writer, or an XCL-fed
input on the chain — none observed).

## Next (code front, not probes)

Frame-clock twin 1a/1b landed (`60c51a70`, `7a06393d`; lib 792/0/1;
retail HOLDS). Next: 1c clock thread + `wait_frame_event`, then split 2
PresentX routing — the M1.4 head for when the frontier advances past
game-side registration.

## Gates

No code changed in 32–38 → no new gates owed. Last full gates at 1b:
both checks EXIT 0, lib 792/0/1, title delta 0, retail HOLDS.
Raw logs: `/tmp/xcl-step3{3,5,5b}-gdb.out` (ephemeral; this doc is the record).
