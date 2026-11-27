# Retail "CreateObject" chain trace (M1.3)

Date: 2026-11-27. Tip: `linux` @ `58534aaa` (step-03 landed). Task key:
`code-assist:xcl-first-frame:step-04:createobject-chain`.

Bounded: one fresh-root retail confirm plus two gdb break+printf runs (task
bound: 2). No product code changed; scripts lived in `/tmp` and are deleted.
VAs below are `0x140000000 + RVA` unless the text says RVA.

## Result in one paragraph

The chain is **not** a name-keyed object factory. It is the boot **pack
loader**: the game checks that the virtual path `"appdata0.rpf"` resolves to a
file through its per-thread mount table, then opens that file as an **RPF9
pack container** (`0x140fc21c0` factory, `0x140fc2240` ctor, vtable
`0x1462fd890`). The container opens successfully; the very next step builds its
**cache child device** (0x80 bytes, vtable `0x1462fd480`) via pack vtable slot
1 (`0x140fc2490`), whose init `0x140fc2540` fails at `0x1408ec200`
(`0x140a34890` → `0x1406bd160` → `0x140536a70` → `0x140613880`: the pack
entry's kind byte is `0x0b`, kind 5, whose class descriptor is the
zero-filled default at `0x1479f7060`, so the resource lookup is NULL).
Slot 1 then stores
NULL and the game dereferences it at `0x142237320` — the fault is the game's
missing NULL check after its own producer refused the object, as M1.2
concluded.

## Correction to the inherited label

M1.2/the frontier notes attributed `0x14678d1bb` to the string
`Exception in CreateObject("%s"):`. That address is a **single NUL byte**: the
format string really is at `0x14678d179` and is 0x42 bytes earlier
(`.rdata` file offset 0x5f3fe00-based dump; objdump `-s`):

```
14678d170 31376709 00256609 00457863 65707469   17g..%f..Excepti
14678d180 6f6e2069 6e204372 65617465 4f626a65   on in CreateObje
14678d190 63742822 25732229 3a0a0900 3f000000   ct("%s"):...?...
```

`0x14678d1bb` is referenced by 1,147 `lea` sites in `.text`: it is the
binary's canonical **empty string** constant. The call at `0x142236e64`
passes that empty string as the auxiliary argument, not a CreateObject format.
The `Exception in CreateObject` text is referenced once, from
`0x1414c4d6c` (function `0x14c4c60-0x14c4e63`, a failure/exception-report
site), not from this chain. The object is therefore not identified by a
class/type name; it is identified by the pack file name.

## Static map

Function bounds from `leonida-work/exe-disasm/functions.json` (`.pdata`).

### Per-thread mount table (the "registry")

`0x140643cd0` (`..0x643d51`) is a TLS singleton getter: TLS slot `+0x7f0`, a
0xc10-byte object allocated on first use, constructor `0x140643d60`:

```
140643d63  movabs rcx, 0x100000000
140643d6d  mov    [rax+0xc00], rcx    ; dword +0xc00 = 0, dword +0xc04 = 1
140643d76  mov    dword [rax+0xc08], 0
140643d80  mov    byte  [rax+0xc0c], 0
```

`0x14162b780` (`..0x162b8f3`) initialises that table from a
semicolon-separated string (`0x14678880a` = `";"`):

- resets `+0xc04 = 0`, `+0xc0c = 0`, `+0xc08 = 0`;
- up to 4 entries, each 0x100 bytes at `table + i*0x100`;
- a token starting `#` or `*` sets `+0xc0c` (`#` → 1) and `+0xc08 = i`;
- empty input falls back to one empty entry (`[table]=0`, count 1).

The boot sets the string to **`"G:/"`** immediately before parsing: at
`0x142236c9e` it stores dword `0x2f3a47` at `0x1474530b0` (little-endian bytes
`47 3a 2f 00`) and calls `0x14162b780(table, "G:/")`. The same function also
installs the static object `0x1468929b0` = vtable `0x144ec7c90`.

`0x14056eb10` (`..0x56eb2f`, wrapper) → `0x14056eb30` (`..0x56ec62`) is the
lookup:

