# Decision Journal

Template for each entry:

- **Decision:** what was decided
- **Chosen Option:** the option taken
- **Confidence:** 0-100
- **Alternatives Considered:** what else was on the table
- **Reasoning:** why
- **Reversibility:** how hard to undo
- **Timestamp:** UTC ISO 8601

---

## DEC-001 — Phase-A gating strategy

- **Decision:** How to make the crate compile on Linux without porting behavior yet.
- **Chosen Option:** `#[cfg(windows)]` whole Windows-only modules in `lib.rs` plus small Linux
  stub modules that return clean errors with `// LINUX-TODO: Phase X`. Split a file into
  `_windows`/`_linux` only when a phase actually needs it on Linux.
- **Confidence:** 88
- **Alternatives Considered:** (a) per-function `cfg` across 80 files; (b) a second `xcl_linux`
  crate; (c) feature flags per platform.
- **Reasoning:** (a) is a huge, unreviewable diff and keeps the crate tangled with Win32; (b)
  duplicates the CLI and build plumbing; (c) features are additive and easy to get wrong.
  Whole-module gating is the smallest reversible step and preserves the Windows build.
- **Reversibility:** high — moving a module back under Linux is a localized change.
- **Timestamp:** 2026-11-24T10:40:00Z

## DEC-002 — Linux HLE export-name preservation

- **Decision:** How Linux gets HLE routing and export names without a second hand-maintained list.
- **Chosen Option:** make `hle_exports!` cfg-dependent: Windows keeps `$name as usize`; Linux
  keeps `stringify!($name)` with address `0`. Resolver treats `0` as an auto-stub, so inspect
  reports STUB until a function is really ported. Move Windows implementation bodies to
  `_windows.rs` files as modules are ported, kernelx first.
- **Confidence:** 85
- **Alternatives Considered:** (a) duplicate name lists in a Linux table; (b) rewrite each HLE
  module's functions with Linux bodies immediately; (c) generate the table from the Windows
  source at build time.
- **Reasoning:** (a) drifts and doubles maintenance; (b) is weeks of unverified work before
  anything runs; (c) adds build-script fragility. The macro split keeps one source of names and
  makes stub-vs-implemented explicit in the resolver.
- **Reversibility:** high — the macro and resolver are localized; a real implementation simply
  replaces `0` with a function address.
- **Timestamp:** 2026-11-24T10:41:00Z

## DEC-003 — Phase-B inspect classification

- **Decision:** How `xcl inspect` runs before the HLE modules are ported.
- **Chosen Option:** extract the route table (module names + DLL patterns) from
  `hle::build` so inspect can use it without calling the implementation export tables. It
  classifies console imports as HLE-module names and unimplemented functions as STUB. No fake
  registry, no `LoadLibrary`.
- **Confidence:** 90
- **Alternatives Considered:** (a) gate inspect until Phase C; (b) a Linux-only hard-coded
  console-DLL list; (c) build the full registry with stubbed implementations early.
- **Reasoning:** (a) delays a high-value offline tool; (b) creates a second source of truth;
  (c) pulls Phase-C work into Phase B. Extracting routes is a small refactor that the HLE port
  reuses.
- **Reversibility:** high.
- **Timestamp:** 2026-11-24T10:42:00Z

## DEC-004 — Test PE cross-build target

- **Decision:** Which Windows target to use for `tests/testtitle` + `tests/testgamedll` on this
  Linux host (deferred to step 11, recorded here as the preference).
- **Chosen Option:** prefer `x86_64-pc-windows-gnullvm` (rust-lld, no MSVC tools); fall back to
  `x86_64-pc-windows-gnu` if a mingw toolchain is available. Evidence required either way; a
  synthetic PE test is the fallback only if neither target can link.
- **Confidence:** 55
- **Alternatives Considered:** use the retail exe as the only Phase-C target; commit a
  prebuilt synthetic PE.
- **Reasoning:** the test title is the intended e2e; the exact target depends on installed
  linkers/libs, unknown until step 11. Committing a prebuilt PE is acceptable only as
  synthetic test data, never a game artifact.
- **Reversibility:** high.
- **Timestamp:** 2026-11-24T10:43:00Z

