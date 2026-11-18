# ItemEntity playerTouch / target UUID / spectator exclusion (Slice 2b)

## 1.21.1 Bytecode Authority

Pinned server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) & official mappings (`03f8985492bda0afc0898465341eb0acef35f570`):

- **`Player.aiStep()` (`cmx.m_()`)**
  - Bytecodes 240–255: requires `getHealth() > 0.0f && !isSpectator()` (`cmx.ew() > 0.0f` and `!aqv.R_()` where `ServerPlayer.isSpectator` checks `GameType.SPECTATOR` / `gm == 3`).
  - Bytecodes 297–318: non-passenger entity touch box is `this.getBoundingBox().inflate(1.0, 0.5, 1.0)` (`cmx.cK().c(1.0, 0.5, 1.0)`).
- **Player & ItemEntity dimensions (`cmx.<clinit>`, `bsx.<clinit>`, `bsu.a(DDD)`)**
  - `EntityType.ITEM` (`bsx.ag`, `bsx.<clinit>` bytecode 2595–2637): `sized(0.25f, 0.25f)` -> `EntityDimensions.makeBoundingBox(x, y, z)` produces `[x - 0.125, x + 0.125] x [y, y + 0.25] x [z - 0.125, z + 0.125]`.
  - `Player` (`cmx.<clinit>`): `STANDING` (`bua.a`) `0.6f x 1.8f`, `CROUCHING` (`bua.f`) `0.6f x 1.5f`, `FALL_FLYING` / `SWIMMING` / `SPIN_ATTACK` (`bua.b`/`d`/`e`) `0.6f x 0.6f`. Inflated by `(1.0, 0.5, 1.0)` around `(px, py, pz)` via `f64::from(width / 2.0)` and `f64::from(height)`.
- **`AABB.intersects(AABB)` (`ewx.c(Lewx;)Z` -> `ewx.a(DDDDDD)Z`)**
  - Strict `<` / `>` comparisons on all 6 faces (`a.minX < b.maxX && a.maxX > b.minX && ...`).
- **`ItemEntity.playerTouch(Player)` (`cjh.b_(Lcmx;)V`)**
  - Bytecodes 27–63: requires `pickupDelay == 0` (`cjh.j == 0`), `this.target == null || this.target.equals(player.getUUID())` (`cjh.n`), and `player.getInventory().add(stack)`.
  - `thrower` (`cjh.l`) is NOT checked in `playerTouch`; it is only persisted under NBT `"Thrower"`, returned by `getOwner()`, and passed to `ServerPlayer.onItemPickup` (`aqv.a(cjh)`). `Player.drop(ItemStack, boolean, boolean)` (`cmx.a(cuq, Z, Z)`, bytecodes 67–76) sets `thrower` when `includeThrowerName` is true (`0x24` status 3/4, `0x0e` slot `-999` and mode `4`), and leaves it `None` on `AbstractContainerMenu.removed` (`0x0f`).
- **`ItemEntity.tryToMerge(ItemEntity)` (`cjh.a(Lcjh;)V`)**
  - Bytecodes 10–21: checks `Objects.equals(this.target, other.target)` before merging stacks (ignores `thrower`).

## Implementation Contract

- Single shared geometry & eligibility owner in `src/main.rs`:
  - `EntityAabb`, `player_touch_aabb`, `Client::is_spectator`, `Client::touch_aabb`, `Drop::bounding_box`, `Drop::can_be_picked_up_by_parts`, `Drop::can_be_picked_up_by`.
  - `drop_tick` applies the check in both the candidate scan and live-entity take loop (keeping `Drop::pickup_delay_elapsed()` as the single delay gate right before `let mut source = ...`).
  - `actor_tick` queries `Drop::can_be_picked_up_by(None, false, true, player_touch_aabb(ax, ay, az, false, false))` so untargeted drops share the 1.21.1 AABB and targeted drops are never absorbed by actors without a matching UUID.
  - `merge_drops` checks `dp[i].target == dp[j].target`.
  - `save_world_into` / `load_world_from` persist `target` and `thrower` UUIDs in an optional `DRO1` footer (`[body][body_len: u64][b"DRO1"]`) emitted only when at least one drop has `target.is_some() || thrower.is_some()`, preserving existing saves and `the_tick_preserves_the_saved_world_payload_byte_for_byte` (`54d762909d3321a41cfd670c7ff0baa9`) bit-for-bit.

## Verification

- `pickup_tick_excludes_spectator_and_dead_players`
- `pickup_tick_matches_1_21_1_player_touch_aabb_boundaries`
- `pickup_tick_enforces_target_uuid_over_closer_non_target`
- `dro1_round_trip_preserves_drop_target_and_thrower_uuids`
- `merge_drops_respects_target_uuid_but_ignores_thrower`
- 3 new `tools/mutation_check.sh` entries + all 6 prior `pickup_` entries proved (`9 proved, 0 not a gate`) under `MUTATION_STABILITY=1`.
