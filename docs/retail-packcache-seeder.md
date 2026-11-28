# M1 fix-01 — pack-cache / class-5 registration seeder: honest negative

> **Correction (2026-11-28):** the class-5/NULL branch below is real, but it
> was not the boot path. XCL's `ReadFile` ignored the `OVERLAPPED` offset, so
> the pack header byte `H[0x2e]` was read from file `0x102e` (`0x0b`, class 5)
> instead of `0x2e` (`0x09`, class 4). With the offset honored the title
> registers the pack and boots past this fault. See
> `docs/retail-overlapped-pack-read.md`; do not re-probe the class-5 chain.

Date: 2026-11-27. Task `code-assist:xcl-first-frame:fix-01:packcache-
registration-seeder` (`task-1795801980-61f2`). Docs only; no product code.
Worktree `/tmp/xcl-seeder`, branch `m1-packcache-seeder`, base
`landing/m14-m24` @ `f590d27d`. The main worktree is the owner's unresolved
`upstream/main` merge (`MERGE_HEAD 81693f02`) and was not touched.

Inputs: `leonida-work/exe-disasm/full.asm` (VAs below are `0x140000000` +
RVA), the retail PE headers/bytes, and two fresh-root retail gdb runs
(task bound: 2). Baseline run: exit 139, `game_scarlett_master.exe+0x2237320`,
code bytes `48 8b 03 …`, `called_stubs` 2 (`ws2_32#115`,
`IGameInput::#9`).

## Result in one paragraph

**There is no seeder to find.** The "class-5 registration state" is not a
registry. `0x140a349a0` is a three-way MSVC magic-static accessor that
returns one of exactly three constant keys; only class 1 and class 4 have a
non-zero key. Every other class — including 5 — returns `0x1479f7060`, a
zero key `{u32 type = 0, u16 word[+4] = 0, 16 zero id bytes}`, produced by
the same accessor calling `0x145ebdad0` with hard-coded `edx=0, r8d=0` and a
zero 16-byte template. The key's `word[+4] = 0` indexes the read-only table
`0x1462fe860` to `0`, and `0x140193f90` rejects every value outside
`[0x10,0x15]`, so the registration call `0x1406bd160` returns 0. The class
is `H[0x2e] >> 1`; the boot pack's byte is `0x0b` → class 5. No code writes
any of the three keys after their one-time construction, or the table, and
the key ctor `0x145ebdad0` has exactly two call sites, both inside the
accessor. The accessor's class-5
answer is the default *by construction*, so no XCL-supplied input is
missing on this chain and no follow-up task is owed.

## Static enumeration (complete searched set)

### The three keys and their guards

The objects are three 0x18-byte magic statics in the zero-filled virtual
tail of `.data` (RVA `0x79f7060` is past `.data` raw size; end of raw data
is RVA `0x6e04e00`), each followed by its 4-byte guard at `object+0x18`:

| class | key object | guard | ctor arguments | id bytes |
|---|---|---|---|---|
| 1 | `0x1479f707c` | `0x1479f7094` | `edx=1`, `r8w=3`, `r9=0x1462fd460` | `27b3a6d53f7dd6b15524d8318a337582` |
| 4 | `0x1479f7098` | `0x1479f70b0` | `edx=0x10000001`, `r8w=0x10`, `r9=0x1462fd470` | `fa0ed6534cad7ac10ec65fbaaed601d6` |
| default (5 and all others) | `0x1479f7060` | `0x1479f7078` | `edx=0`, `r8d=0`, `r9=zeroed stack` | 16 zero bytes |

`0x145ebdad0` (`..0x145ebdaed`) is the whole writer:

```
145ebdad4  mov dword ptr [rcx], edx        ; type
145ebdad6  mov word  ptr [rcx+4], r8w      ; key word
145ebdade  vmovups xmmword ptr [rcx+6], xmm0 (zero)
145ebdae3  vmovups xmm0, xmmword ptr [r9]
145ebdae8  vmovups xmmword ptr [rcx+6], xmm0
```

It has exactly two callers, both in the accessor: `0x140a34aa6` (shared
class-1/class-4 leaf, with the arguments in the table) and `0x140a34b01`
(default leaf). There is no other caller and no other store to the objects.

`0x140a349a0` (`..0x140a34b19`) is the only reader of the objects. Its
dispatch is exhaustive:

```
140a349b6  mov eax, [0x1479f7078]        ; default guard
140a349c0  cmp eax, [tls_block+4]
140a349c6  jg  0x140a34ac7               ; slow init path
140a349cc  cmp cl, 4
140a349cf  je  0x140a34a45               ; class 4
140a349d1  movzx eax, cl
140a349d4  cmp eax, 1
140a349d7  jne 0x140a34ab8               ; every other class -> &0x1479f7060
```

