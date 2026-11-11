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

## First Overworld capture, and the stage mismatch it exposed (2026-11-11)

A pinned 1.21.1 server was started once in an isolated directory (`/var/tmp/mc-native-overworld-20261111`,
seed 0, `level-type=normal`, no players) with a datapack that empties every biome's
`features` and `carvers` (`tools/nofeat_pack.py`; 2,545 placed-feature references and 264
carver entries dropped, the same counts the historical End/Nether capture recorded). It
generated its spawn chunks and was stopped, and chunk 0,0 was read back from the region
file with `tools/terrain_chunk_dump.py` — name-based, because the numeric id space has no
documented property-to-id order.

Result: 32,526 non-air cells, deepslate/stone/dirt/bedrock/water/grass and **no ores**, so
the pack did what it says.

Comparing it against the port's own dump of the same chunk (98,304 cells, every y) gives
**6,635 differences (6.75 %)** — and every one of them is a feature-placed block the port
bakes into its terrain dump, against a native world where features were stripped:

| native | port | cells |
|---|---|---:|
| stone | andesite / diorite / granite | 3,673 |
| deepslate | tuff | 1,417 |
| deepslate | gravel / dirt / sand | 760 |
| stone | coal_ore / iron_ore | 178 |
| … 25 more pairs | | |

So this is **not** a parity result, and it is worth recording precisely because the first
number looked like one: the two sides are at different stages. The port's `MC_SPINE_WG_DUMP`
is not terrain-only — it includes the blocks a feature pass places — while the native
world had every feature removed. Either the native capture must keep features, or the port
needs a stage-limited dump; comparing the current pair would measure the nofeit pack.

Next: run the same server **without** the stripper (features on) into a fresh directory,
capture twice to measure the server's own variability, and only then interpret a residual —
per dimension and per stage, never as one number.

## First Overworld parity measurement (2026-11-11)

Two pinned servers were run **with features on** (no stripper), seed 0, no players, 3-chunk
view distance, into separate directories. The second was launched before the first had
released port 25565, so it failed to bind and stopped without saving; that attempt and its
log are kept, and the run was repeated sequentially:

| run | cells | dump SHA-256 |
|---|---:|---|
| A | 32,518 | `af908b1a…` |
| B | 32,518 | `3e7749b0…` |

**A and B differ in 4 cells** — `(14,24,0)`, `(15,24,0)`, `(14,25,0)`, `(15,25,0)`, diorite
in one run and andesite in the other. So the native reference for this chunk is not
bit-reproducible, and 4 cells is the floor any comparison has to be read against.

Against that floor, the port's own dump of the same chunk:

| comparison | differing cells (of 98,304) |
|---|---:|
| port vs run A | **4** — exactly A's own noise, port agreeing with B |
| port vs run B | **0** |

That is the strongest whole-chunk result in this repository so far, and it is deliberately
narrow: one chunk, seed 0, Overworld, **block names only** — a cell that differs in a
property (stair facing, water level, log axis) is counted as equal here. Property-level
comparison needs the port to emit a canonical state string, which it does not have yet, so
this is a name-level result, not a full-state one. Structures and entities were not
compared, and one chunk says nothing about other seeds, biomes or the second capture window.

Everything needed to repeat it is in the tree: `tools/nofeat_pack.py` (stage-limited
capture), `tools/terrain_chunk_dump.py` (name-based dump), and the port's own
`MC_SPINE_WG_DUMP` (requires `MC_SPINE_WG_PROFILE`, else it refuses to run).

## The Overworld corpus is now in the tree, at full state level (2026-11-11)

`tests/data/overworld_chunk_0_0_1211.tsv.gz` (88 KB, 32,518 non-air cells, with a
`.provenance.json`) is that capture, and
`worldgen::wg_tests::overworld_chunk_matches_native_capture_state_for_state` regenerates the
chunk and compares **canonical state strings** — name plus every property, sorted — so a
stair facing the wrong way or a water level that is off now fails the gate. The port matches
all 32,518 cells, and two compiling mutants cover it: dropping the properties from the
canonical form, and replacing the deep-state fallback with the dimension default.

The name-only comparison above is kept in the record because it is what exposed the stage
mismatch; this test supersedes it as the gate.

Still true, and not claimed: one chunk, one seed, Overworld only, no structures/entities,
and the reference itself varies in 4 cells between runs (named in the provenance).
