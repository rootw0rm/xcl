# Memories

## Patterns

### mem-1795559935-ba49
> Linux CRT terminate/EH plumbing (_purecall/_invoke_watson/terminate/unexpected with handler storage, _invalid_parameter/noinfo dispatch, _CxxThrowException aborts, __CxxFrameHandler3/4 return 1 ContinueSearch) lives in crt_linux and resolves via UCRT HLE for bundled vcruntime/msvcp/concrt routes and api-ms-win-crt-runtime contracts, same NULL-stub fault class as memcpy. Verify with resolver Hle(ucrtbase) checks plus direct FrameHandler==1 and handler set/get/dispatch unit test.
<!-- tags: linux-port, crt, hle | created: 2026-11-24 -->

### mem-1795559676-5175
> Linux CRT operator new/delete (??2@YAPAXI@Z/??2@YAPEAX_K@Z scalar, ??_U vector, ??3/??_V scalar/vector incl. sized PEAX_K, plus _callnewh/_set_new_handler/_query_new_handler) lives in crt_linux as libc malloc/free wrappers and resolves via UCRT HLE for bundled vcruntime/msvcp/concrt routes, same NULL-stub fault class as memcpy. Verify with resolver Hle(ucrtbase) checks plus direct win64 op_new/op_delete roundtrip and null-delete no-op.
<!-- tags: linux-port, crt, hle | created: 2026-11-24 -->

### mem-1795559487-fa65
> Linux CRT string compare/search (strcmp/strncmp/wcscmp/wcsncmp/strchr/memchr) lives in crt_linux as NUL-safe win64 loops and resolves via UCRT HLE for both api-ms-win-crt-string contracts and bundled vcruntime/msvcp/concrt routes, same fault class as memcpy->NULL. Verify with resolver Hle(ucrtbase) checks plus direct strcmp/memchr unit test.
<!-- tags: linux-port, crt, hle | created: 2026-11-24 -->

### mem-1795559318-8a00
> Linux bundled CRT DLLs (vcruntime140/msvcp140/concrt140/vcomp140/vccorlib140) route to UCRT HLE in hle_linux::build so memcpy/malloc etc hit crt_linux real code, skipping the game-DLL branch that would load bundled files; contracts stay routeless via find_any. Verify with resolver test showing Hle(ucrtbase), direct win64 memcpy call, on-disk shadowing, and LoadLibrary/GetProcAddress.
<!-- tags: linux-port, crt, hle | created: 2026-11-24 -->

### mem-1795558975-daaf
> Linux report.json tracks called stubs and import sources: report.rs called_stubs/import_stats/source_label were stubbed to empty behind cfg(windows); un-gating works because crate::resolver on Linux is resolver_linux with identical records()/Source and stubs::all_stubs/modules::describe_address exist on Linux. Verify with cross-built testtitle run: 34 called stubs incl. XclDefinitelyMissingFunction, imports_by_source populated.
<!-- tags: linux-port, report, hle | created: 2026-11-24 -->

### mem-1795558966-c8e0
> Linux report.json tracks called stubs and import sources: report.rs called_stubs/import_stats/source_label were stubbed to empty behind cfg(windows); un-gating them works because crate::resolver on Linux is resolver_linux with identical records()/Source and stubs::all_stubs/modules::describe_address exist on Linux. Verify with cross-built testtitle run: report shows XclDefinitelyMissingFunction and populated imports_by_source.
<!-- tags: linux-port, report, hle | created: 2026-11-24 -->

### mem-1795541191-28aa
> Linux step-11s ships shader/texture creation past stubs: d3dcompiler_linux D3DCompile echoes source into a Blob (never copy payload over the COM header - corrupt vtbl faults in windows-core assume_vtable with the source bytes as pointer), xg_linux re-exports hle/xg.rs via [path] include after switching it to extern win64 (identical on x86_64 Windows) and moving CONSOLE_PIPES/console_pipes to gpu/tiling (gpu/texture is windows-only), d3d12x_linux console slots 44 PlacedResourceX (VA=title memory, Kind::PlacedResource) / 47 S_OK / 48-73 DMA queue+list with immediate host memcpy / 52 S_OK / 55 DerivedX (patch[0]==2 ok, 4 fails) / 56 E_ACCESSDENIED / 60 PsoX / 74-75 page pools, plus OP11 DMA CopyBufferRegion on CommandList vtable. Slots 44/47 collide with PC PipelineLibrary/stream (title never uses them).
<!-- tags: linux-port, hle, d3d12, gpu | created: 2026-11-24 -->

