# Retail M1 — the post-present fault: console queue `Signal` at #16

Date: 2026-11-28. Step 51. Base: `linux` @ `22d94984` (Step 50 docs).
Task `code-assist:xcl-first-frame:step-51:post-present-fault`. Product code
+ tests in one commit, docs in the next.

## Result in one paragraph

After Step 50 the title presents its first frame and then SIGSEGVs at
`exe+0x4fb0aec` (`call [rax+0x60]`, fault `0x9896e0`). A hardware watchpoint on
the object shows the writer is XCL itself: the title called **console queue
slot #16 with `(fence, value)`**, the newer console layout's `Signal`, but Linux
had the PC order's `GetTimestampFrequency` there and wrote `10_000_000` over
the fence object's vtable. The title's submission tracker later calls that
vtable slot (`[fence]+0x60`, console fence #12 = completed-value address) and
faults. Queue #16/#17 now dispatch on the fence registry (`Signal` / `Wait`),
with the PC timing queries preserved for the older call shape, and console
fence #12 returns the address of the fence's own `value` (the null GPU
completes instantly). Retail now passes the present, loads the Bink intro
movie, and reaches `exe+0x5270e50`, where the `strtod` stub left `*endptr`
zero (new frontier).

## Protocol (gdb sample on a fresh root)

Break after `loader::run_main`, `set language c`, guest software breakpoints,
`handle SIGSEGV/SIGBUS/SIGUSR2 nostop noprint pass`. The decisive probe is a
hardware watchpoint on the first fence object created at
`exe+0x141d0b300` (the return of the title's `CreateFence` call):

```
first fence object=0x7ff26c3e66a0 vtbl=0x7ff26c414a70
Hardware watchpoint 5: *(unsigned long long*)$fence
...
=== STOP pc=0x5555562f0f8f fence0=0x989680
#0 xcl::hle::d3d12x_linux::Queue_GetTimestampFrequency
     (_t=0x7ff26c3f7a70, f=0x7ff26c3e66a0) at d3d12x_linux.rs:2428
   0x5555562f0f88: mov    (%rsp),%rax
   0x5555562f0f8f? movq   $0x989680,(%rax)      ; *f = 10_000_000
#1 0x000000014248260e in ?? ()
```

The caller is `exe+0x2482608`:

```
1424825e0  mov rdx, [rsp+0x98]      ; the tracker's fence
1424825e8  mov rsi, [rsp+0xa0]      ; the value
1424825ff  mov rax, [r15]           ; queue vtable
142482602  mov rcx, r15             ; queue
142482605  mov r8,  rsi
142482608  call qword ptr [rax+0x80]  ; console queue #16
```

`0x80/8 = 16`, and the argument shape is `Signal(fence, value)`, not
`GetTimestampFrequency(out)`. The fence object (IID `0a753dcf-c4d8-4b91-adf6-
be5a60d95a76` = `ID3D12Fence`, `d3d12_layout.rs:52`) had a valid vtable at
creation; after the call its first qword was `0x989680`, so the tracker's
later `call [fence+0x60]` used `0x9896e0` as a function address.

Why #16: the consumer is the newer console layout — two console methods sit
before `Signal`, so `Signal #16`, `Wait #17`, `GetTimestampFrequency #18`,
`GetClockCalibration #19`, `GetDesc #20` (already handled at #18–#20 since
Step 48). Linux kept the PC order at #16/#17 (`GetTimestampFrequency` /
`GetClockCalibration`) and therefore wrote a u64 over the fence. The tracker's
next call is console **fence #12**, the completed-value address the title polls
with `monitorx`/`mwaitx` (Windows `hle/d3d12x/fence_x.rs`); Linux left it a
stub returning 0, which the same call site dereferences.

## Parity

- Windows `hle/d3d12x/real.rs:8244-8336` (`queue_dispatch`): the newer layout
  has `Signal #16`, `Wait #17`, `GetTimestampFrequency #18`,
  `GetClockCalibration #19`; a call at #16/#17 whose first argument is a title
  fence selects the shifted layout. `is_fence` uses a registry, exactly as the
  Linux `FENCES` set does here.
- Windows `hle/d3d12x/fence_x.rs:118-146` (`completed_value_address`): fence
  #12 returns the address of a stable cell kept equal to the fence's completed
  value. The null Linux GPU completes synchronously, so the `value` field the
  signals already update is that cell (`AtomicU64::as_ptr`), with no follower
  thread.

## Fix (`hle/d3d12x_linux.rs`)

- `static FENCES: LazyLock<Mutex<HashSet<usize>>>`; `Device_CreateFence`
  records every fence it creates, and `is_title_fence` tells the console
  Signal/Wait shape from the PC out-pointer shape without dereferencing the
  timing out-parameter.
