# CF-5 — Sodium + Distant-Horizons + culling: the three-layer architecture

This document fixes the ownership boundary for chunk visibility work in the
client fork. Three layers exist and must not be merged:

1. **Server (`src/` binary, protocol 767)** decides *what a client is allowed
   to hold*: which chunk columns are tracked, how fast their `0x27` payloads
   may be produced, and when light is re-sent. It is authoritative and knows
   nothing about rendering.
2. **Shared kernel (`client-kernel/` + `client-port/` data types)** owns the
   *O(1) facts every layer reads*: per-state render/occlusion/light flags and
   the packed light layout. No layer may re-derive block facts with a second
   table.
3. **Client (`client-port/` meshing kernel + `client-app/` integration)**
   decides *what is worth drawing this frame*: section visibility proofs,
   frustum and graph culling, bucketed draw submission, incremental translucent
   ordering, and coarse LOD for distant columns.

A change that moves a decision across a boundary (e.g. the server guessing
what the client will draw, or the mesher re-reading registry JSON) is a
boundary violation, not an optimization.

## Layer 1 — Server: packet production and tracking culling

The server side already implements the "Sodium server" half: a bounded,
ack-paced, look-prioritized chunk sender in front of a shared, validated
chunk-packet cache.

| Responsibility | Current implementation |
| --- | --- |
| Chunk packet cache (`ChunkPacketCache`) | `MapChunkCache` / `map_chunk` / `build_map_chunk` / `map_chunk_sig` / `chunk_versions_3x3` in [src/main.rs](../../src/main.rs) (cache struct ~line 9620, `map_chunk` ~line 9740). One assembled `0x27` payload per `(DimId, cx, cz)`, validated against the 3×3 `chunk_version_dim` tuple plus an order-independent FNV-1a signature of signs, spawners and neighbor `GenSources`; FIFO cap `MAP_CHUNK_CACHE_CAP = 512`. A hit also restores the chunk's own generated emitters into the caller's `GenSources`. |
| Visibility budget | `ChunkSendBudget` (`queued`, `max_chunks_per_tick`) and `prioritize_chunks` (center first, then look-forward, side, behind; distance + coordinate tie-break) in [src/entity_spatial.rs](../../src/entity_spatial.rs); driven per tick by `ChunkDelivery::next_batch` in [src/connection_limits.rs](../../src/connection_limits.rs). |
| Tracking culling | `ChunkWindow` (Chebyshev `(2·view+1)²` window, `contains`/`leaving`/`unsent`) in [src/play_protocol.rs](../../src/play_protocol.rs); `chunk_view_diff` in `entity_spatial.rs`; `ChunkFeedback` ack gate (`0x08` Chunk Batch Received → `sent_batch` → next `0x0c`/`0x0d` batch) in [src/chunk_ack.rs](../../src/chunk_ack.rs). `SERVER_VIEW_DISTANCE = 8` bounds the window; `stream_chunks` emits `0x21` unloads only for chunks the client actually holds. |
| Wire shaping | `cull_chunk_light` rewrites `map_chunk`'s full light lists into sparse mask+array form in [src/play_protocol.rs](../../src/play_protocol.rs); `splice_chunk_block_entities` merges live block entities; entity tracking uses per-type `client_tracking_range`/`update_interval` from the 1.21.1 registry (`entity_spatial.rs`, `compute_tracking_diff`). |

Server guarantees the client layer depends on:

- A chunk is sent at most once per window membership; leaving the window
  always produces a `0x21` unload, so "sent set == held set".
- Batch production is bounded by `ChunkFeedback::batch_size()` (the client's
  requested chunks/tick, clamped to the window) and never exceeds one batch
  per tick (`last_batch_tick`).
- `map_chunk` output is byte-stable for identical `(dim, cx, cz, versions,
  sig)` inputs — the cache key *is* the invalidation contract. Any new input
  to `build_map_chunk` must be folded into `map_chunk_sig` or the 3×3
  versions, or the cache silently serves stale terrain.

