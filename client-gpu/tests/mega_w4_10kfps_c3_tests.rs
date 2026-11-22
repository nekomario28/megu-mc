// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane MEGA-W4-GPU-10KFPS-C3 coverage on real headless `wgpu`:
//!
//! * §4 F1 — GPU-quad UI overlay frames driven by
//!   `tessellate_draw_list` + `UI_QUAD_WGSL` with zero CPU raster uploads.
//! * §4 F3 — the zero-alloc `cull_live_sections` reuse cache hits on
//!   consecutive identical views.
//! * §4 F4 — `LiveSession::hud_revision` skips the `HudState` clone until
//!   a server packet or local edit actually mutates HUD state.
//! * §4 F5 — `--vsync off` selects `PresentMode::Immediate` first and
//!   skips `pre_present_notify`; shadow casters reduce to a `u64`
//!   FNV-1a digest that changes iff casters change.
//! * §2 C3 — 2,000 kinetic parts resolved into one `InstancedPartBatch`
//!   draw plus a 6-DOF contraption `SubWorldRenderEntry` whose pixels
//!   move cleanly on pose-only updates.

use mc_spine_client::protocol::{ClientPlayEvent, PlayEvent};
use mc_spine_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use mc_spine_client_gpu::live_session::{KineticPartSpec, LiveSession};
use mc_spine_client_gpu::renderer::{
    probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer, OFFSCREEN_FORMAT,
};
use mc_spine_client_gpu::subworld_render::{box_mesh, quat_from_axis_angle, SubWorldTransform};
use mc_spine_client_gpu::{select_present_mode, should_call_pre_present_notify, Camera};
use mc_spine_client_mesh::vfx_engine::DynamicColoredLight;
use mc_spine_client_mesh::RenderBackendPreference;
use mc_spine_ui::draw::palette;
use mc_spine_ui::draw::{tessellate_draw_list, UiCommand, UiDrawList, UiRect};
use mc_spine_ui::state::HudState;

/// A headless renderer, or `None` when the host has no GPU adapter (the
/// same graceful skip every client-gpu GPU test uses).
fn try_gpu_renderer() -> Option<GpuRenderer> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP mega-w4 GPU test: probe_adapters found 0 adapters");
        return None;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("probe found adapters but request_gpu_device failed");
    Some(GpuRenderer::new(ctx, OFFSCREEN_FORMAT))
}

fn test_camera() -> Camera {
    Camera {
        position: [8.0, 26.0, 32.0],
        yaw_deg: 0.0,
        pitch_deg: -10.0,
        ..Camera::default()
    }
}

/// One composed frame over `live.subworlds` with no CPU UI framebuffer —
/// the GPU-quad overlay state installed via `update_ui_mesh` supplies
/// any UI pixels.
fn render_live_frame(
    renderer: &mut GpuRenderer,
    live: &mut LiveSession,
    camera: &Camera,
    blender: &BiomeAtmosphereBlender,
    tracker: &ChunkFadeTracker,
    world_revision: u64,
) -> Vec<u8> {
    let lights: [DynamicColoredLight; 0] = [];
    let mut frame = ComposedFrame {
        subworlds: &mut live.subworlds,
        lights: &lights,
        atmosphere: blender,
        chunk_fades: tracker,
        ui_rgba: None,
        world_revision,
        world_time_ticks: 6000,
    };
    renderer
        .render_offscreen_composed_frame(160, 120, camera, &mut frame)
        .expect("composed frame")
}

