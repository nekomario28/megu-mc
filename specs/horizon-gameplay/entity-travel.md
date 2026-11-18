# `LivingEntity.travel` Fluid Branches, Buoyancy & Lava Push Scale (M2 Gameplay Slice 4d)

## 1.21.1 Bytecode Authority

Pinned server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) & official mappings (`03f8985492bda0afc0898465341eb0acef35f570`).
`tools/bytecode_probe.sh net.minecraft.world.entity.LivingEntity` and
`… net.minecraft.world.entity.Entity` reproduce every offset cited here; the offsets are
`javap -p -c` `Code:` offsets.

- **`LivingEntity.travel(Vec3)` (`btn.a(Lexc;)V`, offsets 0–1251)** — the method is one
  `if (isControlledByLocalInstance())` (`btn.da()`) block whose body is a five-way branch on the
  environment. Slice 4d owns the two fluid branches and the gravity value every branch reads;
  the fall-flying and default branches remain out of scope.
  - Entry (`8–53`): `double d = this.ba()` = `Entity.getGravity()` = `0.08`, lowered to
    **`0.01`** when `this.dr().d <= 0.0` and the entity has `MobEffects.SLOW_FALLING`
    (`bsb.B`). `bl` (`istore 4`) is that same `docs` `getDeltaMovement().y <= 0.0` sample.
  - **Water branch** (`67–351`), taken when `isInWater() && isAffectedByFluids() &&
    !canStandOnFluid(fluidState)`:
    - `float f = isSprinting() ? 0.9F : getWaterSlowDown()`; `getWaterSlowDown()`
      (`btn.fh()`) is the constant **`0.8F`**.
    - `float g = 0.02F`; `float h = (float)getAttributeValue(Attributes.WATER_MOVEMENT_EFFICIENCY)`
      (`buw.E`), halved when `!onGround()`; while `h > 0`, `f += (0.54600006F - f) * h` and
      `g += (getSpeed() - g) * h` (`0.54600006` is `WATER_MOVEMENT_EFFICIENCY_SLOW_DOWN`,
      `getSpeed()` is `LivingEntity.speed`, field `cg`).
    - `MobEffects.DOLPHINS_GRACE` (`bsb.D`) sets `f = 0.96F`.
    - `moveRelative(g, movementInput)`; `move(MoverType.SELF, getDeltaMovement())`;
      `if (horizontalCollision && onClimbable()) vec3 = (vec3.x, 0.2, vec3.z)`;
      `setDeltaMovement(vec3.multiply(f, 0.8F, f))` — the vertical factor is the **`float 0.8F`
      widened to double (`0.800000011920929`)**, not `f`;
      `setDeltaMovement(getFluidFallingAdjustedMovement(d, bl, deltaMovement))`;
      `if (horizontalCollision && isFree(v.x, v.y + 0.6F - getY() + e, v.z)) v.y = 0.3F`
      (`0.30000001192092896`, `0.6000000238418579`; `e` is `getY()` sampled at `90–94`, so the
      probe reduces to `v.y + 0.6 - moved.y`).
  - **Lava branch** (`353–561`), `isInLava() && isAffectedByFluids() && !canStandOnFluid(...)`:
    - `moveRelative(0.02F, movementInput)`; `move(MoverType.SELF, getDeltaMovement())`.
    - `if (getFluidHeight(FluidTags.LAVA) <= getFluidJumpThreshold())`
      `deltaMovement = deltaMovement.multiply(0.5, 0.8F, 0.5)` then
      `getFluidFallingAdjustedMovement(d, bl, deltaMovement)`; **else**
      `deltaMovement = deltaMovement.scale(0.5)` (vertical included).
    - `if (d != 0.0) deltaMovement = deltaMovement.add(0.0, -d / 4.0, 0.0)` → the **`-0.02`**
      per-tick lava sink at `d = 0.08`.
    - The same `horizontalCollision && isFree` lift to `0.3F` closes the branch.
- **`LivingEntity.getFluidFallingAdjustedMovement(double, boolean, Vec3)` (`btn.a(DZLexc;)Lexc;`,
  offsets 0–100)**: returns the vector untouched when `d == 0.0` **or** `isSprinting()`; otherwise
  `e = v.y - d / 16.0`, except when `bl` and `|v.y - 0.005| >= 0.003` and `|v.y - d / 16| < 0.003`,
  where `e = -0.003`. At `d = 0.08` the second predicate is `|v.y - 0.005| < 0.003`, so the two
  predicates are exclusive and the `-0.003` frame is only reachable at the `SLOW_FALLING` gravity
  (`d = 0.01`, `d / 16 = 0.000625`). The `-d / 16` term is the buoyancy that makes a still entity
  in water converge on `-0.025`/tick and in lava on `-0.125`/tick instead of accelerating.
