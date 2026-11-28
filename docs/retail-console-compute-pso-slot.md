# Retail M1 — Step 44: console slot 62 is the five-argument compute PSO

Date: 2026-11-28. Task `code-assist:xcl-first-frame:step-44:console-compute-pso-slot`.
Base: `linux` @ `fc808f74` (Step 43 docs). Product code + test @ `d0faaeaa`.

## Result in one paragraph

`xboxbvhbuilder.dll` calls console device vtable slot 62 with the console
`CreateComputePipelineState` signature `(desc, a, b, riid, ppv)` — two
arguments more than the PC method. Linux mapped slot 62 to the PC
`CreateStateObject` order `(desc, riid, ppv)`, so the DLL's `r9 = 0` was read
as `ppv`: the handler returned `S_FALSE (1)` without writing the sixth
argument, and the caller faulted at `xboxbvhbuilder.dll+0xc4f3`. Slot 62 now
has `Device_CreateComputePipelineStateX` with the console signature; the call
returns `S_OK`, writes the out slot, and the BVH-builder crash is gone. Retail
now reaches `game_scarlett_master.exe+0x4db9277`, a 992-byte hash over an
offset-shaped pointer that is not mapped (new frontier).

## Protocol (gdb sample on the fresh root)

At the crash site from Step 43 (`xboxbvhbuilder.dll+0xc470`) the call is:

```
call [device+0x1f0]              ; slot 62
  rdx = &desc                    ; D3D12_COMPUTE_PIPELINE_STATE_DESC
  r8  = 0                        ; console extra #1
  r9  = 0                        ; console extra #2
  stack arg5 = 0x1800caad8       ; riid (forwarded, never dereferenced)
  stack arg6 = out slot
```

gdb at the new handler with the retail DLL:

```
Breakpoint, Device_CreateComputePipelineStateX
  _t=0x7ff2783eb500 d=0x7ffff5be6eb8 a=0 b=0
  _riid=0x1800caad8 ppv=0x7ff2785ec7f0
desc:
  +0x00 pRootSignature 0x7ff27860a330
  +0x08 CS { pShaderBytecode 0x00000001800c96b0, BytecodeLength 0x1410 }
return: rax = 0 (S_OK), return address 0x18000c4e4 = xboxbvhbuilder+0xc4e4
```

So the two console extras are zero for this title, the CS bytecode is a
5136-byte blob inside `xboxbvhbuilder.dll`, and the out pointer arrives in the
sixth argument (first stack slot after the register arguments).

## Parity

The Windows twin already implements this contract:

- `hle/d3d12x/real.rs:935-938`: comment — device #62 is
  `CreateComputePipelineState` with two extra arguments from
  XboxBvhBuilder.dll (`desc, NULL, NULL, riid, ppv`); PC #62 =
  `CreateStateObject`.
- `:988`: `(62, dev_create_compute_pso_x)`.
- `:3026-3035`: six-argument handler — warns on non-null `a`/`b`, forwards
  `desc`/`riid`/`ppv` to the PC compute-PSO method.

## Fix

`hle/d3d12x_linux.rs`:

- `v[62] = Device_CreateComputePipelineStateX` (was
  `Device_CreatePipelineState4`).
- New handler
  `Device_CreateComputePipelineStateX(this, desc, a, b, riid, ppv)`:
  `xlog_warn!` on non-null `a`/`b` with the Windows text
  (`CreateComputePipelineState (console, slot 62): unknown parameters … ignored`),
  `capture_compute_desc(desc)`, then `out_obj(ppv, Kind::PipelineState)`.
  `riid` is never dereferenced, as on Windows.
- `Device_CreatePipelineState4` is deleted; `Kind::StateObject` and its
  `iface()` arm are deleted (no Linux constructor existed: only the variant
  and the old handler).

## Test (RED first)