## Layer 2 — Shared kernel: O(1) render facts and lighting

Both sides read the same physical facts; neither side keeps a private copy.

| Fact | Kernel surface | Client-port mirror |
| --- | --- | --- |
| Per-state physical facts | `BlockFacts` (`is_solid_render`, `can_occlude`, `sturdy_faces_mask`, `face_occlusion[6]`, `opacity`, `luminance`, `fluid_kind/level`, `is_waterlogged`) in [client-kernel/src/registry.rs](../../client-kernel/src/registry.rs), served by `Registry::facts(state)`; `face_occlusion_from_octants` in [client-kernel/src/light.rs](../../client-kernel/src/light.rs). | `Block { state_id, tint, fluid_tint, flags: u8, occlusion: u8, fluid_kind, fluid_level }` in [client-port/src/section.rs](../../client-port/src/section.rs): `OPAQUE/CUTOUT/TRANSLUCENT/WATERLOGGED/TINTED/FLUID_TINTED/SAME_STATE_CULL` bit flags plus a 6-bit `occlusion` mask in `Direction::ALL` order, and `Light(u8)` packing block-low/sky-high nibbles matching the server's 2,048-byte section arrays. |
| Section-level O(1) proofs | `PalettedSection::{is_uniform, uniform_state, non_air, light_class}` in [client-kernel/src/section.rs](../../client-kernel/src/section.rs) — uniform/empty sections skip decode and traversal. | `SectionVis` (`boundary_faces` 6-bit mask, `interior_faces`, `uniform_air`) and the `emits_nothing`/`emits_any` proofs in [client-port/src/cull.rs](../../client-port/src/cull.rs); `MeshColumnSection.flags` `ABI_COLUMN_UNIFORM_AIR`/`ABI_COLUMN_SEALED_SOLID` hints in [client-port/src/ffi.rs](../../client-port/src/ffi.rs). |
| Lighting | `LightEngine` (`load_chunk`, `apply_edits` → `LightUpdate` with per-section nibble arrays) in [client-kernel/src/light.rs](../../client-kernel/src/light.rs); `LightFact`/`LightEdit` carry opacity, emission and the six face-occlusion quadrant masks. | `client-app` feeds `LightUpdate` sections back into `MeshCache` via `apply_local_light_update` in [client-app/src/world.rs](../../client-app/src/world.rs), which re-meshes only sections whose light actually changed. |

The rule: a consumer that needs "is this face/section emittable" asks the
packed flags (`Block::occludes`, `Block::renderable`, `SectionVis`) — it does
not rescan cells or consult `assets/*.json` at run time.

## Layer 3 — Client: `client-port` meshing kernel + `client-app` integration

Stages S1–S6 below are the target pipeline. **Status is marked per stage**;
"kernel" means implemented and tested inside `client-port`, "app" means wired
into `client-app`, "design" means specified here but not yet implemented.
Nothing in this section changes the wire contract with the server.

### S1 — 6×6 Visibility Graph (design; substrate exists)

Nodes are the 16³ sections inside the client's loaded column window; an edge
exists between sections sharing a face, and each node carries its
`SectionVis` mask plus `uniform_air`/`sealed` proofs. The graph answers "which
sections can possibly emit geometry toward a given side" without touching
block data.

Current substrate this builds on:

- `MeshCache` already computes the exact dirty neighborhood of an edit — the
  26-section halo plus edge/corner samples — in
  [client-port/src/cache.rs](../../client-port/src/cache.rs) (`SectionCoord::
  neighbors`, `insert`/`remove`/`set_cell` boundary comparison, `build`
  assembling `borders`/`edges_and_corners`).
- `SectionVis::visible_towards(facing)` in
  [client-port/src/cull.rs](../../client-port/src/cull.rs) is the per-node
  predicate the graph traversal consults; `box_facing_mask` produces the
  camera-side `facing` mask for a section box.

