# Ruined portal placement: the WG heightmap is frozen at the carver, not the surface

Source baseline: `a1530b8` on `t3/daf7d459`. Target world: the authenticated
1.21.1 server (`/tmp/nat-4242-ruined-portal/world`, seed 4242, SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c` for `server.jar`), chunk `(6, 7)`.

## Symptom

`minecraft:ruined_portal`'s start NBT at `(6, 7)` was already identical to native
(`BB [97,100,112,106,107,123]`, `on_land_surface`, `TPX/TPY/TPZ = 96/100/112`,
`COUNTERCLOCKWISE_90`, `portal_10`, `mossiness 0.2`, `cold 1`), and the template
pass wrote identical cells. Everything that differed was the netherrack base and
the drip columns that follow it — 124 cells of
`netherrack`/`dirt`/`stone` in one chunk, both directions (`netherrack -> stone`
on one flank, `stone -> netherrack` on another). A symmetric swap around an
otherwise matching centre is the signature of a *draw stream* misalignment, not
of a wrong formula: cells inside Manhattan distance 6 have weight 1.0 and place
whatever the draw is, so only the fringe moves.

## The wrong input

`RuinedPortalPiece.placeNetherrackBase` places at
`getSurfaceY(x, z) = level.getHeight(placement.heightmap(), x, z) - 1`, and for
`on_land_surface` that heightmap is `WORLD_SURFACE_WG`. Tracing the port's
`portal_base` (temporary `RP_TRACE`) showed the port reading the *uncarved*
terrain top: `(107,122)` gave `h = 110`, so `|110 - minY(100)| = 10 > 3` and the
cell was skipped, while native placed a base at `y = 100` with a two-block drip
column below it. Native's own chunk agrees with `100`: the netherrack top is at
`(107,100,122)` and the freeze-top-layer snow sits at `101`, i.e. the column's
top block before the structure was `100`.

`ProtoChunk.setBlockState` does not update *all* heightmaps; it updates
`getStatus().heightmapsAfter()`. Decoded from the pinned jar:

```
dvz: o = {OCEAN_FLOOR_WG, WORLD_SURFACE_WG}          b = {OCEAN_FLOOR, WORLD_SURFACE, MOTION_BLOCKING, MOTION_BLOCKING_NO_LEAVES}
     empty..surface  -> o        carvers..full -> b
```

`ChunkStep` applies the target status *after* the stage body returns, so the
carver pass writes while the chunk still reports `SURFACE` (set `o`) — a carve
lowers both WG maps — and the feature pass writes while it reports `FEATURES`
(set `b`), which never touches them. The port had the WG maps frozen one stage
too early (at the surface), so every carved column kept its pre-cave height.

Confirmed at both boundaries before the fix: the port's own `--mode terrain`
(no carvers) chunk has grass at `(107,110,122)` and `--mode carvers` has air
there, i.e. the terrain really is that tall and the carver really does remove it;
the live chunk after features shows the same air at `101..110`.

## Fix

`Gen::wg_after_carve` stores each chunk's two WG maps (first-available form) as
`apply_carvers` leaves them; `FC::heightmap` serves them for `*_WG` kinds and
falls back to the terrain scan only for chunks this generator never carved.
Absent the snapshot the answer is the pre-carve terrain — the previous behaviour,
so a no-carver fixture is unchanged.

## Measurement

`tools/port_triage.py /tmp/nat-4242-ruined-portal/world 6 7 --seed 4242
--allow-stage-mismatch --pop-order …/sculk-trace.txt --pop-order-scope recorded`

| | chunk `(6,7)` | 4x4 window at `(5,6)` | ruined-portal cells |
|---|---|---|---|
| before | 278 | 706 | 124 |
| after | 154 | 566 | **0** |

The 154 that remain in `(6,7)` are the pre-existing amethyst-geode defect
(`outside_structures | underground`, `y = -30..-17`), untouched by this change.
Cell-level A/B over the same 4x4 window (`before` vs `after` vs native, all 16
chunks): **140 cells fixed, 0 regressions**, 566 cells differ in both (geode,
two mineshaft fence-connection cells). The 140 are the portal (124) plus 16
cells in `(7,8)`, `(6,9)`, `(7,9)` where a snow-layer/`short_grass` placement now
matches (features read the WG map too, e.g. `world_surface_wg` placement
modifiers).

Guards: `wg_heightmap_keeps_the_carved_top_and_ignores_the_feature_layer` plus a
`tools/mutation_check.sh` entry (`MUTATION_FILTER=wg_heightmap_keeps_the_carved`,
proved red when the snapshot lookup is deleted). The pre-refactor feature capture
digest moved (`de36bbef… -> b680a4c9…`) because population reads the same map.

## Not claimed

- The remaining 154/566 cells are not addressed here; the geode defect is a
  separate lane.
- The native oracle harness models `getHeight` as a live scan of its scripted
  world, which is neither the pre-carve terrain nor the post-carve freeze; its
  71 vectors still pass because the scripted scenes have no carvers. Nothing in
  `tests/data/template_structures_1211.tsv` moved.
- One seed, one chunk set, one host.
