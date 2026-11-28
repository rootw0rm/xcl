# Retail "obj1" initializer audit (Step 35)

> **Correction (2026-11-28):** the class-5/NULL branch below is real, but it
> was not the boot path. XCL's `ReadFile` ignored the `OVERLAPPED` offset, so
> the pack header byte `H[0x2e]` was read from file `0x102e` (`0x0b`, class 5)
> instead of `0x2e` (`0x09`, class 4). With the offset honored the title
> registers the pack and boots past this fault. See
> `docs/retail-overlapped-pack-read.md`; do not re-probe the class-5 chain.

Date: 2026-11-28. Tip: `linux` @ `a8f87b44`. Task key:
`code-assist:xcl-first-frame:step-35:obj1-initializer-audit`.
Depends on: `docs/retail-fault-outslot.md`, `docs/retail-createobject-chain.md`.

Bounded: static first (`leonida-work/exe-disasm/full.asm`,
`functions.json`, raw `.rdata`/`.data` qword scans), then two fresh-root
retail runs with break-after-`run_main` only. No product code; gdb scripts
lived in `/tmp` and are not committed.

VAs are `0x140000000 + RVA` unless the text says RVA. `full.asm` prints
`<.text+OFFSET>` comments against the section VA `0x140001000`, so
`.text+0xfc11c0` is VA `0x140fc21c0` — this offset-vs-VA conflation is the
root of the Step 35 question (see "The 0xfc11c0 phantom").

## Result in one paragraph

The object the notes called "obj1" is a real class: its vtable is
`.rdata` RVA `0x6308300` (VA `0x146308300`), slot 0 = `0x140fc1160`
(virtual deleting destructor), slot 1 = `0x1429e5480` (this document's
target), slot 2/3 = `0x1429e54a0` / `0x142253520` (key makers for
`0x129a1749` / `0x87f50b35`), slot 4 = `0x140004380` (`ret`), slot 5 = 0.
Its **intended initializer is the constructor `0x140d3bad0`** (pdata
`0xd3bad0…0xd3bb00`), reached only through the owner constructor
`0x140d3b9d0` (allocates the child, calls the ctor, then post-inits through
`0x140d3bb00`) and the wrapper `0x140d3b8e0`, whose only caller is
`0x143e838b7` inside `0x143e83890`. That thunk is **statically
unreachable**: no `call`/`jmp`/`lea` reference anywhere in `.text`, no
qword pointer in `.rdata`/`.data`, only its own `.pdata` entry. Both
retail runs confirm 0 executions of the ctor/wrapper chain before the
crash. The class is therefore real but not instantiated on this boot, and
it is not the fault object anyway (fault vtable `0x1462fd890`, slot 1
`0x140fc2490`). The "factory mid-entry `0xfc11c0`" never existed: that
string is the `.text`-relative annotation of the real factory
`0x140fc21c0`, and VA `0x140fc11c0` is mid-instruction inside the virtual
deleting destructor. Step 35 closes as an honest negative for the fault.

## The class and its vtable (static)

Raw qword dump at RVA `0x6308300` (`.rdata` file offset `0x6307100`):

```
rva 0x63082f8  (vtable-8, RTTI)  0x0                    <- no RTTI
rva 0x6308300  slot 0            0x140fc1160  deleting dtor
rva 0x6308308  slot 1            0x1429e5480  key test
rva 0x6308310  slot 2            0x1429e54a0  key maker 0x129a1749
rva 0x6308318  slot 3            0x142253520  key maker 0x87f50b35
rva 0x6308320  slot 4            0x140004380  ret
rva 0x6308328  slot 5            0x0
```

Slot 1 (`0x1429e5480`) is not a "4-insn bool stub" — it is six
instructions and reads only its second argument:

```
1429e5480  cmp edx, 0x129a1749
1429e5486  sete cl
1429e5489  cmp edx, 0x87f50b35
1429e548f  sete al
1429e5492  or  al, cl
1429e5494  ret
```

So the virtual protocol is "does this 32-bit key match either class key",
paired with two out-of-line makers that write each key:
`0x1429e54a0` (`mov [rdx], 0x129a1749; ret`) and `0x142253520`
(`mov [rdx], 0x87f50b35; ret`). Slot 4 is a bare `ret`. None of these
methods stores into the fault's out-slot; slot 1 is a pure predicate.

