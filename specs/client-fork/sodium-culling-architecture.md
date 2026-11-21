# CF-5 — Graph occlusion culling + coarse LOD: the three-layer architecture

> Filename note: this file keeps its historical name
> (`sodium-culling-architecture.md`) only so existing markdown links stay
> valid. Every implementation described here is a clean-room original Rust
> design built on standard computer-graphics algorithms (visibility-graph
> reachability, frustum culling, directional face bucketing, multi-draw batch
> submission, hierarchical LOD); no third-party mod source code, shaders or
> assets were consulted, copied or ported.

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

The server side already implements the paced, cache-backed half: a bounded,
ack-paced, look-prioritized chunk sender in front of a shared, validated
chunk-packet cache.

| Responsibility | Current implementation |
| --- | --- |
| Chunk packet payload cache (`MapChunkCache`) | `MapChunkCache` / `map_chunk` / `build_map_chunk` / `map_chunk_sig` / `chunk_versions_3x3` in [src/main.rs](../../src/main.rs) (cache struct ~line 9620, `map_chunk` ~line 9740). One assembled `0x27` *payload* per `(DimId, cx, cz)`, validated against the 3×3 `chunk_version_dim` tuple plus an order-independent FNV-1a signature of signs, spawners and neighbor `GenSources`; FIFO cap `MAP_CHUNK_CACHE_CAP = 512`. A hit also restores the chunk's own generated emitters into the caller's `GenSources`. |
| Shared `0x27` frame cache (`ChunkPacketCache`) | `ChunkPacketCache` / `CachedChunkPacket` / `chunk_packet_cache` / `cached_chunk_frame` / `chunk_packet_revision` in [src/connection_limits.rs](../../src/connection_limits.rs) (~line 366) and [src/play_protocol.rs](../../src/play_protocol.rs) (~line 2219). Sits in front of `map_chunk`: one fully framed packet (`varint len` + `0x27` + payload, after the live block-entity splice and `cull_chunk_light`) per `(DimId, cx, cz)` at a world *revision*, shared as `Arc<Vec<u8>>` across every reader at that revision. Each entry stores the uncompressed `plain` frame plus at most one lazily built `compressed` frame memoized per negotiated threshold — a different threshold rebuilds it. The revision is an FNV-1a fold of the 3×3 `chunk_versions` tuple + `map_chunk_sig` + a fingerprint over the sorted live block entities (`live_chunk_block_entities`), so a block edit, a relight or a block-entity change all invalidate. Bounded FIFO `CHUNK_PACKET_CACHE_CAP = 512`; the whole map drops when the generator `generation` moves; a build that races an edit is served via `frame_uncached` (byte-identical, not retained). A hit restores the chunk's own emitters into the caller's `GenSources`, exactly as a rebuild would. |
| Visibility budget | `ChunkSendBudget` (`queued`, `max_chunks_per_tick`) and `prioritize_chunks` (center first, then look-forward, side, behind; distance + coordinate tie-break) in [src/entity_spatial.rs](../../src/entity_spatial.rs); driven per tick by `ChunkDelivery::next_batch` in [src/connection_limits.rs](../../src/connection_limits.rs). |
| Tracking culling | `ChunkWindow` (Chebyshev `(2·view+1)²` window, `contains`/`leaving`/`unsent`) in [src/play_protocol.rs](../../src/play_protocol.rs); `chunk_view_diff` in `entity_spatial.rs`; `ChunkFeedback` ack gate (`0x08` Chunk Batch Received → `sent_batch` → next `0x0c`/`0x0d` batch) in [src/chunk_ack.rs](../../src/chunk_ack.rs). `SERVER_VIEW_DISTANCE = 8` bounds the window; `stream_chunks` emits `0x21` unloads only for chunks the client actually holds. |
| Broadcast range/dimension culling | `broadcast_near` / `broadcast_near_in` in [src/play_protocol.rs](../../src/play_protocol.rs) (~line 1453) over `broadcast_radius` / `within_broadcast_range` in [src/connection_limits.rs](../../src/connection_limits.rs) (~line 480). Vanilla `PlayerList.broadcast` semantics: same `DimId` only, strict `d² < r²` sphere, and the radius is read from the packet itself — sound `0x68` reaches `max(16·volume, 16)`, particles `0x29` reach 512 only under the long-distance flag (else 32), explosions `0x20` reach 64. A packet id with no vanilla positioned contract is refused rather than sent to the whole player list. |
| Wire shaping | `cull_chunk_light` rewrites `map_chunk`'s full light lists into sparse mask+array form in [src/play_protocol.rs](../../src/play_protocol.rs); `splice_chunk_block_entities` merges live block entities (its output feeds the frame revision fingerprint); entity tracking uses per-type `client_tracking_range`/`update_interval` from the 1.21.1 registry (`entity_spatial.rs`, `compute_tracking_diff`). |

