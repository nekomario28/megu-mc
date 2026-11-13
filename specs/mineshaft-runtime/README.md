# Mineshaft runtime gameplay

Continue the generated-content [ownership contract](../mineshaft-dispatch/README.md):
make its chest carts move, make their delayed loot inventory usable, and tick cave-spider
spawners. Keep the current generated UUID/NBT/source-claim persistence. No publication,
Codex review, generic vehicle framework, or save-format migration is authorized here.

## Next Agent Prompt

All three runtime slices in this spec are implemented and wired into `src/main.rs` on branch
`perf/worldgen-parallel` (uncommitted on top of `94f3ca6`):
- Deferred mineshaft chest-cart loot (`src/chest_loot.rs`, `tests/data/mineshaft_loot_1211.tsv`)
  evaluated on first access and owned via `ContainerKey::{Block, Cart}` in `CHESTS`, reusing
  existing window/click transactions and survival break spill/cleanup.
- Cave-spider spawner tick (`src/spawner.rs`, `tests/data/mineshaft_spawner_1211.tsv`) wired into
  `spawner_tick()` / `game_tick()`, preserving mutable `generated_content().spawners` NBT,
  light/obstruction/cap rules, world-event `2004` particles, and block-break cleanup.
- Chest-minecart motion (`src/minecart.rs`, `tests/data/mineshaft_motion_1211.tsv`) wired into
  `cart_tick()` / `game_tick()`, with block/AABB collision, comparator-weighted rail drag,
  natural off-rail slowdown, `0x70`/`0x5a` tracker updates, and `Pos`/`Motion`/`OnGround` save-reload.

Verified on the current working tree (without modifying `roll_loot` or the caller-held `SAVE_LOCK`
snapshot contract):
- Default serial suite (`cargo test --locked -- --test-threads=1`): `435 passed; 0 failed; 1 ignored`
  (includes `chest_loot::tests::native_abandoned_mineshaft_loot_matches_all_19_seeds`,
  `spawner::tests::*`, `minecart::tests::*`, and all 3 `mineshaft_runtime_tests::*` packet/runtime tests).
- 8-thread parallel suite (`cargo test --offline --locked -j 2 -- --test-threads=8`, 1 run):
  `435 passed; 0 failed; 1 ignored`.
- Wire smoke (`wire_smoke.py`, `client_settings_wire.py`, `gameplay_wire.py`, `container_wire.py`,
  `population_plan_test.py`, `mineshaft_dispatch_test.py`): all `PASS`.
- Feature builds (`--all-targets --all-features`, `--features fabric-probe`, `--features neoforge-events`): pass.
- Module layering & debt: `tests/layers_gate_test.py` (`17 tests ... OK`), `tools/layers_check.sh`
  (`42 modules, 108 edges, 0 named errors`), `tools/debt_ratchet.sh` (`0 over limit`).
- Mutation checks (`tools/mutation_check.sh`): `tests/gate_scripts.py` (`15/15 PASS`, proving all
  316 anchors uniquely exist in `src/*.rs`), focused runs proved `9/9` new `mineshaft_runtime_tests`
  mutants, `2/2` `a_broken_chest_leaves_no_orphan_entry_across_a_save_and_load` mutants, and `5/5`
  updated `template_*` mutants (`0 skipped`, `0 parked`).

Not yet run on this working tree: full `tests/verify.sh` end-to-end (including all standalone
`tests/*_oracle_test.py` scripts, 3x parallel repeat, and unfiltered 316-mutant `mutation_check.sh`),
commit/push, or Codex review. Do not claim M2/M3 or whole-world parity from these slices.

- [x] Pinned native loot, minecart and spawner runtime observations.
- [x] One container identity/store: reuse current window/click transactions for carts;
      delayed seeded loot is evaluated only on access and persisted as items thereafter.
- [x] Spawner tick: activation, delay, attempted positions, rule/obstruction checks,
      nearby cap, entity insertion and reset; preserve the mutable NBT owner.
- [x] Chest-cart motion: rail/off-rail movement and collision, natural slowdown,
      position/motion save-resume and tracker updates.
- [x] Packet-path gameplay regressions, compiling mutants and closeout checks.

## Owners and reuse decisions

- ADOPT existing item Stack/component wire format, slot operations, click/drag/shift
  handling, chunk/interest tracking, world reads, hot-region contract and Mob owner.
- ADAPT the block-container key into a typed block/entity identity instead of inventing
  virtual coordinates or a second inventory/menu implementation. A cart's inventory
  has one runtime owner; NBT is the save handoff, not a shadow live inventory.
- REFERENCE_ONLY the authenticated native server. Keep jar/decompiled/runtime world
  artifacts local; production must never execute Java or read captured outcomes.
- HOLD the existing unseeded `roll_loot`: inspection shows its sequence-based draws,
  equal-choice entries and simplified functions do not implement seeded native chest
  table fill. Do not silently change mob loot to make cart loot work.
- Existing collision support supplies full-shape predicates but not a general swept
  voxel mover. Inspect native movement and collision consumers before adding a rule.

Each slice needs a native controlled case and a Rust production-entry-point regression.
State precisely which world/rule/packet inputs each fixture supplies. Match RNG source,
consumption and conditional draw order; no fitting to a few seeds or rounding by guess.
The old raw snapshot builder's caller-held SAVE_LOCK contract must remain intact.
Block reads must not acquire SAVE_LOCK while holding entity/player locks.

A native trajectory/delay trace is not proof of a playable Rust route. Exercise the
actual tick/interaction/window packet dispatch and saved restart state. Native entity
creation failure or an unserved native method invalidates that case, not its rule.
Do not call the full promotion or whole-world parity complete from these slices.
