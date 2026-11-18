# Village taiga step 4: the (n+4)² promotion footprint, and the fluid arm that was refusing plants

Base `798f645` on `perf/worldgen-parallel`. Raw logs, dumps, the decompiled classes behind every
vanilla claim and the two scan scripts used here:
`mc-spine-local/audit/village-3-20261118T064633/`.

**Result: `3 / 884,736` -> `1 / 884,736` on both captures, zero new cells.** Two of the three open
cells close; the third is attributed below with the evidence that says why it cannot close from the
port side without a new native instrument.

## 1. What landed

| change | file | measured by |
|---|---|---|
| the promotion region is native's `FULL` demand region, `(n+4)²` for an `(n+2)²` plan: a promotion candidate is clipped to the plan window plus one ring | `worldgen.rs::promotion_region_contains`, `feature.rs::promote_full_chunks` | both captures (§2), a mutation (§6) |
| a 3x3 neighbour outside the recorded plan counts as `FULL` instead of blocking the promotion | `worldgen.rs::promotion_window_ready` | both captures, `a_recorded_plan_window_answers_for_the_neighbours_it_does_not_cover`, a mutation (§6) |
| the population plan carries its own window (`PopulationPlan`), installed by `Gen::populate_plan` | `worldgen.rs`, `main.rs` | the two tests above; the default path is untouched (§3) |
| a fluid replaces a cell whose collision **shape is empty** (`short_grass` and the rest of the `canHoldFluid` families), sideways and below | `flowing_fluid.rs::Cell::Passable`, `template_structures.rs::holds_fluid` | the two measured water cells (§2), two fluid tests, a mutation (§6) |

## 2. The two water cells: `(98,19,-512)` and `(99,19,-512)`

