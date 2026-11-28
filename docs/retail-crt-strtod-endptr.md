# Retail Step 52 — `strtod`/`_strtoui64` end pointers

Retail (Step 51, `012038f1`) presents the first 3840x2160 frame, loads the Bink
intro movie, and then SIGSEGVs at `game_scarlett_master.exe+0x5270e50`. gdb on
a fresh root showed the crash consumer:

```
145270e30  push rsi/rdi/rbx; sub rsp,0x30
145270e37  test rcx,rcx; je ...
145270e3c  lea rdi,[rsp+0x28]        ; endptr
145270e44  mov rdx,rdi
145270e47  call [0x146794750]        ; strtod (generic auto-stub, returns 0)
145270e4d  mov rcx,[rdi]             ; *endptr == 0
145270e50  cmpb $0x23,(%rcx)         ; SIGSEGV at 0
```

`strtod` and `_strtoui64` had no handler in `hle/crt_linux.rs`, so the resolver
installed the generic auto-stub: it returns 0 and never writes the caller's
`char **endptr`. The title parses a `#…` colour/config string and dereferences
the null end pointer.

## Fix

`hle/crt_linux.rs` now exports both names with C-locale UCRT semantics.

`strtod` scans the longest valid subject sequence and stores `nptr`/`nptr+used`
in `*endptr` (no write on no conversion, so the caller's pointer is preserved):

- optional C-locale whitespace (`0x09..=0x0D`, `0x20`) and one sign;
- decimal: digits with an optional `.` and an optional `e`/`E` exponent that is
  consumed only when at least one digit follows (`1.e5`, `.5`, `5.` accepted);
- hexadecimal: `0x` with hex digits before/after an optional `.`, at least one
  digit total, plus an optional `p`/`P` binary exponent. An incomplete prefix
  (`0x`, `0x.`) is not part of the subject sequence; the decimal fallback then
  consumes just `0` (C behaviour), leaving `endptr` at `x`;
- `inf`/`infinity` and `nan`/`nan(payload)` (case-insensitive);
- overflow to infinity and underflow to zero from a nonzero mantissa set
  `errno = ERANGE (34)`; the token is still consumed.

`_strtoui64(nptr, endptr, base)`:

- base 0 auto-detects `0x`/`0X` (consumed only before a hex digit) and a
  leading `0` as octal; explicit bases 2–36; base 16 accepts but does not
  require the `0x` prefix;
- `-` negates the magnitude modulo 2^64, so `-1` returns `ULLONG_MAX`;
- magnitude above `u64::MAX` returns `ULLONG_MAX` with `ERANGE`, after
  consuming every digit;
- a base outside 0/2–36 is `EINVAL (22)` with no conversion;
- no digits leaves `*endptr` at the caller's pointer.

`parse_hex_float` (used by `%a` and now `strtod`) parses the binary exponent
with saturation instead of `i32::parse`, so `0x1p999999999` yields infinity
rather than "no conversion".

## Tests (RED first)

New `strtod_and_strtoui64_write_their_end_pointers` resolves both names through
`exports()` and drives the exact consumer shapes. At the parent (`012038f1`)
with only the test added, in `/tmp/xcl-step52-red`:

```
thread '...strtod_and_strtoui64_write_their_end_pointers' panicked:
strtod must be wired
test result: FAILED. 0 passed; 1 failed; 0 ignored; 891 filtered out
```

At the commit the same test covers decimal/hex/inf/nan end offsets, no
conversion (`0xzz`, `+#ff`), null `endptr`, `1e999` + ERANGE, base 0
`0x1f`/`0777`, bases 2/36, `-1`, `u64::MAX` vs `u64::MAX+1`, `0x` without a
hex digit, and invalid base 37 (EINVAL).

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **890 passed / 1 failed /
  1 ignored**; the failure is the known global-VFS flake
  `mount_with_ui_async_does_not_leak_mount_on_invalid_or_aborted_call`, which
  passes alone (`1 passed; 0 failed`), as in the Step-44/49 reviews.
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.

## Retail evidence

Fresh root `/tmp/xcl-step52-r1` (debug binary at HEAD, no gdb): exit **139**.

- `D3D12.X Vulkan: presented 3840x2160 (token 0x0)` — the first frame still
  presents.
- Both CRT stubs are gone from the log: no `strtod` / `_strtoui64` lines, and
  `exe+0x5270e50` does not occur.
- The crash report writer now **completes**: `report.json` has
  `exit_code: 139`, a non-null `crash`, and 26 `called_stubs` entries.
- New frontier: `SIGSEGV at game_scarlett_master.exe+0x25a087d`
  (pc `0x1425a087d`), fault address `0x7fa4b708f0f4` (unmapped), code
  `46 8b 14 08 45 21 c2 41 09 ca 46 89 14 08 44 8b`
  (`mov r10d,[rax+rcx]; and r10d,r8d; or r10d,ecx; mov [rax+rcx],r10d`).
- 26 first-call stub lines: the Step-49 d3d12 surface plus four Bink entries,
  now `isalnum` (`exe+0x274943d`, returns 0) and
  `ID3D12CommandQueue::#31` (`exe+0xf2cc86`).
- No regression: first frame, Step-42 heaps, Step-43 display targets, Step-49
  placed resources, Step-51 fence/queue Signal.

### New frontier (Step 53)

Sample `exe+0x25a087d` (one instruction earlier; software breakpoints after
`loader::run_main`) for `rax`/`rcx` and the caller chain, and decide whether
the unmapped `0x7fa4b708f0f4` is a truncated/immediately-freed host pointer or
a guest buffer the preceding `isalnum`/queue #31 stubs failed to create.
