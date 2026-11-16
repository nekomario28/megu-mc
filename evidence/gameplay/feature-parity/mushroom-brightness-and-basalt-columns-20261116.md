# The two residuals that are not support rules: mushroom raw brightness, and a basalt column's layer count

Track 5C set out to close the Nether `(-25,19)+3x3` 32-cell residual and the Village Taiga
`(4,-32)+3x3` 4-cell residual by fixing `MushroomBlock.canSurvive`'s brightness read and
`BasaltColumnsFeature`'s column placement. **Both attributions are wrong**, and this page records
what they actually are, measured on the pinned 1.21.1 jar
(`server.jar` SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`, mappings `server.txt` SHA-1
`03f8985492bda0afc0898465341eb0acef35f570`) and the two saved worlds.

The lane lands no parity change for either residual. What it lands is the contract the port has to
keep (with a Nether-shaped contract test the Overworld fixture cannot see), the two instruments the
next lane needs, and the reason each residual is out of reach offline.

Baseline and final numbers are identical, on purpose - see [Regression](#regression).

> **Review status (2026-11-16, independent readiness review).** Accepted: the `CEL_TRACE`
> instrument and its `cel_layer_ys`/`cel_empty` split, the Nether-shaped contract test that pins the
> `isSolidRender` reading, the corrected reading of `dla.a(dtc,dcz,jd)` itself, and the retraction of
> the "column-placement difference inside `BasaltColumnsFeature`" attribution. **Not accepted as
> measurement**: the light attribution in §1 and the draw-count decomposition in §2 (see the inline
> notes). This page carries no `.run.json` receipt, and the streams behind its counts are not bound to
> a source SHA. Detail: [readiness-89ac2124-fc9dfdf-20261116.md](../../review/readiness-89ac2124-fc9dfdf-20261116.md).

## 1. `MushroomBlock.canSurvive`: the support test is `isSolidRender`, and the residual is not the support test

`dla.a(dtc,dcz,jd)` decompiled from the pinned jar:

```java
protected boolean a(dtc state, dcz level, jd pos) {
   jd below = pos.e();                                  // pos.below()
   dtc support = level.a_(below);
   return support.a(awe.bb)                             // BlockTags.MUSHROOM_GROW_BLOCK
        ? true
        : level.b(pos, 0) < 13 && this.b(support, level, below);
}
```

`awe.bb` resolves through the official mappings to `MUSHROOM_GROW_BLOCK`, whose pinned contents are
`mycelium, podzol, crimson_nylium, warped_nylium` - **not** netherrack.

The second conjunct is the part both the lane brief and the previous evidence page got wrong. `dla`
*overrides* `b(dtc,dcc,jd)`:

```java
// dla
protected boolean b(dtc state, dcc level, jd pos) { return state.i(level, pos); }
// mappings 1035:1042: boolean isSolidRender(BlockGetter, BlockPos) -> i
```

i.e. `BlockState.isSolidRender(BlockGetter, BlockPos)`, not `BushBlock.mayPlaceOn`
(`dgh.b(dtc,dcc,jd)`: farmland | `#minecraft:dirt`) - the two have the same signature, which is
exactly how the reading went wrong. Measured, not just read: switching the arm to the `#dirt`
reading and re-running the Nether fixture costs **24 cells** (all `brown/red_mushroom_nether` on
netherrack, which the reference places), while the solid-render reading leaves those 24 matching.

`getRawBrightness` (`BlockAndTintGetter.b(jd,int)`, `dbz` line 12) is not a constant:

```java
default int b(jd pos, int amount) { return this.y_().a(pos, amount); }   // getLightEngine()
```

and `WorldGenRegion.getLightEngine()` (`arb.y_()`) forwards to the **real `ServerLevel`'s**
`LevelLightEngine`. `LevelLightEngine.a(jd,int)` (`eot` 127) is
`max(blockEngine.getLightValue(pos), skyEngine.getLightValue(pos) - amount)`, with the sky engine
`null` in a dimension without a sky layer. For a column with no lit section data
`SkyLightSectionStorage.e(pos, false)` (`eoy`) returns **15** (`return cached && !j(...) ? 0 : 15`
with `cached == false`), and the block engine returns **0**. So the port's
`uninitialized_raw_brightness` (15 with a sky layer, block light otherwise) is the right model of an
*uninitialized* read - and that is all it is.

### The residual is a *lit* read, and the fixture separates on fire distance

Both `brown_mushroom_nether` and `red_mushroom_nether` are `patch_brown_mushroom`/`patch_red_mushroom`
(`random_patch`, `tries 96`, `xz_spread 7`, `y_spread 3`) whose nested placed feature is
`block_predicate_filter(matching_blocks: air)` in front of `simple_block`. Those are the only two
state reads, so the candidate set is the whole story:

* the port's draw stream for `(-25,19)` `gi=9,step=7` and `gi=10,step=7` is **identical to the
  reference's for all 580 draws each** (`FSEED_ARM=-25,19,9,7;-25,19,10,7` against the pass's
  `SPWINDOW` 7 and 8) - the same 96 candidate cells, in the same order, on both sides;
