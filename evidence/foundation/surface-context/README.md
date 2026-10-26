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
- The per-column scan was extracted to `scan_surface_column`; production `cols`
  and the synthetic test use the same function. It tracks air/fluid/solid,
  stone depth above/below and water height as `SurfaceSystem.buildSurface` does.
- Canonical mutations: steep threshold `4` -> `3` and `sda += 1` -> `+= 0`,
  both detected.

No surface oracle was able to construct a full vanilla `SurfaceSystem` and apply
a rule to a generated chunk in this slice. The steep reference is the decompiled
actual class, not a running world; no claimed cell-level surface parity.

A separate condition oracle now evaluates actual `SurfaceRules` conditions with
a synthetic `dzp$g` context: **4,037 rows** covering stone depth (floor/ceiling,
offset, surface-depth and secondary range), water block/start, hole and
above-preliminary. It does not cover y anchors, steep, temperature, biome or
noise conditions (those need a chunk/biome/noise source). The Rust test compares
all 4,037 rows; a mutation that swaps ceiling/below fields is detected.

A `buildSurface` scan oracle now fills a real `ProtoChunk` with synthetic
stone/water columns, runs the actual `SurfaceSystem.buildSurface` with a decoded
actual stone-depth rule, and records the gold cells. **768 columns** (3 offsets),
**1,536 gold cells**; the production `scan_surface_column` reproduces every
column. It uses a fixed biome and no running server; whole-world application is
still separate.

## Verification

| Gate | Result |
| --- | --- |
| Focused surface tests | steep heightmap and 4,037 synthetic conditions pass |
| Native condition oracle, twice | 4,037 rows, exact |
| Native buildSurface scan, twice | 768 columns / 1,536 gold cells, exact |
| Full serial release | **239 passed / 0 failed / 1 ignored**, exit 0 |
| Progression | 238/0/1 -> 239/0/1; no failed names |
| Selected mutations | 3 proved (four-block threshold, ceiling/below field, stone-depth accumulation); 0 not-a-gate/parked/skipped |
| fmt / all-target warnings-denied clippy | Passed |
| Layers / debt / wire | 20 modules, 47 edges, 0 named errors; 0 over limit; wire PASS |

## Limits and not run

- The scan is pinned on synthetic stone/water columns; real noise terrain,
  cave fluids, deep-state interactions, biome extensions and whole-world
  application remain outside this fixture.
- y anchors, steep/temperature/biome/noise condition sources are outside the
  synthetic condition oracle; the scan fixture uses only stone_depth.
- Whole-world surface comparison, the full `tests/verify.sh`, whole mutation
  campaign, hosted CI, push, main merge and release were not run.
- The change is native-bytecode-derived; it is not a claim that surface
  generation now matches 1.21.1 cell-for-cell.
