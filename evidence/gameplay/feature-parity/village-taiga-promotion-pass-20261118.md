# Village taiga step 3: the write-time brightness, the mark set the port was missing, and the fluid tap

Base `61bcf63` on `perf/worldgen-parallel`. Raw logs and the decompiled classes used for every
vanilla-side claim: `mc-spine-local/audit/village-taiga-closure-20261118T114736/`.

**Result: `4 / 884,736` -> `3 / 884,736` on both captures.** One cell closes. The other three are
still open, and this page states what each one needs from the measurements rather than from
attribution — one of them needed a rule that this lane measured to be *regressing* and did not
land (§5), and two need a chunk outside the compared window to be promoted (§4).

## 1. What landed

| change | file | measured by |
|---|---|---|
| `uninitialized_raw_brightness` answers `0`, not `15`, in a sky dimension | `generation_providers.rs` | two captures (§2), `brightness-verdict-20261118T110138` (34/34 native), a mutation (§6) |
| the fill files `markPosForPostprocessing` on a settled aquifer fluid | `worldgen.rs::terrain_cell_marked` | `the_fill_marks_a_settled_aquifer_fluid_and_nothing_else`, a mutation (§6) |
| the surface updater files the same mark when its own write carries a fluid | `worldgen.rs::terrain_cell_marked` | mark-set breakdown (§3) |
| `WorldCarver`'s two mark sites (`apply`, and the restored top material under a cave mouth) | `feature.rs::carve_block` | mark-set breakdown (§3) |
| `MultifaceGrowthFeature` / `MultifaceSpreader` mark the growth cell | `feature.rs` | mark-set breakdown (§3) |
| the promotion pass ticks a marked fluid cell (`LevelChunk.postProcessGeneration`'s `fs.tick`) | `template_structures.rs` + `main.rs` seam | `postprocess_ticks_a_marked_source_fluid_into_its_neighbour`, a mutation (§6) |
| terrain marks travel with the worker's rows, so the mark set does not depend on `MC_SPINE_WG_GEN_JOBS` | `worldgen.rs` (`BaseRow`, `file_terrain_marks`) | `full8` dump byte-identical at jobs 1 and 8 (§3) |

`Aquifer.shouldScheduleFluidUpdate()` (`dyk$c.w`) is now a field on `Gen`, set at every return of
`aq_state` exactly where `computeSubstance` sets `this.w`: `false` for `density > 0`, for a disabled
aquifer, for the lava arm and for each blend that wins; `sim12 >= k` for the settled arm; `true` for
water over lava and for a fluid that survives the blends. `k` is
`1 - |144 - 100| / 25 = -0.76` (`k = a(square(10), square(12))`, same constant the MIT Horizon pin
carries as `FLOWING_UPDATE_SIMILARITY`, `crates/horizon-world/src/worldgen/aquifer/noise_based.rs:13`).

## 2. The brightness: `4 -> 3`, and the identity row does not move

| measurement | before | after |
|---|---|---|
| `matrix-v1/nat-4242-village-taiga` 3x3 | 4 / 884,736 | **3 / 884,736** |
| `t6b/nat-taiga-mlight` 3x3 | 4 / 884,736 | **3 / 884,736** |
| seed-0 `MC_SPINE_WG_DUMP_N=8` (the identity row) | `670fdfe3…` | **`670fdfe3…` (bit-identical)** |
| seed-0 N=8 at `MC_SPINE_WG_GEN_JOBS=1` vs `8` | — | **byte-identical, `670fdfe3…` both** |

The closed cell is `(99,68,-502)`: `patch_brown_mushroom`'s taiga write is now accepted at write
time (measured native read: 0, not 15), and the promotion pass keeps it.

`generation_contract_tests::mushrooms_in_fresh_generation_need_the_special_substrate` pinned the
retracted model ("the native fixture's uninitialized sky light reads 15 even under trees"). It is
rewritten to the measured one: at `FEATURES` time the brightness half of `MushroomBlock.canSurvive`
is satisfied in every dimension, so the *substrate* half decides (`isSolidRender`, with `glass` and
`oak_leaves` as the false side), and removal is the promotion pass' job.

## 3. The mark set: four of the nine sources, and what the trace says

`MARK_TRACE` now prints the pass' input per promoted chunk with the writer that filed it
(`MARK x y z chunk=cx,cz src=fill|surface|carver|write|multiface`).

Village 3x3, `MARK_TRACE=all`: **3454 marks consumed** by the five chunks that reach `FULL`.

| state at pass time | cells |
|---|---|
| `minecraft:water` | 3427 |
| `minecraft:magma_block` | 14 |
| `minecraft:glow_lichen` | 10 |
| `minecraft:brown_mushroom` | 3 |

The reference's 106 RUNTIME lines are the same *kind* of record but a **filtered view**: the two
chunks it compared were at `minecraft:full`, and the capture's own proto chunks show what the fill
really files. `PostProcessing` in the capture's `mc-native` world:

| chunk status | marks in `PostProcessing` |
|---|---|
| `minecraft:carvers` (40 chunks) | up to **2045 per chunk** (chunk `(10,-32)`: 2045; the status total is 4927) |
| `minecraft:initialize_light` (32 chunks) | up to **2595 per chunk** (chunk `(9,-32)`: 2595; the status total is 6721) |

So the reference files thousands of marks per chunk at the surface/carver stages, and the 106 lines
are the cells whose *state changed* in the box the runtime hook was scoped to. The port's 3454 is
the honest count of what its own mark set contains; it is not a 30x over-mark against native.

**`StructurePiece.placeBlock` on the jigsaw/template path is NOT a source here, measured.** The
window holds 22 `SHAPE_CHECK_BLOCKS` cells (18 `torch`/`wall_torch`, 2 `ladder`, 2 `spruce_fence`)
and **none** of them is in the reference's 106 RUNTIME lines. A bytecode scan of the pinned jar for
`duy.e(Ljd;)V` call sites returns exactly the nine owners the earlier page listed
(`arb`, `dkz$b`, `dzb`, `dzq$1`, `eaa`, `ebe`, `ece`, `ecw`, `ejv`), so the template path writes
through `arb` only (which marks on `hasPostProcess` alone) and **no code was added for it**.

## 4. The three open cells, and what each needs

| cell | native | port | what is missing, measured |
|---|---|---|---|
| `(98,19,-512)` | `water[level=1]` | `air` | a promotion of chunk `(6,-33)` (§4.1) and a neighbour the fluid machine can read (§4.2) |
| `(99,19,-512)` | `water[level=1]` | `air` | same |
| `(82,69,-500)` | `air` | `fern` | the position is never marked by any of the nine sources (§5) |

### 4.1 The marked source is at `z = -513`, i.e. in a chunk the dump does not promote

The fluid branch is proven: with `MARK_TRACE`, `(98,19,-513)` and `(99,19,-513)` are marked
`src=fill` (the fill writes the aquifer's water there and the flag holds), and the tick is attempted
at both. But those cells belong to chunk `(6,-33)`, and a 3x3 dump at `(4,-32)` populates the 5x5
ring `(3..7) x (-33..-29)`, so `(6,-33)` never satisfies `shape_marks_ready` (it needs
`(5..7) x (-34..-32)`) and is never promoted. Its marks are filed and never consumed.

Measured with a hand-written 12-chunk order that *does* complete that 3x3
(`(5..7) x (-34..-31)`, dump at `(6,-32)`): `MARKPASS 6 -33 marks=471` — the chunk promotes, the
tick runs at the two source cells, and it stops on `(100,19,-513)` (a plant in the port, `water` in
native). That cell is outside the compared window, so the fixture cannot see the divergence that
blocks the spread; `flowing_fluid` refuses a non-full collision shape by contract
(`flowing_fluid.rs`, "Non-full collision shapes and cross-fluid contact require additional native
shape/contact contracts") and the pass leaves the cell alone rather than guess.

Closing these two cells therefore needs three things this lane does not own: a population footprint
that reaches `(n+4)^2` (native's `FULL` region is 7x7 for a 3x3 request — its capture holds 49
`minecraft:full` chunks), the out-of-window terrain divergence at `(100,19,-513)` attributed, and a
non-full-shape arm in `flowing_fluid`.

### 4.2 The fluid tap

`structure_template` cannot import `flowing_fluid` (Layer 3 vs Layer 4), so the pass takes a
registered `FluidTicker` (`register_fluid_ticker`, the same shape as
`feature::register_beard_resolver`) and `main` registers `promotion_fluid_tick`, which drives
`flowing_fluid::tick` with `expected_source = (level == 0)` and `source_conversion = water`.
`FluidState.tick` is skipped when the state's fluid is empty; the cell's own `st` is read **before**
the tick and both branches use that stale state, exactly as `dvi.H` does. `FLUID_TICK_TRACE=1`
reports every refusal with the cell the machine stopped on — position only, because naming the
block from inside `worldgen::with_gen` re-enters the generator lock (measured: that call deadlocked
a diagnostic run in `futex_wait`).

## 5. `BushBlock.updateShape`: measured, implemented, and **reverted**

The rule is `!state.canSurvive(level, pos) -> air` with
`mayPlaceOn = state.is(BlockTags.DIRT) || state.is(Blocks.FARMLAND)` (`dgh.java:12-20`). Of the
nineteen direct `dgh` subclasses in the pinned jar, only `TallGrassBlock` (`doe`), `FlowerBlock`
(`dja`) and `SaplingBlock` (`dmn`) inherit that body unchanged — the other sixteen override
`mayPlaceOn`, `canSurvive` or `updateShape` (`DoublePlantBlock` pairs halves, `DeadBushBlock` wants
sand, `CropBlock`/`StemBlock` want farmland and a growth stage, `WaterlilyBlock` wants water).
`#minecraft:small_flowers` is exactly the fourteen `FlowerBlock`s; `#minecraft:flowers` is not (it
also holds tall flowers, leaves, azalea, `pink_petals`, `chorus_flower`, `spore_blossom`) and
`#minecraft:saplings` is not either (it holds `azalea`, `flowering_azalea`,
`mangrove_propagule`).

Two measurements decided it:

1. **Reachability from the pass: zero cells.** `POSTPROC_STATES=1` over the village 3x3 lists every
   consumed mark's state at pass time — 3427 `water`, 14 `magma_block`, 10 `glow_lichen`,
   3 `brown_mushroom`, and **no bush family at all**. `(82,69,-500)` itself is never marked: the
   fill's answer there is `air` (`--worldgen-cell overworld 4242 82 69 -500`), the carver's writes
   in that column are `y = -7..-2`, no region write touches it before the fern, and the piece-mark
   path is measurably absent (§3).
2. **Landed on the shared `update_shape`, it regresses the fixture.** With the rule in
   `update_shape`, village-taiga goes **3 -> 232** diffs, all inside one `village_taiga` jigsaw
   piece `[86..96, 68..76, -512..-503]`: a whole spruce (log and canopy) moves, because
   `apply_shape_passes` also runs at *placement* time and the port's edge pass visits plant cells
   whose substrate is written later in the same pass. Reverted; the removal of the 232 was verified
   by rebuilding with the rule out.

So the rule must be scoped to the promotion pass (`postprocess_shape_marks`), not to the shared
`update_shape` the placement edge pass calls. That is the next lane's first task, and it becomes
observable only once a mark can reach a plant cell (i.e. after §4's footprint).

## 6. Gates

| gate | result |
|---|---|
| `tools/port_triage.py` `matrix-v1/nat-4242-village-taiga` | **3 / 884,736** (`--n 3 --pop-order … recorded`) |
| `tools/port_triage.py` `t6b/nat-taiga-mlight` | **3 / 884,736** |
| seed-0 N=8 dump, `MC_SPINE_WG_GEN_JOBS=1` vs `8` | **byte-identical**, `670fdfe3472812af…` (the pinned identity hash) |
| `cargo test --offline --locked -j 4 -- --test-threads=8` | **668 passed; 0 failed; 1 ignored** |
| `tools/fixture_suite.py …/manifest-promotable.json` | **8 MATCH, 0 DIFF, 0 INVALID** (no promotion: village stays `DIFF` at 3, so `finite-acceptance-set.md` §6 is unchanged) |
| `PATH=…/gxkit/.venv/bin:$PATH bash tools/layers_check.sh` | **PASS** (44 modules, 111 edges, 0 named errors) |
| `bash tools/debt_ratchet.sh` | **0 over limit** |
| `MUTATION_FILTER=… MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline` | 3 new entries, **3 proved, 0 not a gate** — `the fill marks a settled aquifer fluid`, `the promotion pass ticks a marked fluid cell`, `an uninitialized sky light reads 0 in every dimension` |

Mutations registered and *dropped* after measuring that they had no gate: "the fill gate is the
aquifer flag, not the state" (the fluid test in `terrain_mark` catches the always-mark mutation, so
the two halves are not separable by a test), "a liquid block skips the shape branch" (a water cell
re-derives to itself, so removing the skip changes nothing observable), and the pre-existing
"fresh-generation mushrooms do not infer shade from a canopy" (with the measured brightness its
`mushroom_grow_block` half no longer changes the answer, so it cannot be a gate any more).

## 7. Reproduce

```sh
CAP=/home/yuu/mc-spine-local/captures/matrix-v1/nat-4242-village-taiga
tools/port_triage.py "$CAP/world" 4 -32 --n 3 --pop-order "$CAP/order.tsv" \
  --pop-order-scope recorded --binary target/debug/mc-spine          # 3 / 884736

# the mark set, per cell and per writer (the port-side counterpart of the RUNTIME lines)
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/v.bin MC_SPINE_WG_DUMP_N=3 \
MC_SPINE_WG_DUMP_X=4 MC_SPINE_WG_DUMP_Z=-32 MC_SPINE_POP_ORDER="$CAP/order.tsv" \
MC_SPINE_POP_ORDER_SCOPE=recorded MARK_TRACE=all POSTPROC_STATES=1 ./target/debug/mc-spine

# the terrain stage's answer at one cell, with the aquifer's mark flag
./target/debug/mc-spine --worldgen-cell overworld 4242 98 19 -513
# -> minecraft:water density=-0.005794… fill=minecraft:water/minecraft:water(sched=80) mark_fill=true

# the fill's own mark file, from the capture (the mark set native really had)
#   mc-native world, chunk (10,-32) Status=minecraft:carvers -> PostProcessing = 2045 marks
```