fn count_diff_pixels(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
        .filter(|(pa, pb)| {
            (0..3)
                .map(|c| (pa[c] as i32 - pb[c] as i32).abs())
                .sum::<i32>()
                > 12
        })
        .count()
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

/// §4 F1+F3+F4+F5 — consecutive real-wgpu frames with F3 text that
/// changes every frame draw the UI through the GPU quad pipeline while
/// the CPU raster path stays cold; the cull reuse cache and HUD
/// revision gate both fire; the `u64` shadow digest tracks caster state.
#[test]
fn f1_f3_f4_f5_gpu_ui_quads_cull_cache_hud_rev_and_shadow_u64() {
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    let mut live = LiveSession::offline_with_null_audio();
    renderer.update_live_terrain(&live.world);
    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = test_camera();

    // ---- F4: hud_revision gates the HudState clone -------------------
    let mut hud_mirror = HudState::default();
    assert!(
        live.sync_hud_to(&mut hud_mirror),
        "first sync must clone the HUD"
    );
    assert!(
        !live.sync_hud_to(&mut hud_mirror),
        "unchanged revision must skip the clone"
    );
    assert!(
        live.hud_sync_skips >= 1,
        "hud_sync_skips must count the gate hit"
    );
    // A real server packet mutates HUD state -> revision bumps -> clone.
    live.apply_event(ClientPlayEvent::Play(PlayEvent::SystemChat {
        text: "server ready".to_owned(),
        overlay: false,
    }))
    .unwrap();
    assert!(
        live.sync_hud_to(&mut hud_mirror),
        "mutated HUD must re-sync"
    );
    assert_eq!(hud_mirror.chat_lines.last().unwrap(), "server ready");
    // Idle again -> the gate re-arms.
    assert!(!live.sync_hud_to(&mut hud_mirror));
    assert!(live.hud_sync_skips >= 2);

    // ---- F1 + F3: consecutive GPU-quad UI frames + cull cache --------
    let digest_empty = renderer.shadow_caster_digest();
    let mut frame_ms = Vec::new();
    for frame_idx in 0..4u32 {
        // F3-style diagnostics text changes every frame (frame_ms moves),
        // so each frame produces a fresh UiDrawList -> fresh quad mesh.
        let mut list = UiDrawList::default();
        list.commands.push(UiCommand::FillRect {
            rect: UiRect::new(4, 4, 150, 30),
            color: palette::PANEL,
        });
        list.commands.push(UiCommand::Text {
            x: 8,
            y: 8,
            scale: 2,
            color: palette::TEXT,
            text: format!(
                "F3 | frame {frame_idx} | {0:.3} ms",
                frame_idx as f32 * 0.317
            ),
        });
        let mesh = tessellate_draw_list(160, 120, &list);
        assert!(!mesh.indices.is_empty(), "F3 HUD must tessellate");
        renderer.update_ui_mesh(160, 120, &mesh);
        let t0 = std::time::Instant::now();
        let pixels = render_live_frame(
            &mut renderer,
            &mut live,
            &camera,
            &blender,
            &tracker,
            frame_idx as u64,
        );
        frame_ms.push(t0.elapsed().as_secs_f64() * 1_000.0);
        if frame_idx == 0 {
            assert_nonblank_frame(&pixels, "GPU-quad composed frame");
        }
    }
    eprintln!("mega-w4: composed-frame ms = {frame_ms:?} (includes readback)");
    assert!(
        renderer.ui_gpu_quad_frames >= 2,
        "GPU quad UI path must draw on consecutive changing-F3 frames, got {}",
        renderer.ui_gpu_quad_frames
    );
    assert_eq!(
        renderer.ui_cpu_raster_uploads, 0,
        "the live UI path must never rasterize a w*h*4 framebuffer"
    );
    assert!(
        renderer.cull_cache_hits >= 1,
        "identical camera + world revision must hit the cull reuse cache, got {}",
        renderer.cull_cache_hits
    );

    // ---- F5: u64 shadow caster digest changes iff casters change -----
    // Empty bank so far -> digest is the empty sentinel across frames.
    assert_eq!(
        renderer.shadow_caster_digest(),
        digest_empty,
        "unchanged casters must hold the digest"
    );
    let (verts, inds) = box_mesh([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0], [0.8, 0.3, 0.2]);
    live.upsert_live_contraption_subworld(
        7,
        SubWorldTransform {
            position: [8.0, 22.0, 16.0],
            ..SubWorldTransform::IDENTITY
        },
        verts,
        inds,
    );
    render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 10);
    let digest_one = renderer.shadow_caster_digest();
    assert_ne!(digest_one, digest_empty, "adding a caster must change it");
    // Same casters, another frame -> identical digest.
    render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 10);
    assert_eq!(
        renderer.shadow_caster_digest(),
        digest_one,
        "unchanged casters must hold the digest across frames"
    );
    // Transform-only move is a caster change for shadow purposes.
    assert!(live.update_contraption_pose(
        7,
        SubWorldTransform {
            position: [10.0, 23.0, 15.0],
            quaternion: quat_from_axis_angle([0.0, 1.0, 0.0], 0.5),
            scale: 1.0,
        },
    ));
    render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 10);
    let digest_moved = renderer.shadow_caster_digest();
    assert_ne!(
        digest_moved, digest_one,
        "a caster transform change must change the digest"
    );
    // Removing the last caster returns to the empty digest.
    assert!(live.subworlds.remove(7));
    render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 10);
    assert_eq!(
        renderer.shadow_caster_digest(),
        digest_empty,
        "empty caster set must hash to the empty sentinel"
    );
}

/// §4 F5 — presentation policy: `--vsync off` picks `Immediate` when the
/// surface offers it and never calls `pre_present_notify`; vsync-on
/// behavior is unchanged.
#[test]
fn f5_vsync_off_selects_immediate_and_skips_pre_present_notify() {
    let supported = [
        wgpu::PresentMode::Fifo,
        wgpu::PresentMode::Mailbox,
        wgpu::PresentMode::Immediate,
    ];
    assert_eq!(
        select_present_mode(false, &supported),
        wgpu::PresentMode::Immediate,
        "vsync off must prefer Immediate over Mailbox"
    );
    // Fallback ladder without Immediate: Mailbox, then AutoNoVsync.
    assert_eq!(
        select_present_mode(
            false,
            &[wgpu::PresentMode::Fifo, wgpu::PresentMode::Mailbox]
        ),
        wgpu::PresentMode::Mailbox
    );
    assert_eq!(
        select_present_mode(false, &[wgpu::PresentMode::Fifo]),
        wgpu::PresentMode::Fifo,
        "Fifo-only surface must not synthesize an unsupported mode"
    );
    assert_eq!(
        select_present_mode(false, &[]),
        wgpu::PresentMode::AutoNoVsync
    );
    // VSync-on behavior preserved: Fifo first, AutoVsync fallback.
    assert_eq!(
        select_present_mode(true, &supported),
        wgpu::PresentMode::Fifo
    );
    assert_eq!(select_present_mode(true, &[]), wgpu::PresentMode::AutoVsync);
    // The Wayland frame-callback notify is a vsync-only pacing hint.
    assert!(!should_call_pre_present_notify(false));
    assert!(should_call_pre_present_notify(true));
}