### mem-1795540579-a959
> Linux D3D12.X null device is a win64 port of hle/d3d12x/null.rs without windows crates: same 256-slot PC vtable guesses with logging stubs, NullObj leaked with refs clamped, queue Signal fetch_max completes fence instantly, Fence SetEventOnCompletion calls kernelx_linux SetEvent on the handle value, clock via clock_gettime, Map via libc malloc+zero, shader capture no-ops on Linux (readable false). Wire via merge into d3d12x_exports; hle_exports! macro returns 0 on Linux so write exports() manually with as usize. Dynamic LoadLibrary path (not static imports) is what the title uses, so inspect stub count may not move.
<!-- tags: linux-port, hle, d3d12 | created: 2026-11-24 -->

### mem-1795540291-7b97
> Linux headless XAudio2 is a win64 timing fake, not a decoder: XAudio2Create hands a static engine (slots 2/5/7 real), voices are leaked heap objects with tag/channels/rate + callback ctx, Submit derives samples from play_length (XMA2) / 4800 for Opus / bytes-per-frame for PCM and duration_ms clamped 20-2000, Start spawns a thread sleeping that long then fires OnBufferStart/OnBufferEnd(ctx)/OnStreamEnd via win64, GetState shows queued=1+ctx before and queued=0+samples after with packed 20-byte layout. Accept console flags 0x41/processor 0x3c silently; mastering voice has no callback.
<!-- tags: linux-port, hle, xaudio2 | created: 2026-11-24 -->

### mem-1795539932-3e29
> Linux XUser/XAsync/PLS is a win64 port of xasync.rs + mod.rs user/pls/save sections onto xtaskqueue_linux queues: ACTIVE map keyed by block addr + generation id, begin_with payload with fixups, offline user is a static USER pointer with refs clamped at 1, token path returns E_CANNOT_CONNECT when xbox_live_tokens is off, PLS path prefers config.pls_drive (S:\) with host folder ensured via data_dir/pls and space via statvfs quota-minus-used. Keep exports to the Windows 127-name table (no DeviceAssociation extras) or the hle registry count test fails.
<!-- tags: linux-port, hle, xuser | created: 2026-11-24 -->

### mem-1795539642-6594
> Linux XTaskQueue is a portable port of taskqueue.rs minus windows_sys: manual ports queue VecDeque + Condvar, delayed go on a shared timer heap, pool/serialized spawn via a shared mpsc worker pool, composite queues share Arc<Port> so Terminate cancels only own items, Terminate sets terminated then drains own queued+delayed with canceled=true and waits on idle_cv. RegisterWaiter has no host wait object so return E_NOTIMPL. Networking is headless UDP 3074 + InternetAccess/initialized hint[12]==1. advapi32legacy LsaOpenPolicy needs a real HLE address for GetProcAddress probes even though the title never calls it.
<!-- tags: linux-port, hle, xtaskqueue | created: 2026-11-24 -->

### mem-1795539383-f7c9
> Linux GameInput is a headless static IGameInput: 24-slot vtable with QI/AddRef/Release/Timestamp real and the rest stub thunks via stubs::make_stub_info; timestamps are Instant micros+1 so sleep(2ms) advances; Release clamps at 1 (static object never freed). Wire via merge into gameinput_exports like minuserx so gameinput.dll resolves as HLE.
<!-- tags: linux-port, hle, gameinput | created: 2026-11-24 -->

### mem-1795539185-e2cc
> Linux minuserx window is headless fakes with console semantics: RegisterClassW/Ex return monotonic atoms, CreateWindowExW/A return fake HWNDs, GetWindowLongW -20 returns 0x80 and -16 returns 0x80000000, GetClientRect returns 1920x1080, DwmGetWindowAttribute attr 14 sets cloaked=1 with S_OK. user32.dll/dwmapi.dll stay routeless and resolve via cross-HLE find_any (same pattern as CRT), so implement in minuserx_linux and merge into minuserx_exports.
<!-- tags: linux-port, hle, user32 | created: 2026-11-24 -->

