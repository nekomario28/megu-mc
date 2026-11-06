# Neighbor finalization

First contract: the exposed-face sequence is exactly native DiscreteVoxelShape,
independent of insertion order, duplicate writes, cavities and disconnected cells.
Extract the existing tree traversal semantics into one geometry owner and route
both tree and template through it. Check actual native forAllFaces output and
existing native tree edge cases. Kill axis/coordinate/face-order mutants through
the shared owner. Do not claim block-state finalization from face ordering alone.

Next contract: invoke native updateShapeAtEdge/updateFromNeighbourShapes on
scripted levels with nontrivial neighbors, capture state writes and ordered
block/fluid reservations, and isolate each first divergence. Inventory existing
runtime owners before adding a state-transition module. Unknown native operations
must fail, not silently become no-ops. Cover waterlogged, connected, fragile,
falling and mixed-fluid states; do not assume a full-cube output makes neighbors
unchangeable. Production callers and both-template write visibility stay pinned.

## First bounded implementation batch (2026-11-06)

This batch implements **a subset**, not general native `updateShapeAtEdge` parity:

- The existing `voxel_faces::exposed_faces` is unchanged and still provides native
  face traversal order. `tree::schedule_leaf_edge_state` now has a single owner
  reused by both tree edge tests and solid-template placement; neither caller
  computes leaf distance ticks independently.
- A newly placed ordinary **full-cube** template state can connect an existing
  horizontal fence's corresponding side. The rule lives in
  `BReg::fence_after_full_neighbor` for future runtime reuse. It refuses
  vertical directions, unchanged connections, non-full/transparent/exception
  neighbor states and non-fence targets. **This is a positive connection-only
  subset**: disconnects, other fence/gate adjacency, panes, walls, stairs,
  waterlogging, fragile blocks and other shape effects remain unsupported.
- Fluid reservation on exterior template faces keeps its existing path and
  identity/order. Source-liquid scheduling and real fluid delivery are different
  gates. Generated leaf ticks follow the existing queue/deduplication owner.
- A new `solid_template_edges_update_fences_and_reserve_native_leaf_ticks`
  production test checks the actual `Template::place_solid_blocks` path,
  read-through update, two ordered writes and a generated leaf tick. The
  `fence_full_cube_edge_changes_only_the_facing_connection` test checks
  four directions, side preservation, idempotence and rejection controls.
- `tools/template_neighbor_oracle.py` calls the pinned native
  `BlockState.updateShape` for seven positive/negative fence probes. This
  optional native gate is not a full `StructureTemplate.placeInWorld` oracle:
  the actual whole-template ordering and secondary side effects are still open.

### Commands and evidence boundary

```sh
python3 tests/template_neighbor_oracle_test.py
cargo test --locked fence_full_cube_edge_changes_only_the_facing_connection -- --test-threads=1
cargo test --locked solid_template_edges_update_fences_and_reserve_native_leaf_ticks -- --test-threads=1
cargo test --locked leaf_edge_schedules_match_native_order -- --test-threads=1
python3 tools/template_neighbor_oracle.py --jar /path/to/1.21.1/server.jar
bash tests/verify.sh
```

The Java probe requires the authenticated pinned game jar; the regular portable
suite must not depend on it. On 2026-11-06, the GitHub-only authoring environment
could not run Rust or native-Java-with-jar; passing results are **not claimed**.
Check the actual finalization trace and all existing Fossil native fixtures before
promoting this from a bounded slice to a broader shape-update implementation.

## Additional bounded implementation (2026-11-06, PR #33)

- **Connected shapes**: `BReg::pane_after_full_neighbor` updates a glass-pane
  or iron-bars side for a newly adjacent opaque full cube; an already-true
  direction remains unchanged. `wall_after_full_neighbor` handles the isolated
  `up=true`, all-other-sides `none`, air-above wall case and converts the one
  exposed side `none -> low`. Complex wall posts, tall coverage, gate, other
  wall adjacency and negative disconnects remain out of scope.
- **Stairs**: `stair_after_neighbors` recomputes straight/inner/outer shape
  from the read-through neighborhood, matching stair orientation/half and
  guarding corners with the flank state. This is exercised by direct Rust
  tests; a solid-only template can *change an existing stair's neighbors*
  but cannot itself place arbitrary stair palette states.
- **Waterlogged**: state rewrites preserve the waterlogged property. The current
  runtime `fluid_from_block` recognizes only dedicated water/lava block IDs
  80..111 and cannot safely process a waterlogged fence/stair/pane as a fluid
  source. **No fake zero-delay water tick is enqueued for waterlogged blocks.**
  This is **retention only, not native fluid scheduling or delivery parity**.
- **Fragile support**: `BReg::fragile_loses_support` classifies known standing/
  wall torch and hanging/standing lantern attachment directions on loss of air/
  fluid support. The existing runtime `block_updated` calls
  `note_fragile_supports`, conditionally removes an unsupported state and
  drops its item once via `set_block_if`; its existing bounded/deferred
  recursion also allows support-loss chains. Other fragile families, scheduled
  native break timing and full shape-tag semantics are **not** covered.
- **Falling**: no second gravity implementation was added. The existing runtime
  `note_falling` / `falling_tick` and tests in `tick_tests.rs` already
  cover support loss, multi-block cascade, motion, conservation and save/reload.
  These are gameplay-port tests, **not** new 1.21.1 native full-placement captures.
- **Native oracle** now includes 13 isolated shape cases covering fences,
  panes/bars, the simple wall and stair corners plus negative controls.
  Portable validation checks Java/Python case IDs, missing/reordered rows,
  positive no-op and negative unexpectedly changing states.

### Verification status and exact commands

At authoring time: mirrored portable Python validation **6 passed**, Python
syntax check **passed**; Java 21 `javac -proc:none` **passed with a local stub
NativeHarness**, not the actual native classpath. The connected environment has
**no cargo/rustc** and no authenticated server jar. Rust tests, all full gates,
true Java native output and a running-world acceptance were **not run**.

```sh
python3 tests/template_neighbor_oracle_test.py
cargo test --locked fence_full_cube_edge_changes_only_the_facing_connection -- --test-threads=1
cargo test --locked pane_wall_and_waterlogged_connections_preserve_unrelated_properties -- --test-threads=1
cargo test --locked stairs_recompute_outer_inner_and_preserve_waterlogged_on_neighbor_changes -- --test-threads=1
cargo test --locked fragile_support_loss_drops_torches_and_lanterns_exactly_once -- --test-threads=1
cargo test --locked fragile_direction_matching_is_not_just_any_air_neighbor -- --test-threads=1
cargo test --locked leaf_edge_schedules_match_native_order -- --test-threads=1
cargo test --locked fossil_ -- --test-threads=1
cargo test --locked leaf_updates_wait_for_an_eligible_chunk_and_propagate -- --test-threads=1
cargo test --locked two_stacked_sand_blocks_fall_without_deadlocking -- --test-threads=1
python3 tools/template_neighbor_oracle.py --jar /path/to/authenticated-1.21.1-server.jar
bash tests/verify.sh
```

**Next native gaps**: input-to-output state for every affected block class,
waterlogged fluid reservation and delivery with actual timing, generic
neighbor shape recursion with ordered writes, fragile drops from real
`StructureTemplate.placeInWorld`, falling-block scheduled timing, and
whole-chunk correctness. Keep P1-1 **open** until those are compared against
native running 1.21.1 (including failure cases); do not promote PR #33 to
ready solely on the portable checks.
