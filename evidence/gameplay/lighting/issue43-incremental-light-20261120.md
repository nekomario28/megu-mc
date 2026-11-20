# Issue #43 incremental lighting, E3-3 and E4-25 (2026-11-20)

## Implementation and ownership

E3-3 (`6598fe6`) introduced the clean-room incremental light engine in
`src/lighting.rs`. A live edit darkens and relights the affected 14-block
Manhattan-radius region using surviving emitters, direct sky, and boundary light.
Unloaded chunks bound propagation. Each update reports the affected sections with
low-nibble-first 2048-byte payloads or empty mask bits. E3-3 matched ten initial
Vanilla chunk snapshots with 30 sky arrays and six emitter block arrays.

E4-25 adds per-face occlusion for top, bottom, and double slabs and both stair
halves across all facings and corner shapes. Each shape covers quadrants of the
six faces. Light can cross if the exit and opposite entry face together leave an
opening. This rule applies to direct skylight, queued propagation, and light
entering a relight region from its boundary. Shape-only edits can invalidate sky
and block paths. The existing scalar constructor retains its attenuation behavior.
The geometry and shared-face rule were checked against the pinned server's
`SlabBlock`, `StairBlock`, `LightEngine.shapeOccludes`, and
`ChunkSkyLightSources.isEdgeOccluded` using Mojang's class mappings.
[Mojang 1.21.1 server mappings](https://piston-data.mojang.com/v1/objects/03f8985492bda0afc0898465341eb0acef35f570/server.txt)

The standalone suite has **16 tests**, including the original twelve. New cases
cover every slab face and complementary half-faces, all 40 combinations of stair
half, facing, and shape, shape-only skylight and block-light edits checked against
a rebuild, and exact section clearing after removing the last emitter across a
chunk boundary.

## Vanilla 1.21.1 comparison

The probe starts the pinned jar at `/home/yuu/tmpx/carverfresh/server.jar`, SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`, with OpenJDK 21 and a flat
Overworld from y=-64 with height 384. It forceloads a 3×3 chunk area and models
its four-layer opaque floor. The roof spans x,z=6..10 at y=-40, with a support
at (8,-41,8) and a central cell that takes the slab and stair states. A second
fixture puts a torch next to complementary slabs, differently oriented straight
stairs, and two stair corners. The probe asserts the two corner state IDs on the
wire so a neighbor-driven shape change fails at the fixture boundary.

All **15** snapshots of chunk (0,0) matched **45 transmitted skylight arrays**
and **12 transmitted block-light arrays** byte-for-byte. The block comparisons
also include implicit zero sections. The 12 transmitted block arrays include
three explicit zero arrays after removal. Implicit upper sky sections are outside
the wire comparison.

### Settled lantern removal

The client tracks chunk (0,0) from chunk (3,0) at view distance two. This puts the
target on the tracked border, where the pinned server sends `Update Light`.
The removal client's initial chunk matches the independently checked lit snapshot.
The probe removes the lantern at (8,-40,8), verifies the air block update, replays
the target's `Update Light` arrays, and compares every resulting block-light
section with a fresh chunk packet and the standalone engine's incremental result.

| Target chunk record | Block data mask | Empty block mask | Payload |
| --- | --- | --- | --- |
| Vanilla lit Chunk Data | `0xe` | `0x1` | Three lit arrays |
| Vanilla removal Update Light | `0xe` | `0x0` | Three zero-filled arrays |
| Vanilla settled Chunk Data | `0xe` | `0x1` | Three zero-filled arrays |
| Standalone incremental removal | `0x0` | `0xe` | Empty mask clears the three arrays |

The byte comparison between the lit and settled states gives an exact changed
section mask of `0xe` (indices 1–3). Canonicalizing each changed zero-filled
array to an empty bit gives the same `empty_block_mask=0xe` as the engine. Vanilla
retains its allocated arrays. In the pinned jar, `DataLayer.isEmpty` tests for
an unallocated zero layer, which accounts for the different raw data and empty
masks. The probe asserts the raw masks as well as the identical decoded light.

## Reproduction

```bash
mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"
rustfmt --check src/lighting.rs
TMPDIR="$PWD/target/tmp" rustc --edition 2021 --test -D warnings src/lighting.rs -o target/tmp/lighting-engine-tests && target/tmp/lighting-engine-tests
TMPDIR="$PWD/target/tmp" cargo test --offline --locked -- light --test-threads=1
TMPDIR="$PWD/target/tmp" JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData" python3 tests/lighting_parity_test.py
bash tools/layers_check.sh && bash tools/debt_ratchet.sh && python3 tools/provenance_check.py && git diff --check
```

Rustfmt and `git diff --check` passed. The standalone suite passed 16 tests with
zero failures. The Cargo light filter passed 25 tests with zero failures and
1019 filtered out. Observed fixture and gate output follows.

```text
air: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
roof: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
opening: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
water: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
ice: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
oak_leaves: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
oak_slab: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
oak_stairs: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
top_slab: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
double_slab: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
top_stairs: PASS native sky_mask=0xe block_mask=0x0 empty_block_mask=0xf matched_block_sections=0 matched_sky_sections=3
torch: PASS native sky_mask=0xe block_mask=0xe empty_block_mask=0x1 matched_block_sections=3 matched_sky_sections=3
lantern: PASS native sky_mask=0xe block_mask=0xe empty_block_mask=0x1 matched_block_sections=3 matched_sky_sections=3
removed_lantern: PASS native sky_mask=0xe block_mask=0xe empty_block_mask=0x1 matched_block_sections=3 matched_sky_sections=3
removed_lantern: PASS update block_mask=0xe empty_block_mask=0x0 changed_block_sections=0xe canonical_empty_block_mask=0xe
directional_wall: PASS native sky_mask=0xe block_mask=0xe empty_block_mask=0x1 matched_block_sections=3 matched_sky_sections=3
vanilla: PASS jar sha1=59353fb40c36d304f2035d51e7d6e6baa98dc05c fixtures=15
source graph: 44 modules, 111 edges, 0 named errors
debt ratchet: 0 over limit
provenance: clean - none of assets/pblocks.json, assets/properties.json, resourcepack.zip is tracked or referenced
```

## Pending server integration

The checked-in `src/main.rs` has no `mod lighting` declaration. The standalone
Rust suite and the Vanilla probe exercise the implementation directly. Wiring
engine ownership, generated emitters, registry properties, chunk lifecycle, and
client publication through the mc-spine server remains in the server integration
lane. Its runtime wire behavior was **NOT RUN** in E4-25.
