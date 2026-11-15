# Village Taiga (4,-32)+3×3: the lake anchor and the vegetation-patch surface set

Source baseline: `4611f49` on `t3/aef0d8c5`. Target world: the authenticated 1.21.1
server (`server.jar` SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`, mappings
`server.txt` SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`), seed 4242,
`/tmp/nat-4242-village-taiga/world` (biomes `taiga` + `lush_caves`, structures on).

```
python3 tools/port_triage.py /tmp/nat-4242-village-taiga/world 4 -32 --n 3 --mode full --samples 3
```

| build | `all_air_kinds` | note |
|---|---|---|
| baseline `4611f49` | **6868** | |
| `Lake` grid anchor | 6037 | −831 |
| vegetation-patch surface set (HashSet order) | **5390** | −714 |

Regressions unchanged by both changes (re-run with the final binary):

| world | command | before | after |
|---|---|---|---|
| `/tmp/nat-4242-ruined-portal/world` | `6 7 --mode full` | 0 | **0** |
| `/tmp/nat-4242-stronghold/world` | `-96 35 --mode full` | 6 | **6** (pre-existing) |
| `…/mc-native-ow-seed4242-20261111/world` | `0 0 --mode full` | 0 | **0** |

## `LakeFeature` anchors its grid with `below(4)`, not `offset(-8,-4,-8)`

`Cf::Lake` (`src/feature.rs`) placed every lava lake eight blocks diagonally. The
decompiled `LakeFeature.place` reads

```java
if ($$1.v() <= $$2.I_() + 4) return false;   // pos.getY() <= minBuildHeight + 4
$$1 = $$1.c(4);                              // anchor
...
jd $$30 = $$1.b($$27, $$29, $$28);           // world cell = anchor.offset(x, y, z)
```

