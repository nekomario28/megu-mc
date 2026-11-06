# P1-1: exhaustive support inventory and ordered native event baselines

Status: **test infrastructure only**. A complete list of block states is NOT a
complete implementation of `BlockState.canSurvive`, nor is a native scheduling
request a native executed tick. This slice is stacked on the draft P1-1 branch
and does not independently change `perf/worldgen-parallel` or `main`.

## Coverage and authority

- Pinned protocol/version: Minecraft Java **1.21.1**.
- Required outer server JAR SHA-1:
  `59353fb40c36d304f2035d51e7d6e6baa98dc05c`.
  `vanilla_java.pinned_runtime` checks this before any Java execution.
- Registry input: `assets/blocks_1211.json`, **1,060 distinct block names,
  26,684 contiguous block-state IDs**. `native_support_inventory.py` expands
  the complete Cartesian product of each block's typed properties; verifies
  each state-ID interval and total; refuses missing or duplicated ranges.
- **12 environments per state**:
  empty, six orthogonal single-stone supports, all-stone, all-but-floor,
  all-but-north, floor water, floor lava. The resulting exhaustive *state ×
  selected contexts* matrix is **320,208 independent native canSurvive
  observations**. It is not all possible world configurations, light levels,
  biome conditions, redstone power, entities, or player behavior.
- Native caller: `NativeSurvivalOracle.java`, invoking the 1.21.1 obfuscated
  `BlockStateBase.canSurvive(LevelReader, BlockPos)` directly. Block tags are
  loaded through existing `VanillaBlockTags` from the game's native pack;
  `NativeFluidTags` loads **real** water and lava tags using the native
  registry/resource-pack loader. A tag/level/registry failure is recorded as
  `U:...`, and the run exits nonzero. It cannot silently vote "false".
- Outputs contain state ID, context, native boolean/unsupported, normalized
  state including properties, and the ordered world read positions. Validator
  rejects missing, duplicated, reordered, mismatched-state and unsupported rows.
  Do **not** commit generated native data or bundle the JAR for redistribution.

## Native neighbor events, scheduled-tick requests and delivery

- `NativeHarness.Level` records the native requested setBlock writes and
  **the native ScheduledTick record itself** (type, position, due tick, priority,
  sub-tick order). LevelAccessor's default `scheduleTick` constructs the
  record; a typed `LevelTickAccess` proxy only records its insertion.
- `NativeNeighborTraceOracle` exercises waterlogged pane/fence/stairs,
  leaf next to log, falling sand and unsupported torch; multiple callbacks
  share one native sub-order counter. A separate native
  `updateNeighbourShapes` pass observes changes with a write-through proxy.
- `NativeTraceDeliveryOracle` imports **that trace**, schedules its tick
  appointments into real 1.21.1 `LevelTicks` + `LevelChunkTicks` objects,
  and records actual callback order for each independent *block* and *fluid*
  queue. The Python validator checks due times, identity, exact-once
  dispatch and repeatability across **two fresh JVM runs**. Queues are separate
  (do not invent an interleaved block/fluid global order).
- These are **requests and bounded native queue deliveries**, not the effect of
  executing `ServerLevel.tickBlock` / `FlowingFluid.tick` on a real server.
  Full neighbor-recursive world mutation, scheduled physics results, frozen
  chunks, save/reload and multi-chunk activation require real-server fixtures.
  Unsupported proxy methods remain fail-closed.

## Commands (fish / POSIX)

```sh
python3 tests/native_survival_inventory_test.py
python3 tests/native_survival_oracle_test.py
python3 tests/native_neighbor_trace_test.py

# Generate an owned temporary local fixture, not a tracked vanilla dataset.
python3 tools/native_support_inventory.py --scope all --output /tmp/mcspine-states.tsv

# Expensive full native canSurvive pass. May reveal unsupported native calls.
python3 tools/native_survival_oracle.py --scope all \
  --jar /path/to/pinned-1.21.1/server.jar \
  --output /tmp/mcspine-native-support.tsv

# Native callback requests, duplicate run, then native LevelTicks delivery.
python3 tools/native_neighbor_trace.py \
  --jar /path/to/pinned-1.21.1/server.jar \
  --output /tmp/mcspine-native-request.tsv

# Finally rebuild/regress the port on this exact git SHA.
cargo test --locked -- --test-threads=1
bash tests/verify.sh
```

Each generated local output should be retained by the operator with JAR hash,
commit SHA, scope, and SHA-256. Native matrices and traces are not checked in
before evaluating their provenance, reproducibility and licensing.

## Criteria before closing P1-1

- [ ] All 26,684 pinned states visited; 320,208 rows produced, 0 `U:` rows,
  native identities match the block property registry.
- [ ] Native 1.21.1 events and deliveries run twice with identical order and
  digests, all queued positions eligible and all recorded appointments delivered.
- [ ] Capture **paired port observations** at the same initial blocks, world
  seed, time and chunk eligibility. Compare write order, block state, tick
  identity/due/priority/sub-order and delivered world states. Fix the first
  divergence; do not compare only the final counts or a sorted set.
- [ ] Extend to more than 12 support contexts and real server environment
  (plants, rails, portals, multiblocks, light/biome/tag-sensitive supports).
- [ ] Capture actual callback recursion and tick *effects* in server, including
  save/reload, hot/cold chunks, wall/rail cascades, source fluid and waterlogging.
- [ ] Rust serial, parallel, wire, adapter and mutation gates green, with
  run-specific evidence on the exact PR SHA.

These tests are intentionally not named `parity_passed` until all final gates
are met. Existing bounded runtime implementations and supporting PR #33 are
still considered incomplete.
