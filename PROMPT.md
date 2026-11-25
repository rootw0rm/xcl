# PROMPT.md — Linux static recompilation of XCL (for another LLM)

You are working on the `linux` branch of XCL. Goal: produce **one static
Linux exe** (`x86_64-unknown-linux-musl`) built from this repo that can
inspect, recompile, and eventually run the Xbox Series X|S title that lives
next to the machine. `main` is Windows-only; this branch diverges where
needed. Read `AGENTS.md` first — it is the short contract. This file is the
full work plan.

## Status snapshot (2026-11-24 — refresh this when a phase flips)

- `linux` @ `a776061` (merge of upstream `main` @ `6e1e2be`, plus Linux
  fixes; pushed to `origin/linux`).
  Verify with `git log --oneline -3; git status -sb`.
  Upstream `6987e49` → `6e1e2be` is README/assets only (front-page banner,
  status, DXC line) — no code, no Linux fallout. Previous syncs' notes
  still hold.
  A ralph loop is actively working the tree (glsl/spirv/vulkan_graphics WIP,
  uncommitted) — do not touch its files or stash the workdir.
  NOTE: this machine has **no game files** — `/run/media/...` does not
  exist here. Retail-dependent steps are SKIPPED on this host (see §4.7);
  never block the loop on them.
- **Phase A done.** `cargo check -p xcl --tests` and `cargo check --workspace`
  are green (0 errors) on `x86_64-unknown-linux-gnu`; `cargo test -p xcl --lib`
  passes (490 passed, 0 failed, 1 ignored; re-verified after the `6e1e2be` sync).
  `target/debug/xcl` builds.
- **Phase B done** (where game files exist). Offline tools run on the real
  paths, e.g. `xcl inspect .../game_scarlett_master.exe` reports 587 imports /
  247 stubs; `xcl rpf`, `xcl shaders`, `xcl disasm` work on the game folder and
  `leonida-work/shaders/`. **On a host without the game dirs these steps are
  SKIPPED, not failed** — work synthetic/unit tests instead.
- **Phase C: synthetic title done; CRT-forwarding blocker FIXED.**
  Steps 11a-11s landed (`loader_linux.rs`, `tls_linux.rs`, `pdata.rs`, the `eh`
  sigaction handler, `vfs` + overlay, `kernelx_linux.rs` threads/sync/files,
  xgameruntime identity, XTaskQueue/XUser/storage, GameInput timestamps, user32
  shims, D3DCompile/XG shims, null D3D12 device, headless XAudio2).
  On top of that (commits `dd7a3a7`, `c4187df`, `b96aa6e`, `cf92746`,
  `b45ca53`, `ed0fd32`): bundled CRT DLLs (`vcruntime140`, `msvcp140`,
  `concrt140`, `vcomp140`, `vccorlib140`) forward to the `ucrtbase` HLE
  (`hle/crt_linux.rs` via `resolver_linux.rs`) — `memcpy`/`memmove`/`memset`/
  `memcmp`, `strcmp`/`strncmp`/`wcscmp`/`wcsncmp`/`strchr`/`memchr`,
  scalar/vector `new`/`delete`, `terminate`/purecall/`__CxxFrameHandler3/4` —
  never to logging stubs returning NULL (the old `msvcp140.dll DllMain+0x2efd`
  `SIGSEGV` class). `report.json` tracks `called_stubs`/`imports_by_source`
  on Linux like Windows. **Do not re-do CRT forwarding**; extend it only with
  the next missing family (§4.7b).
- **Empirical frontier (2026-11-24):** the real exe has NOT been re-run —
  no host on this machine has the game folder. Next verification is the
  synthetic title cross-build (`x86_64-pc-windows-gnu` and `-gnullvm` targets
  are installed): `tests/testtitle` + `tests/testgamedll` e2e on Linux.
  Only on a host WITH the game files: `timeout 120 ./target/debug/xcl run
  "<game folder>"`, triage via `xcl.log` `[STUB]` + `report.json` `crash`.
