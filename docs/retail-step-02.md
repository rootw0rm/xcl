# M1 step-02 — honest negative: no HLE-actionable slice on the traced abort path

Date: 2026-11-27. Tip: `linux` @ `7f7026ee` (step-01 record commit).
Depends on: `docs/retail-frontier.md` (step-01). Task key:
`code-assist:launcher-first-frame:step-02:next-slice`.

## Bounded re-derivation (one pass, fresh data root)

```bash
rm -rf ~/.local/share/XCL/GTA6.Xbox.v1.0.2.1-CYBERLEEK
timeout 120 ./target/debug/xcl run \
  "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
```

Triage (`~/.local/share/XCL/GTA6.Xbox.v1.0.2.1-CYBERLEEK/logs/`):

- Exit **139 (SIGSEGV)** — `report.json` `exit_code: 139`.
- Crash: `SIGSEGV (signal 11) at game_scarlett_master.exe+0x2237320;
  pc 0x142237320; fault address 0x0: unmapped
  (code bytes: 48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b)`.
  Instruction `mov rax, qword ptr [rbx]`, **rbx NULL** — identical to step-01.
- `called_stubs`: **20**, identical list and counts to step-01
  (`atan2f` 180, `cosf` 99, `sinf` 90, `tanf` 20,
  `GetThreadGroupAffinity` 16, `powf` 6, `exp2f` 5, `log10f` 5, …,
  `ws2_32 #115` x1, `GameInput IGameInput::#9` x1 → `0x80004001`,
  `CreateMutexA` x2, `OpenThread` x1, `SetProcessPriorityBoost` x1).
  All benign early-boot; zero storage/package/DLC/XVD imports.
- `xgameruntime.apis`: exactly **XAsync, XNetworking, XUser** (all XCL);
  `unimplemented: {}`.