Slot 0 `0x140fc1160` is the class's virtual deleting destructor, not a
factory: pdata `0xfc1160…0xfc1232`, it calls the destructor body
`0x140fc1240` and then, when its `edx`/`edi` flag is non-zero, returns the
allocation to the pool at `0x147bc4ce8` (`lock dec [ctrl+0x10]` +
`cmpxchg` free-list push, `0xfc1179…0xfc121b`).

The destructor body `0x140fc1240` (pdata `0xfc1240…0xfc1288`) confirms the
class hierarchy and the two member writes that matter:

```
fc1245  lea  rax, [0x146308300]   ; this class
fc124f  mov  [rcx], rax
fc1252  cmp  qword [rcx+0x20], 0
fc1263  call 0x140438860          ; free(this+0x20) when set
fc1268  lea  rax, [0x1465283f0]   ; base class
fc1276  call 0x1451f2560          ; destroy +0x10 subobject
fc1283  jmp  0x1451f2570
```

Base chain: grandbase vtable `0x1466c7160` (word `+8 = 0xffff`, set by
`0x144f153e0`) <- base vtable `0x1465283f0` <- this class
`0x146308300`.

## Intended initializer and init protocol

`0x140d3bad0` — the only writer of vtable `0x146308300` after the base
ctor (pdata `0xd3bad0…0xd3bb00`):

```
d3badb  call 0x144f153b0          ; base ctor: vptr 0x1465283f0,
                                  ; zeroes the +0x10 subobject
d3bae0  lea  rax, [0x146308300]
d3baeb  mov  [rsi], rax           ; this class vptr
d3baee  vmovups [rsi+0x20], xmm0  ; +0x20..+0x2f = 0
d3baf3  mov  byte [rsi+0x30], bl  ; init flag from the caller
```

Its only call site is `0xd3ba67` inside `0x140d3b9d0` (pdata
`0xd3b9d0…0xd3ba85`), which is both an allocator and the *owner* class
constructor (owner vtable `0x146309dc0`):

```
d3b9e3  call 0xd3baa0             ; owner base ctor (vtable 0x146309480)
d3b9e8  lea  rax, [0x146309dc0]
d3b9f9  mov  [rsi], rax           ; owner vptr
d3b9fc…d3ba62                     ; pool alloc for the child, allocator ctrl
                                  ; 0x147bc4ce8; size = dword[ctrl+8] * index
d3ba67  call 0xd3bad0             ; child ctor, edx = init flag
d3ba6f  call 0xd3ba90             ; thunk: edx=0, r8=0, jmp 0xd3bb00
d3ba74  mov  dword [rsi+0x10], eax; owner stores the post-init result
```

Post-init `0x140d3bb00` (pdata `0xd3bb00…0xd3bc11`), called with the child
in `rcx`, `edx = 0`, `r8 = 0`:

```
d3bb14  call 0x144e3bfa0          ; thread/context
d3bb22  call 0x144e8f430          ; alloc buffer, size dword[0x147445e14]
d3bbac  call 0x141f67ba0          ; record ctor: vtable 0x14618c280,
                                  ; [buf+0x18] = child
d3bbb4  call 0x140a61410          ; allocator handle of buf -> eax
d3bbbb  test bl, bl / je return   ; bl (flag) is 0 on this path
```

Net init protocol: pool-allocate, base-construct, install vtable
`0x146308300`, zero `+0x20`, store the caller flag at `+0x30`, then
register a record holding the child and store its 32-bit allocation
handle in the owner's `+0x10`. Destruction mirrors it: virtual slot 0
frees `[this+0x20]`, chains the base destructor, then returns the block
to the same pool when the flag is set.

## Reachability (static + runtime)

- `0x140d3bad0` is called only from `0xd3ba67` (`0x140d3b9d0`).
- `0x140d3b9d0` is called only from `0xd3b8ef` (`0x140d3b8e0`,
  pdata `0xd3b8e0…0xd3b90b`).
- `0x140d3b8e0` is called only from `0x143e838b7` inside `0x143e83890`
  (pdata `0x3e83890…0x3e838e5`); the argument is
  `([ [rsi] ] != 0)` from `0x143e838b1…b4`.
- `0x143e83890` has no static reference at all: `full.asm` contains only
  its own definition line (no `call`/`jmp`/`lea … # 0x143e83890`), and a
  qword scan of `.rdata` + `.data` for VA `0x143e83890` finds 0 hits. The
  single dword hit for RVA `0x3e83890` is its own `.pdata` entry (file
  0x7006374). The surrounding block `0x143e83000…` is entered exactly
  once, by a branch from `0x143e82fac` to its first function
  `0x143e83002`; no branch, call, or address materialization targets
  `0x143e83890`, and `int3` padding separates it from the function
  before it (`0x143e83884…0x143e8388f`), so it is not reachable by
  fall-through either.