### mem-1795538934-42b5
> Linux file reads go through NtReadFile + GetFileInformationByHandle(Ex), not just ReadFile: Rust std get_long_path calls GetFullPathNameW before CreateFileW (stub returning 0 looks like FILE_NOT_FOUND before any open; strace shows no open attempt), so implement GetFullPathNameW as copy-through for console-absolute (G:/GAME:/drive/UNC) and G:relative for relative paths, plus GetCurrentDirectoryW as game root. NtReadFile fills IOSB Status+Information like NtWriteFile; CRT fopen/fread go through vfs_hooks::open with the same overlay semantics as CreateFileW.
<!-- tags: linux-port, hle, files | created: 2026-11-24 -->

### mem-1795538410-11df
> Linux LoadLibrary is a thin resolver wrapper: LoadLibraryW/A/ExW/ExA + LoadPackagedLibrary call resolver::load_library (HLE pseudo-handles, mapped bases, game-DLL load path); NULL fails ERROR_INVALID_PARAMETER, miss fails ERROR_MOD_NOT_FOUND. FreeLibrary never unloads (known handles succeed, unknown fail); DisableThreadLibraryCalls records no_thread_calls on mapped DLLs. GetProcAddress stays null-on-miss for probing, so Stub-route DLLs still miss dynamically even though static imports become stubs.
<!-- tags: linux-port, hle, loader | created: 2026-11-24 -->

### mem-1795538073-ecc4
> Linux XMem is a high-VA pool plus malloc homes: graphics allocations come from an 8 GiB PROT_NONE mmap at/above 16 GiB with first-fit + coalescing (64 KiB grain, 4 MiB pages), committed via mprotect and zeroed so freed blocks merge and reuse at the same address; heap types are libc aligned+zeroed with raw-pointer headers; physical pages are calloc homes with frame-count numbers, mapping commits pool VAs and moves data with the page (unmap copies back home), freed pages keep VA contents. VirtualFree releases pool ranges back to the free list.
<!-- tags: linux-port, hle, xmem | created: 2026-11-24 -->

### mem-1795537619-4f70
> Linux xgameruntime identity shims return console constants without OS calls: XSystemGetDeviceType is 7 Series X / 6 Lockhart from config.is_series_s, XGameRuntimeInitialize is idempotent S_OK, package ID is config.package_identifier or boot.title plus _1.0.0.0_x64__xcl, console/sandbox IDs via write_cstr_out. Wire as real win64 addresses merged into xgameruntime_exports so cross-HLE find_any resolves them as HLE.
<!-- tags: linux-port, hle, xgameruntime | created: 2026-11-24 -->

### mem-1795537186-c492
> Linux ExitProcess/TerminateProcess are non-returning HLE shims: write report.json with the title exit code via report::write_final_with_code, flush logs, then std::process::exit. The launcher summarize patch only runs on the entry-return path, so ExitProcess must set exit_code itself or the report keeps exit_code null. TerminateProcess only supports the current-process pseudo-handle (-1); foreign handles fail with ERROR_INVALID_PARAMETER.
<!-- tags: linux-port, hle, process | created: 2026-11-24 -->

### mem-1795536750-5668
> Linux SEH dispatch needs naked capture/restore plus two-phase RaiseException: VEH returns 0 for ContinueSearch (-1 resumes), so treat -1 as ContinueExecution; RtlUnwindEx must handle target-below-current (direct restore with RAX=ReturnValue/RDX=Info[3]) or it returns and the guest calls abort; __rust_try currently resolves to normal return (EB) instead of catch, scan forward for catch prolog 48 8B 4C 24 30
<!-- tags: linux-port, eh, seH | created: 2026-11-24 -->

### mem-1795535180-9501
> Linux .pdata unwinding reads the mapped image in place: parse RUNTIME_FUNCTION + UNWIND_INFO from guest memory, binary-search lookup, unwind fixed frames (PUSH_NONVOL/ALLOC_SMALL/ALLOC_LARGE-OpInfo0/SET_FPREG, ignore SAVE_XMM) with prolog-offset skipping and FP-derived start RSP; SAVE_NONVOL/MACHFRAME/chained/32-bit allocs fail closed. Stack reads use process_vm_readv to avoid host SIGSEGV; CONTEXT integer offsets RAX 0x78..RIP 0xF8.
<!-- tags: linux-port, eh, unwind | created: 2026-11-24 -->

