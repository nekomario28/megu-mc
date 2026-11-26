// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane GPU-INDIRECT-AND-OPT-WIRE proofs on the real adapter:
//!
//! 1. The composed opaque pass submits `draw_indexed_indirect` /
//!    `multi_draw_indexed_indirect` from a GPU-written args buffer fed by
//!    `GpuHizCuller` — zero CPU readback in the render path — renders
//!    pixel-identical frames to the direct-draw path (zero false-negative
//!    missing geometry), and the section behind a full opaque wall carries
//!    `instance_count == 0` once the frame-depth pyramid is built.
//! 2. The exact p-vertex AABB frustum test and the distance-ring + 3D
//!    Morton Z-order tie-break ordering are deterministic and keep every
//!    frustum-touching box (no false negatives).
//! 3. `LiveSession` drives `KernelAutotuner::observe_sample` with real
//!    mesh-batch durations and the renderer feeds real frame P95 into the
//!    upload budget — the counters prove both feedback loops execute.

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client_gpu::gpu_hiz::IndirectArgs;
use megu_client_gpu::live_session::hw_adaptive_engine::DomainKind;
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    aabb_in_frustum_pvertex, frustum_planes_from_view_proj, morton3d,
    opaque_section_sort_key, probe_adapters, request_gpu_device,
    section_morton_key, GpuDevice, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::window::GpuApp;
use megu_client_gpu::{Camera, GpuCliConfig};
use megu_client_mesh::{box_in_frustum, RenderBackendPreference, SectionCoord, EDGE};
use megu_ui::state::UiState;

// ---------------------------------------------------------------------------
// Shared fixtures (mirrors the opt-wiring live-session fixture)
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

/// A headless GPU device, or `None` when the host has no adapter (the
/// same graceful skip the other GPU tests use).
fn try_gpu() -> Option<GpuDevice> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP gpu-indirect test: probe_adapters found 0 adapters");
        return None;
    }
    Some(
        request_gpu_device(RenderBackendPreference::Auto, None, None)
            .expect("probe found adapters but request_gpu_device failed"),
    )
}

/// Copy `size` bytes of `src` to a map-read buffer and return the bytes —
/// test verification only; the production indirect path never reads back.
fn read_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    src: &wgpu::Buffer,
    size: u64,
) -> Vec<u8> {
    let dst = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-readback"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("test-readback"),
    });
    encoder.copy_buffer_to_buffer(src, 0, &dst, 0, size);
    queue.submit([encoder.finish()]);
    let slice = dst.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let bytes = slice.get_mapped_range().unwrap().to_vec();
    dst.unmap();
    bytes
}

// ---------------------------------------------------------------------------
// 1. GPU-driven indirect opaque path on the real adapter
// ---------------------------------------------------------------------------

