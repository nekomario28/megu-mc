# Native feature contracts and the generation/tick boundary

## What is established

The normal release executable's `MC_SPINE_WG_DUMP` path, with
`MC_SPINE_POP_ORDER`, matches paused native 1.21.1 at every one of the 884,736
cells in the canonical 3×3 footprint. No leaf/property/runtime filter is used.
The canonical saved reference is unchanged and still differs from generation by
1,803 cells. A fresh **native** ticked control matches that canonical reference
exactly; successful last-writer stacks attribute all 1,803 changes.

The machine-readable [stage receipt](feature-contracts/stage-report.json)
records hashes, complete coverage, actual clock boundaries and writer counts.
It establishes generation parity with explicit source order, **not port
runtime parity**. Different native source schedules were observed in the two
arms; the actual final states and all cross-stage runtime writers are checked,
rather than treating this as a single-variable scheduler experiment.

## Why these implementations changed

Native probes execute actual pinned game bodies. Regression captures then pin
the independently justified result:

- Ore discard=1 checks buried exposure without an extra random draw; dungeon
  placement uses native solid-render predicates, protected writes and wrapped
  WorldgenRandom nextLong.
- WG heightmaps retain their configured kind and frozen generation lifetime.
  Live heightmaps still follow removals. Normal noise uses the actual modern
  creation path and caches by generator lifetime, not a reused address.
- SimpleBlock places double plants atomically; mushrooms use the fresh
  generation context. Matching-fluid predicates inspect FluidState, including
  waterlogged plants and source/flowing distinctions.
- Multiface growth preserves enabled directions, shuffle/draw order, spreading
  types and the native search origin.
- TreeFeature leaf updates are **not shortest paths**. They use seven buckets,
  a mutable depth cursor, visited voxels and Java HashSet traversal. FIFO changes
  98 of 117 native cases. The native HashSet also treeifies collision bins;
  linked-bin-only emulation was insufficient.
- Native bootstrap alone leaves block tags unbound. The shared
  `VanillaBlockTags` loader binds actual pack tags and asserts log/protection
  membership before tag-sensitive probes run.

The independent leaf suite contains 117 actual TreeFeature cases at three
absolute origins. Fifteen actual HashSet<BlockPos> operation vectors cover
interleaved removal/insertion, tree bins and resizing; Rust checks the
iteration-chain/tree structure after each operation. These run without Java
from the captured vectors in the normal Rust suite.

An equal full hash between distinct positions in a tree bin invokes native JVM
object-identity ordering. It is not a deterministic coordinate order. The port
fails explicitly rather than inventing a tie-break; a regression exercises that
guard. Whole-world acceptance must resolve or scope that remaining limitation.

## Ordered production path

`Gen::populate_in_order` owns claim-before-callback and exactly-once execution
for both lazy population and explicit replay. The dump validates a complete,
unique one-chunk source halo and preserves the supplied sequence. Duplicate,
missing, malformed and out-of-footprint entries fail; an already-populated
overlap also fails at the dump boundary. No coordinates from this fixture are
embedded in production scheduling.

The legacy `MC_SPINE_POP_REGION` scan and default lazy scan remain available,
but neither is represented as vanilla ticket scheduling. The two explicit
configuration modes are mutually exclusive.

## Reproduction

Use an authenticated bundler via `MC_SPINE_VANILLA_SERVER_JAR` and the
[bounded native capture driver](../../../tools/carve-agent/README.md).
Choose disk-backed `TMPDIR` when the host's tmpfs lacks space. Native probe
preflight refuses insufficient extraction space before writing members.

After host admission, capture two **new** output directories:

```sh
tools/carve-agent/native_fixture.sh "$FROZEN" -64 32 3 frozen
tools/carve-agent/native_fixture.sh "$TICKED" -64 32 3 ticked
```

The driver waits for real readiness, FULL chunks and save acknowledgement.
The paused arm must have unchanged game time and zero runtime writes.

With a freshly built release binary, inside the admitted bounded scope:

```sh
MC_SPINE_WG_PROFILE=0 MC_SPINE_SEED=0 \
MC_SPINE_POP_ORDER="$FROZEN/order.tsv" \
MC_SPINE_WG_DUMP="$PORT_DUMP" MC_SPINE_WG_DUMP_N=3 \
MC_SPINE_WG_DUMP_X=-64 MC_SPINE_WG_DUMP_Z=32 \
  target/release/mc-spine

python3 tools/forest_stage_check.py \
  "$FROZEN" "$TICKED" "$PORT_DUMP" "$UNCHANGED_CANONICAL_WORLD"
```

The checker re-reads saved worlds, verifies the dumps are not stale, rejects
incomplete/unmapped chunks and requires actual final runtime writers for every
cross-stage change. `tools/carver_diff.py` also counts all states with or
without its historical `--tick` flag; partial reference coverage exits 2.

## Remaining work

Native source order is currently supplied, not computed from vanilla tickets.
Port scheduled-leaf/fluid updates, pending tick ownership and timing still need
independent native contracts and same-stage post-tick validation. Structures,
other seeds/biomes and whole-world acceptance remain separate. This checkpoint
does not close M2/M3, authorize publication or integrate the divergent main.