- `vfs.unmapped_paths`: [] ; no DLC/package content requested.
- `grep -ci d3d12/DXGI/PresentX xcl.log` = **0** — still no console-slot
  calls on the abort path (step-146 `bb3b1edc` stands: PresentX #30 and
  device #78–81 routing stays out).
- `LoadLibrary(dbghelp.dll)` / `LoadLibrary(rpcrt.dll)` → NULL (game
  continues past both, as in step-01).

**Frontier HOLDS one-to-one at the new tip.**

## One-level-deeper trace (new this step, with disassembly refs)

Exe disassembly: `/run/media/anon/12TB_B/leonida-work/exe-disasm/full.asm`
(base `0x140000000`; `.pdata` function bounds from `functions.json`).

- Fault `0x2237320` is in `0x22372c0…0x2237478`. Its NULL `rbx` comes
  from the slot written by the game-code vtable call
  `call qword ptr [rax+0x8]` at `0x2237313` (rcx=rdi factory product,
  rdx=&slot, r8=0, r9=0).
- The factory `0x140fc21c0` (pdata `0xfc1160…0xfc1232`) takes the
  alloc-init path on this run (r8 = 0 skips the `RPF9`-magic branch at
  `0xfc21d7`); init is `0x140fc2240` (pdata `0xfc2240…0xfc2490`), which
  sets the product vtable to `0x1462fd890`.
- The `[rax+8]` producer resolves (vtable base `0x1462fd890` read from
  the exe via section mapping, slot +8) to **`0x140fc2490`**
  (pdata `0xfc2490…0xfc252e`):
  - `0xfc2490` allocates (`0x4d9c160`), inits via `0x140fc2540`, then
    gates on **`call qword ptr [rax+0x3a0]`** (`0xfc24ec`): nonzero →
    writes the record, zero → writes **NULL** (`0xfc2518`) and releases.
  - The `[rax+0x3a0]` predicate resolves (vtable `0x1462fd480` + `0x3a0`)
    to **`0x140fc2530`**: `return [rcx+0x30] != 0`. So the NULL is the
    game's own validity gate on field `+0x30` of the `0xfc2540`-inited
    object — not an HLE return code.
- `[rsi+0x30]` is assigned in `0xfc2240`/`0xfc2540` from the `[rax+0x108]`
  / `[rax+0xf8]` vtable calls on the `0x140968480`-built `[rsi+0x10]`
  object (the rbx-non-NULL `0xfc23d0` branch is not taken: r8 = 0).
  `0x140968480` is game code (hot-patch `jmp 0x140968490` into a
  `:`-prefixed-name dispatcher: xxhash-style compares `0xb37535d9`,
  `0x46b0fb26`, `0x5dccb711`, tables at `0x1478f5eb0`/`0x1478f6880`,
  keys such as `globalroot`, `_root`, `faderPreservationRoot`,
  `tcpip:`, `t:/polys:/verts:` from `.rdata`).
- The **only import call** in the `0xfc2240` / `0xfc2540` / `0x968490`
  bodies is `call qword ptr [rip+0x5e2c2ff]` at `0x14096853b` → IAT
  slot `0x146794840` = **`strncmp`
  (`api-ms-win-crt-string-l1-1-0.dll`)**, resolved via the exe import
  directory. `strncmp` is **HLE-covered** (ucrtbase) and never appears in
  `called_stubs` as a failure; every other call on these bodies is a
  direct game-internal call (`0x165fa80`, `0x55db994`, `0xa57f70`,
  `0x1fb2600`, `0x1fb2780`, `0x8ec200`, `0x87aec0`, `0x28ce230`,
  `0x1bd1f0`, `0x56eb30`, `0x79e350`, `0x28cd1a0`, `0x4d9c160`,
  `0x643cd0` — all `push`/`sub` function bodies, no `jmp [rip]` thunks).
- The `0x14678d1bb`-region wide strings consumed by `0x56eb30` decode
  (`.rdata`, UTF-16) to minidump-upload format pieces
  (`D:\LocalDumps`, `D:/LocalDumps`, `%s%s`, `%s\%s`,
  `upload_file_minidump…`): crash-telemetry formatting, not a content
  load with a game-code consumer XCL can satisfy. The `0x1474539xx`
  `lea rdx` pointers sit in `.data` beyond its raw size (zero-init at
  file level) — runtime-built tables, not static content gaps.

## Verdict

**Honest negative.** No stub on the traced path (the single reached
import, `strncmp`, is HLE-covered); no newly observed HLE-actionable
writer (the NULL writer is the game-internal predicate `0xfc2530` on
game-built state); no content gap with a named game-code consumer
(`vfs.unmapped_paths` empty, zero storage/XVD imports, surrounding
strings are telemetry + internal dispatch keys). Prior TERMINAL bisects
on this NULL-manager-record path stay closed — this pass adds the
producer→predicate→dispatch chain as the documented reason, not a new
variant. PresentX #30 / device #78–81 routing stays out per step-146
(0 d3d12 hits re-confirmed). No upstream port is in service of a
non-slice (notes not trawled, no sync-log commit).

Pivot (for the next loop, not this step): the missing value is created
inside game-built tables (`[rsi+0x30]` writer is the data-dependent
`[rax+0x108]`/`[rax+0xf8]` target). The cheapest next evidence would be
runtime observation — e.g. a temporary HLE `strncmp` argument log or a
gdb watchpoint on the `[rsi+0x30]` writer — which needs its own task and
a reversibility note; grinding further static hops without runtime
values is out of bounds by the terminal-discipline rule.

## Gates (doc-only change: no code, no failing-first test owed)

- `timeout 300 cargo check -p xcl --tests` → ok.
- `timeout 300 cargo check --workspace` → ok.
- `timeout 400 cargo test -p xcl --lib -- --test-threads=4` →
  **787 passed, 0 failed, 1 ignored** (matches step-01 exactly).
- Retail run (this step, fresh root): exit 139 at `exe+0x2237320`,
  no-regress vs `docs/retail-frontier.md`.
- Title e2e: not re-run (change touches no boot/HLE/loader/VFS/GPU
  path); owned by step-03 regression-guard on a fresh root.
- Deadlock shape (`if let` + `.lock()` guard temporary): N/A — no code.
