# Whole-chunk native comparison

Reuse authenticated server capture and recorded-order replay. Find an actually
placing fossil scene before choosing a fixture. Match seed, dimension, template
resources, complete chunk status, source margin, target window and population
order. Capture native twice and measure its own variability. Freeze generation
before runtime; compare every full block state, heightmaps, initialization and
pending ticks. Then replay the same explicit runtime ticks and compare delivery
and resulting states separately. Name skipped chunks and unresolved data; an
empty/incomplete reference is a prerequisite failure, not zero differences.
Preserve existing captures and expected output. Native Java/helper and fake
adapter checks remain different evidence classes. Packet/light parity needs its
own decoded comparison, not a generator digest.

## What already exists (measured 2026-11-11)

A whole-chunk native comparison **is** in the tree, for the two dimensions whose terrain
does not need the Overworld's biome source:

| fixture | preset | seed | chunks | cells compared |
|---|---|---|---|---|
| `tests/data/dimension_terrain_1211/0-end.bin.gz` | End | 0 | 1×1 at (0,0) | 128 × 256 |
| `…/1-end.bin.gz` | End | −1 | 3×3 at (100,−103) | 128 × 256 × 9 |
| `…/2-nether.bin.gz` | Nether | 0 | 1×1 at (0,0) | 128 × 256 |
| `…/3-nether.bin.gz` | Nether | −1 | 3×3 at (−65,32) | 128 × 256 × 9 |

`worldgen::wg_tests::dimension_terrain_matches_all_native_fixture_states` compares
**655,360 cells** of `terrain_cell` against those bytes, and each fixture carries its own
`provenance.json` (server SHA-1, stage `terrain`, min_y/height, chunk counts, run hash,
`runtime_writes: 0`). So "no whole-chunk result" is too strong: End and Nether terrain are
covered at this stage.

## The gap, stated precisely

1. **Overworld terrain** has no native corpus. Its `terrain_cell` mixes biomes, the
   aquifer and the material rule — the three parts that only exist for the Overworld — so
   End/Nether parity does not transfer.
2. **Post-feature chunks** (structure starts, features, block entities) have no corpus in
   any dimension. Every capture above is `stage: terrain`, and its provenance records zero
   runtime writes, so nothing in the tree compares a *decorated* chunk.

## Why an Overworld corpus is not a local script

Generating a native chunk offline needs `ChunkGenerator.fillFromNoise(Blender, RandomState,
StructureManager, ChunkAccess)`, and `StructureManager` is constructed from a `Level`. That
is the same wall as `sign.setText` and the block-entity cast: `ProtoChunk` plus a
datapack `RegistryAccess` gets a chunk *constructed*, not *generated*. The existing End and
Nether fixtures therefore came from a real server run (`/data/lane-scratch/…` in their
provenance), not from the harness.

So the next step is a decision, not a coding task: run the pinned server once in an
isolated worktree with a fixed seed/window/stage and dump the chunk, accepting the cost of
a server start. The last unplanned launch left a save that could not be restored, so the
protocol has to be: isolated working directory, fixed input, capture two runs to measure
variability, and dump before any runtime tick.

## What the port must match when that capture exists

- The same biome source: the port's `NoisePreset::Overworld` sampling must be the same
  `MultiNoiseBiomeSource` the server used, or the comparison measures biome choice, not
  terrain.
- Stage `terrain` first (density → aquifer → material rule), then a separate
  `decorated` fixture, because a single "diff count" cannot say which stage diverged.
- Every full block state, plus heightmaps and pending ticks, with chunks that were skipped
  named rather than counted as equal.