- **Phase D not started.** No Vulkan/SPIR-V deps in `Cargo.toml` (Linux has
  only `libc`); `hle/d3d12x_linux.rs` is a null device (accepts calls, renders
  nothing); no window/swapchain; preflight skips GPU/DXC probes on Linux. The
  SPIR-V backend + `ash` device + present path is the largest remaining chunk.
  **Do not start Phase D until the synthetic title runs to `ExitProcess` on
  Linux** (and, where game files exist, the real exe survives CRT init).
- **Phase E not started.** XAudio2 decodes (XMA2/Opus) but the sink is headless
  (no `cpal`/ALSA); GameInput returns timestamps only (no `evdev`); no
  keyboard/gamepad mapping.
- **Phase F not started.** Only `x86_64-unknown-linux-gnu` is installed — no
  `x86_64-unknown-linux-musl` target, no musl toolchain.

## 0. Ground rules (non-negotiable)

1. Game files stay **in place, read-only**. Never copy them into the repo,
   never commit them:
   - Full title: `/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/`
     (`game_scarlett_master.exe`, `*.dll`, `shaders.rpf`, `*.rpf`,
     `MicrosoftGame.Config`).
   - Working extracts: `/run/media/anon/12TB_B/leonida-work/`
     (`shaders/` with `dxbc/`, `fspx/`, `spirv/`; `exe-disasm/`,
     `*-carve/`, `ttpool*`, `intro_demo/`).
2. Never commit: game data/dumps, console SDK headers/binaries, keys, DRM
   circumvention. Sample-based tests use `XCL_SAMPLE_*` env vars and must
   no-op + pass when unset.
3. License: **GPL-2.0-only**. No WinDurango ports (reference only).
   New deps must be GPL-2.0-compatible **and** static-musl-friendly:
   MIT / BSD / zlib / dual MIT-or-Apache-2.0 (used under the MIT grant)
   are fine; pure-Apache-2.0-only or GPL-3-only are not. When in doubt,
   check the dep's `Cargo.toml` license field and refuse pure-Apache.
   (This matters below: `ash`, `winit`, `cpal`, `evdev` are all usable
   under their MIT grant; `cubeb` (MPL) and pure-Apache crates are out.)
4. Small diffs, one logical change per commit. No drive-by refactors,
   renames, or whole-tree `cargo fmt` (tree is not rustfmt-clean; format
   only touched lines). Reuse the module's existing helpers.
5. Upstream sync (when asked): fetch upstream `main`, merge/rebase as
   instructed — but **never re-add `CLAUDE.md` / `CONTRIBUTING.md`** (stay
   deleted) and **never overwrite this branch's `AGENTS.md`** with the
   Windows version; port only what still applies.
6. Evidence before claims: run what you say you ran, report exact command +
   passed/failed/ignored/skipped. `SKIPPED <test>: <reason>` is not
   coverage. If it wasn't built/tested, say so.

## 1. Survey first (do this before writing code)

1. `ls` both game dirs; confirm `game_scarlett_master.exe`,
   `MicrosoftGame.Config`, `shaders.rpf`, and
   `leonida-work/shaders/{dxbc,fspx,spirv}/`.
2. Inventory the Windows-only surface — this is the porting backlog:
   ```bash
   grep -rn "use windows" crates/xcl/src --include="*.rs" | sort
   grep -rn "windows_sys::" crates/xcl/src --include="*.rs" | cut -d: -f1 | sort | uniq -c | sort -nr
   grep -rn "cfg(windows" crates/xcl/src --include="*.rs"
   grep -rn "retour\|dxcompiler\|d3d12\|XAudio2\|GameInput\|Agility" crates/xcl/src --include="*.rs" -l
   cat crates/xcl/Cargo.toml   # note [dependencies.windows] + windows-sys
   ```
   Expect hits in: `main.rs` (`.CRT$XLY` TLS callback, `extern "system"`),
   `eh.rs` (vectored exception handlers, `Rtl*`, `CONTEXT`), `host.rs`
   (process-memory query), `hle/kernelx*`, `hle/d3d12x/real.rs`,
   `guard.rs`, `present.rs`, `gpu/recomp/dxc.rs` + `runner.rs`,
   `gpu/agility.rs`, `uma.rs`, `hle/xaudio2.rs`, `gameinput.rs`, `kbm.rs`,
   `mfplat.rs`, `dstorage.rs`, `vfs_hooks.rs`, `tls.rs`, `launcher.rs`.