Runtime confirmation, two fresh-root runs (gdb software breakpoints after
`xcl::loader::run_main`; scripts removed):

```
break 0x140d3bad0 child ctor      0 hits
break 0x140d3b9d0 owner ctor      0 hits
break 0x140d3b8e0 wrapper         0 hits
break 0x143e83890 thunk           0 hits
break 0x140d3bb00 post-init       0 hits
break 0x142237313 fault call site 1 hit:
      rdi=0x7ff3a01364a0 vtable=0x1462fd890 slot1=0x140fc2490
```

`report.json` (run 2, `XCL_DATA_ROOT=/tmp/xcl-step35-r2`): exit **139**,
SIGSEGV at `game_scarlett_master.exe+0x2237320`, fault `0x0`, code bytes
`48 8b 03 48 89 d9 ff 90 b8 03 00 00 8b 46 0c 3b`, `called_stubs` **2**,
`xgameruntime.unimplemented={}` — unchanged.

## The 0xfc11c0 phantom (corrects `retail-fault-outslot.md` and PROMPT)

The fault path calls `0x140fc21c0` at `0x22372e6` (outer pack-container
factory, pdata `0xfc21c0…0xfc2231`; it allocates 0x60 bytes and calls the
ctor `0x140fc2240`, which stores vtable `0x1462fd890`). `llvm-objdump`
annotates that call as `<.text+0xfc11c0>` because `.text` starts at VA
`0x140001000`, so `.text+0xfc11c0` = `0x140fc21c0`.

Two separate errors came from treating that annotation as an address:

1. The factory is `0x140fc21c0`, not `0xfc11c0`; its pdata is
   `0xfc21c0…0xfc2231`, not `0xfc1160…0xfc1232`.
2. VA `0x140fc11c0` is **not an instruction boundary**: it is the last
   byte (`0x01`) of `add r9d, 0x1000000` at `0xfc11ba` (bytes
   `41 81 c1 00 00 00 01`), inside the deleting destructor `0x140fc1160`.
   `0xfc1160+0x60 = 0xfc11c0` therefore skips the `call 0xfc1240`
   destructor-body call and lands in the middle of an instruction — it is
   not a "mid-entry" a caller could use.

## Why 0x29e5480 was never the fault's producer

The fault call at `0x2237313` is `call qword ptr [rax+0x8]` with
`rax = [rdi]` = the outer pack object's vtable. File and runtime agree:

```
vtable 0x1462fd890 (rva 0x62fd890): slot0 0x1456dd500, slot1 0x140fc2490
```

`0x29e5480` could only be reached by `[vptr+8]` if the object's vtable were
`0x146308300`; the object at the call site has `0x1462fd890`. The producer
that writes the out-slot is `0x140fc2490`, whose gate refuses the
class-5 pack-cache record (`docs/retail-packcache-seeder.md`,
`docs/retail-inner-init.md`). No relation to obj1.

Also note the earlier `retail-fault-outslot.md` scan claim ("zero qword
hits for `0x14029e5480`"): the VA is `0x1429e5480` (RVA `0x29e5480` +
base). Searching that value in `.rdata` finds exactly one qword at RVA
`0x6308308` — slot 1 of this class's vtable, as recorded above. Vtables
store absolute VAs, so an RVA-as-qword scan could not have seen it.

## Verdict

- **Intended initializer:** ctor `0x140d3bad0` (base ctor `0x144f153b0`,
  class vptr `0x146308300`, `+0x20 = 0`, `+0x30 = flag`), reached through
  owner ctor `0x140d3b9d0` -> wrapper `0x140d3b8e0` -> thunk
  `0x143e83890`.
- **Status on this boot:** never instantiated. The only caller chain ends
  at a statically unreferenced (pdata-only) thunk; two retail runs hit 0
  constructor/wrapper/post-init breakpoints.
- **Relation to the crash:** none. The fault object's vtable is
  `0x1462fd890`, whose slot 1 is the pack-cache producer `0x140fc2490`;
  the `0xfc11c0` "factory mid-entry" is an annotation/VA conflation.
- **Product code:** none changed; nothing to port. Item closes.
