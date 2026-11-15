# Procedural piece writes go through the chunk's postprocess shape pass

Source baseline: `4611f49` on `t3/c52386e8`, branched from `perf/worldgen-parallel`.
Target worlds: the authenticated 1.21.1 server (`server.jar` SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`), seed 4242, captured with
`tools/carve-agent/native_fixture.sh`:

| capture | dimension | chunk(s) | stage | `world/` |
|---|---|---|---|---|
| `/tmp/nat-4242-stronghold` | `minecraft:overworld` | `(-97, 34) + 3x3` | `structures` | `world` |
| `/tmp/nat-4242-nether-fortress` | `minecraft:the_nether` | `(-25, 6)` | `structures` | `DIM-1` |

`tools/port_triage.py --mode full` reads them; `--preset nether` selects
`DIM-1`.

## Symptom

Two residual classes the previous two commits isolated, both of them
connectible blocks placed by a *procedural* `StructurePiece` (a piece that
writes blocks with `placeBlock`, not a template):

- chunk `(-96, 35)` had 4 `oak_fence` cells at `x = -1536`: native
  `[east,north,west]` beside two `oak_log` pillars and a fence, and *no* bit at
  all at `z = 562`. The port kept the single bit its placement call passed
  (`mscorridor#130`, `#161`).
- chunk `(-25, 6)` had 16 `nether_brick_fence` cells with native
  `[east,north,south,west]` — one more bit than the placement state — at the
  fortress room's edge (`nebs#1`, `nebs#2`).
- the 3x3 grid around `(-97, 34)` also carried 4 `iron_bars` cells
  (`shpr#25`, `shrc#15`) and ~14 more mineshaft fence cells of the same shape.

## The native rule (decompiled, pinned build)

`tools/vanilla_src.sh net.minecraft.world.level.levelgen.structure.StructurePiece
…structures.MineshaftPieces …structures.NetherFortressPieces
…structures.StrongholdPieces`, plus `chunk.LevelChunk`, `chunk.ProtoChunk`,
`chunk.ChunkAccess`, `server.level.ChunkMap`, `block.MultifaceBlock`:

```
ProtectedThreeArgStructurePiecePlaceBlock (ejv.a(dds, dtc, int, int, int, ejj))
   if (box.isInside(pos) && canPlace(world, x, y, z, box)) {
      state = rotate/mirror(state);
      world.setBlock(pos, state, 2);            // 2 = UPDATE_CLIENTS, NO neighbour updates
      …fluid legacy block…
      if (SHAPE_CHECK_BLOCKS.contains(state.getBlock()))
         world.getChunk(pos).markPosForPostprocessing(pos);
   }

ChunkMap.protoChunkToFullChunk (aqb.b(apz))
   …this.a(holder, 1, chunk -> ChunkStatus.FULL)…    // the whole 3x3 must be FULL
   on the main thread: levelChunk.H()                // LevelChunk.postProcessGeneration

LevelChunk.postProcessGeneration (dvi.H)
   for each section ascending, for each marked offset in mark order:
      state = getBlockState(pos);
      if (!(state.getBlock() instanceof LiquidBlock))
         level.setBlock(pos, Block.updateFromNeighbourShapes(state, level, pos), 20);  // 4|16
```

Two consequences the port did not model:

1. Placement flag `2` means the *placement* state is never neighbour-updated, so
   every hardcoded bit survives until the postprocess pass. The bits native
   stores are `updateFromNeighbourShapes` against the world **as it stands when
   the chunk is promoted**, not against the world at write time.
2. `ChunkMap` asks for `ChunkStatus.FULL` over radius 1 before it promotes a
   chunk, so a chunk's postprocess runs only once the whole 3x3 around it has
   finished generating. Measured on chunk `(-96, 35)` of seed 4242: the fence at
   `(-1521, -21, 560)` reads its east neighbour as `deepslate` while only that
   chunk has generated, and as the mineshaft corridor's `cave_air` once
   `(-95, 35)` (which decorates later, `order.tsv` line 8) has. Native's stored
   bits are the second answer, so an "end of this chunk's own generation" flush
   is wrong — it takes `(-96,35)` from 6 diffs to 4 instead of 0.

`SHAPE_CHECK_BLOCKS` is eleven `Blocks` identities, resolved through the
official mappings (`dga.fo/cp/cq/dU/kd/ki/kg/ke/kf/cO/eW`):
`nether_brick_fence`, `torch`, `wall_torch`, `oak_fence`, `spruce_fence`,
`birch_fence`, `jungle_fence`, `acacia_fence`, `dark_oak_fence`, `ladder`,
`iron_bars`. Not a name pattern: `crimson_fence`, `warped_fence`,
`mangrove_fence`, `cherry_fence`, `bamboo_fence`, the panes and the
soul/redstone torch family are absent.

## The second class was a different rule than the earlier guess

`4611f49` recorded the two `glow_lichen` cells at `(-1521, -30, 573..574)` as
"the feature reads a cell in the neighbouring chunk that native's region-radius
write has already cleared and the port's per-chunk write clip has not". That is
wrong. The cells are written by the `minecraft:trial_chambers` template itself,
with `east=true`; their support cell `(-1520, -30, 573..574)` lies outside the
placement box, so it is an *exposed face* of the written voxel set. Native's
`updateShapeAtEdge` applies `MultifaceBlock.updateShape` there:

```
MultifaceBlock.updateShape (dky.a(dtc, ji, dtc, dcx, jd, jd))
   if (!hasAnyFace(state)) return AIR;
   return hasFace(state, direction) && !canAttachTo(level, direction, neighborPos, neighborState)
        ? removeFace(state, direction)      // AIR once no face is left
        : state;
```

The port's `structure_template::update_shape` had the fence, pane, wall, stair,
door, chest, crop and torch families but not this one, so the face was never
cleared. `canAttachTo` is `isFaceFull(supportShape) || isFaceFull(collisionShape)`
in the face's direction, which is the pinned `assets/multiface_support_1211.bin`
table (`BReg::supports_multiface`) the sculk/glow-lichen features already use.

## What changed

No layer edge. `evidence/foundation/module-layers/policy.json` is untouched:
`mineshaft_piece` (layer 5) still cannot see `structure_template` (layer 3).

- the shape rule stays in `structure_template`:
  `MultifaceBlock.updateShape` inside `update_shape`, and
  `postprocess_shape_marks` = one `postProcessGeneration` cell loop over a mark
  list;
- "this write needs a postprocess" is a write-layer fact, so the four
  `placeBlock`-shaped helpers (`mineshaft_piece::Writer::put`,
  `fortress_monument::PieceWorld::place`,
  `stronghold_structures::StrongholdWriter::place`,
  `single_piece_structures::PieceWriter::place_block`) record it on the write
  layer: `FC::mark_shape` → `Gen::store_shape_mark` (per owning chunk, mark
  order), with `BReg::needs_postprocess_shape` owning the eleven-block set.
  Direct `level.setBlock` calls are not marked, matching native
  (`fillColumnDown`, chest/spawner writes);
- `feature::populate` flushes a chunk's list when `Gen::shape_marks_ready` says
  its 3x3 has generated (`protoChunkToFullChunk`'s radius-1 `FULL`), in section
  order then mark order (`sort_by_key` is stable), and takes the bucket so it is
  consumed once.

## Measured

`tools/port_triage.py … --mode full`, seed 4242:

| target | before | after |
|---|---|---|
| stronghold `(-96, 35)` | 6 | **0** |
| nether fortress `(-25, 6)` (`--preset nether`) | 16 | **0** |
| stronghold `(-97, 34) + 3x3` grid total | 725 | 686 |
| … of which structure-attributed | 40 | **1** |

The 685 remaining grid cells are the pre-existing underground pair differences
(`andesite`↔`diorite`/`granite` 340+196+122+11+8+6, `gold_ore`→`stone` 8,
`iron_ore`→`gravel` 1, `deepslate`→`deepslate_gold_ore` 1) and do not move. The
one structure-attributed cell is also pre-existing and unchanged, cell for cell,
from the `4611f49` baseline: `glow_lichen[down=true]` at `(-1541, 0, 544)`
(`shlt#26`) where the port has `cave_air` — native grows a multiface face the
port's `glow_lichen`/`sculk_vein` feature does not, which is a different
(configured-feature) rule and stays out of this change.

Regression check on the other captures (same `tools/port_triage.py`, just the
grid total): ruined portal `(5,6)+3x3` 379 → 377, jungle temple `(73,-42)+3x3`
14913 → 14910, nether fossil `(-25,19)+3x3` 62 → 62. The improvements are the
same two rules (a `glow_lichen` face and a fence bit); nothing got worse.

## Tests and gates

- `worldgen::wg_tests::structure_chunk_matches_native_capture_state_for_state`:
  new fixture `tests/data/stronghold_chunk_-96_35_1211.tsv.gz` (31,721
  non-`air` cells of chunk `(-96,35)`, canonical states, produced by
  `tools/native_chunk_capture.py`) compared cell for cell. It pins the four
  fence cells and the two removed `glow_lichen` cells. The test has to register
  the beard resolver the process entry point registers; without it the trial
  chamber's `encapsulate` shell moves and 300 unrelated cells differ.
- `structure_template::template_structures_tests`: the eleven-block set
  (including the look-alikes that must not be in it), a multiface face cleared
  while its sibling face survives, the last-face-goes case for `glow_lichen` and
  `sculk_vein`, and a marked fence recomputed while a liquid cell in the same
  list is skipped.
- `worldgen::wg_tests::shape_marks_wait_for_the_whole_three_by_three_and_are_consumed_once`:
  the deferral and the consume-once property.
- `tools/mutation_check.sh --offline`, four entries, all proved: `&& false` on
  the multiface support test, `iron_bars`→`glass_pane` in the block set, the
  mineshaft mark call disabled, and the 3x3 readiness gate forced open. The last
  two are caught by the chunk capture, so that fixture is a gate rather than a
  green light.

`cargo test --offline --locked --bin mc-spine -- --test-threads=1` 509 passed /
0 failed / 1 pre-existing ignored; `tools/layers_check.sh` PASS (42 modules, 108
edges, unchanged); `tools/debt_ratchet.sh` 0 over limit.

## Still open

- 685 underground pair cells in the `(-97,34)+3x3` grid (andesite/granite
  noise, plus `gold_ore`/`iron_ore`/`deepslate_gold_ore` singles).
- the one `glow_lichen[down=true]` cell at `(-1541, 0, 544)`.
- `postprocess_shape_marks` skips a marked cell whose state is `water`/`lava`
  (`!(getBlock() instanceof LiquidBlock)`) but does not run the fluid `tick` the
  same loop performs first; no measured structure marks a fluid cell, so the
  difference is unobserved rather than proven absent.