- `0x140643c90` classifies a name: true if it starts `/` or `\` or contains
  `:`, else false (relative names are composed with the table entries);
- the composed name is formatted by `0x14128bb70`, normalised by
  `0x14128bae0`, then resolved by `0x140968480` (device resolution, `dl=1`);
- `0x140968480` (`..0x9686xx`) hashes the up-to-15-byte drive stem
  (`0x144fc2280`) for `:` names; `0x1478f5eb0` is one of its device-object
  candidates (static `.data`);
- the resolved device's `vtbl+0x168` is then asked whether the path exists;
  the function returns that boolean.

So `0x56eb10("appdata0.rpf")` = "does this virtual path exist", not "find a
registered object by type".

### Path aliases (separate table, owned by the pack VFS)

`0x142236bc0` (`..0x2236c40`, called from `0x1429324b5`) registers five
alias→path pairs with `0x141712b70`:

```
%ROOT%     -> G:/          0x146789ea9 / 0x14678983a
%PACKDATA% -> packcache:/  0x146789ec5 / 0x1474539d5 (runtime-built)
%PLATFORM% -> xbsx         0x146789eba / 0x146778a41
%PROJECT%  -> americas     0x146789eb0 / 0x14677c98f
//         -> /            0x14678988a / 0x146789896
```

`%PACKDATA%` → `packcache:/` is the same key the fault path uses later.

### Boot pack loader

`0x142236c90` (`..0x2236df2`), single caller `0x142240ea6`:

1. installs the static object/vtable, writes `"G:/"`, parses the mount table
   (`0x142236cc1` → `0x14162b780`);
2. lazily allocates the 0x28-byte loader state at global `0x147453e58`
   (`0x144d9c160(0x28,8)`, ctor `0x14582a610` zeroes it) and calls the loader
   `0x142236e00(state)`.

`0x142236e00` (`..0x22371bd`) is the boot pack loader:

```
142236e5d  lea rdx, 0x1474539f6          ; "appdata0.rpf"
142236e64  lea r8,  0x14678d1bb          ; "" (empty-string constant)
142236e6e  call 0x14056eb10              ; path-exists check
142236e73  test al, al
142236e75  je   0x14223716c              ; not present -> cleanup, no pack
...
142236f29  call 0x1422372c0              ; rcx = loader state
```

It also appends two path records to the state's vector (`+0x10` pointer,
`+0xc` length, `+0x8` capacity) via `0x1422371f0` with the runtime templates
`0x1478f5eb0` / `0x1478f5e80` (zero-fill `.data`, so written at boot), and
calls `0x140715f40(registry, scratch_path)`.

### Pack and child

`0x1422372c0` (`..0x2237478`, single caller `0x142236f29`) does:

```
1422372cf  lea rdi, [rsp+0x38]       ; out-slot
1422372d7  lea rdx, 0x1474539f6      ; "appdata0.rpf"
1422372e6  call 0x140fc21c0          ; RPF9 factory
1422372eb  mov rdi, [rdi]            ; pack container (non-NULL)
1422372f4  mov rax, [rdi]            ; vtable 0x1462fd890
142237313  call [rax+8]              ; slot 1 = 0x140fc2490 -> child
142237316  mov rbx, [rsp+0x38]       ; child (NULL on refusal)
142237319  lea rdx, 0x1474539d5      ; "packcache:/"
142237320  mov rax, [rbx]            ; FAULT
142237326  call [rax+0x3b8]          ; child would bind "packcache:/"
```

`0x140fc21c0` (`..0xfc2231`) is the RPF9 factory: optional header check
`dword [r8] == 0x52504639` ("RPF9" in RAGE's constant convention), alloc 0x60,
ctor `0x140fc2240`; success iff `[obj+0x18] != -1`.

`0x140fc2240` (`..0xfc248f`) sets `[obj] = 0x1462fd890`, so the container is
the RAGE RPF pack class.

`0x140fc2490` (`..0xfc252e`) is that class's vtable slot 1. It allocates the
0x80-byte child, runs `0x140fc2540`, then calls child vtable `+0x3a0`
(`0x140fc2530`, `[child+0x30] != 0`); on false it writes NULL at `0xfc2518`
and destroys the child.

`0x140fc2540` (`..0xfc26b2`) is the child init; `[child] = 0x1462fd480`. Two
content checks:

- `0x141fb2600` at `0xfc263d` (pack header view) — passes;
- `0x1408ec200` at `0xfc2664` — fails, so `0xfc2695` sets `[child+8] = 0`.

Child vtable `0x1462fd480`: `+0x3a0` = `0x140fc2530`, `+0x3b8` =
`0x1412c5630` (child, then a path argument; the call that crashes when the
child is NULL). Its `+0x298/+0x2a0` slots (`0x14056f130` / `0x14056ec70`) are
the same pack-path module as `0x14056eb10`.

`0x1408ec200` (`..0x8ec270`) gates on `[parent+8] & 1`, then
`0x140a34890(view)`, then `0x1428ce530`, `0x141fb2300`, `0x1406bd040`,
`0x1408ec350`. The **first** subcheck (`0x140a34890`) fails:

```
140a348a7  mov rax, [rcx+8]        ; pack entry/header view object
140a348ae  mov bl,  [rax+0x2e]     ; kind byte (measured 0x0b)
140a348b3  call 0x140b85020        ; test cl,cl; setne al
140a348ba  je   0x140a34964        ; kind == 0 -> al = 1 (SUCCESS)
140a348c0  shr bl                  ; kind >> 1 = class id (5)
140a348c4  call 0x140a349a0        ; class id -> descriptor
140a34964  mov al, 1               ; (b0 01) the kind == 0 return
```

`0x140a349a0` hands out a real class descriptor only for class id 1
(`0x1479f707c`) and id 4 (`0x1479f7098`); anything else gets the zero-filled
default `0x1479f7060`. For that default `word[desc+4] == 0`, which
`0x140536a70` maps through the `.rdata` table `0x1462fe860`
(`{0,1,2,0x10,0x10,0x10,0x12,0x12}`) to category 0; `0x140193f90` then takes
its invalid-category branch to `0x140575a70(key=0, 0, 1)` → NULL.

## Dynamic evidence

Fresh-root confirm (`/tmp/xcl-co-retail`): exit **139**, `SIGSEGV` at
`game_scarlett_master.exe+0x2237320`, pc `0x142237320`, fault `0x0`, code
bytes `48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b`; `called_stubs`
**2** (`ws2_32#115`, `IGameInput::#9`); zero `[VFS] unmapped` lines. (Plan
step 4 says "confirm 12": stale — M1.1a/b already took it 12→2.)

