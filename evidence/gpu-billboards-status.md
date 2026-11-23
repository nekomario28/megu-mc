# GPU-instanced particle billboards — status (2026-11-23)

Ledger item L29 / M34 residual: replace the CPU-staged per-frame particle
quad path (`set_particle_billboards` built 4 `TerrainVertex` + 6 indices per
particle every frame, 216 B/particle upload) with a GPU-instanced billboard
draw.

## What changed

- `client-port/src/ber_instancing.rs` — extracted the shared helpers so the
  instanced path uses the *identical* math as the CPU path:
  `VanillaParticleWorld::billboard_axes` (the orthonormalized right/up
  `billboard_quads` expanded corners with) and
  `VanillaParticle::billboard_half_size` (the age-scaled half extent).
  `billboard_quads` is unchanged and now delegates to them.
- `client-gpu/src/subworld_render.rs` — new `ParticleInstance` (76 B POD:
  `center` + `axis_u`/`axis_v` half-edges + affine UV basis
  `uv_origin`/`uv_du`/`uv_dv` + `color`), `PARTICLE_QUAD_LAYOUT` (static
  unit quad, slot 0) / `PARTICLE_INSTANCE_LAYOUT` (instance stream, slot 1),
  `pack_particle_billboard` (quad → instance; reports `CpuOnly` for
  non-parallelogram/non-affine-UV quads so the whole call can fall back),
  and WGSL `billboard_vs` + a `LitFragment.alpha` lane multiplied in
  `lit_fs`. `SpriteFrame { rect, anim }` 32-byte `atlas_rects` layout and
  `atlas_sample` untouched.
- `client-gpu/src/renderer.rs` —
  `ParticleInstanceRing` grow-only instance buffer (write-in-place steady
  state, counts into `dynamic_buffer_allocs`/`dynamic_ring_reuses`),
  `composite.billboard` pipeline (`composite-billboard`, same `lit_layout`
  + `ALPHA_BLEND` + `depth_write=false` contract as
  `lit_alpha_pipeline`), `shadow.billboard_color` twin,
  `particle_instancing_supported` limit gate (≥2 vertex buffers, ≥8
  attributes, ≥76 B stride — all guaranteed by default wgpu limits),
  `particle_instance_digest` in the shadow bundle key, and
  `particle_draw_slots` (1 slot instanced vs 1-per-quad CPU) at both
  draw-capacity sites. `set_particle_world` packs particles straight from
  the sim (no intermediate `VanillaParticleQuad`); `set_particle_billboards`
  packs arbitrary quads and falls back per-call.

## Behavior parity

- Same draw order: the instanced draw sits exactly where the CPU particle
  stream drew in the composed pass; instance order = quad order =
  oldest-to-newest spawn order. One `CpuOnly` quad routes the *whole* call
  to the CPU path so output and ordering stay identical.
- Same blending: `ALPHA_BLEND` (src-alpha / one-minus-src-alpha color,
  one / one-minus-src-alpha alpha); per-particle alpha travels in
  `instance.color.a` → `lit_fs` `draw.params.y * fade * in.alpha`, the
  same product the CPU path got from per-draw uniform slots.
- Same expansion: VS computes `center + corner.x*axis_u + corner.y*axis_v`
  with `axis_u = right*size`, `axis_v = up*size` — bit-identical to
  `billboard_quads`' `pos + size*(x*right + y*up)`.
- Rotation/shear: the axis-vector form supports in-plane rotation and
  shear natively (baked into `axis_u`/`axis_v`), not just axis-aligned
  billboards — a superset of the audit's `size+rot` packing.

## Bounds and counters

- `MAX_DYNAMIC_QUADS` bounds per-frame instance uploads (same bound the
  CPU path had). New public counters: `particle_instance_uploads`
  (records written), `particle_instances_capped` (dropped by the cap),
  `particle_cpu_fallbacks` (calls routed to the CPU path). Instance-ring
  allocs/reuses fold into the existing `dynamic_buffer_allocs` /
  `dynamic_ring_reuses`.
- `set_particle_instancing_enabled(false)` forces the CPU path
  (parity testing / pathological adapters); enabling on a device whose
  limits could not build the pipeline is a no-op.

## Upload cost

76 B/particle instance vs 216 B (4×32 B verts + 6×4 B indices + alpha
uniform padding per quad) — ~2.8× less per-frame upload, plus the vertex
and index buffers are now static (4 verts + 6 indices total, written
once) instead of re-uploaded per frame. The audit's 24 B record
(`pos+size+uv-rect+packed color`) is *not* what landed: `VanillaParticleQuad`
carries per-corner `[[f32;2];4]` UVs and arbitrary positions, so the
76 B affine form was chosen to cover every quad the CPU path renders —
anything outside it is `CpuOnly`, never silently reshaped.

## Tests

`client-gpu/tests/gpu_particle_billboards_tests.rs` (new, self-skips
without a GPU adapter, `atlas_gpu_bind_tests.rs` style):

- `particle_instance_layout_and_packing` — CPU unit test: layout strides,
  `pack_particle_billboard` on a real `billboard_quads` output
  (center/axes/UV basis reconstruction), `CpuOnly` on a sheared quad,
  `Skip` on non-finite color.
- `billboard_axes_and_half_size_match_quad_expansion` — pins the
  client-port helpers against `billboard_quads` corner-for-corner.
- `gpu_instanced_billboards_match_cpu_path` — real adapter: renders a
  `VanillaParticleWorld` (Flame/Smoke/Crit/Explosion, mixed alphas) via
  `set_particle_world` on the instanced path, verifies staged records and
  readback pixels differ from a no-particle baseline, repeats the frame
  for determinism, then forces `set_particle_instancing_enabled(false)`
  and checks the CPU fallback counter and a `CpuOnly` quad fallback.

## Verification

- `cargo test --release --offline -j2 --test gpu_particle_billboards_tests`:
  3/3 pass (real adapter present — test did not self-skip).
- `cargo test --release --offline -j2` (client-gpu, full suite): 26 test
  targets, 0 failures — including the pre-existing
  `fx_dynamic_ring_zero_steady_allocs_*` allocation-contract test.
- `cargo check --offline` for `megu-client-gpu` and `megu-client-mesh`: clean.
- `COMPOSITE_LIT_WGSL` re-validated with `naga::valid::Validator`
  (all flags/capabilities): clean.

## Notes / incidents

- A `git stash -u` ran in the shared tree at 22:46 mid-verification
  (parallel worker tooling); the three modified files + the new test file
  were restored from `stash@{0}` (`^3` for the untracked test file)
  without popping, so the worker's own stashed work is untouched.
- 76 B vs the audit's 24 B: deliberate — see "Upload cost". Bandwidth is
  still ~3× down and the CPU path survives as the exact-parity fallback.