## DEC-005 — Linux loader placement, libc, and deferred loader legs

- **Decision:** Where the Linux PE mapper lives and how it compiles before the TLS/HLE/EH
  steps exist.
- **Chosen Option:** a new `crates/xcl/src/loader_linux.rs` exposed as `crate::loader` on
  non-Windows via `#[path]`; `loader.rs` stays the untouched Windows implementation. Add
  `libc` only for `cfg(target_os = "linux")`. Map with `mmap(MAP_FIXED_NOREPLACE)` at
  `ImageBase`, keep the relocation path but make an occupied base a hard error, parse
  imports/TLS/delay tables, and leave IAT resolution (step 10), TLS install (step 7) and
  `.pdata` (step 9) as explicit `LINUX-TODO`s.
- **Confidence:** 85
- **Alternatives Considered:** (a) split `loader.rs` into `loader_windows.rs` + dispatcher;
  (b) compile `loader.rs` on Linux with scattered `cfg(windows)`; (c) port resolver/TLS/EH
  early so imports resolve during step 6.
- **Reasoning:** (a) moves 525 unchanged Windows lines for no behavior; (b) mixes two ABIs
  and Windows-only calls into the mapping code; (c) pulls three later steps into this one and
  would fake HLE addresses. The `#[path]` swap keeps one module name for later host code while
  preserving the Windows build, and the deferred legs are named instead of silently stubbed.
  `libc` is MIT OR Apache-2.0 (MIT grant), musl-friendly, and needed by the upcoming
  `sigaction`/pthread work anyway.
- **Reversibility:** high — renaming the file and swapping the `#[path]` line is local; the
  deferred legs are additive.
- **Timestamp:** 2026-11-24T11:20:00Z

## DEC-006 — Linux static TLS via a fake GS TEB

- **Decision:** How PE `__declspec(thread)` accesses work for manually mapped images on
  Linux, and when the loader installs them.
- **Chosen Option:** `crates/xcl/src/tls_linux.rs` (exposed as `crate::tls` on non-Windows)
  sets `GS` per attached thread via `arch_prctl(ARCH_SET_GS)` to a fake TEB with
  `ThreadLocalStoragePointer` at `0x58` and a slot array. Blocks are 16-aligned copies of
  the relocated template; the main image takes slot 0, DLLs allocate from 1, and the image's
  `_tls_index` is written by the installer. Threads enter through `tls::spawn` (kernelx
  wrapper), which attaches, runs `f`, and detaches (also on panic). `load_main` installs the
  main image only; DLL TLS install is deferred to the step-10 game-DLL load path.
- **Confidence:** 90
- **Alternatives Considered:** (a) `LD_PRELOAD`/pthread interposition; (b) translate every
  PE TLS access to a host TLS variable; (c) install every mapped image at `map_image`.
- **Reasoning:** PE code hard-codes `gs:[0x58]`, so only a GS-based TEB keeps the index and
  pointer semantics identical; Linux does not use GS in userspace, and xcl's own TLS is in
  FS, which frees slot 0 for the main image (no Windows-style exchange). The task forbids
  pthread interposition, and kernelx owns all game threads. Installing at `map_image` would
  run retail TLS callbacks from unrelated test threads before HLE exists; the DLL load path
  is the point where blocks and callbacks belong.
- **Reversibility:** high — the module boundary is `install`/`attach`/`detach`/`spawn`; the
  loader can install DLLs at step 10 without changing the mechanism.
- **Timestamp:** 2026-11-24T12:10:00Z

## DEC-007 — Split Phase-C HLE/host bring-up into reviewable slices

- **Decision:** Whether to keep registry/resolver, kernelx, file APIs, and DLL/host entry in
  one Phase-C task.
- **Chosen Option:** split the former step 10 into 10a registry/resolver, 10b kernelx core,
  10c kernelx file APIs through VFS, and 10d DLL loading plus host-entry/TLS lifecycle.
- **Confidence:** 90
- **Alternatives Considered:** keep one large kernelx/host task; defer TLS lifecycle fixes
  until retail smoke triage.
- **Reasoning:** each slice now has an independently testable user-facing outcome and a
  reviewable diff. The split also makes the known TLS obligations explicit: attach the main
  thread before entry, filter detach callbacks by images present at thread attach, preallocate
  the slot array, and serialize late DLL loads.