Run A (`/tmp/xcl-co-gdb`) — chain:

```
BP c90 init:      state_global=(nil) mountstr='G:/'
BP cc1 parse:     table=0x7ff3a016d450 str='G:/' count=1 special=0 idx=0
BP e00 loader:    state=0x7ff3a0139b00
BP e73 lookup:    al=1 keys='appdata0.rpf' / 'packcache:/'
BP f29 -> open:   rcx=0x7ff3a0139b00 rdx=0x2f r8=0xff r9=0x7ff3a016d85a
BP 2e6 factory:   name='appdata0.rpf' r8=(nil) out_slot=(nil)
BP fc21c0 factory: out=0x7fffffffa5e8 name='appdata0.rpf' hdr=(nil)
BP fc2540 child:  child=0x7ff3a0196800 parent=0x7ff3a01364a0
BP fc263d check1: rcx=0x7ff3a0196818 rdx=0x7ff3a0196820 -> al=1
BP fc2669 check2: al=0
BP fc2518 store:  child=0x7ff3a0196800 child+8=(nil) child+0x30=(nil)
```

Notes on the planned "64 B rdx keys": at `0x142236f29` only `rcx` is an
argument (`rdx=0x2f`, `r8=0xff`, `r9=stack` are dead scratch). The real keys
are the two `lea` constants above; there is no 64-byte key blob.

Run B (`/tmp/xcl-co-gdb2`) — bisect `0x1408ec200`:

```
BP ec200 entry: sub=0x7fffffffa4c8 parent=0x7ff3981364a0 parent+8=0x00
                sub=[nil, 0x7ff3981aa4b0, 0x7ff3981aa4f0, 0x7ff3981b63a8]
BP ec221 a34890_al=0
BP fc2695 init_fail: child=0x7ff398196800 parent=0x7ff3981364a0
```

So the pack container opened fine (factory returned a non-NULL object), and the
refusal is in the child init’s second check’s first subcheck `0x140a34890`.
An earlier revision of this paragraph said the view’s `+0x2e` byte was 0 and
that `0x140a348a0` required it non-zero; both halves were wrong — see the
2026-11-27 re-trace at the end of this file.