The "6×6" name denotes the graph's shape contract: nodes are 16³ sections
and every node carries a six-bit `boundary_faces` mask over the six axis
directions (`Direction::ALL` = Down/Up/North/South/West/East), so each node
has exactly six neighbor slots — the stage evaluates six-direction
adjacency on six-bit masks. How many section rings the graph spans (the
loaded column window vs. a fixed 6×6-section band per the
Distant-Horizons design) is fixed when the graph is implemented, not
before.

### S2 — Frustum × BFS `GraphCuller` (design; primitives exist)

A breadth-first walk over the S1 graph from the camera's section, pruning a
node when (a) its box fails `box_in_frustum` against the six view planes or
(b) `SectionVis::visible_towards` proves no emitted face can face the camera.
BFS (not per-section independent tests) is what makes occlusion *between*
sections cheap: a sealed neighbor ring stops the walk early.

Existing primitives:

- `box_in_frustum(min, planes)` — conservative six-plane test, false only on
  proven outside — in [client-port/src/cull.rs](../../client-port/src/cull.rs).
- `box_facing_mask(min, camera)` — which of the six section sides face the
  camera — same file.
- `client-app`'s software renderer currently iterates every packed section
  and sorts translucent sections by squared camera distance in
  [client-app/src/render.rs](../../client-app/src/render.rs); the culler
  replaces that full scan with the graph walk and feeds only survivors to
  the rasterizer.

### S3 — 7-direction bucketed backface culling (kernel)

Inside `mesh_section`/`mesh_column`, face emission is decided per direction
slice — six `Direction` sweeps for unit cubes (`mesh_cubes_fast`, slices
0–5 = Down/Up/North/South/West/East in [client-port/src/mesh.rs](../../client-port/src/mesh.rs))
plus the fluid surface pass (`mesh_fluids_fast`) — with neighbor occlusion
carried as bitmasks (`encode_occ`, `scratch.occ`, `PackedFace` shading
keys) rather than per-face branches. Same-state culling
(`SAME_STATE_CULL` + equal `state_id`) and the waterlogged/fluid rules are
part of the same predicate, mirrored conservatively by `face_can_emit` in
[cull.rs](../../client-port/src/cull.rs) so `SectionVis` proofs stay a superset
of real emission.

Every emitted `Quad` is tagged with a `Layer` bucket — `Opaque`, `Cutout`,
`Translucent`, `Fluid` ([mesh.rs](../../client-port/src/mesh.rs)) — which is
the draw-order contract S4's MultiDraw consumes: opaque/cutout buckets are
order-free, translucent and fluid are sorted (S5). The "7-direction" stage
name covers the six cube-face directions plus the fluid direction bucket;
the invariant is that culling happens before greedy merging, never after.

### S4 — `RenderRegion` 8×4×8 MultiDraw (design; vertex contract exists)

Group sections into render regions of 8×4×8 sections (128×64×128 blocks —
the Distant-Horizons LOD-cell footprint scaled to section quads) and submit
each region as one indirect/MultiDraw call per layer bucket instead of one
draw per section. This is a client-app/GPU concern only; `client-port`'s
contract is unchanged.

Existing substrate:

- `PackedVertex` — 16-byte vertex, 4 per quad, integer attributes — produced
  by `mc_spine_client_mesh_pack_vertices` in
  [client-port/src/ffi.rs](../../client-port/src/ffi.rs) /
  [client-port/src/packed.rs](../../client-port/src/packed.rs); positions,
  state id, tint, AO, block/sky light, diagonal and UV extents are all
  section-local, so a region draw needs only a per-region origin offset.
- `Quad::indices()` gives the two triangles per quad; `Layer` tags the
  bucket; `sort_translucent` indices (S5) apply within the translucent
  bucket only.
- `client-app`'s rasterizer in [client-app/src/render.rs](../../client-app/src/render.rs)
  already consumes `(coord, &[PackedVertex])` pairs — the region boundary is
  where per-section iteration becomes per-region batching.

### S5 — Incremental translucent sort (app)

Translucent correctness requires back-to-front order, but re-sorting the
whole world per frame is the classic churn point. The implementation sorts
*per section, only when that section re-meshes*:

