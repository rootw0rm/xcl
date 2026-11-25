# Progress — Linux static recompilation of XCL

Builder and Critic append here. Keep entries short: what landed, exact verification commands
and counts, what was skipped and why.

## Verification Notes

- Baseline (planner, 2026-11-24): `cargo check -p xcl --target x86_64-unknown-linux-gnu`
  fails while compiling `retour 0.3.1` (`"stdcall"/"fastcall" is not a supported ABI`).
  No Linux coverage yet.
- Toolchain: rustc/cargo 1.100.0, only `x86_64-unknown-linux-gnu` installed.
- Retail files verified present: `game_scarlett_master.exe`, `MicrosoftGame.Config`,
  `shaders.rpf`; `leonida-work/shaders/{dxbc,fspx,spirv}`.
- Planner refresh (2026-11-24, `linux` @ `f63470d`): steps 1–7 are closed and independently
  reviewed in `progress.md`. The next task is step 8 (Linux VFS). Its implementation is
  already present as uncommitted work in the clean-egret worktree; no planner code changes
  or new verification claims were made. Installed Rust targets are now
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`, and `x86_64-pc-windows-msvc`;
  `x86_64-unknown-linux-musl` is still absent. A read-only survey then added step 9b
  (import-free PE execution) and step 10e (Linux launcher/host), split step 10 into 10a–10e,
  and made VFS traversal containment plus runtime stub thunks explicit. Queue after replan:
  26 tasks, 7 closed, 19 open, exactly one ready task (step 8).

## Completed Tasks

### step-01 build skeleton (`task-1795516176-b26a`)

Landed: `windows`/`windows-sys`/`retour` are `cfg(windows)` dependencies; the host runtime
(`eh`, `hle`, `hooks`, `host`, `inspect`, `launcher`, `loader`, `modules`, `preflight`,
`report`, `resolver`, `stubs`, `tls`, `vfs`, `vfs_hooks`) is gated in `lib.rs`; GPU D3D12
runtime pieces (`adapter`, `agility`, `fence`, `texture`, `uma`, `recomp/{dxc,pso,runner,tests}`)
are gated while RDNA2/decode/emit/tiling stay; `gpu::desc` keeps the pure T#/S#/V# decode and
gates the D3D12 conversions. Linux leaves: log thread id, `rpf` atomic no-clobber link(2),
UTF-8 ANSI helpers, `gpu::shader::readable` returns false (never dereferences), DXC paths in
`shaders`/`gpu::shader` return "DXC is Windows-only; Phase D". MicrosoftGame.Config decoding
moved from `launcher` to `manifest` so the pure manifest/config/state chain builds on Linux.
`main.rs`: TLS callback, `.CRT$XLY`, volatile read and Explorer COM/comctl menu are
`cfg(windows)`; `xcl run` on Linux prints `xcl run: not supported on Linux yet (Phase C)` and
exits 2. `testtitle` includes its PE body only on Windows; `testgamedll` is an empty crate on
Linux. `build.rs` only passes the PE ASLR linker flag for Windows targets.

Verification (2026-11-24, branch `linux`):

- `cargo check --workspace --target x86_64-unknown-linux-gnu` — Finished (4 dead-code
  warnings for Windows-only helpers; no errors).
- `cargo build --workspace` — Finished; `target/debug/xcl` 33,113,184 B and
  `target/debug/xcl_testtitle` 4,496,136 B (Linux stub).
- `cargo test -p xcl --lib` — `127 passed; 0 failed; 0 ignored; 0 measured`.
- `xcl --version` → `xcl 0.1.0`, exit 0; `xcl --help` exit 0.
- `xcl run /tmp/nope` → `xcl run: not supported on Linux yet (Phase C)`, exit 2.
- Skipped on Linux by `cfg(windows)`: the PE-vs-system-DLL parser tests, VirtualQuery
  readable tests, D3D12 desc conversions and the DXC/GPU `recomp` test suite. They still
  build/run on Windows.
- Windows cross-check not run: `x86_64-pc-windows-msvc` needs `lib.exe` and
  `x86_64-pc-windows-gnu` needs `x86_64-w64-mingw32-gcc` (both absent on this host). Windows
  code paths are unchanged behind `cfg(windows)`; the cross-build is step 11.


### step-01 review (Critic, independent)

Verified on the current worktree, not from the Builder's notes:

- `cargo check --workspace --target x86_64-unknown-linux-gnu` — Finished, 4 dead-code
  warnings (`pls_quota_bytes`, `rect_list_gs`, `passthrough_elements`,
  `from_file_ansi_ptr`), 0 errors.
- `cargo test -p xcl --lib` — `127 passed; 0 failed; 0 ignored; 0 measured` (10.8 s).
- `xcl --version` → `xcl 0.1.0`, rc 0; `xcl --help` → 21 lines, rc 0;
  `xcl run /tmp/nope` → `xcl run: not supported on Linux yet (Phase C)`, rc 2;
  `xcl inspect /tmp/nope` → `... (Phase B)`, rc 2.
- Real files (wiring was in scope only, but the paths are green): `xcl rpf shaders.rpf --top 5`
  → 26,503 records, rc 0; `xcl disasm <568 B dxbc> --hex` → RDNA2 container decode, rc 0;
  `xcl shaders <1-file dir>` → CS 1, 4 instructions, rc 0; `--precompile` → Phase D message,
  rc 2; `--compile 1` → Phase D notice, rc 0.
- `tests/testtitle/src/title_body.rs` is the old `main.rs` body byte-for-byte plus a 2-line
  header comment (`diff` clean); `tests/testgamedll` is `#![cfg(windows)]`.
- `unimplemented_on_linux` census: 0. `grep LINUX-TODO crates/xcl/src` → 10 markers, all on
  Linux stubs with a Phase letter.
- Windows cross-check still impossible on this host: `cargo check -p xcl --target
  x86_64-pc-windows-gnu` dies in `retour`'s cc build (`x86_64-w64-mingw32-gcc` absent);
  `--target x86_64-pc-windows-msvc` dies in the same cc build (`lib.exe` absent). Matches the
  Builder's note; step 11 owns the cross-build.

Verdict: pass. Follow-ups below feed the step-02 revision, not another step-01 round.

### step-02 platform leaves (`task-1795516176-be4f`)

Landed a shared `platform` module (the Windows-only `launcher`/`host`/`kernelx` cannot host
the Linux leaves):

- `platform::data_root()`: Windows keeps `XCL_DATA_ROOT` > `%LOCALAPPDATA%\XCL` > temp;
  Linux uses `XCL_DATA_ROOT` > `$XDG_DATA_HOME/XCL` > `$HOME/.local/share/XCL` > temp/XCL.
  The precedence rule is a pure helper tested without mutating the process environment.
- `platform::process_private_bytes()`: one implementation for host logging, kernelx
  `TitleMemoryStatus` and `XMemGetWorkingSetStatistics`. Windows keeps
  `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage` (fallback `PagefileUsage`); Linux parses
  `/proc/self/status` `VmData` in KiB and documents why it is an analogue of commit charge,
  not `PrivateUsage` (virtual private writable mappings, not resident bytes/stack).
