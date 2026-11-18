# Entity Fluid Height, Eye Immersion, Fluid Push & Lava Contact (M2 Gameplay Slice 4b)

## 1.21.1 Bytecode Authority

Pinned server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) & official mappings (`03f8985492bda0afc0898465341eb0acef35f570`):

- **`FlowingFluid` (`epc`) & `LiquidBlock` (`dko`)**
  - `FlowingFluid.getOwnHeight(FluidState)` (`epc.a(Lepe;)F`, offsets 0–9): `(float)fluidState.getAmount() / 9.0F`.
    - `LiquidBlock.getFluidState` (`dko.b_`): `level == 0` -> source (`amount = 8`, `falling = false`); `1..=7` -> flowing (`amount = 8 - level`, `falling = false`); `8..=15` -> falling (`amount = 8`, `falling = true`).
    - Waterlogged blocks (`SimpleWaterloggedBlock`) and always-waterlogged blocks (`kelp`, `kelp_plant`, `seagrass`, `tall_seagrass`, `bubble_column`) report source water (`amount = 8`, `8 / 9.0F`) via `BReg::has_water`.
  - `FlowingFluid.hasSameAbove(FluidState, BlockGetter, BlockPos)` (`epc.c`, offsets 0–20) & `getHeight(FluidState, BlockGetter, BlockPos)` (`epc.a(Lepe;Ldcc;Ljd;)F`, offsets 0–15): returns `1.0F` when `(x, y + 1, z)` holds the same fluid family (`Water` or `Lava`), else `getOwnHeight`.
  - `FlowingFluid.getFlow(BlockGetter, BlockPos, FluidState)` (`epc.a(Ldcc;Ljd;Lepe;)Lexc;`, offsets 0–340):
    - Iterates `Direction.Plane.HORIZONTAL` (`NORTH`, `EAST`, `SOUTH`, `WEST`), skipping neighbors rejected by `affectsFlow` (`epc.g`: empty or same fluid family).
    - Adds `(own - neighborHeight)` when neighbor contains fluid, or step-down `own - (belowHeight - 0.8888889F)` when neighbor is empty with `!blocksMotion`.
    - For `falling` fluid adjacent to a `isSolidFace` (`epc.a(Ldcc;Ljd;Lji;)Z`, offsets 0–62; ignores same fluid and `IceBlock` `djw`, checks `isFaceSturdy(..., SupportType.FULL)` on `(nx, y, nz)` or `(nx, y + 1, nz)`), normalizes the horizontal vector and adds `(0, -6.0, 0)`.
    - Returns `Vec3.normalize()` (`exc.d()`, returning `ZERO` below `1.0E-4`).
- **`Entity.updateFluidHeightAndDoFluidPushing(TagKey<Fluid>, double)` (`bsr.a(Lawu;D)Z`, offsets 0–431)**
  - `AABB aabb = getBoundingBox().deflate(0.001)` (`ewx.h(0.001)`).
  - Scans `[floor(minX)..ceil(maxX))` × `[floor(minY)..ceil(maxY))` × `[floor(minZ)..ceil(maxZ))`.
  - Computes `double e = (double)((float)pos.y + fluidState.getHeight(level, pos))`; when `e >= aabb.minY`, marks `inFluid = true` and `maxFluidHeight = max(e - aabb.minY, maxFluidHeight)`.
  - If `isPushedByFluid()`, scales `flow` by `maxFluidHeight` when `maxFluidHeight < 0.4`, accumulates into `vec3`, and increments `count`.
  - When `vec3.length() > 0.0`: divides by `count` if `count > 0`, calls `vec3.normalize()` iff `!(this instanceof Player)` (`cmx`), multiplies by `motionScale` (`0.014` water via `bsr.bn()`, lava via `bsr.bm()`: `ultraWarm() ? 0.007 : 0.0023333333333333335`), and clamps to minimum speed `0.0045000000000000005` when `|vx| < 0.003 && |vz| < 0.003 && len < 0.0045000000000000005`.
- **`Entity.updateFluidOnEyes()` (`bsr.H()`, offsets 0–155) & `LivingEntity.baseTick()` (`btn.aw()`, offsets 229–267)**
  - Verified against `server-1.21.1.jar`: **`bsr.H()` uses `double d = getEyeY()` directly with NO `-0.11111111F` subtraction** (0 occurrences of `0x3de38e39` across the entire jar).
  - Looks up `BlockPos.containing(getX(), d, getZ())` and marks eye immersion iff `(double)((float)by + fluidState.getHeight(level, pos)) > d`.
  - `LivingEntity.baseTick()` requires `isEyeInFluid(FluidTags.WATER) && !level().getBlockState(BlockPos.containing(getX(), getEyeY(), getZ())).is(Blocks.BUBBLE_COLUMN)` before draining air supply.
- **`Entity.isInLava()` (`bsr.bt()`, offsets 0–29)**
  - Verified against `server-1.21.1.jar`: returns `!this.firstTick && this.fluidHeight.getDouble(FluidTags.LAVA) > 0.0` (populated by `updateFluidHeightAndDoFluidPushing` with `deflate(0.001)` rather than `deflate(-0.1, -0.4, -0.1)`).
