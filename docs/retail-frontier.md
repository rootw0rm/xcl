# Retail frontier record (M1 step-01, bounded re-derivation)

Date: 2026-11-27. Tip: `linux` @ `3958614a` (14 ahead of `origin/linux`).
Drive mounted: both
`/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/` and
`/run/media/anon/12TB_B/leonida-work/` present (mount check passed, retail
run NOT skipped).

## Run

```bash
timeout 120 ./target/debug/xcl run "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
```

Triage: `~/.local/share/XCL/GTA6.Xbox.v1.0.2.1-CYBERLEEK/logs/{xcl.log,report.json}`
(fresh data root — `~/.local/share/XCL/*` removed before the run).

- Exit: **139 (SIGSEGV, core dumped)** — `report.json` `exit_code: 139`.
- Crash: `SIGSEGV (signal 11) at game_scarlett_master.exe+0x2237320;
  pc 0x142237320; fault address 0x0: unmapped
  (code bytes: 48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b)`.
- Instruction: `mov rax, qword ptr [rbx]` with **rbx NULL**.

## Fault-function disassembly (base 0x140000000)

Exe disassembly: `/run/media/anon/12TB_B/leonida-work/exe-disasm/full.asm`
(llvm-objdump, manifest sha matches retail exe). Fault `0x2237320` sits in
the `.pdata` function `rva 0x22372c0 … end 0x2237478` (fault at +0x60):

```
1422372e6:  call  0x140fc21c0          # outer factory (r8 = 0 on this path)
1422372eb:  mov   rdi, [rdi-slot]      # factory result object
1422372f4:  mov   rax, [rdi]
142237313:  call  qword ptr [rax+0x8]  # (rcx=rdi, rdx=&slot, r8=0, r9=0) -> writes record ptr to slot
142237316:  mov   rbx, [slot]          # record ptr — NULL on this run
142237320:  mov   rax, [rbx]           # <-- FAULT (rbx NULL)
142237323:  mov   rcx, rbx
142237326:  call  qword ptr [rax+0x3b8]
14223732c:  mov   eax, [rsi+0xc]       # never reached; twin growable-array appends follow
```

- No IAT/import call on the fault path: the function issues only direct
  internal calls (factory `0xfc11c0`, alloc/free pair `0x4d9b140`/`0x4d9b130`,
  `0x5f3f560`) plus vtable calls (`[rax+8]`, `[rax+0x3b8]`, `[rax]`). So no
  single stubbed import is the next slice — the NULL record is produced by
  game code (`[rax+8]` wrote NULL into the slot), not by an HLE stub.
- Outer factory `0x140fc21c0` (pdata `0xfc1160…0xfc1232`) checks
  `dword ptr [rbx] == 0x52504639` ('RPF9' magic) when its third param is
  non-NULL; with r8 = 0 it takes the alloc-init path. Content-adjacent
  (pack parsing), but the faulting NULL comes from the *inner* `[rax+8]`
  call, one level down.
- Caller: pdata function `0x2236e00…0x22371bd` calls `0x22372c0` at
  `0x2236f29`, then continues into its own checks — i.e. the caller expects
  the callee to succeed.
- `grep -ci d3d12/DXGI/PresentX xcl.log` = **0**: still no console-slot
  calls on the abort path (step-146 `bb3b1edc` stands — PresentX #30 and
  device #78–81 routing still moves nothing).

## Stub / API surface (`report.json` + `xcl.log`)

- `called_stubs`: **20** (all benign/early-boot; top by calls: `atan2f` 180,
  `cosf` 99, `sinf` 90, `tanf` 20, `GetThreadGroupAffinity` 16; the rest are
  single-digit math/time/sync/locale calls plus `ws2_32 #115` x1,
  `GameInput IGameInput::#9` x1 → `0x80004001`, `CreateMutexA` x2,
  `OpenThread` x1, `SetProcessPriorityBoost` x1). Zero storage / package /
  DLC / XVD imports.
- `xgameruntime.apis`: exactly **XAsync, XNetworking, XUser** (all XCL);
  `unimplemented: {}`.
- `stubbed_imports`: 424 (static import inventory, unchanged in kind).
- `vfs.unmapped`: [] ; no DLC/package content requested before the fault.
- `LoadLibrary(dbghelp.dll)` / `LoadLibrary(rpcrt4.dll)` → NULL (not found);
  game continues past both.

## Verdict on the provisional frontier

**HOLDS.** `exe+0x2237320` / `mov rax,[rbx]` rbx-NULL / exit 139 /
`called_stubs` 20 / xgr exactly XAsync+XNetworking+XUser reproduces
one-to-one at tip `3958614a`. The fault-path disassembly corroborates the
standing MISSING-REGISTRATION read (a game-code `[rax+8]` producer hands a
NULL record to the `[rax+0x3b8]` consumer; no stub on the traced path), and
the step-89 H-DLC falsification (`99918e0a`) is not reopened: still zero
storage/package/DLC/XVD imports and an empty unmapped-VFS list. Prior
TERMINAL bisects stay closed — no new evidence (no stub on path, no newly
observed writer, no content gap with a named consumer).

Next (step-02): needs a *new* writer or consumer on this exact path with
disassembly refs, or another honest negative; PresentX/device routing stays
out per step-146.
