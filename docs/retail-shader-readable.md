# Retail Step 90 — Linux `gpu::shader::readable`, and the shaders it unlocks

Date: 2026-11-28. Base: Step 89 (`fe5c92fd`). Task key:
`code-assist:xcl-first-frame:step-90:shader-readable`.

`gpu::shader::readable` is the gate every shader-capture and console
descriptor path uses before touching a title pointer. On Linux it was a
stub that always answered `false`, so `capture` refused every blob before
reading it and the descriptor writers refused every title descriptor.

## Input (Step-89 artifact)

`/tmp/xcl-step89-r1/GTA6.Xbox.v1.0.2.1-CYBERLEEK/logs/report.json`:

```json
"shaders": {
  "unique": 0, "submitted": 106, "by_kind": {}, "by_stage": {},
  "rejected_pointers": 106, "xbox_hw_stages": {}
}
```

`xcl.log` carried five
`[SHADER] rootsig: invalid pointer/size (…, 16) – different struct layout
on the console?` warnings: the 106 root signatures were the only
submissions that got as far as `capture`, and all were refused.

## Change

`crates/xcl/src/gpu/shader.rs` — the Linux `readable(p, len)` now answers
from `/proc/self/maps`:

- the Windows guards are kept: `p == 0`, `len == 0`, `len > 256 MiB` and
  `p + len` overflow are `false`; `uma::get()` is Windows-only, so the
  Linux path has no committed-window shortcut (the graphics pool is
  `mprotect`ed `PROT_READ | PROT_WRITE`, so its mappings answer normally);
- every `/proc/self/maps` line overlapping `[p, p+len)` is clipped to that
  range, sorted by start, and must cover it with no hole and a permission
  field starting with `r` (`---`/`PROT_NONE` and execute-only mappings
  refuse);
- only the maps text is read, never `[p, p+len)` itself, so a garbage
  title address cannot fault here. That is what makes it usable from
  `capture`/descriptor writers. The crash handler does not call it on
  Linux (`eh.rs` is Windows-only; `eh_linux.rs` has its own syscall-only
  maps parse for signal context).

`hle::kernelx_linux::gfx_range_readable` was the same function under a
Linux-only name (its doc claimed `readable` must stay a refusal because a
signal context calls it — false on Linux). It is deleted and its callers
(`title_descriptor_heap_memory`, `Device_CreateSamplerX`,
`write_console_view`, `Device_CreateShaderResourceViewAt`, and the
resource-desc check) call `gpu::shader::readable` directly; the nearby
"`readable` is windows-only" comments are corrected. Behaviour is
unchanged — the two bodies were identical.

No cache: the parse runs at PSO creation and descriptor writes, not per
draw (the retail run below is the check that it is not hot).

## Tests

- RED (parent tree): the new
  `gpu::shader::tests::readable_answers_from_proc_self_maps` fails at
  `assertion failed: readable(v.as_ptr() as usize, v.len())` — 0 passed /
  1 failed.
- GREEN 1/0. The test covers a live heap buffer, `readable(0/0x10, 8)`,
  `len == 0`, `len > 256 MiB`, overflow, a three-page anonymous mapping
  (full range and an interior range), one page `mprotect`ed `PROT_NONE`
  (single page, and a range crossing into it), a hole after `munmap`
  (single page and the whole range), and the unmapped address after
  `munmap`.
- `gpu::shader` 3/0; `cargo check -p xcl --tests` and `cargo check
  --workspace` exit 0; full lib **997 passed / 0 failed / 1 ignored**
  (`--test-threads=4`); launcher_e2e 7/0.

## Retail delta (one fresh-root run)

`XCL_DATA_ROOT=/tmp/xcl-step90-r1 ./target/debug/xcl run
/run/media/anon/12TB_B/torrents/GTA6.Xbox.v1.0.2.1-CYBERLEEK`, debug
build, no extra env:

- first present 7.318 s (`token 0x1`), **559** presents, last `token
  0x22f` at 27.002 s; exit **133 SIGTRAP
  `game_scarlett_master.exe+0x2219651`** (pc 0x142219651, fault 0) — the
  same parked trap as Steps 83–89, not worse.
- `shaders`: `submitted 2848`, `rejected_pointers 0`, `unique 2735`,
  `by_stage {cs: 2742, rootsig: 106}`, `by_kind {unknown: 1,
  xbox-native: 2734}`, `xbox_hw_stages {CS: 2734}`, `xbox_errors 0`,
  `unknown_parts {}`, `rdna2_instructions 15_612_338`,
  `rdna2_undecoded_dwords 0`. The one `unknown` blob is the 16-byte root
  signature (all 106 submissions hash alike); every `cs` blob is an
  Xbox-native container with `SFI0/ISG1/OSG1/PSV0/RTS0/STAT/ILDN/HASH/DXIL`
  parts and parses (2734 unique, 0 recompiled yet — the emitter is not on
  this path).
- Warning set 33 → 28: the difference is exactly the five rootsig
  "invalid pointer/size" lines; no new warning. `called_stubs` is the same
  19-entry set as Step 89 (diff empty), `stubbed_imports` stays 329,
  `unimplemented` stays `{XSpeechSynthesizerCreate: 1}`.
- The present count rose 137 → 559. The plausible cause is the
  descriptor gates that were refusing host pointers before
  (`Device_CreateSamplerX`, `write_console_view`,
  `Device_CreateShaderResourceViewAt`): with `readable` answering true
  they now write sampler/view descriptors and capture the PSO bytecode
  (2848 submissions vs 106). This is an observation, not an isolated
  cause: no frame/OOM causality is claimed, and the exit and PC are
  unchanged.
- Log volume: `logs/xcl.log` 29 KB → 424 KB (3437 lines), of which 2735
  are `[SHADER]` info lines (one per unique blob, dumps off). With
  `XCL_DUMP_SHADERS=1` this run would also write 2735 `.bin` files plus
  RDNA2 listings, so dumps stay opt-in.
