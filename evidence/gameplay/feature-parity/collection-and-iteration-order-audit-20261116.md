# Collection iteration order, `betweenClosed`, `Direction` order and the draw stream

Track 6C. Tracks 4H/4K/4L each closed a large parity gap with the same kind of fix: vanilla
collects positions into a `Set`, or walks a box with `BlockPos.betweenClosed`, or walks
`Direction.Plane.HORIZONTAL`, and draws from the `RandomSource` (or writes world state that a
later draw-driven test reads) *inside* that iteration. A port that walks the same cells in a
different order keeps every draw count and permutes which position receives which value.

This page is the codebase-wide audit of that class, the four discrepancies it found, and what
each fix moves. The authoritative reference is the pinned 1.21.1 `server.jar`
(sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`, official mappings
`/var/tmp/mcm/server.txt` sha1 `03f8985492bda0afc0898465341eb0acef35f570`), read with
`tools/bytecode_probe.sh`.

## How the candidate list was produced

Guessing which features "look like they walk a set" is how the same class gets missed twice, so
the list is mechanical. The obfuscated names of every class under
`net.minecraft.world.level.levelgen.{feature,structure}**` were resolved from the mappings
(414 classes incl. nested), disassembled once with `javap -p -c`, and scanned for the
order-sensitive signatures. The mapping from an official name to the obfuscated one is the
mappings' own:

| construct | signature in the disassembly |
|---|---|
| `BlockPos.betweenClosed(BlockPos,BlockPos)` | `Method jd.c:(Ljd;Ljd;)Ljava/lang/Iterable;` |
| `BlockPos.betweenClosed(int x6)` | `Method jd.b:(IIIIII)Ljava/lang/Iterable;` |
| `BlockPos.betweenClosedStream(BoundingBox)` | `Method jd.a:(Ldda;)Ljava/util/stream/Stream;` |
| `BlockPos.randomBetweenClosed` | `Method jd.a:(Layw;IIIIIII)Ljava/lang/Iterable;` |
| `BlockPos.breadthFirstTraversal` | `Method jd.a:(Ljd;IILjava/util/function/BiConsumer;...)` |
| `BlockPos.spiralAround` | `Method jd.a:(Ljd;ILji;Lji;)Ljava/lang/Iterable;` |
| `Direction.values()` | `Method ji.values:()[Lji;` |
| `Direction.Plane.HORIZONTAL` | `Field ji$c.a:Lji$c;` |
| `Plane.getRandomDirection` / `Plane.getShuffled` | `Method ji$c.a:(Layw;)Lji;` / `ji$c.c:(Layw;)Ljava/util/List;` |
| `Sets.newHashSet` / `new HashSet<>` | `Sets.newHashSet` / `java/util/HashSet."<init>"` |
| `Util.shuffledCopy` | `Method ad.a:(Lit/unimi/dsi/fastutil/objects/ObjectArrayList;Layw;)Ljava/util/List;` |

Plus the two facts the whole class rests on, both read off the pinned jar rather than assumed:

* `BlockPos.betweenClosed` visits **x fastest, then y, then z** (its lambda decodes the index as
  `x = i % w`, `y = (i/w) % h`, `z = i/(w*h)` in `jd.c`), so the *outer* Rust loop is `z`.
* `Direction.values()` is `DOWN, UP, NORTH, SOUTH, WEST, EAST` (`ji`'s enum order
  `a..f`), and `Direction.Plane.HORIZONTAL`'s faces array is `[ji.c, ji.f, ji.d, ji.e]` =
  **NORTH, EAST, SOUTH, WEST** (`ji$c.<clinit>`). `Plane.getRandomDirection` is
  `faces[nextInt(faces.length)]` (`ji$c.a` → `Util.getRandom(T[], RandomSource)` =
  `array[nextInt(array.length)]`), and `Plane`'s `iterator()` is `Iterators.forArray(faces)` —
  the same array in both roles.

The scan is committed as `tools/feature_order_scan.py` (0.5 s for the whole jar); the tables below
are its output joined to the Rust arm that implements each class, and every flagged class was read
against its Rust arm rather than assumed to match.

## Table A — the 59 configured feature types

`-` means the class contains **no** order-sensitive construct at all, so no iteration-order
question exists for it. Every flagged class was then read against its Rust arm.

| configured feature | Java class | order-sensitive construct |
|---|---|---|
| `bamboo` | `BambooFeature` | - |
| `basalt_columns` | `BasaltColumnsFeature` | betweenClosed(ints), randomBetweenClosed |
| `basalt_pillar` | `BasaltPillarFeature` | - |
| `block_column` | `BlockColumnFeature` | - |
| `block_pile` | `BlockPileFeature` | betweenClosed |
| `blue_ice` | `BlueIceFeature` | Dir.values |
| `bonus_chest` | `BonusChestFeature` | HORIZONTAL |
| `chorus_plant` | `ChorusPlantFeature` | - |
| `coral_claw` | `CoralClawFeature` | HORIZONTAL |
| `coral_mushroom` | `CoralMushroomFeature` | - |
| `coral_tree` | `CoralTreeFeature` | HORIZONTAL, Plane.getShuffled |
| `delta_feature` | `DeltaFeature` | Dir.values |
| `desert_well` | `DesertWellFeature` | HORIZONTAL |
| `disk` | `DiskFeature` | betweenClosed |
| `dripstone_cluster` | `DripstoneClusterFeature` | HORIZONTAL |
| `end_gateway` | `EndGatewayFeature` | betweenClosed |
| `end_island` | `EndIslandFeature` | - |
| `end_platform` | `EndPlatformFeature` | - |
| `end_spike` | `SpikeFeature` | betweenClosed |
| `flower` / `random_patch` / `no_bonemeal_flower` | `RandomPatchFeature` | - |
| `forest_rock` | `BlockBlobFeature` | betweenClosed |
| `fossil` | `FossilFeature` | - |
| `freeze_top_layer` | `SnowAndFreezeFeature` | - |
| `geode` | `GeodeFeature` | Dir.values, betweenClosed |
| `glowstone_blob` | `GlowstoneFeature` | Dir.values |
| `huge_brown_mushroom` | `HugeBrownMushroomFeature` | - |
| `huge_fungus` | `HugeFungusFeature` | - |
| `huge_red_mushroom` | `HugeRedMushroomFeature` | - |
| `ice_spike` | `IceSpikeFeature` | - |
| `iceberg` | `IcebergFeature` | - |
| `kelp` | `KelpFeature` | - |
| `lake` | `LakeFeature` | - |
| `large_dripstone` | `LargeDripstoneFeature` | - |
| `monster_room` | `MonsterRoomFeature` | HORIZONTAL |
| `multiface_growth` | `MultifaceGrowthFeature` | - (the shuffle lives in `MultifaceGrowthConfiguration`) |
| `nether_forest_vegetation` | `NetherForestVegetationFeature` | - |
| `netherrack_replace_blobs` | `ReplaceBlobsFeature` | - |
| `ore` | `OreFeature` | - |
| `pointed_dripstone` | `PointedDripstoneFeature` | HORIZONTAL |
| `random_boolean_selector` | `RandomBooleanSelectorFeature` | - |
| `random_selector` | `RandomSelectorFeature` | - |
| `root_system` | `RootSystemFeature` | - |
| `scattered_ore` | `ScatteredOreFeature` | - |
| `sculk_patch` | `SculkPatchFeature` | - |
| `sea_pickle` | `SeaPickleFeature` | - |
| `seagrass` | `SeagrassFeature` | - |
| `simple_block` | `SimpleBlockFeature` | - |
| `simple_random_selector` | `SimpleRandomSelectorFeature` | - |
| `spring_feature` | `SpringFeature` | - |
| `tree` | `TreeFeature` | Dir.values, newHashSet |
| `twisting_vines` | `TwistingVinesFeature` | - |
| `underwater_magma` | `UnderwaterMagmaFeature` | HORIZONTAL |
| `vegetation_patch` | `VegetationPatchFeature` | newHashSet |
| `vines` | `VinesFeature` | Dir.values |
| `void_start_platform` | `VoidStartPlatformFeature` | - |
| `waterlogged_vegetation_patch` | `WaterloggedVegetationPatchFeature` | newHashSet |
| `weeping_vines` | `WeepingVinesFeature` | Dir.values |

The scan also flags three classes that are not configured-feature types but are part of the
same surface, and each has a verdict below: `MultifaceGrowthConfiguration` (the `multiface_growth`
config's per-cell `Util.shuffledCopy(directions)`), `Feature` (the shared base, a `Direction.values`
neighbour test) and `StructureTemplatePool` (`shuffledCopy` of the piece list).

Verdicts for the flagged rows (the rest are `-` and need no reading):

| feature | Java order | mc-spine | verdict |
|---|---|---|---|
| `basalt_columns` | `randomBetweenClosed(random, k, …)` = three `nextInt` per candidate, x then y then z (y's bound is 1 and is still drawn) | `pos.0 - r + next_bound(2r+1)`, `pos.1 + next_bound(1)`, `pos.2 - r + next_bound(2r+1)` per attempt | match |
| `block_pile` | `betweenClosed(origin.offset(-i,0,-j), origin.offset(i,1,j))`; two `nextFloat` always, a third on the else branch | `for z { for y { for x } }`, same draw shape | match |
| `blue_ice` | 200 attempts of `nextInt(5) - nextInt(6)`; no box walk | was a hand-written 7x5x7 ball with one `nextInt(6)` per cell | **differed — fixed (F3)** |
| `bonus_chest` / `coral_*` / `desert_well` | `Plane.HORIZONTAL` for a *shape* test (`getRandomDirection`, `getShuffled`) | F4 below | **differed — fixed (F4)** |
| `delta_feature` | `Direction.values()` only inside `isExposed`'s neighbour test (no draw) | `for d in 0..6` | match |
| `disk` | `betweenClosed(origin.offset(-r,0,-r), origin.offset(r,0,r))` = z slowest; provider draws per column | `for cx { for cz }` (x slowest) | **differed — fixed (F1)** |
| `dripstone_cluster` | `Plane.HORIZONTAL` in `isLavaOrWaterTouching` (no draw) | plain horizontal loop | match |
| `end_gateway`, `end_spike`, `end_platform` | `betweenClosed` over a fixed box, no draw inside | `z,y,x` walk | match |
| `forest_rock` | descend while (below is air) or (below is not `BASE_STONE_OVERWORLD` and not `DIRT`), then 3 blob iterations of three `nextInt(2)` + a shifted origin | ascend/descend on `is_air` with a `solid` break, one blob | **differed — recorded, not fixed (§ Open)** |
| `geode` | `betweenClosed` layer walk + `Direction.values()` crack | `z,y,x` walk (pinned by `geode_walks_the_generation_box_in_between_closed_order`) | match |
| `glowstone_blob`, `vines`, `weeping_vines`, `twisting_vines` | `Direction.values()` for a neighbour scan (no draw) | `0..6` | match |
| `multiface_growth` | `MultifaceGrowthConfiguration` shuffles its `directions` list per cell with `Util.shuffledCopy` | `feature_util_shuffle(&mut dirs, rng)` | match (Track 4K) |
| `coral_tree`, `coral_claw` | `Plane.getShuffled` / the plane array shuffled, then a claw arm walk | `feature_util_shuffle` + the plane array | match (Track 4K) |
| `StructureTemplatePool` | `shuffledCopy` of the piece list before a weighted pick | the pool walk | audited, not a position walk |
| `monster_room` | x/y/z walk (hole count only, no draw) | `dx,dy,dz` | match — order-insensitive |
| `pointed_dripstone` | `Plane.HORIZONTAL` + `nextFloat` before each gate; `Direction.getRandom` = `VALUES[nextInt(6)]` | `[(0,-1),(1,0),(0,1),(-1,0)]`, `direction_random` | match |
| `tree` | `Sets.newHashSet()` logs/leaves/roots → bucket order → stable sort by y | `tree::decorator_list` | match (Track 4L) |
| `underwater_magma` | `betweenClosed` = x fastest | `for dz { for dy { for dx } }` (pinned by test + mutation) | match |
| `vegetation_patch` | `Sets.newHashSet()` surface points walked for the vegetation pass | `java_hashset_order` | match (Track 4H) |

## Table B — trunk placers, foliage placers, root placers, decorators

Scan of `feature.{trunkplacers,foliageplacers,rootplacers,treedecorators}`:

| class | method | construct | mc-spine |
|---|---|---|---|
| `TrunkPlacer` | `placeLog`/`setDirtUnderneath` | - | match (Track 4L: the dirt write is a *log* entry) |
| `StraightTrunkPlacer`, `GiantTrunkPlacer`, `MegaJungleTrunkPlacer` | `placeTrunk` | - (MegaJungle walks a `cos/sin` arc, no box) | match |
| `DarkOakTrunkPlacer` | `placeTrunk` | `Plane.HORIZONTAL.getRandomDirection` for the lean | reversed plane order | **differed — fixed (F4)** |
| `FancyTrunkPlacer` | `placeTrunk` | - (no plane access) | match |
| `ForkingTrunkPlacer` | `placeTrunk` | `Plane.HORIZONTAL.getRandomDirection` (lean + branch) | reversed plane order | **differed — fixed (F4)** |
| `BendingTrunkPlacer` | `placeTrunk` | `Plane.HORIZONTAL.getRandomDirection` | reversed plane order | **differed — fixed (F4)** |
| `UpwardsBranchingTrunkPlacer` | `placeTrunk` | `Plane.HORIZONTAL.getRandomDirection` | reversed plane order | **differed — fixed (F4)** |
| `CherryTrunkPlacer` | `placeTrunk` | `Plane.HORIZONTAL.getRandomDirection` for the branch direction (one draw, mirrored for the second branch) | `[(0,-1),(0,1),(-1,0),(1,0)]` = N,S,W,E | **differed — fixed (F4)** |
| `FoliagePlacer` | `createFoliage`/`shouldSkipLocation` | `Plane.HORIZONTAL` neighbour test (no draw) | chunk-overlap test | match |
| `Blob`, `Bush`, `Cherry`, `DarkOak`, `MegaJungle`, `MegaPine`, `Pine`, `RandomSpread`, `Spruce` foliage placers | `place`/`placeLeavesRow` | - (row/radius walks, no Set, no box order) | match |
| `MangroveRootPlacer` | `place` | `for (Direction d : Plane.HORIZONTAL)` over the four root branches, drawing per branch | `[south, west, east, north]` | **differed — fixed (F4)** |
| `MangroveRootPlacement`, `AboveRootPlacement` | record accessors | - | match |
| `AlterGroundDecorator` | `place` | - (list of the lowest logs, plus 4 fixed corners) | match |
| `AttachedToLeavesDecorator` | `place` | `new HashSet<>` used only for `contains`/`add` (never iterated) + `betweenClosed` for the exclusion box; `Util.shuffledCopy(leaves)` then `nextFloat()` **before** `isPosEmpty` | drew the float *after* the empty test | **differed — fixed (F2)** |
| `BeehiveDecorator` | `place` | `Collections.shuffle(List)` — the **unseeded, single-argument** overload | seeded Fisher–Yates | **differed — recorded, not fixable (§ Open)** |
| `CocoaDecorator` | `place` | `for (Direction d : Plane.HORIZONTAL)` with one `nextFloat` per direction, `nextInt(3)` for AGE inside `isAir` | `[south, west, east, north]` | **differed — fixed (F5)** |
| `LeaveVineDecorator` | `placeVine` | four `nextFloat` per leaf, order west/east/north/south, each with a `VineBlock` property | west/east/north/south, four floats | match |
| `TrunkVineDecorator` | `place` | four `nextInt(3)` per log, order west/east/north/south | same | match |

## Table C — structure pieces and the template system

| class | construct | verdict |
|---|---|---|
| `StructurePiece` | `getRandomHorizontalDirection` = `Plane.HORIZONTAL.getRandomDirection` | `single_piece_structures::HORIZONTAL_FACINGS` / `stronghold_structures::STRONGHOLD_FACINGS` already read `[North, East, South, West]` → match |
| `StructureTemplate` | `betweenClosed` in `fillFromWorld` (no draw); `placeInWorld` walks the stored block list | match — order-insensitive |
| `BlockAgeProcessor` | `Plane.HORIZONTAL.getRandomDirection` for the aged-facing choice | `template_structures` already reads `[north, east, south, west]` → match |
| `OceanRuinPieces` | `betweenClosed` in a count/scan helper (no draw) | match — order-insensitive |
| `ShipwreckPieces` | `betweenClosed` over the hull for the block writes; the chest/sus-block choice `a(int, ayw)` draws but is not inside the walk | match |
| `BuriedTreasurePieces`, `MineshaftPieces`, `OceanMonumentPieces`, `WoodlandMansionPieces`, `OceanMonumentStructure`, `DesertPyramidPiece`, `EndPodiumFeature` | `Direction.values()` / `Plane.HORIZONTAL` for **adjacency or rotation** tests, not for a draw-bearing walk | match — audited, no draw is bound to the order |
| `StructureCheck`, `StructureFeatureIndexSavedData` | `HashMap`/`LongOpenHashSet` caches (the saved index is a membership set, never an iteration source) | match — not a draw order |

## Findings

Five findings across seven code sites. Every fix carries a contract test in
`src/generation_contract_tests.rs` and a proved entry in `tools/mutation_check.sh`.

### F1 `DiskFeature` walks its radius box z-slowest, the port walked it x-slowest

```
ebx.a(ecg)   … invokevirtual eef.c:()Lbpw; → bpw.a(ayw)  I   // radius sample
             … jd.b:(III) origin.offset(-I,0,-I)  →  jd.c:(Ljd;Ljd;)   // betweenClosed
             … a(eef,dds,ayw,int,int,jd$a)                    // per column, top→bottom
ebx.a(eef,dds,ayw,top,bottom,mut)   … eef.b:()Leaf.test → egr.a:(Ldds;Layw;Ljd;)Ldtc;  // draws
```

`betweenClosed` is x fastest / z slowest, so the outer loop is `z`; the port had
`for cx in -rr..=rr { for cz in -rr..=rr }`. Each accepted column then samples the state
provider (`WeightedStateProvider` draws `nextInt`), so the transposed walk binds every column's
draw to the wrong column. Fix: swap the two loops (`src/feature.rs`, `Cf::Disk`).
Test `disk_walks_the_radius_box_in_between_closed_order` replays the reference order and asserts
each cell's state, and asserts the transposed walk would bind differently.

### F2 `AttachedToLeavesDecorator` draws `probability` before it checks the column

```
egw.a(eha$a):
  72: Set.contains(pp)            ; ifeq  86      // exclusion set
  86: ayw.i:()F (nextFloat)  >= b ; ifge 220      // probability
 100: this.a(ctx, leaf, dir)      ; ifeq 220      // isPosEmpty × required_empty_blocks
 189: Set.add(every cell of the exclusion box)
 204: ctx.a(pp, blockProvider.getState(random, pp))
```

The port tested the exclusion set **and** walked the `required_empty_blocks` column *before*
drawing the float, so a leaf whose direction was blocked saved one draw
(`required_empty_blocks` 1–16; `mangrove` uses 2). Fix: contains → nextFloat → column walk
(`src/tree.rs`, `Deco::AttachedLeaves`). Test
`attached_to_leaves_draws_its_probability_before_the_required_empty_check` pins the stream state
after a refused leaf, and shows the "check first" order gives a different state.

### F3 `BlueIceFeature` was a hand-written sketch, not the reference's algorithm

The port's body ("drop to the ocean floor, then scatter blue/packed ice") consumed one
`nextInt(6)` per cell of a 7×5×7 ball. The reference:

```
ebn.a(ecg):
  16: pos.getY() > level.getSeaLevel() - 1              → return false
  33: !get(pos).is(WATER) && !get(pos.below()).is(WATER) → return false
  70: for (Direction d : Direction.values()) if (d != DOWN && get(pos.relative(d)).is(PACKED_ICE)) flag = true
 144: if (!flag) return false
 151: setBlock(pos, BLUE_ICE, 2)
 166: for (i = 0; i < 200; ++i) {
 177:   j = nextInt(5) - nextInt(6)
 197:   k = 3; if (j < 2) k += j / 2
 215:   if (k < 1) continue
 224:   p = pos.offset(nextInt(k) - nextInt(k), j, nextInt(k) - nextInt(k))
 280:   st = get(p); if (st.isAir() || WATER || ICE || PACKED_ICE)
 324:     for (Direction d : Direction.values()) if (get(p.relative(d)).is(BLUE_ICE)) { setBlock(p, BLUE_ICE, 2); break }
```

(`dga.mW` = `BLUE_ICE`, `dga.iC` = `PACKED_ICE`, `dga.dO` = `ICE`, `dga.G` = `WATER` from the
mappings.) The two `Direction.values()` walks only ask "does any neighbour match", so their order
is not a draw; the 200 attempts are. Fix: the reference's body, `nextInt` draws spelled out so
the bounds are visible (`src/feature.rs`, `Cf::BlueIce`). Test
`blue_ice_spends_the_native_two_ints_per_attempt_stream` replays 200 attempts and pins the stream
state, the origin state and the placed-cell count (68 for seed 99 in an all-air world).

### F4 `Direction.Plane.HORIZONTAL` was indexed backwards — five trunk placers and the root placer

`ji$c.<clinit>` builds `HORIZONTAL`'s faces array as `[ji.c, ji.f, ji.d, ji.e]`, and `ji`'s
enum order is `a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST`, i.e. **NORTH, EAST, SOUTH, WEST**.
`Plane.getRandomDirection` is `faces[nextInt(4)]`, and `Plane.iterator()` walks the same array.
`tree::rand_horizontal` mapped `0 → south, 1 → west, 2 → east, 3 → north` — the array read
backwards — so `DarkOakTrunkPlacer`, `ForkingTrunkPlacer`, `BendingTrunkPlacer` and
`UpwardsBranchingTrunkPlacer` leaned and branched in mirrored directions, and
`MangroveRootPlacer` walked its four branches in the wrong order (its `canGrow` draws, so the
order is the binding). Fix: `tree::horizontal_dir(i)` is the one owner of the plane order and
`plane_horizontal()` is the iteration array; the trunk placers, the cocoa decorator (F5) and the
root placer (and `CherryTrunkPlacer`, which had a third permutation, N/S/W/E) all read it.
Test `horizontal_plane_order_is_north_east_south_west` asserts the four offsets. Every other
module that models the plane (`feature::HORIZONTAL`, `end_city_mansion`, `single_piece_structures`,
`stronghold_structures`, `template_structures`, `fortress_monument`) already read
`[NORTH, EAST, SOUTH, WEST]` — `tree.rs` was the only place that indexed the array backwards, which
is why the fix is one helper rather than six patches.

`Direction.BY_2D_DATA` is a *different* array (`Arrays.stream(VALUES).filter(horizontal)
.sorted(comparingInt(get2DDataValue))` = SOUTH, WEST, NORTH, EAST) and is not affected;
`end_city_mansion::BY_2D_DATA` already matches it.

### F5 `CocoaDecorator` walked the plane backwards

```
egy.a(eha$a) → a(ayw,eha$a,jd):
  0: getstatic ji$c.a ; 3: invokevirtual ji$c.iterator()
 27: ayw.i:()F  <= 0.25
 39: ji.g:()Lji;  → dir.getOpposite()          // p = pos.relative(opposite)
 47: jd.b:(III)  → pos.offset(opp.getStepX(),0,opp.getStepZ())
 63: ctx.isAir(p)
 72: setBlock(p, COCOA{AGE = nextInt(3), FACING = dir})
```

The port's position and facing were right; the direction *order* was `[south, west, east, north]`.
Because the `nextInt(3)` for AGE is inside `isAir`, a different direction binding can also change
the number of draws, which shifts every later decorator and later tree. Fix: `plane_horizontal()`.
Test `cocoa_binds_its_four_probability_draws_to_north_east_south_west` replays
gate → per-direction float → `isAir` → `nextInt(3)` and asserts the written cells, and asserts
the reversed binding would write a different set.

## Measurement

`tools/port_triage.py <world> <cx> <cz> --mode full …`; the "before" column is the same binary
with the seven sites reverted (verified by dumping the same chunk from both binaries and
diffing the dumps cell by cell — the 13 cells in the table below are exactly the delta).

| fixture | command | before | after |
|---|---|---|---|
| jungle-temple | `74 -41 --n 1` | 116 | **121** |
| nether-fossil | `-25 19 --n 3 --preset nether` | 60 | 60 |
| village-taiga | `4 -32 --n 3` | 4 | 4 |
| stronghold | `-96 35 --n 1` | 0 | 0 |
| ruined-portal | `6 7 --n 1` | 0 | 0 |
| nether-fortress | `-25 6 --n 1 --preset nether` | 0 | 0 |
| end-outer | `10 10 --n 1 --preset end` | 0 | 0 |

(`end-city (65 45)` cannot be compared at all: the port's End dump covers noise `min_y`/`height`
0..128 and the city's purpur sits at y=136, so `port_triage` refuses the window rather than
comparing a truncated one.)

The jungle-temple number **goes up by five**, and that is reported rather than tuned away. The
13 cells that move were read off the two dumps and against the native chunk:

| moved cells | where | native | judgement |
|---|---|---|---|
| 4 `short_grass`/`fern` → `air` | `(1199,71,-656)`, `(1199,72,-651)`, `(1196,72,-645)`, `(1196,72,-642)` | `air` | **newly right** |
| 9 `air` → `oak_leaves[distance=2..5]` | `x=1198..1199, y=73..75, z=-656..-655` | `air` | **newly wrong** |

so 116 + 9 − 4 = 121. Both sides place **zero** cocoa cells in the chunk (measured:
`FEATURE_WRITE_TRACE` on the port, and the native chunk scanned for the cocoa family), so nothing
about the cocoa positions moved. The **hypothesis** is a *draw-count* cascade: with the correct
plane order a cocoa pass takes `isAir` decisions in a different sequence, spends a different
number of `nextInt(3)` AGE draws, and every later tree in the same pass shifts. That attribution is
**inherited, not re-provable**: the two revert variants' sources were not kept, and the
before/cocoa-only dumps share one sha256 (`70b3afe0…`), so "cocoa walk only" and "both reverted"
are the same artifact. The 9 leaves are also *new* differences, not cells that existed and were
carried — "the correct rule applied to an already-broken world" is a reading, not a measurement.
The order itself is settled by `ji$c.<clinit>` and is not ambiguous. The number is recorded with
the cells so the lane that owns the east-edge canopy can re-measure it; that the east-edge fix will
*lower* the number is a hypothesis, not a proven cause.

### 独立再測定と受入判定（2026-11-16, integration acceptance audit）

統合候補のバイナリで、この節の数値と機構を独立に取り直した。

- native の配列は jar 側で再確認済み: `ji$c.<clinit>` は
  `anewarray` に `ji.c, ji.f, ji.d, ji.e` を順に積み、`ji.<clinit>` の enum は
  `a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST`。よって HORIZONTAL = NORTH, EAST, SOUTH, WEST。
  曖昧さは無い。
- `port_triage … 74 -41 --n 1 --mode full` で **121 / 98304**。
  `horizontal_dir` だけを逆向き配列に戻したビルドで **116 / 98304**。差は主張どおり +5。
- 121 側の差分内訳も一致: `air -> oak_leaves` が 9 セル
  （`x=1198..1199`, `y=73..75`, `z=-655/-656` 付近）、`short_grass`/`fern -> air` は 11 セル
  （うち 4 が新規に正しい）。
- **機構の切り分け（継承・再証明不能）**: `horizontal_dir` は正しいまま、cocoa decorator の walk
  （`for f in plane_horizontal()`）**だけ**を旧配列に戻すと **116** に戻る、という記録がある。
  ただし revert した2変種の source は保存されておらず、before と cocoa-only の dump は
  **同一 sha256**（`70b3afe0…`）であるため、「cocoa だけ戻した」と「両方戻した」は保存物から
  区別できない。よって「+5 の全量が cocoa pass に帰属」は**継承主張**で、独立再現はできない。
- **cocoa は両側で 0 セル**: port の dump と native の保存チャンクの両方で
  chunk (74,-41) の cocoa 状態は 0。位置は動いていない（レビューも独立に確認）。

受入判定: **コンポーネントとしては ACCEPT**（配列順は native と一致し、契約テストと変異で
押さえられている）。ただし **fixture の指標としては HOLD**（jungle が 116 -> 121）。
この 2 つは別物として扱う。121 の内訳は「port が native に無い leaves を 9 セル置き、
`short_grass`/`fern` の 4 セルが新たに正しくなった」であって、差分セル数が増えたこと自体は
良し悪しの判定ではない。east-edge の canopy 欠陥が直れば数字が下がる、というのは
**仮説**で、測定された原因ではない。再測定の条件は「east edge の樹が native と一致すること」。
座標を動かして数字を合わせることはしない。

The other five fixtures move zero cells, and the reason is measured, not assumed:

```
FEATURE_ATTEMPT_TRACE=1 ./target/debug/mc-spine   # per placed-feature attempt
FEATURE_WRITE_TRACE=all  ./target/debug/mc-spine   # per block write
```

| fixture | `disk_*` attempts | `sand`/`gravel`/`clay` writes | `blue_ice` attempts | mangrove attempts |
|---|---|---|---|---|
| jungle-temple | 27 | **0** | 0 | 0 |
| nether-fossil | 27 | **0** | 0 | 0 |
| village-taiga | 27 | **0** | 0 | 0 |
| stronghold | 27 | **0** | 0 | 0 |
| ruined-portal | 27 | **0** | 0 | 0 |
| nether-fortress | 0 | 0 | 0 | 0 |
| end-outer | 0 | 0 | 0 | 0 |

`disk_*` is *attempted* in five fixtures (`count 3` × 3 features × 9 chunks) and writes nothing,
because the origin filter (`matching_fluids water` at `OCEAN_FLOOR_WG`) rejects every candidate:
`OCEAN_FLOOR_WG`'s origin is the cell *above* the floor, so it is water only under an ocean, river
or pond, and none of these chunks has one. `blue_ice` needs `frozen_ocean`/`deep_frozen_ocean` and
mangrove needs `mangrove_swamp`; neither biome is in any fixture. So the world-level measurement
*for these three features* is 0 by construction, and the correctness evidence is the contract
tests plus the proved mutations — not the fixture diff count (CV1: a change that leaves the metric
bit-identical is not evidence, which is why every fix has a test that fails on the old code and a
`mutation_check.sh` entry that proves the test can fail).

## Proved mutations

```
$ MUTATION_FILTER="disk_walks" bash tools/mutation_check.sh --offline
ok    a disk walks its radius box in betweenClosed order (z slowest)
$ MUTATION_FILTER="attached_to_leaves_draws" bash tools/mutation_check.sh --offline
ok    attached_to_leaves draws its probability before the required-empty check
$ MUTATION_FILTER="blue_ice_spends" bash tools/mutation_check.sh --offline
ok    blue ice runs exactly two hundred attempts
$ MUTATION_FILTER="horizontal_plane_order" bash tools/mutation_check.sh --offline
ok    the horizontal plane is walked north, east, south, west
$ MUTATION_FILTER="cocoa_binds" bash tools/mutation_check.sh --offline
ok    cocoa binds its draws to the plane order, not the reversed array
```

## Open (found, measured, deliberately not fixed)

* **`BlockBlobFeature` (`forest_rock`)** — the port's body is a sketch: it ascends when the cell is
  not air, breaks on `solid`, and places one blob. The reference descends while (the cell below is
  air) *or* (below is neither `BASE_STONE_OVERWORLD` nor `DIRT`), then runs **three** blob
  iterations of `nextInt(2)` × 3 → `f = (a+b+c)/3 + 0.5`, a `betweenClosed(origin.offset(-a,-b,-c),
  origin.offset(a,b,c))` ball tested against `distSqr <= f*f`, and shifts the origin by
  `(nextInt(2)-2, -nextInt(2), nextInt(2)-2)` between iterations. `forest_rock` is in no fixture,
  and the correct port needs its own draw-order lane; it is listed here so the next audit does not
  re-derive it.
* **`BeehiveDecorator`** — the reference calls `java/util/Collections.shuffle:(Ljava/util/List;)V`,
  the **single-argument** overload, which shuffles with a shared unseeded `java.util.Random`. A
  reference whose own order depends on the JVM's `Random` cannot be matched by a seeded
  Fisher–Yates; the port is deterministic and stays that way. Recorded with the signature so the
  claim can be re-checked, not "fixed" into a guess.
* **`ShipwreckPieces` / `OceanRuinPieces` / `StructureTemplate.fillFromWorld`** — the
  `betweenClosed` walks write stored states and draw nothing; they are order-insensitive and were
  read only to confirm that.

## Reproducing

```
# one class, disassembled (pinned jar + mappings)
tools/bytecode_probe.sh net.minecraft.world.level.levelgen.feature.DiskFeature

# the plane array itself
tools/bytecode_probe.sh 'net.minecraft.core.Direction$Plane'

# the scan over all 414 worldgen classes (javap once, then the signatures above)
python3 tools/feature_order_scan.py \
    --jar /home/yuu/mc-spine-local/captures/native/mc-native-overworld-20261111/server.jar \
    --mappings /var/tmp/mcm/server.txt --verify-sha1

cargo test --offline --locked --bin mc-spine -- --test-threads=1
bash tools/mutation_check.sh --offline
```