- Queue vtable: `v[16] = Queue_Slot16` (`Signal` for a registered fence, else
  `GetTimestampFrequency`), `v[17] = Queue_Slot17` (`Wait` for a fence, else
  `GetClockCalibration`). `v[14]`/`v[15]` keep the older layout's
  `Signal`/`Wait`; `#18–#20`, `#32`, `#35` are unchanged.
- Fence vtable: `v[12] = Fence_CompletedValueAddress`, returning
  `(*this).value.as_ptr()`.

## Tests (RED first)

At HEAD (`22d94984`) with only the new tests added:

```
console_queue_signal_slot16_does_not_overwrite_the_fence:
  assertion `left == right` failed: console #16 is Signal
    left: 2      ; the fence's initial value, untouched by the frequency write
   right: 7
1 failed / 890 filtered

fence_slot12_is_the_completed_value_address:
  fence #12 must expose the completed value address
1 failed / 890 filtered
```

The first test creates the fence through console device #36, records the
vtable pointer, calls queue #16 with `(fence, 7)` and asserts the fence value
is 7 **and the vtable pointer is unchanged**, then repeats the same slots with
the PC shapes (a u64 out → 10 MHz; two u64 outs → equal GPU/CPU stamps). The
second asserts slot 12 returns a non-null cell equal to the fence's initial
value, follows queue `Signal(7)` and fence `Signal(9)`.

GREEN at the commit: both `1 passed; 0 failed` (890 filtered).

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **890 passed / 0 failed /
  1 ignored** (888 baseline + 2 tests).
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Retail evidence

Fresh root `/tmp/xcl-step51-r1` (debug binary at HEAD, no gdb): exit **139**.

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 2.269 s — the first
  frame still presents.
- `+0x4fb0aec` / fault `0x9896e0` is **gone** (0 occurrences in the log): the
  fence vtable is no longer overwritten and fence #12 answers the poll.
- The title then loads the intro movie: `bink2scarlett.dll` first calls
  `BinkSetOSFileCallbacks`, `BinkSetMemory`, `BinkStartAsyncThread`,
  `BinkSetSoundSystem`; `QueryApiImpl 3e241977-… → XClosed: missing` and
  `XErrorReport 0x89240107: XGameRuntime is missing required dependencies`; the
  last calls are `strtod` (`+0x5270e4d`) and `_strtoui64` (`+0x4061dce`).
- 28 first-call stub lines: the Step-49 d3d12 surface (`device
  #19/#20/#21/#24/#61/#70/#78/#79/#80`, allocator #9, list
  #131/#132/#139/#32/#74/#52, `IDXGIOutput::#21`, `PeekMessageA`,
  `ShowWindow`, ws2_32 #115, `IGameInput::#9`, `LocaleNameToLCID`) plus the
  four Bink entries, `strtod` and `_strtoui64`.
- No regression: the Step-42 descriptor heaps, the two Step-43 3840x2160
  display targets and the Step-49 placed resources are unchanged.
- The report writer did **not** complete: `report.json` has `exit_code: null`,
  `crash: null` and an empty `called_stubs` array (the fatal SIGSEGV ran the
  Linux reporter, not the full report path).

### New frontier (Step 52): `strtod` must write `*endptr`

gdb on a fresh root (`SIGSEGV stop`): pc `0x145270e50` (`exe+0x5270e50`),
fault `0x0`, `cmpb $0x23,(%rcx)` with `rcx = [rdi] = 0`. The function

```
145270e30  push rsi/rdi/rbx; sub rsp,0x30
145270e37  test rcx,rcx; je ...
145270e3c  lea rdi,[rsp+0x28]        ; endptr
145270e44  mov rdx,rdi
145270e47  call [0x146794750]        ; strtod (XCL stub, returns 0)
145270e4d  mov rcx,[rdi]             ; *endptr == 0
145270e50  cmpb $0x23,(%rcx)         ; SIGSEGV at 0
```

The `strtod` stub returns 0 without writing the `char **endptr`, so the title
(parsing a `#…` colour/config string) dereferences null. `strtod` and
`_strtoui64` are now the consumer-visible inputs; both must write their end
pointer (and `_strtoui64` the value) with C-locale UCRT semantics.

## Sampling log (bounded)

All gdb runs were debug-binary fresh roots under `gdb -q -batch` with
software breakpoints after `loader::run_main` and `set language c`;
`/tmp/xcl-step51-g1`…`g9`. g1 established `rax=0x989680` at `+0x4fb0aec`;
g2–g6 followed the object to `t[0]+0x10` / `arena+0x10` = the first fence and
rejected the queue #18 and `QueryPerformanceFrequency` out pointers;
g7–g8 (`h`) set the hardware watchpoint and caught XCL's
`Queue_GetTimestampFrequency` store at `0x5555562f0f8f`; g9 recorded the new
frontier. The non-gdb retail gate is `/tmp/xcl-step51-r1`.