### mem-1795534406-563f
> Linux WaitOnAddress is a futex emulation keyed by guest address: copy addr/cmp bytes, return 0 immediately on mismatch; else register a private condvar, re-check after registering (a write+wake that raced registration counts as a wake), then block with timeout (0 on timeout with ERROR_TIMEOUT). WakeByAddressSingle wakes one waiter, All wakes all; NULL addresses are no-ops.
<!-- tags: linux-port, hle, synch | created: 2026-11-24 -->

### mem-1795534406-486b
> Linux waitable timers are host threads with a generation counter: CreateWaitableTimerExW/W allocate a TimerState (manual flag from flags/manual), SetWaitableTimer disarms (clears signal, bumps generation) then spawns a thread sleeping the due (negative=relative, positive=absolute FILETIME, 0=immediate) and re-firing every period; Cancel/Close bump the generation so stale sleepers never fire; sync timers consume on WaitForSingleObject/Multiple, manual timers stay signaled. Never write *lock()=lock() (deadlocks the non-reentrant Mutex); compute next then store.
<!-- tags: linux-port, hle, synch | created: 2026-11-24 -->

### mem-1795533372-d6fb
> When a Linux HLE shim wraps RaiseException, log-and-return is the honest short-term behavior: mingw _Unwind_RaiseException always returns 5 after calling it (Windows never returns), so Rust reports 'failed to initiate panic, error 5'. Real C++ unwinding needs .pdata translation (later Phase C); do not fake success beyond logging.
<!-- tags: linux-port, hle, eh | created: 2026-11-24 -->

### mem-1795533372-c48f
> Linux panic-report needs NtWriteFile (not WriteFile): Rust std synchronous_write calls ntdll!NtWriteFile directly and reads bytes-written from the IO_STATUS_BLOCK, so a stub returning STATUS_SUCCESS without filling Information looks like a 0-byte write and the panic message is lost before the int-29 fastfail. Implement NtWriteFile with Status+Information fill; keep GetConsoleMode failing on redirected fds so std takes the NtWriteFile path.
<!-- tags: linux-port, hle, panic | created: 2026-11-24 -->

### mem-1795532533-de60
> When a Linux HLE GetModuleHandle must not load: return hle_handle for HLE routes, find_hle_handle-or-0 for @stub routes, mapped base for game DLLs, 0 otherwise; FROM_ADDRESS resolves via image_base_for_addr. GetEnvironmentVariable returns need-with-NUL on small buffers with ERROR_INSUFFICIENT_BUFFER and 0+ERROR_ENVVAR_NOT_FOUND when missing.
<!-- tags: linux-port, hle | created: 2026-11-24 -->

### mem-1795532533-d2ce
> Linux FLS needs a real 128-slot table (process-wide alloc bitmap + per-thread values) because Rust std TLS-guard uses FlsAlloc/FlsSetValue; auto-stub returning 0 pushes the guard into its stderr-write failure path that faults at exe+0xae3e4. Call game callbacks with win64 ABI on thread exit (fls_thread_exit from CreateThread wrapper + run_main), and back GetStdHandle with fd-backed WriteFile/ReadFile so the guard's WriteFile(STD_ERROR) lands on host stderr.
<!-- tags: linux-port, fls, hle | created: 2026-11-24 -->

### mem-1795531153-5093
> On Linux, UCRT __p___argv/__p__environ are char*** (pointer to the argv array variable), not char** (the array): mingw __getmainargs dereferences once before storing the argv global, so returning the array makes startup strlen the program-name bytes as a pointer. gdb showed s=0x534c5405006c6378 = ASCII 'xcl' plus rodata tail. Keep an ARRAY cell plus a VAR cell pointing at it and return the VAR.
<!-- tags: linux-port, crt, hle | created: 2026-11-24 -->

### mem-1795530441-4fe5
> Linux CRT startup needs real pointer shims, not null stubs: mingw mainCRTStartup copies _fmode/_commode through __p__fmode/__p__commode, so implement them (plus argc/argv/environ/errno/__acrt_iob_func, narrow-init successes, genuine _initterm walks, libc malloc/string) in a routeless ucrtbase HLE module resolved via cross-HLE find_any; gdb backtraces distinguish host-shim faults (0x55.. pc) from game faults (0x140.. pc)
<!-- tags: linux-port, crt, hle | created: 2026-11-24 -->

