// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Focused proofs for the client-gpu optimization wiring:
//!
//! 1. mini-HUD `/proc` + DRM sysfs sampling is throttled to
//!    `MINIHUD_SYS_SAMPLE_INTERVAL_MS` while frame stats update every frame.
//! 2. The connect-time `ComputeStrategy`/`KernelAutotuner` are consumed by
//!    the session and pushed into `GpuRenderer`.
//! 3. Dirty section uploads drain through `ChunkUploadBudget` with a
//!    measurable `deferred_uploads` counter.
//! 4. The composed pass runs graph/frustum (+ optional Hi-Z) visibility
//!    checks before GPU draw submission with a `culled_sections` counter.

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client_gpu::live_session::hw_adaptive_engine::{
    ComputeKernelMode, MemoryTransferStrategy,
};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::Camera;
use megu_client_mesh::builtin_shadow_wgsl::RenderScalePreset;
use megu_client_mesh::{
    ChunkUploadBudget, HiZPyramid, RenderBackendPreference, DEFAULT_MAX_UPLOADS_PER_FRAME,
};
use megu_ui::state::UiState;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

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
/// section surrounded by air, matching the live-session test fixture.
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
/// mesh so `world.packed_meshes()`/`section_graphs()` are populated.
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

/// A headless renderer, or `None` when the host has no GPU adapter (the
/// same graceful skip the live-session GPU tests use).
fn try_gpu_renderer() -> Option<GpuRenderer> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP opt-wiring GPU test: probe_adapters found 0 adapters");
        return None;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("probe found adapters but request_gpu_device failed");
    Some(GpuRenderer::new(ctx, OFFSCREEN_FORMAT))
}

// ---------------------------------------------------------------------------
// 1. Mini-HUD host-metric sampling throttle
// ---------------------------------------------------------------------------

#[test]
fn mini_hud_host_sampling_is_throttled_while_frame_stats_update() {
    let mut live = LiveSession::offline_with_null_audio();
    let mut ui = UiState::new();
    ui.settings.qol.cycle_mini_hud(); // Off -> Basic
    assert_ne!(
        ui.settings.qol.mini_hud_preset(),
        megu_ui::state::MiniHudPreset::Off
    );

    // 120 frames at ~120 fps ~= 1.0 s of accumulated frame time. The
    // `/proc`+DRM sampler may run at frame 1 (cold cache) plus once per
    // 500 ms — never once per frame.
    let dt = 1.0f32 / 120.0;
    for _ in 0..120 {
        live.record_frame_and_refresh_minihud(dt, &mut ui);
    }

    assert_eq!(live.frame_timings.frame_count(), 120);
    assert!(
        live.sys_sample_count <= 3,
        "120 frames in ~1s must sample host metrics <= 3 times, got {}",
        live.sys_sample_count
    );
    assert!(
        live.sys_sample_count >= 1,
        "first HUD frame must sample once"
    );
    // Frame-time telemetry still reaches the HUD every frame from the
    // cached snapshot: 120 frames of 8.33 ms is ~120 fps.
    let fps = ui
        .hud
        .mini_hud_data
        .fps
        .expect("mini-HUD fps telemetry still published");
    assert!(
        (110.0..130.0).contains(&fps),
        "frame stats must keep updating every frame, fps={fps}"
    );
}

// ---------------------------------------------------------------------------
// 2. Adaptive compute strategy consumption
// ---------------------------------------------------------------------------