/// §2 C3 — 2,000 registered kinetic parts resolve into a single
/// `InstancedPartBatch` (one GPU draw, phase advanced analytically), and
/// a 6-DOF contraption sub-world renders non-empty pixels that move on
/// pose-only updates.
#[test]
fn c3_2000_kinetic_gears_render_in_single_instanced_batch_and_contraption_subworld_renders() {
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    let mut live = LiveSession::offline_with_null_audio();
    renderer.update_live_terrain(&live.world);
    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = test_camera();

    // One shared shaft template -> all 2,000 parts instance in one batch.
    const SHAFT_KIND: u32 = 61;
    let (shaft_verts, shaft_inds) =
        box_mesh([-0.5, -0.12, -0.12], [0.5, 0.12, 0.12], [0.55, 0.55, 0.6]);
    live.register_kinetic_template(SHAFT_KIND, shaft_verts, shaft_inds);
    for i in 0..2000u32 {
        live.register_kinetic_part(KineticPartSpec {
            mesh_kind: SHAFT_KIND,
            // A 50x40 grid under the camera so the draw also covers pixels.
            position: [
                (i % 50) as f32 * 0.45 - 3.0,
                19.0,
                (i / 50) as f32 * 0.45 + 10.0,
            ],
            axis: [0.0, 1.0, 0.0],
            ratio: if i % 2 == 0 { 1.0 } else { -0.5 },
            phase0: i as f32 * 0.013,
            scale: 1.0,
            packed_light: 0xff,
            tint_rgba: 0xffff_ffff,
            subworld_id: 0,
        });
    }
    assert_eq!(live.kinetic_part_count(), 2000);
    live.sync_kinetic_network_rpm(64.0, 1.0 / 60.0);
    let batch = live
        .subworlds
        .batches()
        .find(|b| b.mesh_kind == SHAFT_KIND)
        .expect("kinetic template must create one instanced batch");
    assert_eq!(batch.instances.len(), 2000);
    let phase_t0 = batch.instances[0].rot_uv_frame[1];

    // Analytic phase advance: dt advances theta without CPU vertex work.
    live.sync_kinetic_network_rpm(64.0, 0.5);
    let phase_t1 = live
        .subworlds
        .batches()
        .find(|b| b.mesh_kind == SHAFT_KIND)
        .unwrap()
        .instances[0]
        .rot_uv_frame[1];
    assert_ne!(
        phase_t0, phase_t1,
        "sync_kinetic_network_rpm must advance instance rotation"
    );

    // One 6-DOF contraption sub-world in front of the camera.
    let (hull_verts, hull_inds) = box_mesh([-1.5, -1.0, -1.5], [1.5, 1.0, 1.5], [0.8, 0.4, 0.2]);
    live.upsert_live_contraption_subworld(
        42,
        SubWorldTransform {
            position: [8.0, 22.0, 16.0],
            quaternion: quat_from_axis_angle([0.0, 1.0, 0.0], 0.0),
            scale: 1.0,
        },
        hull_verts.clone(),
        hull_inds.clone(),
    );

    let frame_a = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 20);
    assert_nonblank_frame(&frame_a, "kinetic + contraption frame A");
    assert!(
        renderer.instanced_kinetic_draws >= 1,
        "the 2000-part batch must draw"
    );
    assert!(
        renderer.instanced_kinetic_draws <= 4,
        "2,000 kinetic parts must batch into <=4 instanced draws, got {}",
        renderer.instanced_kinetic_draws
    );

    // Pose-only move: rotate + translate, no geometry re-upload.
    assert!(live.update_contraption_pose(
        42,
        SubWorldTransform {
            position: [11.0, 22.0, 14.0],
            quaternion: quat_from_axis_angle([0.0, 1.0, 0.0], 0.8),
            scale: 1.0,
        },
    ));
    let frame_b = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 20);
    let moved = count_diff_pixels(&frame_a, &frame_b);
    assert!(
        moved >= 32,
        "contraption pose update must move pixels cleanly (diff={moved})"
    );

    // Removing the contraption removes its pixels — proving they were
    // non-empty rather than ambient background noise.
    assert!(live.subworlds.remove(42));
    let frame_c = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker, 20);
    let contraption_pixels = count_diff_pixels(&frame_a, &frame_c);
    assert!(
        contraption_pixels >= 32,
        "contraption must cover non-empty pixels (with-vs-without diff={contraption_pixels})"
    );
}