| measurement | before | after |
|---|---|---|
| `matrix-v1/nat-4242-village-taiga` 3x3, recorded order | 3 / 884,736 | **1 / 884,736** |
| `t6b/nat-taiga-mlight` 3x3, recorded order | 3 / 884,736 | **1 / 884,736** |
| seed-0 `MC_SPINE_WG_DUMP_N=8` (the identity row) | `670fdfe3…` | **`670fdfe3…` (byte-identical, 25,165,824 B)** |
| `tools/fixture_suite.py …/manifest-promotable.json` | 8 MATCH | **9 MATCH, 0 DIFF, 0 INVALID, 0 NOT_RUN** (the 9th row is the jungle-temple lane's) |
| `cargo test --offline --locked -j 4 -- --test-threads=8` | 694 passed; 1 ignored | **698 passed; 0 failed; 1 ignored** |

The mechanism is two halves, and both were needed:

1. **The footprint.** `ChunkMap` (`aqb`) promotes a chunk on the load path only:
   `getChunkRangeFuture(pos, 1, $ -> ChunkStatus.FULL)` followed by
   `LevelChunk.postProcessGeneration` (`dvi.H` — the jar's **only** call site of it, a
   `duy.e(Ljd;)V`-style reference scan over the 6,148 classes). So a chunk's marks are consumed
   after its whole 3x3 has reached `FULL`. `FULL`'s accumulated dependency array, computed from
   `ChunkPyramid.GENERATION_PYRAMID` (`dvy.a`) and `ChunkStep.Builder`'s radius array (`dwc$a`), is
   `[light@0, initialize_light@1, carvers@2, biomes@3, structure_starts@4..10]`: a chunk needs *its
   own* `FEATURES` (through `light@0`) but only *earlier* stages of its neighbours. The `FULL`
   region is therefore the request plus two rings while the decorated region is the request plus
   three — and the capture agrees: a 3x3 request at `(4,-32)` holds **49 `minecraft:full` chunks in
   `(2..8) x (-34..-28)`** with zero pending `PostProcessing`, while the recorded plan covers the
   5x5 `(3..7) x (-33..-29)`.

   The port's old rule (`shape_marks_ready` = the whole 3x3 is in `pop_done`) is that same timing,
   but it asked for the neighbours outside the recorded plan as well — and the plan is a *narrowed
   input*: native decorated them, this model did not. So chunk `(6,-33)` (the chunk holding the
   marked source at `z = -513`) could never promote. The fix states the two regions the jar states:
   a candidate must lie in the plan window plus one ring, and a 3x3 neighbour outside the plan
   counts as `FULL`. Every window of a promotion-region chunk is inside native's decorated region
   (`promotion window + 1 = (n+6)²` = the request plus three rings), so the assumption is bounded by
   the same proof that produces the region.

   Under the same recorded order this promotes all 25 plan chunks (and the 9 ring chunks of the
   demand region that have data), where the port promoted 9: `MARKPASS 6 -33 marks=477` against the
   lane-3 hand-written 12-chunk order's 471, the difference being the marks filed by the two chunk
   passes that order did not run.

2. **The fluid arm.** With `(6,-33)` promoted, the tick at the marked source `(98,19,-513)` still
   aborted: `FlowingFluid.getNewLiquid` reads every horizontal neighbour's fluid state, and
   `(100,19,-513)` is `minecraft:short_grass` in this port (measured with `MC_SPINE_WG_DUMP_CANON`;
   water in native). `flowing_fluid` refused any non-solid, non-air cell. Vanilla's gate is
   `canPassThrough` (the two **collision shapes** do not intersect — an empty shape cannot
   intersect) plus `canHoldFluid` (`epc.a(dcc,jd,dtc,epd)`: `SimpleWaterloggedBlock` first, then
   `DoorBlock`, `#minecraft:signs`, `ladder`, `sugar_cane`, `bubble_column`, `nether_portal`,
   `end_portal`, `end_gateway`, `structure_void` are refused by name, everything else answers
   `!state.blocksMotion()`), and `spreadTo` then destroys the block and writes the fluid. So a cell
   with an empty collision shape is one a fluid may enter; a cell with a *partial* shape stays
   refused, because its answer depends on the two shapes' geometry.

   That is the arm, and it closes both cells: the source at `(98,19,-513)` now writes
   `water[level=1]` into `(98,19,-512)` and the one at `(99,19,-513)` into `(99,19,-512)`.

## 3. What the footprint change does *not* touch

The default population discipline (`MC_SPINE_POP_ORDER` unset) decorates native's whole decorated
region, so `pop_done` alone already answers the promotion question there — `promotion_window_ready`
has no window to narrow and `promotion_region_contains` has no region to clip. That is why the
seed-0 N=8 identity dump stays byte-identical: it runs the default path. The window exists only for
a recorded plan, and only a plan's boundary can be a narrow window.

## 4. The one remaining cell: `(82,69,-500)`, `air` in native against `fern` here

Native's own trace says the cell was **marked**: the write is
`RUNTIME 82 69 -500 state=0 wrote=true` with
`stack = dcw.a(267) | dcw.a(217) | dvi.H(539) | aqb.a(692) | …`, i.e. `LevelChunk.postProcessGeneration`'s
`setBlock(pos, state, 20)` — `H` only ever walks marked positions. (The two water cells show the
same frame at `dvi.H(534)`, one branch up: `epc`/`epe` = `WaterFluid`/`FluidState` under the fluid
branch.) The substrate at pass time is `dirt_path` (state 12513, `RUNTIME 82 68 -500 … wrote=false`),
which is outside `#minecraft:dirt`, so `BushBlock.updateShape` (`dgh.java`) answers air — the rule
the step-3 lane measured and reverted from the *shared* `update_shape` (§5 of
`village-taiga-promotion-pass-20261118.md`).

What is missing is the **mark**, not the rule:

| instrument | result at `(82,69,-500)` |
|---|---|
| `MARK_TRACE=all` on the village 3x3 (8,551 marks, 32 promoted chunks) | no line for the position |
| `FWRITE 82 * -500` (feature writes, whole column) | `69` = the fern (state 2006, by a `patch_taiga_grass`-class feature of chunk `(4,-32)`), `68` = `dirt_path` (piece), `67` = stone (carver top material). **No disk write anywhere in the column.** |
| the jar's nine `markPosForPostprocessing` owners | port files six (`write`, `piece`, `fill`, `surface`, `carver`, `multiface`); the two it does not are `Feature.markAboveForPostProcessing` (`ece.a(dds,jd)`, whose only callers are `DiskFeature` and `LakeFeature` — two call sites, so the old "no caller in the pinned jar" comment was wrong and is corrected) and `Blender` (needs blending data this version never writes) |

`Feature.markAboveForPostProcessing` marks `pos.above()` and `pos.above(2)`, stopping before an air
cell, so it would need a disk write at `y = 67` or `68` of that column to reach `69` — and there is
none in the port. Two sources therefore remain possible and the existing capture cannot separate
them: (a) native's disk placement differed at that column (a placement divergence upstream of
marks), or (b) a mark source neither the port's six nor the two named here. Deciding between them
needs a native run with a `ChunkAccess.markPosForPostprocessing` hook logging `(position, caller)`,
which is a new capture — out of scope for this lane, and the reason the cell is left standing rather
than guessed at.

