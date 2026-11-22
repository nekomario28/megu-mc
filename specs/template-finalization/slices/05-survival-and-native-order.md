# P1-1: exhaustive support inventory and ordered native event baselines

Status: **native and bounded Rust paired tests implemented; full P1-1 parity remains open**. A complete list of block states is NOT a
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
python3 tools/native_support_inventory.py --scope all --output /tmp/megu-states.tsv

# Expensive full native canSurvive pass. May reveal unsupported native calls.
python3 tools/native_survival_oracle.py --scope all \
  --jar /path/to/pinned-1.21.1/server.jar \
  --output /tmp/megu-native-support.tsv

# Native callback requests, duplicate run, then native LevelTicks delivery.
python3 tools/native_neighbor_trace.py \
  --jar /path/to/pinned-1.21.1/server.jar \
  --output /tmp/megu-native-request.tsv

# Finally rebuild/regress the port on this exact git SHA.
cargo test --locked -- --test-threads=1
bash tests/verify.sh
```

Each generated local output should be retained by the operator with JAR hash,
commit SHA, scope, and SHA-256. Native matrices and traces are not checked in
before evaluating their provenance, reproducibility and licensing.

## Recorded evidence (2026-11-07)

- [CI: Native/JVM and Rust pairing, run #42338550702](https://github.com/nekomario28/megu-mc/actions/runs/42338550702): 26,684 states in 12 **synthetic** contexts (320,208 rows, zero unsupported); 336 support decisions across 8 implemented fragile families matched Rust; 25 single-callback native state transitions matched Rust; native 8 appointments / 8 deliveries matched Rust's queue at identical time, identity, position and order. All native and dedicated Rust pairing jobs passed.
- Run #42338550702's **full repository verification failed** at the ignored-test debt ratchet, not at the native pairing tests. The source-layer dependency violation from the previous run was repaired, and 17/17 layering tests passed.
- The three fixture-only Rust tests now require the explicit Cargo feature \`native-oracle\`; the native CI supplies real JAR-generated TSVs. There are no added \`#[ignore]\` gates and the debt limit was **not** raised. Confirm the complete post-fix \`tests/verify.sh\` run before claiming a green full suite.
- The runtime still creates falling entities immediately on support removal while native FallingBlock.updateShape **schedules a 2-game-tick block callback**. This remains a known behavior mismatch; do not treat 8/8 delivery parity (which replays already-authored requests) as booking parity or full tick effects.

Reproduce the mandatory paired tests after collecting fixtures from the same JAR and commit:

\`\`\`sh
MC_NATIVE_SURVIVAL_MATRIX=/tmp/megu-native-support.tsv \
  cargo test --locked --features native-oracle native_survival_fragile_port_pair -- --nocapture
MC_NATIVE_NEIGHBOR_SHAPES=/tmp/megu-neighbor-shapes.tsv \
  cargo test --locked --features native-oracle native_neighbor_shape_port_pair -- --nocapture
MC_NATIVE_TICK_REQUESTS=/tmp/megu-native-request.tsv \
MC_NATIVE_TICK_DELIVERIES=/tmp/megu-native-request.delivered.tsv \
  cargo test --locked --features native-oracle native_level_ticks_port_delivery_pair -- --nocapture
\`\`\`

## Criteria before closing P1-1

- [x] All 26,684 pinned states visited; 320,208 rows produced, 0 `U:` rows,
  native identities match the block property registry **in the 12 synthetic scenes**.
- [x] Native 1.21.1 events and deliveries run twice with identical order and
  digests, all queued positions eligible and all recorded appointments delivered
  **in the isolated test harness**.
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