- **Reversibility:** high — tasks can be re-chained or recombined after earlier slices land.
- **Timestamp:** 2026-11-24T11:36:00Z

## DEC-008 — Execute an import-free PE before HLE bring-up

- **Decision:** Whether Phase C should implement HLE before proving that mapped PE code runs.
- **Chosen Option:** add step 9b immediately after EH/report: `load_main`, attach the current
  thread, run process TLS callbacks, call the entry point, then detach.
- **Confidence:** 92
- **Alternatives Considered:** keep the original registry/kernelx-first order; use only the
  retail EXE as the first execution target.
- **Reasoning:** a tiny import-free synthetic PE isolates loader/TLS/entry execution from HLE,
  graphics, and retail noise, and gives the Builder a real runnable slice to review.
- **Reversibility:** high — the harness remains useful as a regression test after HLE lands.
- **Timestamp:** 2026-11-24T11:45:00Z

## DEC-009 — Linux stub metadata versus runtime import thunks

- **Decision:** How unresolved Linux imports should be represented and invoked.
- **Chosen Option:** keep address `0` as inspect/registry metadata meaning `Stub`, but have the
  runtime resolver allocate safe executable auto-stub thunks and write those addresses into
  the IAT. A thunk logs the called stub and returns without a null call or panic.
- **Confidence:** 90
- **Alternatives Considered:** write zero into the IAT; duplicate a Linux export-name table;
  implement every kernel-family fallback before any image can start.
- **Reasoning:** zero is useful classification data but is not callable. Thunks preserve
  forward progress and `called_stubs` reporting while kernelx/file fallbacks land in later
  slices.
- **Reversibility:** high — a real implementation replaces the thunk address in the registry.
- **Timestamp:** 2026-11-24T11:46:00Z

## DEC-010 — Keep strict fixed-address mapping during initial DLL bring-up

- **Decision:** Whether Linux game DLLs should relocate when their preferred base is occupied.
- **Chosen Option:** retain the prompt's strict `MAP_FIXED_NOREPLACE` policy for this phase;
  an occupied main or DLL base is a hard error naming the address. Revisit only if a real
  title demonstrates a collision that blocks progress.
- **Confidence:** 70
- **Alternatives Considered:** allocate game DLLs at a fallback base and apply PE relocations,
  matching the Windows loader's usual behavior.
- **Reasoning:** the current objective explicitly bans silent base fallback, and the test
  title can prove DLL loading without speculative relocation policy. A future revision can
  narrow the rule to DLLs with evidence.
- **Reversibility:** medium — relocation support already exists in the PE path, but choosing a
  base and updating pointers is additional loader policy.
- **Timestamp:** 2026-11-24T11:47:00Z

## DEC-011 — Linux VFS rejection and containment policy

- **Decision:** How Linux console paths that escape a mount are represented and checked.
- **Chosen Option:** add a Linux-only `Target::Reject(u32)`; normalize mount tails, require
  lexical containment, then compare the nearest existing canonical ancestors of the mount
  root and target. Apply the same checks to game/overlay relative paths, forbid a symlinked
  overlay root, and keep title-data mounts inside the canonical data root. Reject dangling
  final symlinks and return `ERROR_PATH_NOT_FOUND` for untranslated console drives. Keep the
  Windows `Target` variants and mount parser semantics unchanged.
- **Confidence:** 88
- **Alternatives Considered:** (a) return `Target::Pass` and rely on the host open to fail;
  (b) clamp escaping paths to the mount root; (c) reject every `..` component; (d) use
  `openat2(RESOLVE_BENEATH)` for every host open.
- **Reasoning:** pass-through can turn `Z:/...` into a relative host path, clamping silently
  changes title-visible semantics, and rejecting all `..` needlessly breaks valid in-mount
  paths. Canonical-ancestor checks also cover existing symlink parents and missing files
  below them without introducing a new dependency. `openat2` remains a possible later
  hardening step for race-free opens when kernelx file APIs land.
- **Reversibility:** high — the rejection variant and helpers are Linux-only and localized to
  `vfs.rs`; runtime open hardening can be added in `vfs_hooks_linux.rs` later.