- **`Entity.bm()` (`bsr.bm()`, offsets 0–61)** — `updateInWaterStateAndDoFluidPushing()`:
  `fluidHeight.clear(); updateFluidHeightAndDoFluidPushing();`
  `double d = dimensionType().ultraWarm() ? 0.007 : 0.0023333333333333335;` then
  `updateFluidHeightAndDoFluidPushing(FluidTags.LAVA, d)`; returns `isInWater() || <lava hit>`.
  `DimensionType.ultraWarm()` is `dwy.i()` (mappings), so **the `0.007` lava push belongs to the
  ultra-warm (Nether) dimension and `0.0023333333333333335` to the temperate (Overworld) one.**
  `Entity.bn()` (offsets 33–42) pushes water at `0.014` regardless of dimension.
- **`Entity.getFluidJumpThreshold()` (`bsr.di()`, offsets 0–19)**: `getEyeHeight() < 0.4F ? 0.0 :
  0.4`.
- **`Entity.getInputVector(Vec3, float, float)` (`bsr.b(Lexc;FF)Lexc;`, offsets 0–104)**: zero
  below `1.0E-7`, normalized above unit length, scaled by the amount, then rotated by
  `Mth.sin`/`Mth.cos(yaw * 0.017453292F)` as `(x·cos − z·sin, y, z·cos + x·sin)`.
- **`LivingEntity.jumpInFluid` / `goDownInWater` (`btn.c(Lawu;)V` / `btn.fg()`)**: `±0.04` on the
  vertical. Both are driven by `aiStep`'s `jumping` flag, which a `Mob` without an input source
  never sets here; they are recorded above and not wired.

## The lava push scale polarity is inverted relative to the Slice 4b wording

`specs/horizon-gameplay/entity-fluid.md` and the Slice 4d brief name `0.007` "Overworld" and
`0.0023333333333333335` "Nether". `bsr.bm()` says the opposite (`ultraWarm → 0.007`). Nothing
observable depended on it before this slice because the only lava-scale call site
(`aabb_in_lava_with`) reads `max_height`, not `push`, so the constants were inert and misnamed
rather than wrong. Slice 4d renames them to the bytecode (`LAVA_ULTRA_WARM_MOTION_SCALE = 0.007`,
`LAVA_TEMPERATE_MOTION_SCALE = 0.0023333333333333335`) and covers both with
`travel_contract_4d_lava_push_scale_dimension`, because the moment the dimension is wired the
names would otherwise produce the wrong number with no test able to see it.

## Implementation Owner Contract (Slice 4d)

`src/entity_fluid.rs` (already `include!`d in `src/main.rs`; no new `mod`, `tools/layers_check.sh`
and `policy.json` untouched):

- `fluid_push_scale(EntityFluidKind, ultra_warm)` — `0.014` water, `0.007` ultra-warm lava,
  `0.0023333333333333335` temperate lava; `level_ultra_warm()` / `set_level_ultra_warm(bool)`
  own the `DimensionType.ultraWarm()` value (the hosted level is the Overworld, `false`).
- `fluid_falling_adjusted_movement(gravity, falling, vel, sprinting)` —
  `getFluidFallingAdjustedMovement`.
- `fluid_jump_threshold(eye_height)`, `living_gravity(falling, slow_falling)`,
  `fluid_input_vector(input, amount, yaw)` (`getInputVector`), `entity_is_free_blocks(aabb)`
  (`Entity.isFree`, block half only).
- `LivingTravelContext` (the branch inputs) and `FluidTravelOutcome { vel, moved }`.
- `travel_fluid_step(kind, vel, input, yaw_deg, ctx, move_fn, is_free)` — one water or lava
  branch. `move_fn` **is** `Entity.move(MoverType.SELF, …)`, so the collision owner stays
  `src/entity_collision.rs` (`entity_collide_world`, `Attributes.STEP_HEIGHT = 0.6`).
- Mob wrappers: `mob_water_state_vel`, `mob_lava_state_vel`, `mob_fluid_travel_tick`.

Wired owners:

- `Mob.vel: Mutex<[f64; 3]>` is `LivingEntity.deltaMovement` for the fluid branches. Land
  movement in this server is displacement based, so nothing else reads it, and
  `mob_fluid_travel_tick` clears it whenever the mob is in neither fluid (the non-fluid branch's
  friction).
- `mob_tick` calls `mob_fluid_travel_tick` once per mob per tick in place of the old
  "water push applied as a displacement" block, folds `updateFluidHeightAndDoFluidPushing`'s push
  into the velocity first (as `Entity.baseTick` does before `travel`), applies the resulting
  position delta, and broadcasts `0x70` when it moved.
