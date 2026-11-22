# P0-2 Reachability Audit — 2026-11-22 (SpawnCosts / NaturalSpawner)

## 対象と判定

- Worktree baseline HEAD = `fc93114a7b5c2cbf07a21463009b2f1edf4e9845` (Lane SPAWN-P02 NaturalSpawner category/ocelot work applied on top of it; see `git diff`).
- Verified current `main` at capture time = `e08d21e16f61ea9262e5c93505dd27be331de0f5`. `HEAD` is an ancestor of `main`; all 41 differing paths are outside audited spawn source/datapack files, confirmed by `git diff --name-only HEAD main`.
- Data source `datapacks/vanilla/` (pinned Minecraft 1.21.1, protocol 767), 64 biome JSON files, 63 dimension-tree biome references, plus the unreferenced `minecraft:the_void`.
- Static datapack/entity/parser/live-tick **source reachability** audit. This does not constitute executed Minecraft server behavioral parity.
- **P0-2 remains OPEN**. Full command exits **1** because the audit still reports 13 records (down from 21). The audit is fail-closed.
- Density, surface, biome mask and feature/placement portions remain free of reported failures.

## Spawn reachability metrics

| Check | Result |
| --- | ---: |
| Entity registry entries | 130 |
| Biome spawner rows | 746 |
| Structurally valid spawner rows | 746 |
| Reachable per-dimension weighted uses | 600 |
| Unreachable per-dimension weighted uses | 146 |
| Spawn cost entries (energy_budget + charge) | 5 |
| Spawn cost entries consumed through Nether runtime path | 5 |
| Invalid spawn cost entries | 0 |
| Grouped unreachable dispatch records | 11 |
| Total audit failure records | 13 |

Category row counts across biome JSONs are `ambient`=52, `axolotls`=1, `creature`=157, `monster`=448, `underground_water_creature`=52, `water_ambient`=20, `water_creature`=16.

The five spawn costs originate from `minecraft:soul_sand_valley` (4) and `minecraft:warped_forest` (1). Their entity mappings and `SpawnCostState`/`PotentialCalculator`/`NaturalSpawner` consumption anchors are present.

### Changes since the previous snapshot (375 reachable / 21 failures)

`src/natural_spawner.rs` gained the Vanilla 1.21.1 Overworld dispatch (`OverworldSpawnMob`, `OverworldSpawnQueries`/`OverworldSpawnDraws`, `OverworldSpawnRules`, `slime_chunk_seed`, `halloween_from_epoch_days`) and the nightly hostile pass now dispatches all 13 monster-table kinds (`bogged`, `creeper`, `drowned`, `enderman`, `husk`, `ocelot`, `skeleton`, `slime`, `spider`, `stray`, `witch`, `zombie`, `zombie_villager`) through per-mob `SpawnPlacements` placements and `check*SpawnRules` predicates decompiled from the pinned server jar. That closed every `overworld` `monster` gap: bogged (2), drowned (12), enderman (51), husk (1), ocelot (2), slime (51), stray (2), witch (51), zombie_villager (51) — 223 uses.

### Remaining records

- 2 `minecraft:ocelot` records in the `monster` category differ from its `Creature` registry category. The audit records this Vanilla data inconsistency unconditionally; the jungle/bamboo-jungle entries themselves now dispatch and count as reachable. The registry correctly models `EntityType.OCELOT` as `Creature`.
- 11 grouped unreachable records (146 uses) for `ambient`, `axolotls`, `underground_water_creature`, `water_ambient`, `water_creature` in the Overworld and `monster` enderman in the End. `tools/p0_2_audit.py`'s `supported` predicate only credits `nether` dispatch, Overworld `creature`, and Overworld `monster` entries named in the nightly `matches!` arm — these categories have no crediting branch, so marking them reachable needs an audit-side extension plus thin `main.rs` call sites into the new `spawn_natural_category_scratch` (per-mob rules, placements and category dispatch are already implemented and unit-tested in `src/natural_spawner.rs`).

## Verbatim `python3 tools/p0_2_audit.py`

Exit code `1`. Standard output recorded without filtering.