#[test]
fn gpu_indirect_draw_matches_direct_render_pixels_and_builds_hiz_pyramid() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    let info = ctx.adapter.get_info();
    eprintln!(
        "adapter: {} ({:?}) — INDIRECT_EXECUTION gate decides this test",
        info.name, info.backend
    );
    let mut indirect = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    if !indirect.indirect_execution_supported() {
        eprintln!("SKIP: adapter lacks DownlevelFlags::INDIRECT_EXECUTION");
        return;
    }
    indirect.set_gpu_indirect_enabled(true);

    // Scene: a solid wall section directly ahead, a solid section fully
    // behind it, and a third beside the wall — all arena-resident, all
    // submitted to the GPU cull pass as `SectionGpuRecord`s.
    let live = session_with_stone_chunks(&[(0, 0), (0, -1), (1, 0)]);
    assert_eq!(indirect.update_live_terrain(&live.world), 3);

    // Direct-draw reference on a second renderer over the same adapter.
    let Some(ctx2) = try_gpu() else {
        return;
    };
    let mut direct = GpuRenderer::new(ctx2, OFFSCREEN_FORMAT);
    assert_eq!(direct.update_live_terrain(&live.world), 3);

    // Camera four blocks in front of the wall's 16x16 face — the face
    // overfills the frame, so section (0,0,-1) behind it is fully
    // occluded once the pyramid exists.
    let camera = Camera {
        position: [8.0, 10.0, 20.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        ..Camera::default()
    };

    // Frame 1: no pyramid yet — `params.z == 0` disarms occlusion, every
    // submitted section draws through the indirect path.
    let indirect_px1 = indirect
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("indirect frame 1");
    let direct_px1 = direct
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("direct frame 1");
    assert_eq!(
        indirect_px1, direct_px1,
        "indirect pass must rasterize the same pixels as direct draws \
         (zero false-negative missing geometry)"
    );
    assert!(
        indirect_px1
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[0] != 0 || px[1] != 0 || px[2] != 0),
        "rendered frame must be non-blank"
    );
    assert!(
        indirect.gpu_indirect_draw_passes >= 1,
        "opaque pass must submit GPU indirect draws, got {}",
        indirect.gpu_indirect_draw_passes
    );
    assert!(
        indirect.gpu_indirect_sections_submitted >= 2,
        "the wall + occluded + side sections must submit records, got {}",
        indirect.gpu_indirect_sections_submitted
    );
    assert!(
        indirect.gpu_hiz_pyramid_builds >= 1,
        "frame-depth pyramid must build after the composed pass, got {}",
        indirect.gpu_hiz_pyramid_builds
    );

    // Frame 2: same camera — the pyramid built under this exact
    // view-projection arms occlusion, so the section behind the opaque
    // wall must carry `instance_count == 0` in the GPU-written args.
    let indirect_px2 = indirect
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("indirect frame 2");
    let direct_px2 = direct
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("direct frame 2");
    assert_eq!(
        indirect_px2, direct_px2,
        "the GPU-culled section was invisible anyway — pixels stay identical"
    );

    let coords = indirect.gpu_indirect_record_coords();
    let args_buffer = indirect
        .gpu_indirect_args_buffer()
        .expect("indirect buffers live after two frames")
        .clone();
    let bytes = read_buffer(
        &indirect.gpu().device,
        &indirect.gpu().queue,
        &args_buffer,
        (coords.len() * std::mem::size_of::<IndirectArgs>()) as u64,
    );
    let args: &[IndirectArgs] = bytemuck::cast_slice(&bytes);
    let find = |target: SectionCoord| {
        coords
            .iter()
            .position(|&c| c == target)
            .map(|i| args[i].instance_count)
    };
    assert_eq!(
        find(SectionCoord(0, 0, -1)),
        Some(0),
        "section behind the opaque wall must be GPU-culled on frame 2"
    );
    assert_eq!(
        find(SectionCoord(0, 0, 0)),
        Some(1),
        "the occluding wall itself must stay visible"
    );
}

// ---------------------------------------------------------------------------
// 2. P-vertex frustum + distance-ring/Morton ordering
// ---------------------------------------------------------------------------

