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

## Live server integration (E4-31)

Both dimension-aware block write paths in `src/main.rs` now queue changes to
emission, opacity, or face occlusion. This covers `/setblock`, player placement,
and player breaking. The block update flush reconstructs the pre-edit state from
the live overlay and generated terrain, then applies edits together in
`IncrementalLighting`. The solver includes the affected held columns and their
available neighbors, including generated emitters and the pinned slab and stair
properties. The packet encoder sends each affected held chunk's changed sky
and block sections as Protocol 767 `0x2a`, using empty masks to clear dark
sections. The chunk and dimension are checked while holding the socket's stream
order so a delta cannot follow a chunk unload.

A bounded cache retains the settled arrays for affected columns whose full
neighborhood was available. The `0x27` writer and server light queries use
those arrays while the generator and nearby terrain versions match. Initial
and incomplete windows continue to use the generated-light snapshot path.
The live flush avoids opening distant terrain solely for lighting.

The root include follows the existing server pattern and keeps the source
graph at 44 modules and 111 edges. The engine now loads the entire input window
in one pass and uses contiguous per-chunk light arrays to keep initialization
and section packing bounded. A local unoptimized 3×3 synthetic window with a
flat opaque floor took 18.89 s to load one chunk at a time with the previous
engine, and 28 ms to load as one batch with the updated engine. The subsequent
torch edit took 421 ms and 20 ms respectively and affected four chunks in both
runs. These single-run measurements cover the synthetic window only.

### Loopback receipt

`tests/client_mesh_wire.py` received six live `0x2a` packets for one
`/setblock` torch placement. The decoded block-light nibbles were 14 at the
torch and 13 on the other side of the chunk border. The client-port Greedy
Mesher read the same packed section through its C ABI and produced a torch quad
with block light 14, up from a pre-edit value of 0. The probe also checked
player placement, player breaking, and removal by command. Fresh `0x27`
snapshots carried levels (14, 13) while lit and (0, 0) after removal. Placing
and removing an opaque block changed live skylight below it from 15 to 14 and
back to 15.

The same torch delivery assertion failed against the pre-wiring binary after
it received the torch block state. The failure was
`live-torch-0x2a-reaches-both-chunks`. This case exercises the real loopback
server, the Protocol 767 decoder, and the client-port library. The pinned
Vanilla probe exercises the standalone solver on its 15 fixtures.

### Checks

Run from the E4-31 checkout based on `68fa967`, with temporary files under
`target/tmp` and OpenJDK 21 for the Vanilla comparison. The default server
binary had SHA-256
`ab271a9db022b31fd8bcf6884ea51ccea825882d162f600041f89ba48e975104`.
The final warning builds reproduced that same default binary.

| Command | Exit | Elapsed | Result |
| --- | ---: | ---: | --- |
| `cargo build --offline --locked` | 0 | 1.19 s | 0 warnings |
| `cargo build --offline --locked --features neoforge-events` | 0 | 1.26 s | 0 warnings |
| `cargo build --offline --locked --features fabric-probe,neoforge-events` | 0 | 1.25 s | 0 warnings |
| `python3 tests/client_mesh_wire.py` | 0 | 5.80 s | 6 live torch light packets, mesh block light 14 |
| `python3 tests/lighting_parity_test.py` | 0 | 71.55 s | 15 pinned Vanilla 1.21.1 fixtures |
| `cargo test --offline --locked -- light --test-threads=1` | 0 | 21.38 s | 42 passed |
| `python3 tests/m2_gameplay_wire.py` | 0 | 15.13 s | PASS |
| `python3 tests/gp1_gp6_wire.py` | 0 | 50.17 s | PASS |
| `python3 tests/gp3_gp4_gp5_wire.py` | 0 | 27.43 s | PASS |
| `python3 tests/gp5_gp7_wire.py` | 0 | 19.31 s | PASS |
| `python3 tests/dimension_wire.py target/debug/mc-spine` | 0 | 2.93 s | PASS |
| `bash tools/layers_check.sh` | 0 | 2.68 s | 44 modules, 111 edges, 0 named errors |
| `bash tools/debt_ratchet.sh` | 0 | 0.02 s | 0 over limit |
| `python3 tools/provenance_check.py` | 0 | 0.12 s | Clean |
| `git diff --check` | 0 | 0.01 s | Clean |

The standalone `rustc --edition 2021 --test -D warnings src/lighting.rs` suite
passed all 17 tests. The added batch case checks two neighboring chunks in a
dimension without a sky layer. `rustfmt --check --edition 2021 src/lighting.rs`
also passed. Full `tests/verify.sh` and live Vanilla comparison of the
mc-spine wire fixture were outside this run.
