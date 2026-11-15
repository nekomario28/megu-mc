# Nether plant `mayPlaceOn`, `count_on_every_layer`, and the FEATURES write window

Three decompiled-and-measured contracts in the feature layer, on the pinned 1.21.1
server jar (`/tmp/mcmx/server-1.21.1.jar`, mappings `/var/tmp/mcm/server.txt`,
vineflower). One of them also falsifies a hypothesis that had been read off a
trace field, which is recorded below because the trace is easy to misread again.

## 1. The nether plants do not share one `mayPlaceOn`

`dmk` (`RootsBlock`) and `dld` (`NetherSproutsBlock`) are near-clones: both
override

```java
protected boolean mayPlaceOn(BlockState state, BlockGetter world, BlockPos pos) {
   return state.is(BlockTags.NYLIUM) || state.is(Blocks.SOUL_SOIL) || super.mayPlaceOn(state, world, pos);
}
```

and `dgh` (`BushBlock`) adds `state.is(BlockTags.DIRT) || state.is(Blocks.FARMLAND)`.
`dje` (`FungusBlock`) is the same **plus** `Blocks.MYCELIUM`; its `grows_on` block
(`crimson_nylium` / `warped_nylium`, from `dga`'s registration) gates only
`isValidBonemealTarget` (`dje.b(dcz,jd,dtc)`), not placement.

The port had one arm for all five/five blocks: `reg.in_tag(below, "minecraft:nylium")`.
That value was read off `NetherForestVegetationFeature` (`ecx`), which checks
`below.is(NYLIUM)` on the *origin* before it ever calls `canSurvive`, so
`tests/data/nether_forest_vegetation_1211.tsv` cannot see the difference - and
that fixture stays green after the split (2 mutation entries proved from the new
`RootsBlock`/`FungusBlock` contract test instead).

The pinned `#minecraft:dirt` tag is `dirt, grass_block, podzol, coarse_dirt,
mycelium, rooted_dirt, moss_block, mud, muddy_mangrove_roots` (from the jar's
`data/minecraft/tags/block/dirt.json`), so `dirt_path` and `pale_moss_block` are
**not** accepted here even though the shared `is_dirtish` helper used by the
generic BushBlock arm accepts them.

## 2. `CountOnEveryLayerPlacement` (`eim`)

```java
int layer = 0; boolean found;
do {
   found = false;
   for (int j = 0; j < this.count.sample(random); j++) {   // sampled per layer
      int x = random.nextInt(16) + pos.getX();
      int z = random.nextInt(16) + pos.getZ();
      int top = context.getHeight(Heightmap.Types.MOTION_BLOCKING, x, z);
      int y = getY(context, x, top, z, layer);
      if (y != Integer.MAX_VALUE) { builder.add(new BlockPos(x, y, z)); found = true; }
   }
   layer++;
} while (found);
```

with `getY` walking down from `top` and counting `!isEmpty(below) &&
isEmpty(cur) && !below.is(Blocks.BEDROCK)`, `isEmpty = isAir() || WATER || LAVA`.
The port drew the count once, used `OCEAN_FLOOR` (the heightmap every other
modifier here asks for), and re-implemented the walk inline.

Not claimed: the heightmap *kind* is unobservable through this walk. A layer
boundary is always at or below the first free cell above the topmost
non-empty block, and between `OCEAN_FLOOR` and `MOTION_BLOCKING` there are only
fluid cells (which `isEmpty` treats as empty), so no boundary can be skipped by
starting lower. `MOTION_BLOCKING` is still the pinned read; the mutation that
matters is the per-layer count sample, which the new test pins through the draw
stream.

## 3. The FEATURES write window, and the read-only hypothesis that failed

`arb` (`WorldGenRegion`) has two independent refusals:

* `ensureCanWrite` (`arb.f_`) drops a write further than the step's
  `blockStateWriteRadius` from the decorated chunk. `dvy` (`ChunkPyramid`) builds
  the FEATURES step as `features -> carvers@1 + structure_starts@8 + .a(1)`, i.e.
  radius **1 chunk**.
* The target chunk refuses when it is no longer a writable proto chunk:
  `ImposterProtoChunk.setBlockState` (`dvh.a(jd,dtc,Z)`) returns `null` if
  `allowWrites` is false. `ChunkMap` installs `new ImposterProtoChunk(levelChunk,
  false)` into every proto-status slot when a chunk is promoted to a
  `LevelChunk`.

The port modelled neither; it now models the first (`Gen::feature_write_allowed`,
checked at the top of `FC::set`). **It deliberately does not model the second**:
`allowWrites` flips at promotion, not when the chunk's own FEATURES pass returns,
and the offline generator has no promotion stage.

Measured, order-sensitive, on the pinned Nether fixture
(`tools/port_triage.py /tmp/nat-4242-nether-fossil/world -25 19 --n 3 --mode full
--preset nether --pop-order /tmp/rt-4242/nf-pop49.tsv --pop-order-scope recorded
--samples 3`, native A/B within the same run = 0):

| arm | grid diffs |
|---|---|
| before | 34 |
| treat "its pass finished" as read-only | **1183** |
| window only (this change) | 32 |

Native accepted writes from a later pass into an already-decorated neighbour that
was still a proto chunk - e.g. blackstone at `(-385,10,304)` written by pop
`(-24,19)` while `(-25,19)` had already finished its own pass - so "finished
pass => read-only" would refuse 1149 cells vanilla kept. The refused writes are
visible with `FEATURE_REFUSE_TRACE=<cx>,<cz>` (they print `FREFUSE`).

The trace field that suggested the read-only rule does not exist: `FCELL ... wrote=`
is `WorldGenRegion.setBlock`'s return value, which is `true` even when the inner
`ImposterProtoChunk` dropped the write. `tools/carve-agent/README.md` now says so.

## Measurement

Same fixture and command, port A/B with one binary per arm (cell-level, all nine
chunks):

| | before | after |
|---|---|---|
| ordered (`--pop-order …/nf-pop49.tsv`) | 34 | **32** |
| unordered | 62 | **60** |
| fixed cells | - | 2 (`crimson_roots` on soul soil at `(-386,85,350)`, `(-390,87,351)`) |
| regressed cells | - | **0** |

Regression fixtures (same `--mode full`, baseline binary vs this one, all
`native_ab_non_air_diffs = 0`): stronghold `(-96,35)` 6 -> 6, ruined portal
`(6,7)` 0 -> 0, end outer `(10,10)` 0 -> 0, Overworld `(0,0)`
(`/tmp/nat-4242-village-taiga`) 1 -> 1.

## Not claimed / residual

The 32 remaining cells in the ordered run are 28 `large_basalt_columns` and 4
`brown/red_mushroom_nether` cells, and neither is one of the three contracts
above:

- the 26 `air -> basalt[axis=y]` cells are written by pop `(-25,22)`, whose other
  107 writes into that chunk match native, so this is not a chunk-level refusal
  (a refused chunk drops all of them) but a column-placement difference inside
  `BasaltColumnsFeature`; no native per-position trace exists for this world
  (`run.log` carries `FCELL` only), so the next step there is a feature-position
  oracle, not a guess;
- the 4 mushroom cells are `SimpleBlockFeature.canSurvive` on netherrack, where
  the port's `MushroomBlock.canSurvive` reads raw brightness 0 in a dimension
  without skylight (`FC::block_light`) and native evidently did not; the native
  chunk does contain 24 matching mushrooms, so the placement origins agree.

One seed, one dimension, one host.