* the port places 28 mushrooms, the reference 24, and the port's set is a strict superset.

So the four cells differ inside `canSurvive`. The port reads block light 0 (accept); the reference
refused, which needs `getRawBrightness >= 13`. The reference's world says why - `patch_fire` and
`patch_soul_fire` ran earlier in the same pass (`gi=4,5`) and fire emits light 15:

| cell | nearest `fire`/`soul_fire` | Manhattan | lit block light | reference | port |
|---|---|---|---|---|---|
| `(-399,39,308)` | `(-398,39,307)` | 2 | 13 | refused | placed |
| `(-396,38,304)` | `(-395,38,305)` | 2 | 13 | refused | placed |
| `(-395,38,307)` | `(-396,38,306)` | 2 | 13 | refused | placed |
| `(-400,39,308)` | `(-400,39,310)` | 2 | 13 | refused | placed |
| the 24 matching cells | | **>= 3** | <= 12 | placed | placed |

Every mismatching cell is at Manhattan distance <= 2 from a fire block and every matching cell is at
>= 3 - the partition `15 - d >= 13` predicts. **That is a 4-cell post-hoc fit, not a measured light
value.** It supports the hypothesis that the reference's raw brightness on this pass was the **lit**
block light rather than the uninitialized 0, because the light engine it reads is the running
server's. Three links in that chain are still unmeasured: the value the reference actually returned
(no `getRawBrightness` hook exists yet - see the last paragraph of this section), whether a
same-pass `patch_fire` write updates the light engine at all (the fixture shows where fire *ended
up*, not that a light update ran before the mushroom's read), and whether the support state the
reference read is the one the saved world carries. Until those are measured, treat the light
attribution as a **hypothesis**; do not rewrite `canSurvive` or its brightness model on it.

That is not modellable from an offline generator: it needs the light data of the chunk that the
server happened to have already lit, which depends on generation order and on every nearby light
source, blocks the feature layer has not written yet included. The same reading explains the Village
Taiga cell `(99,68,-502)`, where the reference placed a `brown_mushroom` on `grass_block` under a
canopy: its skylight read was a *lit* value below 13 (`<=3` blocks of leaf stay between it and the
open sky), while the port's uninitialized read is 15 and refuses (measured earlier: 41 `MushroomBlock`
reads on the forest fixture were 15, "beneath a canopy included").

**Recorded:** nether residual stays 4 cells, taiga stays 1. The next instrument is a native
`getRawBrightness` hook on `MushroomBlock.canSurvive` in the capture agent, so the read per cell is
measured instead of derived from fire distance.

## 2. `large_basalt_columns`: not a column-placement rule, a `count_on_every_layer` layer count two features upstream

