# M1 terminal: retail RPF payload is ciphertext (precondition: keyed decrypt)

Date: 2026-11-27. Tip: `linux` @ `cdb5d59f`. No code changed (read-only
investigation: entropy windows in place + codebase grep).

## Verdict

**XCL feeds ciphertext where the console feeds plaintext.** Everything
downstream of the first RPF payload read — volume-kind byte, pool
registration, the entire steps-07→38 NULL-record chain — is built on
untrusted bytes. All gate analysis stands as record of what the game
does with these bytes, but no gate verdict transfers to console
behavior until reads decrypt.

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

## Consequence for the 07–38 chain

Kind byte `0x0b` (→ kind 5 → default pool → zeroed `0x1479f7060` →
gate fail → NULL record → `exe+0x2237320`) is a ciphertext byte.
Console plaintext at that offset is unknown and may give kind 4
(the populate path with the real template at the first filler site).
Do not re-probe gates to explain the fault; do not pre-populate the
pool (fake-success — true contents unknown).

## Precondition (before any pool/gate work resumes)

1. **Key material (name only, never committed):** per-title RPF9
   content/TOC key embedded in the game exe (fixed key per HANDOFF)
   + its scope/IV. Retrieval is exe-analysis work on the user's own
   copy. Ground rule: keys never enter the repo, never ship; a
   user-provisioning story (env/file, `XCL_*`, absent = passthrough)
   is a project decision, still open.
2. **Host-side transform (code, keyless-safe):** console-transparent
   decrypt belongs host-side, not in game code. Correct slots:
   `vfs_hooks.rs:603` `hk_read_file` post-read buffer transform
   (the observed ReadFile backing path) + `hle/dstorage.rs:835`
   `read_exact_at` / `:905` post-read stage (DStorage path).
   `vfs.rs:1093` / `vfs_hooks.rs:237` are resolution-only — not slots.
   No transform exists today (`dstorage.rs` has bcpack/zlib unpack
   only; `rpf.rs` offline scan + miniz inflate only; no key/env
   plumbing anywhere).

## What stays valid

- Mount registrar, loader/TLS, ReadFile backing-offset accounting,
  generation-counter mechanics (all verified against true structure).
- M2 (synthetic title, plaintext test RPFs) is unaffected — green
  on Vulkan at this tip.
- Oodle/compression alone cannot explain the identical 192B prefix
  across files; compression handling (`bcpack_unpack`, inflate) is a
  separate layer *under* the decrypt, also unbuilt for runtime reads.

## Next when precondition clears

Re-run the retail control fresh-root with keyed reads; expect the
kind byte (and the whole downstream chain) to change shape. Only
then do gate verdicts become meaningful again.
