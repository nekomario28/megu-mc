# The tree's trunk dirt, the decorator list order, and the live heightmap dump

Two contracts in `TreeFeature`'s decorator hand-off and one diagnostic that was
printing its own scan bound. Measured against the pinned 1.21.1 jar
(`/home/yuu/mc-spine-local/captures/native/mc-native-overworld-20261111/server.jar`,
mappings `/var/tmp/mcm/server.txt`, `tools/bytecode_probe.sh`).

Both contracts are RNG-stream contracts, not cosmetics: `TrunkVineDecorator` draws
four `nextInt(3)` per entry of the tree's log list and `LeaveVineDecorator` four
`nextFloat` per entry of its leaf list, so a short list or a differently ordered one
moves every later decorator draw (and, because a vine inside a later tree's
`minimum_size` scan rejects that tree, can move whole trees).

## 1. `setDirtUnderneath` writes through the *log* setter

```
$ tools/bytecode_probe.sh net.minecraft.world.level.levelgen.feature.trunkplacers.TrunkPlacer
protected static void a(ddc level, BiConsumer<jd, dtc> setter, ayw random, jd pos, efe config)
   0: aload 4 ; 2: getfield efe.k:Z ; 5: ifne 16        // config.forceDirt()
   8: aload_0 ; 9: aload_3 ; 10: invokestatic ehm.c     // level.getBlockState(pos).is(<tag>)
  13: ifne 33                                           // ... then skip the write
  16: setter.accept(pos, config.dirtProvider().getState(random, pos))
```

`ehm.c` resolves to `awe.u`, and `awe` is `BlockTags` with `u -> net.minecraft.tags.TagKey LOGS`
(the mapping line `net.minecraft.tags.TagKey LOGS -> u`). So the skip condition is
"the cell below the trunk is already a log", **not** the dirt tag; `dirt_path` /
`grass_block` / etc. are irrelevant to it.

The `BiConsumer` is the one `placeLog` gets. `TreeFeature.place` (`edq`) creates two
setters - `var10`/`var4` for the root placer's `roots` set and `var11`/`var5` for the
trunk placer - and `TreeDecorator.Context` is built as
`(level, decoratorSetter, random, var7 /*logs*/, var8 /*leaves*/, var6 /*roots*/)`.
`StraightTrunkPlacer.placeTrunk` calls `setDirtUnderneath` with the same consumer it
passes to `placeLog`, so the dirt position lands in `var7`, the log list.

Measured: a height-10 `jungle_tree` at `(1175,68,-653)` in the pinned jungle fixture
drew **40** `nextInt(3)` in the port and **44** in the reference (`SPLICE`/`FWDRAW`
stream `s=3926`, `pass=73,-41`) - 44 = 11 entries = 10 logs + the dirt below the trunk.
The port also wrote the dirt, it just did not record it.

`TrunkPl::generate` now pushes the position it writes, and `set_dirt` returns without
writing when the cell is a log (unless `force_dirt`). `TreeExtra::force_dirt` is read
from the pinned `force_dirt` field instead of being assumed false.

## 2. `TreeDecorator.Context` orders the lists: `HashSet` bucket order, then `y`

`eha$a`'s constructor (`TreeDecorator.Context`):

```
19: new ObjectArrayList ; 24: aload 6 (roots)  ; 26: invokespecial <init>(Collection) ; -> f
32: new ObjectArrayList ; 37: aload 4 (logs)   ; 39: invokespecial <init>(Collection) ; -> d
45: new ObjectArrayList ; 50: aload 5 (leaves) ; 52: invokespecial <init>(Collection) ; -> e
58..70: d.sort(Comparator.comparingInt(jd::v))    // BlockPos.getY
73..85: e.sort(...)  ; 88..100: f.sort(...)
```

The sets are filled by `Sets.newHashSet()` in `edq.place`, so three things happen that
insertion order does not model:

* duplicates collapse (`HashSet`);
* `ObjectArrayList(Collection)` walks the set in `HashMap` **bucket order**;
* the sort by `y` is stable, so equal-`y` ties keep that bucket order.

`Vec3i.hashCode` (read off `kh.hashCode`, `BlockPos` does not override it) is
`(getY() + getZ() * 31) * 31 + getX()` with Java's wrapping `int`; the bucket is
`(h ^ (h >>> 16)) & (cap - 1)` with a default table of 16 doubling while
`size > cap * 0.75`. `tree.rs::decorator_list` reproduces exactly that
(`java_blockpos_hash`, bucket append in insertion order, stable sort by `y`) and is
applied to copies, because `TreeFeature.updateLeaves` is handed the unsorted sets.

Why it matters here, concretely: the foliage placer emits rows from the top down, so
without the order the port's leaf list is the reference's list reversed. `leave_vine`
draws four `nextFloat` per leaf, so the drawn values were assigned to the *wrong*
leaves. One wrongly ordered canopy put a jungle tree's leaves 18 blocks above the
reference's terrain, which moved a later `would_survive(oak_sapling)` candidate from
`y=68` to `y=87` and rejected a tree the reference placed
(`BLOCKPRED 1170 87 -652 ok=false below=minecraft:jungle_leaves`).

## 3. `MC_SPINE_HM_DUMP` printed its scan bound, not the heightmap

The old dump printed `g.top_oc(x,z).max(g.feature_height(x,z,1))` /
`g.top_ws(x,z).max(g.feature_height(x,z,0))`. `feature_height` is backed by
`note_feature_write`'s per-column maxima, which are *scan bounds*: max-only (a removal
never lowers them), they ignore the type's predicate (a leaf above the floor is not an
`OCEAN_FLOOR` answer for a predicate that rejects it) and they cannot see carver
writes (those live in the override map, not the tally). The dump also never read the
live map a feature actually reads.

