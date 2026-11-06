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
