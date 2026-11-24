# Plan — Linux static recompilation of XCL

Task keys are `code-assist:linux-port:step-NN:<slug>`. Later steps are provisional: their
outcome and acceptance are fixed, implementation detail may change after earlier slices land.
Every step must leave `cargo check --workspace --target x86_64-unknown-linux-gnu` green and
Windows behavior unchanged.

## Working decisions

- **D1 — Phase A gating.** Windows-only modules are `#[cfg(windows)]` in `lib.rs` and get a
  Linux stub module whose public functions return a clean `Err`/exit code and carry
  `// LINUX-TODO: Phase C|D|E`. Do not sprinkle `cfg` through 80 files. Split a file
  (`foo.rs` → `foo_windows.rs` + thin dispatcher) only when a later phase needs the module on
  Linux. This keeps the first diff reviewable and preserves the Windows build byte-for-byte.
- **D2 — HLE name tables without duplicating names.** `hle_exports!` gets a
  `cfg(windows)` form that keeps `$name as usize` and a `cfg(not(windows))` form that keeps
  `stringify!($name)` but stores address `0`. The export names therefore survive on Linux
  with no second hand-maintained list; resolver treats address `0` as an auto-stub, so
  `xcl inspect` reports unimplemented console functions as `STUB`, never as working HLE.
  Move each Windows implementation body to `hle/<module>/windows.rs` (or `_windows.rs`) only
  when that module's real implementation is ported. Kernelx/registry come back first.
- **D3 — inspect before the HLE port.** For Phase B, `inspect` uses the route table
  (module names + patterns) decoupled from the implementation tables; no fake registry and no
  duplicate path list. Full HLE routing lands in Phase C.
- **D4 — per-phase verification.** Each step ends with the narrowest test (`cargo test -p xcl
  --lib <filter>`), then `cargo check --workspace` and, where a command exists, one real-file
  run. GPU tests print `SKIPPED <test>: <reason>` without a device; report skips, don't count
  them.

## Phase A — Linux build skeleton

1. **Linux build skeleton** — `code-assist:linux-port:step-01:build-skeleton`.
   Outcome: `xcl --version` / `--help` run on `x86_64-unknown-linux-gnu`; `xcl run` exits 2
   with `xcl run: not supported on Linux yet (Phase C)`; `cargo check --workspace` green.
   Scope: target-gate `windows`/`windows-sys`/`retour`; `#[cfg(windows)]` the Windows modules
   with Linux stubs; gate the `.CRT$XLY` callback, volatile read and Explorer/COM/comctl menu;
   keep `disasm`, `shaders`, `rpf` dispatch wired; make `testtitle` (bin) and `testgamedll`
   check on Linux without breaking the Windows PE builds; add the Linux `// LINUX-TODO:`
   census. Gate `gpu` runtime modules; keep RDNA2/emit modules.
   Acceptance: exact commands in the task description; zero `unimplemented_on_linux` outside
   marked stubs.

2. **Phase-A platform leaves** — `code-assist:linux-port:step-02:platform-leaves`.
   Outcome: data root, process-memory, thread-id, ANSI-string and atomic-replace helpers have
   Linux implementations with unit tests: `XCL_DATA_ROOT` > `XDG_DATA_HOME` >
   `~/.local/share/XCL`; `/proc/self/status` parse; thread id; UTF-8 fallback; `rename(2)`.
   Windows paths keep their current behavior.

## Phase B — offline tools on Linux

3. **disasm + rpf** — `code-assist:linux-port:step-03:offline-disasm-rpf`.
   Outcome: `xcl disasm` on the smallest `leonida-work/shaders/dxbc` file and `xcl rpf` on the
   retail `shaders.rpf` (probe/`--top`) print sane results on Linux with no DLL opens.
   Acceptance: pasted commands and output shape; `strace -e openat` shows no `*.dll`.

4. **shaders** — `code-assist:linux-port:step-04:offline-shaders`.
   Outcome: `xcl shaders` stats and `--recomp` over a small corpus + one sampled retail RPF
   run; `--compile` reports "DXC is Windows-only; Phase D" and exits cleanly. Translator hash
   list untouched unless emitter files move (then update `TRANSLATOR_SOURCES`).
   Acceptance: stage/instruction counts agree with the Windows-era expected shape; no panic.

5. **inspect** — `code-assist:linux-port:step-05:offline-inspect`.
   Outcome: `xcl inspect game_scarlett_master.exe` reports PE header fields, import counts and
   per-DLL HLE/game-DLL/STUB classification on Linux without `LoadLibrary`. Console DLLs map
   through the shared route table; functions are `STUB` until Phase C implements them.
   Acceptance: import totals match the PE headers; no claim of implemented HLE; JSON mode valid.

