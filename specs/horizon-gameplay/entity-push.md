# Entity Push, Cramming & Spatial Grid (M2 Gameplay Slice 4c + M3 P5 Spatial Grid)

## 1.21.1 Bytecode Authority

Pinned server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) & official mappings (`03f8985492bda0afc0898465341eb0acef35f570`):

- **`Entity.push(Entity)` (`bsr.h(Lbsr;)V`, offsets 0–169)**
  - Skips if `this.isPassengerOfSameVehicle(other)` (`bsr.y(Lbsr;)Z`) or `this.noPhysics || other.noPhysics` (`bsr.ag:Z`, true for spectators).
  - Computes `dx = other.getX() - this.getX()`, `dz = other.getZ() - this.getZ()`, and `d = Mth.absMax(dx, dz)` (`ayo.a(DD)D` = `max(|dx|, |dz|)`).
  - When `d >= 0.009999999776482582` (`0.01F as f64`):
    - `d = Math.sqrt(d)`, `dx /= d`, `dz /= d`.
    - `scale = Math.min(1.0 / d, 1.0)`.
    - `dx *= scale; dz *= scale;`
    - `dx *= 0.05000000074505806; dz *= 0.05000000074505806;` (`0.05F as f64`).
    - If `!this.isVehicle() && this.isPushable()` (`!bT() && bB()`): `this.push(-dx, 0.0, -dz)` (`bsr.j(DDD)`).
    - If `!other.isVehicle() && other.isPushable()` (`!bT() && bB()`): `other.push(dx, 0.0, dz)`.
- **`LivingEntity.pushEntities()` (`btn.r()V`, offsets 0–216) & `LivingEntity.doPush(Entity)` (`btn.E(Lbsr;)V`, offsets 0–5)**
  - Server branch queries `level().getEntities(this, this.getBoundingBox(), EntitySelector.pushableBy(this))` (`dcw.a(Lbsr;Lewx;Ljava/util/function/Predicate;)`).
  - `EntitySelector.pushableBy(Entity)` (`bsw.a(Lbsr;)Predicate`): excludes `NEVER` team collision rule, `EntitySelector.NO_SPECTATORS` (`!isSpectator()`), `!other.isPushable()` (`Entity.bB()` returns `false`, `LivingEntity.bB()` returns `isAlive() && !isSpectator() && !onClimbable()`, `ArmorStand` `ciw.bB()` & `Bat` `cez.bB()` return `false`), same-vehicle riders, and mismatched `PUSH_OTHER_TEAMS`/`PUSH_OWN_TEAM` teams (default `ALWAYS`).
  - When `!list.isEmpty()`:
    - Reads `maxCramming = GameRules.RULE_MAX_ENTITY_CRAMMING` (`dcs.u`, default `24`).
    - If `maxCramming > 0 && list.size() > maxCramming - 1 && this.random.nextInt(4) == 0`:
      - Counts non-passenger entities (`!entity.isPassenger()`, `!bsr.bS()`).
      - If `count > maxCramming - 1`: deals `6.0F` cramming damage (`this.hurt(damageSources().cramming(), 6.0F)`).
    - For each entity in `list`: `this.doPush(entity)` -> `entity.push(this)`.
  - **The damage is a `hurt` call, so it carries `LivingEntity.hurt`'s invulnerability window** (`btn.a(Lbrk;F)`, offsets 228–311): a hit is rejected while `invulnerableTime > 10` (field `am:I`) and the hit is no stronger than `lastHurt` (`bm:F`); an accepted hit sets `invulnerableTime = 20` and `LivingEntity.tick` decrements it once per tick, so an equal 6.0 hit lands exactly 10 ticks after the previous one. A 25-cow pile takes damage from its own `nextInt(4)` roll about six times per tick, but each cow can only *accept* one of those per 10 ticks — without the window the pile dies roughly four times faster than vanilla.
- **`ServerPlayer.pushEntities()` (`aqv.r()`, offsets 0–17)**
  - Calls `super.pushEntities()` only when the tick-rate manager runs normally (`dcw.s()` = `tickRateManager()`, `brc.i()` = `runsNormally()`), so a player is a pusher and a cramming victim in vanilla.
  - This slice keeps players as *candidates* only: `apply_entity_push_step` iterates `mobs_snap` as pushers. Observable consequences are recorded under “Modelled Boundaries”.
- **`ArmorStand.pushEntities()` (`ciw.r()`, offsets 0–62)**
  - Overrides the body entirely: `getEntities(this, box, ciw.cc)` where `ciw.cc` = `e -> e instanceof AbstractMinecart && e.getMinecartType() == RIDEABLE`, then `if (this.distanceTo(e) <= 0.2) e.push(this)`. No cramming at all.
  - So an armor stand *does* push, but only rideable minecarts within `0.2` of its centre — it does **not** push cows or players. The model (armor stand is not a pusher) is therefore correct for the entity set this slice indexes (minecarts are not in `EntitySpatialGrid`).
- **`EntityType.<clinit>` (`bsx`)**
  - `armor_stand` (`3`): `0.5F × 1.975F`, `isPushable() = false` (`ciw.bB()`).
  - `bat` (`6`): `0.5F × 0.9F`, `isPushable() = false` (`cez.bB()`), `pushEntities()` no-op (`cez.r()`).

