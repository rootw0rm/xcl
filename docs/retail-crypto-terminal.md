# M1: RPF "encryption blocks boot" hypothesis — DEAD (retracted)

Date: 2026-11-27. Tip: `linux` @ `cdb5d59f`. No code changed (read-only
investigation: entropy windows in place + codebase grep).

## Verdict (CORRECTED — the ciphertext-terminal claim below is WRONG)

**Retracted same day after direct verification.** The "XCL feeds
ciphertext" conclusion rested on a single worker's over-claimed
evidence that fails on re-check:

- The "byte-identical 192B prefix across files" is FALSE: direct
  `open().read()[0x40:0x40+192]` comparison of `appdata0.rpf` vs
  `common.rpf` → **not identical** (verified by hand, not delegated).
- `leonida-work/shaders/dxbc/` holds **25,739 extracted `.dxbc`**
  files plus `bvh/`, `spirv/`, carves and `ttpool` extracts — content
  is readable without any keys, exactly as `gpu/pack.rs` documents:
  **only the TOC is encrypted; shader/content records are plain**
  (deflate-raw → DXBC).
- `rpf_probe.rs` `TocEvidence::Encrypted` fires on shared TOC bytes,
  which is equally consistent with identical plaintext header
  templates — and the TOC is not on the consumed path anyway: the
  game-visible blob was verified byte-identical to file bytes
  (`0x102e` → `0x0b`, src-head match), so whatever the game parses
  here it parses from the same bytes a console reads off the same
  disc image.
- Load-bearing logic the worker skipped: the game reads THE SAME
  FILE on console. File bytes are file bytes unless the OS
  transforms them, and no evidence names such a transform on this
  path (the only HLE touch, ReadFile, is byte-correct).

**Standing:** encryption hypothesis DEAD for the boot path. No key
material is owed; no decrypt transform is owed. The 07–38 chain
remains exactly what it was: honest-negative on HLE surface, with
the primer (boot order / thread priming) still unknown. The
original "terminal with precondition" text is preserved below
struck through the record — do not act on it.

## Original (superseded) claim — kept for the record, DO NOT ACT ON IT

> **(superseded)** XCL feeds ciphertext where the console feeds
> plaintext ... precondition: key material + host-side decrypt at
> `vfs_hooks.rs:603` / `dstorage.rs:835` ...

Superseded because the evidence above disproves the premise. The
slot-in-layer analysis (`hk_read_file` post-read transform) may
still be useful IF a transform is ever evidenced — none is.

## What stays valid from the investigation

- Entropy numbers themselves (appdata0 header 3.437 vs payload
  7.952; script.rpf header 1.649 with zero runs) — consistent with
  compressed content + plaintext headers, which is also what the
  game consumes (opaque/compressed record, game-internal parse).
- `appdata0.rpf` is only 8880 bytes; all claims about its "payload
  windows" concern ~6 KiB past the header.
- Codebase crypto surface: none for RPF content (correct — none
  needed on this path); `bcpack`/inflate handle compression.
- The open question is unchanged since step-38: what primes the
  generation globals / why no `cl==1/4` sibling leaf precedes the
  `cl==5` call on this boot — a boot-order question, not a
  byte-fidelity question.

## Evidence

- `appdata0.rpf`: header window 0–0x1000 entropy **3.437** (structured);
  payload 0x1000–0x2000 **7.952**; sub-window 0x40+0xC0 6.939, 0 zeros.
- `common.rpf` header 7.95 / payload 7.96; `script.rpf` header 1.649
  (plaintext, 0x400+0x400 zeros), nested header 2.97, payload 7.932.
  Same `9FPR`+3×u32 magic shape in all; payload windows fail raw-zlib
  inflate; ascii ~38–40%, ~250/256 distinct bytes.
- Cross-file identical prefix: 0x40+192B byte-identical across
  appdata0/common/script (different sizes/content) — same-ciphertext
  prefix, not per-file compressed data.
- Codebase already agrees: `gpu/pack.rs:3` ("RPF9 packs encrypt their
  TOC"), `rpf_probe.rs:100-110,178-189` (`TocEvidence::Encrypted` =
  ≥2 headers sharing nonzero TOC prefix @0x40..0x60),
  `HANDOFF.md:160` (nested RPF9s "encrypted TOC (fixed key from the
  game... probably Oodle)"), `docs/specs/2026-11-20-xcl-design.md:33`
  (decrypting packages out-of-scope non-goal).
- Live cross-check: game-visible blob byte `[blob+0x2e]=0x0b` ==
  file offset `0x102e` byte `0x0b` (ReadFile backing byte-correct —
  correctly returning the ciphertext).

## Consequence for the 07–38 chain (REVISED)

Kind byte `0x0b` is a genuine file byte (same file ships on the disc
the console reads). Kind 5 happens on console too — so the console
must survive the `cl==5` path, which means the differentiator is NOT
byte fidelity but boot state XCL doesn't replicate (generation
priming / call order / an earlier init; see step-38 and the
generation-xref). Do not pre-populate the pool (still fake-success).

## Precondition (CANCELLED — was premised on the dead hypothesis)

~~(1) key material ... (2) host-side decrypt transform ...~~ Neither
is owed. No keys, no transform, no provisioning story needed for
this path.

## What stays valid

- Mount registrar, loader/TLS, ReadFile backing-offset accounting,
  generation-counter mechanics (all verified against true structure).
- M2 (synthetic title, plaintext test RPFs) is unaffected — green
  on Vulkan at this tip.
- Oodle/compression alone cannot explain the identical 192B prefix
  across files; compression handling (`bcpack_unpack`, inflate) is a
  separate layer *under* the decrypt, also unbuilt for runtime reads.

## Next (boot-order, not bytes)

Which factory inputs arrive per boot and in what order — `packroot:`
vs `packcache:` vs `dlc:`/others — and which one should prime the
generation counters first. `XCL_TRACE_STRNCMP` log comparison is the
cheapest evidence (no gdb). Only a boot-order divergence (a location
XCL rejects that the console serves, skipping the priming call) would
reopen an XCL slice here; gate re-probes stay closed.