#[test]
fn compute_strategy_and_kernel_autotuner_are_consumed() {
    let mut live = LiveSession::offline_with_null_audio();
    // The connect-time strategy must be a real resolved plan, not a stub.
    assert!(live.hw_caps.logical_cores >= 1);
    assert_eq!(
        live.compute_strategy,
        megu_client_gpu::live_session::hw_adaptive_engine::select_compute_strategy(
            &live.hw_caps
        )
    );

    // Kernel autotuner is seeded from the strategy and fed real frame times.
    let expected_mode = if live.compute_strategy.prefer_lut_over_recompute() {
        ComputeKernelMode::LutBatch
    } else {
        ComputeKernelMode::RecomputeSwar
    };
    assert_eq!(live.active_kernel_mode(), expected_mode);
    let mut ui = UiState::new(); // mini-HUD stays Off: autotuner still feeds
    for _ in 0..8 {
        live.record_frame_and_refresh_minihud(0.008, &mut ui);
    }
    assert!(
        live.kernel_autotuner()
            .estimated_ns(expected_mode)
            .is_some(),
        "frame-time samples must reach the kernel autotuner"
    );

    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    let negotiated = live.apply_compute_strategy(&mut renderer);
    assert_eq!(live.active_compute_strategy_applied(), 1);
    assert!(renderer.compute_strategy_applied());
    assert_eq!(renderer.compute_strategy_apply_count(), 1);

    // Mesh-worker concurrency hint mirrors the strategy's foreground split.
    assert_eq!(
        renderer.mesh_worker_threads,
        live.compute_strategy.workers.foreground_threads
    );
    assert!(renderer.mesh_worker_threads >= 1);

    // Upload-budget preset follows the memory topology.
    let expected_max_uploads = match live.compute_strategy.memory {
        MemoryTransferStrategy::UnifiedZeroCopy => 2, // ChunkUploadBudget::low_spec()
        MemoryTransferStrategy::DiscreteStagedArena => DEFAULT_MAX_UPLOADS_PER_FRAME,
    };
    assert_eq!(
        renderer.upload_budget().max_uploads_per_frame,
        expected_max_uploads
    );

    // Memory topology tunes transfer-layer parameters only: render scale
    // and shadow quality keep their user/device values on BOTH topologies
    // (no forced UMA downgrade — the returned value is the unchanged
    // negotiated shadow quality).
    assert_eq!(renderer.render_scale, RenderScalePreset::Native100);
    assert_eq!(renderer.shadow_quality, negotiated);
    // The transfer budget still tracks the memory topology (the UMA row
    // is RAM-class dependent: Standard/Abundant 3x2 MiB, Constrained
    // 2x1 MiB — both smaller than the discrete 3x8 MiB preset).
    let budget = renderer.transfer_topology_budget();
    match live.compute_strategy.memory {
        MemoryTransferStrategy::UnifiedZeroCopy => {
            assert!(budget.staging_slot_bytes <= 2 * 1024 * 1024);
            assert!(budget.frame_dma_cap_bytes <= 3 * 1024 * 1024);
            assert!(budget.arena_slab_bytes <= 8 * 1024 * 1024);
        }
        MemoryTransferStrategy::DiscreteStagedArena => {
            assert_eq!(budget.staging_ring_slots, 3);
            assert_eq!(budget.staging_slot_bytes, 8 * 1024 * 1024);
            assert_eq!(budget.frame_dma_cap_bytes, 8 * 1024 * 1024);
            assert_eq!(budget.arena_slab_bytes, 32 * 1024 * 1024);
        }
    }

    // Applying again is idempotent and counted.
    live.apply_compute_strategy(&mut renderer);
    assert_eq!(live.active_compute_strategy_applied(), 2);
    assert_eq!(renderer.compute_strategy_apply_count(), 2);
}

// ---------------------------------------------------------------------------
// 3. ChunkUploadBudget-gated section uploads
// ---------------------------------------------------------------------------

#[test]
fn dirty_section_uploads_drain_through_chunk_upload_budget() {
    // Three stone chunks -> three dirty section uploads queued at once.
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0)]);
    let packed_count = live.world.packed_meshes().count();
    assert_eq!(packed_count, 3, "fixture must yield 3 packed sections");

    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    // Deterministic 1-upload-per-frame budget regardless of host strategy.
    renderer.set_upload_budget(ChunkUploadBudget::with_limits(1, usize::MAX, u64::MAX));

    assert_eq!(renderer.update_live_terrain(&live.world), 3);
    assert_eq!(renderer.pending_upload_count(), 3);
    assert_eq!(renderer.live_section_count(), 0);
    assert_eq!(renderer.admitted_uploads, 0);
    assert_eq!(renderer.deferred_uploads, 0);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };

    // Frame 1: budget admits exactly one upload; the other two are deferred.
    renderer
        .render_offscreen_rgba(64, 64, &camera, None)
        .expect("offscreen frame 1");
    assert_eq!(renderer.live_section_count(), 1);
    assert_eq!(renderer.pending_upload_count(), 2);
    assert_eq!(renderer.admitted_uploads, 1);
    assert!(
        renderer.deferred_uploads >= 2,
        "rejected uploads must be counted, got {}",
        renderer.deferred_uploads
    );

    // Frames 2-3 drain the rest; nothing is lost.
    for _ in 0..3 {
        renderer
            .render_offscreen_rgba(64, 64, &camera, None)
            .expect("offscreen drain frame");
    }
    assert_eq!(renderer.live_section_count(), 3);
    assert_eq!(renderer.pending_upload_count(), 0);
    assert_eq!(renderer.admitted_uploads, 3);
}