Server guarantees the client layer depends on:

- A chunk is sent at most once per window membership; leaving the window
  always produces a `0x21` unload, so "sent set == held set".
- Batch production is bounded by `ChunkFeedback::batch_size()` (the client's
  requested chunks/tick, clamped to the window) and never exceeds one batch
  per tick (`last_batch_tick`).
- `map_chunk` output is byte-stable for identical `(dim, cx, cz, versions,
  sig)` inputs — the cache key *is* the invalidation contract. Any new input
  to `build_map_chunk` must be folded into `map_chunk_sig` or the 3×3
  versions, or the cache silently serves stale terrain. The same rule holds
  one level up: any new input to the *sent* `0x27` frame (post-splice,
  post-light-cull bytes) must be folded into `chunk_packet_revision`, or
  `ChunkPacketCache` serves stale frames to every reader at that revision.

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

### S1 — 6×6 Visibility Graph (kernel + app)

`FaceVisibility` in [client-port/src/visibility.rs](../../client-port/src/visibility.rs)
implements the per-section reachability graph: a packed 36-bit matrix where
bit `enter * 6 + exit` records whether sight entering the section through
face `enter` can leave through face `exit`. A cell is open unless it is a
full-occlusion opaque cube (`OPAQUE` with all six `occlusion` bits set);
air, partial-occlusion cubes, cutout/translucent blocks and fluids all admit
sight. `compute` packs a closed-cell bitmap, then floods each open
connected component with a stack-allocated 512-bit visited bitmap —
allocation-free. `compute_hinted` honours the `uniform_air` /
`sealed_solid` caller guarantees in O(1), matching the
`MC_SPINE_MESH_COLUMN_*` column flags. Reachability is symmetric and
`visible_from(enter)` returns the six-bit exit mask for O(1) graph
traversal. Covered by unit tests in the same file (air = `ALL`, sealed =
`NONE`, enclosed cavity, x=8 wall disconnecting only West↔East, single-file
tunnel, waterlogged opaque still blocking).

The cross-section graph walk that consumes these masks is S2; the
`SectionVis` boundary-face proofs in [cull.rs](../../client-port/src/cull.rs)
remain the cheaper per-node predicate where full reachability is not needed.

### S2 — Frustum × direction-monotonic BFS `GraphCuller` (kernel + app)

`GraphCuller` in [client-port/src/culler.rs](../../client-port/src/culler.rs)
implements the graph occlusion-culling walk: a breadth-first search from the
camera's section over per-section `SectionGraph { through, renderable }`
masks — the same 36-bit `entry * 6 + exit` reachability shape as S1's
`FaceVisibility`. Two monotonicity rules bound the walk: a node never exits
through a face it entered (no backtracking), and a node is only re-queued
when it gains a previously unseen entry face (six bits per section).
Off-frustum sections are culled at enqueue time via `box_in_frustum`, so
they neither draw nor propagate; `CullResult { visible, culled }` reports
BFS-ordered survivors and the occluded count. `SectionGraph::AIR`
propagates without drawing; `SectionGraph::SOLID` draws but never
propagates. Unit tests cover a synthetic 8×4×8 terrain + cave fixture
(≥60% cull), corridor connectivity gating, off-frustum non-traversal,
multi-entry dedup, and a camera inside sealed geometry.

`client-app` calls `GraphCuller` in front of the renderer in
[client-app/src/render.rs](../../client-app/src/render.rs) (`ab06605`).

### S3 — 7-direction bucketed backface culling (kernel + app)

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
`Translucent`, `Fluid` — which is the draw-order contract S4's MultiDraw
consumes: opaque/cutout buckets are order-free, translucent and fluid are
sorted (S5). On top of that, `DirectionBuckets` /
`bucket_draw_mask` in [mesh.rs](../../client-port/src/mesh.rs) add the
seven-way *direction* bucketing: six face-direction buckets plus
`DIRECTION_BUCKET_UNASSIGNED` for renderer-appended geometry. A section's
`box_facing_mask` (which of its six sides face the camera) maps in O(1) to
the draw mask — three back-facing buckets are skipped — and
`DirectionBuckets::group`/`group_into` stably regroups quads into
contiguous per-bucket ranges. The C ABI exposes this as
`mc_spine_client_mesh_bucket_directions` in
[client-port/src/ffi.rs](../../client-port/src/ffi.rs) (null output =
query counts; `DirectionBuckets` reports offsets/counts). The "7-direction"
invariant is unchanged: culling happens before greedy merging, never after.

