# Retail M1 — guest thread stacks get a host minimum; the first frame presents

Date: 2026-11-28. Step 50. Base: `linux` @ `d9465d5c` (Step 49 docs).
Product code + test in one commit, docs in the next.

## Result in one paragraph

After the Step-49 placed-resource fix, the title's render thread entered
`Queue_PresentX`, which runs HLE and therefore the real Vulkan instance/window
creation on that guest thread. The title had asked for only 0x8000 of stack
(`CreateThread` handles run 0x8000–0x57800), and `libvulkan`'s
`vkCreateInstance` ran off the bottom of it: SIGSEGV with `si_addr = rsp+0x90`
at the stack floor. `CreateThread` now clamps every guest thread's host stack
to `MIN_HOST_THREAD_STACK = 8 << 20`. Retail presents its **first frame**
(`D3D12.X Vulkan: presented 3840x2160 (token 0x0)`) and then faults at
`exe+0x4fb0aec` (fault `0x9896e0`, unmapped), the Step-51 frontier.

## Root cause

The uncommitted experiment from the Step-49 review,
`CreateThread` `stack_size(stack.max(2 << 20))`, was enough to present one
frame. The mechanism: HLE calls execute synchronously on the calling guest
thread, so any deep host-library entry — Vulkan loader/ICD init here,
libX11, DXC below — descends from the guest's stack floor. On Windows the
title's own `dwStackSize` is equally a reserve hint and the loader/ICD
stack use is shallower; on Linux the Vulkan path needs far more than the
console's 32 KiB request.

## Fix (`hle/kernelx_linux.rs`)

```rust
const MIN_HOST_THREAD_STACK: usize = 8 << 20;   // DEC-017

let builder = std::thread::Builder::new()
    .name(thread_name)
    .stack_size(stack.max(MIN_HOST_THREAD_STACK));
```

- Applies to every `CreateThread`, including `stack = 0` (Rust's 2 MiB
  default would also be short) and every larger request (kept as asked).
- Suspended-start gate, TLS attach/detach, FLS exit, id/handle bookkeeping
  and error paths are unchanged. Reservation is virtual only.

## Test (RED first)

`create_thread_stack_has_a_host_minimum` spawns `CreateThread` with
`0x8000` and with `0`, and each probe reads the size of the `/proc/self/maps`
mapping containing a local address into an atomic; both must be at least
`MIN_HOST_THREAD_STACK`.

Parent tree (`d9465d5c` + this test only), throwaway worktree:

```
CreateThread(stack 0x8000) mapped 0xb000 of host stack, below the 0x800000 minimum
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 888 filtered out
```

At HEAD:

```
test hle::kernelx_linux::tests::create_thread_stack_has_a_host_minimum ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 888 filtered out
```

## Gates

- `cargo check -p xcl --tests`: ok.
- `cargo check --workspace`: ok.
- `cargo test -p xcl --lib -- --test-threads=4`: **888 passed, 0 failed,
  1 ignored**.
- `cargo test -p xcl --test launcher_e2e`: **7 passed, 0 failed**.

## Retail evidence

Fresh root `/tmp/xcl-step50-r1` (debug binary at HEAD, no gdb): exit 139,
crash report written.

```
xcl: console descriptor heap: 0x2600000 bytes from the title's memory at 0x1000000000
xcl: console display target 3840x2160 format 28: 0x1ff8000 bytes ... at 0x100bc00000
xcl: console display target 3840x2160 format 28: 0x1ff8000 bytes ... at 0x100dc00000
xcl: [STUB] d3d12_xs.dll!ID3D12Device::#19 ... +0x1647616
... 22 first-call stub lines (same surface as Step 49)
vulkan: Intel(R) Arc(tm) A770 Graphics (DG2) subgroupSize 32 ... -> split-32
xcl: D3D12.X Vulkan: presented 3840x2160 (token 0x0)
xcl: memory: process commit 18.1 GiB ...; graphics 0.1 GiB, Windows 4.5 GiB,
     heaps 0.0 GiB, physical pages 7.0 GiB
crash: SIGSEGV (signal 11) at game_scarlett_master.exe+0x4fb0aec;
       pc 0x144fb0aec; fault address 0x9896e0: unmapped
       (code bytes: ff 50 60 4d 8b 37 4d 8b 67 08 48 8d 4c 24 20 48)
```

- The `vkCreateInstance` stack SIGSEGV is gone; the first frame presents.
- No regression: Step 42's 38 MiB heap is at `0x1000000000`, both Step 43
  display targets are at `0x100bc00000`/`0x100dc00000` with 0x1ff8000 bytes,
  and device #133 is not a stub (Step 49 placed resources stay routed).
- The 22 stub lines match the Step-49 inventory: device #19/#20/#21/#24/
  #61/#70/#78/#79/#80, allocator #9, graphics list #32/#52/#74/#131/#132/
  #139, IDXGIOutput #21, `PeekMessageA`, `ShowWindow`, ws2_32 #115,
  `IGameInput::#9`, `LocaleNameToLCID`.

## Next frontier (Step 51)

`exe+0x4fb0aec` faults at `0x9896e0`; its first bytes are `ff 50 60`
(`call qword ptr [rax+0x60]`), so the call target is an offset-shaped
pointer near `0x989680` rather than a mapped table. Step 51 samples the
registers and callers at that pc and routes the responsible console slot,
or records an honest negative.
