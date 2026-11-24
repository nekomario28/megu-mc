# Template neighbor finalization and native fossil comparison — E4-3 (2026-11-20)

## Scope and reference

- Lanes: bounded P1-1 neighbor-edge finalization and P1-4 native-positive fossil comparison.
- Specifications: `specs/template-finalization/README.md` and `specs/template-finalization/slices/01-neighbors.md`.
- Vanilla reference: Minecraft Java 1.21.1, Java 21, `<home>/tmpx/carverfresh/server.jar`.
- Jar SHA-1: `59353fb40c36d304f2035d51e7d6e6baa98dc05c`.
- Jar SHA-256: `e3bc55693e93cda0188f2e60aea28113fc647c5e85a15fa3d1b347349231b4bb`.

## P1-1 native edge observations and implementation

- Existing `BReg` predicates and transformations handle connection and fragile-support decisions in `src/structure_template.rs`. No duplicate state tables were added.
- Air and liquid placement can detach neighboring fences, panes, walls, and waterlogged iron bars; unsupported torches and hanging lanterns break based on center-face sturdiness.
- Horizontal faces of top/bottom slabs are not full-center support. Double slabs are full-face support and can retain or form fence, pane and wall joins. Top/bottom slabs have direction-dependent torch/lantern vertical support.
- The wall path uses `BReg::wall_after_full_neighbor` when joining a non-air neighbor to cover double-slab connections. Stairs retain their existing neighbor-state transformation.
- Waterlogged generated-fluid reservations continue through the existing proto queue. A tuple containing `0` identifies a fluid state and is not a scheduled zero-delay game tick. No synthetic zero-delay live ticks were added.
- In-source template placement regressions cover air/water detachments, support-dependent fragile blocks, waterlogged iron bars, single slabs and positive double-slab joins.

### Native oracle checks

- `TMPDIR="$PWD/target/tmp" python3 tests/template_neighbor_oracle_test.py`: 7/7 passed.
- `TMPDIR="$PWD/target/tmp" python3 tools/template_neighbor_oracle.py --jar <home>/tmpx/carverfresh/server.jar`: 25/25 native assertions passed, using 184 block tags and 2 fluid tags.
- Additional local native neighbor probe: 46/46 assertions passed (single/double slabs, wall/fence/pane joints, vertical fragile support, water edges). Source and output live outside Git at `<local>/captures/matrix-p1/native-extra/`.
- Supplemental `native-extra.tsv` SHA-256: `4bda0b26ab2a348c58ead1580d3763927fc035d94a5edc6d2c84506a4cdd95ea`.

These checks cover bounded `updateShape` cases and the specified production-path regressions. Complete ordered template-neighbor recursion, all supported block families, and real runtime fluid-tick delivery remain outside the proved P1-1 subset.

## P1-4 positive native chunk

- Capture tool: `tools/structure_capture.py`, reference stage `structures`, seed `4242`, Nether biome `minecraft:soul_sand_valley`.
- Structure: `minecraft:nether_fossil` in chunk `(-22, 20)`, Minecraft 1.21.1 DataVersion `3955`, chunk status `minecraft:full`.
- Verified native `structures.starts["minecraft:nether_fossil"].Children[0]` is `minecraft:nefos` with bounding box `[-356, 55, 319, -350, 61, 324]` intersecting the target chunk.
- Tag `nat-4242-nether-fossil-e43`, 9 decorated chunks, LastUpdate `2`, no runtime writes.
- Native chunk state census: 24,688 non-air, 40,200 air, 648 cave_air, 0 void_air. Native pending ticks: 6 fluid, 0 block. Native block entities: 0.
- Capture root: `<local>/captures/matrix-p1/nat-4242-nether-fossil-e43/world`.
- Native `level.dat` SHA-256: `5b00d25036e669d888a81f5616dfd0c3a8f350f2aa766ed5b25be8f6e89ca324`.
- Native `DIM-1/region/r.-1.0.mca` SHA-256: `db16ec8649dfbd47306eb855e2cafb22f0ddb4d875ba57c5e09f634686efe7df`.
- Native capture `order.tsv` SHA-256: `0f3b50d763e088e9df27fb23ba4f1326689f2992d8dd0e231e7e1a7fe36d127c`.
- Native capture `run.log` SHA-256: `34fd2bdcace8c33b7552ec91bdbe6e467558a6f2712ac9561fe2b99f98f47ad9`.
- Native capture `boundary.json` SHA-256: `f295fe121c986622bf6e1b6d811e1b7a7b381fb26d2ab192079438ea649659d0`.

### Full typed comparison

- `tools/port_triage.py --mode full --preset nether --height-window dimension --fail-on-diff` exited 0.
- Compared 65,536/65,536 typed states, covering all y=0..255 and all 16x16 chunk positions, with **0 differences and 0 out-of-window cells**. Air, cave_air and every serialized typed attribute were included.
- Both native/port typed binary dumps were 262,144 bytes; matching SHA-256: `b6628d89652caa1ba6cb1ffc66f2ea1781afea501f87e7e0f3edb9b64a1f0f90`.
- Port executable used after production changes SHA-256: `2f2723fb92bb4536d79f5f64ebc8e2b44342962dd0230a4ddc1f2d387f640db5`.
- Triage outputs: `<local>/captures/matrix-p1/dumps/nat-4242-nether-fossil-e43/`.

Reproduction from the repository root (after building the binary):

```bash
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk
export PATH="$JAVA_HOME/bin:$PATH"
export JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData"
python3 tools/port_triage.py <local>/captures/matrix-p1/nat-4242-nether-fossil-e43/world -22 20 --n 1 --mode full --preset nether --height-window dimension --binary "$PWD/target/debug/mc-spine" --out-dir <local>/captures/matrix-p1/dumps/nat-4242-nether-fossil-e43 --json-out <local>/captures/matrix-p1/dumps/nat-4242-nether-fossil-e43/triage.json --fail-on-diff
```

### Cell-count constraint and evidence limits

The requested 98,304 cells are an Overworld column at y=-64..319 (384 vertical cells per horizontal location). A Nether column contains 65,536 cells because the valid height is 256. The positive Nether fossil has verified `structures.starts.Children[]`, whereas the Overworld `minecraft:fossil` is a configured feature and `structure_capture.py --dry-run` returned exit 2, `no structure set lists minecraft:fossil`. Inventing/padding 32,768 out-of-dimension cells would invalidate a native comparison. All 65,536 actual Nether cells were compared with no attribute exclusions.

Typed cell equality establishes the captured post-generation block-state comparison for this chunk. It does not independently establish fluid/block pending-tick equivalence, runtime tick delivery, block-entity initialization, or complete P1-1 ordered neighbor-cascade parity. The global P1-1 milestone remains open beyond the bounded edge cases.

