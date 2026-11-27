# M1 inner-init closure: the failing check inside 0x140fc2540

Date: 2026-11-27. Task `code-assist:xcl-first-frame:fix-02:inner-init-closure`.
Docs only; no product code. Worktree `/tmp/xcl-seeder`, branch
`m1-packcache-seeder` (base `771493b8`, which already carries
`docs/retail-packcache-seeder.md`). The main worktree is the owner's
unresolved `upstream/main` merge (`MERGE_HEAD 81693f02`) and was not touched.

Inputs: `leonida-work/exe-disasm/full.asm` (VAs = `0x140000000` + RVA), the
retail PE bytes, and **six** short fresh-root retail gdb probes (one
observation run, four diagnostic variants of the same boot, one `H`
byte dump; ~4 s each). Baseline reproduction: exit 139,
`game_scarlett_master.exe+0x2237320`, code bytes
`48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b`, `called_stubs` 2
(`ws2_32#115`, `IGameInput::#9`).

This closes the "Next" item recorded at the end of
`docs/retail-fault-outslot.md` (which check inside the inner init
`0x140fc2540` fails and why).

## Answer

**The pack-header read `0x141fb2600` succeeds.** The failing gate is the
second one, `0x1408ec200` at `0xfc2664`, and it fails at exactly the class-5
registration refusal already documented in
`docs/retail-packcache-seeder.md`: `0x140a34890` -> `0x140a349a0(5)` ->
zero default key `0x1479f7060` -> `0x1406bd160` -> `0x140536a70` ->
`0x140613880` returns 0. There is no XCL-supplied input on that predicate;
every input is either a pack-file byte or a static key.

## Static chain

Outer producer body (crash function `0x22372c0`):

```
142237313  call  [rax+8]            ; 0x140fc2490 producer
142237316  mov   rbx, [rsp+0x38]    ; NULL
142237320  mov   rax, [rbx]         ; FAULT
```

`0x140fc2490` allocates the inner object and calls the inner init
`0x140fc2540`, then gates on `0x140fc2530`
(`cmp qword ptr [rcx+0x30], 0; setne al`). The init:

```
fc25a9  vmovups [rsi+0x30], ymm0     ; the field the gate tests, zeroed
fc2614  mov  rax, [rsi+0x8]          ; outer record (RPF9 check already passed)
fc2621  vmovups xmm0, [rax+0x10]     ; {reader object, ctx}
fc2626  mov  rcx, [rax+0x20]         ; base offset passed to the reader
fc262a..37  stack args: 16-byte {reader,ctx}, outer+0x20, caller arg5
fc263d  call 0x141fb2600             ; <-- gate 1: header/table read
fc2642  test al, al
fc2644  je   0x140fc2695             ; fail: [rsi+0x8] = 0 (never sets +0x30)
fc2646  mov  r8,[rsi+0x18] ; mov r9,[rsi+0x20]
fc2658  call 0x141fb2780             ; build view object at rsp+0x48
fc265d  mov  rdx,[rsi+0x8]           ; outer
fc2661  mov  rcx,rbx (&view)
fc2664  call 0x1408ec200             ; <-- gate 2: pack registration
fc2669  test al, al
fc266b  je   0x140fc2695
fc267b  call 0x14087aec0             ; fills inner+0x28 (incl. +0x30)
```

`0x141fb2600` (header/table read):

```
1fb260e  test r9,r9 ; jne 1fb2758     ; caller-supplied header, not used here
1fb2663  rax = [rdi]                  ; reader vtable
1fb267f  call [rax+0x60]              ; read(reader, ctx, base, &hdr, 0x10)
1fb2682  cmp eax, 0x10 ; jne fail
1fb268e  cmp [hdr], 0x52504639        ; 'RPF9'
1fb269c..a7  size = hdr[arg3 ? 2 : 1]; allocate 16 + size - 0x10
1fb26cf  copy the 16 header bytes
1fb26db  call [rax+0x60]              ; read(reader, ctx, base+0x10, body, size-0x10)
1fb26de  cmp eax, r14d ; jne fail
1fb26e3  cmp ebp,1 ; jne success      ; ebp = arg3 = 0 in this boot
```

Observed: one call, return address `0x140fc2642`, reader
`0x7ff3981aa380` (vtable `0x1462fc810`), `base = 0`: read 1 = 16 bytes,
read 2 = 0x350 bytes (`0x360 - 0x10`), both complete (`eax` 16 / 848),
`al = 1`. The stitched `H` is 0x360 bytes and its sampled bytes are the
file's: `H[0..0xf]` = `appdata0.rpf[0..0xf]`, `H[0x10..0x17]` =
`appdata0.rpf[0x1010..0x1017]`. Checked in the loaded process at the
gate:

```
H=0x7ff3a01aa4b0 H[0x2e]=0xb
0x7ff3a01aa4b0: 39 46 50 52 60 03 00 00 40 03 00 00 40 00 00 00
0x7ff3a01aa4c0: 42 30 26 7e 83 81 e6 5e  ...
file 0x0:      39 46 50 52 60 03 00 00 40 03 00 00 40 00 00 00
file 0x1010:   42 30 26 7e 83 81 e6 5e  ...
```

So `H[0x2e]` is file byte `0x102e` = `0x0b`, not an uninitialised field.

`0x1408ec200` (gate 2) is a three-line predicate over the same gate as
fix-01:

```
8ec200  al = [rdx+8] ; dl = !(al & 1) ; rdx = outer record
8ec21c  call 0x140a34890              ; M1.3 pack gate, rcx = view
8ec221  test al,al ; je 0x1408ec267   ; -> xor eax,eax
```

`0x140a34890` on `H[0x2e] = 0x0b` is unchanged from the seeder doc:
pre-gate `0x140b85020` is `test cl,cl; setne al`, so it proceeds; the
class is `0x0b >> 1 = 5`; `0x140a349a0(5)` returns the all-zero default
key; because the byte is odd it builds the two descriptors
(`0x140a34980(H[0x1a], [view+0x10])` and
`{[view+0x18], H[0x1c]-H[0x30]}`) and calls
`0x1406bd160(&desc, key)`; the key word `0` gives category `0`, and
`0x140613880` returns 0 via `0x140193f90`'s out-of-range arm.

So the NULL slot is the title's own gate on an incomplete inner object
whose only missing field is set by the pack-registration success path.

## Observed run

```bash
rm -rf /tmp/xcl-innerinit
XCL_DATA_ROOT=/tmp/xcl-innerinit timeout 150 gdb -q -batch \
  -x /tmp/xcl-innerinit.gdb --args ./target/debug/xcl run "<retail>/"
```

Breakpoints (inserted after `xcl::loader::run_main`, software, guest
signals passed through): `0x140fc2642` (gate 1 result), `0x1406bd189`
(inside `0x1406bd160`, so `al` is `0x140536a70`'s return), `0x1408ec221`
(gate 2 result, also prints `H[+0x2e]`), `0x140fc2669` (init result).

```
=== INIT-A after 0x141fb2600: al=1
=== REG 0x1406bd160: al=0          (0x140536a70 returned 0)
=== GATE 0x140a34890: al=0 obj=0x7fffffffa4c8 H=0x7ff3a01aa4b0 b2e=0xb
=== INIT-B after 0x1408ec200: al=0
```

`report.json`: exit **139**, same RIP `0x142237320`, same code bytes,
`called_stubs` 2 — i.e. the probe did not perturb the boot.

## Diagnostic: can the key word alone be forced?

Not shipped, and not a product change: guest memory only, in gdb, for one
run. (The seeder doc already noted that stuffing a key word would be fake
success; the point here is whether the refusal is a single-field
condition.)

1. The key object is `0x1479f7060`, its word at `+4`
   (`0x1479f7064`). Patching it right after `run_main` is overwritten:
   the magic-static ctor (`0x140a34af7..0x140a34b01`) still runs on the
   accessor's slow path and writes a zero key. The patch must land after
   the accessor returns.
2. Patching `word[key+4] = 5` at `0x140a348c9` (accessor return): the
   category becomes `0x10` and `0x140193f90` takes its valid arm
   (`0x140193fb0`; it receives the key type `0` in `rcx` and the mode
   flag `1` in `r8d`), but the lookup still returns **NULL**
   (`rax = (nil)` at `0x1406138e3`), so `0x140613880` returns 0 and the
   gate stays 0 (`INIT-B al=0`).

Why: the key word selects the catch-all category, but `0x140536a70` also
feeds the key's **type word** (`dword[key+0] = 0`) and its 16 zero id
bytes to `0x140613880`; only the class-1 and class-4 magic keys carry a
usable type/id pair (`1` and `0x10000001`). Forcing the type/id as well
would mean fabricating a class-1/class-4 identity for a class-5 pack —
the fake success the seeder doc forbids. The refusal is structural, not a
single missing field.

## Verdict

* The pack header/table read is **not** the failure: `0x141fb2600`
  requests 0x10 + 0x350 bytes, both reads complete, and the sampled `H`
  bytes are the file's (so a failed or short header/table read is
  excluded on this path).
* The failure is the class-5 registration refusal, reached from
  `0x1408ec200`; the crash at `0x2237320` is downstream of it and still
  the title's own missing NULL check.
* No new XCL-input task: the predicate has no host-supplied state.

Reopen only with new evidence: a console-side trace showing a write to the
key cluster, a console-equivalent pack whose byte `0x102e` is not `0x0b`,
or evidence that the pack reader returns different bytes for this pack.
This is the same reopen set as `docs/retail-packcache-seeder.md`, now with
the inner-init branch eliminated.

## Gates

Docs only; no code changed, no compile/test gates apply. The retail probe
above is the verification (same exit/RIP/stubs as the baseline). Main
worktree untouched; no landing.