- **Timestamp:** 2026-11-24T12:01:08Z

## DEC-012 — Keep Linux fatal-signal reporting outside the handler

- **Decision:** How to write `report.json` after a fatal signal without making the signal
  handler allocate, take Rust locks, or perform file I/O.
- **Chosen Option:** the `SA_SIGINFO` handler stores signal/PC/address in atomics, writes one
  byte to a `pipe2`, and pauses; a pre-started watchdog thread formats the crash description,
  writes a crash-only portable report, and re-raises the original signal with the default
  action.
- **Confidence:** 90
- **Alternatives Considered:** (a) call the existing report builder directly from the handler;
  (b) hand-format JSON with only async-signal-safe libc calls in the handler; (c) install an
  alternate signal stack and accept the non-async-safe report path.
- **Reasoning:** (a) can deadlock in allocator/log/report locks and is not async-signal-safe;
  (b) would duplicate serialization and still complicate mapped-module naming; (c) only fixes
  stack exhaustion and not lock safety. The watchdog keeps the handler small while preserving
  normal Rust reporting and exact termination status.
- **Reversibility:** high — the pipe/watchdog is contained in `eh_linux.rs`; a future
  `openat2`/preformatted-report design can replace the watchdog without changing the report
  schema.
- **Timestamp:** 2026-11-24T12:41:21Z

## DEC-013 — Use the explicit Windows x64 ABI for Linux PE calls

- **Decision:** How Linux calls manually mapped PE entry points and TLS callbacks.
- **Chosen Option:** type them as `unsafe extern "win64" fn(...)` on Linux, including
  `tls_linux::TlsCallback` and `loader_linux::ExeEntry`.
- **Confidence:** 96
- **Alternatives Considered:** (a) `extern "system"` (the host C ABI on Linux); (b) naked
  assembly bridges; (c) make synthetic PE callbacks use the host ABI.
- **Reasoning:** PE code expects RCX/RDX/R8 and the Windows shadow-space/alignment rules.
  `extern "system"` is SysV on Linux and would pass arguments in RDI/ESI/RDX. Rust supports
  the `win64` ABI on x86_64, so the compiler can emit the correct call sequence without
  hand-maintained assembly; the synthetic callback reads its reason from EDX and proves the
  ABI at runtime.
- **Reversibility:** high — only Linux function-pointer types change.
- **Timestamp:** 2026-11-24T13:05:05Z

## DEC-014 — Linux kernelx core lives in hle/kernelx_linux.rs with handle tables

- **Decision:** How step 10b implements kernelx memory/threads/sync/time on Linux.
- **Chosen Option:** new `hle/kernelx_linux.rs` wired into `hle_linux` (real addresses for the
  10b slice, `0` stubs otherwise) with per-process handle tables: `mmap`/`mprotect` + tracked
  reservations for `Virtual*`, game threads via explicit `tls::attach/detach` (same callbacks as
  `tls::spawn`), reentrant critical sections and auto/manual events in Rust, SRW-exclusive as an
  inline atomic, monotonic `clock_gettime` time, best-effort priority/affinity (never abort), and a
  thread-local last error. File/module/physical-page exports stay stubs for steps 10c–10d.
- **Confidence:** 85
- **Alternatives Considered:** (a) port the whole Windows `kernelx.rs` at once; (b) separate OS
  dispatcher per function family; (c) `pthread_mutex_t` mapped onto the game's CRITICAL_SECTION bytes.
- **Reasoning:** (a) drags UMA/GPU/file paths into this slice; (b) adds layers the module does not
  need; (c) couples the layout to libc internals. The single Linux file keeps the diff reviewable
  and leaves Windows behavior untouched.
- **Reversibility:** high — the slice is additive behind the existing registry/resolver seam.
- **Timestamp:** 2026-11-24T13:40:00Z

## DEC-015 — Linux kernelx files call the VFS directly, no hooks