## Implementation Owner Contract (Slice 4c + M3 P5)

- **Spatial index owner [`src/entity_spatial.rs`](../../src/entity_spatial.rs)** (`include!("entity_spatial.rs");` in `src/main.rs` right after `include!("entity_fluid.rs");`):
  - Contiguous `SpatialEntity` array + CSR-packed `(chunk_x, chunk_z)` bucket index `EntitySpatialGrid` (`cells: HashMap<(i32, i32), (u32, u32)>`, `cell_entries: Vec<u32>`) with stamp-based `SpatialScratch` deduplication.
  - `query_aabb_into` scans only `(floor(min_x) >> 4)..=(floor(max_x) >> 4)` × `(floor(min_z) >> 4)..=(floor(max_z) >> 4)` without locking entity `Mutex`es in the inner loop.
  - `query_radius_xz_into` provides `O(1)` amortized XZ radius lookups.
- **Push & cramming owner [`src/entity_push.rs`](../../src/entity_push.rs)** (`include!("entity_push.rs");` in `src/main.rs` right after `include!("entity_spatial.rs");`):
  - `entity_push_delta`, `entity_push_pair`, `entity_selector_pushable_by`, `push_entities_step_indexed` / `push_entities_step_with`, `hurt_window_accepts`, and `apply_entity_push_step`.
  - `entity_selector_pushable_by` keeps the bytecode's deny shape for team collision rules: `(PUSH_OTHER_TEAMS on either side) && !sameTeam -> false` and `(PUSH_OWN_TEAM on either side) && sameTeam -> false` (`eyi$a` declaration order is `ALWAYS, NEVER, PUSH_OTHER_TEAMS (c), PUSH_OWN_TEAM (d)`, so the bytecode reads inverted against the constant names; the bytecode is the authority).
  - `Mob.last_hurt_tick: Mutex<Option<u64>>` is the per-entity `invulnerableTime` stamp for `hurt_window_accepts`. It is **not** persisted: vanilla does not keep `invulnerableTime` through a reload either, and adding it to the `MET4` meta tail would change the save payload for a value that is zero on load anyway.
- **Wired owners (`src/main.rs` & `src/entity_collision.rs`)**:
  - `mob_dimensions` adds `armor_stand` (`3 => (0.5, 1.975)`) and `bat` (`6 => (0.5, 0.9)`).
  - `GR_MAX_ENTITY_CRAMMING: AtomicI32` (default `24`) wired to `/gamerule maxEntityCramming [<int>]` and persisted cleanly via `GCR1` after `RNG1` when non-default (preserving existing `MET4` boolean gamerule bytes and default saved world payload digests).
  - `mob_tick` builds `EntitySpatialGrid::build_mobs_and_clients` once per tick, executes `apply_entity_push_step` (wall-safe displacement via `mob_collide_move`, `0x70` entity teleport broadcast, `0x5a` velocity impulse to pushed non-spectator `Client`s, `6.0` cramming damage with `0x24` hurt anim + `mob_env_die` on lethal cramming), and replaces the `O(N^2)` breeding partner scan (`mobs_snap.iter().find(...)`) with `grid.query_radius_xz_into`.

## Modelled Boundaries (deliberate, with the bytecode they stand on)

- **Players push nothing in this slice.** Vanilla runs `pushEntities` for `ServerPlayer` too, so a mob/player pair applies one impulse from each side per tick and players can die to cramming. Here only mobs are pushers, so a mob/player pair gets one impulse per tick instead of two, and player cramming damage is not modelled. The direction of every observed impulse is still vanilla (the mob moves away from the player, the player receives `0x5a`); the *rate* is halved for pairs that involve a player. Moving players into the pusher loop is a separate slice because it has to route client damage through `damage_after_invulnerability` + the client death path.
- **One pass per pair instead of vanilla's two.** `apply_entity_push_step` snapshots the grid, accumulates every impulse, then applies them; vanilla mutates positions inside each entity's own `aiStep`, so the outcome of a symmetric pile depends on iteration order. The two-phase form is order-independent (deterministic) and therefore differs from vanilla only in pile geometries where order matters — it is not a cost optimisation that changed an outcome; the index itself returns exactly the brute-force neighbourhood (`entity_spatial_grid_matches_brute_force_across_1200_entities_and_chunk_boundaries`).
- **`this.random.nextInt(4)` is `world_rng::next_u64() % 4`.** The server has one world RNG rather than a per-entity one; the draw count per pusher (`list.size() > maxCramming - 1` then one roll) matches the bytecode.
- **Passengers and vehicles are always absent from the grid** (`is_passenger` / `is_vehicle` / `same_vehicle` are modelled but always `false`), so the riding graph is not consulted (see Out of Scope).
- **Minecarts are not pushers or candidates.** `ArmorStand.pushEntities` only ever pushes rideable minecarts, and minecarts never call `LivingEntity.pushEntities`; both are outside the indexed entity set.

## Out of Scope (Deferred)

- Vehicle/passenger riding graph (`Entity.startRiding` / `Boat` / `Minecart` passenger push exclusion beyond `is_passenger` / `is_vehicle` / `same_vehicle` primitives).
- Scoreboard team `CollisionRule` storage wiring into runtime `Client`/`Mob` entities (currently modeled in `TeamCollisionRule` and defaulted to `Always`).
