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

## Historical gap before the Overworld captures

The two gaps below describe the pre-capture state, not the current corpus. The later
canonical-state seed-0 section supersedes the decorated-Overworld gap.

1. **Overworld terrain** has no native corpus. Its `terrain_cell` mixes biomes, the
   aquifer and the material rule — the three parts that only exist for the Overworld — so
   End/Nether parity does not transfer.
2. **Post-feature chunks** (structure starts, features, block entities) have no corpus in
   any dimension. Every capture above is `stage: terrain`, and its provenance records zero
   runtime writes, so nothing in the tree compares a *decorated* chunk.

## Historical capture prerequisite: an actual Level

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

## Acceptance requirements, including future captures

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
carver entries dropped, the same counts the historical End/Nether capture recorded).

The pack first declared `pack_format 34`, which is 1.21.1's **resource**-pack format; the
data format is 48. It still loaded (the log says "Found new data pack file/nofeat, loading
it automatically", and the captured chunk had no ores), but the declaration was wrong and is
now 48 with a test, after the same fix appeared in another session's draft. It
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
number looked like one: the two sides are at different stages. The port's `MEGU_WG_DUMP`
is not terrain-only — it includes the blocks a feature pass places — while the native
world had every feature removed. Either the native capture must keep features, or the port
needs a stage-limited dump; comparing the current pair would measure the nofeit pack.

Next: run the same server **without** the stripper (features on) into a fresh directory,
capture twice to measure the server's own variability, and only then interpret a residual —
per dimension and per stage, never as one number.

## Historical name-level Overworld measurement (2026-11-11)

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
bit-reproducible in those runs. This observed variability is not a universal floor or
a tolerance mask for later comparisons.

For those two named references, the port's own dump of the same chunk:

| comparison | differing cells (of 98,304) |
|---|---:|
| port vs run A | **4** — exactly A's own noise, port agreeing with B |
| port vs run B | **0** |

That historical observation is deliberately narrow: one chunk, seed 0, Overworld,
**block names only** — a cell that differs in a
property (stair facing, water level, log axis) is counted as equal here.
At this historical point, property-level comparison still needed a canonical-state dump. The later section implements that gate;
this earlier result remains name-level evidence, not a full-state one.
Structures and entities were not compared, and one chunk says nothing about other seeds, biomes or the second capture window.

The historical measurement used the tooling now in the tree: `tools/nofeat_pack.py` (stage-limited
capture), `tools/terrain_chunk_dump.py` (now a canonical-state dump), and the port's own
`MEGU_WG_DUMP` (requires `MEGU_WG_PROFILE`, else it refuses to run).

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

## A second seed found what one chunk could not (2026-11-12)

A capture at seed 4242, chunk (3,−2) was compared at canonical-state level against the
port. Two things came out of it, in this order:

1. **My own dumper was wrong.** `terrain_chunk_dump.py` emitted *local* coordinates, which
   are also absolute for chunk (0,0) — the seed-0 result was right by coincidence and the
   first 4242 comparison reported 64,592 differences in a perfectly symmetric pattern. Fixed
   (absolute coordinates), and the seed-0 fixture was verified byte-identical afterwards.
2. **A snowy-state path changed the measured residual.** With coordinates fixed, the
   original comparison had 302 differences, including 238 native `snowy=true` grass cells
   that were false in the port. `Gen::apply_snowy` now consults the blended biome and its
   base temperature; the test `snowy_grass_follows_the_biome_temperature` pins that port
   behavior. It does **not** independently prove this is the native SnowyDirtBlock contract.
   The existing `Cf::FreezeTop` snow/ice placement path is another owner to compare.

After that change, the reported residual is **82 cells in four state-pair groups**:
32 air/stone, 30 diorite/granite, 18 snowy false/true, and 2 oak_planks/air.
The snowy discrepancy is not yet attributed to an incorrect biome lookup: the lookup
position, upper/lower cell, stage and writer must first be compared with native.

Seed 0's named capture matches exactly; seed 4242 remains an exploratory comparison,
not a second committed acceptance fixture. Counts are retained instead of a percentage
that mixes non-air row counts with the full chunk volume.

## The 82 residual cells: observations, not established causes (reviewed 2026-11-13)

The second native run differs from the first in **23 cells**, all diorite → andesite.
The retained port dump differs from run A in **82 cells** and from run B in **105**.
These are observed run pairs, not a justified general noise model or an allowed mask.
Keep both references and all mismatches visible.

| cells vs A | observed bounds | native → port | cause status |
|---:|---|---|---|
| 32 | x=63, y=5..12, z=-30..-17 | air → stone | Unknown writer/stage; final air does not establish a carver origin |
| 30 | x=52..56, y=19..22, z=-19..-17 | diorite → granite | Ore-type/order hypothesis; not proved by shape or counts |
| 18 | x=50..59, y=64..65, z=-31..-20 | grass snowy=false → true | Lookup position, biome, temperature, stage and writer remain to compare |
| 2 | x=62..63, y=21, z=-17 | oak_planks → air | Structure-origin hypothesis; the responsible native piece/writer is not identified |

### Retraction of the previous causal diagnosis

Earlier revisions suggested a missing carver margin, then missing neighbour seeds,
then a biome boundary that selected a different carver list for the air and snowy cells.
These are not established explanations:

- The port already walks the 17×17 neighbour seed region. Existing folding/clipping code
  rules out merely **absent** neighbour iteration; it does not prove the whole carver path.
- Both `evidence/foundation/biome-carvers/vanilla.json` and the installed biome JSON group
  all **53 Overworld biomes** under the same ordered list:
  `cave, cave_extra_underground, canyon`. A normal Overworld biome misassignment alone
  cannot change that list.
- `Gen::biome_name_at`, used for carver selection, is quart-sampled at neighbour corners
  at y=0. `Gen::apply_snowy` uses `biome_blended` at a surface block; `Cf::FreezeTop`
  separately selects a biome at its heightmap position and places snow/ice.
  Comparing those distinct queries as though they were one stimulus hides the stage boundary.
- Sharing a snow-covered z band does not prove one causal boundary. No paired native
  lookup or writer trace has yet established that the 32+18 cells have one cause.

The previous claim of “four named causes” is therefore withdrawn, while the observations,
earlier commits, original captures and expected states are preserved. No implementation
is changed merely to fit that diagnosis.

## Next bounded measurement — 2026-11-13 実施結果と残件

[調査手順](../../worldgen-parity/investigation-workflow.md) に従い、seed 4242 chunk `(3,-2)` の 82 セル差分（全空気種別では 101 セル）を分解・検証した（詳細: [seed-4242 診断記録](../../../evidence/gameplay/feature-parity/seed-4242-chunk-3-m2-20261113.md)）。

1. **18 セルの `snowy` 残差は解消 (`0` 残差):**
   旧 `Gen::apply_snowy` による surface 事後書き換えは、上に `short_grass`/`poppy`/`dandelion` がある `grass_block` を誤って `snowy=true` にしていた（18/18 例すべて両 native run で同じ植物が直上）。これを撤去し、[FreezeTopLayerOracle.java](../../../tools/structure-oracle/FreezeTopLayerOracle.java)（`edn` 29 場面）に一致する `Cf::FreezeTop` + `snow_can_survive`（`MotionBlocking` heightmap、温度・光・`snow_layer_can_survive_on`）へ一本化した結果、`238/238` の `snowy=true` を保ったまま `snowy` 差分は **0** になった（コンパイルする 4 件の mutant を含む）。
2. **残り `30 diorite→granite` と native A/B 間 `23 andesite→diorite` は population 順で説明済み:**
   `FEATURE_CELL_TRACE` と `MEGU_POP_ORDER` により、`(3,-1)`・`(2,-3)` と `(3,-2)` の生成順入れ替えだけで A/B ともに ore 差分は **0** になることを確認。
3. **残る `34` 非air セル（全空気種別では `53` セル）は未実装 `minecraft:mineshaft`:**
   `c32['structures']['References']['minecraft:mineshaft']` の開始 chunk `(4,3)` に属する `child 85 mscorridor` / `child 86 msstairs` / `child 185 mscorridor` が交差領域（`51/53` は piece BB 内の `cave_air` 掘削、残り `2` は `child 185` の `y=21` 支持 `oak_planks`）と一致する。

Seed 4242 は `minecraft:mineshaft`（P2-1）と生成順固定が未完了のため引き続き探索資料とし、committed acceptance fixture にはしない。Heightmaps, block entities, pending ticks, runtime, packet/light and positive Fossil/structure acceptance remain separate open gates.

## Local artifact locations

On the reorganized host, source is `~/megu`; retained captures are under
`~/megu-local/captures/native/`, TSV exports under `captures/exports/`, and audit
records under `~/megu-local/audit/historical/save-booking-20261110/`.
Historical temporary paths above record where captures originally ran; their compatibility
aliases have been retired. The local `~/megu-local/inventory/` maps old paths to new ones.
Native worlds/jars and exploratory dumps remain local; this document does not authorize
their redistribution or change tracked fixture expectations.