The slow path `0x140a34ac7..0x140a34b14` initialises the *default* guard
with `0x1455db8cc` (MSVC `__Init_thread_header`, sets `-1` while in
progress) and calls the ctor with `xor edx,edx; xor r8d,r8d; vmovaps
[r9]=0` (`0x140a34af7..0x140a34b01`); the footer `0x1455db934` then writes a
monotonic completion token. Live the guard is `0x8000007b`, i.e. already
completed; later calls skip the slow path. Either way the default key is
all-zero.

### The reference set (why there is no other writer)

* `0x1479f7060`: exactly two `full.asm` references — `0x140a34ab8`
  (`lea rax` return of the default key) and `0x140a34af0` (`lea rcx` for
  the ctor). No RIP-relative store, no `movabs` of the address, no
  `.data`/`.rdata` pointer, and no base-relocation entry for RVA
  `0x79f7060` (parsed the PE `.reloc` section: no match for the key, the
  guards, or the table).
* `0x1479f7078/0x1479f707c/0x1479f7094/0x1479f7098/0x1479f70b0`: all
  references are the 13 instructions inside `0x140a349a0..0x140a34b19`.
  The next object after this cluster starts at `0x1479f70b8` and is written
  by the unrelated allocator init `0x1428c2010`; that init writes nothing
  in `0x1479f7060..0x1479f70b4`.
* `0x1462fe860`: exactly one reference — the reader `0x140536acb` inside
  `0x140536ac0` (`cmp cx,0x12; ja 0; lea rdx,[0x1462fe860]; mov eax,
  [rdx+4*rax]`). The table lives in read-only `.rdata`, has 19 dwords
  `{0,1,2,0x10,0x10,0x10,0x12,0x12,0x12,0,0x10,0x10,0x10,0x0d,0x0e,0x0f,
  0x10,0x11,0x12}`, and has no writer and no relocation.

### The failing path, with the live values

```
H[0x2e] = 0x0b
140a348c0  shr bl                              ; 0x0b >> 1 = 5
140a348c4  call 0x140a349a0                    ; -> 0x1479f7060 (default key)
140a348c9  mov rsi, rax
...
140a3492b  call 0x1406bd160                    ; register(view, key)
1406bd16a    movzx ebp, word [key+4]           ; = 0
1406bd182    call 0x140536a70                  ; table[0] -> eax = 0
140536a79      movzx ecx, word [key+4]
140536ad2      mov eax, [0x1462fe860 + 4*rax]  ; -> 0
140536aa5    call 0x140613880                  ; (view, 1, 0, key[0]=0, key+6, 0x10)
1406138de      call 0x140193f90                ; (type=0, category=0, mode=1)
140193f90        add edx, -0x10                ; 0 - 0x10
140193f93        cmp edx, 0x5
140193f96        ja  0x140193fc0               ; -> xor edx,edx; 0x140575a70
1406bd187    test al, al
1406bd189    je  0x1406bd21a                 ; return 0
```

`0x140193f90` never touches an allocator/registry for a category outside
`0x10..0x15`; it takes the invalid arm (`0x140193fc0: xor edx,edx; jmp
0x140575a70`), which returns NULL. `0x140a34890` therefore cannot be saved
by any *seeded* state: the key word is the only input to the category
selection, and it is 0.

## The `0x53525C0` table is the mount resolver, not class-5 state

The task note "the `0x53525C0` table via `0x643C10`" refers to the
per-process **mount/device-prefix** table, a different structure:

* consumer `0x1453525c0` (`..0x145352685`): TLS table getter
  `0x140643cd0`, name classifier `0x140643c90`, then a scan of count
  `[0x1478f5ec4]` / base `[0x1478f5ec8]`, 0x20-byte records, compare via
  the IAT slots `0x1467947b8`/`0x146794828`; called from `0x140643c25`
  (inside `0x140643c10`) and `0x142743c83`;