The removal rule itself is a separate, smaller task for whoever lands that instrument: `dgh`'s
`!canSurvive -> air` with `mayPlaceOn = #minecraft:dirt || farmland`, scoped to
`postprocess_shape_marks` and **not** to the shared `update_shape` (landing it on the shared one
costs 232 diffs; measured in step 3 and not repeated here).

## 5. Gates

| gate | result |
|---|---|
| `tools/port_triage.py` `matrix-v1/nat-4242-village-taiga` 3x3, `--pop-order … recorded` | **1 / 884,736** |
| `tools/port_triage.py` `t6b/nat-taiga-mlight` 3x3, `--pop-order … recorded` | **1 / 884,736** |
| seed-0 N=8 dump (`670fdfe3472812af…`, 25,165,824 B) | **byte-identical** |
| `cargo test --offline --locked -j 4 -- --test-threads=8` | **698 passed; 0 failed; 1 ignored** |
| `tools/fixture_suite.py …/manifest-promotable.json` | **9 MATCH, 0 DIFF, 0 INVALID, 0 NOT_RUN**, exit 0 |
| `bash tools/layers_check.sh` | **PASS** (44 modules, 111 edges, 0 named errors) |
| `bash tools/debt_ratchet.sh` | **0 over limit** |
| `MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline` (filtered, 2 entries) | **2 proved, 0 not a gate** (registry 480 -> 482) |

Mutations registered: "a recorded plan's window is native's decoration, not this model's"
(`!in_window(c) || pop_done.contains(&c)` -> `pop_done.contains(&c)`) and "a fluid replaces a cell
whose collision shape is empty" (`Cell::Passable => put(fluid)` -> `Ok(())`). The classification
itself (`holds_fluid`) is pinned by `the_pass_sees_an_empty_collision_shape_as_a_cell_a_fluid_can_hold`
but has no registered entry: two entries was the lane's budget, and the machine arm is the one the
measured cells exercise.

## 6. Reproduce

```sh
CAP=/home/yuu/mc-spine-local/captures/matrix-v1/nat-4242-village-taiga
tools/port_triage.py "$CAP/world" 4 -32 --n 3 --pop-order "$CAP/order.tsv" \
  --pop-order-scope recorded --binary target/debug/mc-spine          # 1 / 884736

# which chunks promote, and with how many marks
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/v.bin MC_SPINE_WG_DUMP_N=3 \
MC_SPINE_WG_DUMP_X=4 MC_SPINE_WG_DUMP_Z=-32 MC_SPINE_POP_ORDER="$CAP/order.tsv" \
MC_SPINE_POP_ORDER_SCOPE=recorded MARK_TRACE=all ./target/debug/mc-spine | grep MARKPASS

# the cell the fluid machine used to refuse
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/c.bin MC_SPINE_WG_DUMP_N=1 \
MC_SPINE_WG_DUMP_X=6 MC_SPINE_WG_DUMP_Z=-33 MC_SPINE_WG_DUMP_CANON=/tmp/c.canon \
MC_SPINE_POP_ORDER=/home/yuu/mc-spine-local/audit/village-taiga-closure-20261118T114736/order-c6m32-ring.tsv \
MC_SPINE_POP_ORDER_SCOPE=recorded ./target/debug/mc-spine
awk -F'\t' '$2==19 && $3==-513' /tmp/c.canon     # 100 = short_grass, 98/99 = water
```