### mem-1795529960-9ad4
> Linux loader_linux now resolves imports like Windows (register before resolving, resolver::resolve_import per entry with IAT bounds checks, delay handles via load_library); main images with imports load instead of failing. Test title reaches entry then SIGSEGV in CRT startup because api-ms-win-crt stubs return 0, so full testtitle e2e needs a CRT system leg which no task scopes yet
<!-- tags: linux-port, loader, resolver | created: 2026-11-24 -->

### mem-1795529956-9c44
> Linux cross-build uses x86_64-pc-windows-gnullvm with llvm-mingw from Arch package extracted to /tmp (provides x86_64-w64-mingw32-clang); export PATH to that bin before cargo build. DEC-004 preferred target works when the wrapper is on PATH because gnullvm needs the mingw clang driver plus its bundled headers/libs
<!-- tags: linux-port, build, mingw | created: 2026-11-24 -->

### mem-1795529509-dcb4
> Linux xcl run enters through launcher_linux::run and host_linux::run_boot in-process: resolve game folder/exe, XDG data/config/manifest/log/report setup, VFS init, HLE/resolver init, loader load_main/run_main; CPU-only preflight with XCL_SKIP_CPU_CHECK escape; missing folders return 2 and non-PE loads fail with 0x7C1_0002 plus a crash report; VFS/state globals require child-process isolation for e2e tests
<!-- tags: linux-port, launcher, host | created: 2026-11-24 -->

### mem-1795528164-b147
> Linux kernelx file APIs live in hle/kernelx_linux.rs and call vfs_hooks_linux::open plus Vfs stage/commit; unmapped/traversal paths return Win32 errors before host access. Resolver treats them as HLE/HleOther while CRT stays stubs.
<!-- tags: linux-port, kernelx, vfs | created: 2026-11-24 -->

### mem-1795527434-67e9
> Linux kernelx core (step 10b) lives in crates/xcl/src/hle/kernelx_linux.rs: VirtualAlloc/Free/Protect via mmap/mprotect with tracked reservations (occupied base is ERROR_INVALID_ADDRESS, flagless type rejected, DECOMMIT zeroes via MADV_DONTNEED); game threads enter through tls attach/detach with per-handle priority memory and best-effort affinity (fit_affinity_mask mirrors Windows); sync is reentrant critical sections + manual/auto events + inline-atomic SRW-exclusive; time is clock_gettime monotonic QPC/QPF/ticks; last error is thread-local. hle_linux merges real addresses over the 59-name stub table plus kernel-family fallbacks; resolver returns Hle for the 10b slice. Pre-existing failure vfs_hooks_linux ancestor-whiteout also fails on the clean tree (root ignores 0o500), left untouched.
<!-- tags: linux-port, kernelx | created: 2026-11-24 -->

### mem-1795525646-b0cc
> On Linux, manually mapped PE entry points and TLS callbacks must use unsafe extern "win64" function pointers, not extern "system" (which is host SysV and passes arguments in RDI/ESI/RDX instead of RCX/EDX/R8). Rust emits the Windows shadow-space and alignment rules for the win64 ABI.
<!-- tags: linux-port, loader, tls | created: 2026-11-24 -->

### mem-1795524162-e72d
> On Linux, crate::eh is eh_linux: fatal sigaction handlers store signal/PC/address in atomics and wake a pipe watchdog; report::write_crash runs in that normal-context thread before the original signal is re-raised. Keep allocation, Rust locks and file I/O out of the signal handler.
<!-- tags: linux-port, eh, report | created: 2026-11-24 -->

### mem-1795522896-9c71
> On Linux, console paths are classified in vfs.rs and opened through vfs_hooks_linux::open (re-exported as vfs_hooks); there is no LD_PRELOAD. Game, overlay, and title-data mount paths are containment-checked, unsafe traversal/symlinks return Target::Reject before host access, content-data/overwrite rights are exact, and a VFS write lock spans staged host opens so failed-open whiteout rollback cannot erase concurrent changes. Default mounts still come from Config.
<!-- tags: linux-port, vfs | created: 2026-11-24 -->

