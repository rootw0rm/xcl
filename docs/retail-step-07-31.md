# M1 steps 07–31 — packcache:/packroot: NULL-record chain (honest-negative series)

Date: 2026-11-27. Tip probed: `linux` @ `1597e296` (all gdb runs).
Current tip at landing: `041e1c7b` (two upstream merges since; re-verify below).
Depends on: `docs/retail-frontier.md` (step-01), `docs/retail-step-02.md`,
`docs/retail-step-06.md`. No code changed in any step 07–31 (read-only gdb).

## Frontier (unchanged throughout)

Every probed + control run: exit 139,
`SIGSEGV at game_scarlett_master.exe+0x2237320`
(`mov rax,[rbx]`, rbx NULL, fault 0x0, bytes `48 8b 03 …`),
`called_stubs` 20, xgr exactly XAsync+XNetworking+XUser,
`vfs.unmapped_paths` [], 0 d3d12/DXGI/PresentX. HOLDS.

## Chain (each step bounded, same run_main-hook soft-breakpoint recipe)

Base `0x140000000`. `hbreak` unsupported in sandbox; breakpoints planted
after XCL maps the game (break on `run_main`, then `break *addr`).

- 07: none of six `0x140fc2540` callees writes inner `[+0x30]`
  (`0x165fa80`, `0x1fb2600` al=1, `0x1fb2780`, `0x8ec200` al=0→early-exit,
  `0x87aec0`-direct/`0x28ce230` never execute on packroot: path).
- 08: first failing gate of `0x1408ec200` is gate 1 `0x140a34890` ret 0
  (slot-5 zeros); gates 2–5 never execute.
- 09: comparator `0x140613880` never compares — NULL slot short-circuits;
  table `0x1479f7060` slot 5 all zeros; no writer (only reader `0x140a349a0`).
- 10: caller is `0x140fc2664` only; kind byte `[blob+0x2e]=0xb`→5 (default);
  blob filler one level up; no HLE on-chain.
- 11: blob filler is `0x141fb2600` (`RPF9` file bytes via two `[rdi+0x60]`
  virtual reads); no HLE.
- 12: reader `0x7ff3981aa380` vtable `0x1462fc810`,
  `[+0x60]=0x141330cc0` trampoline; bytes from mapped tables; no VFS touch.
- 13: FINAL `0x140cdd930` → `0x140313fc0`; no HLE/VFS touch.
- 14: slot target `0x140314200` (game code); `reader[+0x10]` not an index.
- 15: `0x140314200` returns byte count `0x1000`; loop calls HLE ReadFile
  (`[0x146793970]`, `kernelx_linux.rs:4442`) + GetLastError/Sleep/memcpy —
  FIRST HLE touch. Backing verified: appdata0.rpf file offset 0x1000–0x2000.
  But 0x350 record does NOT feed NULL rbx (negative link).
- 16: ReadFile args `h=0x101ba/0x1013b, buf=0x1479fb000, n=0x1000, out=0x1000`;
  copy to dst verified; consumer `0x2237320` faults with rbx=0 already,
  `[rsp]=0` (frame slot, not fiber). Negative link confirmed.
- 17: dispatcher `CALL 0x142236f29→0x1422372c0`; const `0x1474539d5`
  = `"packcache:/"` (+`appdata0.rpf` at +0x20); rsi vector all-zero head;
  NULL from `mov rbx,[rbx]` at `0x2237316` after `call *0x8(%rax)` leaves
  `[stackobj]=0` for `packcache:/`+`0x2f` input.
- 18: callee at `*0x2237313` is `0x140fc2490` (slot +0x8 of
  `0x1462fd890`); `0x140fc21c0` SUCCEEDS (record nonzero in);
  `0x140fc2490` zeroes the slot itself (NULL-record YES).
- 19–20: NULL store at `0x140fc2518` fires (predicate `0x140fc2530`
  ret 0, `[inner+0x30]` nil); `0x140fc2507` never fires. Packcache: path
  gate2 `0x1408ec200` al=0 → `0x140fc2695` early-exit; skip-gate never fires.
