# Valence-inspired binary / geometry seams — local WIP

Pinned authority is **Minecraft 1.21.1 / protocol 767**, server SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. This is not whole-server acceptance.
[Donor provenance and license](../valence-reuse/README.md) identify the older
Valence revision; its registries, Bevy, JavaString type and infinite-ray formula
were not imported. No dependency was added or donor code executed.

## What the comparison establishes

`vanilla.json` is produced by [the authenticated oracle](../../../tools/primitives_oracle.py).
Default execution compares every request, result and provenance field. Recording
requires explicit `--output`; a wrong jar is rejected before extraction/Java and
cannot overwrite an existing output. The Java class uses normal constructors and
real methods, no Unsafe allocation, server startup, downloads or fake world.
Guava can emit a deprecated Unsafe warning internally; that is not constructor
bypass in this probe.

- **Section encoding:** actual `PalettedContainer.write` with the real BlockState
  ID map. The initial value is the first input; writes proceed in linear cell
  order, so first-seen palette order is controlled. Singleton, indirect thresholds,
  global registry IDs, non-straddling longs, high IDs and all three native air
  states are covered. The non-air count calls actual `BlockState.isAir` per cell;
  it is not a whole `LevelChunkSection` serialization test. Biomes use a synthetic
  identity IdMapper of 64 entries with the actual SECTION_BIOMES strategy: this
  proves bit packing, not biome contents or registry synchronization. Rust compares
  length and SHA-1 of the complete encoded section, not only palette counts.
- **NBT strings:** actual `StringTag.write(DataOutput)` and JDK `readUTF` on the
  output. Legal-size Rust strings compare byte length and whole-output hash,
  including NUL, BMP, astral pairs and the length boundary. Java isolated-surrogate
  cases do not get coerced into Rust String: their recorded raw StringTag bytes
  are preserved through production lore append and component persistence. This is
  not an arbitrary NBT parser or structure-template implementation.
- **AABB clip:** actual `AABB.clip(Vec3,Vec3)` returns positions, compared as raw
  f64 bits. Positive/negative axes, zero/tiny direction, open endpoints, starting
  inside, corner contact, epsilon boundaries and deterministic varied segments
  are exercised. A production tick regression proves the continuous path reaches
  a target between the retired sample positions; a separate point-blank
  regression retains the existing start-inside contact policy.

The corpus currently has **29 section / 12 string / 99 clip cases**. Nine string
cases are valid Rust Unicode; three isolated-surrogate cases are deliberately
opaque. The corpus guard checks exact generated input membership, not merely these
counts. Native decode/write success does not imply full world or network parity.

## Concrete failures and ownership

The old 257-state section emitted **5,084 bytes**, versus native **8,200**: it
continued writing a local palette above eight bits. The existing section encoder
now uses one palette writer for blocks and biomes, switching to target-registry
IDs and widths at the appropriate boundary. State IDs and cardinalities belong to
protocol 767, not the donor. No speedup is claimed.

The old NBT writer emitted an ordinary zero byte for NUL (**3 total bytes** rather
than native **4**). Lore append subsequently replaced modified-UTF-8 bytes with
lossy replacement characters. Both existing string writers now share modified
UTF-8 emission, and lore append keeps existing StringTag bytes opaque.

The old arrow tick left a synthetic high-speed target at **20 HP**, although the
finite segment crossed its box. Continuous intersection applies the existing
6-point damage and leaves **14 HP**. High velocity is a geometry discriminator,
not a claim that the port's integrator matches native physics. The old point-blank
and velocity-decay tests remain active; sample-specific assertions/mutations were
retargeted to the continuous production owner, not left testing dead helpers.

## Deliberate limits

- The oversized-text policy remains a bounded Unicode-scalar prefix. It is now
  measured in **modified-UTF-8 bytes** and cannot split an astral pair. Java
  writeUTF instead throws above 65,535 bytes; overflow-policy parity is **not**
  claimed. The custom save envelope/component-length limits are unchanged; the
  small opaque-byte round-trip is not a promise that arbitrarily large components
  fit the existing u16 component-payload field.
- Lore append still supports its existing bounded list of StringTag components;
  compound/list text components and general semantic patch equality are not added.
- Arrow target boxes, pose dimensions, contact policy, target iteration order,
  block occlusion/voxel collision, piercing, damage, movement and tracking are not
  established by AABB primitive parity. Block collision still checks the end cell.
  This change must not be described as a complete projectile/raycast engine.
- Finite numeric inputs are the geometry contract exercised here. NaN/Infinity
  construction or arbitrary invalid registries are not tested.

## Gates and current result

The Rust primitive tests consume the shared native fixture and exercise production
section/chat/item writers, loot append and arrow tick. Python guards check corpus
identity, result shapes, source/jar provenance, and fail-closed output behavior;
these run from `tests/verify.sh` and CI alongside the Rust suite. Java is needed
only to regenerate/recheck the native reference.

Final scoped results are bound to source, fixture, measured binary and unit
invocations in `run.json`:

- Native repeat: complete equality for all 140 cases. New Rust tests: six passed.
- Selected mutations: **9 primitive + 2 retargeted arrow gates proved**, with no
  skipped/parked/surviving mutant. The suite-summary parser has one additional
  proved mutation, with baseline and restored controls. Source hashes were
  restored and no mutation backup remained before building the measured binary.
- All-target clippy with warnings denied, format, corpus guards, layer gate,
  debt ratchet, and production container/gameplay wire regressions passed.
- Full serial release: **194 passed / 24 failed / 1 ignored**, versus 188/24/1.
  Failed-name sets are identical; 15 failures are mutex-poison cascades, so 24 is
  not an independent defect count. No old worldgen captures were re-recorded.
- Independent static review used an isolated copy of the restored control source
  while mutations ran in the implementing worktree. It found no actionable
  regressions and did not execute/independently verify builds or native probes.

Full migration acceptance remains red. Full `tests/verify.sh`, the whole mutation
campaign, hosted CI, push, merge, mirror refresh and publication were **not run**.
This is a local WIP checkpoint, not canonical acceptance.

A validation-harness mistake was also reproduced: selecting the first Rust test
summary returned a child process's **1/0/0**, although the terminal suite result
was **194/24/1**. [The shared summary reader](../../../tools/suite_compare.py) now
requires the terminal summary and complete failed-name roster. Its four tests and
an observed first-summary mutation pin that class. An unchanged red baseline is
only attribution of failures, never a green acceptance signal.

Receipt collection also rejects a missing invocation-bound completion marker.
`systemd-run --pipe` delivered stdout to the caller rather than the journal;
the final checks therefore use journal stdout explicitly. Accounting and marker
are from the same invocation, not a reaped unit's default status properties.
`unit.jsonl` retains the earlier attempts as well as final accepted invocations;
`log-normalization.json` discloses trailing-whitespace/EOF-only normalization.

## Reproduce (admit/cap resources separately)

```sh
python3 tools/primitives_oracle.py
python3 tests/primitives_oracle_test.py
python3 tests/suite_compare_test.py
cargo test --release --offline --locked -j 1 primitives_ -- --test-threads=1
MUTATION_FILTER=primitives_ MUTATION_RELEASE=1 CARGO_BUILD_JOBS=1 \
  bash tools/mutation_check.sh --offline
```
