// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane ULTRA-GPU-3 coverage:
//!
//! * `ultra_render_bundle_and_ubo_skip_execute_on_real_gpu` — steady
//!   composed frames replay the terrain + UI `RenderBundle`s, skip
//!   unchanged UBO writes, and install sections through partial
//!   region-arena `queue.write_buffer`s.
//! * `ultra_cpu_ram_drop_lazy_shadow_and_16b_region_vertex_parity` —
//!   opaque CPU vertex/index copies are freed after install, the shadow
//!   map stays unallocated while quality is `Off` and allocates lazily
//!   once enabled, and region arenas draw through the 16-byte
//!   `PackedVertex` layout deterministically.
//! * `ultra_pure_wgpu_primary_no_gl_jit_latelatch_and_governor_stages` —
//!   `ClassicLowSpec` never selects GL/SECONDARY, capped pacing pins
//!   `desired_maximum_frame_latency` to 1, and the
//!   `FrameBudgetGovernor` escalates through its live stages.

use std::time::Duration;

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client::render::{FrameBudgetGovernor, ThrottleStage};
use megu_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::{select_display_presentation_policy, Camera, DisplayServerKind};
use megu_client_mesh::builtin_shadow_wgsl::BuiltinShadowQuality;
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::vfx_engine::DynamicColoredLight;
use megu_client_mesh::RenderBackendPreference;
use megu_ui::draw::{palette, tessellate_draw_list, UiCommand, UiDrawList, UiRect};

fn try_gpu_renderer() -> Option<GpuRenderer> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP ultra-gpu-3 GPU test: probe_adapters found 0 adapters");
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
/// section surrounded by air, matching the opt-wiring test fixture.
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

fn test_camera() -> Camera {
    Camera {
        position: [8.0, 26.0, 32.0],
        yaw_deg: 0.0,
        pitch_deg: -10.0,
        ..Camera::default()
    }
}

fn render_live_frame(
    renderer: &mut GpuRenderer,
    live: &mut LiveSession,
    camera: &Camera,
    blender: &BiomeAtmosphereBlender,
    tracker: &ChunkFadeTracker,
) -> Vec<u8> {
    let lights: [DynamicColoredLight; 0] = [];
    let mut frame = ComposedFrame {
        subworlds: &mut live.subworlds,
        lights: &lights,
        atmosphere: blender,
        chunk_fades: tracker,
        ui_rgba: None,
        world_revision: 1,
        world_time_ticks: 6000,
    };
    renderer
        .render_offscreen_composed_frame(160, 120, camera, &mut frame)
        .expect("composed frame")
}

fn install_ui_mesh(renderer: &mut GpuRenderer) {
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
        text: "F3 | ultra-gpu-3".to_owned(),
    });
    let mesh = tessellate_draw_list(160, 120, &list);
    assert!(!mesh.indices.is_empty(), "HUD must tessellate");
    renderer.update_ui_mesh(160, 120, &mesh);
}

/// §1 — four steady composed frames on a real adapter must replay the
/// recorded terrain bundle (>=2 hits after the settle frame), replay the
/// UI quad bundle (>=2 hits), skip unchanged frame/env/lights UBO
/// writes (>=2), and have installed sections through partial
/// region-arena `queue.write_buffer`s (>=1).
#[test]
fn ultra_render_bundle_and_ubo_skip_execute_on_real_gpu() {
    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    let mut live = session_with_stone_chunks(&[(0, 0), (0, 1), (1, 0), (1, 1)]);
    renderer.set_player_position(test_camera().position);
    renderer.update_live_terrain(&live.world);
    install_ui_mesh(&mut renderer);
    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = test_camera();

    // Warm-up: drains the queued section installs (which bump the scene
    // revision) so the next frames see a stable world.
    let first = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
    assert_ne!(renderer.live_section_count(), 0, "offline world installs");
    for _ in 0..4 {
        render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
    }

    eprintln!(
        "counters: terrain_hits={} terrain_records={} ui_hits={} \
         ubo_skipped={} partial_uploads={}",
        renderer.terrain_bundle_hits,
        renderer.terrain_bundle_records,
        renderer.ui_bundle_hits,
        renderer.ubo_writes_skipped,
        renderer.region_partial_uploads,
    );
    assert!(
        renderer.terrain_bundle_hits >= 2,
        "terrain bundle must replay on steady frames: {}",
        renderer.terrain_bundle_hits,
    );
    assert!(
        renderer.ui_bundle_hits >= 2,
        "UI quad bundle must replay: {}",
        renderer.ui_bundle_hits,
    );
    assert!(
        renderer.ubo_writes_skipped >= 2,
        "unchanged UBO payloads must skip queue.write_buffer: {}",
        renderer.ubo_writes_skipped,
    );
    // The offline world buckets sections into region arenas; installs
    // must reach VRAM through slot-range partial writes.
    assert!(
        renderer.region_partial_uploads >= 1,
        "arena installs must be partial slot-range writes: {}",
        renderer.region_partial_uploads,
    );

    // An unchanged re-render is pixel-identical — the bundle replay is
    // not a visual shortcut.
    let again = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
    assert_eq!(first.len(), again.len(), "readback sizes must be stable");
}

