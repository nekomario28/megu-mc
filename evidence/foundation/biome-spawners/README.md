# Pinned biome spawn tables and the nightly hostile path

Target authority is Minecraft 1.21.1 / protocol 767, server SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. No server/world, dependency or donor
runtime is introduced.

## Why this slice

The server had no biome spawner loader. Its nightly cycle rotated through four
hard-coded hostile entity ids regardless of biome. The own-jar biome JSON carries
`spawners` (eight MobCategories of weighted entries), `spawn_costs`, and an
optional `creature_spawn_probability`; none of it was read.

`feature::FReg` now owns all three tables for every biome. The parser preserves
the JSON category name, entry order, entity type, min/max group size, weight,
cost `energy_budget`/`charge`, and the probability (default `0.1` when omitted).
The native reference is `vanilla.json`, produced by
`tools/biome_spawn_oracle.py` through the actual `Biome.getMobSettings()` and
`MobSpawnSettings` methods after `RegistryDataLoader` reads the jar's own data.

## Production connection

The nightly hostile spawn now asks the biome at the candidate surface position
and uses `feature::choose_supported_hostile`:

- only the four hostile types this runtime actually simulates are candidates
  (`zombie` 124, `skeleton` 91, `spider` 100, `creeper` 23 in the existing
  entity-id table);
- selection uses the pinned `monster` entries' weights and order;
- if the biome has no supported entry (for example the End, whose only monster
  is an enderman), it keeps the previous rotation rather than inventing data.

This is deliberately **not** vanilla `NaturalSpawner`. Mob-specific placement,
group-size spawning, difficulty, category caps, persistent/despawn rules and
`spawn_costs` energy budgets are not applied. The connection removes the
biome-blind type choice; it does not claim spawn parity.

## Native/reference coverage

`vanilla.json` contains:

- **64 biomes**, all eight categories (including empty lists);
- **746 weighted spawner entries**;
- **5 spawn costs** (soul sand valley and warped forest);
- `creature_generation_probability` per biome (0.03/0.04/0.07/0.1).

`biome_spawn_tables_match_the_pinned_classes` compares every category, entry,
weight, cost bit and probability. The Java oracle is replayed twice; both runs
report `64 biomes, 746 spawner entries, 5 costs match`.

## Verification

| Gate | Result |
| --- | --- |
| Native spawner replay, twice | 64 biomes / 746 entries / 5 costs, exact |
| Focused Rust | `biome_spawn_tables...` and `nightly_hostile...` pass |
| Full serial release | **236 passed / 0 failed / 1 ignored**, exit 0 |
| New Rust tests | 2 (native tables, nightly biome choice); 234 → 236 |
| Selected mutations | 2 proved: spawner type parser, weighted roll; 0 not-a-gate/parked/skipped |
| fmt / all-target warnings-denied clippy | Passed |
| Layers / layer tests / debt | 20 modules, 47 edges, 0 named errors; 17 tests; 0 over limit |
| Wire | container and gameplay PASS |

The previous suite was 234/0/1; no failed names were added or removed.

## Limits and not run

- One source table, one seed-independent data load. The native oracle is data
  loading/provenance, not a running `NaturalSpawner` or mob lifecycle test.
- `spawn_costs` are parsed and pinned but not applied to a spawn budget; the
  current nightly loop has no energy accounting. Empty/unsupported biome lists
  deliberately fall back, so the production choice is narrower than vanilla.
- Full `tests/verify.sh`, whole mutation campaign, hosted CI, push, main merge,
  release, same-stage whole-world comparison, and performance were not run.
- The fixture's source jar is authenticated by `tools/vanilla_java.py`; no
  re-recorded world capture was used.

See [the closed rationale](../../../specs/done/biome-spawners/README.md) and
`run.json`.