- **Decision:** How step 10c implements title file I/O on Linux.
- **Chosen Option:** new file section in `hle/kernelx_linux.rs` whose `CreateFileW/A/2`
  call `vfs_hooks_linux::open` and return `File`-holding HANDLEs; metadata/mutation go
  through `Vfs::{delete_file,create_dir,remove_dir,stage_write,commit_write}` with
  `std::fs` host ops; `FindFirst/Next` merge overlay+original in-process with `*`/`?`
  matching; disk space via `statvfs`; `CloseHandle` also closes file/find handles.
  Kernel-family fallbacks are these same exports (resolved as HLE/HleOther); CRT stays stubs.
- **Confidence:** 80
- **Alternatives Considered:** (a) LD_PRELOAD interception; (b) porting the Windows hook
  table with trampolines; (c) implementing CRT fopen/fread in this slice.
- **Reasoning:** (a) contradicts the in-process host model; (b) has no KERNELBASE on Linux
  to hook; (c) is not needed for the VFS acceptance and stays deferred to the e2e slice.
- **Reversibility:** high — additive exports behind the registry/resolver seam.
- **Timestamp:** 2026-11-24T14:20:00Z

## DEC-016 — Next CRT slice is convert/onexit from the synthetic binaries' actual imports

- **Decision:** After 12b, implement `mbrtowc`/`wcrtomb` + `_execute_onexit_table` (step 12c), leaving `__stdio_common_vfprintf` provisional.
- **Chosen Option:** triage the next CRT family from `llvm-readobj --coff-imports` on the cross-built `xcl_testtitle.exe` + `testgame_mw.dll` diffed against `crt_linux.rs` exports, not from PROMPT's example list (`errno`/`math`, `qsort`/`bsearch` — already covered or not imported).
- **Confidence:** 85
- **Alternatives Considered:** (a) `qsort`/`bsearch` next (PROMPT example, but nothing imports them); (b) bundle `__stdio_common_vfprintf` into the same slice (needs a win64 `va_list` format engine, too big for one reviewable commit).
- **Reasoning:** §4.7b orders families by what the exe actually imports. The audit shows exactly 4 MISS UCRT imports; three are small and real (UTF-8 convert pair, onexit no-op consistent with existing register/initialize successes), while vfprintf needs its own design.
- **Reversibility:** high — additive exports behind the existing registry/resolver seam.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-017 — Next slice is FormatMessageW from the title's actual stub list

- **Decision:** After 12d, implement `FormatMessageW` in `kernelx_linux` (step 12e), leaving D3D12/DStorage/MFPlat stubs for later slices.
- **Chosen Option:** triage the next family from the synthetic title's fresh `report.json` `called_stubs` (34 entries at `ExitProcess(0xe)`/14 known fails, 0 MISS UCRT per the 12d `llvm-readobj` re-verification), not from PROMPT's CRT examples (`qsort`/`bsearch`, math — nothing imports them).
- **Confidence:** 80
- **Alternatives Considered:** (a) `qsort`/`bsearch`+math next (PROMPT example, but zero importers in either synthetic binary); (b) DStorage/MFPlat/D3D12 command-list slots next (most of the 34 stubs, but each is a larger multi-function family needing its own design).
- **Reasoning:** §4.7b orders families by what the exe actually calls. `FormatMessageW` is the only `kernel32` stub the title calls (1 call; the rest of the non-GPU stubs are `DStorageGetFactory` x1 and `MF*` x3, each a bigger integration). It is small, has a clear Win32 contract (FROM_SYSTEM/FROM_STRING/inserts/ALLOCATE_BUFFER/truncation), and its disappearance from `called_stubs` is a checkable e2e signal.
- **Reversibility:** high — additive export behind the existing registry/resolver seam.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-018 — Next slice is a headless MFPlat manager from the title's actual stub list

