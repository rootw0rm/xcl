# Retail fault out-slot trace (M1.2)

Date: 2026-11-27. Tip: `linux` @ `a072156b` (step-02 landed). Task key:
`code-assist:xcl-first-frame:step-03:fault-outslot-trace`.
Depends on: `docs/retail-frontier.md`, `docs/retail-step-05.md`.

Bounded: two retail runs, break + printf only. No product code changed; the
trace scripts lived in `/tmp` and are deleted.

## What was asked

At the crash fn `exe+0x22372c0` (`.pdata` `[0x22372c0, 0x2237478)`):

1. What does `[rsp+0x38]` hold before and after the `+0x53` call `[rax+8]`?
2. What does `0x29e5480` return in `al` (`edx` vs `0x129a1749` /
   `0x87f50b35`)?
3. What bytes sit at `0x1474539f6` (and `0x1474539d5`) at the fault?
4. Which out-slot address is passed at `+0x53` (`[rsp+0x38]` identity)?
5. Name the writer of the NULL, or kill the theory.

## Method

`XCL_TRACE_FAULTS` is implemented in `crates/xcl/src/eh.rs`, which `lib.rs`
selects only for Windows; Linux uses `eh_linux.rs`, whose fatal-signal handler
reports the fault but cannot break at arbitrary guest PCs. The trace therefore
used gdb 18.1 break + printf against the debug build, with no code change:

- hardware breakpoints are unavailable on this host (`hbreak` →
  "No hardware breakpoint support in the target"); software breakpoints work.
