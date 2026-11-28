# Retail Step 66 — the fail-fast at `exe+0x56abfa6` is a zero-size BVH allocation

Date: 2026-11-28. Step 66. Base: `linux` @ `f59e7d51` (Step 65 docs).
Task `code-assist:xcl-first-frame:step-66:failfast-helper`.

## Result in one paragraph

The `int3` at `exe+0x56abfa5` is the title's alloc-check: `0x1456abf90`
calls `0x140075230(size, dl=1)` and traps when it returns 0. The live caller
is the third static one, `0x145c0c469` (function `0x145c0c3b0`), and it
passes `rcx = [node]` — the first qword of a BVH node built by
`0x140075480`. That function's tail calls
`XboxBvhBuilder::GetRaytracingAccelerationStructurePrebuildInfo` through the
title's IAT (`0x146793f98`) and copies the returned
`ResultDataMaxSizeInBytes` into `[node]`. The import was an XCL auto-stub:
it returned 0 and wrote no output, so the size was 0 and the alloc-check
fired. The import was stubbed because the shipped `XboxBvhBuilder.dll`
never loaded: it and `msvcp140.dll` both link at `0x180000000`;
`msvcp140` maps first since Step 62, and the Linux loader treated the
occupied preferred address as a hard error and silently fell back to auto
stubs. Fix: port the Windows loader's relocation fallback for an occupied
preferred address, so the real `DYNAMIC_BASE` DLL loads and its prebuild
info returns real sizes. Retail moves from `+0x56abfa6` (SIGTRAP) to
`+0x2219651` (the title's `bad_alloc`/unreachable path) with the first
frame still presented.

## Sample (bounded gdb, pre-fix tree)

One valid sample (`/tmp/xcl-step66-g3`; `-g1`/`-g2` were harness failures:
passing `SIGTRAP` to the inferior killed it at the entry trap, so gdb never
ran; `-g4` is post-fix and is described under Retail). Breakpoints were
planted after the `xcl::loader::run_main` stop, as in earlier steps.

| probe | hits | values |
|---|---|---|
| `0x14022cb16` (caller 1) | 0 | not this path |
| `0x145b1a71a` (caller 2) | 0 | not this path |
| `0x145c0c469` (caller 3) | 1 | `rcx=0x0`, `rsi=0x7ff2bcff74f0` |
| `0x1456abf90` (thunk) | 1 | `rcx=0x0`, `rdx=0x30` (`dl=1`) |
| `0x140075440` (`0x140075230`) | 1 | `rcx=0x0`, `rdx=0x1` |
| `0x140075452` (after `0x14221ad20`) | 1 | `rax=0x0`, `r8=0x100` |
| `0x14007546c` (alloc return) | 1 | `rax=0x0` |
| `0x1456abf9b` (after the call) | 1 | `rax=0x0` → `int3` |

So the failing size is exactly 0: `0x14221ad20(0, 0x100)` calls the
allocator wrapper `0x144fac3c0` with `rdi = 0`, whose virtual call
`0x140104070` returns 0 for a zero size, and `0x142219620`'s throwing path
is not taken. The trap at `0x1456abfa5` follows.

### Where the 0 comes from

Static map (confirmed against `leonida-work/exe-disasm/full.asm`):

```
145c0c3b0  build a BVH node descriptor at [rsp+0x50] (type 6, one node)
145c0c45d  call 0x144ee1860          ; jmp 0x140075480
145c0c462  [rsi+0x28] = rax         ; rax = node
145c0c466  rcx = [rax]              ; ResultDataMaxSizeInBytes
145c0c469  call 0x1456abf90         ; alloc(rcx) + assert
```

`0x140075480` builds the builder inputs and at `0x14007595c` calls the IAT
slot `0x146793f98`; then copies the output pair into the node:

```
14007595c  call [0x146793f98]       ; XboxBvhBuilder::GetRaytracingAccelerationStructurePrebuildInfo
140075962  ...
140075976  [rdi]     = out[0]      ; ResultDataMaxSizeInBytes
14007597d  [rdi + 8] = out[1]      ; ScratchDataSizeInBytes
```

`xcl.log` (Step 65 baseline, same binary) pins the stub and its caller:

```
[STUB] xboxbvhbuilder.dll!?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@... first called from game_scarlett_master.exe+0x75962 → returning 0x0
```

`0x14075962` is the return address of the call above. The cold-path
`0x1456ac100` sample site in Step 65's notes is a different helper (the
BVH stream/prebuild path used by `0x14022cb16`); it is not on this trace.

### Why the real `XboxBvhBuilder.dll` was not there

Both shipped DLLs declare `ImageBase 0x180000000` (verified with
`objdump -p`; the builder also carries 364 `DIR64` relocations and
`DYNAMIC_BASE`). The loader maps the first one at its link address and the
second one at the same address would fail. The module list proves the
order changed with Step 62:

- Step 44 (`/tmp/xcl-step44-r1`): `xboxbvhbuilder.dll @ 0x180000000`
  mapped, `msvcp140` HLE-stubbed (`imports_by_source`:
  `game-dll:xboxbvhbuilder.dll: 7`).
- Step 65 baseline (`/tmp/xcl-critic-step65-r1`) and this run:
  `msvcp140.dll @ 0x180000000` mapped, every `xboxbvhbuilder.dll` import
  `→ Stub` (`stub: 327`).

