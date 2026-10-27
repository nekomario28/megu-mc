# Native feature contracts and the generation/tick boundary

## What is established

The normal release executable's `MC_SPINE_WG_DUMP` path, with
`MC_SPINE_POP_ORDER`, matches paused native 1.21.1 at every one of the 884,736
cells in the canonical 3×3 footprint. No leaf/property/runtime filter is used.
The canonical saved reference is unchanged and still differs from generation by
1,803 cells. A fresh **native** ticked control matches that canonical reference
exactly; successful last-writer stacks attribute all 1,803 changes.

The original [stage receipt](feature-contracts/stage-report.json) establishes
generation parity and identifies the native runtime writers. The newer
[port runtime receipt](feature-contracts/port-runtime-report.json) compares all
884,736 states after 700 calls to the actual `game_tick`: both an uninterrupted
run and a run saved after 5 ticks and resumed in another process for 695 ticks
have zero differences from the resumed native saved world. That native result
also matches the earlier native dump independently compared to the canonical
saved reference. Both port runs use the observed native population order and a
forced 3×3 footprint, so this is not a general ticket-scheduler claim.

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
The native-style fluid owner currently covers generated spring flows and the
full-cube/air/liquid cases in its recorded oracle; contact, partial collision
shapes, waterlogged leaves, other fluid producers and general dimensions remain
unimplemented or unverified. Unsupported native fluid scenes are deferred and
reported. The persisted tick state and source replay were checked on this
fixture, not under arbitrary concurrent generation and saves. Structures,
other seeds/biomes and whole-world acceptance remain separate. This checkpoint
does not close M2/M3 or authorize publication.

### Scheduled tick investigation

[Saved tick inventory](feature-contracts/saved-ticks.json) reads both serialized
tick lists in every FULL chunk and hashes their region files. In the existing
frozen capture, the 3×3 contains 2,885 leaf block ticks and 29 fluid ticks; the
7×7 contains 11,113 leaf block ticks and 88 fluid ticks. Other block ticks are
also present. The ticked capture's central 3×3 has empty saved tick lists.
These are different native captures with different decoration orders, not a
resume of the frozen capture. Saved delays are reported verbatim and list order
does not establish cross-chunk execution order or tick eligibility.

```sh
python3 tools/saved_tick_inventory.py "$FROZEN/world" -66 30 7 7
python3 tools/saved_tick_inventory.py "$FROZEN/world" -64 32 3 3 --include-entries
TMPDIR=/data/lane-scratch python3 tools/leaf_tick_oracle.py --jar "$MC_SPINE_VANILLA_SERVER_JAR"
```

The last command replays 172 vectors through the pinned game's actual private
`LeavesBlock.updateDistance` body, called by its scheduled `tick`. The vectors
cover six directions, log/leaf/other neighbours, persistent and waterlogged
leaves, and two neighbours at once. This is a distance calculation contract;
it does not execute `ServerLevel` tick, create pending ticks, or validate
runtime behaviour. Vanilla `ProtoChunkTicks` records one entry per type and
position; `LevelChunkTicks` maintains a priority queue. The existing port's
position-only, clamped, backoff-driven fluid queue has a different contract.
In a new [same-world resume](feature-contracts/resumed-stage-report.json), the
copied world's central 3×3 matches its frozen source at all 884,736 cells before
unfreezing; it then changes 1,803 cells with the same 1,763/40 last-writer
split. The source clock was 2, the resumed frozen clock was 4, and the final
clock was 704. This is evidence for the compared cells, not proof that all
surrounding state or ticket activation is identical across restart.

An opt-in trace on `ProtoChunkTicks` recorded 2,885 accepted leaf entries in
the central 3×3, all caused by tree shape-edge updates. A Rust generation run
using the traced native source order matched all 2,885 positions, block types,
and per-chunk insertion orders; its generated dump still matched all 884,736
native frozen cells. The independent native tree-edge oracle exercises 39 dry
leaf scenes, including 38 with scheduled entries. The port stores the pending
leaf entries in the generator and transfers them to the runtime owner. In a
separate replay of native execution traces, the leaf distance calculation
matched 774 recorded central-chunk native writes; this is not a port test.

Runtime tick queues use native-captured ordering, deduplication and eligibility
contracts; their native oracle has 48 cases. The 28 recorded fluid scenes use
the actual native tick code. The runtime comparison above provides a separate
end-to-end check for the stated footprint. It does not establish arbitrary
world or workload parity.

The receipt checker can be rerun against the preserved scratch captures:

```sh
python3 tools/port_runtime_stage_check.py \
  /data/lane-scratch/mc-tick-trace-1027 \
  /data/lane-scratch/mc-tick-runtrace-1027 \
  /data/lane-scratch/mc-parity-final-full-1027 \
  /data/lane-scratch/mc-parity-final-early-1027 \
  /data/lane-scratch/mc-parity-final-reload-1027 \
  /home/yuu/.cache/mc-spine-work/r1-ticked-native/native.bin \
  /home/yuu/.cache/mc-spine-work/r1-stage-report.json \
  target/debug/mc-spine
```