- the guest image does not exist when gdb starts, so the script first stops at
  `xcl::loader::run_main` (image mapped and protected), then inserts guest
  breakpoints and continues:
  `set language c` (gdb's Rust language rejects C casts), and
  `handle SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGUSR1/SIGUSR2 nostop noprint pass`
  so the title's own crash/watchdog paths stay native.

Run 1 addresses: `0x142237313` (before `call [rax+8]`), `0x142237316` (after),
`0x140fc2490` (producer entry), `0x140fc2538` (validity gate result),
`0x142237320` (fault instruction), `0x1429e5480` (suspect, call-site return
address `0x142237316` only).

Run 2: `0x1429e5480` entry and `0x1429e5494` (its `ret`) unconditionally,
`0x140fc2518` (NULL store), `0x140fc2507` (success store), `0x142237316`
(post-call slot + key bytes). Run 2 deliberately does not break at
`0x2237320`, so its crash report keeps the canonical code bytes (run 1's
report shows `cc 8b 03 …` because gdb re-inserted its `int3` while
single-stepping the faulting instruction; the RIP/exit are unaffected).

## Fault path (static, VAs; `full.asm` = `leonida-work/exe-disasm`)

```
1422372cf  lea    rdi, [rsp+0x38]          ; out-slot address
1422372d7  lea    rdx, [rip+…]             ; key1 0x1474539f6
1422372e6  call   0x140fc21c0              ; outer factory writes [slot]
1422372eb  mov    rdi, [rdi]               ; rdi = outer record (non-NULL)
1422372f4  mov    rax, [rdi]               ; outer vtable 0x1462fd890
1422372ff  lea    rbx, [rsp+0x38]
142237310  mov    rdx, rbx                 ; arg2 = &slot
142237313  call   qword ptr [rax+0x8]      ; producer 0x140fc2490  <-- A
142237316  mov    rbx, [rbx]               ; rbx = [slot]                 <-- B
142237320  mov    rax, [rbx]               ; FAULT when [slot] == NULL
```

`0x140fc2490` (outer vtable slot 1):

```
fc24b0  call 0x144d9c160                     ; alloc 0x80 inner object
fc24e1  call 0x140fc2540                     ; inner init
fc24e6  mov rax, [rdi]                       ; inner vtable 0x1462fd480
fc24ec  call qword ptr [rax+0x3a0]           ; gate 0x140fc2530
fc24f2  test al, al
fc24f4  je   0x140fc2518
fc2507  mov  [rsi], rdi                      ; success: slot = inner
fc2518  mov  qword ptr [rsi], 0              ; failure: slot = NULL
```

`0x140fc2530` (inner vtable `+0x3a0`):

```
fc2530  cmp qword ptr [rcx+0x30], 0
fc2535  setne al
fc2538  ret
```

## Run 1 — full trace

```bash
rm -rf /tmp/xcl-faultslot
timeout 240 gdb -q -batch -x /tmp/xcl-trace1.gdb
#   ... --args ./target/debug/xcl run "<retail>/",
#   XCL_DATA_ROOT=/tmp/xcl-faultslot XCL_TRACE_FAULTS=1
```

```
=== A1 0x142237313 before call [rax+8] ===
rsp=0x7fffffffa5a0 rdi=0x7ff3981364a0 rsi=0x7ff398139b00 rax(vtable)=0x1462fd890
rcx(obj)=0x7ff3981364a0 rdx=0x7fffffffa5d8 r8=(nil) r9=(nil)
slot addr rsp+0x38=0x7fffffffa5d8 rdx==rsp+0x38=1
[rax+8] target=0x140fc2490
slot before=0x7ff3981364a0
obj vtable=0x1462fd890 +0x10=0x7ff3981aa380 +0x18=0x7ff397ecdd90 +0x20=(nil) +0x30=0x1dd7a1270e2bf4b +0x58=(nil)

=== C1 0xfc2490 writer entry ===
rcx(obj)=0x7ff3981364a0 rdx(slot)=0x7fffffffa5d8 r8=(nil) r9=(nil) ret=0x142237316
obj vtable=0x1462fd890 slot0=0x1456dd500 slot1=0x140fc2490 slot0x3a0=0x1428c8690 slot0x3b8=0x14573d7f0

=== D1 0x140fc2538 gate result (inner vtable+0x3a0) ===
rcx(inner)=0x7ff398196800 [inner vtable]=0x1462fd480 [inner+0x30]=(nil) al=0 ret=0x140fc24f2
0x7ff398196800: 0x00000001462fd480 0x0000000000000000
0x7ff398196810: 0x0000000000000000 0x00007ff3981aa4b0
0x7ff398196820: 0x0000000000000000 0x0000000000000002
0x7ff398196830: 0x0000000000000000 0x0000000000000000
```

```
key1 0x1474539f6: 61 70 70 64 61 74 61 30 2e 72 70 66 00 00 78 00
                  a  p  p  d  a  t  a  0  .  r  p  f
key2 0x1474539d5: 70 61 63 6b 63 61 63 68 65 3a 2f 00 00 00 00 79
                  p  a  c  k  c  a  c  h  e  :  /   \0
```

```
=== B1 0x142237316 after the call ===
rbx(&slot)=0x7fffffffa5d8 slot after=(nil)

=== F1 0x142237320 faulting instruction ===
rsp=0x7fffffffa5a0 rbx(slot value)=(nil) rdi(obj)=0x7ff3981364a0 rdx(key2)=0x1474539d5
slot [rsp+0x38]=(nil)
```

`report.json`: exit **139**, `SIGSEGV at game_scarlett_master.exe+0x2237320`,
pc `0x142237320`, fault `0x0`; `called_stubs` **2** (`ws2_32#115`,
`IGameInput::#9`) — unchanged from step-02.

## Run 2 — confirmation and the suspect

```bash
rm -rf /tmp/xcl-faultslot2
timeout 240 gdb -q -batch -x /tmp/xcl-trace2.gdb
```

```
=== NULL store at 0xfc2518 #1: rsi(slot)=0x7fffffffa5e8 rdi(inner)=0x7ff3a0196800 current [slot]=0x7ff3a01364a0
=== B 0x142237316 after call [rax+8]: rbx(&slot)=0x7fffffffa5e8 slot=(nil)
=== RUN2 COUNTERS: suspect entry=0 exit=0, null_store=1, success_store=0
```

`report.json`: exit **139**, same RIP, code bytes
`48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b` (clean); `called_stubs` **2**.

## Static cross-checks

- `0x29e5480` is referenced nowhere in the image: zero qword hits for
  `0x14029e5480` (VA), zero for `0x29e5480` as qword or dword. It is dead
  code for this boot, consistent with run 2's 0 entry / 0 exit hits.
- The outer vtable `0x1462fd890` slot 1 is `0x140fc2490` (confirmed both by the
  file's `.rdata` and at run time). The prior "obj1 vtable slot 1 = RVA
  `0x29e5480`" identification is wrong for this call site.
- The inner vtable `0x1462fd480` slot `+0x3a0` is `0x140fc2530` (file and run
  agree).
- Both key strings are in `.data`'s zero-fill tail: their file offsets
  (`0x74525f6` / `0x74525d5`) lie past `.data`'s raw end `0x6e03a00` and past
  EOF `0x729d400`, so the PE loader starts them zeroed; the boot writes them.
- The inner object's `[+8]` is **0** at the gate even though init
  `0xfc2540` sets it to the outer record at `0xfc2584`. The only other write
  is `mov qword ptr [rsi+8], 0` on init's failure exit at `0xfc2695`, so
  init bailed before the final commit. `0xfc2540` only zeroes `[+0x30]`
  (`0xfc25a9`); the gate observes it 0 and no later writer ran.

## Answers

1. **Out-slot contents.** Before the `+0x53` call it holds the outer record
   `0x7ff3981364a0` (valid, non-NULL); after the call it holds **NULL** — a
   deliberate store, not garbage.
2. **`al`.** The producer's gate (`0xfc2530`, inner vtable `+0x3a0`) returns
   **`al = 0`**. `0x29e5480` never executes, so it has no `al`; if it had been
   the target its `edx` would have been the low half of `&slot`
   (`0xffffa5d8` in run 1), matching neither `0x129a1749` nor `0x87f50b35`,
   so its result would also have been 0.
3. **`edx` / keys.** At `+0x53` `edx` is the low half of `rsp+0x38`
   (`0xffffa5d8` in run 1, `0xffffa5e8` in run 2), because `rdx = &slot`;
   the key pointers are the two `.data` strings at `0x1474539f6` and
   `0x1474539d5`.
4. **Key bytes.** `0x1474539f6` = `"appdata0.rpf"` (13 bytes incl. NUL;
   one preceding `0x01` byte at `0x1474539f5`), `0x1474539d5` =
   `"packcache:/"` (12 bytes incl. NUL). Both runtime-built; NUL-terminated
   ASCII, not GUIDs or binary keys.
5. **Slot identity.** Confirmed: at `0x2237313` `rdx == rsp+0x38`
   (printed `1`), and the producer receives that same address in `rdx`.

## Verdict

**Writer named, theory killed.** The NULL is written by the title's own
producer `0x140fc2490` at `0xfc2518`, gated by `0xfc2530`
(`cmp qword ptr [rcx+0x30], 0; setne al`). The field `inner+0x30` is 0
because the inner init `0xfc2540` took its failure exit (`[inner+8] == 0`
observed), so the gate correctly refuses the incomplete object. No HLE stub
and no import-routing gap is on this path.

The `0x29e5480` suspect is dead: not in any static vtable/table reference and
not executed once in a full boot. Its `edx`-vs-key comparison belongs to some
other producer, not this call.

## Next (own task, no code yet)

Find which check inside `0xfc2540` fails and why:
`0x1fb2600` (at `0xfc263d`, consumes outer `+0x10/+0x18/+0x20`) or
`0x8ec200` (at `0xfc2664`) — that is the missing-content producer the game
expects before it will accept the record. Static reading of `0xfc2540`
(`[inner+0x30]` zeroed at `0xfc25a9`, success store at `0xfc2507` never
reached) narrows it to that init chain, not to `0x22372c0` itself.
