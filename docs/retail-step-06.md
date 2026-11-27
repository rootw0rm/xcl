# M1 step-06 — +0x30 writer probe on the default-table object

Date: 2026-11-27. Tip: `linux` @ `7ade09c6` (step-05 commit).
Depends on: `docs/retail-frontier.md` (step-01), `docs/retail-step-02.md`
(step-02), `docs/retail-step-05.md` (step-05). Task key:
`code-assist:launcher-first-frame:step-06:writer-probe`.

## What this slice is

Step-05's pivot asked for the writer behind field `+0x30`: why the
predicate `0x140fc2530` (`[rcx+0x30] != 0`) takes the NULL branch at
`0x140fc2518` for the runtime-built inputs `G:/appdata0.rpf` and
`packroot:/appdata0.rpf`. One bounded approach, per the task: a gdb
breakpoint/watch run on the `[rsi+0x30]` store in `0x140fc2240` /
`0x140fc2540` during a fresh-root retail run (no code change — the vtable
targets are game code, and the run below shows an HLE shim cannot see
them). Extending the `XCL_TRACE_STRNCMP` probe was the alternative; it
was not taken because the writer bodies call no HLE import (below).

## Method (gdb recipe — hardware breakpoints unavailable here)

- `hbreak` fails in this sandbox even on a trivial hello binary
  (`No hardware breakpoint support in the target` — debug registers
  unavailable), so software (`break *addr`) breakpoints were used.
- Soft breakpoints cannot plant before XCL maps the game: the image is
  `mmap(MAP_FIXED_NOREPLACE)`'d at `0x140000000` at runtime, so pre-run
  `break *0x140...` fails with `Cannot access memory`. Workaround: break
  on XCL's `run_main` symbol
  (`_RNvNtCsiSbGE6hMDjL_3xcl6loader8run_main` — fires after the game is
  mapped, before its entry runs), then plant the six game breakpoints
  from an embedded Python block (`set language c` first, else `*(long*)`
  casts fail with `No symbol 'long' in current context` in asm frames).
- Six probes: dispatcher call `0x140fc2398`, `+0x108` call site
  `0x140fc23e2`, `+0x30` XCHG `0x140fc23e8`, `+0xf8` call site
  `0x140fc2457`, predicate gate `0x140fc24ec`, NULL branch
  `0x140fc2518`. `handle SIGSEGV nostop pass` so XCL's own crash handler
  still writes `report.json`.

## Bounded runs (fresh roots throughout)

Probed run:

```bash
XCL_DATA_ROOT=/tmp/xcl-step06-gdb timeout 100 gdb -batch \
  -x /tmp/xcl-step06-py.cmds --args ./target/debug/xcl run \
  "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/"
```

Writer evidence, verbatim (one factory pass; the only `0x140fc2398`
dispatcher hit in this boot):

```
>> dispatcher call: input=0x7ff3a0190940 edx=0x1
0x7ff3a0190940: "packroot:/appdata0.rpf"
>> +0x108 call site: obj=0x7ff3a01aa380 vtable=0x1462fc810 target=0x14153c8f0
0x7fffffffa8a0: "packroot:/appdata0.rpf"
0x7ff3a01aa380: 0x00000001462fc810
>> +0x30 XCHG: outer=0x7ff3a01364a0 rax_ret=0x1dd7a1270e2bf4b old=(nil)
>> +0xf8 call site: obj=0x7ff3a01aa380 vtable=0x1462fc810 target=0x14149f080
0x7fffffffa8a0: "packroot:/appdata0.rpf"
>> predicate gate: inner=0x7ff3a0196800 vtable=0x1462fd480 plus30=(nil)
>> NULL branch taken
```

The run then dies at the known fault (SIGSEGV observed by gdb right
after the NULL-branch line) — the probed run holds the frontier shape.

Control run (`XCL_DATA_ROOT=/tmp/xcl-step06-ctrl`, no gdb, no env):
exit **139**, `SIGSEGV at game_scarlett_master.exe+0x2237320`
(`mov rax,[rbx]`, rbx NULL, fault `0x0`, bytes `48 8b 03 …`),
`called_stubs` **20**, xgr exactly XAsync+XNetworking+XUser (3 APIs),
`vfs.unmapped_paths` [], **0** d3d12/DXGI/PresentX hits. Frontier HOLDS.

## Reading (stated confidence, with disassembly refs)

- The factory-path dispatcher input in this boot is
  **`packroot:/appdata0.rpf`** — quoted verbatim from the heap pointer
  at `0x140fc2398` (`rcx`, `edx=0x1` incidental leftover). Only one
  `0x140fc2398` hit: the step-05 `G:/appdata0.rpf` probes flow through
  other dispatcher call sites (dump-path `exe+0x54F8A70`, argv scan),
  not this factory path — out of scope for this slice.
