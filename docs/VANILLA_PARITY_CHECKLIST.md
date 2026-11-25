<!-- SPDX-License-Identifier: LGPL-3.0-or-later -->
# Vanilla 1.21.1 Survival Parity Checklist

Automated counterpart and **authoritative machine check**:
`python3 tools/strict_vanilla_parity_audit.py --strict` — it exits non-zero
when any gate FAILs and is the form wired into `tools/run_fast_gates.py`
(`--json` emits the `megu-strict-vanilla-parity/1` report). The audit reads
only live (non-`#[cfg(test)]`) Rust source through the comment/string-aware
lexer in `tools/vanilla_parity_audit.py`, so a commented-out handler or a
test-only helper never counts as wired. The docs-to-code sync guard
`python3 tools/docs_sync_audit.py --strict` cross-checks that every gate ID
below is documented here and that no stale scorecard fraction is
reintroduced.

Each row is one CI gate plus the manual spot-check that proves the same
thing in-game on a vanilla client. The **status** column separates three
claims:

- **implemented** — vanilla-faithful logic exists in the tree (constants,
  tables, handlers);
- **wired** — a live call path exists and `--strict` proves it;
- **differentially verified** — behaviour was additionally compared against
  the Java 1.21.1 reference (a clean-room probe under `tools/structure-oracle/`
  or an `evidence/` oracle vector). Rows without that label are verified by
  the static wiring proof plus the manual spot-check only.