`BasaltColumnsFeature` (`ebi`) was read end to end against `src/feature.rs`'s `Cf::BasaltColumns`:
`place` (`ebi.a(ecg)`), `canStart` (`ebi.a(dcx,int,jd)`), `findSurface` (`ebi.a(dcx,int,a,int)`),
`findAir` (`ebi.a(dcx,a,int)`), `placeColumn` (`ebi.a(dcx,int,jd,int,int)`), the
`randomBetweenClosed` candidate draws, `distManhattan` (`kh.k(jd)`, mappings `239:242`) and the
`MutableBlockPos` cursor. They agree: `k()` is `mutable()` (not `below()` - `below()` is `e()`), the
candidate loop draws x,y,z then the per-candidate `reach` sample, the column square iterates x
fastest, and the `remaining - dist/2` + basalt-continuation walk matches byte for byte. Nothing in
that class is a candidate for the 26 cells.

`CARVEAGENT`'s ordered trace gives the actual answer. In pass `(-25,22)` (16th in the recorded pop
order) the two `ebi` windows are `large_basalt_columns` (window 3) and the port's
`FSEED_ARM=-25,22,2,4` stream is the comparable unit. Both streams agree draw for draw up to the
first feature call and then diverge at draw 16:

| | placement (`count_on_every_layer`, `count: 2`) | first feature draw |
|---|---|---|
| reference | **16** `nextInt(16)` = 4 layers x 2 samples | `nextInt(6) = 3` |
| port | **20** `nextInt(16)` = 5 layers x 2 samples | `nextInt(6) = 5` |

Same for `delta` (`ebv`, pass `(-25,22)` `gi=0,step=4`, placement `count_on_every_layer(40)`): the
port's `(0,4)` stream is 780 draws and the reference's 800, **identical for the whole 780**. The
delta draws 5 per accepted position (one `nextDouble` for the 0.9 blob test, two `size` samples, and
two `rim_size` samples only when the blob test passed, which 73 of the 78 did), so
`156 + 2*156 + 2*73 = 780` port positions and `82 + 2*82 + 2*77 = 800` for the reference: the same
five layers sampled (400 `nextInt(16)` on both sides), **four of the 200 samples found a layer in the
reference that the port did not**.

> Review 2026-11-16: that decomposition does not evaluate to the totals it states -
> `156 + 2*156 + 2*73` is **614**, not 780, and `82 + 2*82 + 2*77` is **400**, not 800. The two totals
> are consistent with 156 vs 160 accepted positions at 5 draws each, which keeps the layer-walk
> reading but not the stated arithmetic. The `four of 200` figure also does not fall out of the
> surviving `CEL_TRACE` log (pass `(-25,22)`: 149 columns, 38 of them stopping at layer 3, layers
> `0..4` requested - the *port* side of the divergence, not the count). Treat the draw counts as
> author-reported until the streams are re-derived from a receipt that names its source SHA.

`getY` (`eim.a(eiw,int,int,int,int)`) walks the column itself, so a layer-count difference is a
*column* difference: the reference's column carries one more "empty cell above a non-empty, non-bedrock
cell" than the port's. The port's arm is not the suspect - `CEL_TRACE` (`PM::CountEveryLayer`'s
sample list; see `cel_layer_ys`) prints exactly the boundary set the walk uses, and both sides
consume the same draws.

The divergence is *not* local to the basalt features and not local to the compared window. Chunk
`(-25,22)` - the pop that writes the 28 window cells - is **966 cells** off on its own, and
`(-24,22)` 559 (`-25 19 --n 4`, the 3x3 sub-block unchanged at 32, so the wider window is a faithful
replay). The 26 `air -> basalt[axis=y]` cells and the 2 missing ones are the part of that pass that
lands inside the compared window; the delta's layer walk is the *first* divergence in the pass, so
everything after it - the delta's own four missing columns, both basalt column sets, the 966 cells -
is downstream of one column that reads differently before any step-4 feature wrote to it.

That column is `(-389,361)`, and its step-4 history in the port is in the write trace
(`FEATURE_WRITE_TRACE=-25,22`): `delta` writes `magma_block` at `(-389,91,361)` and `lava` at
`(-389,54,361)`, `basalt_blobs` (`gi=0,step=7`) writes the `4,5` and `62..75` basalt, and
`large_basalt_columns` writes `6,7`. The reference has `basalt[axis=y]` at `84..95`, `91` included -
a cell a basalt column cannot overwrite through `magma_block` - so at step 4 the reference's support
column there was taller and did not accept the delta at 91. Falsifying that needs the reference's
mid-pass column, which the saved world does not carry and the capture's `FCELL`-less trace does not
either.