- **Decision:** After 12e, implement headless `mfplat.dll` HLE (`MFStartup`/`MFShutdown`/`MFCreateDXGIDeviceManager`/`MFResetDXGIDeviceManagerX` + manager vtable) as step 12f, leaving `DStorage` and Phase-D D3D12 slots for later slices.
- **Chosen Option:** triage the next family from the synthetic title's fresh `report.json` `called_stubs` (33 entries at `ExitProcess(0xe)`: ~27 Phase-D D3D12 slots, `DStorageGetFactory` x1, `mfplat` trio x1 each, `XclDefinitelyMissingFunction` probe x1) and `stubbed_imports`, not from PROMPT's examples.
- **Confidence:** 82
- **Alternatives Considered:** (a) `DStorageGetFactory` headless next (also x1 call, but factory/queue/file/status/fence surface is larger than one reviewable slice alongside MF); (b) Phase-D D3D12 console slots next (27 stubs across many methods, needs real Vulkan backend — explicitly blocked per §4.7); (c) kernel32 leftovers (`CreateFileMappingA`, `WriteConsoleW`, … — in `stubbed_imports` but zero calls in `called_stubs`, so per §4.7b they stay stubs).
- **Reasoning:** §4.7b orders families by what the exe actually calls. The `mfplat` trio is the smallest called family (3 calls) with a clear headless precedent (`gameinput_linux`/`xaudio2_linux` null objects): `MFCreate` currently returns `S_OK` with a null manager so the title fails before `Reset`; a static manager with token-validated `Reset` (`E_INVALIDARG` on stale token) plus `Open/Test/Close` handle slots flips the MFPlat check to PASS with a checkable `called_stubs` 33→30 signal.
- **Reversibility:** high — additive `mfplat_linux` module behind the existing registry/resolver seam.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-019 — Next slice is a Linux DStorage file-read subset from the title's actual stub list

- **Decision:** After 12f, implement a title-verbs-only `xcrdapi.dll` HLE (`DStorageGetFactory` + factory/queue/file/status slots + synchronous `Submit` + fence signal) as step 12g, leaving Phase-D D3D12 slots for later.
- **Chosen Option:** triage the next family from the synthetic title's fresh `report.json` `called_stubs` (30 entries at `ExitProcess(0xd)`: ~28 Phase-D D3D12 slots, `DStorageGetFactory` x1, `XclDefinitelyMissingFunction` probe x1) and `xcl.log` `[STUB]` lines, not from PROMPT's examples.
- **Confidence:** 78
- **Alternatives Considered:** (a) full `hle/dstorage.rs` port next (2665 lines + `windows_sys` `DuplicateHandle`/`VirtualQuery`/`CreateEventW` surface — too large for one reviewable slice; zlib/BCPack/swizzle/memory-source paths have zero title callers); (b) Phase-D D3D12 console slots next (~28 stubs across many methods, needs real Vulkan backend — explicitly blocked per §4.7); (c) declare stub-triage done and start Phase D GLSL now (premature: one called non-GPU family remains, and its e2e PASS signal is checkable in one slice).
- **Reasoning:** §4.7b orders families by what the exe actually calls. `DStorageGetFactory` is the only called non-GPU, non-probe stub left. The title only exercises uncompressed file→memory reads (512 B exe prefix + physical-page dest + cancel-tag), status `GetHResult`, and fence `Signal`/`SetEventOnCompletion` + 5 s wait — so a synchronous file-read subset flips the DirectStorage check to PASS with a checkable `called_stubs` 30→29 signal while compression/BCPack/swizzle stay refused behind the existing error codes.
- **Reversibility:** high — additive `dstorage_linux` module behind the existing registry/resolver seam.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-020 — Stub-triage exhausted; unblock Phase D GLSL backend with a synthetic gate