- **`Player.<clinit>` (`cmx`) & `EntityType.<clinit>` (`bsx`)**
  - Player eye heights: standing `1.62f`, crouching `1.27f`, fall-flying `0.4f`; bounding boxes `0.6f × 1.8f` / `1.5f` / `0.6f`.
  - Mob eye heights: `cat` `0.35f`, `cave_spider` (`16`) `0.45f`, `chicken` `0.644f`, `cow` `1.3f`, `sheep` `1.235f`, `spider` `0.65f`, `zombie`/`skeleton`/`drowned`/`husk`/`stray`/`zombie_villager` `1.74f`, `enderman` `2.55f`, `squid`/`glow_squid` `0.4f`, `villager`/`witch` `1.62f`, `wolf` `0.68f`, default `height * 0.85f`.
  - `EntityType$Builder.fireImmune()` (`bsx$a.c`): IDs `8, 31, 45, 67, 88, 103, 112, 116, 119, 120, 123, 127`.

## Implementation Owner Contract (Slice 4b)

- Shared owner module [`src/entity_fluid.rs`](../../src/entity_fluid.rs) (`include!("entity_fluid.rs");` in `src/main.rs` right after `include!("entity_collision.rs");`, keeping `tools/layers_check.sh` and `policy.json` untouched):
  - `entity_fluid_state_of`, `fluid_own_height`, `fluid_has_same_above_with`, `fluid_height_with`, `fluid_flow_with`, `update_fluid_height_and_push_with`.
  - `eye_in_fluid_with`, `eye_in_bubble_column_with`, `aabb_in_water`, `aabb_in_lava`.
  - Player & mob wrappers: `player_eye_height`, `player_bounding_box`, `player_eye_in_water`, `player_drowning_submerged`, `player_in_lava`, `mob_eye_height`, `mob_fire_immune`, `mob_drowning_submerged`, `mob_in_water`, `mob_in_lava`, `mob_water_state`.
- Wired owners (`src/main.rs` & `src/entity_collision.rs`):
  - Fixed `cave_spider` entity type ID `13 -> 16` in `src/entity_collision.rs::mob_dimensions`.
  - `player_touch_aabb` delegates to `player_bounding_box(...).inflate(1.0, 0.5, 1.0)`.
  - `Client` tick replaces ad-hoc `block_at == wst` / `== lst` (including the missing-parentheses `||` precedence bug on `fire_immune`) with `player_drowning_submerged` and `!fire_immune && player_in_lava`.
  - `cart_in_water` delegates to `aabb_in_water(EntityAabb::from_dimensions(pos[0], pos[1], pos[2], 0.98, 0.7))`.
  - `mob_snap_y` short-circuits when `mob_in_water`, descends through non-motion-blocking water columns (`!fluid_blocks_motion`) to the bed, and suppresses `cow_land` and fall damage when `landed_in_water` is true.
  - `mob_tick` replaces hardcoded `(mp.1 + 1.0).floor() == wst` with `mob_drowning_submerged`, applies water current push via `mob_collide_move` (with `0x70` teleport broadcast on displacement), clears undead (`124 | 91`) daylight burn `fuse` and suppresses daylight burn in water (without touching creeper `23` fuse), and applies `4.0` lava contact damage on the `server_ticks(10)` cadence for `!mob_fire_immune` mobs (emitting `0x24` hurt anim and `mob_env_die(..., true)` on lethal hit).
  - Existing `GeneratedFluidOwner` (`fluid_from_block`) remains intact for scheduled fluid flow ticks.

## Verification & Mutation Gates

- 6 new tests in `entity_fluid_tests`:
  - `entity_fluid_height_and_has_same_above_water_lava_waterlogged_and_kelp`
  - `entity_fluid_get_flow_horizontal_slope_stepdown_and_falling`
  - `entity_fluid_update_height_and_push_motion_scale_and_player_vs_mob`
  - `entity_fluid_eye_immersion_player_poses_and_mobs`
  - `entity_fluid_is_in_lava_deflate_0_001_boundary_and_flowing_lava`
  - `entity_fluid_integration_client_cart_and_mob_tick`
- 3 `tools/mutation_check.sh` entries (`entity_fluid_has_same_above_returns_full_height`, `entity_fluid_is_in_lava_uses_0_001_deflate`, `entity_fluid_eye_immersion_respects_pose_and_surface`).
- Full test suite (`cargo test --offline --locked -j 4 -- --test-threads=8`): `692 passed; 0 failed; 1 ignored` (pre-existing ignored gate), `layers_check.sh` PASS (`44 modules, 111 edges, 0 named errors`), `debt_ratchet.sh` PASS (`0 over limit`).

## Out of Scope (Deferred to Slice 4c+)

- Full `LivingEntity.travel` fluid buoyancy / swim speed / vertical water drag integration and `Player` server-authoritative water current pushback.
- Lava `motionScale` dimension wiring. **Corrected by Slice 4d:** `bsr.bm()` is
  `dimensionType().ultraWarm() ? 0.007 : 0.0023333333333333335`, so `0.007` belongs to the
  ultra-warm (Nether) dimension — the polarity in the line above was inverted. Nothing observable
  depended on it before Slice 4d (the only call site read `max_height`, not `push`); the constants
  are renamed to the bytecode in `src/entity_fluid.rs` and pinned by
  `travel_contract_4d_lava_push_scale_dimension` (see [entity-travel.md](entity-travel.md)).
