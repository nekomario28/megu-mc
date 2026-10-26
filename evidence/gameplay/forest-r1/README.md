# Forest R1 fixture — same-stage layer attribution

This is the first R1 same-stage comparison after the surface/spawn slices.
Reference: pinned 1.21.1, seed 0, chunks `(-64,32)` through `(-62,34)`, generated
by the actual server with every chunk force-loaded and saved. The port dumps the
same 3x3 region through `MC_SPINE_WG_PROFILE=0`; `tools/carver_diff.py` compares
block-state ids, maps property mismatches, and separates deep/carver/surface/
feature layers. Structures are disabled (`generate-structures=false`) except in
the explicit structures run; the reference is checked for all 9 chunks before a
diff is accepted.

Reproduce with a fresh admission:

```sh
tools/chunk_world_fixture.sh forest-terrain -64 32 3 terrain
tools/chunk_world_fixture.sh forest-carvers -64 32 3 carvers
tools/chunk_world_fixture.sh forest-features -64 32 3 features
tools/chunk_world_fixture.sh forest-structures -64 32 3 structures
```

## Results

| Stage | Region differences | Target chunk `(-64,32)` | Layers |
| --- | ---: | ---: | --- |
| terrain (no features/carvers/structures) | **207** | **0** | deep 207 |
| carvers (features off, real carvers) | **207** | **0** | deep 207 |
| features (real features/carvers, no structures) | **3,550** | **401** | deep 2,058, carver 1,492 |
| features after the tree fixes | **2,190** | **382** | deep 1,555, carver 635 |
| features after neighbour pre-carve | **1,789** | **114** | deep 1,346, carver 443 |
| structures (real features/carvers/structures) | **6,908** | **854** | deep 3,111, carver 3,797 |

The target chunk's **terrain, surface and carver outputs are exact**: both
terrain and carvers stages report zero differences there. The carver run adds
nothing over terrain in this region (still 207). Remaining target differences are
features (+550) and structures (+304); the 3x3 regional feature total is +4,381
over terrain and the structure run adds another +2,320.

The current full-feature rerun against the same saved reference is
**2,190 region / 382 target** (`features-after-tree.txt`). The target's top
pairs are `deepslate` vs `cave_air` (138), `deepslate` vs `cobblestone` (91),
`oak_leaves` distance-only (60), `deepslate` vs `mossy_cobblestone` (39),
`air` vs `lava` (20), plus ore cells. The cobblestone/mossy/lava/spawner/chest
cluster is a dungeon (`monster_room`) shape, so the next isolation is that
feature in this exact region, not another tree pass.

The target's dungeon residual was traced to neighbour carver visibility: the
MonsterRoom call at `(-1013,-13,526)` had `xr=3,zr=3` and `holes=0`, while the
reference's qualifying hole is at `(-1009,-13/-12,530)` in chunk `(-64,33)`
(`monster-room-neighbor.txt`). `populate` now claims/carves the 3x3 decoration
area before running the centre's features. That places the dungeon and reduces
the full-feature residual **2,190/382 -> 1,789/114**
(`neighbor-carve.txt`, `features-after-neighbor-carve.txt`).

The target's remaining 114 cells are 60 leaf-distance, 20 lava, about 30 ore,
1 chest, and 1 grass cell; they are the next feature layers.

## What the remaining cells are

- **surface-only residual outside the target**: 207 cells in the 3x3 region are
  `dirt`↔`stone` (77), `stone`↔`grass_block` (67), `grass_block`↔`dirt` (50),
  and `stone`↔`dirt` (13). The target column has none; the next step is to
  isolate those other columns rather than call the target a surface miss.
  The initial features run was 4,588/550 before the carver-visibility fix below.
- **feature residual in the target after the fix**: 401 cells. Top block-state pairs include
  `deepslate` vs `cave_air` (138), `deepslate` vs `cobblestone` (91), `granite`
  vs `air` (84), `deepslate` vs `mossy_cobblestone` (39), `smooth_basalt`/
  `calcite` vs `air`/`deepslate` (geode-shaped), and ore cells
  (`deepslate_lapis_ore`, `deepslate_diamond_ore`, `coal_ore`,
  `deepslate_redstone_ore`). `oak_leaves` distance-only cells also appear; the
  saved world has been ticked, so these are not counted as worldgen until the
  leaf-distance filter is checked per cell.
