# AGENTS.md — linux branch

Goal of this branch: a **Linux static recompilation** of XCL — one static
Linux exe built from this repo. `main` stays Windows-only; this branch
diverges where needed. Do not merge Windows-only assumptions back in
without a `cfg`.

## Game files (do not copy into the repo, do not commit)

- `/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/` — full
  unpacked title (`game_scarlett_master.exe`, `*.dll`, `*.rpf`,
  `MicrosoftGame.Config`).
- `/run/media/anon/12TB_B/leonida-work/` — working extracts
  (`shaders/`, `exe-disasm/`, `*-carve/`, `ttpool*`, `intro_demo/`).

Reference them in place. Point opt-in sample tests at them via
`XCL_SAMPLE_*` env vars (same pattern as `main`); without the vars the
tests must no-op and pass. Never commit game data, dumps, console SDK
headers/binaries, keys, or DRM circumvention.

## Codebase map

- `crates/xcl/src/`: `loader.rs` + `pe.rs` (PE mapping), `tls.rs`,
  `eh.rs`, `resolver.rs` + `hle/` (kernelx, xgameruntime, d3d12x, xaudio2),
  `vfs.rs` + `vfs_hooks.rs`, `gpu/` (`rdna2*.rs`, `xbox.rs`, `shader.rs`,
  `pack.rs`, `recomp/` = RDNA2→HLSL emitter, `dxc.rs`, `runner.rs`),
  `host.rs`, `launcher.rs`, `report.rs`, `inspect.rs`, `shaders.rs`,
  `rpf*.rs`, `stubs.rs`.
- `crates/xcl/build.rs`: hashes translator sources for the shader disk
  cache — keep the source list accurate when moving emitter files.
- `tests/testtitle` + `tests/testgamedll`: our own console title for e2e.
- `scripts/`: `gen_rdna2.py`, `gen_d3d12_layout.py`, `gen_tiling.py`,
  `gen_gdk_table.py` — generated `*_spec.rs` / tables are never edited by
  hand.

## Working practices

- One logical change per commit, small diffs. Do what was asked, nothing
  more: no refactors, renames, reformatting, or new deps on the side.
- Follow the module around you; reuse existing helpers instead of adding
  layers. Format only the lines you touch (`cargo fmt` on the whole tree
  is off-limits: it is not rustfmt-clean).
- `windows` / `windows-sys` deps are Windows-only: every use gets a
  `cfg(windows)` gate plus a Linux path (stub, libc, or Vulkan/ALSA).
  New deps must be GPL-2.0-compatible and static-musal-friendly (no
  LGPL-3.0 WinDurango ports, no pure-Apache-2.0 / GPL-3-only code).
- Fast iteration: `cargo test -p xcl --lib <filter>` for one area,
  `cargo build --workspace` / `cargo test --workspace -- --test-threads=4`
  for the full check. GPU tests skip with `SKIPPED <test>: <reason>` when
  the adapter can't run them — report skips, don't count them as coverage.
- Run tests before claiming they pass; report exact command + counts
  (passed/failed/ignored/skipped). If it wasn't built/tested, say so.

## Efficient static Linux exe

- Target `x86_64-unknown-linux-musl` for the final static binary; keep
  `x86_64-unknown-linux-gnu` working for fast iteration.
  `rustup target add x86_64-unknown-linux-musl` + a musl toolchain.
- Release profile for the shipped exe (tune in `Cargo.toml`, don't commit
  experiments):
  `opt-level = 3`, `lto = true` (final only; `thin` while iterating),
  `codegen-units = 1` (final only), `strip = true`, keep `debug = 2`
  during bring-up so crashes resolve to file:line, strip at packaging.
  Never set `panic = "abort"` without checking `eh.rs` / unwinding first.
- Binary diet: prefer `no_mangle` shims over new crates, gate debug dumps
  (`XCL_DUMP_SHADERS`, frame dumps, verbose `report.json` fields) behind
  env/`cfg(debug_assertions)` so release stays lean; check size with
  `ls -lh target/.../xcl` after each lever.
- Hot paths (recompiler emit, tiling decode, VFS read, command-list
  replay): no per-instruction allocation, reuse buffers, parallelize with
  the existing rayon-style chunks, keep the on-disk shader cache keyed on
  translator-source hash so rebuilds don't retranslate.
- Linux GPU path: RDNA2→SPIR-V/Vulkan replaces HLSL→DXIL/DXC; keep the
  emitter (`gpu/recomp/emit.rs`) backend-agnostic and push HLSL/DXIL and
  D3D12 specifics behind the Windows backend so the static exe links no
  `dxcompiler`/`d3d12` on Linux.

## Upstream sync

- On request, sync `linux` with upstream `main`: fetch upstream, merge or
  rebase as asked, fix conflicts, build/test as usual.
- Never bring back `CLAUDE.md` or `CONTRIBUTING.md` during a sync — they
  stay deleted on this branch. If a merge wants to re-add them, drop those
  hunks/files and keep the deletion.
- Same for `AGENTS.md`: keep this branch's Linux version. Do not overwrite
  it with upstream's Windows version; port only what still applies.
