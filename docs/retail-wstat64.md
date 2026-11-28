# Retail Step 81 — UCRT `_wstat64` through the VFS

The Step-80 artifact (`/tmp/xcl-step80-r1`) calls
`api-ms-win-crt-filesystem-l1-1-0!_wstat64` 9 times (IAT `0x146794798`,
report return address `exe+0x1a994ad`). The auto-stub returned 0 without
writing the caller's 0x38-byte `_stat64`, so every probe reported success
with stale size/time fields.

## The caller

`0x141a99440` is the title's file-probe helper:

```
0x1a99449  mov  rsi, rcx             ; out: {mtime, atime, size}
0x1a9944c  mov  rcx, [rdx]           ; title string object
0x1a99461  call 0x140d427b0          ; string -> length/capacity
0x1a9947c  call [rax+0x50]           ; title allocator hook
0x1a9949a  call 0x140eb5b50          ; build the wide path
0x1a9949f  lea  rdx, [rsp+0x20]      ; struct _stat64 (0x38 bytes)
0x1a994a4  mov  rcx, r14             ; wchar_t *path
0x1a994a7  call [0x146794798]        ; _wstat64
0x1a994bc  call [rax+0x60]           ; free the wide path
0x1a994bf  test edi, edi ; je hit    ; 0 = success
0x1a994d2  vpermilps xmm0, [rsp+0x40], 0x4e  ; stat+0x20 = atime/mtime
0x1a994da  vmovups [rsi], xmm0                ; lane-swapped pair to out
0x1a994de  mov  rax, [rsp+0x38]               ; stat+0x18 = st_size
0x1a994e3  mov  [rsi+0x10], rax
```

So the fields the title consumes are `st_size` at `+0x18` and
`st_atime`/`st_mtime` at `+0x20`/`+0x28`. The caller's 9 probes use the
same name with `.dds`, `.sif`, `.pvr`, `.tga` (`0x14677c803`,
`0x1467814b8`, `0x14677cff2`, `0x14678601f`) and stop at the first hit.
The stub made all four candidates "present", so the probe always selected
`.dds` and copied uninitialised times/size out.

## Contract implemented

`hle/crt_linux.rs::_wstat64(const wchar_t *path, struct _stat64 *buffer)`:

- Resolves the UTF-16 path through the process VFS with
  `Vfs::real_path_for_read` — the same resolver the loader/DirectStorage
  readers use — then `stat`s the host file. Without a VFS the path is
  used as a host path, matching the other Linux file shims.
- Fills the 0x38-byte MSVC layout:

  | offset | field | value |
  |---|---|---|
  | `0x00` | `st_dev` (u32) | host device |
  | `0x04` | `st_ino` (u16) | host inode (truncated) |
  | `0x06` | `st_mode` (u16) | `_S_IFREG 0x8000` / `_S_IFDIR 0x4000` + `_S_IREAD 0x0100` / `_S_IWRITE 0x0080` / `_S_IEXEC 0x0040` |
  | `0x08` | `st_nlink` (i16) | host link count |
  | `0x0a`/`0x0c` | `st_uid`/`st_gid` | 0 (no Windows owner mapping) |
  | `0x10` | `st_rdev` (u32) | 0 |
  | `0x18` | `st_size` (i64) | file length; 0 for a directory (Windows answer) |
  | `0x20` | `st_atime` | POSIX seconds |
  | `0x28` | `st_mtime` | POSIX seconds |
  | `0x30` | `st_ctime` | POSIX seconds (host status-change time; Windows has creation time here) |

- Returns 0 on success. A null path or buffer is `-1`/`EINVAL`, a path the
  VFS hides or the host cannot resolve is `-1` with the mapped `errno`
  (`ENOENT` for missing/whiteout, `EACCES` for permission denial). A
  failed call writes nothing into `buffer`.
- Registered in `exports()`.

## Tests

`wstat64_reports_real_files_and_directories_through_the_vfs` resolves the
name through `exports()` (parent tree fails with `_wstat64 must be
wired`, RED 0/1; GREEN 1/0). It pins a real temp file's exact 10-byte
size, `_S_IFREG`, non-zero link count and all three times; a directory's
`_S_IFDIR` and zero size; a missing path as `-1`/`ENOENT` with a
`0x5A`-filled buffer byte-identical afterwards; and null buffer/path as
`-1`/`EINVAL`.

Gates: `cargo check -p xcl --tests` 0, `cargo check --workspace` 0,
`hle::crt_linux` **33/0**, lib **953 passed / 0 failed / 1 ignored**
(`--test-threads=4`), `launcher_e2e` **7/0**.

## Retail delta

ONE plain fresh-root run, `/tmp/xcl-step81-r1`
(`XCL_DATA_ROOT=/tmp/xcl-step81-r1`):

| | Step 80 | Step 81 |
|---|---|---|
| first present | 7.015 s | 6.964 s |
| presents | 135 | **144** |
| exit | 133 SIGTRAP `exe+0x2219651` | 133 SIGTRAP `exe+0x2219651` |
| called stubs | 32 | **31** |

- `_wstat64` left `called_stubs`; no new warning, no fault at the stat
  call. The frontier is the parked allocator trap (`+0x2219651`, pool
  fill), unchanged.
- No causality claim for the pool OOM; that item stays parked on the
  console sample (`docs/retail-pool-fill.md`).
- Remaining called stubs by count: `PeekMessageA` 118 (honest null-window
  answer), allocator `#9` 118, list `#75`/`#126`/`#142` 117 each,
  `LocaleNameToLCID` 12, queue `#31` 19, `#131` 6, BCrypt RNG 5 each,
  `#132` 5, `log2`/`_fdtest` 2 each.