3. Classify each file into: (a) pure logic — compiles as-is on Linux
   (`pe.rs`, `rdna2*.rs`, `xbox.rs`, `pack.rs`, `tiling*.rs`, `rpf*.rs`,
   `shaders.rs`, `disasm.rs`, `inspect.rs`, most of `recomp/emit.rs`);
   (b) OS-shim needed (threads, mmap, hooks, audio, input, Vulkan);
   (c) replace on Linux (DXC→SPIR-V toolchain, D3D12→Vulkan/null,
   XAudio2→cpal/ALSA, GameInput→evdev).
4. Check toolchain: `rustup target list --installed`, add
   `x86_64-unknown-linux-musl` when you need the static link; keep
   `x86_64-unknown-linux-gnu` for iteration speed. `musl-tools` must be
   installed for the musl target. Throughout this doc, "static exe" means:
   all Rust/pure-Rust code statically linked via musl; system GPU/audio
   drivers (`libvulkan.so.1`, `libasound`) are **runtime-`dlopen`ed**, never
   bundled — so there are no `.so` files *shipped next to* the exe, but
   `ldd` on a musl static binary correctly reports "not a dynamic
   executable" regardless.

## 2. Phase A — Linux `cargo check` on pure logic (first win)

Goal: `cargo check -p xcl` passes on `x86_64-unknown-linux-gnu` without
touching behavior.

1. Make `windows` / `windows-sys` Windows-only in
   `crates/xcl/Cargo.toml`:
   ```toml
   [target.'cfg(windows)'.dependencies]
   windows = { ... }
   windows-sys = { ... }
   ```
   Treat `retour` the same way until proven otherwise (it is a
   Windows-style detour library; assume Windows-only, gate it, stub the
   call sites). Gate every `use windows*` / `use retour` module with
   `#[cfg(windows)]` and provide a Linux twin (`*_linux.rs` or a
   `#[cfg(unix)]` stub returning a clean "unsupported on Linux yet"
   `Err`, never `panic!` in library paths).
2. Start with the leaves:
   - `main.rs`: `.CRT$XLY` and `extern "system"` do not exist on ELF.
     Gate the whole TLS-callback block behind `cfg(windows)`; on Unix
     call the init explicitly from `main` (do not invent a C-style
     constructor attribute — there is none in stock Rust).
   - `host.rs`: replace the `K32GetProcessMemoryInfo` block with a
     `/proc/self/status` (or `libc::getrusage`) read behind `cfg(unix)`.
   - `launcher.rs` data-root: `LOCALAPPDATA` → `XDG_DATA_HOME` /
     `~/.local/share/XCL`, keeping the `XCL_DATA_ROOT` override first.
3. Do **not** invent a Linux hooking framework in Phase A; stub hook
   install points with a logged error and move on. The game runs
   in-process in host mode, so Linux shims are linked-in functions, not
   `LD_PRELOAD` tricks — no `LD_PRELOAD` design is needed.
4. Done when: `cargo check -p xcl --target x86_64-unknown-linux-gnu`
   **and** `cargo check --workspace` are both green with zero
   `unimplemented_on_linux` leftovers outside explicitly-marked stubs
   (each stub has a `// LINUX-TODO:` comment naming its phase).

Practical advice: one module per commit (`launcher`, `host`, `eh`
stubs...). If a file is 90% Windows API, don't sprinkle 50 `cfg`s —
split it (`foo.rs` → `foo_common.rs` + `foo_windows.rs` +
`foo_linux.rs` with a tiny `cfg` dispatcher in `foo.rs`).

## 3. Phase B — offline tools work on Linux (big value, no GPU)

Goal: these run natively on Linux against the real game files in place:
`xcl inspect`, `xcl disasm`, `xcl shaders [--recomp]`, `xcl rpf`.

