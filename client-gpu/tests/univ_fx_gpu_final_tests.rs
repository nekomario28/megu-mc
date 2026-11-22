// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane UNIV-FX-GPU-FINAL coverage: front-to-back `RegionArena` ordering,
//! post-opaque sky `DepthCompare::Equal`, TBDR depth `StoreOp::Discard`,
//! vertex-hoisted `atlas_rect`/fog in `COMPOSITE_TERRAIN_WGSL` (Naga),
//! rotation-invariant texel-snapped CSM reuse + per-cascade AABB culling,
//! the persistent `DynamicGeometryRing`, the integer-cell voxel cloud cache
//! and the stack-array `BiomeAtmosphereBlender::is_settled`. GPU tests skip
//! only when the host has no adapter at all.

use mc_spine_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use mc_spine_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use mc_spine_client_gpu::live_session::hw_adaptive_engine::GpuComputeParadigm;
use mc_spine_client_gpu::live_session::{LiveSession, PlayerPose, CLOUD_SUBWORLD_ID};
use mc_spine_client_gpu::renderer::{
    probe_adapters, request_gpu_device, select_depth_store_op, sort_regions_front_to_back,
    ComposedFrame, GpuRenderer, SKY_DEPTH_COMPARE, OFFSCREEN_FORMAT,
};
use mc_spine_client_gpu::subworld_render::{
    box_mesh, SubWorldRenderBank, SubWorldRenderEntry, SubWorldTransform, COMPOSITE_TERRAIN_WGSL,
};
use mc_spine_client_gpu::Camera;
use mc_spine_client_mesh::ber_instancing::{VanillaParticleKind, VanillaParticleQuad};
use mc_spine_client_mesh::builtin_shadow_wgsl::BuiltinShadowQuality;
use mc_spine_client_mesh::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad};
use mc_spine_client_mesh::{ChunkUploadBudget, RenderBackendPreference};
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// A headless renderer, or `None` when the host has no GPU adapter (the
/// same graceful skip the live-session GPU tests use).
fn try_gpu_renderer() -> Option<GpuRenderer> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP univ-fx GPU test: probe_adapters found 0 adapters");
        return None;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("probe found adapters but request_gpu_device failed");
    Some(GpuRenderer::new(ctx, OFFSCREEN_FORMAT))
}

fn varint(bytes: &mut Vec<u8>, mut value: u32) {
    loop {
        let part = (value & 0x7f) as u8;
        value >>= 7;
        bytes.push(part | if value == 0 { 0 } else { 0x80 });
        if value == 0 {
            break;
        }
    }
}

/// Protocol 767 `level_chunk_with_light` body, dimension -64..320 (24
/// sections). Section index 4 covers world y 0..16 — a single solid-stone
/// section surrounded by air.
fn chunk_body(cx: i32, cz: i32, stone_section: i32) -> Vec<u8> {
    let mut sections = Vec::new();
    for section in 0..24 {
        let filled = section == stone_section;
        sections.extend_from_slice(&(if filled { 4096i16 } else { 0 }).to_be_bytes());
        sections.push(0); // single-valued block palette
        varint(&mut sections, if filled { 1 } else { 0 });
        varint(&mut sections, 0); // no packed block values
        sections.push(0); // single-valued biome palette
        varint(&mut sections, 0);
        varint(&mut sections, 0);
    }
    let mut body = Vec::new();
    body.extend_from_slice(&cx.to_be_bytes());
    body.extend_from_slice(&cz.to_be_bytes());
    body.extend_from_slice(&[10, 0]); // empty unnamed heightmap compound
    varint(&mut body, sections.len() as u32);
    body.extend_from_slice(&sections);
    for _ in 0..7 {
        varint(&mut body, 0); // block entities, light masks and arrays
    }
    body
}

/// Ingest one stone-section chunk per `(cx, cz)` and rebuild every pending
/// mesh so `update_live_terrain` has real section geometry to upload.
fn session_with_stone_chunks(chunks: &[(i32, i32)]) -> LiveSession {
    let mut live = LiveSession::offline_with_null_audio();
    live.apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
        x: 8.0,
        y: 20.0,
        z: 24.0,
        yaw: 0.0,
        pitch: -20.0,
        flags: 0,
        teleport_id: 1,
    })))
    .unwrap();
    for &(cx, cz) in chunks {
        let chunk =
            decode_chunk_packet(&chunk_body(cx, cz, 4), 24, 64).expect("decoded real chunk");
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
    }
    for _ in 0..16 {
        if live.world.pending_mesh_sections() == 0 {
            break;
        }
        live.world
            .rebuild_dirty_bounded(256, [8.0, 20.0, 24.0])
            .expect("mesh rebuild");
    }
    assert_eq!(live.world.pending_mesh_sections(), 0);
    live
}

