# Retail M1 — the pipeline-cache stubs: deserialize returns E_NOTIMPL

Date: 2026-11-28. Step 48 (first slice). Base: `linux` @ `a75065dc`
(Step 47 docs). Product code + test in one commit.

## Result in one paragraph

After the device-#108 and queue-layout fixes, retail called device #57 585
times and #67 42 times — the console `Deserialize{Graphics,Compute}PipelineStateX`
slots. Linux's default stub returned `S_OK` without touching the caller's out
pointer, while the Windows twin returns `E_NOTIMPL` and nulls it (console
pipeline caches are not supported; the title creates the pipeline from its
desc). Routing both to that handler and `#86` `CreatePipelineState` to a
`PipelineState` out object removes the uninitialised pipeline pointers. Retail
then leaves the title job-dispatcher crash (`+0x247a098`) and stops instead at
`+0x501f808`, dereferencing a null resource object (`[container+0x20]`) after
the thread-affinity setup.

## Windows parity

`hle/d3d12x/device_x.rs:590-601`:

```
deserialize_pso_x(_this, _state, _riid, ppv) -> E_NOTIMPL, *ppv = null
```

with the warning “console pipeline caches are not supported”.
`create_pipeline_state_x` (`:579`) forwards the stream desc to the PC
`CreatePipelineState`; the null device has no PC pipeline, so it hands back a
`Kind::PipelineState` object like the other console PSO slots.

## Fix (`hle/d3d12x_linux.rs`)

- `v[57] = v[67] = Device_DeserializePipelineStateX`: warns once, nulls `ppv`,
  returns `E_NOTIMPL`.
- `v[86] = Device_CreatePipelineStateX`: `out_obj(ppv, Kind::PipelineState)`.

## Tests (RED first)

- `console_deserialize_pso_slots_return_e_notimpl_and_null_the_out`: slots 57
  and 67 with a `0xdead` out pointer. RED: stub returned `S_OK` and left
  `0xdead`; GREEN: `E_NOTIMPL`, null.
- `console_create_pipeline_state_slot86_returns_a_pso`: a zeroed 0x38 desc,
  out in arg4. RED: stub returned 0 with a null out; GREEN: `S_OK`, non-null,
  `Kind::PipelineState`.

## Retail evidence

`/tmp/xcl-step49-r1` (both fixes, debug binary at HEAD):

```
exit 139, no crash report (report writer did not complete)
the 585/42 DeserializePipelineStateX stub lines are gone
last log lines: SetThreadGroupAffinity mask 0xff / 0x3fff / 0x2000
                (fitted to the host's 0x1f00 by kernelx; calls succeed)
remaining stubs: device #21, allocator #9, list #139/#32/#74/#52,
                 device #80, PeekMessageA
```

gdb (`/tmp/xcl-step49-g2`) stops at:

```
PC 0x14501f808   mov rax, [rcx]      rcx = [container+0x20] = 0
  14501f7fc  mov rcx, [rdi + 0x20]
  14501f805  mov rdx, rdi
  14501f808  mov rax, [rcx]          ; null object
  14501f80b  call [rax + 0x50]
```

The container lives on the crashing thread's stack; `[container+0x20]` is a
resource/stream object that was never resolved. Return addresses on the stack:
`0x144dac5fa`, `0x144fac3d7`, `0x141647536`, `0x140f7d342`. The title job
dispatcher crash is gone, so the null job callback was a consequence of the
garbage pipeline out pointers (or at least no longer reproduces first).

### New frontier (Step 49)

Sample the `0x14501f7fc` container: its fields `+0x0..+0x50`, the constructor
that should fill `+0x20`, and the four return addresses above. The thread had
just set affinity masks, so the object may be the title's per-thread resource
cache; identify which HLE call was supposed to create the object and whether
it failed. Then the remaining device #21 (CreateDepthStencilView), allocator
#9 and list #139/#32/#74/#52 stubs should be checked in the same run.

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **886 passed / 0 failed /
  1 ignored** (884 baseline + the two tests).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Retail `/tmp/xcl-step49-r1`: SIGSEGV at `+0x501f808`, no crash report.

## Step 49 — the missing placed resource (device #133)

Date: 2026-11-28. Base: `linux` @ `ebfb2057` (Step 48 docs). Code + test in
one commit, docs in the next.

### Result in one paragraph

The null at `0x14501f7fc` is not a per-thread cache: it is the texture
wrapper's `ID3D12Resource*` (`+0x20`), which the title's renderer fills through
device **#133** `CreatePlacedResourceX` with a console barrier layout. Linux
left #133 a logging stub, so the call returned `S_OK` and never wrote the out
pointer; 33 streamed textures were created with `+0x20 = null`. The render
thread then called `ID3D12Resource::GetDesc` on that null at `+0x501f808`.
Routing #133 to `Device_CreatePlacedResourceLayoutX` (the Windows twin's
`create_placed_resource_layout_x`, `real.rs:1006`, `device_x.rs:1190`) writes
a `Kind::PlacedResource` at the title's VA; the fault is gone. Retail now
reaches the queue's `PresentX` on its render thread; that HLE path overflows
the title's small thread stack inside `vkCreateInstance` (new frontier).

