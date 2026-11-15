# Village Taiga (4,-32)+3×3: support predicates and placement order

Source baseline: `53b75d8` on `t3/1b855eea`. Target world: the authenticated 1.21.1 server
(`server.jar` SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`, mappings `server.txt`
SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`), seed 4242,
`/tmp/nat-4242-village-taiga/world` (biomes `taiga` + `lush_caves`, structures on).

```
python3 tools/port_triage.py /tmp/nat-4242-village-taiga/world 4 -32 --n 3 --mode full --samples 3
```

| build | `all_air_kinds` | note |
|---|---|---|
| `53b75d8` (Track 4H) | **5390** | |
| `BushBlock` support predicate | 2761 | −2629 |
| `VinesFeature` body | 2579 | −182 |
| vegetation-patch surface set (`placeGround` return) | 111 | −2468 |
| spore blossom support | 20 | −91 |
| `UnderwaterMagmaFeature` order/scan/enclosure | 6 | −14 |
| small dripleaf support | **4** | −2 |

Regressions re-measured with the final binary:

| world | command | before | after |
|---|---|---|---|
| `/tmp/nat-4242-ruined-portal/world` | `6 7 --mode full` | 0 | **0** |
| `/tmp/nat-4242-stronghold/world` | `-96 35 --mode full` | 6 | **6** (pre-existing) |
| `…/mc-native-ow-seed4242-20261111/world` | `0 0 --mode full` | 0 | **0** |
| `/tmp/nat-4242-jungle-tree/world` | `73 -42 --mode full` | 2121 | **2122** (see below) |

The lane moves exactly three cells on the jungle-tree fixture and adds one diff: (1168,68,-665)
`vine[east]`→`vine[west]` (both wrong; native has `oak_log`), (1169,74,-667) `air`→`vine[up=true]`
(native has leaves) and (1172,79,-666) `air`→`vine[south]` (native has air, so this one is a real
new error). That fixture's own baseline is 2121 differing cells, i.e. its canopy is not the
reference's, and a `vine` is placed against whatever the port's world holds. It is recorded rather
than claimed clean; it is not a gate for this lane.

## How the diffs were localised

Every step below compares the *same* decoration pass on both sides. The capture already carries
the agent's ordered trace (`SPASS <cx> <cz>` groups, `SPLACE <class>` per configured-feature
placement, `SDRAW <kind>@dyn(<bound>) <value>` per typed draw), and the port emits the same shapes:
`FEATURE_ATTEMPT_TRACE=1` for the attempt list, `FSEED_ARM=<cx>,<cz>,<gi>,<step>` for the draw
stream (`DRAW n f=<gi>,<step> <kind>=<value>`).

A pass attempts the biome's placed features once per chunk in a fixed order, so attempt index `i`
on the port is window `i` in the native pass. `/tmp/vt/passcheck.py` (scratch, not committed) diffs
them and reports the first diverging draw of the first diverging attempt. On the final binary all
nine passes of the 3×3 report **0 diverging attempts**; before that, each fix below was found as
the first divergence of a specific attempt:

| chunk | attempt | first divergent draw | cause |
|---|---|---|---|
| (5,-32) | `lush_caves_vegetation` (25,9) | 124: native `nextFloat` vs port `nextInt(16)` | `placeGround` return value |
| (5,-30) | `lush_caves_ceiling_vegetation` (22,9) | 40: native `nextInt(2)` vs port `nextFloat` | a column vanilla accepts, the port refused — a missing `vine[up=true]` made its `isAir` test fail |
| (5,-30) | `lush_caves_clay` (24,9) | 310 | the same surface-set rule |
| (5,-30) | `lush_caves_vegetation` (25,9) | 282 | a *world-state* difference (1-chunk dumps are not a faithful comparison — see below) |

Two measurement lessons, both of which produced a wrong conclusion before the bytes were read:

- **A one-chunk dump is not the same world.** The same pass compared from an `--n 1` dump put the
  port's draw 40 at `nextFloat` where the 3×3 dump puts it at `nextInt(2)` — the columns the patch
  accepts depend on cross-chunk feature writes, and the population order changes with the dump
  window. Every conclusion here was re-measured on the 3×3 window.
- **Compare typed draws, not values.** `nextFloat` and `nextInt(bound)` each consume one
  `next(bits)`, so a shifted *classification* leaves both streams the same length and the values
  where the kinds agree. The kind sequence is what says "this column drew a depth where the
  reference drew an edge chance".

