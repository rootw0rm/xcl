# Retail frame clock, queue #31 and allocator #9

Step 77. Base `c7bd2a5d` (Step 76). One plain fresh-root retail run, no gdb.

## Retail input

`/tmp/xcl-step76-r1` (42 called stubs) listed the remaining frame-path
calls:

| slot | calls | first caller | report arguments (r8/r9) |
|---|---|---|---|
| device `#78` | 3 | `exe+0x247abde` | `0x8235 0x3` |
| device `#79` | 4 | `exe+0x247ac02` | `0x0 0x0` |
| device `#80` | 108 | `exe+0x248244e` | `0x0 0x0` |
| device `#81` | 0 | — | — |
| queue `#31` | 13 | `exe+0xf2cc86` | `&stack 0x7f4847c00000` |
| allocator `#9` | 108 | `exe+0x188501` | `0x7f4cdade47c0 0x1440` |

`#78` runs 3 times at 0.182 s with `length = 0x8235` (33,333 µs, 30 Hz)
and `periodicity = 3`; `#79` follows at 0.182 s. The first `#80` is at
6.795 s, just before the first present at 6.833 s; one `#80` per presented
frame (108 calls, 134 presents in that run). `#81` is never called.

The frame slots are reached through the title's display/frame helper
(`exe+0x247a00`–`exe+0x248400`; that helper's queue #35 submission is
documented in `docs/retail-list-per-frame.md`). A direct
`call [device + 0x270/0x278/0x280/0x288]` does not appear in the helper:
the retail calls arrive through the title's indirect dispatch, so the
report's `first_caller` is the only static anchor. The argument shapes
(`out, 33333, 3`; `type, offset, ancillary, flags`; `type, timeout,
ancillary, flags, token`) match the Windows twin's `#78`–`#81` signatures.

## Decision: record-only clock on the null backend

The Linux frame twin (`hle/d3d12x_frame_linux.rs`) is backend-free, but
`wait_frame_event` blocks on `clock.fired[ty]` until the next timer tick
(the shared clock thread) or the timeout. The null device starts no
clock thread, and its only present path (`Queue_PresentX`, queue `#32`)
consumes a token: it is not an event source that could drive the clock.
Routing the blocking adapter there would make every `#80` sleep for a
frame that nothing fires — up to the title's timeout per frame.

Chosen: **(b), a record-only clock that never blocks**, matching the
measured public-HLE behaviour of `WaitFrameEventX` (returns at once with
a fresh non-zero token before a presenter exists):

- `#78 SetFrameIntervalX` records the interval with the twin's
  validation (`E_INVALIDARG` off-display, length 0 stops, `S_FALSE`
  otherwise).
- `#79 ScheduleFrameEventX` records the type/offset/ancillary handles
  with the twin's validation (`E_INVALIDARG` without an interval, type
  >= 3, nonzero type before the origin, unscheduling the origin) and
  returns `0x10000000` when it starts the schedule, else `S_FALSE`. No
  clock thread is started.
- `#80 WaitFrameEventX` applies the same schedule/token gate and returns
  `S_OK` immediately with a fresh origin token (`S_FALSE` is never
  returned). Types 1/2 keep the same fire gate.
- `#81 GetFrameStatisticsX` is the same adapter as the Vulkan device:
  origin/flip of a flipped token, `E_FAIL` with `count = 0` otherwise.

The Vulkan device keeps the real timer clock (`Frame_WaitFrameEventX`).
The null device uses `Null_ScheduleFrameEventX` / `Null_WaitFrameEventX`;
`#78` and `#81` are shared. Because `#80` now writes the token back, the
retail presenter carries it: this run's PresentX tokens are `0x1 … 0x85`
(Step 76: all `0x0`), so `#81` has a real token to name once the title
calls it.

## Honest negatives

### queue `#31`