- The dispatcher-built object (`obj=0x7ff3a01aa380`, kept at
  `[rsi+0x10]`) carries runtime vtable **`0x1462fc810`** (obj[0] read
  live). Its slots resolve live, not statically: `+0x108` →
  **`0x14153c8f0`**, `+0xf8` → **`0x14149f080`**. Both take
  `(rcx=obj, rdx=path)` with the stack path buffer reading verbatim
  `"packroot:/appdata0.rpf"`.
- Both targets share one shape: call `0x140643c10(obj, stackbuf, path)`;
  if it returns the stack buf (identity = path already canonical),
  tail-call the sub-object's (`[rsi+8]`) same slot, else return 0
  (`xor eax,eax`). Observed `rax_ret=0x1dd7a1270e2bf4b` (nonzero,
  opaque cookie — not a heap/image/stack pointer) proves the identity
  branch was taken for `packroot:/appdata0.rpf` and the sub-call
  returned nonzero. Neither body calls any HLE import (only
  game-internal `0x4d9c130`-family + tail-calls) — an HLE shim probe
  cannot observe this writer, which is why the gdb path was taken.
- The writer **fired**: `xchg [rsi+0x30],rax` at `0x140fc23e8` stored
  nonzero into the **outer** product (`outer=0x7ff3a01364a0`, the
  `0x140fc2240` frame object whose vtable is set to `0x1462fd890` at
  `0x140fc226c`). The old value was 0 (the `mov [rsi+0x30],r15`/r15=0
  zero-init at `0x140fc2319`).
- But the predicate reads a **different object**: the gate at
  `0x140fc24ec` calls `[rax+0x3a0]` with `rdi` = the `0x140fc2490`
  0x80-byte alloc, whose vtable is `0x1462fd480` (confirmed live; slot
  `+0x3a0` → `0x140fc2530` confirmed statically from `.rdata`). Its
  `[inner+0x30]` is **0** (`plus30=(nil)`): zeroed by the `ymm` store
  at `0x140fc25a9` and never written on this path, so the NULL branch
  at `0x140fc2518` (`mov [slot],0`) is taken and the factory hands a
  NULL record down to the `0x2237320` fault.
- Static cross-check (exe section map): vtable bases `0x1462fd890`
  (outer, slot `+0x8` → `0x140fc2490` producer, `+0x108` →
  `0x140657c10`, `+0xf8` → `0x14043cc50`) and `0x1462fd480` (inner,
  `+0x3a0` → `0x140fc2530`) both live in `.rdata`; the dispatcher
  result tables (`0x1478f5e80` family) sit beyond `.data` raw size
  (runtime-built) — which is why the `0x1462fc810` vtable could only be
  resolved live. No `.lock()`/tracing-code concern: zero code changed.

## Verdict

**New evidence, no HLE slice (honest-negative for the slice).** The
`+0x30` that stays 0 is the **inner** record's (`0x1462fd480`-vtable
object from `0x140fc2540`), not the field the `[rax+0x108]`/`[rax+0xf8]`
writer stores to (outer `0x1462fd890`-vtable product — that store
succeeds with a nonzero cookie for `packroot:/appdata0.rpf`). No stub
is implicated anywhere on this chain (the only import reached remains
the HLE-covered `strncmp`/`_strnicmp` pair; both writer targets are
pure game code), `vfs.unmapped_paths` is empty, and 0 d3d12 hits
re-confirm that PresentX #30 / device #78–81 routing stays out. No
upstream port is owed.

Pivot (own task, not this step): name the **inner** `+0x30` writer —
i.e. which of the `0x140fc2540` callees (`0x165fa80`, `0x1fb2600`,
`0x1fb2780`, `0x8ec200`, `0x87aec0`, `0x28ce230`) should set
`[rsi+0x30]` for the `packroot:`/`G:` location, and what content it
reads. Cheapest next evidence: the same run_main-hook gdb recipe with
breakpoints on those six callees (or a watch on `[inner+0x30]` once its
address is known — hardware watchpoints are unavailable in this
sandbox, so use call-site breakpoints), not another boot-wide log.

## Gates

- `timeout 300 cargo check -p xcl --tests` → ok (pre-existing warnings only).
- `timeout 300 cargo check --workspace` → ok.
- `timeout 400 cargo test -p xcl --lib -- --test-threads=4` →
  **789 passed, 0 failed, 1 ignored** (unchanged vs step-05; no code
  touched, so no failing-first test owed).
- Title e2e (fresh root) → 4 passed + 1 failed by design;
  12 unique `[FAIL]`, ALL `D3D12.X` GPU-only, 0 non-GPU;
  `ExitProcess(0xc)` — baseline one-to-one, no regression.
- Retail probed (gdb, fresh root) + control (plain, fresh root) runs →
  frontier HOLDS (exit 139, `exe+0x2237320`, stubs 20); writer evidence
  verbatim above. No code changed → default-off/control-probe-line
  comparison is vacuous (0 HLE probe lines exist on this path by
  construction); the gdb run is the evidence.
- Deadlock shape: N/A — no code changed. (For the record: software
  breakpoints were used because hardware breakpoints are unsupported in
  this sandbox, proven on a hello binary too.)