`resolve_inner_opt` silently converts a failed `load_game_dll` into
`stub_for(...)`, so the loader's hard error never reached the log. The
`debug` log line for the import is:

```
import game_scarlett_master.exe: xboxbvhbuilder.dll!?GetRaytracing... → Stub
```

## Fix (XCL-side, Windows parity)

`crates/xcl/src/loader_linux.rs`:

- `Mapping::new(base, len, relocatable)` tries `mmap(MAP_FIXED_NOREPLACE)` at
  the preferred address. On `EEXIST` with `pe.has_relocs()` it maps an
  OS-chosen address (plain `mmap(NULL)`), and `apply_relocations` patches the
  image with the resulting delta — the same two-step fallback as
  `loader.rs`'s `VirtualAlloc(preferred)` → `VirtualAlloc(NULL)`.
- A reloc-stripped image stays a hard error naming the address
  (`… is occupied … and the image has no relocations`), matching the Windows
  loader's `… preferred address … is taken and the image has no relocations`.
- A relocated load logs `preferred address … is occupied; mapped at … with
  relocations` and `loaded <dll> @ <base> (size …, preferred …)`.

The mod-level and `load_game_dll` docs no longer claim fixed-address-only
mapping. No other loader invariant changes: fixups, TLS callbacks, imports,
exports and DllMain all run from the actual `MappedModule.base`.

## Tests (RED first)

RED in a throwaway worktree at `f59e7d51` with only the new scenario test
grafted (`load_game_dll` on a relocatable DLL whose preferred base is held by
an anonymous mapping):

```
loader::tests::step66_red_relocatable_dll_with_occupied_base_loads ... FAILED
a relocatable DLL must load elsewhere: "…: preferred address 0x7f76b684a000 is occupied
(File exists (os error 17)); the image is mapped only at its link address (no relocation fallback)"
test result: FAILED. 0 passed; 1 failed; 914 filtered out
```

GREEN at the fix, `cargo test -p xcl --lib -- occupied`:

- `relocates_a_relocatable_image_when_the_link_address_is_occupied`
- `rejects_an_occupied_link_address_without_relocations`
- `load_game_dll_relocates_a_dll_whose_base_is_occupied`
- `load_game_dll_rejects_an_occupied_base_without_relocations`

**5 passed / 0 failed** (the fifth is the existing
`maps_a_synthetic_pe_at_its_link_address` name match).

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **915 passed / 0 failed /
  1 ignored** (913 before + 2 net new).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Retail evidence

Fresh roots, debug binary at HEAD, no gdb:

- `/tmp/xcl-step66-r1`: `xboxbvhbuilder.dll: preferred address 0x180000000
  is occupied; mapped at 0x7faac9b20000 with relocations`, `imports
  resolved (21 DLLs + 0 delay, 57 functions)`, `DllMain(...PROCESS_ATTACH)`;
  first frame `presented 3840x2160` at 6.777 s; exit 133 SIGTRAP at
  `game_scarlett_master.exe+0x2219651`.
- `/tmp/xcl-step66-r2` (final): same load, first frame at 7.464 s, exit
  **133** SIGTRAP at **`+0x2219651`** (`pc 0x142219651`, code bytes
  `4c 89 f1 48 83 c4 28 …`, fault 0), **44** called stubs.

Stub delta against Step 65's 48:

- gone: `xboxbvhbuilder.dll` constructor and prebuild-info (real module),
  `bink2scarlett.dll` `BinkSetMemory`/`BinkSetOSFileCallbacks`/
  `BinkSetSoundSystem`/`BinkStartAsyncThread`, `libhttpclient.gdk.dll`
  `HCInitialize`;
- new: `advapi32.dll` `EventRegister`/`EventSetInformation`,
  `api-ms-win-crt-math-l1-1-0.dll` `log2`.

No line names an `xboxbvhbuilder.dll` stub any more. The old
`+0x56abfa6` trap is gone; the title's real prebuild info ran and returned
nonzero sizes, and the title now fails while allocating that size.

## New frontier (Step 67 input)

`exe+0x2219651`: `0x142219620` is the allocator's throwing path
(`lock → vtable[0x30] → int3`; the `int3` is the unreachable marker after a
non-returning throw). It is reached from `0x144fac3c0` when the allocator's
virtual slot 0 returns 0 for a nonzero size. The size is the real prebuild
info from `0x140075480` (`[node]`). Next inputs:

1. the requested size and alignment at `0x140075452` / `0x144fac3c0`, the
   allocator object and its vtable (is it the title memory pool that
   `XMemAlloc`/the `XMem` hooks back, and does its budget refuse the block?);
2. whether the title's `_CxxThrowException` path is expected to unwind here
   (Linux `.pdata` translation is not implemented, so the `int3` is where a
   caught `bad_alloc` would be);
3. a separate observation from the post-fix gdb run `/tmp/xcl-step66-g4`:
   the title can also reach `VulList_TimestampBottom` with `va = 0x1`, which
   XCL's Rust write turns into a misaligned-pointer panic at
   `hle/d3d12x_linux.rs:2338`. That run presented three frames and aborted
   on that panic; it did not take the BVH path, so it is timing-dependent and
   not the gate run. Treat it as a second candidate frontier.

Do not fake a nonzero prebuild return, do not patch the `int3`, and do not
skip the allocation check.