fn assert_nonblank_frame(pixels: &[u8], ctx: &str) {
    let (rgba, tail) = pixels.as_chunks::<4>();
    assert!(tail.is_empty(), "{ctx}: readback length not multiple of 4");
    let first = rgba[0];
    assert!(
        rgba.iter().any(|px| *px != first),
        "{ctx}: frame is a flat color — nothing rendered"
    );
    assert!(
        rgba.iter().all(|px| px[3] == 255),
        "{ctx}: every pixel must end opaque"
    );
}

// ---------------------------------------------------------------------------
// 1. Front-to-back arenas + post-opaque Equal sky + TBDR discard + hoisted WGSL
// ---------------------------------------------------------------------------

#[test]
fn univ_front_to_back_sky_equal_tbdr_discard_and_vertex_hoisted_wgsl() {
    // --- Front-to-back ordering: nearest region center first, ties stable.
    let camera = [8.0, 20.0, 24.0];
    let mut origins = vec![[3, 0, 3], [0, 0, 0], [-2, 0, -1], [1, 0, 0]];
    sort_regions_front_to_back(&mut origins, camera);
    let dist2 = |origin: [i32; 3]| {
        let center = [
            origin[0] as f32 * 16.0 + 64.0,
            origin[1] as f32 * 16.0 + 32.0,
            origin[2] as f32 * 16.0 + 64.0,
        ];
        (0..3)
            .map(|i| (center[i] - camera[i]) * (center[i] - camera[i]))
            .sum::<f32>()
    };
    for pair in origins.windows(2) {
        assert!(
            dist2(pair[0]) <= dist2(pair[1]),
            "regions must draw nearest-first: {origins:?}"
        );
    }

    // --- TBDR depth-store policy matrix.
    assert_eq!(
        select_depth_store_op(GpuComputeParadigm::TileBasedTbdr, false),
        wgpu::StoreOp::Discard,
        "TBDR must discard depth when no readback consumer exists"
    );
    assert_eq!(
        select_depth_store_op(GpuComputeParadigm::TileBasedTbdr, true),
        wgpu::StoreOp::Store,
        "depth readback forces Store even on TBDR"
    );
    for paradigm in [
        GpuComputeParadigm::DiscretePcieHighBw,
        GpuComputeParadigm::UnifiedApuSharedRam,
        GpuComputeParadigm::LowPowerIntegratedWgpu,
        GpuComputeParadigm::HeadlessNoGpu,
    ] {
        assert_eq!(
            select_depth_store_op(paradigm, false),
            wgpu::StoreOp::Store,
            "{paradigm:?} keeps storing depth"
        );
    }

    // --- Sky pipeline depth compare is pinned to the live descriptor.
    assert_eq!(SKY_DEPTH_COMPARE, wgpu::CompareFunction::Equal);
    if let Some(renderer) = try_gpu_renderer() {
        assert_eq!(renderer.sky_depth_compare(), wgpu::CompareFunction::Equal);
    }

    // --- Vertex-hoisted terrain WGSL validates through real Naga and the
    //     atlas rect crosses the stage boundary as a flat varying.
    assert!(
        COMPOSITE_TERRAIN_WGSL.contains("@interpolate(flat) atlas_rect: vec4<f32>"),
        "opaque terrain must carry atlas_rect as a flat interpolant"
    );
    assert!(
        COMPOSITE_TERRAIN_WGSL.contains("out.fog = fog_factor_at(world);"),
        "terrain_vs must compute the fog factor per vertex"
    );
    let module = wgsl::parse_str(COMPOSITE_TERRAIN_WGSL).unwrap_or_else(|err| {
        panic!(
            "COMPOSITE_TERRAIN_WGSL: naga parse failed:\n{}",
            err.emit_to_string(COMPOSITE_TERRAIN_WGSL)
        )
    });
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|err| {
            panic!(
                "COMPOSITE_TERRAIN_WGSL: naga validation failed:\n{}",
                err.emit_to_string(COMPOSITE_TERRAIN_WGSL)
            )
        });

    // --- Real-GPU pixel-valid frame: sky dome + opaque subworld geometry.
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    let mut bank = SubWorldRenderBank::new();
    let (floor_vertices, floor_indices) =
        box_mesh([-8.0, -0.2, -8.0], [8.0, 0.0, 8.0], [0.6, 0.6, 0.6]);
    bank.upsert(SubWorldRenderEntry::new(
        21,
        SubWorldTransform::IDENTITY,
        floor_vertices,
        floor_indices,
    ));
    let (box_vertices, box_indices) =
        box_mesh([0.0, 0.0, -2.0], [2.0, 3.0, 0.0], [0.6, 0.6, 0.6]);
    bank.upsert(SubWorldRenderEntry::new(
        22,
        SubWorldTransform::IDENTITY,
        box_vertices,
        box_indices,
    ));
    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = Camera {
        position: [-1.0, 8.0, 14.0],
        yaw_deg: 0.0,
        pitch_deg: -30.0,
        far: 96.0,
        ..Camera::default()
    };
    let (width, height) = (320, 240);
    let pixels = renderer
        .render_offscreen_composed_frame(
            width,
            height,
            &camera,
            &mut ComposedFrame {
                subworlds: &mut bank,
                lights: &[],
                atmosphere: &blender,
                chunk_fades: &tracker,
                ui_rgba: None,
                world_revision: 1,
                world_time_ticks: 6000,
            },
        )
        .expect("GPU composed frame");
    assert_nonblank_frame(&pixels, "univ-fx composed frame");

    // The sky dome shades depth-untouched pixels while the floor/box cover
    // the lower half: top-row pixels must differ from a geometry pixel.
    let px = |x: usize, y: usize| {
        let i = (y * width as usize + x) * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    let sky_px = px(width as usize / 2, 2);
    let ground_px = px(width as usize / 2, height as usize - 4);
    assert!(
        (0..3)
            .map(|c| (sky_px[c] as i32 - ground_px[c] as i32).abs())
            .sum::<i32>()
            > 12,
        "sky dome (top) and lit geometry (bottom) must shade differently: \
         sky={sky_px:?} ground={ground_px:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Rotation-invariant texel-snapped CSM + per-cascade AABB culling
// ---------------------------------------------------------------------------

#[test]
fn fx_csm_rotation_invariant_texel_snap_zero_rerenders_and_cascade_culling() {
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    // One near chunk inside every cascade volume plus a far chunk 320
    // blocks east — a different RegionArena outside cascade-0's light AABB.
    let live = session_with_stone_chunks(&[(0, 0), (20, 0)]);
    // Effectively unbounded, but finite: the adaptive controller adds to
    // `uploads_per_frame` and would overflow on `u32::MAX`.
    renderer.set_upload_budget(ChunkUploadBudget::with_limits(
        1_000_000,
        1 << 40,
        u64::MAX / 4,
    ));
    assert!(renderer.update_live_terrain(&live.world) >= 2);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    // Warm-up frames drain every queued upload while shadows are still Off.
    for _ in 0..4 {
        renderer
            .render_offscreen_rgba(64, 64, &camera, None)
            .expect("warm-up frame");
    }
    assert_eq!(renderer.pending_upload_count(), 0);
    assert!(renderer.live_section_count() >= 2);

    let negotiated = renderer.set_shadow_quality(BuiltinShadowQuality::High);
    if negotiated.config().cascade_count == 0 {
        eprintln!("SKIP CSM rotation test: adapter negotiated Off");
        return;
    }
    // First shadowed frame renders the whole cascade array (FirstFrame).
    renderer
        .render_offscreen_rgba(64, 64, &camera, None)
        .expect("first shadow frame");
    assert_eq!(renderer.shadow_rerender_count, 1);
    assert!(
        renderer.shadow_regions_culled > 0,
        "far RegionArena must be AABB-culled from at least one cascade"
    );

    // Ten pure look rotations — position, sun and world stay fixed, so no
    // cascade re-render may be scheduled: snapped bounding-sphere orthos
    // do not see yaw/pitch at all.
    for i in 0..10 {
        let rotated = Camera {
            yaw_deg: (i as f32 + 1.0) * 11.25,
            pitch_deg: -20.0 + (i as f32 - 4.5) * 3.0,
            ..camera
        };
        renderer
            .render_offscreen_rgba(64, 64, &rotated, None)
            .expect("rotated frame");
    }
    assert_eq!(
        renderer.shadow_rerender_count, 1,
        "pure camera rotation must never re-render a shadow cascade"
    );
    assert!(
        renderer.shadow_rotation_reuses >= 9,
        "rotation reuses must cover the look frames: {}",
        renderer.shadow_rotation_reuses
    );
    assert!(renderer.shadow_reuse_count >= 9);
}

// ---------------------------------------------------------------------------
// 3. DynamicGeometryRing steady state + cloud cell cache + settled blender
// ---------------------------------------------------------------------------

#[test]
fn fx_dynamic_ring_zero_steady_allocs_cloud_cell_reuse_and_atmosphere_settled() {
    // --- Persistent dynamic geometry: one warm-up allocation per ring,
    //     every later same-size update is a write_buffer into it.
    let Some(mut renderer) = try_gpu_renderer() else {
        eprintln!("SKIP dynamic-ring test: no GPU adapter");
        return;
    };
    let mut mesh = ExtrudedMesh::new();
    mesh.quads.push(ExtrudedQuad {
        positions: [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ],
        normal: [0.0, 0.0, 1.0],
        uv: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        tint_index: 0,
    });
    let particle = VanillaParticleQuad {
        kind: VanillaParticleKind::Flame,
        positions: [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ],
        uv: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        color: [1.0, 1.0, 1.0, 0.8],
    };
    renderer.set_entity_and_ber_meshes(vec![(&mesh, [0.0, 64.0, 0.0], [1.0, 1.0, 1.0])]);
    renderer.set_particle_billboards(&[particle]);
    let warm_up_allocs = renderer.dynamic_buffer_allocs;
    assert_eq!(warm_up_allocs, 2, "one alloc per ring at warm-up");
    for _ in 0..4 {
        renderer
            .set_entity_and_ber_meshes(vec![(&mesh, [0.0, 64.0, 0.0], [1.0, 1.0, 1.0])]);
        renderer.set_particle_billboards(&[particle]);
    }
    assert_eq!(
        renderer.dynamic_buffer_allocs, warm_up_allocs,
        "zero create_buffer calls in steady state"
    );
    assert!(
        renderer.dynamic_ring_reuses >= 4,
        "same-buffer uploads must reuse the ring: {}",
        renderer.dynamic_ring_reuses
    );

    // --- Integer-cell voxel cloud cache: sub-cell ticks slide the model
    //     matrix instead of rebuilding geometry.
    let mut live = LiveSession::offline_with_null_audio();
    live.vanilla_plus_cfg.nature.enabled = true;
    live.vanilla_plus_cfg.nature.layered_voxel_clouds = true;
    live.pose = Some(PlayerPose {
        feet: [0.0, 80.0, 0.0],
        yaw: 0.0,
        pitch: 0.0,
    });
    live.world.time_of_day = 6000;
    // The deterministic noise field can be empty under a given camera —
    // step the camera until the first refresh really built the sheet.
    let mut built = false;
    for step in 0..16 {
        live.pose = Some(PlayerPose {
            feet: [0.0, 80.0, step as f64 * 96.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.refresh_voxel_clouds();
        if live.subworlds.get(CLOUD_SUBWORLD_ID).is_some() {
            built = true;
            break;
        }
    }
    assert!(built, "cloud field must produce geometry for some camera");
    let base_reuses = live.cloud_mesh_reuses;
    // 0.4 blocks/tick into a 12-block cell: ticks +1..+5 stay in-cell and
    // must each be a transform-only hit.
    for _ in 0..5 {
        live.world.time_of_day += 1;
        live.refresh_voxel_clouds();
    }
    assert!(
        live.cloud_mesh_reuses >= base_reuses + 4,
        "in-cell scroll must reuse the cached mesh: {}",
        live.cloud_mesh_reuses
    );
    let transform = live
        .subworlds
        .get(CLOUD_SUBWORLD_ID)
        .expect("cloud subworld present")
        .transform;
    assert_eq!(transform.position[1], 0.0);
    assert_eq!(transform.position[2], 0.0);
    assert!(
        transform.position[0] < 0.0 && transform.position[0] > -12.0,
        "in-cell slide must be the sub-cell X fraction: {:?}",
        transform.position
    );

    // --- Stack-array is_settled: a snapped blender converges exactly.
    let mut blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let target = AtmosphereParams {
        fog_density: 0.05,
        sky_horizon_rgb: [0.4, 0.5, 0.7],
        ..AtmosphereParams::default()
    };
    blender.set_target(target);
    assert!(!blender.is_settled(0.0), "fresh target must be unsettled");
    blender.snap_to_target();
    assert!(
        blender.is_settled(0.0),
        "snap_to_target must be exactly settled"
    );
}

