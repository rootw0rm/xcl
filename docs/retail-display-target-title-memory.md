# Retail M1 — Step 43: display targets take their memory from the title's allocation hook

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-43:display-target-title-memory`.
Base: `linux` @ `714d3f15` (Step 42 docs). Product code + test @ this commit.

## Result in one paragraph

The Linux null device now implements console device slot 127
(`CreateCommittedResourceX` with a barrier layout) far enough for a display
target: a committed resource with the console `ALLOW_DISPLAY` heap flag gets its
memory from the title's exported `__XMemAllocationHooks__` allocator with
`D3D_DISPLAY_TARGET`, exactly like the Windows twin's
`create_display_target`. That is the event the title's registered
display-target region (`0x1456a2a60`, class-0x80 list) bumps on. Retail's two
3840x2160 back buffers now fill the region the title reserved for them
(`0x100bc00000` + two 64 KiB-aligned `0x2000000` allocations = `0x4000000`) and
the Step-42 trap at `exe+0x247af2e` is gone. The run advances to a new crash in
`xboxbvhbuilder.dll` (`+0xc4f3`, device slot 62's out slot never written).

## Protocol (gdb sample on the fresh root)

`0x14247ad10` builds `{cursor = 0, cap = 0x4000000}` (or `0x5200000` when
`0x140075140()` is true) at `[rsp+0x70]` and registers it through `0x145c8f990`
with handler `0x1456a2a60` in the class-0x80 handler list of
`__XMemAllocationHooks__` (the list head at `0x146843758`; class index
`(attrs >> 16) & 0xff`). `0x1456a2a60`:

```
0x1456a2a66  mov  eax, edx
0x1456a2a68  and  eax, 0xc000
0x1456a2a6d  cmp  eax, 0x8000        ; event: a region allocation
0x1456a2a72  je   0x1456a2a8b
…                                    ; other events: forward to the next handler
0x1456a2a8b  mov  rax, [0x1475e6ff0] ; window base (0: not allocated yet)
0x1456a2a99  test rax, rax
0x1456a2a9c  jne  0x1456a2ad3        ; later calls bump inside the window
…                                    ; first call: [r9] (next handler) allocates
                                     ; the whole [r8+8] = cap window
0x1456a2ad6  add  rax, [r8+8]        ; base + cap = end
0x1456a2ada  add  rcx, 0xffff
0x1456a2ae1  and  rcx, -0x10000      ; align the request to 64 KiB
0x1456a2ae8  add  rcx, rdx           ; base + aligned request
0x1456a2aeb  cmp  rcx, rax
0x1456a2aee  ja   0x1456a2afd        ; int3: over the window
0x1456a2af0  mov  [r8], rcx          ; arena cursor
0x1456a2af3  mov  rax, rdx           ; return the block
```

so `[rsp+0x70]` is the bump cursor, `[rsp+0x78]` the cap, and
`[0x1475e6ff0]` the window base. The end assertion

```
0x14247af14  mov rax, [rsp+0x78]
0x14247af19  sub rax, [rsp+0x70]
0x14247af1e  add rax, [0x1475e6ff0]
0x14247af25  cmp rax, 0x1fffff
0x14247af2b  jbe 0x14247af2e
0x14247af2d  int3
```

therefore reduces to **`cap - allocated <= 2 MiB`**: the base cancels
(`cap - (base + allocated) + base`). Before this step nothing ever called the
handler, so the operands were `{cursor 0, cap 0x4000000, base 0}` and the
64 MiB block was unused.

`D3D_DISPLAY_TARGET = 2 << 14 | 0x80 << 16 | 16 << 24 | 1 << 29` has
`(attrs & 0xc000) == 0x8000` and class `0x80` — the exact event and list the
region was registered on. The Windows twin already allocates display targets
this way (`hle/d3d12x/real.rs::create_display_target` →
`kernelx::runtime_alloc(size, D3D_DISPLAY_TARGET)`), with the comment for this
same retail pattern ("back buffers one block … asserts that it is used up").

Inside `0x14247ad10` the title creates two display targets through device
slot 127. gdb at the call site `0x14193e1b4`:

```
props  { Type = DEFAULT, node masks 1 }
flags  0x8                       ; console D3D12_HEAP_FLAG_ALLOW_DISPLAY
desc   Dimension TEXTURE2D, 3840x2160, R8G8B8A8_UNORM, 1 mip / 1 slice,
       flags @+0x30 = 0x300005