- **Decision:** After 12g, start Phase D step 13 (`linux:glsl-backend`) as an unblocked task gated synthetically, leaving steps 14–21 chained behind SKIPPED step-12.
- **Chosen Option:** fresh title triage here (`ExitProcess(0xc)`/12 fails, `called_stubs: 29` = 28 `d3d12_xs` console slots + `XclDefinitelyMissingFunction` probe, zero `xcrdapi`/`mfplat`/`FormatMessage`, zero non-GPU calls) shows no further one-family HLE slice: every remaining FAIL is a null-device D3D12 category needing the real Vulkan backend. PROMPT §4.7's "synthetic title runs to `ExitProcess`" gate is met (retail leg vacuous on this host), so the backend-neutral emitter + GLSL 450 backend is next, verified against a synthetic/in-tree blit-class RDNA2 program with the `XCL_SAMPLE_RPF` retail sample opt-in (no-op when unset, never a counted pass) and `glslc -fshader-stage=vert` bring-up only.
- **Confidence:** 75
- **Alternatives Considered:** (a) keep Phase D blocked behind SKIPPED retail-smoke and publish `review.passed` (stalls the loop — §8.7 forbids consecutive no-diff iterations, and a real Phase-D slice is ready); (b) fold step 13 into the existing chained `code-assist:linux-port:step-13:glsl-backend` by clearing its `blocked_by` (mutates the stable-key queue the replan deliberately left intact); (c) slice a smaller D3D12 null-device piece first (all 28 remaining stubs span many vtable methods and need the Vulkan backend anyway — not one reviewable slice).
- **Reasoning:** §4.7b orders by what the exe actually calls; nothing callable outside Phase D remains. A side `linux:glsl-backend` task (like the 12a–12g side tasks) keeps the stable-key chain untouched while giving the Builder a reviewable vertical slice that runs without game files. The shipped path must not shell out to an external compiler.
- **Reversibility:** high — additive emitter backend behind a trait; the original step-13 chain entry stays provisional.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-021 — Next slice is in-process SPIR-V for the same blit VS

- **Decision:** After 13, implement step 14 (`linux:spirv-inproc`) as an unblocked side task gated synthetically, leaving steps 15–21 chained behind SKIPPED step-12.
- **Chosen Option:** in-process SPIR-V backend for the same blit-class VS the GLSL slice covers (synthetic/in-tree RDNA2 program as the no-retail gate, `XCL_SAMPLE_RPF` retail sample opt-in, `spirv-val` where present else structural word checks, no shell-out in the shipped path). Verify the new dep's license field before adding (`rspirv` only if its `Cargo.toml` license is GPL-2.0-compatible dual MIT-or-Apache-2.0 used under MIT / MIT / BSD / zlib; pure-Apache-2.0-only is out per AGENTS.md — then hand-roll a minimal SPIR-V assembler for the blit subset reusing the `glsl.rs` decode/step shape). Move the shared `VsBackend` trait to `mod.rs` when the third backend lands (13 review follow-up); revisit `TRANSLATOR_SOURCES`/disk-cache identity when translation caching lands.
- **Confidence:** 72
- **Alternatives Considered:** (a) keep Phase D blocked and publish `review.passed` (stalls the loop — a real Phase-D slice is ready and §8.7 forbids consecutive no-diff iterations); (b) fold step 14 into the chained `code-assist:linux-port:step-14:spirv-inproc` by clearing its `blocked_by` (mutates the stable-key queue left intact since 12a); (c) jump to the Vulkan runner first (needs valid SPIR-V words before device dispatch — premature).
- **Reasoning:** §4.7 orders by executable evidence; the GLSL slice proved the blit-subset register/export semantics and the `VsBackend` seam with a real `glslc` gate. The next checkable increment is byte-identical semantics as SPIR-V words with no external compiler, keeping the synthetic title at `ExitProcess(0xc)`/12 fails / `called_stubs: 29` as the regression gate.
- **Reversibility:** high — additive emitter backend; the stable-key step-14 chain entry stays provisional.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-022 — Next slice is the ash Vulkan compute runner with a trivial-kernel gate