#[test]
fn morton_front_to_back_sort_and_pvertex_frustum_have_zero_false_negatives() {
    // Single-plane sanity: inside half-space `dot(n,p) + d >= 0`.
    let right_plane = [[1.0, 0.0, 0.0, 5.0]]; // x >= -5
    assert!(aabb_in_frustum_pvertex([0.0; 3], [1.0; 3], &right_plane));
    // A box that merely *touches* the boundary stays (zero false
    // negatives) — only fully-outside boxes cull.
    assert!(aabb_in_frustum_pvertex(
        [-5.0, -5.0, -5.0],
        [-5.0, 5.0, 5.0],
        &right_plane
    ));
    assert!(!aabb_in_frustum_pvertex(
        [-10.0, -5.0, -5.0],
        [-6.0, 5.0, 5.0],
        &right_plane
    ));

    // Real camera planes: the p-vertex contract must agree with
    // `box_in_frustum` (same six-dot-product semantics) on every section
    // around the camera, including grazers — no false cull of a visible
    // box is tolerated.
    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    let view_proj = camera.view_projection(16.0 / 9.0);
    let planes = frustum_planes_from_view_proj(&view_proj);
    let mut checked = 0u32;
    let edge = EDGE as f32;
    for x in -4..=4 {
        for y in -2..=3 {
            for z in -4..=4 {
                let min = [x as f32 * edge, y as f32 * edge, z as f32 * edge];
                let max = [min[0] + edge, min[1] + edge, min[2] + edge];
                assert_eq!(
                    aabb_in_frustum_pvertex(min, max, &planes),
                    box_in_frustum(min, &planes),
                    "p-vertex test must match box_in_frustum at {min:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 300, "the sweep must exercise hundreds of boxes");
    // Sanity anchor: the wall section in front of the camera is inside,
    // the section behind the camera is out.
    assert!(aabb_in_frustum_pvertex([0.0, 0.0, 0.0], [16.0; 3], &planes));
    assert!(!aabb_in_frustum_pvertex(
        [0.0, 0.0, 32.0],
        [16.0, 16.0, 48.0],
        &planes
    ));

    // Morton key sanity: the canonical Z-order spread.
    assert_eq!(morton3d(0, 0, 0), 0);
    assert_eq!(morton3d(1, 0, 0), 1);
    assert_eq!(morton3d(0, 1, 0), 2);
    assert_eq!(morton3d(0, 0, 1), 4);
    assert_eq!(morton3d(1, 1, 1), 7);
    // Sign-folded lanes keep i32 order stable — distinct coords, distinct
    // keys, deterministic.
    assert_ne!(section_morton_key(SectionCoord(0, 0, 0)),
               section_morton_key(SectionCoord(0, 0, -1)));

    // Front-to-back: quantized 16-block distance rings are
    // non-decreasing, and ties resolve through the Morton key — the sort
    // must be deterministic across runs.
    let camera_pos = camera.position;
    let mut sections: Vec<SectionCoord> = (-3..=3)
        .flat_map(|x| (-3..=3).map(move |z| SectionCoord(x, 0, z)))
        .collect();
    let mut ordered = sections.clone();
    ordered.sort_by_key(|&c| opaque_section_sort_key(c, camera_pos));
    let mut ordered2 = sections.clone();
    ordered2.sort_by_key(|&c| opaque_section_sort_key(c, camera_pos));
    assert_eq!(ordered, ordered2, "sort must be deterministic");
    let rings: Vec<u32> = ordered
        .iter()
        .map(|&c| opaque_section_sort_key(c, camera_pos).0)
        .collect();
    assert!(
        rings.windows(2).all(|w| w[0] <= w[1]),
        "distance rings must be non-decreasing front-to-back"
    );
    // The nearest ring's first section is the camera's closest section —
    // a global distance minimum must lead the submission order.
    let closest = sections
        .iter()
        .copied()
        .min_by_key(|&c| {
            let edge = EDGE as f32;
            let dx = c.0 as f32 * edge + edge * 0.5 - camera_pos[0];
            let dy = c.1 as f32 * edge + edge * 0.5 - camera_pos[1];
            let dz = c.2 as f32 * edge + edge * 0.5 - camera_pos[2];
            (dx * dx + dy * dy + dz * dz).to_bits()
        })
        .expect("non-empty");
    assert_eq!(ordered[0], closest, "nearest section submits first");
    // Shuffling the input must not change the deterministic output order.
    sections.reverse();
    let mut ordered3 = sections;
    ordered3.sort_by_key(|&c| opaque_section_sort_key(c, camera_pos));
    assert_eq!(ordered, ordered3, "input permutation must not reorder output");
}

// ---------------------------------------------------------------------------
// 3. Live autotuner + upload-budget P95 feedback
// ---------------------------------------------------------------------------

#[test]
fn live_session_feeds_kernel_autotuner_and_upload_budget_p95_feedback() {
    let mut live = session_with_dirty_stone_chunks(&[(0, -2), (0, -1), (0, 0)]);
    assert_eq!(live.autotuner_samples(), 0);
    assert!(live.world.pending_mesh_sections() > 0);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    let planes = frustum_planes_from_view_proj(&camera.view_projection(16.0 / 9.0));
    // `poll` drains the dirty queue through a measured batch rebuild —
    // the elapsed_ns lands in the autotuner.
    live.poll(camera.position, &planes);
    assert!(
        live.autotuner_samples() >= 1,
        "measured mesh-batch duration must feed observe_sample, got {}",
        live.autotuner_samples()
    );
    // Frame-time feedback feeds the autotuner too.
    let mut ui = UiState::new();
    live.record_frame_and_refresh_compact_debug_hud(0.008, &mut ui);
    assert!(
        live.autotuner_samples() >= 2,
        "frame-time sample must feed observe_sample, got {}",
        live.autotuner_samples()
    );

    // Domain projections: the unified plan must expose the three
    // client-gpu domains with the expected kinds.
    let hints = *live.domain_hints();
    assert_eq!(hints.chunk_mesh.domain, DomainKind::Meshing);
    assert_eq!(hints.gpu_cull.domain, DomainKind::Lighting);
    assert_eq!(hints.shadow.domain, DomainKind::ShadowRendering);

    let Some(ctx) = try_gpu() else {
        return;
    };
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    let applied = live.apply_compute_strategy(&mut renderer);
    assert_eq!(live.active_compute_strategy_applied(), 1);
    assert_eq!(applied, renderer.shadow_quality);
    // The chunk_mesh hint lands on the renderer's ordering flag.
    assert_eq!(renderer.morton_order_enabled, hints.chunk_mesh.use_morton_layout);
    // gpu_cull hint: discrete adapters with INDIRECT_EXECUTION get the
    // GPU-driven path; TBDR/iGPU single-pass plans keep it off.
    assert_eq!(
        renderer.gpu_indirect_enabled,
        renderer.indirect_execution_supported() && !hints.gpu_cull.prefer_single_pass_gpu
    );

    // Composed frames feed the measured frame P95 into the upload
    // budget — the observation counter must advance.
    for _ in 0..3 {
        renderer
            .render_offscreen_rgba(64, 64, &camera, None)
            .expect("offscreen frame");
    }
    assert!(
        renderer.upload_budget_p95_observations() >= 1,
        "measured frame P95 must feed the upload budget, got {}",
        renderer.upload_budget_p95_observations()
    );
}

// ---------------------------------------------------------------------------
// C2 — live-session attach activates the compute strategy once
// ---------------------------------------------------------------------------

/// `attach_live`/`connect_to` arm the pending flag; the first live frame
/// pushes the session's `ComputeStrategy` exactly once per renderer and
/// the indirect gate matches `indirect_execution_supported()` and the
/// unified plan — fail-closed on adapters without indirect+compute.
#[test]
fn attach_arms_strategy_and_first_live_frame_pushes_once() {
    let mut app = GpuApp::new(GpuCliConfig::default());
    app.attach_live(LiveSession::offline_with_null_audio());
    assert!(
        app.live_strategy_pending(),
        "attach must arm the compute-strategy push"
    );

    let Some(ctx) = try_gpu() else {
        return;
    };
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    assert_eq!(renderer.compute_strategy_apply_count(), 0);

    app.push_pending_compute_strategy(&mut renderer);
    assert!(!app.live_strategy_pending());
    assert_eq!(renderer.compute_strategy_apply_count(), 1);
    let hints = *app.live().as_ref().unwrap().domain_hints();
    assert_eq!(
        renderer.gpu_indirect_enabled,
        renderer.indirect_execution_supported() && !hints.gpu_cull.prefer_single_pass_gpu,
        "indirect must engage only on capable adapters the plan allows"
    );

    // Later frames are free: the flag stays cleared and the renderer is
    // not re-negotiated.
    app.push_pending_compute_strategy(&mut renderer);
    assert_eq!(renderer.compute_strategy_apply_count(), 1);

    // A re-attached session re-arms — the next live frame pushes onto
    // the same renderer (device hand-off/re-attach ordering).
    app.attach_live(LiveSession::offline_with_null_audio());
    assert!(app.live_strategy_pending());
    app.push_pending_compute_strategy(&mut renderer);
    assert_eq!(renderer.compute_strategy_apply_count(), 2);
}
