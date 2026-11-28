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