// ---------------------------------------------------------------------------
// 4. Composed-pass visibility culling
// ---------------------------------------------------------------------------

#[test]
fn composed_pass_culls_off_screen_sections_before_draw_submission() {
    // Camera at [8,20,24] looks -Z: chunks at cz <= 0 are in front, the
    // chunk at cz = +2 sits entirely behind the camera.
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0), (0, 2)]);
    assert_eq!(live.world.packed_meshes().count(), 4);

    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    assert_eq!(renderer.update_live_terrain(&live.world), 4);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    renderer
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("offscreen cull frame");

    // The drain admitted every section on frame 1 (default budget >= 4).
    assert_eq!(renderer.live_section_count(), 4);
    assert_eq!(renderer.pending_upload_count(), 0);
    // Exactly the behind-camera section is skipped before draw submission.
    assert_eq!(
        renderer.last_frame_culled_sections, 1,
        "only the behind-camera section may be culled"
    );
    assert_eq!(renderer.culled_sections, 1);

    // Turning the camera around must cull the previously visible front
    // sections — proving the check is camera-relative, not a one-off.
    let behind = Camera {
        yaw_deg: 180.0,
        ..camera
    };
    renderer
        .render_offscreen_rgba(128, 128, &behind, None)
        .expect("offscreen reversed frame");
    assert!(
        renderer.culled_sections > 1,
        "reversing the camera must cull the front sections too"
    );

    // An installed Hi-Z pyramid participates conservatively: an all-near
    // depth buffer proves every frustum-visible section occluded, so the
    // culled set covers all resident sections.
    let mut renderer2 = try_gpu_renderer().expect("second GPU renderer");
    assert_eq!(renderer2.update_live_terrain(&live.world), 4);
    renderer2.set_hiz_pyramid(Some(
        HiZPyramid::from_depth_buffer(2, 2, &[0.0; 4]).expect("1x1-ish depth pyramid"),
    ));
    renderer2
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("offscreen Hi-Z frame");
    assert_eq!(renderer2.live_section_count(), 4);
    assert_eq!(
        renderer2.last_frame_culled_sections, 4,
        "Hi-Z occlusion must cull every resident section"
    );
}

// ---------------------------------------------------------------------------
// 5. Lane OPT-C4C5C6 proofs: ordered mesh drain, background mesh workers,
//    live Hi-Z feedback, and facing-bucket + region-arena batching.
// ---------------------------------------------------------------------------

/// Ingest chunk packets but leave them dirty so `poll` performs the
/// ordered rebuild itself.
fn session_with_dirty_stone_chunks(chunks: &[(i32, i32)]) -> LiveSession {
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
    live
}

#[test]
fn c4c5c6_poll_drains_dirty_meshes_through_ordered_rebuild() {
    let mut live = session_with_dirty_stone_chunks(&[(0, -2), (0, -1), (0, 0)]);
    assert!(live.world.pending_mesh_sections() > 0);
    assert_eq!(live.ordered_rebuilds_count, 0);

    // A real frustum: camera at the teleport position looking -Z.
    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    let planes = megu_client_gpu::renderer::frustum_planes_from_view_proj(
        &camera.view_projection(16.0 / 9.0),
    );
    live.poll(camera.position, &planes);

    assert!(
        live.ordered_rebuilds_count > 0,
        "the ordered drain must mesh dirty sections on poll"
    );
    assert_eq!(
        live.world.pending_mesh_sections(),
        0,
        "one poll must drain the whole dirty queue under the default budget"
    );
    // Every rebuilt section became a renderer-visible packed mesh with
    // direction buckets (drives bucket culling + arena residency).
    assert_eq!(live.world.packed_meshes().count(), 3);
}