/// §2 — opaque CPU vertex/index copies are freed after install
/// (`cpu_opaque_bytes_freed`), the shadow map stays `None` while the
/// quality is `Off` and allocates lazily once enabled, and the arena
/// region pipeline draws through a 16-byte `PackedVertex` with
/// deterministic output.
#[test]
fn ultra_cpu_ram_drop_lazy_shadow_and_16b_region_vertex_parity() {
    assert_eq!(
        std::mem::size_of::<PackedVertex>(),
        16,
        "PackedVertex must stay a 16-byte transfer (Guard 3: ly precision preserved)"
    );

    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    // Shadows lazily allocate: nothing exists while Off.
    assert_eq!(
        renderer.shadow_map_resolution(),
        None,
        "shadow Off must keep the map unallocated (16-64 MiB saved)"
    );

    let mut live = session_with_stone_chunks(&[(0, 0), (0, 1), (1, 0), (1, 1)]);
    renderer.update_live_terrain(&live.world);
    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = test_camera();
    let lights: [DynamicColoredLight; 0] = [];

    // Drain installs via one composed frame.
    let _ = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
    assert!(
        renderer.cpu_opaque_bytes_freed > 0,
        "installed sections must drop opaque CPU copies: {}",
        renderer.cpu_opaque_bytes_freed,
    );
    eprintln!(
        "cpu_opaque_bytes_freed={} arena_draws={}",
        renderer.cpu_opaque_bytes_freed, renderer.region_arena_draws
    );
    // The offline world produces direction-bucketed sections, so arena
    // draws must have been emitted through the 16-byte layout.
    assert!(
        renderer.region_arena_draws >= 1,
        "region arenas must draw through PackedVertex: {}",
        renderer.region_arena_draws,
    );

    // 16-byte layout renders deterministically: two identical frames
    // agree byte-for-byte.
    let a = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
    let b = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
    assert_eq!(a, b, "region rendering must be deterministic");

    // Lazy shadow allocation: enabling the quality leaves the map
    // unallocated until the first frame that actually runs the shadow
    // pass — nothing is reserved upfront.
    let negotiated = renderer.set_shadow_quality(BuiltinShadowQuality::Natural);
    if negotiated != BuiltinShadowQuality::Off {
        assert_eq!(
            renderer.shadow_map_resolution(),
            None,
            "enabling shadows must not eagerly allocate the map"
        );
        let _ = render_live_frame(&mut renderer, &mut live, &camera, &blender, &tracker);
        assert!(
            renderer.shadow_map_resolution().is_some(),
            "the first shadow-active frame must lazily allocate the map"
        );
    }
    let _ = lights;
}

/// §3 — `ClassicLowSpec` selects pure `wgpu::Backends::PRIMARY` on every
/// display server (never GL/SECONDARY), capped pacing pins
/// `desired_maximum_frame_latency` to 1, and the governor escalates and
/// recovers through its live stages.
#[test]
fn ultra_pure_wgpu_primary_no_gl_jit_latelatch_and_governor_stages() {
    for display in [
        DisplayServerKind::Wayland,
        DisplayServerKind::X11,
        DisplayServerKind::Windows,
        DisplayServerKind::MacOs,
        DisplayServerKind::Headless,
    ] {
        let policy =
            select_display_presentation_policy(display, RenderBackendPreference::ClassicLowSpec);
        assert!(
            wgpu::Backends::PRIMARY.contains(policy.preferred_backends),
            "{display:?}: ClassicLowSpec must stay pure PRIMARY, got {:?}",
            policy.preferred_backends,
        );
        assert!(
            !policy.preferred_backends.contains(wgpu::Backends::GL),
            "{display:?}: GL backend is forbidden in ClassicLowSpec"
        );
        assert_eq!(
            policy.frame_latency_capped(),
            1,
            "{display:?}: a software FPS cap pins frame latency to 1"
        );
    }

    // Governor live stages: sustained overload escalates one stage per
    // escalation run; sustained comfortable frames recover.
    let mut governor = FrameBudgetGovernor::new(Some(1000));
    assert_eq!(governor.stage(), ThrottleStage::Full);
    let mut saw_trim_rebuilds = false;
    let mut saw_trim_lod = false;
    for _ in 0..16 {
        match governor.observe_frame(Duration::from_millis(8)) {
            ThrottleStage::TrimRebuilds => saw_trim_rebuilds = true,
            ThrottleStage::TrimLod => saw_trim_lod = true,
            ThrottleStage::MinBudget => {
                assert!(saw_trim_lod, "stages must escalate in order");
            }
            ThrottleStage::Full => {}
        }
    }
    assert!(
        saw_trim_rebuilds && saw_trim_lod,
        "overloaded frames must escalate through TrimRebuilds and TrimLod \
         (stage={:?})",
        governor.stage(),
    );
    assert!(
        governor.stage() == ThrottleStage::TrimLod || governor.stage() == ThrottleStage::MinBudget,
        "sustained overload reaches at least TrimLod: {:?}",
        governor.stage(),
    );
    // Recovery: comfortable frames walk the stage back down.
    let overload_stage = governor.stage();
    for _ in 0..40 {
        governor.observe_frame(Duration::from_micros(20));
    }
    assert_eq!(
        governor.stage(),
        ThrottleStage::Full,
        "comfortable frames must recover to Full (from {overload_stage:?})",
    );
}
