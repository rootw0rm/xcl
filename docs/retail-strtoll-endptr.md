# Retail Step 65 — `strtoll`/`strtoull` end pointers and `errno`

Retail (Step 64, `46df1a43`) passes the TLS certificate init, presents its
frames and then SIGSEGVs at `game_scarlett_master.exe+0x18e10c7`. The caller at
`exe+0x1418e1060` parses a numeric resource string:

```
1418e1060  push rsi/rdi/rbx; sub rsp,0x30
1418e106a  xor ecx,ecx
1418e107b  call [0x146794538]        ; _set_errno(0)
1418e1081  cmp byte ptr [rbx],0x30   ; "0"?
1418e1084  je 0x1418e10d9            ;   "0x" -> hex path
1418e1086  lea rdx,[rsp+0x20]        ; endptr
1418e108e  mov r8d,0xa
1418e1094  call [0x146794768]        ; strtoll(str,&endptr,10)  (auto-stub)
1418e109a  ...
1418e10bd  mov rax,[rsp+0x20]        ; *endptr == 0
1418e10c7  cmp byte ptr [rax],0x0    ; SIGSEGV at 0
```

`objdump -p game_scarlett_master.exe` pins the IAT slots (RVA + 0x140000000):
`0x146794538` `_set_errno`, `0x1467944d8` `_get_errno`, `0x146794768` `strtoll`,
`0x146794778` `strtoull`. None had a handler, so the resolver installed the
generic auto-stub: it returns 0 and never writes the caller's `char **endptr`.
The `0x` branch calls `strtoull(str + 2, &endptr, 16)` at `0x1418e10f7` and
joins at `0x1418e10ba`; the no-digits path at `0x1418e1118` reads `errno` back
through `_get_errno` and compares it with `0x22` (`EINVAL`), while the caller
itself treats `ERANGE (34)` as "out of range" at `0x1418e1121`.

## Fix

`hle/crt_linux.rs` now exports all four names with UCRT semantics.

The unsigned scanner was split so both consumers share one prefix/whitespace
pass and one digit loop:

- `scan_int_magnitude(input, base)` returns `(bytes consumed, negative,
  magnitude: u128, magnitude > u64::MAX)`; base 0 auto-detects `0x`/leading
  `0` as octal, explicit base 16 accepts but does not require `0x`, a bad base
  is rejected by the callers before scanning.
- `scan_int_digits` keeps the exact magnitude in `u128` (saturating only far
  past any 64-bit limit) so `strtoll` can test `LLONG_MAX`/`LLONG_MIN` without
  a second parse.
- `scan_strtoull` is the old `_strtoui64` contract on top: `-` negates modulo
  2^64, overflow returns `U64_MAX` and reports `ERANGE`.
- `strtoull` forwards to `_strtoui64`; `strtoll` parses the same subject
  sequence, returns `LLONG_MAX` (positive) or `LLONG_MIN` (negative) with
  `ERANGE` one past the signed limit, `0`/`EINVAL` with `*endptr = nptr` for a
  base outside 0/2–36, and `*endptr = nptr + used` on every successful
  conversion (no digits leaves the caller's pointer untouched).
- `_set_errno(value)` stores the thread-local `errno` and returns 0;
  `_get_errno(value)` copies it out and returns 0, or `EINVAL (22)` without a
  write for a null pointer. Both use the existing per-thread `ERRNO_CELL`.

## Tests (RED first)

New `strtoll_strtoull_and_errno_write_their_outputs` resolves the four names
through `exports()`, then drives the caller's shapes: `" -42rest"` (decimal),
`"1f"` and `"0x1f"` (base 16), both signed limits, `LLONG_MAX+1` /
`LLONG_MIN-1` with `ERANGE` and full digit consumption, no conversion
(`"+#ff"`), invalid base 37 (`EINVAL`), `strtoull` parity (`0x1f`, `-1`,
`ULLONG_MAX+1`, `xyz`, `0x`), and the `_set_errno`/`_get_errno` round trip
including `_get_errno(NULL) == 22`.

With only the test added at the parent tree the run is RED:

```
thread '...strtoll_strtoull_and_errno_write_their_outputs' panicked:
strtoll must be wired
test result: FAILED. 0 passed; 1 failed; 0 ignored; 913 filtered out
```

At the commit it is GREEN (`1 passed; 0 failed`), and the Step-52
`strtod_and_strtoui64_write_their_end_pointers` still passes.

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **913 passed / 0 failed /
  1 ignored**. (The default-parallel run flaked the known global-VFS overlay
  and mutex-owner tests; each passes alone, as in earlier steps.)
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Retail evidence

Fresh root (debug binary at HEAD, no gdb): exit **133** (SIGTRAP), first frame
`D3D12.X Vulkan: presented 3840x2160 (token 0x0)` at 6.75 s, crash
`SIGTRAP at game_scarlett_master.exe+0x56abfa6` at ~11.2 s.

- `+0x18e10c7` is gone; `strtoll`, `strtoull`, `_set_errno`, `_get_errno`
  appear in no stub line (48 `called_stubs` are recorded instead of the
  Step-64 45, with the title now reaching `erff`, `log2f`, `_fdtest`,
  `strncpy_s`, the D3D12 command-queue slots and `XboxBvhBuilder`).
- The report writer completes: `exit_code: 133`, non-null `crash`, 48 stubs.
- The title gets further along the resource/config path: the new trap is the
  title's own fail-fast, not a bad-pointer dereference.

### New frontier (Step 66)

`full.asm` shows the reported pc is the recovery `jmp` after an `int3`:

```
1456abf90  sub rsp,0x28
1456abf94  mov dl,1
1456abf96  call 0x140075230
1456abf9b  test rax,rax
1456abf9e  je 0x1456abfa5
1456abfa0  add rsp,0x28; ret
1456abfa5  int3                     ; SIGTRAP; report pc = 0x1456abfa6
1456abfa6  jmp 0x1456abfa0
```

So `0x140075230` returned 0. That entry jumps to `0x140075440`, which with
`dl=1` calls `0x14221ad20(0x100)`; the non-`dl` path uses the global at
`0x148420808`. `0x1456abf90` has three static callers: `0x14022cb16`,
`0x145b1a71a`, `0x145c0c469`. Sampling that helper (which caller, the
`0x14221ad20` return, and the object in `rcx`) is the next bounded step; the
`0x1418e1060` parse callers are `0x144062c56`, `0x14406334d` and
`0x145537343` (`0x145537290` via the `0x1418e1050` thunk).
