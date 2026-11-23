# H20 — `MeshJob::HiZDepth` CPU-fallback payload cost

Status: implemented — cheaper fallback landed (`mesh_pool.rs` only); the
GPU-routing option (a) is documented below as a renderer-side TODO because
the only call site lives outside the permitted file scope.

## What changed (`client-gpu/src/mesh_pool.rs` only)

`MeshJob::HiZDepth` previously held `Vec<HiZSplatSection>`, each section
carrying `Arc<[PackedVertex]>` — every drawn section's whole vertex buffer
pinned from submission until the worker finished the entire job, and the
raster pass streamed the full 64-byte `PackedVertex` stride per quad while
using only 20 bytes of it (`position` per vertex + `attributes` of vertex
0). Each job also allocated a fresh `vec![1.0; 160*90]` depth grid (~57.6
KiB) plus the compact-side vectors.

The worker now runs an extract → rasterize split (options b + c):

- `extract_splat_quads` walks exactly the quads the old inline splat
  walked — `draw_mask`-kept bucket ranges (or the whole stream when
  unbucketed), translucent/water quads skipped, same clamping — and copies
  only the four packed `position` words per quad into a pooled
  `SplatQuad` stream (16 B/quad). `sections` is consumed by value, so
  each section's `Arc` is released as soon as its quads are read: vertex
  buffers are pinned only until a worker dequeues and extracts the job,
  not through rasterization and the channel round-trip. `MeshResult`
  never carried `packed`, so the release point is strictly earlier than
  before.
- `HiZScratch` per worker retains `depth` (the 160×90 grid), `quads`, and
  `spans` across jobs — steady-state frames allocate nothing on the
  worker. Replaces per-job allocation (option c).
- `splat_quad` math is verbatim-identical (same decode, same
  `INSIDE_MARGIN`, same near-plane/`far_z` early-outs, same `min` merge),
  so the produced `HiZPyramid` is bit-identical and the visible set is
  unchanged.

Public API is unchanged: `HiZSplatSection`, `MeshJob`, `MeshResult`,
`MeshJobPool`, `HIZ_PYRAMID_*`, `packed_quad_translucent`,
`compute_opaque_bucket_ranges` — `renderer.rs` compiles untouched.

## Why not option (a) — route around CPU payloads when GPU Hi-Z runs

A GPU pyramid path exists (`GpuHizCuller`,
`client-gpu/src/gpu_hiz.rs`: `build_from_depth`, `encode_cull`), and the
renderer already builds it per frame when the indirect path is armed
(`encode_gpu_hiz_build` at `renderer.rs` ~10971, gated on
`self.gpu_hiz.is_some()` → `gpu_indirect_enabled &&
indirect_execution_supported`).

Skipping the CPU job there is **not a clean swap**, so it stays a TODO:

- The CPU `HiZPyramid` is still consumed every frame by
  `cull_live_sections` (`renderer.rs` ~7569) to produce `self.cull_result`,
  which the CPU draw path (non-arena `live_sections`, bucket masking) and
  `submit_hiz_job`'s occluder skip set both read. The GPU cull writes
  indirect args + masks; it does not feed `cull_result`.
- Therefore a correct gate requires first routing GPU cull results into
  the `cull_result` contract (or splitting consumers), then:

**TODO (renderer owner):** in `submit_hiz_job`
(`client-gpu/src/renderer.rs` ~7197, called from `encode_composed_world`
~10433), early-return when the GPU pyramid is armed for this frame —
i.e. `self.gpu_hiz.as_ref().is_some_and(|h| h.pyramid_ready())` together
with the indirect path enabled — *after* `cull_live_sections` is taught
to consult the GPU cull set so non-arena sections keep their occlusion
coverage. Exact insertion point: immediately after the `hiz_manual`
early-out. Until then every `HiZDepth` job must still be submitted or the
CPU draw path loses occlusion culling entirely.

Also from `audit/ram-vram-role-separation-and-gpu-hiz-design-20261123.md`
§4: the end state removes `LiveTerrainSection::packed` from RAM entirely —
that field is what `submit_hiz_job` clones (`renderer.rs` ~7220) and is
owned by the renderer lane this round.

## Tests (in-file, `mesh_pool::tests`)

- `compact_splat_matches_legacy_grid_and_verdicts_bit_exact` — three
  view-projections × randomized mixed scenes (bucketed/unbucketed,
  masked buckets, translucent/water quads, empty sections, out-of-range
  bucket spans, near-plane straddlers, behind-camera sections): the new
  depth grid is **bit-identical** to a verbatim copy of the old splat,
  every mip level identical, and 512 random AABBs per case return
  identical `cull_world_aabb` verdicts → same visible set.
- `occluder_wall_produces_expected_visible_set` — known-answer: wall
  culls strictly-behind AABBs, keeps nearer and behind-camera AABBs.
- `masked_buckets_and_translucent_quads_never_splat` — masked buckets +
  blended quads contribute nothing.
- `pooled_scratch_survives_jobs_without_realloc` — second job reuses all
  three buffers with zero realloc and still produces the correct grid.
- `hiz_depth_job_releases_vertex_buffers_and_matches_verdict` — real
  pool round-trip; `Weak` proves the job drops the vertex `Arc`.
- `consecutive_hiz_jobs_stay_independent` — pooled scratch never replays
  stale state across jobs.
- `section_geometry_job_still_round_trips`,
  `translucent_decode_matches_layer_and_fluid_bits`,
  `bucket_ranges_count_opaque_only`.

## Verification

Shared working tree is currently mid-flight broken by a sibling change
unrelated to H20 (`CHANNEL_MOD_REGISTRY_SYNC` is being relocated inside
`client-app`; `client-gpu/src/live_session.rs` already imports its new
home), so verification ran
in a `git worktree` at `HEAD` (baseline, all sibling edits absent) with
the new `mesh_pool.rs` copied in — the environment this file must
compile against.

- `cargo check --offline -p megu-client-gpu`: clean, no warnings.
- `cargo test --release --offline -j2 --lib` (in `client-gpu/`):
  **57 passed, 0 failed** — includes all 9 new `mesh_pool::tests`.
- Full `cargo test --release --offline -j2` (all integration targets):
  **166 passed, 0 failed** across 27 binaries. A real GPU adapter was
  present, so the live end-to-end paths ran, including
  `c4c5c6_live_hiz_pyramid_builds_and_culls_occluded_sections` (worker
  builds the pyramid through the new extraction path over real frames,
  installs it, and culls occluded sections) and
  `v4_gpu_compute_hiz_and_section_cull_execute_on_real_gpu_zero_false_negatives`.

`renderer.rs`/`world.rs` untouched; no commits made.