`hle::d3d12x_linux::tests::console_compute_pso_slot62_writes_the_sixth_argument`
calls slot 62 with a zeroed `0x38`-byte
`D3D12_COMPUTE_PIPELINE_STATE_DESC`, `a = b = 0`, null `riid`, out in arg6, and
asserts `S_OK` + non-null out + `Kind::PipelineState`. A second call with
`a = b = 1` asserts the same (the extras must not shift the out argument).

RED at HEAD with only `v[62]` pointed at the PC four-argument
`Device_CreateComputePipelineState` (same `r9`-as-`ppv` misread):

```
assertion `left == right` failed
  left: 1      ; S_FALSE from out_obj(null)
 right: 0      ; S_OK
test result: FAILED. 0 passed; 1 failed; 881 filtered out
```

GREEN at the commit:

```
xcl: CreateComputePipelineState (console, slot 62): unknown parameters 0x1 0x1 ignored
test result: ok. 1 passed; 0 failed; 0 ignored; 881 filtered out
```

## Retail evidence

Fresh root `/tmp/xcl-step44-r1`, debug binary at `d0faaeaa`:

```
exit 139, crash SIGSEGV (signal 11) at game_scarlett_master.exe+0x4db9277
       pc 0x144db9277, fault 0x4688e4ec: unmapped
       code bytes 4d 8b 47 10 49 8b 47 28 49 8b 77 08 49 03 77 30
called_stubs (13): device #19 ×8, list #131/#132 ×5, #70 ×3, #20 ×2,
                   #79 ×2, LocaleNameToLCID ×2, #78 ×1, #133 ×1,
                   IDXGIOutput::#21 ×1, ShowWindow ×1, ws2_32#115 ×1,
                   IGameInput::#9 ×1
vfs.unmapped_paths: []      xgameruntime.unimplemented: {}
```

The old `xboxbvhbuilder.dll+0xc4f3` crash is gone, and the stub surface is
identical to Step 43 (slot 62 was never a stub; #127 remains routed). The Step
42/43 behaviour is unchanged in the same log: five title descriptor heaps
(`0x2600000 @ 0x1000000000` first) and the two 3840x2160 display targets at
`0x100bc00000` / `0x100dc00000` consuming the reserved `0x4000000` block.

### New frontier (Step 45): the 992-byte record hash

The crash is inside the title's XXH3-shaped hash helper. gdb at the fault:

```
PC  0x144db9277 (= exe+0x4db9277)
  144db9262  lea r15, [rcx + rdx - 0x40]
  144db9267  mov r13, rcx
  144db926a  mov r14, rdx
  144db9277  mov r8, [r15 + 0x10]     ; r15 = 0x4688e4dc, unmapped

rcx 0x4688e13c   rdx 0x3e0 (992)   r13 0x4688e13c   r14 0x3e0
r15 0x4688e4dc   fault 0x4688e4ec (r15 + 0x10)
return address into 0x14247f6b9 = exe+0x247f6b9
```

`0x145232960` is a jump to the hash entry `0x144db9200`, called at
`0x14247f6b4` from `exe+0x247f590` (the wrapper that sizes a stream record,
copies it and hashes it). The record pointer is `rcx = 0x4688e13c`, a value
below every mapped guest range, while the title's stream-base global
`0x14ab83ec0` is a real mapping (`0x7ff39ec00000`). Hypothesis (not yet
verified): the path hands a container offset to the hash without adding the
resolved base (the base lookup returned 0 on this path). Step 45 should sample
`0x14247f590`'s entry arguments at caller `exe+0x247b8b8`
(`[container+0x18]`, `[container+0x20]`, `rdx = [rbp+0xd0]`) and
`0x1428ca230`'s return to identify which stream/file base failed to resolve.

## Gates (this commit)

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **881 passed / 0 failed /
  1 ignored** (880 baseline + the new test).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Fresh-root retail: exit 139 at `game_scarlett_master.exe+0x4db9277`;
  `xboxbvhbuilder.dll+0xc4f3` gone; no new stubs.