`VEG_COL_TRACE=<cx>,<cz>` was added for this lane: it prints a `VPATCH` header (origin, `xr`, `zr`,
surface) and one `VCOL` line per candidate column with the edge draw it consumed and why it was
refused (`SKIP edge`, `SKIP surface air=.. sturdy=..`, `ACCEPT y=.. dep=..`). Column order plus the
edge draw is what ties a patch's `depth` draws to positions, so it pairs directly with the native
window's kind sequence.

## `BushBlock.canSurvive` is the dirt tag or farmland — `dirt_path` is neither

`dgh.b(dtc,dcc,jd)` (`BushBlock.mayPlaceOn`) reads

```java
return state.is(Blocks.FARMLAND) || state.is(BlockTags.DIRT);
```

`#minecraft:dirt` holds dirt, grass_block, podzol, coarse_dirt, mycelium, rooted_dirt, moss_block,
mud, muddy_mangrove_roots — and **not** `minecraft:dirt_path`. Measured by running the native
`BlockState.canSurvive` itself against a scripted level (one-off probe on the pinned jar, reusing
`tools/structure-oracle/NativeHarness`):

| below | spruce_sapling | oak_sapling | short_grass |
|---|---|---|---|
| dirt / grass_block / coarse_dirt / podzol / moss_block / farmland | true | true | true |
| **dirt_path** | **false** | **false** | **false** |
| stone / sand | false | false | false |

The port's default arm of `can_place` used `is_dirtish`, which lists `dirt_path` by name, so a
spruce sapling "survived" on a village road. `spruce_checked` is
`block_predicate_filter(would_survive)` in front of `minecraft:spruce`, so the tree grew and
`TrunkPlacer.setDirtAt` (`ehm.a(ddc,BiConsumer,ayw,jd,efe)`) wrote `dirt` over the road:

```
(65,69,-498)  native dirt_path  port dirt   <- street straight_04 at (59,69,-498), terrain dirt below
(71,68,-502)  native grass_block port dirt  <- street straight_02 at (68,68,-498)
```

Those cells are not the template's fault: the jigsaw placement was already correct, and the port's
own write trace (`FEATURE_CELL_TRACE=65,69,-498`) shows `12513` (dirt_path) written by
`village/taiga/streets/straight_04` and then overwritten by `state=10` (dirt) from
`feature=41,9` = `trees_taiga`. Two spurious spruces then rearranged the canopy, the light and the
`would_survive` outcomes of later candidates — 2,543 `trees_taiga` cells. The one redundant
`dirt_path` early return in `SimpleBlockFeature` was removed with the fix: `can_place` now owns
that rule, and the fixture is identical either way.

Guard: `bush_block_family_support_is_the_dirt_tag_or_farmland`, plus the same rule through
`Pr::WouldSurvive`.

## `VinesFeature` writes one block, from `Direction.values()` order, UP first

`edu.a(ecg)`:

```java
if (!level.isEmptyBlock(pos)) return false;
for (Direction dir : Direction.values())
   if (dir != Direction.DOWN && VineBlock.canSupportAtFace(level, pos.relative(dir), dir)) {
      level.setBlock(pos, VINE.setValue(VINE_PROPS.get(dir), true), 2);
      return true;
   }
return false;
```

`canSupportAtFace` (`dov.a(dcc,jd,ji)`) is `MultifaceBlock.canAttachTo` (`dky.a`): the neighbour's
block-support shape **or** its collision shape fully covers the face pointing back at the vine —
so leaves (full collision cube) can carry a vine. The port hung a four-block column and only from
the four horizontal neighbours, which is not this version's body.

In the lush caves this is the ceiling anchor: `classic_vines_cave_feature` places
`vine[up=true]` under a cave ceiling, and every missing one made a later `moss_patch_ceiling`
column's `isAir` test pass that the reference refuses (measured at (78,-2,-497), chunk (5,-32):
native `vine[up=true]`, port `air`, and the port then filled two moss cells the reference keeps as
deepslate).

Guard: `vines_attach_to_the_first_support_in_direction_values_order`.

## `VegetationPatchFeature.placeGround` returns "this column joins the set"

`edt.a(dds,efh,Predicate,ayw,jd.a,int)`:

```java
for (int i = 0; i < depth; i++) {
   BlockState next = config.groundState.getState(random, mut);
   BlockState existing = level.getBlockState(mut);
   if (next.is(existing.getBlock())) continue;      // no move either
   if (!replaceable.test(existing)) return i != 0;
   level.setBlock(mut, next, 2);
   mut.move(config.surface.getDirection());
}
return true;
```

