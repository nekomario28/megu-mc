# CF-2 section meshing

Phase E Wave 3, lane E3-4a. The standalone `client-port/` Cargo package builds a Rust library and a C-compatible shared library with zero external crate dependencies. The implementation is original and available under LGPL-3.0-or-later, GPL-3.0-or-later, or AGPL-3.0-or-later at the recipient's choice. The repository carries the respective texts in [COPYING.LESSER](../../COPYING.LESSER), [LICENSE](../../LICENSE), and [COPYING.AGPL](../../COPYING.AGPL).

## Input contract

The caller expands each Java 1.21.1 (protocol 767) section to 4,096 registry state IDs in `y*256 + z*16 + x` order and supplies the appearance and occlusion metadata for each cell. This is the same cell order used by the server's [paletted storage](../../src/paletted.rs) and [terrain section export](../../src/worldgen.rs). Values pass through as `u32` registry IDs. The pinned [block registry](../../src/block_registry.rs) covers IDs 0 through 26,683. Air, void air, and cave air have no render-layer flags. A waterlogged cell supplies the water source flag and may also supply independent block and fluid tints.

Block and sky light use separate 2,048-byte arrays in the server [light packer's](../../src/main.rs) layout. Each byte stores the even cell in its low nibble and the odd cell in its high nibble. A border sample instead stores both kinds in one byte, block low and sky high. Packet decoding, registry metadata lookup, and selection of the correct light-array section belong to the caller.

The six 256-cell border slices are ordered Down, Up, North, South, West, East, matching [voxel face directions](../../src/voxel_faces.rs). Indices are `z*16+x` for Down/Up, `y*16+x` for North/South, and `z*16+y` for West/East. Each slice is the adjacent section's touching layer in world axis order. An optional 200-sample halo covers cells one step past two or three section axes. Its packing is defined in the [C header](../../client-port/include/mc_spine_client_mesh.h). The halo lets independently built sections sample the same AO neighbors and fluid corner heights where multiple section borders meet. Missing samples use the selected air (block 0, sky 15) or opaque (both zero) fallback.

## Geometry and lighting

The mesher sweeps a 16×16 mask for every slice and direction. An opaque neighbor with a full opposing face occludes the current face. A shared transparent interface is culled when both blocks opt into same-state culling and carry the same state ID. Unit-cube models can select opaque, cutout, or translucent output. Each greedy rectangle extends in U, then V, while the state ID, render layer, tint, face light, AO, and corner-light tuples match. Corner shading must also be invariant along each merged axis so interpolation across a long quad preserves the per-cell values.

The directional light takes sky from the cell in front of the face and the larger of its block light and the source cell's block light. Each corner uses two side cells and one diagonal in the outside face plane. AO is 0 when both sides occlude. Otherwise it is 3 minus the number of occluding samples. Block and sky corner values are rounded averages of the face and those three samples, using the face light in place of a blocked diagonal and the source block light as a floor. If corners 0 and 2 have the brighter AO sum, the mesh selects diagonal 1–3 to reduce directional AO artifacts. Vertex order is outward counterclockwise for both triangulations.

The water and lava level encoding follows the [server fluid state](../../src/entity_fluid.rs). Level 0 and falling levels 8–15 have amount 8. Levels 1–7 have amount `8 - level`. A fluid corner combines the four horizontal cells meeting there. Each contributes its amount out of 9, with weight 10 for amount 8, weight 1 for other fluid cells and zero-height open cells, and no weight for fully opaque cells. A contributing same-kind column with fluid immediately above forces that corner to height 1. A same-kind adjacent cell hides the internal face. An opaque ceiling hides the fluid top when the entire surface meets the ceiling. Exposed tops and sides share their corner positions and expose a downhill X/Z slope for texture flow. Falling cells also carry a falling flag. Waterlogged geometry uses the same unit-cell fluid calculation and respects the block's opaque face mask.

The mesh is deterministic for the same cell data and halo. The zero-drift contract covers section order, registry state ID preservation, light packing, and equivalent results from the Rust and C entry points. The cube model and fluid interpolation define this mesh format. Arbitrary block models, renderer-specific shading, texture lookup, and vanilla pixel parity are outside its scope.

## Incremental rebuilds

`MeshCache` owns loaded section snapshots, a deduplicated queue, and the last assembled meshes. Use one cache per world or dimension. An interior edit queues its own section. An edit on a face also queues the loaded face neighbor. Edge and corner edits reach the loaded sections that sample those halo cells, up to seven neighbors at a corner. Insertions, replacements, and removals compare the affected boundary data, including both light channels. The cache builds borders and halo from loaded sections when it drains the queue. `rebuild_dirty(limit)` bounds the number of processed sections, and a dirty mesh is unavailable until rebuilt.

## C ABI

Check `mc_spine_mesh_abi_version()` and initialize `mc_spine_mesh_input`. Call `mc_spine_mesh_section` with a null output and zero capacity to obtain the quad count, then call again with the same inputs and a caller-owned buffer. A short buffer returns the required count and leaves the output untouched. Each call performs a mesh pass and retains no pointers. The header specifies alignment, native byte order, statuses, and vertex/index conventions. A renderer can reconstruct repeating unit-cube UVs from the face direction and section-local positions. The C ABI exports stateless meshing for use by C, C++, or Java native interop. The incremental cache is a Rust API.

## Verification

From the repository root, with a POSIX shell and a Linux C toolchain:

```sh
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
cd client-port
cargo test --offline --locked
cargo run --release --offline --locked --bin mesh_bench -- 500
cargo build --offline --locked --lib
cc -std=c11 -Wall -Wextra -Werror -pedantic -Iinclude tests/abi_smoke.c \
    -Ltarget/debug -lmc_spine_client_mesh -o "$TMPDIR/mesh_abi_smoke"
LD_LIBRARY_PATH="$PWD/target/debug" "$TMPDIR/mesh_abi_smoke"
```

The solid benchmark fixture begins with 24,576 block faces, exposes 1,536 unit faces after culling, and produces six quads after merging. The checkerboard fixture exposes 12,288 faces and produces 12,288 quads. The timing covers the CPU mesh pass, including output allocation, and excludes conversion, I/O, and rendering.