## Phase C — loader + host on Linux

6. **loader mmap** — `code-assist:linux-port:step-06:loader-mmap`.
   Outcome: `mmap(MAP_FIXED_NOREPLACE)` at `ImageBase`, per-section `mprotect`, relocations,
   imports by name/ordinal/delay, TLS directory and entry RVA preserved. Occupied address is a
   hard error naming the address; no silent base fallback.
   Acceptance: unit tests map a synthetic PE, reject a non-PE and an occupied base; retail exe
   maps successfully in a headless harness.

7. **TLS per thread** — `code-assist:linux-port:step-07:tls-threads`.
   Outcome: static TLS block allocated per host-created thread, game TLS callbacks run on
   thread start/exit in the kernelx wrapper, TLS index/pointer semantics match Windows.
   Acceptance: synthetic TLS unit test on two threads; `tests/testgamedll` TLS bump after
   cross-build.

8. **VFS** — `code-assist:linux-port:step-08:vfs-linux`.
   Outcome: `GAME:`/`G:`/`D:` rebased read-only onto the game folder, `S:`/`T:` under the title
   data dir, copy-on-write overlay for writes; Windows path parsing unchanged.
   Acceptance: existing VFS unit tests pass on Linux; a write lands in the overlay while the
   game folder hash is unchanged; traversal such as `G:\..\outside` cannot escape the mount
   or address an arbitrary host file, and an unmapped drive is rejected cleanly.

9. **EH signal handler** — `code-assist:linux-port:step-09:eh-signal`.
   Outcome: a portable report core builds on Linux, and a `sigaction` handler (SA_SIGINFO)
   for SIGSEGV/SIGILL/SIGFPE/SIGBUS records the faulting PC, address, signal and mapped
   module into `report.json`; `install_pc_hook` is a logged no-op. No claim of C++ unwinding.
   Acceptance: a deliberate fault in a child process produces a report with the right PC/address;
   unsupported paths return `Err` with `// LINUX-TODO: Phase C`.

9b. **import-free synthetic PE execution** —
   `code-assist:linux-port:step-09b:synthetic-pe-entry`.
   Outcome: the Linux loader can execute an import-free synthetic PE:
   `load_main` → attach current thread → process TLS callbacks → entry point → process/thread
   detach. This is the first runnable image-code slice and does not pretend imports/HLE exist.
   Acceptance: a synthetic entry returns a known value (for example `0x2a`) and a TLS callback
   observes its per-thread template through `gs:[0x58]`; loader/TLS tests and the workspace
   check pass with exact counts.

10a. **HLE registry + resolver** — `code-assist:linux-port:step-10:hle-resolver`.
    Outcome: `hle::registry()` builds on Linux from the preserved export-name tables; address
    `0` is metadata for `Stub`, while the runtime resolver writes safe executable auto-stub
    thunks rather than null pointers. Routes (kernel family, xgameruntime, d3d12_xs, audio,
    media…) stay identical to Windows; no `LoadLibrary`/`GetProcAddress` on Linux.
    Acceptance: registry/resolver/inspect tests pass; calling an unresolved import reaches a
    logged stub without a null call or panic; implementations remain stubs in this slice.

10b. **kernelx core** — `code-assist:linux-port:step-10b:kernelx-core`.
    Outcome: kernelx memory, thread creation, synchronization, and monotonic time work on
    Linux; threads enter through `tls::spawn`; unavailable realtime priority degrades
    gracefully instead of aborting.
    Acceptance: focused kernelx/TLS tests plus the full Linux library suite and workspace
    check pass, with exact counts.

10c. **kernelx file APIs through VFS** — `code-assist:linux-port:step-10c:kernelx-files`.
    Outcome: the title's file operations reach `vfs_hooks_linux` on the host call path, so
    reads see the read-only game layer and writes land in the overlay without `LD_PRELOAD`;
    required ordinary kernel-family fallbacks are libc-backed rather than absent imports.
    Acceptance: synthetic file tests cover read, overlay write, delete/whiteout, and an
    adversarial unmapped or traversal path; the source game tree hash is unchanged.