### Evidence

gdb on a fresh root, at `0x14501f7fc` (the `mov rcx, [rdi+0x20]`):

```
resource=0x7ff3a6170ea0 field20=(nil)
  14501f7fc  mov rcx, [rdi+0x20]
  14501f800  lea rdi, [rsp+0x48]
  14501f805  mov rdx, rdi
  14501f808  mov rax, [rcx]          ; rcx = 0
  14501f80b  call [rax+0x50]         ; ID3D12Resource::GetDesc
```

The object is a 0x70-byte texture wrapper (vtable `0x1461ecdb0`, embedded
iface at `+0x50`), built by `0x14501f390`; `+0x38` holds the console
`D3D12_RESOURCE_DESC` copy and `+0x48` the dword `0x43000000` (byte `+0x4b` =
`0x43`, the format the `r14b` branch keys on). All 33 wrappers went through
`0x14193de10`, which calls the title renderer at `0x14193e2d4`:

```
call [r10 + 0x428]                  ; device vtable slot 0x428/8 = 133
  rcx = device (0x1475df048)
  r9d = 6                           ; D3D12_BARRIER_LAYOUT generic read
  [rsp+0x30] = &wrapper+0x20        ; the out pointer
```

Every call logged `TEXDONE eax=0 outval=(nil)`: the generic stub returns
`S_OK` and leaves the out pointer alone. (The run log already named the slot:
`ID3D12Device::#133 first called from game_scarlett_master.exe+0x1e86853`.)

### Fix (`hle/d3d12x_linux.rs`)

- `v[133] = Device_CreatePlacedResourceLayoutX`: console signature
  `(this, va, desc, layout, clear, riid, ppv)`; forwards to
  `Device_CreatePlacedResourceX` (slot #44), the same title-memory resource;
  the null device does not consume the initial state the layout replaces.
- The resource is `Kind::PlacedResource` with `value = va`, so the title's
  `GetDesc` (slot 10) and `GetGPUVirtualAddress` (slot 11) answer it.

### Test (RED first)

`console_placed_resource_layout_slot133_writes_the_resource`: a 256×256
console desc, out in arg7. RED with `v[133]` unmapped: the stub returns
`S_OK` and `slot 133 must write the resource`; GREEN: non-null
`Kind::PlacedResource`, GPU VA == the placed VA, `GetDesc` round-trips.

### Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **887 passed / 0 failed /
  1 ignored** (886 + the new test).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

### Retail after the fix

`/tmp/xcl-step49-r2` (fresh root, no gdb): `+0x501f808` is gone; exit 139.
gdb `/tmp/xcl-step49-i`/`-j`/`-l` (fresh roots) catches the new fault on a
guest render thread (`xcl-game-10155`, stack sizes the title asks for are
`0x8000`–`0x57800`):

```
#0 libvulkan.so.1  0x7ffff5b3286a  movl $0x20, 0x90(%rsp)
#1 libvulkan.so.1  vkCreateInstance
#2 ash::Entry::create_instance
#3 xcl::gpu::recomp::vulkan_present::present_rgba_image (3840x2160)
#4 xcl::hle::d3d12x_linux::Queue_PresentX
#5 game_scarlett_master.exe+0x24824c7   ; the queue's PresentX call
```

`si_addr == rsp + 0x90`, at the floor of that thread's stack mapping
(`fs_base` = TCB; no frames above the title's present call were unwindable),
so `present_rgba_image`'s Vulkan instance creation runs out of the guest
thread's stack. `report.json` completed but wrote no `crash`/`exit_code`
(`/tmp/xcl-step49-r2`, `/tmp/xcl-step49h-r1`).

**Experiment (not committed):** temporarily give guest threads
`stack.max(2 << 20)` in `kernelx_linux::CreateThread`. The present then
succeeds — `D3D12.X Vulkan: presented 3840x2160 (token 0x0)`, the **first
frame** — and retail moves on to SIGSEGV `exe+0x4fb0aec`, fault `0x9896e0`
unmapped, report `exit_code 139` with the crash text. The stub surface is
unchanged: device #21, allocator #9, list #139/#32/#74/#52, device #80,
`PeekMessageA`.

### New frontier (Step 50)

Run the Vulkan presenter (or at least its instance creation) on a stack of
XCL's own, or enforce a host minimum for guest thread stacks, so a title
worker with a 32 KiB/350 KiB reserve can call the HLE present. Then sample
`exe+0x4fb0aec` (fault `0x9896e0`) and the remaining stubs on that path.