### S4 — `RenderRegion` 8×4×8 MultiDraw (kernel + app)

[client-port/src/region.rs](../../client-port/src/region.rs) implements the
region layer inside the kernel: an `8×4×8` slab of sections
(`REGION_X/Y/Z`, 256 slots per region) shares one packed-vertex arena and
one index arena. `RenderRegion::upload` carves variable-length
`SectionSlab`s out of the arenas with a first-fit allocator whose free
list is offset-sorted and coalesced on release, so an unload/rebuild cycle
reuses the released range instead of growing the arena; `remove` returns
the slab and drops empty regions via `RenderRegions`. `build_multidraw`
folds every non-empty slot into one `MultiDrawCommand` per
`(region, box_facing_mask bucket)` pair — the `DrawElementsCommand`
parameter block matches `glDrawElementsInstancedBaseVertexBaseInstance` —
replacing per-section draw calls with a single multi-draw submission per
bucket. `client-app` owns a `RenderRegions` store in
[client-app/src/world.rs](../../client-app/src/world.rs) and batches
survivors by region in
[client-app/src/render.rs](../../client-app/src/render.rs) (`ab06605`),
keeping the `PackedVertex`/`Quad` contracts unchanged.

Existing substrate this builds on:

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

### S5 — Incremental translucent sort (kernel scheduler + app)

Translucent correctness requires back-to-front order, but re-sorting the
whole world per frame is the classic churn point. The kernel now provides
two schedulers in [client-port/src/cache.rs](../../client-port/src/cache.rs):

- `MeshCache::rebuild_dirty_ordered(limit, camera, planes)` rebuilds at most
  `limit` dirty sections in-frustum-and-near-camera-first order
  (`queue_key`: off-frustum last, then ascending squared distance to the
  section box), replacing plain coordinate-order `rebuild_dirty` when a
  camera is known.
- `MeshCache::resort_translucent(camera)` is the threshold-gated
  incremental sort: while the camera stays within `resort_threshold`
  (default 1 block, `DEFAULT_RESORT_THRESHOLD`) of the last committed sort
  it is a no-op; once crossed, every clean mesh has its translucent quads
  reordered back-to-front with the same `depth_key` + ascending-index
  tie-break as `mc_spine_client_mesh_sort_translucent`, and only sections
  whose order actually changed are reported. Dirty sections are skipped —
  their rebuild replaces the order anyway.

App side: `client-app`'s dirty-queue drain calls
`rebuild_dirty_ordered(limit, camera, planes)` and the threshold-gated
`resort_translucent(camera)` in
[client-app/src/world.rs](../../client-app/src/world.rs) (`ab06605`).

### S6 — Coarse terrain LOD (kernel + app)

Coarse distant-terrain column summaries for terrain beyond the active
mesh distance, generated from the same `SectionView` data the mesher uses —
no second decode path:

- `summarize_column(views, edge, out)` produces `edge²` `LodCell`s
  (top non-air `state_id`, `tint`, `top_y`, packed `light`) at `LOD_COARSE =
  4` (4×4-block footprints) or `LOD_FINE = 16` (per-block columns), scanning
  sections top-down and stopping once every column found its top —
  [client-port/src/lod.rs](../../client-port/src/lod.rs).
- `LodSummary` owns the cell buffer so regeneration is allocation-free
  (`update` reuses the `Vec`, reports `height()` = covered column height).
- `lod_bench` measures summary cost and the sealed-section culling savings on
  the same 24-section Overworld fixture the mesh bench uses —
  [client-port/src/bin/lod_bench.rs](../../client-port/src/bin/lod_bench.rs).

`client-app` consumes coarse LOD cells through `LodManager` in
[client-app/src/lod.rs](../../client-app/src/lod.rs) (`32a59e4`), which
selects `active_lod_columns` beyond the mesh radius and reuses
`LodCell.state_id/tint/top_y/light` verbatim — no re-interpretation of
block facts.

## Data flow

```
world state ──► server: MapChunkCache ──► 0x27/0x21/0x2a/0x0c paced by
                ChunkSendBudget + ChunkFeedback        ChunkWindow+look dir
                        │
                        ▼
        client-app: DecodedChunk ──► PalettedSection/Block+Light (kernel facts)
                        │
                        ▼
        client-port: MeshCache (dirty tracking) ──► mesh_section (S3 cull,
                        │                          S1 FaceVisibility proofs)
                        │                            │
                        │                            ▼
                        │                  PackedVertex + Layer buckets (S4
                        │                  RenderRegion arenas / MultiDraw)
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