- **Decision:** After 14, implement step 15 (`linux:vulkan-runner`) as an unblocked side task gated synthetically, leaving steps 16-21 chained behind SKIPPED step-12.
- **Chosen Option:** Linux-only compute runner on `ash` (verify its `Cargo.toml` license field is GPL-2.0-compatible dual MIT-or-Apache-2.0 used under MIT / MIT / BSD / zlib before adding; pure-Apache-2.0-only is out per AGENTS.md; Vulkan `dlopen`ed at runtime so the static exe ships no `.so`) that enumerates instance/device/queue, builds a compute pipeline from caller-supplied SPIR-V words, dispatches one trivial embedded copy/add kernel, and reads the storage buffer back. Gate on the real device present here (Arc A770 / RTX 3090 Ti / Raphael, `vulkaninfo` 1.4.363); mirror `runner::skip_test` (`SKIPPED <test>: <reason>`, `XCL_REQUIRE_GPU` fast-fail) when no device. Pin 64 work-items and log `subgroupSize`; 32/64-lane selection stays step-16. No window, no graphics pipeline (the VS Vertex words from steps 13/14 need one), no D3D12.X routing change, no `TRANSLATOR_SOURCES` change (no disk cache). Kernel-mode recompiler SPIR-V emission is an explicit follow-up if it does not fit one reviewable slice.
- **Confidence:** 70
- **Alternatives Considered:** (a) keep Phase D blocked and publish `review.passed` (stalls the loop — a real Phase-D slice is ready and section 8.7 forbids consecutive no-diff iterations); (b) fold step 15 into the chained `code-assist:linux-port:step-15:vulkan-runner` by clearing its `blocked_by` (mutates the stable-key queue left intact since 12a); (c) bundle Kernel-mode recompiler emission into the same slice (one slice would then cover both `ash` plumbing and a new emitter backend — too large for one careful review; deferred as a follow-up instead).
- **Reasoning:** section 4.7 orders by executable evidence; the stub-triage loop is exhausted and steps 13/14 proved the VS blit-subset semantics as GLSL text and SPIR-V words without executing anything on a GPU. The smallest checkable execution increment is dispatching caller-supplied SPIR-V compute words on the real device, keeping the synthetic title at `ExitProcess(0xc)`/12 fails / `called_stubs: 29` as the regression gate.
- **Reversibility:** high — additive Linux-only runner module; the stable-key step-15 chain entry stays provisional.
- **Timestamp:** 2026-11-25T00:00:00Z

## DEC-023 — Next slice is subgroup-width-aware dispatch in the ash runner

- **Decision:** After 15, implement step 16 (`linux:subgroup-width`) as an unblocked side task gated synthetically, leaving steps 17-21 chained behind SKIPPED step-12.
- **Chosen Option:** extend `gpu/recomp/vulkan_runner.rs` to read `VkPhysicalDeviceSubgroupProperties::subgroupSize` (via `VK_EXT_subgroup_size_control` where available, `vulkaninfo` here reports `subgroupSize 32` + `subgroupSizeControl = true`) and select the compute lane path from it, mirroring the existing `upper_lanes` / `x64_masks` / `mesh_wave` 64-vs-split-32 decision the Windows `runner::split_for` / `split_wave64` path makes (`wave64_supported` / `wave64_programs_supported` / `pso::graphics_wave32`). Keep `WORK_ITEMS = 64` shaping but stop assuming 64 lanes: a deterministic unit test forces both widths (32 and 64) even though this host reports only 32, so both branches are covered without a second GPU. Fold the three step-15 review follow-ups into the same slice (they are all in the touched file): align `skip_test` arms with `runner.rs` (`wave64` vs `mesh` split), add scoped teardown for the early-`?` error paths inside the `run` closure, and drop or use the unused `_entry` param when subgroup selection lands. No window, no graphics pipeline, no D3D12.X routing change, no `TRANSLATOR_SOURCES` change. `TRANSLATOR_SOURCES`/disk-cache identity revisit stays deferred to caching work.
- **Confidence:** 68
- **Alternatives Considered:** (a) keep Phase D blocked and publish `review.passed` (stalls the loop — a real Phase-D slice is ready and §8.7 forbids consecutive no-diff iterations); (b) fold step 16 into the chained `code-assist:linux-port:step-16:subgroup-width` by clearing its `blocked_by` (mutates the stable-key queue left intact since 12a); (c) jump to the Vulkan present backend first (needs the lane-width decision anyway — the swapchain/graphics pipeline inherits whichever wave width the device runs, so ordering present before width just moves the same decision later).
- **Reasoning:** §4.7 orders by executable evidence; the runner dispatches correctly on this host but hard-codes the 64-lane assumption the recompiler was built to avoid (`upper_lanes` shadow analysis, `x64_masks` exchange liveness, `mesh_wave` requirements, `split_wave64` forced-32 path). The smallest checkable increment is selecting that existing lane decision from the Vulkan device properties with a forced-both-widths unit test, keeping the synthetic title at `ExitProcess(0xc)`/12 fails / `called_stubs: 29` as the regression gate.
- **Reversibility:** high — additive runner logic behind the existing `dispatch_compute` seam; the stable-key step-16 chain entry stays provisional.
- **Timestamp:** 2026-11-25T00:00:00Z