### mem-1795519445-1a19
> Step-10 Linux TLS obligations recorded in progress.md: late-installed images must not send DLL_THREAD_DETACH to threads that attached before the install (track the image count at attach), and preallocate the per-thread slot array (Windows uses 64 entries) before DLLs load during gameplay so a late install cannot realloc it under a running gs:[0x58] reader.
<!-- tags: linux-port, tls | created: 2026-11-24 -->

### mem-1795519237-a4b0
> Linux PE static TLS lives in crates/xcl/src/tls_linux.rs (exposed as crate::tls on non-Windows): each attached thread gets a fake TEB via arch_prctl(ARCH_SET_GS) with ThreadLocalStoragePointer at gs:0x58, blocks are copies of the relocated template, the main image takes slot 0 and writes the image's _tls_index. Game threads must enter through tls::spawn (attach/callbacks/detach); load_main installs the main image only, DLL TLS install belongs to the step-10 game-DLL load path.
<!-- tags: linux-port, tls | created: 2026-11-24 -->

### mem-1795518636-a528
> Linux PE mapping lives in crates/xcl/src/loader_linux.rs, exposed as crate::loader on non-Windows via #[path]; loader.rs stays the Windows implementation. map_image uses mmap(MAP_FIXED_NOREPLACE) at ImageBase and treats an occupied base as a hard error (no fallback); imports/TLS install/.pdata are LINUX-TODO steps 10/7/9, so the IAT is intentionally empty until the HLE resolver lands.
<!-- tags: linux-port, loader | created: 2026-11-24 -->

### mem-1795518189-6d89
> When a Linux offline tool needs HLE DLL routing, use crates/xcl/src/hle_routes.rs (module_for / is_console_game_dll); hle::build consumes the same ordered ROUTES table, so never add a second DLL pattern list. On Linux, routed functions stay STUB until the HLE modules compile (Phase C).
<!-- tags: linux-port, hle, inspect | created: 2026-11-24 -->

### mem-1795516925-af39
> When extracting RPF records on Linux, publish the finished temp file with link(2)+remove (hard_link), not rename(2): the extraction contract is no-clobber and rpf::tests::atomic_extraction_cleans_up_an_interrupted_write asserts AlreadyExists when the destination appeared before commit
<!-- tags: rpf, linux-port | created: 2026-11-24 -->

## Decisions

## Fixes

### mem-1795560059-1d90
> When cargo clippy -p xcl --lib fails with deny approx_constant TAU on a float literal like 6.283_185_3, replace it with std::f32::consts::TAU (bit-identical, satisfies the lint) rather than allowing the lint, because backpressure law requires 0 clippy errors before build.done.
<!-- tags: tooling, clippy, dxil | created: 2026-11-24 -->

### mem-1795536750-627b
> When RaiseException dispatch finds no handler but logs show handler frames with disp=1, check VEH first: Rust stack_overflow VEH returns 0 (ContinueSearch) for GCC 0x20474343; treating 0 as ContinueExecution causes immediate restore and error 5 without SEH search
<!-- tags: linux-port, eh, debugging | created: 2026-11-24 -->

### mem-1795535180-a565
> When RtlVirtualUnwind fails closed on an unsupported op, return NULL with a warn log instead of guessing RSP: a wrong RSP corrupts the SEH search worse than a clean miss, and the next slice extends op coverage without changing call sites.
<!-- tags: linux-port, eh, unwind | created: 2026-11-24 -->

### mem-1795531153-5a94
> When a Linux HLE shim returns null and the game faults on it, disassemble the import's static caller (objdump -d --start-address) to see which contract it really uses: mingw __getmainargs never calls GetCommandLineW, it reads __p___argc/__p___argv/__p__environ, so the fix belonged in crt_linux pointer levels, not the new kernelx shims. Decode the fault pointer as ASCII/UTF-16 before assuming which shim is at fault.
<!-- tags: linux-port, debugging, hle | created: 2026-11-24 -->

### mem-1795529004-7390
> When the Linux ancestor-whiteout test fails with ERROR_DIR_NOT_EMPTY (145) on v.remove_dir, white out the children with delete_file first: upstream #997 made remove_dir reject non-empty virtual directories.
<!-- tags: linux-port, vfs, testing | created: 2026-11-24 -->

