# Context — Linux static recompilation of XCL

## Request

**Source type:** rough description + `PROMPT.md` (full work plan) on branch `linux`.

Goal: one **static Linux exe** (`x86_64-unknown-linux-musl`) built from this repo that can
inspect, recompile, and eventually run the Xbox Series X|S title next to the machine.
`main` is Windows-only; this branch diverges where needed. `AGENTS.md` is the short contract,
`PROMPT.md` the phase plan. Phases A–F:

- **A** `cargo check`/`cargo test` green on `x86_64-unknown-linux-gnu` without touching behavior.
- **B** `xcl inspect`, `xcl disasm`, `xcl shaders [--recomp]`, `xcl rpf` run natively on Linux
  against the real game files.
- **C** PE loader + static TLS + EH signal handler + resolver/HLE + VFS; `tests/testtitle` e2e.
- **D** RDNA2 → SPIR-V + Vulkan (not DXIL/D3D12); backend-neutral emitter; in-process SPIR-V.
- **E** kernelx threads/memory, XGameRuntime offline, cpal audio, evdev input.
- **F** static musl `release-final` exe, measured size and corpus time.

## Initial survey (2026-11-24, branch `linux` @ 6efdabc)

- `crates/xcl` is a single crate; `windows 0.62`, `windows-sys 0.61` and `retour 0.3` are
  unconditional dependencies in `crates/xcl/Cargo.toml`.
- Baseline: `cargo check -p xcl --target x86_64-unknown-linux-gnu` fails compiling `retour`
  (`"stdcall" is not a supported ABI`); 80 files under `crates/xcl/src` reference
  `windows`/`windows_sys::`.
- Game files present and read-only:
  - `/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/`
    (`game_scarlett_master.exe`, `MicrosoftGame.Config`, `shaders.rpf`, `*.rpf`, `*.dll`).
  - `/run/media/anon/12TB_B/leonida-work/` (`shaders/{dxbc,fspx,spirv}`, `exe-disasm/`,
    `*-carve/`, `ttpf*`, `intro_demo/`).
- Toolchain: rustc/cargo 1.100.0; installed target `x86_64-unknown-linux-gnu` only.
  No musl target, no Windows target installed; `lld`, `ld.lld`, `clang` present,
  `x86_64-w64-mingw32-gcc` absent.
- CLI entry (`main.rs`): `run`, `inspect`, `disasm`, `shaders`, `rpf`, `--version`, `--help`,
  plus an Explorer-style menu when started with no arguments. `.CRT$XLY` TLS callback and
  `extern "system"` at the top; Windows COM/comctl32/cpndlg32 menu code.
- Pure-logic candidates (compile with little or no Windows surface): `pe.rs`, `insn.rs`,
  `disasm.rs`, `inspect.rs` (needs resolver/HLE status only), `rpf.rs` (one `MoveFileExW`),
  `rpf_probe.rs`, `manifest.rs`, `config.rs`, `shaders.rs`, most of `gpu/recomp/emit.rs`,
  `gpu/rdna2*.rs`, `gpu/dxbc.rs`, `gpu/pack.rs`, `gpu/shader.rs`, `gpu/tiling*.rs`,
  `gpu/xbox.rs`, `hle/{opus,xma2,d3d12_layout,kbm}.rs`.
- Windows-only core: `loader.rs`, `modules.rs`, `tls.rs`, `eh.rs`, `resolver.rs`, `stubs.rs`,
  `hooks.rs`, `vfs.rs`, `vfs_hooks.rs`, `preflight.rs`, `report.rs`, `launcher.rs`, `host.rs`,
  `hle/*` implementations, `gpu/{adapter,agility,fence,texture,uma}.rs`,
  `gpu/recomp/{dxc,pso,runner}.rs`.
- `util::wide` is already portable (`encode_utf16`); codepage/ANSI helpers, `under_wine`,
  `log.rs` thread id, `launcher::data_root`, `host.rs` process memory and `rpf.rs` atomic
  replace are the Phase-A leaves.
- `crates/xcl/build.rs` hashes `TRANSLATOR_SOURCES` for the shader disk cache; any moved or
  added file under `gpu/recomp/` or translation-relevant `gpu/*.rs` must update that list.
- Tests: `crates/xcl/tests/e2e.rs` runs `xcl_testtitle.exe` built by
  `cargo build --workspace`; no `cfg(windows)` gate. `tests/testtitle` and
  `tests/testgamedll` use `windows` and `#[link(..., kind = "raw-dylib")]`, which do not
  compile for a Linux host target. `tests/data` has only LLVM ISA text and the fake game data
  folder — no prebuilt PE.
- `cargo check --workspace` is the Phase-A gate, so these two test crates must also check on
  Linux (gate the PE-only bodies; keep the Windows build path intact).

## Current state (planner refresh, 2026-11-24)