- `log::thread_id()` Linux test: stable on one thread, distinct across threads.
- `util`: Linux UTF-8 `from_ansi_ptr`/`from_file_ansi_ptr`/`write_ansi_out` tests, including
  lossy invalid bytes and character-boundary truncation; `under_wine() == false` on Unix.
  UTF-8 is the final Linux ANSI contract (the console's ANSI code page), so the step-2
  `LINUX-TODO` locale markers are gone; a locale shim is not planned.
- `rpf`: kept `link(2)` + remove for the no-clobber publish and added a success test (data
  complete, temp gone, an existing destination is not replaced). `std::fs::rename` would
  replace, contradicting the extraction contract and the existing `AlreadyExists` test.

Verification (2026-11-24, final tree):

- `cargo test -p xcl --lib` — `135 passed; 0 failed; 0 ignored; 0 measured`.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` — Finished, 4 pre-existing
  dead-code warnings, 0 errors.
- `cargo build --workspace --target x86_64-unknown-linux-gnu` — Finished;
  `xcl --version` → `xcl 0.1.0`; `xcl run /tmp/nope` → Phase C message, rc 2.
- Windows cross-check attempted with `CC_x86_64_pc_windows_msvc=clang-cl`: blocked in
  `retour`'s `libudis86-sys` (`stdio.h` not found — no Windows SDK headers), before xcl is
  type-checked. Modified Windows-only files parse clean under `rustfmt --check`; the
  `cfg(windows)` code was inspected, not compiled. Step 11 still owns a real cross-build.

### step-03 disasm + rpf (`task-1795516176-c946`)

No Linux blocker found in either command; no source changes were made. The two retail inputs
and their exact output shape:

- Smallest corpus shader (`ls -S /run/media/anon/12TB_B/leonida-work/shaders/dxbc`, 568 B):
  `target/debug/xcl disasm '.../4water_cutscene_codec_CS_FlushDummy_0c6ce5b6_Wrapped.dxbc'`
  → `format: xbox-native`, parts SFI0/ISG1/OSG1/PSV0/HASH/DXIL/PRIV, `stage CS (DXIL)`,
  SM 6.6, one FSSM (zlib 160 B → 578 B) with `programs: 1`; 4 decoded instructions
  (`v_nop` ×2, `s_inst_prefetch`, `s_endpgm`); rc 0.
- FSPX driver container (`.../shaders/fspx/amd_fidelityfx_upscaler_xbox_06.dxbc`, 42,616 B):
  `format: container-unknown`, `ACCC +0x258 42016 B`; the listing finds one program in the
  ACCC region (`+0x258`, 41,592 B) and decodes the real RDNA2 body from `+0x380` (vector ALU,
  buffer loads/stores, `s_endpgm`); rc 0. `shader::classify`/`rdna2_listing` are pure and
  behave the same on Windows, so the ACCC-region garbage before the real program was left
  alone (`do not change parsing semantics`); a `pso::fspx`-aware disasm belongs to a later
  step if wanted.
- `target/debug/xcl rpf .../shaders.rpf --top 20` → `392.6 MB, 26503 records, packed 213.0 MB
  → 233.8 MB`; first 20 records all `[DXBC]` (e.g. `VS_AlphaTransmissionFullScreenTri_*`),
  `(no extension) 26503 233.8 MB`, `DXBC 26503`; rc 0 (5.4 s).
- `target/debug/xcl rpf .../shaders.rpf --probe --top 20` → `392.6 MB, streaming header
  probe`, `RPF9 archives (headers every 2 KB): 1`, `0x00000000: fields 0x8640 0x8620 0xa0`,
  `TOC encryption: unknown (insufficient complete header evidence)`; rc 0.

Syscall evidence (`strace` absent from the host; used the Arch extra package unpacked to
`/tmp/strace-root`, not the repo): `strace -f -e trace=openat` on both disasm files and on
`xcl rpf --top 20` → 0 `*.dll` opens; the only game-data opens were the two input paths and
`shaders.rpf` under the game folders.

Tests: `cargo test -p xcl --lib rpf` → 10 passed, 0 failed; `cargo test -p xcl --lib rdna2`
→ 9 passed, 0 failed (125/126 filtered out respectively).

### step-03 review (Critic, independent)

Re-ran the acceptance on the current worktree (`9602ed5`; step-03 has an empty diff):

- `target/debug/xcl disasm` on the 568 B `4water_cutscene_codec_CS_FlushDummy_*`: rc 0,
  `format: xbox-native`, stage CS, SM 6.6, one FSSM (zlib 160 → 578 B), 4 instructions
  (`v_nop` ×2, `s_inst_prefetch`, `s_endpgm`).
- `target/debug/xcl disasm` on `amd_fidelityfx_upscaler_xbox_06.dxbc`: rc 0, 9,089 listing
  lines, `format: container-unknown` (`ACCC +0x258 42016 B`), real RDNA2 body from `+0x380`
  (`s_mov_b64`, vector ALU, `buffer_store_dword`), `s_endpgm` at `+0xa4cc`. Piping to `head`
  yields rc 101 (SIGPIPE from `println!`), not a panic — redirected to a file it is rc 0.
- `target/debug/xcl rpf shaders.rpf --top 20`: rc 0 in 5.57 s, 26,503 records, 213.0 MB
  packed → 233.8 MB, all sampled `[DXBC]`; `--probe --top 20`: rc 0, RPF9 ×1, fields
  `0x8640 0x8620 0xa0`.
- `strace -f -e trace=openat` on both disasm runs and `rpf --top 20` (11 syscall lines each):
  0 `*.dll` opens; only `/proc/self/maps`, loader libs and the input path each time.
- `cargo test -p xcl --lib` → `135 passed; 0 failed; 0 ignored` (10.94 s);
  `cargo test -p xcl --lib rpf` → 10 passed; `cargo test -p xcl --lib rdna2` → 9 passed.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, 4 pre-existing
  dead-code warnings, 0 errors. No `gpu/recomp` or `build.rs` translator-source change in
  step-03.

Verdict: pass. No Linux blocker needed a fix; the ACCC-region garbage is pre-existing and
explicitly out of scope.

### step-04 shaders (`task-1795516176-d4b1`)

Landed: `--compile N` on Linux now reports `xcl shaders --compile: DXC is Windows-only; not
supported on Linux yet (Phase D)` once on stderr and exits 2, after still printing the normal
stats (the translations are counted, `dxc_compiled` stays `null` in JSON). Previously the
message was printed only when a complete sample existed and the command still exited 0.
`run` funnels both the single-file and folder/RPF paths through a cfg twin `finish(rc,
compile)`; the Linux `compile_samples_maybe` no longer prints, so the error is emitted exactly
once even for an empty corpus. Regression test
`shaders::tests::compile_reports_dxc_is_windows_only` writes one synthetic shader to a temp
dir and asserts rc 2. No emitter file moved: `TRANSLATOR_SOURCES`/`build.rs` untouched.

Corpus runs (release of game files untouched; the full-corpus folder is a `/tmp` directory of
symlinks to the 26,503 `.dxbc` files, because the literal `leonida-work/shaders` tree also
holds 3.6 GB `dxbc/disasm/*.s`, 477 MB `dxbc/fssm/*.fssm` and 18 GB `spirv/*.spv` side data
that is not shader input):

- `xcl shaders /tmp/xcl-shaders-sample` (200 retail `.dxbc`) → 200 shaders, stages VS 101 /
  MS 78 / CS 18 / PS 3, 103,029 instructions, 0 undecoded dwords, 280 distinct mnemonics,
  0.304 s.
- `xcl shaders /tmp/xcl-shaders-all --top 20` (26,503 retail `.dxbc`) → 26,503 shaders,
  stages PS 12,101 / MS 9,282 / CS 2,689 / VS 2,236 / GS 195; FSSM 26,698, programs 26,698,
  data blocks 1,473; 44,422,713 instructions, 0 undecoded dwords, 452 distinct mnemonics;
  24.853 s.
- `xcl shaders /tmp/xcl-shaders-sample --recomp` → 200/200 programs fully translated (100%),
  0 translation errors, wave32 1, HLSL 23.9 MB, 0.231 s.
- `xcl shaders shaders.rpf --recomp --top 20` → 26,503 records; 26,698/26,698 programs fully
  translated (100%), 0 errors, wave32 2,608, HLSL 9,254.4 MB, 64.893 s.
- `xcl shaders <568 B CS> --compile 1` → rc 2, one stderr line; `--compile 1 --json` → rc 2,
  valid JSON (`dxc_compiled: null`); `xcl shaders <200-file dir> --compile 5` → rc 2, one
  stderr line.
- `cargo test -p xcl --lib gpu::recomp::` → 44 passed; 0 failed; 0 ignored (92 filtered).
  These are the pure emitter tests that stay on Linux; no GPU/DXC skips were hit.
- `cargo test -p xcl --lib shaders::tests` → 3 passed; `cargo test -p xcl --lib` → 136
  passed; 0 failed; 0 ignored (11.12 s).
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, 4 pre-existing
  dead-code warnings, 0 errors.

### step-05 inspect (`task-1795516176-deb2`)

Landed: `inspect` builds on Linux. The ordered DLL→HLE-module table moved out of
`hle::build` into `crates/xcl/src/hle_routes.rs` (29 patterns + the module-name constants,
re-exported from `hle` so Windows paths are unchanged); config aliases and the optional
`XCL_NET_OFFLINE` routes stay in `hle::build`. The Linux `inspect` rewrite uses that table
without the HLE implementation modules or `LoadLibrary`:

- a DLL routed to an HLE module → every imported function is `STUB` (never `HLE`; the modules
  are Phase C);
- a DLL present in the inspected folder → `game DLL (name)` (case-insensitive, read-only);
- anything else → `Windows (name; unavailable on Linux)`, so the system leg is named instead
  of being claimed.

`Console game:` on Linux lists the HLE module names the title routes to. The TLS line and
inspect help text no longer promise a carrier on Linux. `main.rs`'s Linux `inspect` stub is gone.

Verification (2026-11-24, branch `linux`):

- `cargo run -p xcl -- inspect .../game_scarlett_master.exe` → rc 0; EXE x64, ImageBase
  `0x140000000`, entry `+0x56a1ec8`, size `0xb06d000`, subsystem 2, TLS template 5248 B,
  299,257 `.pdata`, stack `0x100000`; `Console game: yes (imports d3d12_xs.dll)`;
  587 imports from 68 DLLs, 157 STUB.
- Independent totals: `llvm-readobj --coff-imports` on the same file → 587 `Symbol:` and 68
  `Name:` entries (matches).
- `--json` → rc 0, `jq` parses; `imports=587`, `dlls=68`, `stubs=157`, `hle=0`,
  `tls_template_bytes=5248`, `exception_entries=299257`.
- `xcl inspect /tmp/nope` → rc 2 with `cannot read`; `xcl inspect /etc/hostname` → rc 3 with
  the PE-signature message.
- `xcl inspect Bink2Scarlett.dll` → rc 0, `Console game: NO`, 60 imports from 20 DLLs (game
  DLLs still recognised).
- `cargo test -p xcl --lib hle_routes` → 2 passed; `cargo test -p xcl --lib inspect` → 2
  passed (Linux classification + case-insensitive game DLL); `cargo test -p xcl --lib` →
  140 passed; 0 failed; 0 ignored. `cargo test -p xcl --lib gpu::recomp::` → 44 passed
  (no GPU/DXC skips hit).
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, the 4 pre-existing
  dead-code warnings, 0 errors. No emitter file moved, `TRANSLATOR_SOURCES` untouched.
- Windows cross-check still unavailable on this host (no MSVC/mingw); the Windows-only
  `hle::build` diff is the same ordered table plus the unchanged aliases/netoff legs, and
  Windows compilation of the cfg(windows) side is step 11's cross-build.

### step-06 loader mmap (`task-1795516176-1dc5`)

Landed (`crates/xcl/src/loader_linux.rs`, `crate::loader` on Linux via `#[path]` in
`lib.rs`; the Windows `loader.rs` is untouched):

- `map_image`/`load_main` parse the PE, then `mmap(MAP_FIXED_NOREPLACE)` exactly at
  `ImageBase` (`MAP_PRIVATE|MAP_ANONYMOUS`, RW while loading). Headers and raw section bytes
  are copied; the anonymous mapping supplies the zero-filled tails. An occupied range is a
  hard error naming the address; there is no relocation fallback even when relocations exist.
- Relocation application keeps the Windows DIR64/HIGHLOW semantics (the path is a no-op while
  the base is the link address, and is unit-tested with a synthetic non-zero delta). Import and
  delay-import tables are parsed with the same `pe` parser; a broken normal table fails the
  load, a broken delay table warns (as on Windows). Resolution itself is step 10 and is not
  faked: the IAT stays empty and the loader logs `resolution is Phase C step-10`.
- TLS directory: template/zero-fill and the callback list (terminated, all addresses inside
  the image) are kept in order on `MappedModule`; the per-thread block/callback execution is
  step 7. `.pdata` is logged and deferred to step 9. Entry RVA, DLL flag, exports and stack
  reserve are the same fields as Windows.
- Per-section `mprotect` from the PE characteristics (headers read-only), `madvise(
  MADV_HUGEPAGE)` for sections ≥2 MiB, and a comment that x86_64 I-cache is coherent instead
  of the Windows `FlushInstructionCache`.
- `modules` now builds on Linux (shared `MappedModule`/registry); the HLE pseudo-handle page
  uses `mmap` and the address-to-name fallback is Windows-only. Added `libc` under
  `[target.'cfg(target_os = "linux")'.dependencies]` (MIT OR Apache-2.0, used under MIT;
  static-musl friendly) for `mmap`/`mprotect`/`madvise`.

Verification (2026-11-24, branch `linux`):

- `cargo test -p xcl --lib loader::` → `8 passed; 0 failed; 0 ignored` (synthetic map, non-PE,
  occupied base, relocations with a non-zero delta, TLS callback order, `.text` protects as
  `r-xp` and headers as `r--p`, broken import table, retail harness).
- `XCL_SAMPLE_EXE=.../game_scarlett_master.exe cargo test -p xcl --lib
  loader::tests::maps_the_retail_exe_when_sample_set -- --nocapture` → `1 passed`.
  Output: `import tables parsed (68 DLLs/587 functions, delay 0 DLLs/0 functions)`;
  `loaded game_scarlett_master.exe @ 0x140000000 (size 0xb06d000, preferred 0x140000000)`;
  `TLS directory: template 5248 B, zero-fill 0 B, 2 callbacks`;
  `mapped ... entry +0x56a1ec8, 10 sections, perms .text Some("r-xp")`.
- `cargo test -p xcl --lib` → `150 passed; 0 failed; 0 ignored` (was 140; +8 loader, +2
  shared `modules` tests now building on Linux).
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, the same 4
  pre-existing dead-code warnings, 0 errors.
- `rustfmt --edition 2021 --check crates/xcl/src/loader_linux.rs` → clean.
- `cargo test -p xcl --test e2e` still fails on Linux (2 of 3: no `xcl_testtitle.exe`/
  `testgame_mw.dll` PE, and `xcl run` is the Phase C stub). Pre-existing and owned by step 11;
  not counted as coverage for this step.
- Windows cross-check unavailable on this host (no MSVC/mingw, `retour` cannot build); the
  Windows `loader.rs` is byte-identical and `modules.rs` changes are `#[cfg(windows)]`-neutral.

### step-06 review (Critic, independent)

Re-ran the acceptance on the current worktree (`ea91b17`); the Windows `loader.rs` is
byte-identical and `modules.rs` only gains `#[cfg]`-gated code.

- `cargo test -p xcl --lib loader::` → `8 passed; 0 failed; 0 ignored` (142 filtered).
- `XCL_SAMPLE_EXE=.../game_scarlett_master.exe cargo test -p xcl --lib
  loader::tests::maps_the_retail_exe_when_sample_set -- --nocapture` → `1 passed`; log:
  `import tables parsed (68 DLLs/587 functions, delay 0 DLLs/0 functions)`,
  `loaded game_scarlett_master.exe @ 0x140000000 (size 0xb06d000, preferred 0x140000000)`,
  `TLS directory: template 5248 B, zero-fill 0 B, 2 callbacks`,
  `mapped ... entry +0x56a1ec8, 10 sections, 2 TLS callbacks, perms .text Some("r-xp")`.
- `cargo test -p xcl --lib` → `150 passed; 0 failed; 0 ignored` (10.97 s);
  `cargo test -p xcl --lib modules::` → 2 passed.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, the same 4
  pre-existing dead-code warnings, 0 errors.
- Code read: overlap with `MAP_FIXED_NOREPLACE` is a hard `EEXIST` error naming the address;
  headers/raw sections are copied with bounds checks; DIR64/HIGHLOW are applied and
  unit-tested with a non-zero delta; a broken normal import table is fatal while a broken
  delay table warns (as Windows); the TLS callback list is bounded to the image and kept in
  order; `entry_rva`/`is_dll`/exports/stack reserve match the Windows fields; per-section
  `mprotect` plus THP for ≥2 MiB; the three deferrals are explicit `LINUX-TODO` markers
  (imports step 10, TLS install step 7, `.pdata` step 9) and the IAT is never faked.
- Around it: `target/debug/xcl --version` → `xcl 0.1.0`; `xcl inspect ...` still prints the
  587/68/157 baseline (the rc 101 on a `head` pipe is the known SIGPIPE, not a failure).
- Windows cross-check still unavailable on this host (no MSVC/mingw; `retour` cannot build);
  the Windows loader file is unchanged and `modules.rs` edits are `#[cfg]`-gated.

Verdict: pass. The follow-ups below are minor and do not block step-07.

### step-07 TLS per thread (`task-1795516176-296b`)

Landed (`crates/xcl/src/tls_linux.rs`, `crate::tls` on non-Windows via `#[path]`; the
Windows `tls.rs` is untouched):

- PE static TLS is reached through `gs:[0x58]` + the image's `_tls_index`. Linux uses `FS`
  for its own thread pointer, so every attached host thread gets a minimal fake TEB
  (self at `0x30`, `ThreadLocalStoragePointer` at `0x58`) installed with
  `arch_prctl(ARCH_SET_GS)`. Each image's block is a 16-byte-aligned, zero-filled copy of
  its relocated template (plus zero fill); the slot pointer array is grown on later installs
  and the TEB pointer follows the `Vec` when it moves.
- Slots: the main image always takes slot 0 (as a process executable does on Windows);
  DLLs allocate from 1. No exchange is needed because xcl itself is an ELF whose TLS is in
  `FS`.
- `install` publishes the slot into the image's `_tls_index` (volatile + fences), and fills
  the block in every thread that is already attached under one install/attach lock; a
  failed allocation rolls the earlier blocks back. `attach_current_thread` is idempotent,
  attaches `DLL_THREAD_ATTACH` callbacks after the blocks, `detach_current_thread` runs
  `DLL_THREAD_DETACH` before freeing and restores the previous `GS`. Callback order matches
  `loader.rs`: attach in load order; detach with modules reversed and callbacks forward.
- `spawn(name, f)` is the kernelx thread-entry wrapper: attach, run `f`, detach, and detach
  also when `f` panics (the panic is resumed). No `pthread_create` interposition.
- `loader_linux::load_main` installs the main image's TLS before page protections (writable
  `_tls_index`, as on Windows). Non-main images keep a `// LINUX-TODO: Phase C step-10`
  marker: the game-DLL load path (`init_dll`, DllMain) does not exist yet, and the retail
  harness maps the exe with `map_image`, not `load_main`. Process attach/detach callbacks
  and kernelx adopting `spawn` are the same step 10; `testgamedll`'s `mw_tls_bump` is the
  cross-built e2e in step 11.
- Synthetic PE test fixture gained a writable `.data` section (RVA 0x2000) because the TLS
  directory, template and `_tls_index` cannot live in `r-xp` `.text`; callbacks are now two
  distinct `ret`s at `+0x1000`/`+0x1001` (also resolves the step-06 order follow-up).

Verification (2026-11-24, branch `linux`):

- `cargo test -p xcl --lib tls::` → `3 passed; 0 failed; 0 ignored` with
  `--test-threads=1` too (template 7→8 isolated on two threads, attach/detach callback
  order and template visibility through `gs:[0x58]`, idempotent attach).
- `cargo test -p xcl --lib loader::` → `9 passed; 0 failed; 0 ignored`; the new
  `main_image_installs_slot_zero_and_blocks_for_new_threads` reads slot 0 from the mapped
  image and sees 7→8 in two fresh threads. With
  `XCL_SAMPLE_EXE=.../game_scarlett_master.exe` → `9 passed`; the retail exe still maps at
  `0x140000000`, logs `template 5248 B, 0 B zero-fill, 2 callbacks` and is not TLS-installed
  by the non-main path.
- `cargo test -p xcl --lib` → `154 passed; 0 failed; 0 ignored` (was 150; +3 TLS, +1 loader).
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, the same 4
  pre-existing warnings, 0 errors.
- `rustfmt --edition 2021 --check crates/xcl/src/tls_linux.rs crates/xcl/src/loader_linux.rs`
  → clean.
- Windows cross-check unavailable on this host (no MSVC/mingw; `retour` cannot build); the
  Windows `tls.rs` is byte-identical and the loader/TLS changes are non-Windows files.

### step-07 review (Critic, independent)

Re-ran the acceptance on the current worktree (`f63470d`); the Windows `tls.rs` is untouched
and only `lib.rs`, `loader_linux.rs`, `tls_linux.rs` changed.

- `cargo test -p xcl --lib tls::` → `3 passed; 0 failed; 0 ignored`; repeated 12× (fresh
  process each time) → same counts, no flake. `--test-threads=1` → 3 passed.
- `cargo test -p xcl --lib loader::` → `9 passed; 0 failed; 0 ignored`. With
  `XCL_SAMPLE_EXE=.../game_scarlett_master.exe` → 9 passed; log: `template 5248 B, 0 B
  zero-fill, 2 callbacks`, mapped at `0x140000000`, `.text r-xp`.
- `cargo test -p xcl --lib` → `154 passed; 0 failed; 0 ignored` (10.80 s).
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, the same 4
  pre-existing dead-code warnings, 0 errors. `rustfmt --check` on both files → clean.
- Code read: fake TEB is zeroed+16-aligned, self at `0x30`, TLS array at `0x58`; blocks are
  `alloc_zeroed` + relocated-template copy with `total_len` honoured; install validates the
  template range, assigns main=0/DLLs from 1 under `INSTALL`, rolls back earlier threads on a
  failed allocation, writes `_tls_index` last with fences; attach installs blocks *before*
  `DLL_THREAD_ATTACH` and registers the thread before callbacks (a callback may install);
  detach removes the thread under `INSTALL`, runs callbacks in `loader.rs` order, restores
  `GS`, frees. `spawn` detaches on panic and resumes it. `load_main` installs before
  `protect_image`.
- Abuse/late-load scratch tests (deleted afterwards, not committed): installing a second
  `main` image, a null template, and template > block all return `Err` without touching
  memory; a panicking `spawn` closure still runs the image's `DLL_THREAD_DETACH`; installing
  an image *after* a worker attached gives that already-attached thread the 7-byte template
  through `gs:[0x58]` and does not synthesize a thread-attach callback. All passed.
- Scratch test also confirmed the one semantic gap listed below: a thread that attached
  before an image was installed still receives that image's `DLL_THREAD_DETACH`. It mirrors
  today's Windows `on_tls_event`, and the late-install path is deferred to step 10, so it is
  a follow-up, not a blocker.

Verdict: pass. `template 7 → 8` is proven independently on two threads and the isolation
comes from per-thread `alloc_zeroed` blocks, not test ordering. The follow-ups below are
minor/deferred.

### step-08 Linux VFS (`task-1795516176-33d6`)

Landed the existing Linux VFS work as one reviewed slice:

- `vfs.rs` now compiles on Linux with host-path normalization while retaining the Windows
  parser and mapping tables. `GAME:`, `G:`, and `D:` mounts targeting the game folder classify
  as read-only `Target::Game`; writes copy the original into `<data>/overlay`, reads prefer
  the overlay, and deletes persist whiteouts. `S:`/`T:` mounts remain translated host paths.
- Linux mount translation is containment-checked after lexical normalization and through the
  nearest existing canonical ancestor. Escaping `..` paths, symlinked parents, and dangling
  final symlinks return a Linux-only `Target::Reject(ERROR_ACCESS_DENIED)` before any host
  operation. Untranslated console drives return `ERROR_PATH_NOT_FOUND` rather than falling
  through to a relative host open. The writable overlay gets the same relative-path and
  canonical containment checks, and its root may not be a symlink. Configured mounts under
  the title data root (default `S:`/`T:`) cannot symlink out of that root.
- `vfs_hooks_linux.rs` provides the linked-in libc open path (no `LD_PRELOAD`): Win32 access
  and disposition flags map to `open(2)`, errors map back to Win32 codes, creation/truncation
  never grant write access implicitly, content-data rights are separate from metadata/control
  rights, append rights cannot authorize `CREATE_ALWAYS` truncation, and no-data-access
  handles use `O_PATH`. `CREATE_ALWAYS` reports whether a visible original or overlay file was
  replaced. A failed host open restores the complete pre-write whiteout set and write-record
  state, including ancestor whiteouts; a VFS write lock is held across the staged host open so
  concurrent whiteout changes cannot be lost. `FILE_FLAG_OPEN_REPARSE_POINT` maps to
  `O_NOFOLLOW`. `lib.rs` exposes it as `vfs_hooks_linux` and re-exports it as `vfs_hooks` for
  the future kernelx call path.
- Regression tests cover D/S/T mapping, COW visibility with a recursive game-tree hash,
  dispositions, `..` escapes (including embedded `GAME:` forms), game/data-mount/overlay
  symlink and dangling-link escapes for both reads and writes, leaf/ancestor failed-open
  whiteout rollback, exact content-access checks, read-only create/truncate/open-always
  attempts, and unmapped drives with both separator styles.

Verification (2026-11-24, branch `linux`):

- `cargo test -p xcl --lib vfs:: -- --test-threads=4` → 13 passed; 0 failed; 0 ignored.
- `cargo test -p xcl --lib vfs_hooks_linux:: -- --test-threads=4` → 5 passed; 0 failed;
  0 ignored.
- `cargo test -p xcl --lib -- --test-threads=4` → 172 passed; 0 failed; 0 ignored. The two
  opt-in sample tests no-op without env vars in this run; each was then run separately with
  its real input (below), so neither is being counted as retail coverage from the bare run.
- `XCL_SAMPLE_EXE=.../game_scarlett_master.exe cargo test -p xcl --lib
  loader::tests::maps_the_retail_exe_when_sample_set -- --nocapture` → 1 passed; retail image
  mapped at `0x140000000`, 68 DLLs/587 imports, 2 TLS callbacks.
- `XCL_SAMPLE_RPF=.../shaders.rpf cargo test -p xcl --lib
  shaders::tests::retail_pack_sample -- --nocapture` → 1 passed.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, 4 pre-existing
  dead-code warnings, 0 errors.
- `cargo build --workspace --target x86_64-unknown-linux-gnu` → Finished;
  `target/x86_64-unknown-linux-gnu/debug/xcl --version` → `xcl 0.1.0`.
- `cargo test --workspace --no-run --target x86_64-unknown-linux-gnu` → fails before running
  tests in the existing Windows-only integration files `heap_probe`, `gpu_pso_e2e`, and
  `console_dlls_e2e` (Win32/D3D12 symbols and gated Linux modules). This is the known step-11
  integration-surface blocker, not a VFS regression; the required library suite and workspace
  check above are green.
- `git diff --cached --check` → clean.
- Independent read-only review (three passes) found no remaining high/medium issue beyond
  the documented check-then-open race and the explicitly deferred file-flag/edge-case
  follow-ups below; it found no Windows regression or recursive VFS lock path.
- Windows cross-check attempted with `cargo check -p xcl --target x86_64-pc-windows-gnu`:
  blocked before xcl type-checking because `libudis86-sys` cannot find
  `x86_64-w64-mingw32-gcc`. Windows `Target` has no rejection variant, and the extracted
  `mount_match` preserves the previous ordered embedded-prefix and drive-prefix behavior.
  Step 11 owns the real cross-build.

## Follow-ups

- step-09 review: `INSTALLED` is set before `pipe2`/watchdog creation succeeds and is not reset
  on those failures, so a transient resource failure permanently prevents a later install retry.
  Robustness only; the normal path is verified.
- step-09 review: `write_crash` deliberately writes a minimal `Report::default` payload. The
  crash string names the mapped module, but the structured `modules`/`vfs`/`shaders` arrays stay
  empty unlike `write_final`; consider collecting safe normal-context diagnostics if retail
  triage needs them.
- step-08: `FILE_FLAG_DELETE_ON_CLOSE` currently triggers COW classification but the Linux
  open primitive does not yet unlink on close; full delete-on-close and directory
  `FILE_FLAG_BACKUP_SEMANTICS` behavior belong with the kernelx file APIs in step 10c.
- step-08: containment currently uses canonical-ancestor checks before `open(2)`. If profiling
  shows the extra path-resolution syscalls are material, step 10c can harden the actual open
  with `openat2(RESOLVE_BENEATH)` while retaining these checks for non-open VFS operations.

Step-01 review items — all resolved by step-02 (verify: `rpf` still publishes via
`link(2)`, platform leaves live in `platform`, UTF-8 is the documented Linux ANSI contract).

- Optional/Phase D: `xcl disasm` on an FSPX `ACCC` container decodes raw ACCC bytes from
  `+0x258` before the real program at `+0x380`; a `pso::fspx`-aware listing or a structured
  summary would avoid the misleading head. Pre-existing Windows behavior, not a regression.

- step-06 minor: `debug_assert_eq!(base, base_addr)` is the only proof that
  `mmap(MAP_FIXED_NOREPLACE)` returned the requested address. A pre-4.17 kernel ignores the
  flag, so a release build could silently map elsewhere (the very failure mode the task bans).
  Return an `Err` naming both addresses instead; the host kernel (7.3) supports the flag, so
  this is defensive only.
- step-06 minor: Linux has no twin of the Windows `LoadGuard`; add load serialization when
  step 10 ports game-DLL/dependency loading so two threads cannot race the same base.
- step-06 minor: `tls_directory_callbacks_are_preserved_in_order` writes the same callback
  address twice, so it proves the count, not the order. Use two distinct VAs (and optionally
  one outside the image for the rejection path). *(Resolved in step-07: two distinct `ret`
  VAs; the outside-image rejection is still untested.)*
- step-07: the fake TEB models only `0x30` (self) and `0x58` (TLS array). PE code reading
  other TEB fields (`0x60` PEB, stack limits, …) sees zero. Extend it in step 10/12 when the
  retail smoke names the first field actually read; do not fill fields speculatively.
- step-07: process attach/detach callbacks for the main executable now run in
  `loader_linux::run_main` (step 09b); game-DLL `DllMain`/process callbacks remain step 10d. A
  thread created outside `crate::tls::spawn` also has no blocks; the kernelx shim must adopt the
  wrapper.
- step-07 review: `load_main` installs the main image's TLS but never attaches the calling
  thread. Step 09b's `run_main` now calls `tls::attach_current_thread()` before image code; the
  step-10e launcher must enter through `run_main`, not call the entry directly.
- step-07 review: `loader_linux.rs`'s module doc still lists "static TLS block installation
  and callbacks — step 7" under "Not ported yet"; update that bullet in step 10 when the DLL
  install path lands.
- step-07 review: a thread that attached before an image was installed receives that image's
  `DLL_THREAD_DETACH` at exit, although it never received `DLL_THREAD_ATTACH`. Real Windows
  sends neither for a DLL loaded after thread creation; today's Windows `on_tls_event` has
  the same asymmetry. When step 10 enables late DLL installs, record the image count at
  thread attach and run detach callbacks only for images present then.
- step-07 review: `install` allocates the slot with `NEXT_SLOT.fetch_add` before it can
  fail; a rollback (or a failed allocation in an attached thread) burns the slot and never
  reuses it. Cosmetic while slots are plentiful; revisit if install failures are expected.
- step-07 review: a late `install` can `Vec::resize` a target thread's slot array while that
  thread executes `mov rax, gs:[0x58]`; the old array is freed before `write_teb` points the
  TEB at the new one. Windows preallocates the TEB array, so no move is needed. Preallocate
  the per-thread slot array (e.g. Windows' 64 entries) when step 10 loads DLLs during
  gameplay; unreachable today because only the main image installs before threads exist.

### step-09 EH signal handler (`task-1795516176-3e86`)

Commit: `3bb3d34` — Linux: report fatal signals with a portable crash report.

Landed:

- `report.rs` now builds on Linux. The common report schema, CRASH bookkeeping, normal writer,
  loader and tests are portable; Windows-only preflight/DXC/D3D12/audio/XGameRuntime/
  DirectStorage fields and resolver/stub collection remain behind `cfg(windows)`. Linux import
  and called-stub collections stay empty until the step-10 resolver exists.
- `eh_linux.rs` installs `SA_SIGINFO` handlers for SIGSEGV, SIGILL, SIGFPE and SIGBUS. The
  handler records signal, x86-64 RIP and `si_addr` in atomics, wakes a watchdog through
  `pipe2`, and pauses. The watchdog formats `module+RVA`, exact PC and fault address into a
  crash-only `report.json`, then restores the default action and re-raises the original signal.
  No allocation, Rust lock or file I/O occurs in signal context.
- `install_pc_hook` logs and returns an explicit unsupported error; function-table
  registration logs that Windows unwind data is unavailable. Both carry Phase-C TODOs, and
  neither claims Windows `.pdata`/C++ unwinding.
- The regression test first failed with no child report under the no-op scaffold. The final
  child test verifies all four handler registrations, an exact two-instruction assembly fault
  PC, address `0x1000`, synthetic mapped-module naming, valid report JSON, and SIGSEGV
  termination without a panic.

Verification (2026-11-24, branch `linux`):

- `cargo test -p xcl --lib eh:: -- --nocapture` → 2 passed; 0 failed; 0 ignored.
- `cargo test -p xcl --lib report:: -- --nocapture` → 1 passed; 0 failed; 0 ignored.
- `cargo test -p xcl --lib -- --test-threads=4` → 175 passed; 0 failed; 0 ignored; 0 filtered.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished, 4 pre-existing
  dead-code warnings, 0 errors.
- `cargo build --workspace --target x86_64-unknown-linux-gnu` → Finished, the same 4
  pre-existing warnings, 0 errors.
- `target/x86_64-unknown-linux-gnu/debug/xcl --version` → `xcl 0.1.0`.
- `rustfmt --edition 2021 --check --config skip_children=true crates/xcl/src/eh_linux.rs` →
  clean; `git diff --check` → clean.
- Windows cross-check attempted with `cargo check -p xcl --target x86_64-pc-windows-gnu`:
  blocked before xcl type-checking because `libudis86-sys` cannot find
  `x86_64-w64-mingw32-gcc`. The Windows report fields/collection remain cfg-gated and the
  Windows EH file is untouched; step 11 owns a real cross-build.
- Not claimed: retail execution, Windows C++ exception unwinding, or `.pdata` translation.

### step-09 review (Critic, independent)

Reviewed `3bb3d34` against the task, `plan.md` step 9, and DEC-012. The signal handler only
stores fixed-size atomics, writes one pipe byte, and pauses; mapped-module lookup, allocation,
serialization, locks, and file I/O remain in the watchdog. Windows-only report fields and
collection are still `cfg(windows)`, and `eh.rs` is unchanged. No acceptance criterion was
narrowed and no C++/`.pdata` parity is claimed.

Independent verification on the current worktree:

- `cargo test -p xcl --lib eh:: -- --nocapture` → 2 passed; 0 failed; 0 ignored; 173 filtered.
- `cargo test -p xcl --lib report:: -- --nocapture` → 1 passed; 0 failed; 0 ignored; 174 filtered.
- `cargo test -p xcl --lib -- --test-threads=4` → 175 passed; 0 failed; 0 ignored; 0 filtered.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` and
  `cargo build --workspace --target x86_64-unknown-linux-gnu` → both Finished, 4 pre-existing
  dead-code warnings, 0 errors. `target/x86_64-unknown-linux-gnu/debug/xcl --version` →
  `xcl 0.1.0`.
- Manual real-child run of the built EH test with `XCL_EH_CRASH_CHILD=1` produced valid JSON:
  `SIGSEGV (signal 11) at signal-test.exe+0x2a; pc 0x5580ff9d302a; fault address 0x1000`;
  shell status 139 (SIGSEGV), no Rust panic.
- A scratch executable under `/tmp/opencode/xcl-eh-probe` linked to the current library
  exercised real SIGSEGV, SIGILL, and SIGFPE instructions plus a raised SIGBUS. Each wrote a
  report and terminated by its original signal: statuses 139, 132, 136, and 135 respectively,
  with no timeout. The SIGBUS probe used `raise(7)`, so its `si_addr` is not treated as a fault
  address proof.
- `rustfmt --edition 2021 --check --config skip_children=true crates/xcl/src/eh_linux.rs` →
  clean; `git diff 3bb3d34^ 3bb3d34 --check` → clean. Whole-file rustfmt on `report.rs`/`lib.rs`
  still reports pre-existing long-line/module-order formatting; unrelated lines were not
  reformatted.
- `cargo check -p xcl --target x86_64-pc-windows-gnu` → blocked before xcl type-checking in
  `libudis86-sys`: `x86_64-w64-mingw32-gcc` is absent. This remains step-11 work.

Verdict: pass. The two minor follow-ups are recorded above; neither blocks step 9b. The task
was already closed in the runtime store when this review arrived and remains closed.

## Verification Notes — step-09b synthetic PE entry (Builder)

Task `task-1795520928-f789` / `code-assist:linux-port:step-09b:synthetic-pe-entry`.
Commit: `7688d38` — Linux: execute synthetic PE entry with TLS lifecycle.

Landed:

- `loader_linux::run_main` validates the registered main EXE, attaches the calling thread, runs
  process TLS callbacks, calls the PE entry with the Windows x64 ABI, then runs process detach
  while TLS blocks are alive followed by thread detach and restoration of the previous `GS`.
- PE entry points and TLS callbacks use explicit `extern "win64"` on Linux; `extern "system"`
  would have used the host SysV ABI. Recorded as DEC-013.
- `load_main` rejects a main image with unresolved normal or delay imports before TLS install
  or module registration. The IAT is not faked and no null import is callable in this slice.
- The synthetic EXE emits real x64 code that reads its template through `gs:[0x58]`, records
  callback id/reason/template byte, changes its thread-local byte 7→8 in the entry, and returns
  `0x2a`. An exact child test process isolates process-wide TLS slot 0, `GS`, and module state
  from parallel tests. The test proves all nine lifecycle events, fresh-template cleanup, GS
  restoration, and per-thread isolation on two workers.
- Title PEB/argv and the PE-requested entry-thread stack are explicitly deferred to step 10d;
  the synthetic entry does not read its null PEB argument.

Test-first evidence:

- `cargo test -p xcl --lib loader::tests::runs_an_import_free_main_through_tls_and_entry --
  --nocapture` first failed to compile with E0425 (`run_main` absent).
- `cargo test -p xcl --lib
  loader::tests::main_image_with_imports_is_rejected_before_execution -- --nocapture` first
  failed because `load_main` accepted and registered the unresolved-import image; the guard
  then made it pass.

Final verification (2026-11-24, `linux` @ `7688d38`):

- `cargo test -p xcl --lib loader:: -- --test-threads=4` → 10 passed; 0 failed; 0 ignored;
  0 measured; 166 filtered out.
- `cargo test -p xcl --lib tls:: -- --test-threads=4` → 3 passed; 0 failed; 0 ignored;
  0 measured; 173 filtered out.
- `cargo test -p xcl --lib -- --test-threads=4` → 176 passed; 0 failed; 0 ignored; 0 measured;
  0 filtered out (10.96 s). The two opt-in sample tests no-op without env vars in this run; the
  loader retail sample was exercised separately below. No GPU test printed a skip.
- `cargo check --workspace --target x86_64-unknown-linux-gnu` → Finished; 4 pre-existing
  dead-code warnings; 0 errors.
- `cargo build --workspace --target x86_64-unknown-linux-gnu` → Finished; the same 4 warnings;
  0 errors.
- `XCL_SAMPLE_EXE=/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/game_scarlett_master.exe
  cargo test -p xcl --lib loader::tests::maps_the_retail_exe_when_sample_set -- --nocapture` →
  1 passed; mapped at `0x140000000`, size `0xb06d000`, entry `+0x56a1ec8`, 10 sections,
  2 TLS callbacks, `.text r-xp`. This is mapping only, not retail execution.
- `target/x86_64-unknown-linux-gnu/debug/xcl --version` → `xcl 0.1.0`.
- `rustfmt --edition 2021 --check crates/xcl/src/loader_linux.rs crates/xcl/src/tls_linux.rs` →
  clean; `git diff 7688d38^ 7688d38 --check` → clean.
- Windows cross-check not run: only non-Windows files changed, and the host still lacks the
  MSVC/mingw SDK/linker. Windows loader/TLS files are untouched.

Not claimed: retail execution, import/HLE resolution, game-DLL DllMain, PEB/argv, dedicated
entry stack, or C++/`.pdata` unwinding. Those remain in steps 10a/10d and later Phase-C work.

## Planner refresh (2026-11-24, `linux` @ `ed0fd32`, no game files on host)

- Queue: 40 closed, 11 open. `task ready --all` offers exactly two tasks: step-12
  retail-smoke (SKIPPED here — `/run/media/...` absent, never stall on it) and
  `linux:crt-copy` (unblocked, next §4.7a family). Steps 13–21 stay chained behind step-12;
  Phase D stays blocked until the synthetic title runs to `ExitProcess`.
- Tree: `cargo test -p xcl --lib` → 369 passed / 0 failed (read-only check, this host).
  Installed targets include `x86_64-pc-windows-gnu` + `-gnullvm`, so the Builder can
  cross-build `tests/testtitle` + `testgamedll` and use `crates/xcl/tests/e2e.rs`
  (`xcl_testtitle: done, failures: 0`) as the crt-copy regression gate.
- Next: Builder takes `linux:crt-copy` (plan.md step 12a). One commit, synthetic
  verification only.

## Planner refresh (2026-11-24, `linux` @ `f3bee90`, build.start, no game files on host)

- HEAD is `f3bee90` (PROMPT snapshot on top of `141768c`; code identical to `ed0fd32` + upstream `20cf130` sync). Prior refresh lines referencing `ed0fd32` updated to `f3bee90`; no phase flip.
- Queue: 40 closed, 11 open. `task-1795560125-71aa` (`linux:crt-copy`, plan.md step 12a) is `in_progress` since 23:38Z — the Builder owns it. `code-assist:linux-port:step-12:retail-smoke` is the only `ready --all` item but is SKIPPED here (`/run/media/...` absent); steps 13–21 stay chained behind step-12. Phase D stays blocked until the synthetic title runs to `ExitProcess`.
- No code changes in this planner pass. Next: Builder continues `linux:crt-copy` (NUL-safe `strcpy`/`strncpy`/`strcat`/`wcs*`/`wmemcpy`/`wmemmove` via ucrtbase HLE, one commit, synthetic verification only).

## Planner refresh (2026-11-25, `linux` @ `8153a44`, build.start, no game files on host)

- Queue verified: 1 in_progress (`linux:crt-copy` task-1795560125-71aa, Builder owns it — uncommitted work in `crt_linux.rs`/`hle_linux.rs`/`resolver_linux.rs` left untouched), 10 open (step-12 retail-smoke + steps 13–21 chained behind it). No missing tasks, no broken chains (step-12's `blocked_by` points at a pruned closed task and it correctly shows as the only formal ready item).
- No replan: docs (`context.md`/`plan.md` step 12a) already describe the crt-copy slice; further CRT families stay provisional until crt-copy lands and triage names the next one. Phase D/E/F remain blocked per PROMPT §4.7. Step-12 stays open but SKIPPED here (`/run/media/...` absent) — never stall on it.
- No code changes in this planner pass. Next: Builder continues `linux:crt-copy` to a one-commit landing with synthetic verification (focused `crt` tests + full lib suite + workspace check + cross-built title e2e regression gate).

## Verification Notes — step-12a crt-copy (Builder, 2026-11-25, `linux`)

Task `task-1795560125-71aa` / plan.md step 12a (`linux:crt-copy`).

Landed (one commit, no refactors):
- `crates/xcl/src/hle/crt_linux.rs`: NUL-safe win64 `strcpy`/`strncpy`/`strcat`/`strncat` + `wcscpy`/`wcsncpy`/`wcscat`/`wcsncat` + `wmemcpy`/`wmemmove` (overlap-safe `copy` vs `copy_nonoverlapping`, zero-length no-op returning `dst`), wired into `exports()`, plus `string_copy_and_concat_match_libc` unit test (pad/truncate/zero-length/overlap).
- `crates/xcl/src/hle_linux.rs`: 10 new names in the HLE spot list.
- `crates/xcl/src/resolver_linux.rs`: `api-ms-win-crt-string-l1-1-0` contract checks + bundled-DLL (`msvcp140`/`vcruntime140`) forwarding checks, plus direct win64 `strcpy`/`wmemmove` callable checks (return `dst`, overlap-safe).

Verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 18 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 371 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → Finished, 6 pre-existing warnings, 0 errors.
- Cross-built synthetic title (prebuilt `target/debug/xcl_testtitle.exe` + `testgame_mw.dll`, fresh temp game+data dirs): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 16`, `ExitProcess(0x10)`. Baseline without this slice on a fresh dir is also 16 (same GPU/null-device + first-run overlay quirks: real-GPU triangle/Present, sparse VRAM, DirectStorage, MFPlat, XG texture pools). No CRT regression; title reaches `ExitProcess`. A rerun on a reused dir drops to 14 (overlay already populated), so fresh-dir counts are the comparator.

## Critic review — step-12a crt-copy (Fresh-Eyes, 2026-11-25, `linux` @ `38f95fd`)

Task `task-1795560125-71aa` (`linux:crt-copy`) — PASS, closed.

Independent re-verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 18 passed; 0 failed (includes `string_copy_and_concat_match_libc` + both resolver forwarding tests).
- `cargo test -p xcl --lib -- --test-threads=4` → 371 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors, 6 pre-existing warnings; `cargo clippy -p xcl --lib` → 0 errors (502 pre-existing warnings only).
- Synthetic title (`target/debug/xcl_testtitle.exe` + `testgame_mw.dll`, fresh temp game + `XCL_DATA_ROOT`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34`, `imports_by_source` includes `hle-cross:ucrtbase.dll`. All 14 FAILs are the known null-device categories (real-GPU triangle/Present, command-list writes, sparse VRAM/coherent window, descriptor-heap reservation, depth bias, raytracing tail, DirectStorage, XG title/streaming textures, MFPlat); no CRT/DllMain failure, title reaches `ExitProcess`. A run via the default data root on the same binaries reported 16 (same categories + first-run overlay variance), so the 14-vs-16 delta is environment/overlay state, not a CRT regression.

Requirements: met — 10 NUL-safe win64 exports (`strcpy`/`strncpy`/`strcat`/`strncat`/`wcscpy`/`wcsncpy`/`wcscat`/`wcsncat`/`wmemcpy`/`wmemmove`) with `dst` return, UCRT pad/truncate/NUL semantics, overlap-safe wide block copy, wired through `exports()` + HLE spot list + `api-ms-win-crt-string` contract and bundled-DLL resolver checks + unit test. One commit, no refactors.

## Follow-ups (minor, non-blocking)
- `plan.md` step 12a acceptance still says the title reports `failures: 0`; on Linux null-device the comparator is 14–16 known failures (Windows `e2e.rs` expects 0 with a real GPU). Worth a plan.md wording touch, not this slice.
- `strncpy` pad / `strncat` re-terminate / `wmemmove` overlap edge cases are covered; null-dest fault parity with `memcpy` holds by construction (deref faults, never a NULL-stub return) with no explicit null test — same as the prior slices.

## Verification Notes — step-12b crt-search-dup (Builder, 2026-11-25, `linux`)

Task `task-1795564589-a185` (`linux:crt-search-dup`, plan.md §4.7a next family).
Retail-smoke `task-1795516176-5dde` SKIPPED here: `/run/media/...` absent (verified `ls` both dirs → No such file or directory), game folder stays untouched; synthetic title is the e2e gate.

Landed (one commit, no refactors):
- `crates/xcl/src/hle/crt_linux.rs`: NUL-safe win64 `strstr`/`strrchr`/`strdup`+`_strdup`/`wcschr`/`wcsstr`/`wcsdup_impl` (exported as `_wcsdup`+`wcsdup`), wired into `exports()`, plus `string_search_and_dup_match_libc` unit test (empty-needle returns haystack, NUL terminator find, miss NULL, dup owned-copy + free).
- `crates/xcl/src/hle_linux.rs`: 8 new names in the CRT spot list.
- `crates/xcl/src/resolver_linux.rs`: `api-ms-win-crt-string` contract checks + bundled-DLL (`msvcp140`/`vcruntime140`) forwarding checks for the new names.

Verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 19 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 372 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors, 6 pre-existing warnings; `cargo clippy -p xcl --lib` → 0 errors (508 pre-existing warnings only).
- Synthetic title (prebuilt `target/debug/xcl_testtitle.exe` + `testgame_mw.dll`, proper `tests/data/testgame` game dir + fresh `XCL_DATA_ROOT`, `XCL_DUMP_SHADERS=1`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34`, `imports_by_source` includes `hle-cross:ucrtbase.dll: 68`. All 14 FAILs are the known null-device categories; no CRT regression.

## Critic review — step-12b crt-search-dup (Fresh-Eyes, 2026-11-25, `linux` @ `761c59a`)

Task `task-1795564589-a185` (`linux:crt-search-dup`) — PASS, closed.

Independent re-verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 19 passed; 0 failed (includes `string_search_and_dup_match_libc` + both resolver forwarding tests).
- `cargo test -p xcl --lib -- --test-threads=4` → 372 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors, 6 pre-existing warnings; `cargo clippy -p xcl --lib` → 0 errors (508 pre-existing warnings only); `git diff HEAD~1 HEAD --check` clean, no unrelated-line reformat.
- Synthetic title (copied `tests/data/testgame` + `target/debug/xcl_testtitle.exe`/`testgame_mw.dll` into fresh temp game dir): without `XCL_DUMP_SHADERS` → `xcl_testtitle: done, failures: 16`, `ExitProcess(0x10)`; with fresh `XCL_DATA_ROOT` + `XCL_DUMP_SHADERS=1` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34` incl. `XclDefinitelyMissingFunction`, `imports_by_source` includes `hle-cross:ucrtbase.dll: 68`. All FAILs are the known null-device + first-run overlay categories (real-GPU triangle/Present, command-list writes, sparse VRAM/coherent window, descriptor-heap reservation, depth bias, raytracing tail, DirectStorage, XG title/streaming textures, MFPlat, plus files-delete/PLS-create overlay variance fresh-vs-reused); no CRT/DllMain failure, title reaches `ExitProcess`.

Requirements: met — 8 NUL-safe win64 exports (`strstr`/`strrchr`/`strdup`+`_strdup` aliased to one fn/`wcschr`/`wcsstr`/`wcsdup_impl` exported as `_wcsdup`+`wcsdup`) with empty-needle-returns-haystack, NUL-terminator find, miss-NULL, and owned heap dup via CRT `malloc`, wired through `exports()` + HLE spot list + `api-ms-win-crt-string` contract and bundled-DLL resolver checks + unit test. One commit (`761c59a`), no refactors, Linux-only files so Windows untouched.

## Follow-ups (minor, non-blocking)
- Resolver tests sample the new family (bundled list covers `_strdup` but not `wcsdup`; `api-ms-win-crt-string` list covers `_wcsdup` but not `wcsdup`) — same subset-sampling as prior slices, routing is generic via `find_any`, so not blocking; consider adding `wcsdup` to both lists next time the file is touched.
- `plan.md` step 12a acceptance still says the title reports `failures: 0`; on Linux null-device the comparator is 14–16 known failures. Worth a plan.md wording touch, not this slice.

## Planner revise — next CRT family triage (2026-11-25, `linux` @ `761c59a`)

Event `plan.revise`: `linux:crt-search-dup` passed review and closed (372 lib tests, title
reaches `ExitProcess` with only known null-device failures). Queue: 1 in_progress
(step-12 retail-smoke, SKIPPED here — `/run/media/...` absent) + 9 open (steps 13–21 chained
behind step-12); `task ready` is empty, so the next behavior slice needs a new unblocked task.

Triage (evidence, not speculation): `llvm-readobj --coff-imports` on the cross-built
`target/debug/xcl_testtitle.exe` (187 imports) and `target/debug/testgame_mw.dll`, diffed
against the 108 export names in `hle/crt_linux.rs`, leaves exactly 4 MISS UCRT imports:

- `api-ms-win-crt-stdio-l1-1-0.dll!__stdio_common_vfprintf` (both binaries)
- `api-ms-win-crt-convert-l1-1-0.dll!mbrtowc` / `!wcrtomb` (both binaries)
- `api-ms-win-crt-runtime-l1-1-0.dll!_execute_onexit_table` (game DLL only)

New task `linux:crt-convert-onexit` (plan.md step 12c, recorded as DEC-016): `mbrtowc`/
`wcrtomb` + `_execute_onexit_table`, one commit, synthetic verification only.
`__stdio_common_vfprintf` stays provisional after 12c (needs a win64 `va_list`
format-string engine over `CrtFile`). Docs updated in the same pass: plan.md steps
12a/12b marked LANDED + new 12c section + 12a acceptance wording fixed to the 14–16
null-device comparator; context.md current-state refresh; DEC-016 recorded.

## Verification Notes — step-12c crt-convert-onexit (Builder, 2026-11-25, `linux`)

Task `task-1795564858-c38a` (`linux:crt-convert-onexit`, plan.md step 12c).
Retail-smoke SKIPPED here: `/run/media/...` absent (no game files on host); synthetic title is the e2e gate.

Landed (one commit, no refactors):
- `crates/xcl/src/hle/crt_linux.rs`: win64 `mbrtowc`/`wcrtomb` (ASCII fast path + UTF-8, `(size_t)-1` on bad sequence with `errno=42`, `-2` on incomplete, NUL/`s==NULL` handling; `wcrtomb` surrogate pairing via `*ps`, astral `mbrtowc` stores the lead surrogate) + `_execute_onexit_table` (no-op `0` consistent with existing register/initialize successes), wired into `exports()`, plus `convert_and_onexit_match_ucrt` unit test.
- `crates/xcl/src/hle_linux.rs`: 3 new names in the CRT spot list.
- `crates/xcl/src/resolver_linux.rs`: `api-ms-win-crt-convert` + `api-ms-win-crt-runtime` contract checks and bundled-DLL forwarding checks; also added `wcsdup` to both lists (12b review follow-up: bundled list covered `_strdup` but not `wcsdup`; contract list covered `_wcsdup` but not `wcsdup`).

Verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 20 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 373 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors, 6 pre-existing warnings; `cargo clippy -p xcl --lib` → 0 errors (only pre-existing `missing_safety_doc`/style warnings).
- Synthetic title (copied `tests/data/testgame` + `target/debug/xcl_testtitle.exe`/`testgame_mw.dll` into fresh temp game dir, fresh `XCL_DATA_ROOT`, `XCL_DUMP_SHADERS=1`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34`, `imports_by_source` includes `hle-cross:ucrtbase.dll: 68`. All 14 FAILs are the known null-device categories; no CRT regression.

## Critic review — step-12c crt-convert-onexit (Fresh-Eyes, 2026-11-25, `linux` @ `24d6820`)

Task `task-1795564858-c38a` (`linux:crt-convert-onexit`) — PASS, closed.

Independent re-verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 20 passed; 0 failed (includes `convert_and_onexit_match_ucrt` + both resolver forwarding tests with the new convert/runtime entries and the `wcsdup` follow-up additions).
- `cargo test -p xcl --lib -- --test-threads=4` → 373 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors, 6 pre-existing warnings; `cargo clippy -p xcl --lib` → 0 errors; `git diff HEAD~1 HEAD --check` clean.
- Synthetic title (fresh temp game dir from `tests/data/testgame` + prebuilt exes, fresh `XCL_DATA_ROOT`, `XCL_DUMP_SHADERS=1`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34` incl. `XclDefinitelyMissingFunction`, `hle-cross:ucrtbase.dll: 68`. All 14 FAILs are the known null-device categories; no CRT regression.
- Abuse probe (temporary integration test, since removed): overlong `E0 80 AF`, surrogate `ED A0 80`, out-of-range `F4 90 80 80` → `-1` + `errno 42`; truncated 4-byte with `n=2` → `-2` with errno untouched; valid astral → lead surrogate `0xD83D` + len 4; `wcrtomb` lone high without `ps`, pending-high + BMP, low without pending → `-1` + `errno 42` with pending cleared; `_execute_onexit_table(0)` and `(0xdeadbeef)` → `0`, no crash. 1 passed; 0 failed.

Requirements: met — win64 `mbrtowc`/`wcrtomb` (ASCII fast path + strict UTF-8 via shared `utf8_decode_one`: `C0/C1` + continuation-lead rejection, `F5+` rejection, `n<len` → `-2`, overlong/surrogate/`>0x10FFFF` → `-1` with `errno=42`, NUL/`s==NULL` handling, surrogate pairing through `*ps`) + `_execute_onexit_table` no-op `0`, wired through `exports()` + HLE spot list + `api-ms-win-crt-convert`/`-runtime` contract and bundled-DLL resolver checks + unit test + the 12b `wcsdup` follow-up in both resolver lists. One commit (`24d6820`), no refactors, Linux-only files so Windows untouched. `llvm-readobj` confirms the only remaining MISS UCRT import in both synthetic binaries is `__stdio_common_vfprintf`.

## Follow-ups (minor, non-blocking)
- `mbrtowc` ignores `ps` and stores only the lead surrogate for astral input (no trail-half state); correct for the single-`wchar_t` destination and the stateless title path, but a restartable stream over astral text would need `ps` stashing. Revisit only if a caller shows it.
- `mbrtowc` takes `ps` as `*mut u8` while `wcrtomb` takes `*mut u32` for the same `mbstate_t`; harmless (both treat it opaquely) but worth unifying next time the file is touched.

## Planner revise — vfprintf triage (2026-11-25, `linux` @ `fdeb6d0`)

Event `plan.revise`: `linux:crt-convert-onexit` passed review and closed (373 lib tests, title
reaches `ExitProcess(0xe)` with only known null-device failures). Queue: 1 in_progress
(step-12 retail-smoke, SKIPPED here — `/run/media/...` absent) + 9 open (steps 13–21 chained
behind step-12); `task ready` is empty, so the next behavior slice needs a new unblocked task.

Triage (evidence, not speculation): the 12c review's `llvm-readobj --coff-imports` audit on the
cross-built `target/debug/xcl_testtitle.exe` (187 imports) vs `crt_linux.rs` exports leaves exactly
1 MISS UCRT import: `api-ms-win-crt-stdio-l1-1-0.dll!__stdio_common_vfprintf` (both binaries;
`mbrtowc`/`wcrtomb` now resolve after 12c — the stale prebuilt exe still lists them, but the
source export exists). It arrives via mingw/Rust `format!` paths (observed verbs in the binary:
`%s`, `%g`, `%p`, `%llx`, `%%`), so the slice is a small win64 `va_list` (`char*` stack slots)
format engine writing into `CrtFile`, with `%n` never honored.

New task `linux:crt-vfprintf` (`task-1795565262-e15d`, plan.md step 12d) is the unblocked next
task. Docs updated in the same pass: plan.md 12c marked LANDED + new 12d section; context.md
current-state refresh. Steps 13–21 stay chained behind SKIPPED step-12; Phase D/E/F remain blocked
per PROMPT §4.7. No code changes in this planner pass.

## Verification Notes — step-12d crt-vfprintf (Builder, 2026-11-25, `linux`)

Task `task-1795565262-e15d` (`linux:crt-vfprintf`, plan.md step 12d).
Retail-smoke SKIPPED here: `/run/media/...` absent (no game files on host); synthetic title is the e2e gate.

Landed (one commit, no refactors):
- `crates/xcl/src/hle/crt_linux.rs`: win64 `__stdio_common_vfprintf(options, stream,
  format, locale, arglist)` — format engine over win64 `va_list` (`char*` to 8-byte
  slots) writing into `CrtFile` via the `fwrite` path. Verbs `%s`/`%c`/`%d`/`%i`/`%u`/
  `%x`/`%X`/`%o`/`%p`/`%%` with width/precision/`*` and `ll`/`l`/`h`/`hh`/`z`/`j`/`t`/
  `I32`/`I64` lengths (Windows sizes: `l` is 32-bit), float `%f`/`%e`/`%g`/`%a` (+upper)
  reading `double` slots, `%ls`/`%S`/`%lc`/`%C` wide re-encoded as UTF-8. `%n` consumes
  its slot and writes nothing; `options`/`locale` accepted and ignored (C locale);
  NULL stream/format (or NULL arglist with args needed) and unknown verbs return `-1`;
  lone trailing `%` emits the prefix. Plus `vfprintf_formats_into_crt_files` unit test
  (ints/width/star/signs/floats/lengths/wide/NULL-string, `%n` no-write with sentinel,
  truncated formats, unknown verb, NULL stream/format/arglist, ignored options/locale).
- `crates/xcl/src/hle_linux.rs`: 1 new name in the CRT spot list.
- `crates/xcl/src/resolver_linux.rs`: `api-ms-win-crt-stdio` contract check and
  `vcruntime140`/`msvcp140` bundled-DLL forwarding checks.

Verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 21 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 374 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors (pre-existing warnings only); `cargo clippy -p xcl --lib` → 0 errors (only pre-existing `missing_safety_doc`/style warnings); `git diff --check` clean.
- Synthetic title (copied `tests/data/testgame` + `target/debug/xcl_testtitle.exe`/`testgame_mw.dll` into fresh temp game dir, fresh `XCL_DATA_ROOT`, `XCL_DUMP_SHADERS=1`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34` (same known GPU/null-device + `FormatMessageW`/`XclDefinitelyMissingFunction`/DirectStorage/MFPlat set; no CRT stub), `imports_by_source` includes `hle-cross:ucrtbase.dll: 75`. All 14 FAILs are the known null-device categories; no CRT regression.
- `llvm-readobj --coff-imports` on both synthetic binaries vs the 109 `crt_linux` export names: 0 MISS UCRT imports — this closes the last MISS from the 12c audit.

## Critic review — step-12d crt-vfprintf (Fresh-Eyes, 2026-11-25, `linux` @ `b4956ba`)

Task `task-1795565262-e15d` (`linux:crt-vfprintf`) — PASS, closed.

Independent re-verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib crt -- --test-threads=4` → 21 passed; 0 failed (includes `vfprintf_formats_into_crt_files` + both resolver forwarding tests with the new stdio entry).
- `cargo test -p xcl --lib -- --test-threads=4` → 374 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors, 6 pre-existing warnings; `cargo clippy -p xcl --lib` → 0 errors (513 pre-existing warnings only); `git diff HEAD~1 HEAD --check` clean.
- Synthetic title (fresh temp game dir from `tests/data/testgame` + prebuilt exes, fresh `XCL_DATA_ROOT`, `XCL_DUMP_SHADERS=1`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `crash: null`, `exit_code: 14`, `called_stubs: 34` incl. `XclDefinitelyMissingFunction`, `hle-cross:ucrtbase.dll: 75`. All 14 FAILs are the known null-device categories; no CRT regression.
- `llvm-readobj --coff-imports` audit re-run here: 46 CRT imports (exe) + 29 (dll) vs 109 `crt_linux` export names → 0 MISS UCRT imports in both binaries. Last MISS from the 12c audit is closed.
- Abuse coverage is in-tree (no temp test needed): truncated `abc%`/`abc%5` → prefix only, `%q` → `-1`, `%d` with NULL arglist → `-1`, NULL stream/format → `-1`, `%n` with live sentinel `0x12345678` → bytes `ABCD`, rc 4, sentinel untouched.

Requirements: met — win64 `__stdio_common_vfprintf` (8-byte slot `VaReader`, NULL-list fails closed; width/precision/`*` with `MAX_FIELD` guard; `ll`/`l`(32-bit)/`h`/`hh`/`z`/`j`/`t`/`I32`/`I64` lengths; `%d/%i/%u/%x/%X/%o/%p/%c/%s/%%` + float `%f/%e/%g/%a` (+upper) via `double` slots + wide `%ls/%S/%lc/%C` as UTF-8; `%n` consumes slot and writes nothing; options/locale accepted-ignored; NULL stream/format and unknown verbs → `-1`; lone `%` emits prefix) writing through the `fwrite` path, wired through `exports()` + HLE spot list + `api-ms-win-crt-stdio` contract and `vcruntime140`/`msvcp140` bundled-DLL checks + unit test. One commit (`b4956ba`), no refactors, Linux-only files so Windows untouched.

## Follow-ups (minor, non-blocking)
- Unknown-verb failure discards the already-rendered prefix (returns `-1`, writes nothing); real UCRT would report `-1` having streamed the prefix. Safe HLE simplification, but worth documenting if a caller depends on partial output.
- Unit test covers `hhd/hd/ld/lld/zd/hhx/hx/lx/llx/zx` but not bare-`I`/`I32`/`I64` spellings; routing is shared (`Ll`/`L`) so risk is low — cover them next time the file is touched.

## Verification Notes — step-12e kernelx-format-message (Builder, 2026-11-25, `linux`)

Task `task-1795566077-cb3a` (`linux:format-message`, plan.md step 12e).
Retail-smoke SKIPPED here: `/run/media/...` absent (no game files on host); synthetic title is the e2e gate.

Landed (one commit, no refactors):
- `crates/xcl/src/hle/kernelx_linux.rs`: win64 `FormatMessageW` + `FormatMessageA`
  (ANSI twin over the same core, UTF-8 on Linux). `FROM_SYSTEM` maps Win32 codes to
  English `\r\n` text (plus `HRESULT_FROM_WIN32` low-word decode and `E_NOTIMPL`);
  unknown codes fall back to `Unknown error %u (0x%08X).\r\n` instead of failing.
  `FROM_STRING` expands `%1..%99` with optional `!s!`/`!d!`/`!x!`/`!X!`/`!o!`/`!u!`/
  `!c!`/`!p!` specs (`%%` → `%`, `%0` terminates); empty/`s` inserts read guest
  wide (`W`) or ANSI (`A`) strings, integer specs format the slot value
  (`d/i` as 32-bit signed, others full-width). `ARGUMENT_ARRAY` reads the array
  directly, otherwise one indirection through the `va_list*`. `ALLOCATE_BUFFER`
  allocates via the process heap and writes the pointer back; plain truncation
  returns 0 with `ERROR_INSUFFICIENT_BUFFER`; NULL output without
  `ALLOCATE_BUFFER`, NULL `FROM_STRING` source, `FROM_HMODULE`-only, or no source
  flag fail with `ERROR_INVALID_PARAMETER`/`ERROR_MR_MID_NOT_FOUND`, never a crash.
  Plus `format_message_system_string_allocate_and_truncate` unit test (system-code
  message, `%1:%2!` + `%1!d!` argument-array inserts, allocate-buffer + heap free,
  truncation, unknown-code fallback, NULL output, ANSI twin).
- `crates/xcl/src/hle_linux.rs`: `FormatMessageW`/`FormatMessageA` in the KERNELX
  spot-check list.
- `crates/xcl/src/resolver_linux.rs`: `kernel32.dll` + `api-ms-win-core-localization`
  contract checks for both names.

Verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib kernelx_linux::tests::format_message -- --test-threads=4` → 1 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 375 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors (pre-existing warnings only); `cargo clippy -p xcl --lib` → 0 errors (pre-existing warnings only); `git diff --check` clean.
- Synthetic title (copied `tests/data/testgame` + `target/debug/xcl_testtitle.exe`/`testgame_mw.dll` into fresh `/tmp/fmt-gate/game`, fresh `XCL_DATA_ROOT=/tmp/fmt-gate/data`): `XCL_DATA_ROOT=... ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)` (rc=14), `report.json` `called_stubs: 33` with no `FormatMessageW` entry and no `[STUB] ... FormatMessageW` line (was 34 with the stub). The `PresentX` FAIL line now shows formatted text (`The operation completed successfully. (0x00000000)`) proving the HLE served the call. All 14 FAILs are the known null-device categories; no new failure.

## Critic review — step-12e kernelx-format-message (Fresh-Eyes, 2026-11-25, `linux` @ `43bdabc`)

Task `task-1795566077-cb3a` (`linux:format-message`) — PASS, already closed.

Independent re-verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib kernelx_linux::tests::format_message -- --test-threads=4` → 1 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 375 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors (pre-existing warnings only); `cargo clippy -p xcl --lib` → 0 errors (pre-existing warnings only).
- Resolver/HLE spot checks are in-tree: `kernelx_core_imports_resolve_to_hle` covers `kernel32!FormatMessageW/A` + the `api-ms-win-core-localization` contract, `hle_linux` spot list covers both names — both pass inside the 375.
- Synthetic title (fresh `/tmp/fmt-critic/game` from `tests/data/testgame` + prebuilt exes, fresh `XCL_DATA_ROOT=/tmp/fmt-critic/data`): `XCL_DATA_ROOT=... ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 14`, `ExitProcess(0xe)`, `report.json` `called_stubs: 33` with zero `FormatMessage` entries and zero `[STUB] FormatMessageW` lines (was 34 with the stub). All 14 FAILs are the known null-device categories; no new failure.
- Adversarial path exercised for real: `PresentX` FAIL line now renders formatted text through the HLE instead of an empty string, proving the title's failure-path formatting call is served (not just resolved).

Requirements: met — win64 `FormatMessageW` + `FormatMessageA` (ANSI twin over shared helpers) in `hle/kernelx_linux.rs`, wired through `exports()` so the `kernel32.dll` route resolves `Hle(kernelx)`; `FROM_SYSTEM` table + `HRESULT_FROM_WIN32` low-word decode + `E_NOTIMPL` + unknown-code fallback text; `FROM_STRING` `%1..%99` with `!s!/!d!/!x!/!X!/!o!/!u!/!c!/!p!` (`%%`, `%0` terminator); `ARGUMENT_ARRAY` direct vs `va_list*` double-deref; `ALLOCATE_BUFFER` via process heap with pointer write-back; truncation → `ERROR_INSUFFICIENT_BUFFER`, NULL output without allocate → `ERROR_INVALID_PARAMETER`. One commit (`43bdabc`), Linux-only files + progress.md, no refactors.

## Follow-ups (minor, non-blocking)
- `format_truncate_at_terminator` scans raw bytes for `%0` before `%%` unescaping, so `"%%0"` truncates to `"%"` instead of rendering `"%0"`. Safe direction (shorter string, still NUL-terminated, no crash) on unrealistic input; fix next time the function is touched.

## Planner revise — mfplat triage (2026-11-25, `linux` @ `6473823`)

Event `plan.revise`: `linux:format-message` passed review and closed (375 lib tests, title
reaches `ExitProcess(0xe)` with only known null-device failures, `called_stubs: 33`
with zero `FormatMessage` entries vs 34 before). Queue: 1 in_progress
(step-12 retail-smoke, SKIPPED here — `/run/media/...` absent) + 9 open (steps 13–21 chained
behind step-12); `task ready` is empty, so the next behavior slice needs a new unblocked task.

Triage (evidence, not speculation): fresh title run here
(`XCL_DATA_ROOT=/tmp/fmt-critic/data ./target/debug/xcl run /tmp/fmt-critic/game`;
`report.json` `exit_code: 14`, `called_stubs: 33`) plus `xcl.log` `[STUB]` lines shows
the only non-GPU, non-probe stubs the title actually calls are `mfplat`
(`MFStartup`/`MFCreateDXGIDeviceManager`/`MFShutdown` x1 each — `MFCreate` stub
returns `S_OK` with a null manager so the title fails before `Reset`, which is
imported but never reached) and `xcrdapi` `DStorageGetFactory` x1, plus the expected
`XclDefinitelyMissingFunction` probe x1; the other ~27 are Phase-D D3D12 console
slots. Kernel32 (`CreateFileMappingA`, `WriteConsoleW`, …) and `oleaut32` names sit
in `stubbed_imports` with zero `called_stubs` entries — per §4.7b they stay stubs.

New task `linux:mfplat-headless` (plan.md step 12f, recorded as DEC-018): headless
`MFStartup`/`MFShutdown`/`MFCreateDXGIDeviceManager`/`MFResetDXGIDeviceManagerX` +
manager vtable in `mfplat_linux.rs` (GameInput/XAudio2 null-object pattern), one
commit, synthetic verification only. `DStorageGetFactory` stays provisional after
12f (larger factory/queue/status/fence surface, own slice). Docs updated in the same
pass: plan.md 12e marked LANDED + new 12f section + Plan Changes entry; context.md
current-state refresh; DEC-018 recorded.

## Verification Notes — step-12f mfplat-headless (Builder, 2026-11-25, `linux`)

Task `task-1795566796-4e2a` (`linux:mfplat-headless`, plan.md step 12f).
Retail-smoke SKIPPED here: `/run/media/...` absent (no game files on host); synthetic title is the e2e gate.

Landed (one commit, no refactors):
- `crates/xcl/src/hle/mfplat_linux.rs` (new): headless win64 `MFStartup` (any version/flags → `S_OK`, idempotent) + `MFShutdown` (`S_OK`) + `MFCreateDXGIDeviceManager` (NULL → `E_POINTER`; else fresh token via monotonic counter + static manager object) + `MFResetDXGIDeviceManagerX` (NULL → `E_POINTER`, foreign manager/stale token → `E_INVALIDARG`, current token → records device + `S_OK`). Static manager vtable (12 slots, `Release` clamped at 1 like GameInput, `OpenDeviceHandle`/`CloseDeviceHandle`/`TestDevice` `S_OK` at title slots 6/3/8, QI/AddRef real, rest `E_NOTIMPL` logging stubs via `make_stub_info` — never NULL). Plus unit test (idempotency, NULL closed, stale vs current token, second-create invalidates first, open/test/close, release clamp, stub slot non-null).
- `crates/xcl/src/hle_linux.rs`: `mfplat_linux` module + merge into `mfplat_exports()` (4 real addresses, not stub metadata) + `MFPLAT` in the implemented branch + `MFStartup`/`MFShutdown`/`MFCreateDXGIDeviceManager`/`MFResetDXGIDeviceManagerX` spot checks.
- `crates/xcl/src/resolver_linux.rs`: `mfplat_imports_resolve_to_hle` test (all four names via `mfplat.dll` resolve `Hle`, callable, non-stub + direct create/reset ABI check).

Verification (this host, no game files — retail SKIPPED):
- `cargo test -p xcl --lib mfplat -- --test-threads=4` → 3 passed; 0 failed.
- `cargo test -p xcl --lib -- --test-threads=4` → 378 passed; 0 failed; 0 ignored.
- `cargo check --workspace` → 0 errors (pre-existing warnings only); `cargo clippy -p xcl --lib` → 0 errors (pre-existing warnings only); `git diff --check` clean.
- Synthetic title (copied `tests/data/testgame` + `target/debug/xcl_testtitle.exe`/`testgame_mw.dll` into fresh `/tmp/mfplat-fresh2/game`, fresh `XCL_DATA_ROOT=/tmp/mfplat-fresh2/data`, `XCL_DUMP_SHADERS=1`): `timeout 120 ./target/debug/xcl run "$GAME" -- --hello` → `xcl_testtitle: done, failures: 13`, `ExitProcess(0xd)`, `report.json` `called_stubs: 30` with zero `mfplat` entries (was 33) and zero `[STUB] mfplat` lines (log shows `MFStartup`/`MFCreateDXGIDeviceManager`/`MFResetDXGIDeviceManagerX bound`/`MFShutdown` INFO). `[PASS] MFPlat` (was FAIL). All 13 remaining FAILs are the known null-device categories (D3D12 slots + DStorage + probe); no new failure. (A run missing `testgame_mw.dll` in the game dir keeps `mw_attached` as a stub and reports 31/14 — game-dir setup, not HLE.)