- `mob_snap_y` short-circuits for lava as well as water, so the surface snap no longer teleports a
  mob out of a lava pocket the travel branch is sinking it through.

## Deliberate deviations, stated as deviations

- **The AI steering impulse is not routed through `moveRelative`.** These mobs have no
  `movementInput` vector: every AI branch calls `mob_collide_move(m, mp, dx, dz)` with a
  displacement. Slice 4d therefore owns the *velocity* (fluid push + buoyancy/sink + the
  `0.8F`/`0.5` drag applied to it) and leaves the steering displacement alone, rather than
  pretending a displacement is an acceleration. The drag constants, the impulse amount, the
  efficiency blend, the sprint/dolphins terms and the falling adjustment are all implemented and
  tested through the same code vanilla runs.
- **`Entity.isFree` is block-only.** Vanilla's `Level.noCollision` also rejects entity AABBs; this
  server's mob has no entity-AABB obstruction query (the push grid excludes the querying mob).
  The `0.3F` escape lift can therefore fire where an entity would have obstructed it.
- **`jumpInFluid` (`±0.04`) is not wired** — no `jumping` input source for these mobs.
- **`WATER_MOVEMENT_EFFICIENCY` is `0.0` at runtime** for every vanilla mob, so the `h > 0` blend
  is exercised by tests but inert in the tick, which is also true of vanilla.

## Verification & Mutation Gates

- 8 tests in `src/tick_tests.rs`, all prefixed `travel_contract_4d_` (no `#[ignore]`):
  - `travel_contract_4d_water_travel_drag_sprint_dolphins_and_impulse` — `0.8F` horizontal,
    widened `0.8F` vertical, `-d/16` falling adjustment, `0.9F` sprint (which also skips the
    falling adjustment), `0.96F` DOLPHINS_GRACE, `0.02` impulse reaching `Entity.move` undamped,
    and the `-0.025` water terminal sink.
  - `travel_contract_4d_water_movement_efficiency_blends_slow_down` — `0.54600006F` blend and the
    airborne `h * 0.5F`.
  - `travel_contract_4d_water_jump_threshold_and_input_vector_rotation` — `0.4F` eye-height
    threshold (strictly below) and `getInputVector` zero/normalize/yaw.
  - `travel_contract_4d_lava_drag_and_vertical_sinking` — `0.5` drag with `0.8F` vertical below
    the jump threshold, `scale(0.5)` above it, `-0.02` sink, zero-gravity arm, `-0.125` terminal.
  - `travel_contract_4d_fluid_falling_adjusted_movement_gravity_and_clamp` — `d/16`, the
    `-0.003` terminal frame at `SLOW_FALLING` gravity, and the two early returns.
  - `travel_contract_4d_horizontal_collision_lift_and_climbable` — `0.3F` `isFree` lift,
    obstructed probe, `0.2` climbable lift.
  - `travel_contract_4d_lava_push_scale_dimension` — `0.014`/`0.007`/`0.0023333333333333335` and
    that the owner flag reaches the scale.
  - `travel_contract_4d_mob_tick_water_and_lava_sink_without_fall_damage` — `mob_tick`
    integration (spider sinks in water with no horizontal drift and no fall damage, blaze sinks
    to the lava floor), `mob_snap_y` deferral for water and for a *unsupported* lava mob, and the
    velocity reset when the mob leaves the fluid.
- 7 `tools/mutation_check.sh` entries (`travel_contract_4d_*`): the `0.8F` vertical drag, the
  `0.9F` sprint slow-down, the ultra-warm lava polarity, the `-0.003` terminal frame, the
  `0.4F` jump threshold, the `mob_tick` vertical delta, and `mob_snap_y`'s lava deferral.
  `MUTATION_FILTER='travel_contract_4d_' MUTATION_STABILITY=1 CARGO_BUILD_JOBS=4` reports
  `7 proved, 0 not a gate, 0 parked, 0 skipped`.
- Full suite: `727 passed; 0 failed; 1 ignored` (the pre-existing parked gate),
  `tools/layers_check.sh` PASS (`44 modules, 111 edges, 0 named errors`),
  `tools/debt_ratchet.sh` PASS (`0 over limit`).

## Out of Scope (Deferred)

- `LivingEntity.travel`'s fall-flying and default branches (terrain friction, sprint jump,
  `MobEffects.LEVITATION`, `Entity.setDiscardFriction`), so land movement keeps the
  displacement model.
- `aiStep`'s jump block (`jumpInFluid` / `jumpFromGround` / `goDownInWater`) and air/fluid
  climbing for mobs.
- `Entity.fallDistance` for mobs: fall damage is still computed by `mob_snap_y`'s descent, which
  travel now bypasses while the mob is in a fluid.
