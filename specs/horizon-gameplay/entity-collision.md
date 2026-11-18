# Entity Collision & Step-Up (M2 Gameplay Slice 4a)

## 1.21.1 Bytecode Authority

Pinned server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) & official mappings (`03f8985492bda0afc0898465341eb0acef35f570`):

- **`Entity.collide(Vec3)` (`bsr.a(Lexc;)Lexc;`, offsets 0–350)**
  - Collects entity collisions in `aabb.expandTowards(vec)` (`dcw.c(Lbsr;Lewx;)`); if `vec.lengthSqr() != 0.0`, evaluates `Entity.collideBoundingBox(this, vec, aabb, level, entityShapes)`.
  - Flags `xChanged = vec.x != vec3.x`, `yChanged = vec.y != vec3.y`, `zChanged = vec.z != vec3.z`, `bl4 = yChanged && vec.y < 0.0`.
  - Step-up enters iff `maxUpStep() > 0.0F && (bl4 || onGround()) && (xChanged || zChanged)` (offsets 126–157).
  - Step-up base `aabb2 = bl4 ? aabb.move(0, vec3.y, 0) : aabb`; candidate sweep `aabb3 = aabb2.expandTowards(vec.x, maxUpStep, vec.z)` and if `!bl4`, `.expandTowards(0.0, (double)(-1.0E-5F), 0.0)` (`-9.999999747378752E-6`, offsets 199–216).
  - Iterates ascending `collectCandidateStepUpHeights(aabb2, colliders, maxUpStep, (float)vec3.y)`, evaluates `collideWithShapes(Vec3(vec.x, h, vec.z), aabb2, colliders)`, and returns **on the first** candidate with `vec32.horizontalDistanceSqr() > vec3.horizontalDistanceSqr()`, returning `vec32.add(0, -(aabb.minY - aabb2.minY), 0)` (offsets 264–342).
- **`Entity.collideBoundingBox` (`bsr.a(Lbsr;Lexc;Lewx;Ldcw;Ljava/util/List;)Lexc;`) & `Entity.collideWithShapes` (`bsr.a(Lexc;Lewx;Ljava/util/List;)Lexc;`, offsets 0–194)**
  - Empty shape list returns `delta` untouched (offsets 0–10).
  - Resolves `Y` first (`if (y != 0.0) { y = Shapes.collide(Y, box, shapes, y); if (y != 0.0) box = box.move(0, y, 0); }`).
  - Horizontal order (`zFirst = Math.abs(x) < Math.abs(z)`, `dcmpg; ifge 80`, offsets 63–81): resolves `Z` before `X` strictly when `|x| < |z|`; **on tie `|x| == |z|`, `X` resolves first**.
- **`Entity.collectCandidateStepUpHeights(AABB, List<VoxelShape>, float, float)` (`bsr.a(Lewx;Ljava/util/List;FF)[F`, offsets 0–156)**
  - Computes `float f = (float)(d - box.minY)`, skips `f < 0.0F` or `f == yClipped`, breaks out of the shape's Y coords when `f > maxUpStep` (so `f == maxUpStep` is included), deduplicates via `FloatArraySet`, and sorts ascending (`FloatArrays.unstableSort`).
- **`Entity.move(MoverType, Vec3)` (`bsr.a(Lbts;Lexc;)V`, offsets 275–380) & `Mth.equal` (`ayo.b(DD)Z`, offsets 0–18)**
  - `horizontalCollision = !Mth.equal(vec.x, vec3.x) || !Mth.equal(vec.z, vec3.z)` with `Mth.equal(a, b)` checking `Math.abs(b - a) < (double)1.0E-5F`.
  - `verticalCollision = vec.y != vec3.y`; `verticalCollisionBelow = verticalCollision && vec.y < 0.0`.
- **`BlockCollisions` (`dca.<init>` offsets 63–176, `computeNext` offsets 172–284)**
  - Scans integer block coordinates `[floor(min - 1e-7) - 1 ..= floor(max + 1e-7) + 1]` on X, Y, Z, visiting blocks whose collision shape extends outside `[0, 1]^3` (such as 1.5-block fences/walls/gates) even when entity `minY` sits at `by + 1.2` (`n + 0.2`), then filters by AABB overlap.