```text
== P0-2 static & datapack reachability audit ==
parsers: configured_feature=63, df=43, placement_modifier=18, surface_cond=12, surface_rule=5
[overworld] bleaf=53/128 df_types=27 (zero_folded=['blend_alpha', 'blend_offset'], unsupported=[]) surface=(4 rules, 11 conds, biome_is=27, missing_bleaf=[])
        pf_reachable=176 pm_types=14 (unsupported=[]) cf_reachable=143 unimpl_cf=[]
[nether] bleaf=5/128 df_types=12 (zero_folded=[], unsupported=[]) surface=(3 rules, 7 conds, biome_is=5, missing_bleaf=[])
        pf_reachable=37 pm_types=8 (unsupported=[]) cf_reachable=31 unimpl_cf=[]
[end] bleaf=5/128 df_types=9 (zero_folded=[], unsupported=[]) surface=(1 rules, 0 conds, biome_is=0, missing_bleaf=[])
        pf_reachable=5 pm_types=8 (unsupported=[]) cf_reachable=5 unimpl_cf=[]
[biomes] total=64 carvers=58 spawners=62 spawn_costs=2 biomes
[structures] sets=20 definitions=34 types=16 (P2-1 gap; e.g. minecraft:mineshaft=['mineshaft', 'mineshaft_mesa'])
[spawn_reachability] registry=130 spawners=746 valid=746 reachable_uses=600 unreachable_uses=146 spawn_costs=5 reachable_cost_uses=5
FAILURES:
  - minecraft:bamboo_jungle/monster[8]: minecraft:ocelot registry category Creature differs from biome Monster
  - minecraft:jungle/monster[8]: minecraft:ocelot registry category Creature differs from biome Monster
  - end: unreachable weighted monster minecraft:enderman in 5 biome(s): minecraft:end_barrens, minecraft:end_highlands, minecraft:end_midlands
  - overworld: unreachable weighted ambient minecraft:bat in 52 biome(s): minecraft:badlands, minecraft:bamboo_jungle, minecraft:beach
  - overworld: unreachable weighted axolotls minecraft:axolotl in 1 biome(s): minecraft:lush_caves
  - overworld: unreachable weighted underground_water_creature minecraft:glow_squid in 52 biome(s): minecraft:badlands, minecraft:bamboo_jungle, minecraft:beach
  - overworld: unreachable weighted water_ambient minecraft:cod in 6 biome(s): minecraft:cold_ocean, minecraft:deep_cold_ocean, minecraft:deep_lukewarm_ocean
  - overworld: unreachable weighted water_ambient minecraft:pufferfish in 3 biome(s): minecraft:deep_lukewarm_ocean, minecraft:lukewarm_ocean, minecraft:warm_ocean
  - overworld: unreachable weighted water_ambient minecraft:salmon in 6 biome(s): minecraft:cold_ocean, minecraft:deep_cold_ocean, minecraft:deep_frozen_ocean
  - overworld: unreachable weighted water_ambient minecraft:tropical_fish in 5 biome(s): minecraft:deep_lukewarm_ocean, minecraft:lukewarm_ocean, minecraft:lush_caves
  - overworld: unreachable weighted water_creature minecraft:dolphin in 5 biome(s): minecraft:deep_lukewarm_ocean, minecraft:deep_ocean, minecraft:lukewarm_ocean
  - overworld: unreachable weighted water_creature minecraft:squid in 11 biome(s): minecraft:cold_ocean, minecraft:deep_cold_ocean, minecraft:deep_frozen_ocean
```

## Verification

`python3 tests/p0_2_audit_test.py` — exit `0` (unit-test runner output)

```text
..........
----------------------------------------------------------------------
Ran 10 tests in 0.558s

OK
```

`cargo test --offline --locked natural_spawner` — 30 passed, 0 failed.

`cargo test --offline --locked nether_spawn` — 11 passed, 0 failed.

`python3 tests/gp2_nether_spawn_wire.py` — `GP-2 Nether wire PASS: 0 natural spawns with 24 charges, 7 after removal`.

`cargo clippy --offline --locked --all-targets` — 0 warnings in `src/natural_spawner.rs` and `src/nether_spawn_rules.rs`.

`bash tools/debt_ratchet.sh` — `debt ratchet: 0 over limit`.

`python3 tools/provenance_check.py` — exit `0`

```text
provenance: clean - none of assets/pblocks.json, assets/properties.json, resourcepack.zip is tracked or referenced
```

`git diff --check` — exit `0` (no diagnostic output).

The real Nether biome subset passes its spawn-entry and cost reachability test. Injecting an unknown cost entity, a registered but unimplemented Nether mob, an unreferenced biome entry, malformed fields, or a broken `environment_time_tick` anchor is detected. The 11 remaining unreachable records require an audit-model crediting branch and `main.rs` call sites outside Lane SPAWN-P02's file ownership; the per-category/per-mob runtime machinery they gate is implemented and unit-tested in `src/natural_spawner.rs`.