| # | Gate | Vanilla 1.21.1 expectation | Status | Manual verification | Automated check (what FAILs the gate) |
|---|------|----------------------------|--------|---------------------|----------------------------------------|
| 1 | `gate1_fishing` | Fishing reel honors Lure (faster bite), Luck of the Sea (treasure weighting), the 5×4×5 open-water check for treasure eligibility, and rolls `minecraft:gameplay/fishing` even with no datapack loaded. | implemented, wired | Fish with a Lure III + Luck III rod: bites come faster, treasure items appear (name tags, saddles, enchanted bows) only in open water. | Reel handler lacks `fishing_luck`/`open_water` wiring, lacks a built-in `chest_loot` fallback, or still carries the 2-fish `(age % 5)` stub. |
| 2 | `gate2_enchanting` | Enchanting table + anvil offer and combine all 42 1.21.1 enchantments, including Mace-exclusive `density`, `breach`, `wind_burst`; costs and conflicts follow vanilla. | implemented, wired | Enchant a mace at 30 levels: density/breach/wind_burst appear; mutually exclusive enchantments are rejected by the anvil. | `enchant_id` map misses a 1.21.1 name; `enchantment_cost`/`roll_table_enchantments` have no live call site; no conflict rules; `LiveMenu::Enchanting`/`Anvil` unwired. |
| 3 | `gate3_structure_loot` | Desert pyramids fill 4 chests, brushable suspicious sand and a TNT trap; jungle temples fill chests and arrow dispensers; dungeons, mineshaft chest carts and trial chambers (`trial_spawner`, `vault`) all produce seeded loot. | implemented, wired; piece placement differentially verified (`tools/structure-oracle/SinglePieceOracle.java`) | Open a freshly generated pyramid/jungle chest with no datapacks: slots contain vanilla-table loot, dispensers hold arrows, TNT sits under the terracotta. | A structure's loot-table id never appears outside `chest_loot.rs`; no `tnt` near desert-pyramid code; `tick_live_trial_spawner`/vault dead; `roll_loot_container_slots` has no built-in fallback. |
| 4 | `gate4_dimensions` | Overworld/Nether→End portals materialize the (100, 48, 0) obsidian platform; End→Overworld returns to the player spawn; end gateways warp; dragon death opens the bedrock exit portal; chorus fruit teleports search the *current* dimension's ground. | implemented, wired; platform & gateway blocks differentially verified (`tools/structure-oracle/EndPlatformOracle.java`, `EndGatewayOracle.java`) | Eat chorus fruit in the Nether/End: landing Y follows that dimension's terrain, not the Overworld surface. Kill the dragon: exit portal + egg + XP spawn. | No `(100+dx, 48, dz)` obsidian platform; `enter_portal` missing End↔Overworld routing; `end_gateway_warp`/exit-portal placement dead; chorus arm uses bare `surface_y(`/`block_at(` with no `*_dim` lookup. |
| 5 | `gate5_spawning` | `is_world_gen()` is true on the live adapter so slime chunks evaluate; natural spawn passes run in Overworld, Nether and End. | implemented, wired | Stand in a slime chunk at y<40: slimes spawn. Piglins/hoglins spawn in the Nether, endermen in the End. | `LiveNaturalSpawnQueries::is_world_gen` body is a bare `false`; any of the three dimension spawn passes has no live call site. |
| 6 | `gate6_disconnect` | `player_info_remove` (S2C `0x3d`) is broadcast when a player disconnects so no ghost stays in the tab list. | implemented, wired | Second player disconnects: their tab-list entry vanishes for everyone remaining. | The disconnect-cleanup function never emits `0x3d`/`player_info_remove`. |
| 7 | `gate7_cramming` | `maxEntityCramming` defaults to 24, cramming suffocation is 6.0 damage, and the post-hurt invulnerability window is 10 ticks. | implemented, wired | Cram 25 mobs in a 1×1: they start taking 6.0 suffocation hits every 10 ticks. | `ENTITY_CRAMMING_DEFAULT != 24`; `GR_MAX_ENTITY_CRAMMING` unwired or never `.load`ed; damage ≠ 6.0; `ENTITY_HURT_INVULNERABLE_TICKS != 10` or unused. |
| 8 | `gate8_survival` | Brewing, villager POI/trading/restock, raids, beds + respawn anchors, shields, bows/crossbows, tridents, elytra/fireworks, redstone/pistons/hoppers/comparators, and weather/lightning are all live. | implemented, wired; block `canSurvive` matrix differentially verified (`tools/structure-oracle/NativeSurvivalOracle.java` → `tests/native_survival_oracle_test.py`) | Exercise each end-to-end on a vanilla client (brew a potion, trade, trigger a raid, set spawn on a charged anchor, block with a shield, boost elytra with rockets, run a clock, watch rain). | Any mechanism's live wiring symbol is missing — presence in a `#[cfg(test)]` region does not count. |
| 9 | `gate9_builtin_bypass` | Every built-in `VANILLA_LOOT_GENERATORS` table is reachable from live consumers: container fills fall back to `chest_loot` when no datapack table exists, and so does the fishing catch. | implemented, wired | Delete all datapacks and open structure chests / fish: vanilla-table loot still flows. | `roll_loot_container_slots` or the fishing reel cannot reach `chest_loot`/`vanilla_loot_roll` — built-ins silently bypassed unless a datapack shadows them. |
| 10 | `gate10_dimension_leak` | Dimension-aware handlers (`enter_portal`, `chorus_fruit`, `end_gateway_warp`, `end_gateway_entry_check`, `live_mob_tick_dimension`) never call Overworld-only `surface_y(`/`block_at(` unguarded. | implemented, wired | Code review or the audit itself; the in-game tell is weird teleport/spawn heights in the Nether/End. | A bare `surface_y(`/`block_at(` inside a handler with no `dim == DimId::Overworld` guard on the same statement (`if`/`&&` chain, ternary arm, or match arm). |
| 11 | `gate11_branch_mining` | The 0x24 dig packet runs the canonical break-progress model (never `req == Some(0.0)`), dig/place reach uses the pose-aware eye height (never `+ 1.62`), rejected digs resync the authoritative block before the 0x05 ack, and the mining helpers stay live-wired. | implemented, wired; deterministic break-progress matrix across tool families (`tests/compat_differential_matrix.rs`) | Mine a fast block while sneaking/swimming and re-dig after a rejected insta-mine: progress matches the tool/material curve, reach follows the pose, and no ghost blocks appear. | 0x24 regressed to `req == Some(0.0)`; no call to the canonical break model; hardcoded `+ 1.62` eye height; resync missing before `ack(seq)`; mining helpers dead or back under `allow(dead_code)`. |

## Current scorecard (origin/main, at time of writing)

`python3 tools/strict_vanilla_parity_audit.py --strict` reports **11/11 PASS**
(0 warn, 0 fail):

- PASS: `gate1_fishing`, `gate2_enchanting`, `gate3_structure_loot`,
  `gate4_dimensions`, `gate5_spawning`, `gate6_disconnect`, `gate7_cramming`,
  `gate8_survival`, `gate9_builtin_bypass`, `gate10_dimension_leak`,
  `gate11_branch_mining`.

Any FAIL is a regression: `--strict` is the gate in
`tools/run_fast_gates.py`, and `tools/docs_sync_audit.py --strict` keeps
this document's gate list and scorecard in sync with the audit tool itself.