- **`Shapes.collide` (`exs.a`) & `VoxelShape.collideX` (`exv.a`)**
  - Early-returns `0.0` when `Math.abs(dist) < 1e-7`.
  - Perpendicular overlap: `a_min + 1e-7 < b_max && a_max - 1e-7 >= b_min`.
  - Sweep clamp: for `dist > 0`, `a_max - 1e-7 < b_min` with `e = b_min - a_max >= -1e-7`; for `dist < 0`, `a_min + 1e-7 >= b_max` with `e = b_max - a_min <= 1e-7`.
- **`Attributes.STEP_HEIGHT` (`buw.B`) & `EntityType.<clinit>` (`bsx`)**
  - `Entity.maxUpStep()` (`bsr.dI`) returns `0.0F` (used for minecarts); `LivingEntity.maxUpStep()` (`btn.dI`) reads `Attributes.STEP_HEIGHT` (default `0.6`).
  - `EntityType.<clinit>` dimensions: `minecart` / `chest_minecart` `0.98f x 0.7f`, `cow` `0.9f x 1.4f`, `creeper` `0.6f x 1.7f`, `sheep` `0.9f x 1.3f`, `pig` `0.9f x 0.9f`, `chicken` `0.4f x 0.7f`, `skeleton` `0.6f x 1.99f`, `zombie` `0.6f x 1.95f`, `spider` `1.4f x 0.9f`, `cave_spider` `0.7f x 0.5f`.

## Implementation Owner Contract (Slice 4a)

- Shared owner module `src/entity_collision.rs` (`include!("entity_collision.rs");` in `src/main.rs`, keeping `tools/layers_check.sh` and `policy.json` untouched):
  - `CollideResult { delta: [f64; 3], horizontal_collision: bool, vertical_collision: bool, vertical_collision_below: bool, stepped_up: bool }`.
  - `collect_block_collision_boxes`, `collide_axis`, `collide_with_shapes`, `collect_candidate_step_up_heights`, `entity_collide`, `entity_collide_world`, `step_entity_move`.
  - `mob_dimensions`, `mob_has_support`, `mob_collide_move`.
- Wired owners:
  - `CartRuntimeWorld::move_cart` (`src/main.rs`) delegates to `entity_collide_world(aabb, delta, false, 0.0, &[])`.
  - `mob_tick` horizontal steps (creeper, passive wheat follow, passive wander, skeleton, zombie) delegate to `mob_collide_move` (`max_up_step = 0.6`) and preserve `mob_snap_y` fallback only when `!res.stepped_up && !mob_has_support(...)`, while `mob_snap_y` itself short-circuits when `mob_has_support` is true (preserving slab resting positions).

## Verification & Mutation Gates

- 6 new tests in `entity_collision_tests`:
  - `entity_collide_axis_priority_z_first_when_x_lt_z_and_x_first_on_tie`
  - `entity_collide_step_up_over_slab_and_refuses_full_block_when_max_up_step_0_6`
  - `entity_collide_step_up_blocked_by_low_ceiling`
  - `entity_collide_fence_1_5_block_prevents_step_over_and_catches_window`
  - `entity_collide_corner_multi_box_sliding_and_zero_1e7_tunneling`
  - `entity_collide_mob_tick_integration_cow_creeper_sheep`
- Existing `cart_` (3 tests) and `spawner_` (8 tests) stay green.
- 3 `tools/mutation_check.sh` entries (`entity_collide_axis_tie_prefers_x`, `entity_collide_step_up_respects_max_up_step`, `entity_collide_block_window_catches_1_5_fence`) proved `3/3` under `MUTATION_FILTER=entity_collide_ MUTATION_STABILITY=1`.
- Full test suite: `681 passed; 0 failed; 1 ignored` (pre-existing ignored gate), `layers_check.sh` PASS (`44 modules, 111 edges, 0 named errors`), `debt_ratchet.sh` PASS (`0 over limit`).

## Out of Scope (Deferred to Slice 4b+)

- Full `LivingEntity.travel` gravity/fluid/drag loop and replacing `mob_snap_y` surface-height snapping altogether.
- Entity-entity pushback / boat / shulker dynamic collider lists (the `extra_boxes` parameter is ready for future wiring).