### mem-1795524772-08a5
> When ralph tools task ready is empty during a pending review, rerun it with --all before concluding the queue is empty, because .ralph/current-loop-id can point to a newer loop while the reviewed tasks belong to an earlier loop
<!-- tags: tooling, ralph | created: 2026-11-24 -->

### mem-1795517550-349b
> When verifying xcl output with a shell pipe, rc 101 is usually SIGPIPE from Rust println! (e.g. 'xcl disasm big.dxbc | head'), not a panic: redirect to a file and check its rc before reporting a failure
<!-- tags: testing, cli, linux-port | created: 2026-11-24 -->

### mem-1795517459-3d68
> When strace is needed on this host it is not installed and sudo needs a password: download the Arch extra package to /tmp, bsdtar -xf, and run /tmp/strace-root/usr/bin/strace; it works without root
<!-- tags: tooling, strace | created: 2026-11-24 -->

## Context

### mem-1795525646-d014
> On Linux, loader_linux::run_main attaches the calling thread, runs main-image process TLS callbacks, calls the Win64 entry, runs process detach while TLS blocks are alive, then tls::detach_current_thread for thread detach and GS restoration. load_main currently rejects unresolved normal/delay imports until the step-10 resolver lands; PEB/argv and the PE-requested stack remain step-10d work.
<!-- tags: linux-port, loader, tls | created: 2026-11-24 -->

### mem-1795518636-a514
> Retail game_scarlett_master.exe maps on Linux at 0x140000000 (size 0xb06d000): 10 sections, entry +0x56a1ec8, 68 import DLLs/587 functions, 2 TLS callbacks, template 5248 B, .text r-xp. Re-run with XCL_SAMPLE_EXE=<exe> cargo test -p xcl --lib loader::tests::maps_the_retail_exe_when_sample_set -- --nocapture.
<!-- tags: linux-port, loader, retail | created: 2026-11-24 -->

### mem-1795518189-6d73
> Retail game_scarlett_master.exe (Linux inspect baseline): 587 imports from 68 DLLs, 157 routed-to-HLE STUBs, 0 HLE; only d3d12_xs.dll marks it a console title. Headers: ImageBase 0x140000000, size 0xb06d000, entry +0x56a1ec8, TLS template 5248 B, 299257 .pdata, stack 0x100000. llvm-readobj agrees 587/68.
<!-- tags: linux-port, retail, pe | created: 2026-11-24 -->

### mem-1795517824-32fa
> Retail shader corpus baseline (debug build, Linux): 26,503 .dxbc in leonida-work/shaders/dxbc = 26,698 RDNA2 programs, 44,422,713 instructions, 452 mnemonics, 0 undecoded dwords; stats 24.85 s, sample 200 --recomp 0.23 s, shaders.rpf --recomp 26,698/26,698 complete in 64.9 s. Exclude the sibling side data when scanning the tree (3.6 G dxbc/disasm/*.s, 477 M dxbc/fssm, 18 G spirv/*.spv) by pointing xcl at a symlink view of the .dxbc files, otherwise .s text is counted as shaders.
<!-- tags: linux-port, gpu, retail, performance | created: 2026-11-24 -->

### mem-1795517459-305e
> Retail FSPX driver compute containers live in /run/media/anon/12TB_B/leonida-work/shaders/fspx/*.dxbc (ACCC part holding FSPX); the disasm listing decodes raw ACCC bytes and shows garbage before the real program at +0x380, because shader::classify/rdna2_listing do not special-case ACCC - use gpu::recomp::pso::fspx() for structured fields
<!-- tags: linux-port, gpu, retail | created: 2026-11-24 -->

### mem-1795517185-620c
> After step-02, shared Linux platform leaves live in crates/xcl/src/platform.rs (data_root precedence, process_private_bytes); platform is always compiled, while cfg(windows) launcher/host/kernelx call into it. Put future leaves there rather than un-gating those Windows-only modules.
<!-- tags: linux-port, modules | created: 2026-11-24 -->

### mem-1795516925-b9b2
> After step-01 of the Linux port, launcher/host/inspect/loader/hle/vfs/tls/eh are cfg(windows) in lib.rs; platform leaves needed on Linux (data_root, process memory) must live in a shared or Linux-only module, not inside those gated modules
<!-- tags: linux-port, modules | created: 2026-11-24 -->
