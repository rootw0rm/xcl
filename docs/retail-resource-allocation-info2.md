# Retail M1 — device #108 allocation info and the newer console queue layout

Date: 2026-11-28. Steps 47–48. Base: `linux` @ `1c528701` (Steps 45–46 docs).
Product code: two commits (device #108, queue layout).

## Result in one paragraph

The `+0xabfe1c` assert was the 16-byte out struct of console device #108
(`GetResourceAllocationInfo2`) left uninitialised by the Linux stub. Routing
it to a null-device handler that walks the 0x48-byte `RESOURCE_DESC1` array
and writes `{SizeInBytes, Alignment}` (plus per-resource offsets) clears the
assert. The title then calls console queue #18 with an out pointer and
expects `GetTimestampFrequency`; Linux's queue vtable had the PC order there
(`#18` = GetDesc), so the failure path tore down the title's thread context
and the next dereference crashed. The newer console queue layout
(#18 frequency, #19 clock, #20 GetDesc, #32 PresentX, #35 ExecuteCommandListsX)
now passes, and retail reaches the title's job dispatcher
(`game_scarlett_master.exe+0x247a098`, SIGSEGV on a null task pointer) as its
next frontier.

## Step 47 — device #108 `GetResourceAllocationInfo2`

Frontier: `0x140abfe1b` int3s unless the 16-byte result of the device call at
`[vtable+0x360]` (slot 108) has a zero high word:

```
140abfdf3  lea rdi, [rsp + 0x40]
140abfe07  call qword ptr [rax + 0x360]   ; #108, rcx = out, rdx = device
140abfe0d  vmovups xmm0, [rdi]
140abfe11  vmovups [rsi], xmm0            ; 16-byte allocation info
140abfe15  cmp dword ptr [rsi + 4], 0
140abfe1b  int3
```

Windows maps #108 to `x::get_resource_allocation_info2_x`
(`real.rs:1004`, handler `device_x.rs:1039`): hidden return first, then the
device, mask, count, an array of 0x48-byte `D3D12_RESOURCE_DESC1`, and an
optional `D3D12_RESOURCE_ALLOCATION_INFO1[]`. Linux had it as a logging stub,
so the stack buffer stayed garbage.

Fix (`hle/d3d12x_linux.rs`): `Device_GetResourceAllocationInfo2` resolves
`this`/`ret` with `this_and_ret`, rounds each descriptor's `approx_size` to
64 KiB, writes `{offset, alignment, size}` per resource to the last argument,
and writes `{total, 0x10000}` through the hidden return pointer. `v[108]` is
routed to it.

Test `console_resource_allocation_info2_writes_offsets_and_total`: two
0x48-byte buffer descs (widths 0x12345 and 0x20), a pre-filled 16-byte output
and info array. RED before the route returned 0 (stub); GREEN now returns the
hidden buffer, total 0x30000, alignment 0x10000, offsets 0 / 0x20000.

Retail after fix 47 (`/tmp/xcl-step47-r1`): the `+0xabfe1c` SIGTRAP is gone;
exit 139 SIGSEGV at `game_scarlett_master.exe+0x2484133` (`mov rcx,[rbx]`,
rbx = TLS+0x12a8, null).

## Step 48 — newer console queue layout

gdb at `0x1424841a6` (the call behind the crash) on the Step-47 binary:

```
this (queue) = 0x7ff27c3f7890
rdx = 0x7ffff5de7958   (out buffer)
r8  = 0x99c   r9 = 0
call [queue vtable + 0x90]   ; slot 18
after: eax = 0x7ffff5de7958 (the hidden-buffer pointer), [out] = 0
```

The caller (`0x142484180`) checks the return as an HRESULT and the out value
as nonzero, then computes `1 / value` (a clock frequency). That is the
**newer console queue layout**: #18 `GetTimestampFrequency`, #19
`GetClockCalibration`, #20 `GetDesc`; #32 `PresentX`, #35
`ExecuteCommandListsX` (`real.rs:8301-8367`, `QUEUE_SHIFTED`). Linux had the
PC order at #18 (`Hidden_ZeroDesc16`), so the return was a pointer and the
out was zero: the failure path at `0x1424841ff` ran, then `[TLS+0x12a8]`
(already null on that thread) faulted.

Fix (`hle/d3d12x_linux.rs`):
- `v[18] = Queue_GetTimestampFrequency` (10 MHz, HRESULT + u64 out),
  `v[19] = Queue_GetClockCalibration`, `v[20] = Hidden_ZeroDesc16`;
- `v[32] = Queue_PresentX` (null-safe: marks the flip, no Vulkan on the null
  queue), `v[35] = Queue_ExecuteCommandListsX` (delegates the lists to the
  null queue's #10 executor; the extra console value word is ignored).

Test `console_queue_timestamp_frequency_and_clock_calibration`: slot 18
writes 10,000,000 and returns `S_OK`; slot 19 writes equal GPU/CPU stamps.
RED before the remap: slot 18 returned the hidden-buffer pointer and left the
frequency zero.

## Retail after both fixes

`/tmp/xcl-step48-r1` (queue #18–#20) and `/tmp/xcl-step48-r2` (#32/#35):
both exit 139 with **no crash report** (the report writer did not complete).
gdb stops at:

```
PC 0x14247a098   mov rax, [rbx]   rbx = 0
  rbx = [job + 0x40] = 0     (title job dispatcher, tasks 0x142479f..)
  r12 = 0x7ff39fecec40
  threads 14–18 (xcl-game-*) all at the same PC
```

The title's job/task system dispatches a task with a null callback. The new
stub surface before the crash: device #57 x585, #67 x42, #86 x6, #61 x2,
#24 x2, #21; CommandAllocator #9; list #139/#32/#74/#52; queue #35/#32 now
routed; device #80 `WaitFrameEventX` still a stub. The #57/#67
(`deserialize_pso_x`), #86 (`create_pipeline_state_x`), #61
(`create_sampler_x`) and #24 (`copy_descriptors_simple_x`) handlers are the
next Windows-parity slice (`real.rs:977,985-989,1003`); whether they also
explain the null job callback must come from a second fresh-root run after
they are routed.

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **884 passed / 0 failed /
  1 ignored** (882 baseline + #108 + queue-layout tests).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Retail: `/tmp/xcl-step47-r1` (SIGSEGV `+0x2484133`),
  `/tmp/xcl-step48-r1`, `/tmp/xcl-step48-r2` (job dispatcher `+0x247a098`);
  no crash report in the last two.
