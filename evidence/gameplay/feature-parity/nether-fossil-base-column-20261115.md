# Nether fossils are placed on the noise column, not the live chunk

Source baseline: `6bc17cd` on `t3/37d03552`. Target world: the authenticated
1.21.1 server (`server.jar` SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`),
seed 4242, captured with `tools/carve-agent/native_fixture.sh`:

| capture | dimension | chunk | stage | `world/` |
|---|---|---|---|---|
| `/tmp/nat-4242-nether-fortress` | `minecraft:the_nether` | `(-25, 6)` | `structures` | `DIM-1` |
| `/tmp/nat-4242-nether-fossil` | `minecraft:the_nether` | `(-24, 20)` | `structures` | `DIM-1` |
| `/tmp/nat-4242-nether-2320-carvers` | `minecraft:the_nether` | `(-23, 20)` | `carvers` | `DIM-1` |
| `/tmp/nat-4242-end-city` | `minecraft:the_end` | `(65, 45)` | `structures` | `DIM1` |
| `/tmp/nat-4242-end-outer` | `minecraft:the_end` | `(10, 10)` | `structures` | `DIM1` |

`tools/port_triage.py --preset nether|end` reads those directories; the dimension
selection is `--preset`/`--dim` over `<world>/DIM-1` / `<world>/DIM1` while
`level.dat` still comes from the world root.

## Symptom

Chunk `(-24, 20)` carried 263 differing cells, all inside the fossil's own
footprint: 16 `bone_block` cells the port wrote and vanilla did not, plus ~247
`soul_sand`/`soul_soil`/`cave_air` cells around them. The neighbouring chunks
were worse (`(-23, 20)`: 1422, `(-22, 20)`: 1592) even though the chunk with no
structure at all (`(-24, 21)`) was already 0 — the difference followed the
fossil, not the terrain.

Terrain itself was not the problem: `--mode carvers` against a carvers-stage
capture of the same chunk was **0 / 32768**, so the density, the surface rule and
the carvers already agreed. Everything that differed came from the structure
layer.

The native capture answers the question the previous commit left open: vanilla
places **no** `nether_fossil` at `(-24, 20)`, and places one at `(-22, 20)` with
a single child at `BB [-356, 55, 319, -350, 61, 324]`. The port had a fossil at
`(-24, 20)` at `y 38..44` and one at `(-22, 20)` at `y 78..84` — the right
columns, the right template, 23 blocks too high.

## The two wrong inputs

`NetherFossilStructure.findGenerationPoint` (`ema.a` in the 1.21.1 jar) draws
`x`, `z`, then a y from its `HeightProvider`, then scans
`ChunkGenerator.getBaseColumn(x, z, level, randomState)` downwards:

```java
int y = this.height.sample(rng, worldGenContext);   // uniform(above_bottom 4, below_top 4)
NoiseColumn column = generator.getBaseColumn(x, z, level, randomState);
while (y > minY) {
   BlockState above = column.getBlock(y);
   BlockState below = column.getBlock(--y);
   if (above.isAir() && (below.is(SoulSand) || below.isFaceSturdy(level, pos, UP))) break;
}
```

1. **`getBaseColumn` is not the live chunk.** `dzb.a(int, int, LevelHeightAccessor,
   RandomState)` builds a fresh 1x1 `NoiseChunk` with
   `DensityFunctions.BeardifierMarker` (no beardifier), runs `getBlockState` per
   cell, and substitutes `noise_settings.default_block` where the aquifer refuses
   a cell. No surface rule, no carvers, no features, no beardifier. mc-spine
   scanned `world.get`, which has all four. That is circular for this structure:
   `nether_fossil` is `terrain_adaptation: beard_thin`, and the Nether's upper
   density is soft enough (`final_density` = `squeeze(0.64 * ...)` with the
   `base_3d_noise` term alone near y 40..100) that the fossil's own beard moves
   the local surface by ~20 blocks. It also folded the stage order into the
   placement, because `feature::populate` pre-carves the 3x3 before
   `References::collect`: with the live scan, re-deriving the start after a
   populate moves it from `y 55` to `y 38`.
2. **`VerticalAnchor.BELOW_TOP` had the offset's sign flipped.** Vanilla resolves
   `below_top: n` to `minGenY + genDepth - 1 - n`; `template_structures.rs`
   computed `min_gen_y + depth - 1 + n`. `nether_fossil`'s height provider is
   `uniform(above_bottom 4, below_top 4)`, i.e. `[4, 123]` in the Nether, and the
   port drew from `[4, 131]` — a different bound, so a different `nextInt` result
   for the same RNG position, and a start y above the air pocket the scan is
   supposed to stop in. `generation_providers.rs` already had the correct
   formula for placed-feature `height_range`; the two now agree.

## Fix

- `Gen::base_column_state(x, y, z)` is `getBaseColumn`: `build_cols(cx, cz, false)`
  is the same interpolation grid as `cols` with `beard: None` and no
  `surf`/`wtop`/`mats` pass, cached in `base_c` (pure in seed/preset/chunk, so
  unlike `col_c` it may be cached while `beard_depth > 0`), and a cell is
  `density > 0 ? default_block : aquifer_or_default_block`.
- `TemplateWorld::base_column` exposes it; `nether_fossil_find` scans it and
  falls back to `world.get` only for a synthetic oracle scene with no generator.
- `VerticalAnchor::BelowTop` subtracts.

## Measured

`tools/port_triage.py --preset nether|end --mode full` (seed 4242, same binary
before/after except where noted):

| world / chunks | before | after |
|---|---|---|
| fortress `(-25, 6)` | 20 | 16 |
| fossil capture, 3x3 grid at `(-25, 19)` | 2956 | **62** |
| `(-24, 20)` | 263 | 0 |
| `(-23, 20)` | 1422 | 0 |
| `(-22, 20)` | 1592 | 0 |
| `(-25, 20)` | 185 | 0 |
| end outer island `(10, 10)` | 0 | 0 |
| end city `(65, 45)` | refused (273 out-of-window cells) | 0 in-window + 273 counted |

The Overworld full dump is unchanged: seed 4242 chunk `(0, 0)` is
0 / 98304 before and after (`mc-native-ow-seed4242-20261111`).

Three `can_place` rules the capture also exposed, fixed in the same commit:
`fire` (`BaseFireBlock.canSurvive` = sturdy up face below) and `soul_fire`
(`#minecraft:soul_fire_base_blocks` below) were falling through to the BushBlock
dirt rule, so `patch_fire`/`patch_soul_fire` placed nothing on netherrack (25
`soul_fire` in the fossil grid plus 1 there and 2 `fire` in the fortress chunk);
`MushroomBlock.canSurvive` is `#mushroom_grow_block` below, else
`getRawBrightness(pos, 0) < 13 && isSolidRender(below)`, and a dimension without
skylight reads block light 0 before `INITIALIZE_LIGHT` (24 cells in the fossil
grid, 2 in the fortress chunk, 4 over-placed — see below). The Overworld keeps
its measured uninitialized skylight of 15, so that arm is unchanged there.

## Remaining gaps in the same captures

- 52 `basalt[axis=y]` cells forming a mass at the z edge of `(-25, 21)`
  (`y 77..84`, `z 348..351`) that the port does not place, plus 4 single-cell
  basalt/magma mismatches in the same chunk. The mass reaches into a chunk the
  dump does not emit, so this needs a 2-chunk-wide capture to attribute.
- 16 `nether_brick_fence` cells at `(-25, 6)`: the port clears one connection
  face (east/west/north/south) on the fence pieces at the fortress room's edge.
- 2 `crimson_roots` cells (the port's `can_place` arm requires `#minecraft:nylium`
  below, which a soul sand valley chunk does not have).
- 4 mushrooms placed that vanilla refuses, in a `nether_wastes` chunk
  `(-25, 19)`: same `air` over `netherrack` as the 24 accepted ones, so the
  light input is not the discriminator there; not yet explained.
- The End City tower above `y = 127`: vanilla's `dimension_type/the_end.json` is
  `min_y 0, height 256` while its `noise_settings` window is 128, so 273 native
  cells (purpur, end stone bricks, stained glass, stairs) sit above the window
  mc-spine generates for the End. Within `0..127` the End City is exact. Closing
  this needs the End's chunk height to follow the dimension type, not the noise
  settings — a chunk-format change, not a placement fix.