#[test]
fn c4c5c6_background_mesh_jobs_submit_and_drain() {
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0)]);
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };

    assert_eq!(renderer.update_live_terrain(&live.world), 3);
    assert_eq!(renderer.bg_mesh_jobs_completed, 0);
    assert!(
        renderer.bg_mesh_jobs_submitted >= 1,
        "queued uploads must offer speculative jobs to the worker pool"
    );

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    // Worker-built geometry drains at the upload boundary of a later
    // frame; bounded spin keeps the test deterministic on slow hosts.
    for _ in 0..32 {
        renderer
            .render_offscreen_rgba(64, 64, &camera, None)
            .expect("offscreen drain frame");
        if renderer.bg_mesh_jobs_completed >= renderer.bg_mesh_jobs_submitted {
            break;
        }
    }
    assert!(
        renderer.bg_mesh_jobs_completed >= 1,
        "worker pool must complete submitted geometry jobs"
    );
    assert!(renderer.bg_mesh_jobs_completed <= renderer.bg_mesh_jobs_submitted);
    assert_eq!(renderer.live_section_count(), 3);
    assert_eq!(renderer.pending_upload_count(), 0);
}

#[test]
fn c4c5c6_live_hiz_pyramid_builds_and_culls_occluded_sections() {
    // Solid wall one chunk in front of the camera; two sections directly
    // behind it must disappear into the worker-built pyramid's occlusion.
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0)]);
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    assert_eq!(renderer.update_live_terrain(&live.world), 3);
    assert_eq!(renderer.hiz_frames_updated, 0);
    assert_eq!(renderer.hiz_culled_sections, 0);

    // Four blocks in front of the wall: the 16x16 near face covers the
    // whole screen at fov 70, so both far sections project inside it.
    let camera = Camera {
        position: [8.0, 10.0, 20.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        ..Camera::default()
    };
    for _ in 0..16 {
        renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .expect("offscreen hiz frame");
        if renderer.hiz_frames_updated >= 1 && renderer.hiz_culled_sections >= 1 {
            break;
        }
    }
    assert!(
        renderer.hiz_frames_updated >= 1,
        "the live pyramid must be built and installed by real frames"
    );
    assert!(
        renderer.hiz_pyramid().is_some(),
        "auto-built pyramid must be installed on the renderer"
    );
    assert!(
        renderer.hiz_culled_sections >= 1,
        "sections hidden behind the wall must be Hi-Z culled, got {}",
        renderer.hiz_culled_sections
    );
    // The wall itself stays visible: only the hidden sections cull.
    assert!(
        renderer.last_frame_culled_sections < 3,
        "the occluding wall must not cull itself"
    );
}

#[test]
fn c4c5c6_facing_buckets_and_region_arena_execute_live() {
    // Four adjacent same-column chunks: two share one 8x4x8 region, so
    // merged arena draws save real bind-group switches every frame.
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0), (0, 2)]);
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    assert_eq!(renderer.update_live_terrain(&live.world), 4);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    renderer
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("offscreen bucket frame");

    assert!(
        renderer.direction_buckets_skipped > 0,
        "opposite-facing buckets of visible sections must be skipped"
    );
    assert!(
        renderer.region_arena_draws > 0,
        "arena-resident sections must draw through the shared buffers"
    );
    assert!(
        renderer.bind_group_switches_saved > 0,
        "merged region draws must save real bind-group switches"
    );
}

#[test]
fn c4c5c6_region_arena_matches_per_section_pixels() {
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0)]);
    let Some(mut arena_renderer) = try_gpu_renderer() else {
        return;
    };
    let Some(mut flat_renderer) = try_gpu_renderer() else {
        return;
    };
    assert_eq!(arena_renderer.update_live_terrain(&live.world), 3);
    flat_renderer.set_region_arena_enabled(false);
    assert_eq!(flat_renderer.update_live_terrain(&live.world), 3);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    // Drain uploads through a few frames so both residency modes settle.
    let mut arena_pixels = Vec::new();
    let mut flat_pixels = Vec::new();
    for _ in 0..4 {
        arena_pixels = arena_renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .expect("arena frame");
        flat_pixels = flat_renderer
            .render_offscreen_rgba(128, 128, &camera, None)
            .expect("per-section frame");
    }
    assert_eq!(
        arena_pixels, flat_pixels,
        "shared region arenas must rasterize the same pixels as per-section buffers"
    );
}
