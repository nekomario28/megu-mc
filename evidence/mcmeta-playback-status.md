# `.png.mcmeta` draw-time playback — wiring status

Date: 2026-11-23. Scope: `client-port`, `client-app`, `client-gpu` only.
No new dependencies; vertex formats unchanged.

## Design

The whole vertical `.mcmeta` strip is packed into the atlas (pre-existing
`PackedAtlas` behaviour); `placement_uv_rect`/`tile_rect` describe the
whole strip. Playback narrows the sampling window per sprite instead of
re-packing or re-meshing:

- `atlas_rects` storage table grew from `vec4<f32>` per sprite to
  `SpriteFrame { rect: vec4, anim: vec4 }` (32 B stride).
  `rect` = active frame window `(u0, v0, u_scale, v_scale/strip_frames)`;
  `anim` = `(next slot v0, interpolate blend, 0, 0)`.
- `SpriteAnimation::rect_pair_at_tick(tile, tick)` (client-port) computes
  the pair purely from the tick; `GpuRenderer::tick_atlas_animations`
  rewrites only animated sprites' 32-byte slots via `queue.write_buffer`
  when the tick advances (steady frames cost one compare). No geometry
  or vertex-buffer rewrites.
- Clock: `LiveSession::atlas_anim_clock` accumulates wall seconds in
  `poll` at the same 50 ms heartbeat as the VFX/entity systems;
  `atlas_anim_tick()` = `clock * 20`. `window.rs` calls
  `tick_atlas_animations(live.atlas_anim_tick())` every frame. Vanilla
  cadence (client tick = 50 ms, `frametime` units are ticks). The clock
  is client-side wall time, matching vanilla `TextureManager` — not
  `world_time_ticks`, which wraps at 24 000 and follows server
  `set_time` jumps.
- `interpolate: true` blends current and cyclic-successor slots in the
  fragment shader (`mix(cur, next, anim.y)`); `anim.y == 0` skips the
  second fetch so static sprites keep one texture read.

## Consumers updated

- `COMPOSITE_TERRAIN_WGSL` (opaque + translucent): `SpriteFrame` table,
  `atlas_frame_for(state_id)` hoists the `state -> sprite -> frame`
  lookup into the vertex stage as before; flat `atlas_rect` +
  `atlas_anim` varyings; `atlas_sample_rect` does the optional second
  fetch.
- `COMPOSITE_LIT_WGSL`: same table layout; `atlas_sample` blends when
  `anim.y > 0`. Covers instanced subworld parts (`atlas_index`).
- `MEGU_SHADOW_CUTOUT_DEPTH_WGSL`: `sample_atlas_alpha` blends alpha the
  same way so the cutout mask tracks the colour frame.
- Shadowed-terrain pipelines are text-transformed from the composite
  WGSL: the `var color = in.albedo;` anchor and the `region_opaque_wgsl`
  / `region_caster_wgsl` packed-position decode anchors are preserved.
- client-app `TERRAIN_OPAQUE_WGSL`/`TERRAIN_TRANSLUCENT_WGSL` are the
  palette fallback — no atlas sampling, unchanged.

## State -> sprite wiring (was dead code)

`set_block_state_sprites` previously had no production caller, so the
shader's `atlas_states` indirection stayed all-`u32::MAX` and every quad
fell back to palette colour. `upload_pending_atlas` now:

1. builds `state_id -> sprite` assignments via
   `state_sprite_assignments` — the registry/`BlockTable` names blocks
   per state id but retains no per-state properties, so each block is
   resolved once to a *representative* texture: the model's `particle`
   texture (then `all`/`side`/`up`/`front`/`top`, then any resolved id)
   under synthetic props from `BlockStateDef::representative_props`,
   resolved through `ResourcePackModelStack::resolved_textures`;
2. uploads them through `set_block_state_sprites` (name resolution now
   mirrors `find_placement`: exact, path-part, `minecraft:`-prefixed);
3. registers `CompiledAssetMeta.animations` via `set_atlas_animations`.

Blocks whose representative texture isn't packed in the atlas are
skipped (log line prints the bound/animated counts), so one missing
texture cannot fail the whole table.

## Verified

- `cargo test --release --offline -j2 --manifest-path client-port/Cargo.toml`
  — all green, incl. new `rect_pair_at_tick_advances_frame_window_and_blends`
  (window V offsets/scales, blend factor, wrap, custom frame order).
- `cargo test --release --offline -j2 --manifest-path client-gpu/Cargo.toml`
  — Naga validation of every edited/transformed WGSL module; new
  `gpu_atlas_mcmeta_playback_advances_and_blends` ran on a real adapter
  (Vulkan AMD Radeon RX 7800 XT): tick 0 -> 5539 red px, `tick_atlas_animations(1)`
  -> 5701 blue px, tick 2 -> 5539 red px (wrap), and the `interpolate`
  blend tick -> 5952 mixed px. Same geometry, no re-mesh or re-upload.
- `cargo test --release --offline -j2 --manifest-path client-app/Cargo.toml`
  — all green except one pre-existing environmental failure noted below.

## Known limitations

- Representative-texture granularity: one sprite per block (vanilla's
  own `particle` stand-in), because the packed-vertex path carries only
  a `state_id`. Per-face/per-variant model UVs are a larger change
  outside this task's scope.
- `interpolate` blends are tick-quantized (vanilla blends per rendered
  frame fraction; here `blend` = elapsed fraction of the current
  frame's duration at whole-tick resolution — a 50 ms quantization,
  below perceptual threshold for vanilla's 1-2 tick frametimes).
- Pre-existing failures unrelated to this change (their code paths are
  untouched by this diff):
  - `client-app integrated::tests::resolve_server_bin_...` fails when run
    inside this repo — `find_runtime_root` sorts candidate ancestors
    alphabetically and the repo root itself contains
    `config_packets.bin`/`datapacks/`/`assets/block_states_1211.json`,
    so `<repo>` outranks the test's `/tmp` runtime.
  - `client-gpu wire_client_math_qol_tests::
    world_block_updates_drain_coalesced_light_queue` — the test is
    written for a checkout *without* generated assets; this checkout has
    untracked `assets/block_state_facts_1211.bin` (~26k states), which
    `Registry::state_facts` discovers, so the test's 2-state synthetic
    pack fails `state_count` validation (`InvalidJson`) before any code
    this diff touches runs.