- Branch `linux` is at `f63470d`; steps 1–7 (build skeleton, platform leaves, all four offline
  tools, mmap loader, static TLS) are closed with independent review notes in `progress.md`.
- The clean-egret worktree carries uncommitted step-8 work: Linux VFS path translation and
  copy-on-write behavior in `crates/xcl/src/vfs.rs`, linked-in file shims in
  `crates/xcl/src/vfs_hooks_linux.rs`, and the non-Windows module exposure in `lib.rs`.
  Treat it as intentional work to review and complete, not as disposable scratch work.
- `windows`, `windows-sys`, and `retour` are Windows-target dependencies; Linux has `libc`.
  The host runtime beyond loader/TLS/VFS is still gated or stubbed for later Phase-C steps.
- Installed targets: `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`, and
  `x86_64-pc-windows-msvc`. The musl target and a working Windows cross-link toolchain are
  still missing; step 11/21 own those proofs.
- Runtime task queue already exists under stable keys `code-assist:linux-port:step-NN:*`.
  Step 8 is the first ready task; later tasks remain provisional and chained in plan order.
- The read-only survey found two execution gaps in the original queue: mapping did not yet
  execute PE code, and no task owned the Linux launcher/host `xcl run` path. Steps 9b and
  10e now cover those. It also confirmed the step-8 WIP needs explicit mount-traversal
  containment and that runtime unresolved imports need executable stub thunks, not null IAT
  entries.

## Current state (planner refresh, 2026-11-24, `linux` @ `f3bee90`)

- Steps 1–11 closed (11a–11s HLE slices, CRT forwarding `dd7a3a7` + `c4187df`/`b96aa6e`/
  `cf92746`, report parity `b45ca53`, clippy `ed0fd32`); `cargo test -p xcl --lib` 369/0.
- No game files on this host: step-12 retail-smoke is SKIPPED here; the synthetic title
  (`tests/testtitle` + `testgamedll`, `gnullvm`/`gnu` targets installed) is the e2e target.
- Next slice is plan.md step 12a (`linux:crt-copy`): copy/concat CRT family, one commit,
  synthetic verification. Phase D/E/F tasks stay chained behind step-12.

## Integration points

- `crates/xcl/src/lib.rs`: module list is the cfg gate boundary.
- `crates/xcl/src/main.rs`: TLS section/callback, Windows menu, `xcl run` dispatch.
- `crates/xcl/Cargo.toml`: move `windows`, `windows-sys`, `retour` under
  `[target.'cfg(windows)'.dependencies]`.
- `crates/xcl/src/util.rs`, `log.rs`, `launcher.rs`, `host.rs`, `rpf.rs`: Phase-A platform leaves.
- `crates/xcl/src/hle/mod.rs`: `Registry`, routes and module export tables.
- `crates/xcl/build.rs`: `TRANSLATOR_SOURCES`.
- `tests/testtitle`, `tests/testgamedll`: must check on Linux; PE artifacts are built for a
  Windows target, not the Linux host.

## Acceptance criteria (from PROMPT.md, verbatim gates)

- A: `cargo check -p xcl --target x86_64-unknown-linux-gnu` **and** `cargo check --workspace`
  green; no remaining `unimplemented_on_linux` outside stubs each marked `// LINUX-TODO:`
  with its phase.
- B: all four commands run against the real paths with sane output and no Windows DLL loads.
- C: loader/TLS/EH/resolver/VFS unit tests, then `tests/testtitle` e2e, then one retail
  `xcl run` smoke with triage through `report.json`/`xcl.log`.
- D: one known-good blit VS translated and run on Vulkan (position output checked), both
  subgroup widths tested, no external compiler in the shipped path.
- E: cpal audio, evdev input with unchanged mapping table, XDG paths for XGameRuntime.
- F: `file` says statically linked, `ldd` says not a dynamic executable, no `.so` beside the
  exe, Phase-B commands work from the musl binary, size + corpus time reported.

## Constraints

- GPL-2.0-only. New deps: MIT/BSD/zlib/dual MIT-or-Apache-2.0 (under MIT), static-musl
  friendly. No pure-Apache-2.0-only, MPL, or GPL-3-only crates.
- Game data stays in place and read-only; never commit game data, dumps, SDK headers/binaries,
  keys, or DRM circumvention. Sample tests use `XCL_SAMPLE_*` and no-op + pass when unset.
- Small diffs, one logical change per commit, no drive-by refactors/renames, no whole-tree
  `cargo fmt` (format only touched lines).
- Library paths never panic for unsupported OS features: return a clean `Err` with a
  `// LINUX-TODO: Phase X` comment.
- `panic = "unwind"` stays; `eh.rs`/C++ unwinding depends on it.
- Final exe is `x86_64-unknown-linux-musl`; system Vulkan/ALSA are runtime-`dlopen`ed, never
  bundled or linked.