10d. **DLL load + host entry/TLS lifecycle** —
    `code-assist:linux-port:step-10d:dll-host-entry`.
    Outcome: serialized dependency/DLL loading runs DllMain and process TLS attach/detach;
    `run_main` attaches the calling thread before entry; late DLL installs preserve Windows
    attach/detach semantics and do not move a live TLS slot array under a reader. The strict
    fixed-address policy remains: an occupied base is a hard error, not a silent relocation.
    Acceptance: synthetic PE tests prove process attach/detach, main-thread TLS visibility,
    DLL callback order, occupied-base failure, late-install detach filtering, and stable
    preallocated slots; loader/TLS tests and the workspace check pass. Retail execution is
    not claimed here.

10e. **Linux launcher/host `xcl run` path** —
    `code-assist:linux-port:step-10e:linux-launcher-host`.
    Outcome: `xcl run <game folder>` performs Linux preflight/config/manifest/log/report
    setup, initializes the VFS, and enters the loader's `run_main` path; missing inputs fail
    cleanly and no Windows DLL is loaded.
    Acceptance: a synthetic import-free game folder runs through the real CLI and records its
    known result; a missing/non-PE input returns a clean error; focused launcher/host tests,
    the full Linux library suite, and workspace check pass.

11. **cross-built test PE e2e** — `code-assist:linux-port:step-11:test-pe-e2e`.
    Outcome: `tests/testtitle` + `testgamedll` are built as Windows PE (choose and document one
    target: `x86_64-pc-windows-gnullvm` preferred, `x86_64-pc-windows-gnu` fallback) and the
    e2e passes through Linux `xcl run`: all title checks, game folder byte-identical, writes in
    the overlay, stub in `report.json`. If cross-build is impossible on this host, the task
    must say so with evidence and instead run the synthetic PE tests; it may not fake a pass.
    Acceptance: `cargo test --workspace --no-run` compiles the integration-test surface; exact
    build/e2e commands and passed/failed/ignored counts are recorded.

12. **retail title smoke** — `code-assist:linux-port:step-12:retail-smoke`.
    Outcome: `xcl run` on the unpacked retail title reaches a stable, reproducible first
    failure (missing HLE/unsupported import) with a readable `report.json`/`xcl.log`; fix the
    first one or two missing kernelx/loader items found.
    Acceptance: sampled run (`XCL_RUN_SECONDS`, if supported) with the failure named; no hang
    past the timeout.

## Phase D — RDNA2 → SPIR-V + Vulkan

13. **backend split + GLSL** — `code-assist:linux-port:step-13:glsl-backend`.
    Outcome: emitter exposes a backend trait; a GLSL 450 backend emits one known-good blit VS
    (e.g. `VS_BlitGenAmbCube` class from the retail pack via `XCL_SAMPLE_RPF`) with the same
    register/export semantics as the HLSL path. External `glslc` is bring-up only.
    Acceptance: translated output passes `glslc -fshader-stage=vert` and the position-output
    expectation test.

14. **in-process SPIR-V** — `code-assist:linux-port:step-14:spirv-inproc`.
    Outcome: `rspirv`-based backend produces valid SPIR-V words for the same VS with no
    external compiler; `compile(compute_source) -> spirv_words` trait replaces `dxc` on Linux.
    Acceptance: SPIR-V module validates (`spirv-val` if present, else structural unit tests)
    and matches the GLSL-backed image; no process spawn in the shipped path.

15. **Vulkan compute runner** — `code-assist:linux-port:step-15:vulkan-runner`.
    Outcome: `ash` compute dispatch replaces D3D12 `runner.rs`; kernel-mode translations run on
    the real device with the existing `SKIPPED <test>: <reason>` pattern when no Vulkan device.
    Acceptance: one instruction-semantics kernel and the blit VS produce expected output on a
    device; the no-device path skips cleanly with the reason printed.

16. **subgroup width** — `code-assist:linux-port:step-16:subgroup-width`.
    Outcome: path selection from `VkPhysicalDeviceSubgroupProperties::subgroupSize`
    (`VK_EXT_subgroup_size_control` where available), mirroring `upper_lanes`/`x64_masks`/
    `mesh_wave`; forced 32-lane test even on a 64-lane device.
    Acceptance: deterministic unit test forces both widths; no assumption of 64.

17. **D3D12.X → Vulkan + present** — `code-assist:linux-port:step-17:vulkan-present`.
    Outcome: console D3D12.X calls route to a `hle/vulkan` backend (`null` stays headless
    fallback); `present.rs` uses a Vulkan swapchain + `winit` window; frame pacing unchanged.
    Acceptance: windowed smoke shows a frame; headless (`null`) path still runs in CI.

## Phase E — system services