* seeder `0x1412c5670` (`..0x1412c58fe`), whose count write is
  `0x1412c5792`; it validates a name ending in `\` or `/` (length ≤ 0xfd),
  deduplicates, and appends a 0x20-byte record (copy `0x140c1b970`);
  wrapper `0x141fb2210` (reads the flag at `[rcx+0x114]`) is called from
  the pack loader (`0x142236885/0x1422369f1/0x142237232`, family
  `0x141fb2210`);
* the cache child's own vtable slot `+0x3b8` = `0x1412c5630` is a thin
  registrar call (`call 0x1412c5670` then `0x140a58030`) — that is where
  `"packcache:/"` would be inserted, at the faulting call
  `0x142237326`, and it never runs because the child is NULL.

Step-32 already verified this table live: `count=1`, `entry[0] =
"packroot:/"`, consult returns 1 before the gate. It is already seeded and
has no reference to `0x1479f7060`, `0x1462fe860`, or the key cluster. It is
a consumer at the device-resolution layer, not the missing class-5 state.

## Dynamic confirmation (2 fresh-root retail runs)

Run A (`/tmp/xcl-seeder-gdbA`, breakpoints after `xcl::loader::run_main`):
break `0x140a348ae` (after `rax = H`, `rdi = view`) and `0x140a348c9`
(after the key call).

```
A a348ae view=0x7fffffffa428 H=0x7ff3a016e8f0
  39 46 50 52 60 03 00 00 40 03 00 00 40 00 00 00
  42 30 26 7e 83 81 e6 5e da c2 d7 17 68 af 66 b8
  1e 15 21 18 55 ad 01 23 50 53 be fe 50 e0 0b 7f
  view+0=00000000 view+4=00000000 view+8=H view+10=... view+18=...
B a348c9 desc=0x1479f7060 d0=0 d4=0
  guard078=8000007b
  0x1479f7060..0x1479f7077 all zero
```

`H[0x2e] = 0x0b`; the key is the default, with both words 0.

Run B (`/tmp/xcl-seeder-gdbB`) at the only header reader `0x141fb2600`,
breakpoints `0x141fb2682` (after the first `[vtable+0x60]` read) and
`0x141fb26de` (after the stitched second read):

```
R1 at=0x141fb2682 self=0x7ff3a01aa380 rdx=0x7ff39fecdd90 r8=(nil) ret=16
  39 46 50 52 60 03 00 00 40 03 00 00 40 00 00 00   ; file[0..0xf]
R2 size=848 off=(nil) H=0x7ff3a01aa4b0 ret=848
  39 46 50 52 60 03 00 00 40 03 00 00 40 00 00 00   ; file[0..0xf]
  42 30 26 7e 83 81 e6 5e da c2 d7 17 68 af 66 b8   ; file[0x1010..]
  1e 15 21 18 55 ad 01 23 50 53 be fe 50 e0 0b 7f   ; file[0x1020..]
```

Interpretation: `0x141fb2600` asks its virtual reader for 0x10 bytes at
pack offset 0 and gets the plaintext header prefix, then for 0x340 bytes at
pack offset 0x10 and gets file bytes `0x1010..0x134f`; it allocates 0x350
and stitches header + TOC body into `H`. Therefore:

* the view the gate parses is synthetic (`0x10` header bytes + `0x340` TOC
  bytes), not a 0x350-byte file read at one offset;
* `H[0x2e]` is at `H+0x10+0x1e` = file byte `0x102e`, verified `0x0b`;
* HLE's only role on this chain is the file content, and it is byte-correct
  (the gdb dumps equal the host file bytes at 0 and `0x1010`).

## What a console pass would require (no new probe)

Inside this image, `0x140a34890` returns 1 only if one of three things is
true: `H[0x2e] == 0` (the pre-gate `0x140b85020` then jumps to the
`mov al,1` success return at `0x140a34964`), the class is 1 or 4, or the
key word is in a range that maps to `0x10..0x15`. All three are
data/constants: the class comes from the file byte, the key word from the
static default, and nothing writes either. A "registration" that stuffed
the key word (e.g. `5`, which maps to `0x10`) would only mask the crash —
fake success, not allowed.

So a console pass cannot be explained by a missing registration *seeder* in
this image. It can only come from different `H` bytes (package/file
provenance) or a different boot path running first. Neither is an
XCL-supplied input: the VFS serves the host file correctly, and no code
path writes the key cluster.

## Verdict and reopen triggers

Honest negative: no seeder exists in `game_scarlett_master.exe`; the
class-5 key is the zero default by construction. Do not pre-populate it and
do not queue an XCL-input task. Reopen only with genuinely new evidence:

1. a console-side trace showing a write to `0x1479f7060..0x1479f70b0`
   (none exists in this image), or
2. a console-equivalent `appdata0.rpf` whose `H[0x2e]` is 0 or whose class
   is 1/4, or
3. evidence that the pack reader's `[vtable+0x60]` returns transformed
   (not raw-file) bytes for this pack.

Until then the remaining divergence is data provenance / boot order, not
registration.

## Gates

Docs only; no code changed. Main worktree untouched (`MERGE_HEAD`
`81693f02`); no landing.