`exe+0xf2cc7f` calls `[queue+0xf8]` with `(count, pointer_array, qword,
u32, 0)`; the pointer array is built at `exe+0xf2cbfb` from
`([r15+8*i+4] << 16) + base`. The entries are GPU virtual addresses, not
COM interfaces: this is not a command-list submission (dereferencing one
as a list would fault in the PC queue's `QueryInterface`). The Windows
twin leaves `#31` unmodelled too, and its console method name is not
measured. It stays the logging stub answering `S_OK`; the measured shape
is pinned by `queue_31_and_allocator_9_stay_unmodelled_stubs`.

### allocator `#9`

`0x1401884fe` calls `[allocator+0x48]` on an XCL `ID3D12CommandAllocator`:

- `rdx = 0x14555df10`, which the plan called an IID: it is **inside
  `.text`** (`.text` is `0x140001000..0x145f3f966`), a function pointer
  that allocates for the title's XMem hook table;
- `r8 = rsi` (the title's owning object), `r9 = index * 0x30` (the
  hook-table entry offset); the return is ignored (`0x188501` continues
  into `call 0x144e9b520`).

The public `ID3D12CommandAllocator` chain is slots 0–8; `#9` is past it
and its console semantics are not measured. It stays the logging stub
answering `S_OK`, like the Windows twin.

## Tests

- `null_device_frame_clock_records_and_never_blocks` (d3d12x_linux):
  pins all four null slots as routed (not stubs); `#78` refuses an
  off-display interval and accepts 16,667 µs; `#80` refuses before `#79`;
  `#79` schedules; `#80` with a 5 s timeout returns in < 500 ms with a
  non-zero token; `#81` refuses before a flip, then reports origin/flip
  after `mark_flipped`; type 1 unscheduled; interval 0 stops the clock.
- `record_only_clock_never_blocks` (d3d12x_frame_linux): the frame core
  validation, the immediate wait, re-stamping a known token, refusing an
  unknown one, type 1, and the unschedule/stop rules.
- `queue_31_and_allocator_9_stay_unmodelled_stubs`: pins both logging
  stubs and calls the measured shapes (a VA batch; a code pointer, owner
  and 0x30 offset) without a dereference or fault.
- `vulkan_frame_clock_and_presentx_route_behind_the_backend` keeps the
  blocking Vulkan clock test; its null-slot assertion now expects the
  routed slots.

RED: the new null-device test at the parent tree failed at the first
assertion, `null slot 78 is routed` (0 passed / 1 failed). GREEN:
focused `hle::d3d12x_frame_linux::tests` 5/0,
`null_device_frame_clock` 1/0, `queue_31_and_allocator_9` 1/0, module
`hle::d3d12x` 73/0.

Full gates at this tree: `cargo check -p xcl --tests` 0 errors,
`cargo check --workspace` 0 errors, lib **949 passed / 0 failed / 1
ignored** at `--test-threads=4`, `launcher_e2e` **7 passed / 0 failed**.

## Retail delta

ONE plain fresh-root debug run, `/tmp/xcl-step77-r1`, same title/config
as Step 76:

- exit **133** SIGTRAP `game_scarlett_master.exe+0x2219651` — the
  Step-67 title-side allocator trap, unchanged;
- first frame `3840x2160` at **6.902 s**, **133** presented frames, last
  at 11.902 s (Step 76: 6.833 s, 134 presents, last 11.917 s). The
  5.0 s present window and the first-present latency are unchanged:
  no stall and no throttling, the one-frame delta is run noise;
- PresentX tokens are now `0x1 … 0x85`, i.e. the title consumes the
  token `#80` writes (Step 76: every token was `0x0`);
- **39** called stubs (Step 76: 42): `ID3D12Device::#78`, `#79`, `#80`
  are gone (`#81` was never called); no new stub;
- the honest negatives remain: allocator `#9` 108 calls, queue `#31`
  13 calls, list `#75/#126/#142` 107 each;
- no new warning from the routed handlers.

Next input from the plan: the CRT math stubs (`erff` 4,800, `atan2` 84,
`fmodf` 52, `log2f` 129) — semantic correctness only; the allocator-OOM
frontier stays parked on its console-sample trigger
(`docs/retail-pool-fill.md`).
