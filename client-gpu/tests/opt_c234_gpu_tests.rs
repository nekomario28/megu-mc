// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane OPT-C2+C3+C4 proofs:
//!
//! 1. `renderer::draw_light_mask` is bit-exact with client-port's
//!    `compute_region_light_mask` — the per-draw `params2.xy` mask the
//!    WGSL `draw_light_mask` walk ANDs in must agree with the port or
//!    fragments would skip lights the reference implementation kept.
//! 2. The shadow pass uploads ONE shared 256 B identity caster record
//!    (`shadow_caster_ubo_bytes == 256`) instead of N×256 B of identical
//!    records, while still emitting per-section casters
//!    (`shadow_caster_draws`).
//! 3. `update_live_terrain_tracked` reconciles a dirty set without a
//!    full `section_graphs` rebuild (`section_graph_partial_syncs`
//!    counts incremental inserts/removals) and `water_sections` stays
//!    in lockstep with the resident mesh scan.

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    draw_light_mask, probe_adapters, request_gpu_device, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::subworld_render::{GpuPointLightGpu, VfxLightsGpu};
use megu_client_gpu::Camera;
use megu_client_mesh::builtin_shadow_wgsl::{
    compute_region_light_mask, BuiltinShadowQuality, GpuPointLight,
};
use megu_client_mesh::RenderBackendPreference;

fn try_gpu() -> Option<megu_client_gpu::renderer::GpuDevice> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP opt-c234 GPU test: probe_adapters found 0 adapters");
        return None;
    }
    Some(
        request_gpu_device(RenderBackendPreference::Auto, None, None)
            .expect("probe found adapters but request_gpu_device failed"),
    )
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn next_f32(&mut self, lo: f32, hi: f32) -> f32 {
        let t = (self.next() & 0xFFFF_FFFF) as f32 / u32::MAX as f32;
        lo + (hi - lo) * t
    }
}

fn gpu_lights(positions: &[[f32; 4]], count: usize) -> VfxLightsGpu {
    let mut gpu = VfxLightsGpu {
        light_count: [count as u32, 0, 0, 0],
        lights: [GpuPointLightGpu::default(); 64],
    };
    for (dst, &pr) in gpu.lights.iter_mut().zip(positions.iter()) {
        dst.position_radius = pr;
        dst.color_intensity = [1.0, 1.0, 1.0, 1.0];
    }
    gpu
}

#[test]
fn draw_light_mask_matches_port_compute_region_light_mask() {
    let mut rng = XorShift(0xC234_00D5_A11C_E00F);
    for case in 0..64u32 {
        // Deterministic light field: up to 64 lights scattered in
        // ±256³, radii 0..40, plus deliberately degenerate entries.
        let n = (rng.next() % 65) as usize;
        let mut positions = Vec::with_capacity(n);
        for _ in 0..n {
            positions.push([
                rng.next_f32(-256.0, 256.0),
                rng.next_f32(-64.0, 320.0),
                rng.next_f32(-256.0, 256.0),
                rng.next_f32(0.0, 40.0),
            ]);
        }
        // Degenerate entries must fail closed (bit clear) in both.
        if n >= 2 && case % 3 == 0 {
            positions[0][3] = -1.0;
            positions[1][3] = f32::NAN;
        }
        let count = n.min(64);
        let gpu = gpu_lights(&positions[..count], count);
        let port: Vec<GpuPointLight> = positions[..count]
            .iter()
            .map(|&pr| GpuPointLight {
                position_radius: pr,
                color_intensity: [1.0; 4],
            })
            .collect();
        // AABB sweep: tight 16³ sections, region spans, degenerate
        // (inverted) bounds — the clamp(min,max) order must not matter.
        let min = [
            rng.next_f32(-128.0, 128.0),
            rng.next_f32(-64.0, 128.0),
            rng.next_f32(-128.0, 128.0),
        ];
        let max = [
            min[0] + rng.next_f32(1.0, 96.0),
            min[1] + rng.next_f32(1.0, 96.0),
            min[2] + rng.next_f32(1.0, 96.0),
        ];
        for (a, b) in [(min, max), (max, min)] {
            let ours = draw_light_mask(a, b, &gpu);
            let theirs = compute_region_light_mask(a, b, &port);
            assert_eq!(
                ours, theirs,
                "case {case}: draw_light_mask {ours:#x} != port {theirs:#x} (n={count})"
            );
        }
    }
}

