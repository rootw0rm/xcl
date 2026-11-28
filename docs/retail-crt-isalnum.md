# Retail Step 53 — `isalnum` in the identifier scanner

Date: 2026-11-28. Step 53. Base: `linux` @ `05bbab1e` (Step 52 docs).
Task `code-assist:xcl-first-frame:step-53:crt-isalnum`. Product code + test in
one commit, docs in the next.

## Result

`hle/crt_linux.rs` now exports C-locale `isalnum` (`0-9`, `A-Z`, `a-z`; every
other value including `EOF` is 0). The title's identifier scanner at
`exe+0x2749412` loops while `isalnum(char)` is nonzero
(`exe+0x2749423` calls the import); the generic auto-stub returned 0 for every
byte, so no identifier ever matched. Retail's stub list loses `isalnum`; the
crash frontier does not move (next section).

## Retail evidence

Step 52's fresh root `/tmp/xcl-step52-r1` ended at `exe+0x25a087d` after
`isalnum` (`exe+0x274943d`, returns 0) and `ID3D12CommandQueue::#31`
(`exe+0xf2cc86`). The scanner is the only consumer of `isalnum` observed on
the path; the queue stub remains unclassified.

Fresh root `/tmp/xcl-step53-r1` at this commit:

- first frame still presented (`D3D12.X Vulkan: presented 3840x2160`);
- `isalnum` is gone from `xcl.log` and from `called_stubs`;
- `report.json` completes: exit `139`, SIGSEGV at `exe+0x25a087d`
  (`mov r10d,[rax+r9]`), fault `0x7f4e73c1a128`;
- the shell observed exit `133` (SIGTRAP) after that reported fault; the
  report records the SIGSEGV and the writer completed;
- 26 first-call stubs, the same Step-49 d3d12/Bink surface.

## Crash sample (bounded gdb, fresh roots)

A gdb stop on the signal (no software breakpoints before the fault) shows the
table walk:

```
pc=0x1425a087d  rax=0x7ff3a7f363e0  r9=0xdb098fc4  rdx=0x801
r14=0x7ff3a7d06b10  [r14+0x800*4]=0x118b4ffd  [rsp+0x3c]=3627
```

`0x1425a0872` loads `r9d = [r14 + 4*rdx]`, subtracts 8 and indexes
`[rax + r9]`. `r14` is the title-allocated offset array; `[rsp+0x3c]` is the
count read from the same stream (3627). Entries `0..=0x7ff` are valid
(`0x9d8, 0x9e0, 0x9e8, …`); entry `0x800` is stale data, so the first
out-of-range offset faults.

The table's source stream is a guest `std::filebuf` over the title's page
cache. Tracing `CreateFileW` shows the last reads are on handle `0x1014a` =
`G:\audio.rpf` (28.5 GB RPF9); every `ReadFile` returns its full requested
count, so the zero/short-read theory for the HLE call is ruled out. The last
read is 8192 bytes into the fixed 64 KiB cache `0x1479fb000`; the title writes
that one 0x2000 page into the table and then walks all 3627 entries without
loading the next page. `audio.rpf` does not contain the table bytes in
plaintext (28 GB scan, no match), so the table is built/decrypted by the
title, not returned verbatim by XCL.

## Diagnostic (not the fix)

At the fault, zeroing the remaining entries in gdb
(`r14+4*0x800 .. r14+4*3627`) lets the walk finish and moves retail to a new
SIGSEGV at `exe+0x9dfd93` (`mov rcx,[r8]`). That proves the dirty tail is the
crash trigger and gives the next frontier; it does not identify which XCL-owned
allocation/fill should have supplied the zero tail.

## Tests

`hle::crt_linux::tests::isalnum_matches_the_c_locale` resolves `isalnum`
through `exports()` and checks every digit/letter is nonzero and
`0`, space, `_`, `#`, `/`, `:`, `0x7f`, `0x80`, `0x100`, `-1` are zero. At the
parent `05bbab1e` with only the test added:

```
thread '...isalnum_matches_the_c_locale' panicked:
isalnum must be wired
test result: FAILED. 0 passed; 1 failed; 892 filtered out
```

GREEN at this commit: `1 passed; 0 failed`.

## Gates

- `cargo check -p xcl --tests`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo test -p xcl --lib -- --test-threads=4`: **892 passed / 0 failed /
  1 ignored**.
- `cargo test -p xcl --test launcher_e2e`: **7 passed / 0 failed**.
- Retail `/tmp/xcl-step53-r1`: first frame, 26 stubs, crash unchanged (above).

## Next (Step 54)

The table walk needs the rest of the `audio.rpf` page. Sample the writer of
the table tail: a hardware watchpoint on the first out-of-range entry (or a
break on the first stream read of `3627*4` bytes) names whether the title's
page fill stops after one 0x2000 page, or whether an XCL allocation/commit
returned non-zeroed memory that the title expects zero. Fix the XCL-owned
half with a RED test; if the stop is title-side, record the honest negative
and the next consumer-visible input.

## Commits

One code+test commit and one docs commit on `linux`, pushed with green gates.