1. They are mostly pure Rust — bring them up first:
   ```bash
   cargo run -p xcl -- inspect "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/game_scarlett_master.exe" | head -n 40
   cargo run -p xcl -- rpf "/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/shaders.rpf" --top 20
   cargo run -p xcl -- shaders "/run/media/anon/12TB_B/leonida-work/shaders" --top 20
   cargo run -p xcl -- disasm "/run/media/anon/12TB_B/leonida-work/shaders/dxbc/<one-small-file>" --hex | head -n 40
   ```
   Pick the small file with `ls -S` (smallest first). If a command takes
   flags (`--json`, `--find NAME`, `--extract NAME --out DIR`,
   `--compile N`), check `xcl <cmd> --help`-equivalent output in
   `main.rs`/`shaders.rs` first — don't guess flags.
2. Work small: one exe, one rpf, 1–5 shaders first. Whole-pack runs
   (`--recomp` on 20k+ programs) come last — they are slow even on
   Windows. Record outputs to `/tmp`, compare shape (counts, stages,
   mnemonics), not giant dumps in chat.
3. Keep `crates/xcl/build.rs` translator-hash list accurate if you move
   emitter files; the disk cache must not silently invalidate. If you add
   or rename a file under `gpu/recomp/` or `gpu/*.rs` that affects
   translation, add it to `TRANSLATOR_SOURCES` in the same commit.
4. Done when: all four commands run on Linux against the real paths and
   print sane results with no Windows DLL loads (verify with
   `strace -e openat -f` if in doubt — zero `*.dll` opens outside the
   game folder itself).

## 4. Phase C — PE loader + host on Linux

This is the hard OS core. Read `loader.rs`, `pe.rs`, `tls.rs`, `eh.rs`,
`resolver.rs`, `vfs*.rs`, `state.rs`, `config.rs` fully before touching.

1. Loader: replace `VirtualAlloc`/`MapView` with
   `mmap(MAP_ANONYMOUS, PROT_READ|PROT_WRITE, MAP_FIXED_NOREPLACE)` at the
   console link address, then `mprotect` per section and `madvise` for
   large regions. `MAP_FIXED_NOREPLACE` fails if the address is occupied —
   treat that as a hard error with the address in the message (don't
   silently fall back to a different base; the game's pointers assume the
   link address). Keep relocations, imports by name/ordinal/delay-load,
   and TLS-callback execution order identical to Windows.
2. Static TLS: the carrier-DLL trick in `tls.rs` is PE/Windows-specific.
   On Linux allocate the static TLS block per thread manually and run the
   game's callbacks on thread spawn by wrapping the host's thread entry
   (all game threads are created by the host's `kernelx` shim, so wrap
   there — no `pthread_create` interposition needed). Test with
   `tests/testgamedll` before the real exe.
3. EH / unwinding: `eh.rs` vectored handlers + `RtlVirtualUnwind` have no
   direct Linux twin, and `__register_frame` registers Linux `.eh_frame`,
   **not** Windows `.pdata` — you must *translate* the game's `.pdata`
   entries into `.eh_frame`/DWARF (or a custom unwinder table the signal
   handler understands), not pass `.pdata` through. Short term: install a
   `sigaction` handler (`sigaction` via `libc`, SA_SIGINFO) that decodes
   the fault address for `report.json`; full C++ unwinding comes later.
   Never claim parity until `tests/testtitle` EH checks pass on Linux.
4. Resolver/HLE routing (`resolver.rs`, `hle/mod.rs`, `stubs.rs`): keep the
   table-driven routing (HLE → game DLL → system → auto-stub). On Linux
   the "system" leg means libc/Vulkan/cpal, not `kernel32`/`xaudio2_9`.
   Log unknown imports to `report.json → called_stubs` exactly as today.