`MC_SPINE_HM_DUMP` now lives in `generation_context::hm_dump` (module layers: `worldgen`
may not reach for the generation context) and prints, per column,
`HM <lx> <lz> <ocean_floor> <world_surface> <motion_blocking> <motion_blocking_no_leaves>`
through `FC::hm_oc` / `FC::hm_ws` / `FC::heightmap`, i.e. the same accessors a feature
placement reads and the same quantity `MC_SPINE_WG_DUMP_HM` / `port_triage --port-hm`
writes. Checked column-for-column against `--port-hm` on chunk (74,-41): 0 of 256
differ.

Live-map fidelity against the saved vanilla chunk (same 3x3 population order):

`before` is the pre-change binary run through the same `--port-hm` (`OCEAN_FLOOR`); the
`WORLD_SURFACE` 89 is Track 4I's number for the same fixture, and the other two are new.

| heightmap | before | after |
|---|---|---|
| `OCEAN_FLOOR` | 77/256 columns | **14/256** |
| `WORLD_SURFACE` | 89/256 columns (Track 4I) | **18/256** |
| `MOTION_BLOCKING` | - | 14/256 |
| `MOTION_BLOCKING_NO_LEAVES` | - | 1/256 |

All 14 remaining columns are `x=1197..1199`, the chunk's east edge, and the port's
value is **lower** (`-3..-10`) than the reference's: a canopy the reference has and
the port does not (see the residual below). One measured fact worth keeping: leaves
count for `WORLD_SURFACE`, `OCEAN_FLOOR` *and* `MOTION_BLOCKING`, and only
`MOTION_BLOCKING_NO_LEAVES` excludes them - the pinned chunk (73,-41) column
`(1170,-652)` stores 70/70/70/69 over `jungle_log@68, oak_leaves@69`. The port's
masks already agreed; `live_heightmap_types_differ_on_leaves` now pins it.

## Measurement

`tools/port_triage.py /tmp/nat-4242-jungle-temple/world 74 -41 --mode full --port-hm`.
The fixture's recorded population order (`order.tsv`) is exactly the port's default
3x3 row-major order, which is why `--pop-order` never changed the number.

| arm | differing cells / 98304 | OC columns | WS columns |
|---|---|---|---|
| baseline (`6cdc2c7`) | 732 | 77 | 89 |
| + dirt joins the log list | 489 | - | - |
| + decorator list order (no `HashSet`) | 419 | 78 | - |
| + decorator list order (with `HashSet` bucket order) | **145** | **14** | **18** |

Regression fixtures (same command, baseline binary vs this one):

| fixture | before | after |
|---|---|---|
| stronghold `(-96,35)` | 0 | **0** |
| ruined portal `(6,7)` | 0 | **0** |
| village taiga `(0,0)` | 1 | **1** |
| nether fossil `(-25,19)`+3x3 (unordered) | 62 | **60** |
| seed-0 overworld `(0,0)`+2x2 | 11 | **11** |
| seed-0 overworld `(-2,-2)`+3x3 | 564 | **564** |

The nether fossil's *ordered* 32-cell number from
`nether-roots-and-layer-placement-20261116.md` used a 49-chunk plan
(`/tmp/rt-4242/nf-pop49.tsv`) that is not on this host; only the unordered 3x3 is
comparable, and it improved.

`feature_population_output_matches_the_pre_refactor_capture`'s digest was re-pinned
(`b680a4c9...` -> `10832b85...`) with the note in the test: the visible blocks of its
own windows are unchanged, so the capture moved with the write *set* in chunks those
windows do not cover.

## Residual (not claimed as fixed)

The remaining 145 cells of chunk (74,-41), by last writer:

| writer | cells | reading |
|---|---|---|
| no feature write (native has `fern`/`short_grass`/leaves there) | 80 | decorations the port did not place |
| `(11,9)` `patch_grass_jungle` | 14 | its own placement stream diverges |
| `(76,9)` | 16 | vines at the temple's west face |
| `(3,7)` `large_dripstone` | 28 | dripstone one to two cells off, at the chunk edge |
| `(4,4)`, `(16,6)` | 7 | tripwire/ore head-room |

The one root cause I could follow to the end: `patch_grass_jungle` for pass `(74,-41)`
matches the reference draw-for-draw for 1271 typed draws and then emits a bound-4 draw
where the reference emits bound-8 (`FDRAW` window `s=4111`, `pass=74,-41`), i.e. the
`random_patch` spread draws come out in a different order from that point on. Downstream
is measurable: the port's `SURF_TRACE=75,-41` shows `SURF 1215 -653 ws=75 oc=74 depth=1
ok=false` - the port's own `short_grass` at `(1215,74,-653)` (written by
`patch_grass_jungle`) is one non-solid block above the floor, so `surface_water_depth_filter`
rejects a `trees_jungle` candidate the reference accepts, and the reference's bush at that
column (`jungle_log@74, oak_leaves@75..76`) is missing. That is the same shape as the
class Track 4I found (a cross-chunk write changing a later filter's answer), with
`patch_grass_jungle` as the writer instead of a vine. It is not fixed here.

## Diagnostics added

* `BLOCKPRED_TRACE=<cx>,<cz>` (`PM::BlockPred`) - every candidate of that chunk with
  `ok`, the block below/at, and the feature id. A rejected `would_survive` draws
  nothing, so it is invisible in a draw-stream comparison; this is the trace that
  localised the `y=87` rejection above.
* `SURF_TRACE=<cx>,<cz>` (`PM::SurfWaterDepth`) - `ws`, `oc`, the depth and the verdict
  per candidate. `surface_water_depth_filter` also draws nothing.

One seed, one dimension, one host.