The returned flag decides whether the caller adds the column to the vegetation `Set<BlockPos>`, so
a column whose ground already **is** the patch's block still contributes a surface point and still
takes a `vegetation_chance` roll. The port gated on "wrote at least one block" (`placed_any`),
which dropped exactly those columns: on chunk (5,-32) `moss_patch` stopped one roll short of
native (draw 124: native `nextFloat`, port the next attempt's `nextInt(16)`) and every moss, clay,
cave-vine and short-grass cell of that pass moved with it — 2,468 cells from one sentence.

Guard: `vegetation_patch_keeps_a_column_whose_ground_is_already_the_patch_block`.

## `SporeBlossomBlock.canSurvive`, `SmallDripleafBlock.canSurvive`

```java
// dnp.a(dtc,dcz,jd)
return Block.canSupportCenter(level, pos.above(), Direction.DOWN) && !level.isWaterAt(pos);
```

The blossom hangs from a ceiling, so its support is the block **above** and its DOWN face. Without
an arm it fell through to the family rule ("dirt below"), which air never satisfies: 7 native
blossoms in the 3×3, 0 in the port. Each one also changed the ceiling walk of the moss patch above
it (111 → 20).

```java
// dne.a(dtc,dcz,jd), measured on the pinned jar (same probe harness)
// lower half: below in #minecraft:small_dripleaf_placeable           (clay, moss_block), any cell
//             or (cell is water and below in #minecraft:dirt/farmland)
```

The port required *water in the cell* **and** a placeable support, which is neither case: it
refused the dripleaf of a `clay_with_dripleaves` patch standing in air on clay ((69,-7,-493) and
(69,-6,-493)).

Guards: `spore_blossom_needs_a_sturdy_ceiling_and_a_dry_cell`,
`small_dripleaf_supports_are_the_placeable_tag_or_dirt_under_water`.

## `UnderwaterMagmaFeature`: `betweenClosed` order, five scan positions, no cell-above test

`eds.a(ecg)`:

```java
OptionalInt floorY = Column.scan(level, pos, config.floor_search_range, s -> s.is(WATER), s -> !s.is(WATER))
                             .map(Column::getFloorY);
if (floorY.isEmpty()) return false;
BoundingBox bb = BoundingBox.fromCorners(floorPos.offset(-r,-r,-r), floorPos.offset(r,r,r));
return BlockPos.betweenClosedStream(bb)
    .filter(p -> random.nextFloat() < probability)     // one draw per cell, in this order
    .filter(p -> this.isValidPosition(level, p))
    .mapToInt(p -> { level.setBlock(p, MAGMA, 2); return 1; }).sum() > 0;
```

Three separate sentences, each with its own mutant:

- `betweenClosed` walks **x fastest, then y, then z**; the port ran `dx`/`dy`/`dz` with `dz`
  fastest, so the probability draws landed on a permutation of the cube and magma appeared two
  blocks diagonally away ((69..70,19,-501) against the reference's (71,19,-502..-503)).
- `Column.scan` (`dyo.a`) moves `i < maxSteps` times and then tests the **moved** position, so with
  `floor_search_range: 5` a floor four blocks under the origin still counts; the port stopped one
  short.
- `isValidPosition` (`eds.b`) tests the cell, the cell **below** and the four horizontal
  neighbours — never the cell above. The port also required the cell above to be non-water.

Guards: `underwater_magma_binds_its_probability_draws_in_betweenclosed_order` (whose fixture is
asserted to select a different set under the competing order, so it cannot pass for both),
`underwater_magma_scans_five_positions_and_ignores_the_cell_above`.

## What is still not matching (4 cells) and why this lane stops here

| cell | native | port | what is known |
|---|---|---|---|
| (98,19,-512), (99,19,-512) | `water[level=1]` | air | a flowing-water cell; the reference holds water with `level=1` where the port has neither water nor a source. Not attributable from the saved state alone. |
| (99,68,-502) | `brown_mushroom` | air | `MushroomBlock.canSurvive` (`dla.a`) is `getRawBrightness(pos,0) < 13 && below.isSolidRender(...)` unless `below` is in the grow-block tag. The cell is open to the sky, so the port's "uninitialized Overworld skylight reads 15" refuses it while the reference placed it. That contradicts the forest fixture's measurement of the same read and needs its own probe (`dcz.b(jd,int)` on a `WorldGenRegion`), not a guess. |
| (82,69,-500) | air | `fern` | The fern comes from the pass of **chunk (4,-32)** (`FCTX 4 -32 52 9`), where the street's `dirt_path` at (82,68,-500) does not exist yet — that cell is written by (5,-32)'s pass (`feature=25,4`). In the reference the same order should apply, so the reference must see a non-dirt block under the fern at that moment; one of the two sides is reading a pre-feature world this lane cannot observe without a native write log for that pass. |

All four are single cells with no cascade left behind them; the fixture's native-side floor (two
runs of the same fixture differ by ~4 cells) is the same order of magnitude.