5. VFS (`vfs.rs`, `vfs_hooks.rs`): rebase `GAME:`/`G:`/`D:` onto the real
   game folder read-only + copy-on-write overlay; `S:\`/`T:\` under the
   title data dir. The `KERNELBASE` CRT hooks become linked-in libc
   `open`/`openat` shims on the host's own call path (same in-process
   mechanism, different intercepted functions).
6. Validate incrementally: unit tests → `testtitle`/`testgamedll` e2e →
   `xcl run <game folder>` smoke (only where game files exist; expect early
   stub crashes; triage via `report.json` + `xcl.log`, fix one missing HLE at
   a time).
7. Current frontier — boot the real exe (do this before any Phase D work).
   FIRST check whether the game files exist on this host
   (`ls /run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK/`).
   If they are absent: record `SKIPPED retail-run: no game files on host`
   and work (a)+(b) on the synthetic title only — never stall the loop
   waiting for retail, never emit a no-op verification iteration.
   a. CRT is forwarded already (§Status: `dd7a3a7`+ follow-ups). Extend it
      only with the NEXT missing family, one per commit (e.g. remaining
      `strcpy`/`strcat`/`wcs*` string ops, `errno`/`math`, `qsort`/`bsearch`):
      each new export gets a unit test + a resolver `Hle(ucrtbase)` check.
      If the game folder exists, reproduce with
      `timeout 120 ./target/debug/xcl run "<game folder>"` and read
      `~/.local/share/XCL/<title>/logs/{xcl.log,report.json}`.
      Exit criterion: the exe survives every `DllMain` (synthetic: reaches
      `ExitProcess`; retail, where available: reaches its entry point's
      first HLE calls).
   b. Then the stub-triage loop: run, read the first crash + `[STUB]` lines,
      implement one family (`AcpHal` audio DSP, `WINHTTP`, `MFPlat`, …) in the
      order the exe actually calls them — not the order in `inspect` output —
      repeat. Families nothing calls stay stubs. Without retail, drive this
      loop from the synthetic title's stub list instead of guessing retail
      import order.
   c. Console integrity/tamper checks (if any) are out of scope until the log
      shows them: do not go looking for DRM, just report what the log shows.

## 5. Phase D — GPU: RDNA2 → SPIR-V + Vulkan (not DXIL/D3D12)

Read `gpu/recomp/emit.rs`, `mod.rs`, `dxc.rs`, `runner.rs`, `pso.rs`,
`hle/d3d12x/*` before designing. State: the Linux null device
(`hle/d3d12x_linux.rs`) already gets titles past graphics *init*; Phase D is
about producing *pixels* — the SPIR-V backend, a real `ash` device, and a
window to present to.

1. Keep the emitter backend-agnostic: `emit.rs` stays "RDNA2 semantics in
   an abstract compute language"; push HLSL/DXIL-isms (`WaveActiveBallot`,
   `[WaveSize(64)]`, root signatures) behind a `backend_hlsl` module and
   add `backend_spirv` behind a trait. Bring-up order: (a) emit GLSL 450
   text and shell out to `glslc`/`dxc -spirv` for correctness; (b) only
   then emit SPIR-V directly in-process (via `rspirv` built as a Rust
   library — no external binary at ship). Never ship a path that shells
   out to an external compiler.
2. Wave64 on Vulkan: subgroups are 32 or 64 depending on the GPU and
   driver. Mirror the existing wave64-on-wave32 split
   (`upper_lanes.rs`, `x64_masks.rs`, `mesh_wave.rs`), selecting the path
   from `VkPhysicalDeviceSubgroupProperties::subgroupSize`
   (Vulkan 1.3; `VK_EXT_subgroup_size_control` where available) instead
   of assuming 64. Test both widths even if your card reports only one —
   force the 32-lane path in a unit test.
3. Replace `dxc.rs` (in-process `dxcompiler.dll`) with a trait, e.g.
   `compile(compute_source) -> spirv_words`; implementations: external
   `glslc` / `dxc -spirv` for bring-up only, in-process emission for the
   static exe (so the final link has no DXC dependency at all). Replace
   `runner.rs` D3D12 dispatch with `ash` compute dispatch (used under its
   MIT grant), keeping the `skip_test(reason)` pattern so GPU tests skip
   cleanly without a Vulkan device. `wgpu` is the fallback if raw `ash`
   proves too much boilerplate — decide once, don't carry both.
4. `hle/d3d12x/real.rs` + `guard.rs` (vtable patching, driver internals)
   do not port — route console D3D12.X to a new `hle/vulkan/` backend
   (`null.rs` stays the headless fallback). `present.rs` becomes a Vulkan
   swapchain + `winit` window (MIT grant); frame pacing stays where it is
   today.
5. Prove with the cheapest GPU test first: translate one VS (e.g. the
   known-good `VS_BlitGenAmbCube`-class blit) and run it on Vulkan,
   checking the position-output equivalent before attempting mesh/GS or
   the full 20k-pack `--recomp`.

## 6. Phase E — system services on Linux

- `kernelx` threads → pthreads with Xbox priorities mapped to nice/RT
  where permitted (degrade gracefully without `CAP_SYS_NICE`; never abort
  for lack of RT priority). Title memory budget from
  `MicrosoftGame.Config` via `mmap` arenas, same accounting as Windows.
- `XGameRuntime` offline (users, saves, PLS, packages) is mostly portable
  — keep it, swapping Win32 registry/path helpers for XDG paths
  (`XDG_DATA_HOME` / `~/.local/share/XCL`, `XCL_DATA_ROOT` override).
- Audio: `xaudio2.rs` → keep the decode (Opus/XMA are already pure-Rust:
  `opus.rs`, `xma2/`) and sink to ALSA/Pulse via `cpal` (MIT), opened
  with the default device. Keep the vtable-patching *shape* but target
  the game's audio callbacks, not system `xaudio2_9.dll`. Do not pull in
  `cubeb` (MPL) or any dynamic-only audio framework.
- Input: `gameinput.rs` + `kbm.rs` → `evdev` (MIT) gamepad +
  keyboard/mouse-as-gamepad mapping; keep the existing mapping table
  byte-for-byte so bindings don't shift between OSes.
- Media: Bink2 decode stays CPU (already portable); display shaders go
  through the SPIR-V path like any other.

## 7. Phase F — the static, fastest exe (read all of this before tuning)

There are three profiles, not one. Keep them straight or you will burn
days:

| Profile | Purpose | Settings |
|---|---|---|
| `dev` | iteration speed | defaults; `opt-level=0`, no LTO |
| `release-bringup` | debugging crashes on Linux | current `[profile.release]` with `debug=2`, LTO **off**, `codegen-units` default |
| `release-final` | the shipped static exe | `opt-level=3`, `lto="fat"`, `codegen-units=1`, `strip="symbols"`, `debug=0`, `overflow-checks=false` |

Concretely, the shipped profile is:

```toml
[profile.release]
opt-level = 3
lto = "fat"            # "thin" while iterating; "fat" only for the final binary
codegen-units = 1      # final only — kills parallel codegen, so never during iteration
strip = "symbols"      # or `strip = true` on older cargo; then no separate `strip` step
debug = 0
overflow-checks = false
panic = "unwind"       # KEEP — `eh.rs`/C++ unwinding needs it; never `abort` without an audit
```

Rules that save you from self-inflicted wounds:

1. **One lever at a time, measured.** Baseline first:
   ```bash
   cargo build --release --target x86_64-unknown-linux-musl
   ls -lh target/x86_64-unknown-linux-musl/release/xcl
   file target/x86_64-unknown-linux-musl/release/xcl   # expect: ELF, statically linked
   ldd  target/x86_64-unknown-linux-musl/release/xcl   # expect: "not a dynamic executable"
   ```
   Then flip exactly one setting, rebuild, record `ls -lh` bytes,
   `nm --size-sort target/.../xcl | tail -n 20` (biggest symbols), and a
   timing run (`time xcl shaders <small-dir> --recomp`, plus one
   single-shader Vulkan run once Phase D exists). `opt-level="z"`/`"s"`
   shrink the binary but *slow down* hot loops (recompiler emit, tiling
   decode, command-list replay) — only take them if your benchmark on
   the shader corpus proves the slowdown is nil, otherwise stay on `3`.
2. **Correctness symbols vs. ship size.** During bring-up keep `debug=2`
   so release crashes resolve to file:line. At packaging, `strip`
   (via Cargo `strip`, not a hand-rolled `strip` invocation that you
   then forget) is what shrinks the exe — never delete the bring-up
   profile, just don't ship it. `debug="line-tables-only"` is the middle
   ground if full `debug=2` makes an intermediate binary unwieldy.
3. **`target-cpu`: baseline unless proven.** The default (`x86-64`
   baseline) runs everywhere. `-C target-cpu=x86-64-v3` (AVX2) can speed
   tiling/memcpy loops but refuses to start on older CPUs — only set it
   (via `RUSTFLAGS`, never hardcoded for all users) with a measured
   corpus speedup *and* a documented minimum-CPU note. No
   `-C target-cpu=native` in anything committed — it makes the binary
   unreproducible across machines.
4. **No magic bullets.** Do NOT use UPX/packers (slower startup, false
   antivirus positives, breaks crash symbolization). Do NOT swap the
   global allocator (`mimalloc` etc.) without a `perf`-proven hot-path
   win — musl's allocator is fine until measurements say otherwise, and
   every allocator swap is a new license + static-link audit. Do NOT set
   `panic="abort"`, `overflow-checks`, or `debug-assertions` differently
   from the table without calling it out — unwinding and report paths
   depend on them.
5. **Size comes from code, not flags.** The big levers are structural:
   audit with `cargo tree -e no-dev` and drop crates you don't call;
   one Vulkan backend (`ash`, MIT grant) — never carry `ash` + `wgpu`;
   one audio sink (`cpal`, MIT) — never two; gate every dump path
   (`XCL_DUMP_SHADERS`, frame dumps, verbose `report.json` fields) behind
   env/`cfg(debug_assertions)` so release carries none of it; keep
   backend-specific code (`backend_hlsl`, DXC/D3D12, Agility) behind
   `cfg(windows)` so the musl link never sees it.
6. **Runtime speed comes from the hot paths.** Profile before touching:
   `perf stat`, `perf record --call-graph=dwarf`, then optimize in this
   order — (a) recompiler emit (no per-instruction allocation, reuse
   `Vec` buffers via `clear()`, operate on slices); (b) GFX10 tiling
   decode (lookup tables are already generated — don't regenerate at
   runtime); (c) VFS reads (bigger readahead, fewer syscalls, reuse
   overlay buffers); (d) command-list replay (batch Vulkan pipeline +
   descriptor creation, cache PSOs keyed on the existing translator-hash
   so rebuilds never retranslate). Parallelize with the existing chunk
   pattern at the *record/pack* level (like today's RPF9 scan), never by
   spawning a thread per shader.
7. Done when ALL hold: `file` says statically linked, `ldd` says not a
   dynamic executable, no `.so` ships beside it (system Vulkan/ALSA are
   `dlopen`ed at runtime), the Phase B commands run from the musl binary,
   and your report shows binary bytes + corpus time before/after the
   final profile flip.

## 8. How to work each session

1. State branch + commit: `git branch --show-current` (must be `linux`),
   `git status -sb`, `git log --oneline -3`.
2. Check game-file presence FIRST (`ls` both dirs from §0.1). If absent,
   every retail-dependent step is `SKIPPED`, and the synthetic title
   (`tests/testtitle`, `tests/testgamedll`, cross-built with
   `x86_64-pc-windows-gnu`/`-gnullvm`) is the e2e target. Never let a
   missing game folder stall the loop.
3. Pick the smallest next step from the phase you're in; implement it;
   run the narrowest test first (`cargo test -p xcl --lib <filter>`),
   then the widening ring (`--workspace`, e2e title, one real game file
   IF present).
4. If a step needs minutes (full-pack recompile, many SPIR-V compiles,
   whole test suite), sample first (N=1, N=5, N=50) and say so.
5. Report: what changed, exact commands + counts, what was skipped and why,
   what remains. No hand-written smoke blocks or test counts — paste tool
   output.
6. When a phase (or the frontier) changes, update the Status snapshot at the
   top of this file in the same commit — one stale paragraph here wastes the
   next session. A snapshot that still describes already-landed work as
   "the frontier" is a P0 bug: fix the snapshot before writing more code.
7. Anti-stall (learned 2026-11-24 — the loop went `loop_stale` after
   7 iterations re-verifying the fixed CRT blocker because the snapshot was
   stale AND the game files were absent on that host):
   every iteration must land new behaviour (code, test, or doc commit) or
   terminate the loop with a handoff. Pure re-verification iterations
   (`cargo test` / clippy with no code change) are forbidden twice in a row;
   if two consecutive iterations produce no diff, stop and report instead
   of emitting a third identical summary.