- 21: inside `0x1408ec200`, failing sub-gate is again `0x140a34890`
  (first call) — SAME shared sub-gate kills packroot: and packcache:.
- 22: inside `0x140a34890`: flag `0xb` passes, `cl==2` full-chain,
  `0x1406bd160` FAIL on zeroed pool → `0x140536a70` table[0]=0 (.rdata)
  → `0x140613880` nil via `0x140193f90(edx=0)` (`edx-0x10>5`→null).
- 23: zero-word writer honest-negative — pool `0x1479f7060` all-zero on
  entry (image-reservation gap, fixed rva `0x79f7060`); `0x141fb2310` is a
  TLS-flag reader, not a builder; `table[0]=0` baked in .rdata by design.
- 24: pool origin is registers (`0x140a349a0` rax→rsi→rdx);
  TLS callbacks YES both run (`loader_linux.rs:326,332,555-566`,
  `tls_linux.rs:429-464`; cb `{0x1455dd4f4,0x1455e2ebc}` reason 1 pre-entry
  + reason 2 per thread). Loader exonerated.
- 25: `0x140a349a0(cl=5)`→default pool `0x1479f7060` zeroed; all six pool
  globals 0; XCL-fixable NO (pre-populating = fake-success).
- 26: propagates YES — zeroed pool → `0x1406bd160` FAIL ret 0 on crashing
  boot (chk1 pins `0x140536a70` as first failure).
- 27: return-0 consumer chain `a348a0→8ec200→fc2540 NULL-store [rsi+8]=0`;
  `fc2540` returns non-NULL rsi (only +8 zeroed) — one hop unproven.
- 28: hop closed via caller `0x140fc24e1`: predicate reads
  `[rcx+0x30]=0` → `2518` NULL branch; rbx still outer at 2518; fault rbx
  reloaded from stack out-param left NULL.
- 29: intermediate is `0x140fc2490` itself (direct `[rax+8]` target);
  `2518` overwrites caller slot; no IAT/HLE.
- 30: packcache: re-probe of all eight `0x140fc2540` callees — none writes
  `[inner+0x30]`; gate `8ec200` al=0 short-circuits `87aec0`/`28ce230`.
- 31: inside `8ec200`: `a34890`→`6bd160`→`536a70`→`613880`→`193f90`
  nil; `28ce230` never reached; `table[0]=0` + zeroed global = runtime
  packcache-registration state never populated. Honest negative, no HLE.

## Pivot (registration filler — honest negative)

No XCL-side filler survives the consumer bar: no `packroot:` mount
(`config.rs:157-163`, `vfs.rs:566,717-724` silent Reject, hence empty
`unmapped_paths`); no runtime RPF/packcache handler (`rpf.rs` offline CLI,
`gpu::pack::scan` shader-only); save/PLS + DLC mounts serve host paths,
never URI tables (`xgameruntime_api_linux.rs:862-921`). Only consumer is
game-side (`0x53525C0` via `0x643C10`, `crt_linux.rs:1114-1141`). d3d12x
irrelevant (0 hits, null device).

## Verdict

Full NULL-record chain mapped from `packcache:/appdata0.rpf` to
`exe+0x2237320` across 25 probes. No HLE slice anywhere except the
verified-correct ReadFile backing (appdata0.rpf 0x1000–0x2000) that does
NOT feed the fault. Do not re-probe this chain without new evidence.
Next cheapest evidence (if pursued): who should fill `0x1479f7060` /
`[rsi+18/20]` before `2540` — the table `0x53525C0` consults for the
`packroot:` prefix. No XCL change owed until it names an XCL input.

## Gates

- Lib/title e2e green at step-06 slice (789/0/1; 12 GPU-only FAILs,
  ExitProcess 0xc). No code changed in 07–31 → no new gates owed;
  re-verify at current tip before building on this record.
- Raw probe logs: `/tmp/xcl-step{07,12b/c,13,18a/b,19c,20a,22*,23b,24b/c,27,30b}-gdb.out`
  (ephemeral; this doc is the tracked record).
