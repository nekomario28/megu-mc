# Surface context — steep heightmap correction

This is a bounded correction to the surface-rule context, not a full
`SurfaceSystem.buildSurface` port.

## Native fact

`SurfaceRules.Context$SteepMaterialCondition` (obfuscated `dzp$g$c`) reads the
chunk's `Heightmap.Types.WORLD_SURFACE_WG` at the four horizontal neighbours and
tests:

```text
z+1 height >= z-1 height + 4  OR  x-1 height >= x+1 height + 4
```

The server's `cc.surf` is the topmost **density-positive stone** column, so under
an ocean it is below the water surface. The surface scan must use the
topmost solid-or-fluid height (`cc.wtop`), which is this server's
`WORLD_SURFACE_WG` basis at the surface stage. `SurfaceSystem.buildSurface`
(the decompiled `dzq.a(...)`) also scans from the ocean-floor heightmap and
tracks water height/stone depth; those remaining scan details are not changed
here.

## Change and gate

- `Gen::eval_rule` now receives the surface/fluid heightmap (`cc.wtop`) at the
  three production call sites: chunk column material scan, carver
  `top_material_at`, and deep-state fallback.
- The steep parameter is named `heightmap`, and the condition is explicitly
  tested at four blocks (true) and three blocks (false), including the x and z
  directions.
- Canonical mutation: threshold `4` -> `3`, detected by
  `surface_steep_reads_the_world_surface_heightmap`.

No surface oracle was able to construct a full vanilla `SurfaceSystem` and apply
a rule to a generated chunk in this slice. The reference for this change is the
decompiled actual class, not a running world; no claimed cell-level surface
parity.

## Verification

| Gate | Result |
| --- | --- |
| Focused surface test | `surface_steep_reads_the_world_surface_heightmap` passes |
| Full serial release | **237 passed / 0 failed / 1 ignored**, exit 0 |
| Progression | 236/0/1 -> 237/0/1; no failed names |
| Selected mutation | 1 proved (four-block threshold); 0 not-a-gate/parked/skipped |
| fmt / all-target warnings-denied clippy | Passed |
| Layers / debt / wire | 20 modules, 47 edges, 0 named errors; 0 over limit; wire PASS |

## Limits and not run

- The scan's water-height, stone-depth, ocean-floor start and deep-state
  interactions are not independently re-derived from a running actual chunk.
  Surface conditions other than steep are covered by the existing decode/
  primitive gates, not by this execution change.
- Whole-world surface comparison, the full `tests/verify.sh`, whole mutation
  campaign, hosted CI, push, main merge and release were not run.
- The change is native-bytecode-derived; it is not a claim that surface
  generation now matches 1.21.1 cell-for-cell.
