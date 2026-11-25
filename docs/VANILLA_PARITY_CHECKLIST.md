<!-- SPDX-License-Identifier: LGPL-3.0-or-later -->
# Vanilla 1.21.1 Survival Parity Checklist

Automated counterpart: `python3 tools/strict_vanilla_parity_audit.py`
(`--json` for the `megu-strict-vanilla-parity/1` report, `--strict` for a
non-zero exit on any FAIL — the intended `ci.yml` / `run_fast_gates.py`
wiring once the `alpha4/fix-*` branches land). The audit reads only live
(non-`#[cfg(test)]`) Rust source through the comment/string-aware lexer in
`tools/vanilla_parity_audit.py`, so a commented-out handler or a test-only
helper never counts as wired.

Each row is one CI gate plus the manual spot-check that proves the same thing
in-game on a vanilla client.

| # | Gate | Vanilla 1.21.1 expectation | Manual verification | Automated check (what FAILs the gate) |
|---|------|----------------------------|---------------------|----------------------------------------|
| 1 | `gate1_fishing` | Fishing reel honors Lure (faster bite), Luck of the Sea (treasure weighting), the 5×4×5 open-water check for treasure eligibility, and rolls `minecraft:gameplay/fishing` even with no datapack loaded. | Fish with a Lure III + Luck III rod: bites come faster, treasure items appear (name tags, saddles, enchanted bows) only in open water. | Reel handler lacks `fishing_luck`/`open_water` wiring, lacks a built-in `chest_loot` fallback, or still carries the 2-fish `(age % 5)` stub. |
| 2 | `gate2_enchanting` | Enchanting table + anvil offer and combine all 42 1.21.1 enchantments, including Mace-exclusive `density`, `breach`, `wind_burst`; costs and conflicts follow vanilla. | Enchant a mace at 30 levels: density/breach/wind_burst appear; mutually exclusive enchantments are rejected by the anvil. | `enchant_id` map misses a 1.21.1 name; `enchantment_cost`/`roll_table_enchantments` have no live call site; no conflict rules; `LiveMenu::Enchanting`/`Anvil` unwired. |
| 3 | `gate3_structure_loot` | Desert pyramids fill 4 chests, brushable suspicious sand and a TNT trap; jungle temples fill chests and arrow dispensers; dungeons, mineshaft chest carts and trial chambers (`trial_spawner`, `vault`) all produce seeded loot. | Open a freshly generated pyramid/jungle chest with no datapacks: slots contain vanilla-table loot, dispensers hold arrows, TNT sits under the terracotta. | A structure's loot-table id never appears outside `chest_loot.rs`; no `tnt` near desert-pyramid code; `tick_live_trial_spawner`/vault dead; `roll_loot_container_slots` has no built-in fallback. |
| 4 | `gate4_dimensions` | Overworld/Nether→End portals materialize the (100, 48, 0) obsidian platform; End→Overworld returns to the player spawn; end gateways warp; dragon death opens the bedrock exit portal; chorus fruit teleports search the *current* dimension's ground. | Eat chorus fruit in the Nether/End: landing Y follows that dimension's terrain, not the Overworld surface. Kill the dragon: exit portal + egg + XP spawn. | No `(100+dx, 48, dz)` obsidian platform; `enter_portal` missing End↔Overworld routing; `end_gateway_warp`/exit-portal placement dead; chorus arm uses bare `surface_y(`/`block_at(` with no `*_dim` lookup. |
| 5 | `gate5_spawning` | `is_world_gen()` is true on the live adapter so slime chunks evaluate; natural spawn passes run in Overworld, Nether and End. | Stand in a slime chunk at y<40: slimes spawn. Piglins/hoglins spawn in the Nether, endermen in the End. | `LiveNaturalSpawnQueries::is_world_gen` body is a bare `false`; any of the three dimension spawn passes has no live call site. |
| 6 | `gate6_disconnect` | `player_info_remove` (S2C `0x3d`) is broadcast when a player disconnects so no ghost stays in the tab list. | Second player disconnects: their tab-list entry vanishes for everyone remaining. | The disconnect-cleanup function never emits `0x3d`/`player_info_remove`. |
| 7 | `gate7_cramming` | `maxEntityCramming` defaults to 24, cramming suffocation is 6.0 damage, and the post-hurt invulnerability window is 10 ticks. | Cram 25 mobs in a 1×1: they start taking 6.0 suffocation hits every 10 ticks. | `ENTITY_CRAMMING_DEFAULT != 24`; `GR_MAX_ENTITY_CRAMMING` unwired or never `.load`ed; damage ≠ 6.0; `ENTITY_HURT_INVULNERABLE_TICKS != 10` or unused. |
| 8 | `gate8_survival` | Brewing, villager POI/trading/restock, raids, beds + respawn anchors, shields, bows/crossbows, tridents, elytra/fireworks, redstone/pistons/hoppers/comparators, and weather/lightning are all live. | Exercise each end-to-end on a vanilla client (brew a potion, trade, trigger a raid, set spawn on a charged anchor, block with a shield, boost elytra with rockets, run a clock, watch rain). | Any mechanism's live wiring symbol is missing — presence in a `#[cfg(test)]` region does not count. |
| 9 | `gate9_builtin_bypass` | Every built-in `VANILLA_LOOT_GENERATORS` table is reachable from live consumers: container fills fall back to `chest_loot` when no datapack table exists, and so does the fishing catch. | Delete all datapacks and open structure chests / fish: vanilla-table loot still flows. | `roll_loot_container_slots` or the fishing reel cannot reach `chest_loot`/`vanilla_loot_roll` — built-ins silently bypassed unless a datapack shadows them. |
| 10 | `gate10_dimension_leak` | Dimension-aware handlers (`enter_portal`, `chorus_fruit`, `end_gateway_warp`, `end_gateway_entry_check`, `live_mob_tick_dimension`) never call Overworld-only `surface_y(`/`block_at(` unguarded. | Code review or the audit itself; the in-game tell is weird teleport/spawn heights in the Nether/End. | A bare `surface_y(`/`block_at(` inside a handler with no `dim == DimId::Overworld` guard on the same statement (`if`/`&&` chain, ternary arm, or match arm). |

## Current scorecard (origin/main, at time of writing)

`python3 tools/strict_vanilla_parity_audit.py` reports **5/10 PASS**:

- PASS: enchanting (2), spawning (5), disconnect (6), cramming (7), survival matrix (8).
- FAIL: fishing reel ignores `fishing_luck`/open-water and keeps the `(age % 5)` stub (1); `roll_loot_container_slots` only routes `abandoned_mineshaft` to a built-in table — 60 chest/archaeology tables plus `gameplay/fishing` are unreachable without a datapack (3, 9); dungeon chests consume the loot seed but stage no `LootContainer` payload (3); chorus fruit teleports resolve `surface_y` against the Overworld (4, 10).

The `--strict` leg should be enabled in CI after the `alpha4/fix-*` branches
turn this scorecard green; until then the default (exit 0) scorecard is the
parity dashboard.