- **structures**: the explicit structure run adds **304** target cells and
  **2,320** region cells; the current server has no structure/jigsaw owner, so
  this is a separate unimplemented contract rather than a density/surface bug.
  The no-structure runs are the attribution baseline.

## Feature isolation started

- `tools/geode_fixture.sh -64 32 3 0`: the **reference region holds 0 geode
  cells**, so no positive geode comparison is possible here. The normal port
  dump does contain geode blocks in two neighbouring chunks (not the target),
  which is a separate placement/false-positive question.
- A one-feature reference with only `monster_room`/`monster_room_deep` in forest
  gives **207 region / 0 target**, identical to terrain-only. Monster rooms are
  therefore not the target residual; they are in the same surface-baseline set.
### Carver visibility fix

`FC::get` now reads feature-layer writes recorded by the carver pass before it
falls back to terrain. Previously a feature saw uncarved stone and could place
an ore into cells the reference carver had made air. This was the lapis cause:

| lapis isolation | ours | vanilla | shared | ours-only |
| --- | ---: | ---: | ---: | ---: |
| before | 28 | 23 | 23 | **5** |
| after | **23** | 23 | 23 | **0** |

The clusters also match exactly after the fix. The full no-structure feature
diff moved from 4,588/550 to **3,550/401** (region/target); terrain and carver
stages remain 207/0. The `feature_population` regression pin was updated only
after this native-order explanation; its old hash is not reused as evidence.

- Ore isolation (same seed/region, only one placed ore feature enabled):
  - `ore_lapis`: **ours 28 / vanilla 23**, shared 23, **ours-only 5**,
    vanilla-only 0; cluster sizes differ by one extra 6-cell cluster.
  - `ore_diamond`: **2 / 2**, shared 2, no extras.
  - `ore_redstone`: **3 / 3**, shared 3, no extras.
  - `ore_coal_upper`: 0 / 0; this is not a positive-coverage result.
  The lapis extra cells were the lead to the carver-visibility fix above; with
  that fix, lapis is **23/23** and all clusters match. Diamond and redstone
  placement also match in this region.
### Feature-order fix and tree isolation

The one-feature tree overlay initially placed no features because `load_freg`
built its `by_step` order only from graph edges: a feature that was the sole
entry in a step had no edge and was dropped before placement. The loader now
starts DFS from every `(step, firstIndex)` node, including dependency-free
nodes. Full-suite output is now 244/0/1, so the ordinary pack is not
reordered; the one-feature fixture is now runnable.

With only `trees_birch_and_oak` enabled in forest, the reference target chunk
has **332 oak leaves + 111 birch leaves**; the port has **188 + 71**. The target
diff is **728 cells** (`carver` 694, `deep` 34), with both missing and extra
tree cells. The instrumented reference logged **41** tree origins in the target
neighbourhood; the port logged **47**, with **26 shared**, **15 reference-only**
and **21 port-only** (`tree-origins.txt`). This is an origin/selection residual,
not the previous loader omission. This is now a **tree placement/RNG residual**, not a harness
failure.

The reference agent was armed on feature `0,9` and its first typed draws match the
port through the in-square positions and the two random-selector chances. The
first integer divergence was draw **7**: the reference asked `nextInt(16)=9`,
the port asked `nextInt(2)=1` (`tree-draws.txt`). That draw is the next count
position's `in_square`, not a tree-shape call: the reference rejected the first
tree after its trunk height, while the port entered foliage.

### Clip rejection: why the first tree stopped

`TreeFeature.place` accepts a clipped tree only when the min-size scan reaches
the requested height, or when the config supplies a `min_clipped_height` that
the clipped top still reaches. `tree.rs` only rejected an absent
`min_clipped_height` when it was `Some`; with `None` it continued after a clip.
The first target tree (`-1014,67,526`) is exactly that case. The reference
rejected it before any foliage draw, so the next count position followed
immediately; the port consumed the first tree's foliage and desynchronised the
rest of the stream.

The condition is now the vanilla disjunction (`top >= height || min_clipped is
present and top >= it`). A unit test pins all four branches. With the fix, the
one-feature run has **41 reference / 41 port origins, all 41 shared**, and the
feature draw sequence after the port's lazy aquifer samples matches the
reference (`tree-draws-after-clip.txt`, `tree-origins-after-clip.txt`).