- `mc_spine_client_mesh_sort_translucent` returns quad indices sorted by
  decreasing squared camera distance from the quad centroid (ascending input
  index on ties) — [client-port/src/packed.rs](../../client-port/src/packed.rs),
  exported through `client-port/src/ffi.rs` and wrapped in
  [client-app/src/world.rs](../../client-app/src/world.rs).
- `WorldState::rebuild_dirty_bounded(limit, camera)` re-meshes at most
  `limit` dirty sections per pass, re-packs vertices and refreshes that
  section's `translucent_orders` entry — dirty tracking lives in
  `MeshCache::rebuild_dirty` ([client-port/src/cache.rs](../../client-port/src/cache.rs)).
- `render.rs` draws opaque/cutout first, then translucent sections far-to-near
  with `sort_translucent` order inside each section.

### S6 — Horizon LOD (kernel; app integration pending)

Distant-Horizons-style coarse column summaries for terrain beyond the active
mesh distance, generated from the same `SectionView` data the mesher uses —
no second decode path:

- `summarize_column(views, edge, out)` produces `edge²` `LodCell`s
  (top non-air `state_id`, `tint`, `top_y`, packed `light`) at `LOD_COARSE =
  4` (4×4-block footprints) or `LOD_FINE = 16` (per-block columns), scanning
  sections top-down and stopping once every column found its top —
  [client-port/src/lod.rs](../../client-port/src/lod.rs).
- `LodSummary` owns the cell buffer so regeneration is allocation-free.
- `lod_bench` measures summary cost and the sealed-section culling savings on
  the same 24-section Overworld fixture the mesh bench uses —
  [client-port/src/bin/lod_bench.rs](../../client-port/src/bin/lod_bench.rs).

`client-app` does not consume `LodSummary` yet; wiring coarse cells into the
renderer (replacing per-section meshes beyond the mesh radius) is the
remaining app-side step and must reuse `LodCell.state_id/tint/top_y/light`
verbatim — no re-interpretation of block facts.

## Data flow

```
world state ──► server: MapChunkCache ──► 0x27/0x21/0x2a/0x0c paced by
                ChunkSendBudget + ChunkFeedback        ChunkWindow+look dir
                        │
                        ▼
        client-app: DecodedChunk ──► PalettedSection/Block+Light (kernel facts)
                        │
                        ▼
        client-port: MeshCache (S1 dirty graph) ──► mesh_section (S3 cull)
                        │                            │
                        │                            ▼
                        │                  PackedVertex + Layer buckets (S4)
                        │                            │
                        ▼                            ▼
        S2 GraphCuller ──► survivors only ──► render.rs (S5 translucent,
                                             S6 LOD cells beyond mesh range)
```

## Verification

From the repository root, with `TMPDIR` pointed at scratch space:

```sh
mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"

# Kernel + port unit tests (cull proofs, mesher, cache, lod, ffi roundtrip)
cargo test --manifest-path client-port/Cargo.toml --offline --locked -j 2
cargo test --manifest-path client-kernel/Cargo.toml --offline --locked -j 2
cargo test --manifest-path client-app/Cargo.toml --offline --locked -j 2

# Culling/perf regression gate: pure-Python math self-test, then the live
# lod_bench/mesh_bench pass asserting quad counts and timing ceilings
python3 tests/culling_perf_gate.py --self-test
python3 tests/culling_perf_gate.py            # builds client-port benches first

# Wire-level client checks that exercise the same kernel through protocol 767
python3 tests/client_mesh_wire.py target/debug/mc-spine   # when binary exists
python3 tests/client_kernel_wire.py target/debug/mc-spine # when binary exists

# Repo gates
bash tools/layers_check.sh
python3 tools/provenance_check.py
git diff --check
```

`tests/culling_perf_gate.py` live mode is the automated regression gate for
this architecture: it fails on any change that alters mesh/LOD output
(mesher, culler or LOD drift) or pushes section/column mesh times past the
pinned ceilings, and it is the place new S2/S4 stages register their budgets.