layout 3                         ; the PC initial state's argument
clear  a D3D12_CLEAR_VALUE*
```

`hle::xg_linux::Texture::new` sizes that console description as `0x1ff8000`
bytes; 64 KiB alignment in the region handler makes each allocation
`0x2000000`, so two of them fill `0x4000000` exactly.

## Fix

- `hle/kernelx_linux.rs`: `D3D_DISPLAY_TARGET` (same value and comment as
  `hle/kernelx.rs`).
- `hle/d3d12x_linux.rs`: device slot 127 →
  `Device_CreateCommittedResourceLayoutX` (console
  `CreateCommittedResourceX` with a barrier layout: `this, props, flags, desc,
  layout, clear, protected, castable, formats, riid, ppv`). With
  `flags & 0x8` (`ALLOW_DISPLAY`) and a console texture description it:
  1. computes the allocation with `crate::hle::xg_linux::Texture::new(...).size()`,
  2. takes it from `kernelx_linux::runtime_alloc(size, D3D_DISPLAY_TARGET)`,
  3. returns a null `Kind::Resource` whose `value` (GPU VA) is the title's
     pointer and whose `mem` is that VA/size, like `CreatePlacedResourceX`.

  Without a title hook, or for any non-display committed resource, the slot
  keeps the plain `new_resource` path.

No other behaviour changes; slot 127 was a logging stub before, so titles
without a display target only gain the generic committed-resource shape.

## Test (RED first)

`hle::d3d12x_linux::tests::display_target_memory_comes_from_the_title_allocation_hook`
installs a synthetic title allocator (test-only `set_title_hooks_for_test`) and
drives device slot 127 from the vtable with the retail description above,
`flags = 0x8`, `layout = 3`:

- the hook receives `attrs == D3D_DISPLAY_TARGET` and `size == 0x1ff8000`;
- the returned resource's `value` is the hook's block and `mem` is
  `(block, size)`.

The two hook tests share `title_hooks_serial()` because `TITLE_HOOKS` is
process-global. RED on the same tree with slot 127 left unrouted:

```
the display target resource was not created
test result: FAILED. 0 passed; 1 failed; 880 filtered out
```

(the logging stub returns `S_OK`/1 but writes no out pointer, so the guest's
resource slot stays null). GREEN at HEAD: `1 passed`, and the log line
`console display target 3840x2160 format 28: 0x1ff8000 bytes from the title's
memory at 0x1000000000`.

## Retail evidence

Fresh root `/tmp/xcl-step43-r5`, debug binary at this commit:

```
exit 139, crash SIGSEGV (signal 11) at xboxbvhbuilder.dll+0xc4f3
       pc 0x18000c4f3, fault 0x0 (code bytes 48 8b 01 ff 50 30 …)
called_stubs: device #19 ×8, list #131/#132 ×5, #20 ×2, #79 ×2, #70 ×1,
              #78 ×1, #133 ×1, IDXGIOutput::#21 ×1, LocaleNameToLCID ×2,
              ShowWindow ×1, ws2_32#115 ×1, IGameInput::#9 ×1
```

(#127 is no longer a stub.) The log now shows the two display targets and the
region assertion passing. gdb on the same tree:

```
ENTER 14247ad10
ARENA size=0x1ff8000 attrs=0x30808000 cur=0x0 cap=0x4000000 global=0x0
console display target 3840x2160 format 28: 0x1ff8000 bytes from the title's memory at 0x100bc00000
ARENA size=0x1ff8000 attrs=0x30808000 cur=0x100dc00000 cap=0x4000000 global=0x100bc00000
console display target 3840x2160 format 28: 0x1ff8000 bytes from the title's memory at 0x100dc00000
ASSERT s70=0x100fc00000 s78=0x4000000 base=0x100bc00000
```

`0x100fc00000 - 0x100bc00000 = 0x4000000 = cap`, so the assertion's
`cap - allocated = 0` and `+0x247af2e` no longer fires. The Step-41
`+0x1cf336d` and Step-42 stubs are unchanged.

### New frontier (Step 44): device slot 62's out slot

`xboxbvhbuilder.dll+0xc470` calls the device method at vtable offset `0x1f0`
(slot 62, XCL's PC-order `CreatePipelineState` mapping) with
`rdx = &desc, r8 = 0, r9 = 0`, stack arg5 = `0x1800caad8` (a callback) and
stack arg6 = the out slot. XCL's `Device_CreatePipelineState4` takes
`(this, desc, ret, ppv)`, so it reads `r9 = 0` as `ppv`, returns `S_FALSE (1)`
and writes nothing:

```
BEFORE rcx=device vtable=… fn=… r9=0 arg6=0x7ff27c5ec7f0 outval=0x0
AFTER  eax=0x1 outval=0x0
CRASH  rcx=0x0 rdi=0x7ff27c5ec7f0 outval=0x0   ; mov rax,[rcx]
```

The caller then dereferences the still-null out slot at `0x18000c4f3`
(`mov rax,[rcx]; call [rax+0x30]`). New frontier: give console slot 62 its
real signature / out slot (the callback-shaped argument at arg5 is not
modelled yet), or route the BVH builder's call before it. The
`[SHADER] rootsig: invalid pointer/size (…, 16)` line is only
`gpu::shader::capture` refusing the console root-signature blob — the device
still returns a `Kind::RootSignature`, so it is not the crash.

## Gates (this commit)

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **880 passed / 0 failed /
  1 ignored** (879 baseline + the new test).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Fresh-root retail: exit 139 at `xboxbvhbuilder.dll+0xc4f3`
  (`exe+0x247af2e` gone); the two display targets consume the reserved
  `0x4000000` block.