The target chunk was then **5 cells** (all tree-base grass/dirt) from the 3x3
diff. Those were the trunk-base write: the pinned `TreeConfiguration` spells
the provider `dirt_provider`, while the parser read `below_trunk_provider`.
The parser now accepts `dirt_provider` first (and keeps the old spelling as a
conversion fallback). With the provider read, the target chunk is **0 cells**.

The full 3x3 diff after that is **728** cells (`carver` 360, `deep` 368; 223
leaf-distance tick artifacts excluded).

### Fancy trunk attachment height

`FancyTrunkPlacer` starts its attachment list at `start + (j - 5)`, not at
`start + k`: `k` is the upper end height used for branches. The port used
`start + k`, which shifted every fancy canopy up by
`k - (height - 3)` and moved its leaves out of the reference rows. With the
list seeded at `j - 5`, the 3x3 diff drops **728 -> 319** (`carver` 55,
`deep` 264; 223 tick artifacts excluded) and the target chunk stays **0**.
The remaining differences are neighbour terrain and canopy cells whose
origins lie in neighbouring chunks outside the instrumented tree-origin
radius. A second pass arming chunk `(-62,32)` found 40 reference / 42 port
origins (32 shared, 8/10 one-sided), matching the boundary canopy split; see
`outside-origin.txt`. A 5x5 same-stage fixture confirms the interior origins
are exact (25/25 shared; target 6/6) while the inner 3x3 still measures 319:
the boundary canopy comes from the next chunk row (`z=31`) whose origins and
terrain differ. See `five-by-five.txt`. A focused z=31 trace shows the first
tree candidate at `(-989,511)` has reference dirt and port stone at its base;
the `would_survive` filter therefore rejects the port tree and moves to the
next count position (`neighbor-support.txt`). This residual is downstream of
neighbour terrain/surface, not tree RNG, trunk placement, dirt placement, or
fancy attachments.

A no-feature control over the same 5x5 region initially gave **207 all-deep
dirt/stone/grass cells** and target 0 (`no-feature.txt`). The surface scan was
classifying `density <= 0` as air even when the aquifer supplied a barrier or
default solid; the reference scans the actual pre-surface block state. The
scan now classifies the aquifer result (None -> Solid, explicit air -> Air,
fluid -> Fluid, otherwise Solid), and applies the material only within four
blocks of the actual pre-surface top. That reduces the no-feature residual
**207 -> 0** and the tree-on residual **319 -> 72**, with target 0
(`material-window.txt`, `no-feature-after-material.txt`,
`diff-after-material.txt`).

The A/B trace then found the fancy centre limb bug: at `n = j/2` vanilla
returns `h2 * 0.5`, while the port returned `h2`, doubling that limb and moving
its candidate by a cell (`centre-layer.txt`). Fixing it reduces the tree-on
residual **72 -> 51** under `--tick`; those 51 are all leaf `distance` values
rewritten by the saved world's LeavesBlock tick. Without `--tick` the inner 3x3
is **0**, with the 51 reported as excluded tick artifacts
(`remaining-51.txt`, `diff-after-centre-no-tick.txt`). The target chunk stays 0.

- `tools/ore_fixture.sh` was repaired for the own-jar `configured_feature` path
  and for delayed `forceload`; without those fixes it read the wrong config path
  and compared against a missing region.

No old capture was re-recorded, and no fixture is used to excuse a missing chunk.
The reference and dump are under `/tmp/chunkfix-*` and `/tmp/forestfix-*` for
this run; the script refuses to diff if the reference is incomplete.

## Limits

- One seed and one 3x3 region; the target chunk is a forest column but the
  surrounding chunks are not all forest. This attributes this region, not all
  forest coordinates.
- `--tick` folds the known saved-world water/leaves changes into the diff; a
  per-cell leaf-distance comparison is still pending. Structure cells are
  intentionally excluded from the terrain/feature baseline and shown separately.
- The carver-visibility fix is included and its source/logs are in this
  directory (`lapis-before.txt`, `lapis-after.txt`,
  `features-after-carver-visibility.txt`, `feature-visibility-mutation.txt`).
  The tree one-feature run is now positive; its final 3x3 diff is 319 cells
  (`diff-after-fancy.txt`), mostly neighbour terrain plus a small canopy/leaf
  set outside the instrumented origin radius. Geode placement outside the
  target and the regional surface cells remain separate follow-ups.