/// Protocol 767 `level_chunk_with_light` body — one solid-stone section
/// per chunk, matching the live-session fixture the other GPU tests use.
fn chunk_body(cz: i32) -> Vec<u8> {
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
    let mut sections = Vec::new();
    for section in 0..24 {
        let filled = section == 4;
        sections.extend_from_slice(&(if filled { 4096i16 } else { 0 }).to_be_bytes());
        sections.push(0);
        varint(&mut sections, if filled { 1 } else { 0 });
        varint(&mut sections, 0);
        sections.push(0);
        varint(&mut sections, 0);
        varint(&mut sections, 0);
    }
    let mut body = Vec::new();
    body.extend_from_slice(&0i32.to_be_bytes());
    body.extend_from_slice(&cz.to_be_bytes());
    body.extend_from_slice(&[10, 0]);
    varint(&mut body, sections.len() as u32);
    body.extend_from_slice(&sections);
    for _ in 0..7 {
        varint(&mut body, 0);
    }
    body
}

fn ingest_chunk(live: &mut LiveSession, cz: i32) {
    let chunk = decode_chunk_packet(&chunk_body(cz), 24, 64).expect("decoded chunk");
    live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
        .unwrap();
}

fn rebuild_meshes(live: &mut LiveSession) {
    for _ in 0..16 {
        if live.world.pending_mesh_sections() == 0 {
            break;
        }
        live.world
            .rebuild_dirty_bounded(256, [8.0, 20.0, 24.0])
            .expect("mesh rebuild");
    }
}

#[test]
fn shadow_caster_ubo_is_one_shared_identity_record() {
    let Some(ctx) = try_gpu() else { return };
    let mut live = LiveSession::offline_with_null_audio();
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    renderer.set_shadow_quality(BuiltinShadowQuality::Low);

    for cz in [-2, -1, 0, 2] {
        ingest_chunk(&mut live, cz);
    }
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
    rebuild_meshes(&mut live);
    renderer.update_live_terrain(&live.world);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    for _ in 0..3 {
        renderer
            .render_offscreen_rgba(64, 64, &camera, None)
            .expect("offscreen frame drains uploads and runs the shadow pass");
    }
    assert_eq!(renderer.live_section_count(), 4);
    assert_eq!(renderer.water_sections, 0, "stone-only world has no water");

    assert_eq!(
        renderer.shadow_caster_ubo_bytes, 256,
        "all identity terrain casters must share one 256 B record \
         (was N×256 B before this change)"
    );
    assert!(
        renderer.shadow_caster_draws > 0,
        "the shadow pass still emits caster draws for live sections"
    );
}

#[test]
fn tracked_sync_updates_graphs_incrementally_without_full_rebuild() {
    let Some(ctx) = try_gpu() else { return };
    let mut live = LiveSession::offline_with_null_audio();
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);

    for cz in [-2, -1, 0, 2] {
        ingest_chunk(&mut live, cz);
    }
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
    rebuild_meshes(&mut live);
    renderer.update_live_terrain(&live.world);
    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    renderer
        .render_offscreen_rgba(64, 64, &camera, None)
        .expect("offscreen frame drains uploads");
    assert_eq!(renderer.live_section_count(), 4);
    let full_syncs = renderer.section_graph_partial_syncs;

    // A second chunk event in the same column marks a dirty set; the
    // tracked reconcile must touch only those coords.
    ingest_chunk(&mut live, 0);
    rebuild_meshes(&mut live);
    let dirty = std::mem::take(&mut live.dirty_terrain_coords);
    assert!(!dirty.is_empty(), "re-ingested chunk must mark dirty coords");
    renderer.update_live_terrain_tracked(&live.world, &dirty, false);
    assert!(
        renderer.section_graph_partial_syncs > full_syncs,
        "tracked sync must run the incremental graph path"
    );
    assert_eq!(renderer.live_section_count(), 4);
    // Re-inserting the dirty coord folds the displaced section out of
    // `water_sections` before crediting the new one — the stone-only
    // world must stay at exactly zero water sections after the churn.
    assert_eq!(
        renderer.water_sections, 0,
        "cached water counter drifted across the incremental re-insert"
    );
}