**Recorded:** the residual stays 26+2 cells and the previous page's attribution ("a
column-placement difference inside `BasaltColumnsFeature`") is retracted; the next instrument is a
native per-position write trace (`FEATURE_WRITE_TRACE`'s counterpart) or a `getRawBrightness`-style
hook on `count_on_every_layer`'s height/column reads.

## 3. Instruments this lane added

`CEL_TRACE=<cx>,<cz>` (`src/feature.rs`): one `CEL` line per `count_on_every_layer` sample - the
placement's chunk, the layer it was asked for, the `MOTION_BLOCKING` top it read, the y it answered
and **every** layer the column offers. A layer-count difference between the port and the reference
then names the boundary one side has and the other does not, which is what localised the two cases
above. `cel_layer_ys` is the annotated walk; `cel_empty` is `isEmpty()` shared with `cel_find_y`, so
the trace cannot drift from the placement it annotates - pinned by
`cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks` and one mutation.

## 4. `sculk_patch` on this HEAD, and the `bonus_chest` oracle

`tools/sculk_gate.py` needs a saved reference region; none was on this host, so it was regenerated
(`tools/chunk_world_fixture.sh gate -16 112 2 features`, seed 0, `FIXTURE_DIR` moved off the full
`/tmp` tmpfs) and the port dump taken at HEAD with the same window:

| | `sculk` | `sculk_vein` | `sculk_catalyst` | total |
|---|---|---|---|---|
| reference | 175 | 125 | 1 | **301** |
| port | 265 | 131 | 1 | **397** |

`missing 107 / extra 203 / wrong-state 0` -> **310 disagreements**, `--require-match` exits **1**.
The reference column set reproduces the number recorded on 2026-10-28 (175/125/1 = 301) exactly, so
the capture is comparable; the port moved from 387 disagreements to 310 in the meantime. The shape
is unchanged: too much `sculk`, too little `sculk_vein`, i.e. the behaviour *selection* inside one
spread, not the placement (whose candidate set, biome test, accepted set, order and draw stream the
same page records as identical). `wrong-state 0` is new.

`tools/feature_oracles.py` names `bonus_chest` and `BonusChestOracle` /
`tests/data/bonus_chest_1211.tsv` still do not exist - and it is **not** straightforward, for a
reason that is in the feature's own body. `ebo.a(ecg)` places the chest and then calls
`RandomizableContainer.setBlockEntityLootTable(level, random, pos, LootTables.SPAWN_BONUS_CHEST)`,
which spends one `nextLong` **only if `level.getBlockEntity(pos)` is a `RandomizableContainer`**. A
scripted level answers `getBlockEntity` with nothing, so an oracle built on `NativeHarness` would
silently drop the draw and produce a fixture that disagrees with the port on the exact sentence it is
meant to pin. The real `WorldGenRegion.setBlock` (`arb.a(jd,dtc,int,int)`) leaves a `DUMMY`
block-entity tag so the guard passes. Wiring the oracle therefore needs a harness capability first
(pending block entities on `setBlock`), not just the ~250-line class.

## Regression

Same binary, `tools/port_triage.py --mode full`, before (`e7ead8e`) and after this lane:

| world | command | before | after |
|---|---|---|---|
| Nether `(-25,19)+3x3`, ordered | `--n 3 --preset nether --pop-order /tmp/rt-4242/nf-pop49.tsv --pop-order-scope recorded` | 32 | **32** |
| Village Taiga `(4,-32)+3x3` | `--n 3` | 4 | **4** |
| stronghold | `-96 35` | 0 | **0** |
| nether fortress | `-25 6 --preset nether` | 0 | **0** |
| ruined portal | `6 7` | 0 | **0** |
| end outer | `10 10 --preset end` | 0 | **0** |

`tools/mutation_check.sh`: 410 proved, 0 not a gate, 0 parked, 0 skipped.

One seed, one dimension, one host.