## HLE / unmapped correlation

- 0 `[VFS] unmapped` lines in the confirmed run; the pack container was opened
  from the real `appdata0.rpf` through HLE VFS, so the read path is not the gap.
- 2 `called_stubs` (`ws2_32#115`, `IGameInput::#9`) are not on this path;
  `xgameruntime.unimplemented` is empty.
- The NULL is written by the title's own slot-1 producer (`0x140fc2490` →
  `0xfc2518`) and dereferenced without a check at `0x142237320`; no HLE import
  or routing change would make the deref safe.

## Verdict

- **Object named:** the RPF9 pack container for the boot pack
  `"appdata0.rpf"` (`0x140fc21c0`/`0x140fc2240`, vtable `0x1462fd890`), plus
  the cache child device (vtable `0x1462fd480`) it tries to derive from it.
- **Registry identity:** the lookup at `0x14056eb10` is a per-thread
  **mount/path table** (TLS `+0x7f0`, 0xc10 bytes, entries 0x100 B at
  `+i*0x100`, count `+0xc04`), seeded with `"G:/"` by `0x14162b780`. It is a
  path-existence resolver (`0x140968480` device lookup), not a
  CreateObject-by-type registry. The name is a path, not a class key.
- **Key bytes:** `0x1474539f6` = `"appdata0.rpf"` (13 B with NUL) and
  `0x1474539d5` = `"packcache:/"` (12 B with NUL), both runtime-built ASCII.
- **Observed gate:** path exists (al=1) → RPF9 container opens → cache child
  init refuses at `0x1408ec200` (first subcheck `0x140a34890`: kind byte
  `0x0b` → class 5 → zero-filled default descriptor → resource lookup NULL)
  → slot 1 stores NULL → game faults.

## Re-trace 2026-11-27 — the kind byte is 0x0b, and 0 would SUCCEED

Re-probed at `linux` `71bb3ee6` (+ the M1.4/M2.4 landings) with soft
breakpoints inserted after `xcl::loader::run_main`. Fresh-root retail run:
exit 139, same RIP/code bytes, `called_stubs` 2. Evidence:

```
A 0x140a348b3  H[0x2e] = 0x0b          (view H = 0x7ff3a01aa4b0)
B 0x140a349a0  cl = H[0x2e] >> 1 = 5
C 0x140a3492b  desc = 0x1479f7060       (zeros; +0x18 = 0x8000007b)
D 0x1406bd16a  word[desc+4] = 0
E 0x1406bd187  0x140536a70 -> al = 0
H 0x1408ec221  0x140a34890 -> al = 0
```

Byte identity: `H+0x10` matches `appdata0.rpf` file offset `0x1010` exactly
(`42 30 26 7e 83 81 e6 5e` …), so `H[0x2e]` is file byte `0x102e` = `0x0b`.
`H[0..0x0f]` is the outer `9FPR` header (`0x360/0x340/0x40`). The byte is a
genuine file byte served correctly by the VFS — the same conclusion as
`docs/retail-crypto-terminal.md`.

Statics agree: `0x140a348ba je 0x140a34964` and `0x140a34964: b0 01`
(`mov al, 1`) — kind 0 is the **success** path; `0x140a348c0` is `d0 eb`
(`shr bl, 1`); `.rdata 0x1462fe860 = {0,1,2,0x10,0x10,0x10,0x12,0x12}`.

This restores `docs/retail-step-07-31.md` steps 09–31 (`kind byte
[blob+0x2e] = 0xb → 5 (default)`, `table[0] = 0`, zeroed class pool), which
had already reached the same honest negative.

## Next

No code. Boot-input order was re-checked (`XCL_TRACE_STRNCMP=1`, one run):
before the fault only `packroot:/appdata0.rpf` (plus the `G:/appdata0.rpf`
probe) is resolved; no sibling `packcache:`/`dlc:` input arrives, so the
console cannot be priming this from an earlier *file*. Pre-populating the
class pool by hand stays fake-success. Reopen only with genuinely new
evidence: a console-side trace showing the class-5 descriptor populated before
this call, or the mechanism that is supposed to seed the pack-cache
registration.