18. **audio** — `code-assist:linux-port:step-18:audio-cpal`.
    Outcome: existing Opus/XMA decode feeds a `cpal` default-device sink; XAudio2 vtable shape
    preserved, targets the game's callbacks (not `xaudio2_9.dll`). No `cubeb`.
    Acceptance: test title's audio checks pass without a Windows DLL; device-less host returns
    a clean error.

19. **input** — `code-assist:linux-port:step-19:input-evdev`.
    Outcome: `evdev` gamepad + keyboard/mouse-as-gamepad; mapping table byte-for-byte unchanged
    from the Windows table.
    Acceptance: mapping unit tests unchanged (same bytes); a fake evdev event maps to the same
    pad state as the Windows path.

20. **XGameRuntime XDG** — `code-assist:linux-port:step-20:xgameruntime-xdg`.
    Outcome: users/saves/PLS/package offline paths use `XDG_DATA_HOME` /
    `~/.local/share/XCL`, `XCL_DATA_ROOT` first; no Win32 registry/path helpers on Linux.
    Acceptance: save round-trip in a temp XDG root; registry-dependent tests gated.

## Phase F — static, fast exe

21. **static musl release-final** — `code-assist:linux-port:step-21:musl-static`.
    Outcome: `x86_64-unknown-linux-musl` target installed; `release-final` profile
    (`opt-level=3`, `lto="fat"`, `codegen-units=1`, `strip="symbols"`, `debug=0`,
    `overflow-checks=false`, `panic="unwind"`); `file` says statically linked, `ldd` says not
    a dynamic executable, no `.so` beside it, Phase-B commands run from the musl binary.
    Acceptance: pasted `file`/`ldd`/`ls -lh` and corpus timing before/after; Vulkan/ALSA
    `dlopen`ed at runtime.

## Plan Changes

- Initial plan created on `build.start`.
- Planner refresh (2026-11-24, `linux` @ `f63470d`): reused the existing stable-key queue
  rather than duplicating it. Steps 1–7 remain closed; step 8 is the first ready task. The
  clean-egret worktree already contains an uncommitted Linux VFS implementation in
  `crates/xcl/src/vfs.rs`, `crates/xcl/src/vfs_hooks_linux.rs`, and `lib.rs`; step 8 must
  review, test, and finish that work instead of replacing it blindly.
- Split the former step 10 into 10a–10e. Registry/resolver, kernelx core, kernelx file APIs,
  DLL/host-entry/TLS lifecycle, and the Linux launcher/host CLI path are separately
  reviewable behaviors; the original bundle was too large for one build-and-review pass.
- Added step 9b after the read-only code survey: mapping alone is not execution, so an
  import-free synthetic PE must run through TLS attach/callbacks/entry/detach before HLE.
  Runtime unresolved imports use executable stub thunks; metadata address `0` is only a
  classification marker. Step 11 now depends on 10e. Later steps remain provisional.
- Planner refresh (2026-11-24, `linux` @ `f3bee90`): steps 1–11 are closed (11a–11s slices +
  CRT forwarding `dd7a3a7` and follow-ups `c4187df`/`b96aa6e`/`cf92746`, report parity
  `b45ca53`, clippy fix `ed0fd32`); `cargo test -p xcl --lib` is 369 passed / 0 failed.
  Queue holds 11 open tasks. Step 12 (retail smoke) is SKIPPED on hosts without
  `/run/media/...` game files — never stall on it. The next behavior slice is **step 12a**
  (`linux:crt-copy`, the only unblocked task): the next §4.7a CRT family
  (`strcpy`/`strncpy`/`strcat`/`wcs*`/`wmemcpy`/`wmemmove` copy/concat), one commit, verified
  synthetically (unit test + resolver `Hle(ucrtbase)` checks + title e2e as regression gate).
  Further CRT families follow one-per-commit after triage; Phase D stays blocked until the
  synthetic title runs to `ExitProcess` on Linux. Installed targets now include
  `x86_64-pc-windows-gnu` and `-gnullvm`, so the Builder can cross-build the title e2e here.

## Phase C frontier — next CRT family

12a. **CRT copy/concat** — `linux:crt-copy`.
    Outcome: NUL-safe win64 `strcpy`/`strncpy`/`strcat`/`wcs*`/`wmemcpy`/`wmemmove` exports in
    `hle/crt_linux.rs`, wired through exports + the HLE spot list, with resolver `Hle(ucrtbase)`
    checks via bundled-DLL routes and string contracts, plus a `crt` unit test.
    Acceptance: focused `crt` tests, full `cargo test -p xcl --lib` counts, workspace check,
    and the cross-built synthetic title still reports `done, failures: 0` (no retail needed).