`jd.c(int)` is `relative(ji.a, n)`; `ji.a` is `DOWN` (both `jd.c(int)` and
`jd.e()` resolve to that constant, and the lake's own geometry proves it: the
native fluid sits in rows `pos.y-4 .. pos.y-1` with the origin's x/z unshifted).
So the anchor is `(pos.x, pos.y - 4, pos.z)` — there is no x/z shift, and the grid
spans `pos.x .. pos.x+15` / `pos.z .. pos.z+15`.

Measured on seed 4242 chunk `(5,-32)` (decoration seed `17077345527266405634`,
`gi=1`, `step=1` — the only chunk in the 3×3 whose `rarity_filter(200)` passes, at
`next_f32() = 0.003581`, `in_square = (1, 4)` → origin `(81, 68, -508)`):

| | fluid (`lava[level=0]`) | `cave_air` |
|---|---|---|
| port before | x 75..86, y 65..67, z −513..−503 | x 75..86, y 68..70, z −513..−503 |
| port after | x 83..94, y 65..67, z −505..−495 | x 83..94, y 68..70, z −505..−495 |
| native | x 83..94, y 65..67, z −505..−496 | x 83..94, y 68..70, z −505..−496 |

The missing `pos.getY() <= minBuildHeight + 4` guard is added at the same time.

Guard: `lake_grid_anchor_is_down_only` + `lake_above_the_bottom_guard`
(`src/generation_contract_tests.rs`), with a mutation entry that restores the
`-8` shift and is proved to fail the test. The pre-fix port also overwrote a
village `cobblestone` with its misplaced barrier stone, so this one cell error
carried structure diffs and, through `would_survive`/heightmap on `dirt_path`,
two spurious trees.

## The vegetation pass walks a `HashSet`, not the insertion order

`VegetationPatchFeature.placeGroundPatch` collects surface points into
`Set<BlockPos> set = new HashSet<>()`, and `distributeVegetation` walks that set:
the per-point `vegetation_chance` draw is bound to points in **hash-bucket order**.
The port used insertion order, which keeps every moss block in place and swaps
which point receives which vegetation (`short_grass` ↔ `moss_carpet` `50/49`,
plus `air -> short_grass` `125`, `air -> moss_carpet` `61` on this fixture).

`java_hashset_order` models `HashMap` exactly: bucket = `spread(h) & (cap-1)` with
`spread(h) = h ^ (h >>> 16)` and `Vec3i.hashCode() = (y + z*31)*31 + x`
(`kh.hashCode` bytecode); capacity doubles from 16 while `size > 0.75*capacity`;
a shared bucket keeps insertion order because `HashMap` appends to the bin tail.
The expected sequences in
`java_hashset_order_matches_java_util_hashset_iteration` are **measured** on the
pinned jar (a default-package `HashSet<BlockPos>` probe against the unsigned inner
1.21.1 server jar), for 25 points (capacity 64) and 197 points (capacity 512).

## Two smaller vanilla mismatches in the same arm

- `placeGround` skips a position whose provider produced the block already there
  and *continues* the depth loop; the port broke out of it. No effect on the
  current datapack (every patch `ground_state` is a constant provider), fixed for
  the next one.
- `WaterloggedVegetationPatchFeature` filters its surface set, then fills, then
  walks the survivors — two loops over the set, and the vegetation position is
  `pos.below().relative(surface.getDirection().getOpposite())`. The port
  interleaved the fill with the filter (so a filled point could make the next
  point look exposed) and placed waterlogged vegetation at the surface point.
  `isExposed` itself is NORTH/EAST/SOUTH/WEST/**DOWN** (`ji.c`, `ji.f`, `ji.d`,
  `ji.e`, `ji.a`), which the port already had; changing it to UP costs +735.

## What is still not matching (and why this lane stops here)

Remaining `5390`, by the responsible port feature (`FEATURE_WRITE_TRACE=all`
attribution over the 3×3 dump):

| diffs | feature | shape |
|---|---|---|
| 1853 + ~690 missing | `trees_taiga` | trees at different columns |
| 658 | `lush_caves_ceiling_vegetation` | moss/vines where native has stone/tuff/andesite |
| 512 | `lush_caves_vegetation` | moss where native has deepslate; vegetation type |
| 387 missing | `lush_caves_clay` | native clay the port never wrote |
| 179 | `lush_caves_clay` | port clay where native differs |
| 134 | *(none)* | native `cave_vines_plant` the port never wrote |

- **Trees are downstream of the village, not `tree.rs`.** Every chunk *without* a
  `village_taiga` piece matches exactly — `(4,-31)` 61/61 trunk columns with
  identical heights, `(5,-30)`, `(6,-30)` too. The mismatching columns are all
  inside village chunks and trace to a structure block: `(65,69,-498)` is native
  `dirt_path` (not in `#minecraft:dirt`, so vanilla's `would_survive` rejects the
  `spruce_checked` candidate) where the port has `dirt`, and `(71,68,-502)` is
  native `grass_block` versus port `dirt` at a candidate vanilla rejects in
  `TreeFeature`'s minimum-size scan. Both are jigsaw/template writes.
- **The ceiling patch covers more surface points than vanilla**, and the cause is
  not in `place`'s own logic: `VEG_TRACE=<cx>,<cz>` shows chunk `(5,-32)` has one
  ceiling patch at origin `(85,-2,-497)` `xr=7 zr=6` with 126 surface points, and
  the RNG for it is verified byte-for-byte, yet native has no moss on its western
  edge column (`78`). `java_hashset_order` and the exposure filter are ruled out
  above, so the next step is a native per-attempt trace
  (`tools/carve-agent/native_fixture.sh` with the agent's `SPLACE`/`SDRAW`
  windows) rather than more reading.

`FEATURE_CELL_TRACE=<x>,<y>,<z>`, `FEATURE_WRITE_TRACE=<cx>,<cz>|all`,
`FEATURE_ATTEMPT_TRACE=1`, `FSEED_TRACE=<cx>,<cz>`, `INSQ_TRACE`, `BIOMEF_TRACE`
and the new `VEG_TRACE=<cx>,<cz>` are the diagnostics this note is built from;
`FEAT_LIST=<step>` prints the per-step feature list with the global index used for
`setFeatureSeed`.
