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
| structures (real features/carvers/structures) | **6,908** | **854** | deep 3,111, carver 3,797 |

The target chunk's **terrain, surface and carver outputs are exact**: both
terrain and carvers stages report zero differences there. The carver run adds
nothing over terrain in this region (still 207). Remaining target differences are
features (+550) and structures (+304); the 3x3 regional feature total is +4,381
over terrain and the structure run adds another +2,320.

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
- Tree isolation is still unresolved: the first one-feature overlay produced
  no port features, so that run is a harness failure, not a negative result.
  The tree step needs a corrected overlay before attribution.
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
  Geode placement outside the target, the tree harness, and the 207 regional
  surface cells remain separate follow-ups.
