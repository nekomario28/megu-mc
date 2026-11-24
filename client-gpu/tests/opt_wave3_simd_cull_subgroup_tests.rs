// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane OPT-WAVE3 proofs:
//!
//! 1. `GpuRenderer::cull_live_sections` now feeds retained SoA bounds to
//!    `megu_core::simd_voxel::batch_aabb_frustum_cull_soa` — every SIMD
//!    verdict bit must equal the old scalar `section_frustum_visible`
//!    result across cameras, angles and section counts (including
//!    non-multiple-of-16 batches and add/remove residency changes).
//! 2. `external_sections` rides a revision-gated delta stream instead of
//!    a per-frame full-map `clone_from`: steady frames drain nothing,
//!    touched coords deliver latest-write-wins, clears wipe the mirror.
//! 3. The capability-gated subgroup section-cull WGSL validates under
//!    Naga `Capabilities::SUBGROUP`, is *rejected* without it, and the
//!    `GpuHizCuller` pipeline selection follows `Features::SUBGROUP`
//!    exactly — baseline everywhere else.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use megu_client_gpu::gpu_hiz::{
    section_cull_subgroup_wgsl, section_cull_wgsl, GpuHizCuller, SECTION_CULL_WGSL,
};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    apply_external_sections_delta, frustum_planes_from_view_proj, probe_adapters,
    request_gpu_device, section_frustum_visible, translucent_order_digest,
    ExternalSectionMesh, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::Camera;
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::{RenderBackendPreference, SectionCoord, EDGE};
use megu_core::simd_voxel::{
    batch_aabb_frustum_cull_block_scalar, batch_aabb_frustum_cull_soa,
    batch_aabb_frustum_cull_soa_scalar,
};
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};

/// A headless device, or `None` on hosts without a usable adapter (the
/// same graceful skip the other GPU-gated suites use).
fn try_gpu() -> Option<megu_client_gpu::renderer::GpuDevice> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP opt-wave3 GPU test: probe_adapters found 0 adapters");
        return None;
    }
    Some(
        request_gpu_device(RenderBackendPreference::Auto, None, None)
            .expect("probe found adapters but request_gpu_device failed"),
    )
}

fn validate_wgsl_with_caps(name: &str, source: &str, caps: Capabilities) -> Result<(), String> {
    let module = wgsl::parse_str(source).map_err(|e| format!("{name} parse: {e:?}"))?;
    let mut validator = Validator::new(ValidationFlags::all(), caps);
    validator
        .validate(&module)
        .map(|_| ())
        .map_err(|e| format!("{name} validate: {e:?}"))
}

// ---------------------------------------------------------------------------
// 1. SoA SIMD frustum cull parity
// ---------------------------------------------------------------------------

/// Deterministic 32-bit xorshift for repeatable section sets.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// `count` section coords clustered around the origin with a few far
/// outliers — negative axes included so both frustum halves are probed.
fn section_coords(count: usize, seed: u64) -> Vec<SectionCoord> {
    let mut rng = XorShift(seed | 0x9E37_79B9);
    let mut coords = Vec::with_capacity(count);
    for i in 0..count {
        if i % 9 == 8 {
            // Sparse far field: most of these sit well outside any frustum.
            coords.push(SectionCoord(
                (rng.next() % 400) as i32 - 200,
                (rng.next() % 32) as i32 - 12,
                (rng.next() % 400) as i32 - 200,
            ));
        } else {
            coords.push(SectionCoord(
                (rng.next() % 24) as i32 - 12,
                (rng.next() % 12) as i32 - 2,
                (rng.next() % 24) as i32 - 12,
            ));
        }
    }
    coords
}

/// Camera variety: yaw sweeps every octant, pitch ranges horizon→down,
/// fov/aspect shift the plane orientation and normalization.
fn test_cameras() -> Vec<Camera> {
    let mut cameras = Vec::new();
    for (i, yaw) in [0.0f32, 45.0, 90.0, 135.0, 210.0, 300.0].iter().enumerate() {
        cameras.push(Camera {
            position: [8.0, 20.0, 8.0],
            yaw_deg: *yaw,
            pitch_deg: [-60.0, -20.0, 0.0, 30.0][i % 4],
            fov_deg: [55.0, 70.0, 90.0, 110.0][i % 4],
            roll_rad: 0.0,
            near: 0.1,
            far: 512.0,
        });
    }
    cameras.push(Camera {
        position: [-96.0, 40.0, 240.0],
        yaw_deg: 17.0,
        pitch_deg: -35.0,
        fov_deg: 80.0,
        roll_rad: 0.0,
        near: 0.5,
        far: 1024.0,
    });
    cameras
}

/// Pack `coords` into the same six-slice SoA layout `CullSectionSoa`
/// builds on the renderer (EDGE-sized AABBs from the coord min corner).
fn pack_soa(coords: &[SectionCoord]) -> [Vec<f32>; 6] {
    let e = EDGE as f32;
    let mut soa = [
        Vec::with_capacity(coords.len()),
        Vec::with_capacity(coords.len()),
        Vec::with_capacity(coords.len()),
        Vec::with_capacity(coords.len()),
        Vec::with_capacity(coords.len()),
        Vec::with_capacity(coords.len()),
    ];
    for c in coords {
        let (mx, my, mz) = (c.0 as f32 * e, c.1 as f32 * e, c.2 as f32 * e);
        soa[0].push(mx);
        soa[1].push(my);
        soa[2].push(mz);
        soa[3].push(mx + e);
        soa[4].push(my + e);
        soa[5].push(mz + e);
    }
    soa
}

/// `true` iff bit `i` of the packed words is set — exactly the read the
/// renderer's `trailing_zeros` walks perform.
fn bit(words: &[u64], i: usize) -> bool {
    words[i / 64] & (1_u64 << (i % 64)) != 0
}

#[test]
fn soa_simd_cull_matches_scalar_oracle_across_cameras_angles_and_counts() {
    // Non-multiple-of-16 counts exercise every SIMD tail; >64 spans
    // multiple output words.
    let counts = [1usize, 5, 15, 16, 17, 63, 64, 65, 129, 197];
    for (cam_i, camera) in test_cameras().iter().enumerate() {
        let view_proj = camera.view_projection(1.0);
        let planes = frustum_planes_from_view_proj(&view_proj);
        for &count in &counts {
            let coords = section_coords(count, (cam_i as u64) << 32 | count as u64);
            let [min_x, min_y, min_z, max_x, max_y, max_z] = pack_soa(&coords);
            let words = count.div_ceil(64);

            let mut simd = vec![0u64; words];
            batch_aabb_frustum_cull_soa(
                &min_x, &min_y, &min_z, &max_x, &max_y, &max_z, &planes, &mut simd,
            );
            let mut oracle = vec![0u64; words];
            batch_aabb_frustum_cull_soa_scalar(
                &min_x, &min_y, &min_z, &max_x, &max_y, &max_z, &planes, &mut oracle,
            );
            assert_eq!(
                simd, oracle,
                "dispatched SIMD vs scalar oracle diverged (camera {cam_i}, n={count})"
            );

            for (i, coord) in coords.iter().enumerate() {
                let min = [
                    coord.0 as f32 * EDGE as f32,
                    coord.1 as f32 * EDGE as f32,
                    coord.2 as f32 * EDGE as f32,
                ];
                let scalar = section_frustum_visible(min, &planes);
                assert_eq!(
                    bit(&simd, i),
                    scalar,
                    "bit {i} ({coord:?}) disagrees with section_frustum_visible \
                     (camera {cam_i}, n={count})"
                );
            }
            // Per-word scalar block reference agrees too.
            for (w, &word) in simd.iter().enumerate() {
                let start = w * 64;
                let len = (count - start).min(64);
                let block = batch_aabb_frustum_cull_block_scalar(
                    &min_x[start..start + len],
                    &min_y[start..start + len],
                    &min_z[start..start + len],
                    &max_x[start..start + len],
                    &max_y[start..start + len],
                    &max_z[start..start + len],
                    &planes,
                );
                assert_eq!(word, block, "block oracle diverged at word {w}");
            }
        }
    }
}

#[test]
fn soa_simd_cull_verdicts_stable_across_add_remove_and_reorder() {
    // Residency churn: the per-coord verdict must not depend on which
    // set the coord rode in with, or where in the SoA row it landed.
    let camera = Camera {
        position: [8.0, 20.0, 8.0],
        yaw_deg: 30.0,
        pitch_deg: -15.0,
        fov_deg: 75.0,
        roll_rad: 0.0,
        near: 0.1,
        far: 512.0,
    };
    let planes = frustum_planes_from_view_proj(&camera.view_projection(1.0));
    let full = section_coords(150, 0xC0FFEE);
    let [min_x, min_y, min_z, max_x, max_y, max_z] = pack_soa(&full);
    let mut full_bits = vec![0u64; full.len().div_ceil(64)];
    batch_aabb_frustum_cull_soa(
        &min_x, &min_y, &min_z, &max_x, &max_y, &max_z, &planes, &mut full_bits,
    );

    // "Removal": keep every third coord out, like an unload sweep.
    let kept: Vec<SectionCoord> = full
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 3 != 1)
        .map(|(_, c)| *c)
        .collect();
    let [kx, ky, kz, kx2, ky2, kz2] = pack_soa(&kept);
    let mut kept_bits = vec![0u64; kept.len().div_ceil(64)];
    batch_aabb_frustum_cull_soa(
        &kx, &ky, &kz, &kx2, &ky2, &kz2, &planes, &mut kept_bits,
    );
    for (i, coord) in kept.iter().enumerate() {
        let source_idx = full.iter().position(|c| c == coord).expect("kept ⊆ full");
        assert_eq!(
            bit(&kept_bits, i),
            bit(&full_bits, source_idx),
            "verdict for {coord:?} changed when neighbours were removed"
        );
    }

    // "Reorder/add": reversed order must produce the same verdict set.
    let mut reversed = full.clone();
    reversed.reverse();
    let [rx, ry, rz, rx2, ry2, rz2] = pack_soa(&reversed);
    let mut rev_bits = vec![0u64; reversed.len().div_ceil(64)];
    batch_aabb_frustum_cull_soa(
        &rx, &ry, &rz, &rx2, &ry2, &rz2, &planes, &mut rev_bits,
    );
    for (i, coord) in reversed.iter().enumerate() {
        let source_idx = full.len() - 1 - i;
        assert_eq!(
            bit(&rev_bits, i),
            bit(&full_bits, source_idx),
            "verdict for {coord:?} depends on SoA row index"
        );
    }
}

/// Renderer-level wiring proof on a real adapter: the resident section
/// set produced by a live session culls to exactly what a scalar replay
/// (subpixel → graph/frustum, no Hi-Z pyramid installed) predicts, and
/// the SoA resync stays revision-gated across camera-only frames.
#[test]
fn renderer_soa_cull_matches_scalar_replay_and_resync_is_revision_gated() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    let mut live = LiveSession::offline_with_null_audio();
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);

    // Four stone sections along -Z like the zerobloat fixture: ingest
    // real chunks so `live_sections` + `section_graphs` mirror a world.
    for cz in [-2, -1, 0, 2] {
        let chunk = megu_client::protocol::decode_chunk_packet(&chunk_body(cz), 24, 64)
            .expect("decoded chunk");
        live.apply_event(megu_client::protocol::ClientPlayEvent::Play(
            megu_client::protocol::PlayEvent::ChunkData(Box::new(chunk)),
        ))
        .unwrap();
    }
    live.apply_event(megu_client::protocol::ClientPlayEvent::Play(
        megu_client::protocol::PlayEvent::Teleport(
            megu_client::protocol::TeleportSync {
                x: 8.0,
                y: 20.0,
                z: 24.0,
                yaw: 0.0,
                pitch: -20.0,
                flags: 0,
                teleport_id: 1,
            },
        ),
    ))
    .unwrap();
    for _ in 0..16 {
        if live.world.pending_mesh_sections() == 0 {
            break;
        }
        live.world
            .rebuild_dirty_bounded(256, [8.0, 20.0, 24.0])
            .expect("mesh rebuild");
    }
    renderer.update_live_terrain(&live.world);

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    renderer
        .render_offscreen_rgba(64, 64, &camera, None)
        .expect("offscreen frame drains uploads and runs cull_live_sections");
    assert_eq!(renderer.live_section_count(), 4);
    assert_eq!(
        renderer.cull_soa_resyncs, 1,
        "first miss packs the SoA arrays exactly once"
    );

    // Scalar replay of the renderer path: subpixel reject first, then the
    // graph verdict for graph-covered coords and `section_frustum_visible`
    // for the rest (no Hi-Z pyramid installed in this fixture).
    let view_proj = camera.view_projection(1.0);
    let planes = frustum_planes_from_view_proj(&view_proj);
    let focal_y_px = 64.0 * 0.5 / (camera.fov_deg.to_radians() * 0.5).tan();
    let camera_section = SectionCoord(
        (camera.position[0] / EDGE as f32).floor() as i32,
        (camera.position[1] / EDGE as f32).floor() as i32,
        (camera.position[2] / EDGE as f32).floor() as i32,
    );
    let graphs: std::collections::HashMap<SectionCoord, _> =
        live.world.section_graphs().collect();
    let graph_visible: std::collections::HashSet<SectionCoord> =
        megu_client_mesh::GraphCuller::new(camera_section, &planes)
            .search(graphs.iter().map(|(&c, &g)| (c, g)))
            .visible
            .into_iter()
            .collect();
    let mut expected_culled = std::collections::HashSet::new();
    for coord in live.world.packed_meshes().map(|(c, _)| c) {
        let min = [
            coord.0 as f32 * EDGE as f32,
            coord.1 as f32 * EDGE as f32,
            coord.2 as f32 * EDGE as f32,
        ];
        if megu_client_gpu::renderer::is_subpixel_section(
            camera.position,
            min,
            focal_y_px,
        ) {
            expected_culled.insert(coord);
            continue;
        }
        let visible = if graphs.contains_key(&coord) {
            graph_visible.contains(&coord)
        } else {
            section_frustum_visible(min, &planes)
        };
        if !visible {
            expected_culled.insert(coord);
        }
    }
    assert_eq!(
        renderer.culled_sections(),
        &expected_culled,
        "SIMD cull result diverges from the scalar replay"
    );

    // Camera-only frames reuse the retained SoA (no resync). The first
    // repeat frame may still be a cull miss — the worker Hi-Z pyramid
    // built from frame one's depth installs asynchronously and bumps
    // `hiz_revision`, which is part of the cache key. Once it settles,
    // an unchanged frame is a pure cache hit.
    for _ in 0..4 {
        renderer
            .render_offscreen_rgba(64, 64, &camera, None)
            .expect("steady frame");
    }
    assert!(
        renderer.cull_cache_hits >= 1,
        "a settled identical frame must hit the cull cache"
    );
    assert_eq!(
        renderer.cull_soa_resyncs, 1,
        "camera-only frames never repack the SoA arrays"
    );

    let tilted = Camera {
        yaw_deg: 90.0,
        pitch_deg: 10.0,
        ..camera
    };
    renderer
        .render_offscreen_rgba(64, 64, &tilted, None)
        .expect("camera-only change");
    assert_eq!(
        renderer.cull_soa_resyncs, 1,
        "camera-only frame must not repack the SoA arrays"
    );

    // A residency change (new chunk column) bumps cull_world_revision →
    // the next frame repacks once.
    let chunk = megu_client::protocol::decode_chunk_packet(&chunk_body(4), 24, 64)
        .expect("decoded chunk");
    live.apply_event(megu_client::protocol::ClientPlayEvent::Play(
        megu_client::protocol::PlayEvent::ChunkData(Box::new(chunk)),
    ))
    .unwrap();
    for _ in 0..16 {
        if live.world.pending_mesh_sections() == 0 {
            break;
        }
        live.world
            .rebuild_dirty_bounded(256, [8.0, 20.0, 24.0])
            .expect("mesh rebuild");
    }
    renderer.update_live_terrain(&live.world);
    renderer
        .render_offscreen_rgba(64, 64, &camera, None)
        .expect("post-add frame");
    assert_eq!(
        renderer.cull_soa_resyncs, 2,
        "section add must trigger exactly one SoA resync"
    );
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

// ---------------------------------------------------------------------------
// 2. Zero-clone external_sections delta delivery
// ---------------------------------------------------------------------------

fn ext_mesh(tag: u32) -> ExternalSectionMesh {
    let packed: Arc<[PackedVertex]> = Arc::from(vec![PackedVertex {
        position: tag,
        ..PackedVertex::default()
    }]);
    let order: Arc<[u32]> = Arc::from(vec![0u32]);
    ExternalSectionMesh {
        packed_quads: packed.len() as u32 / 4,
        order_digest: translucent_order_digest(&order),
        order_len: order.len() as u32,
        cpu_released: false,
        packed,
        order,
        buckets: None,
        digest: u64::from(tag),
        content_epoch: 1,
        world_revision: 0,
    }
}

#[test]
fn external_sections_delta_latest_write_wins_and_apply_order() {
    let coord_a = SectionCoord(0, 1, 0);
    let coord_b = SectionCoord(0, 2, 0);
    let coord_c = SectionCoord(0, 3, 0);
    let mut live = LiveSession::offline_with_null_audio();

    // Steady state: nothing touched → no delta, no work for the mirror.
    assert!(live.take_external_sections_delta().is_none());

    live.insert_external_section(coord_a, ext_mesh(1));
    live.insert_external_section(coord_b, ext_mesh(2));
    let rev1 = live.external_sections_rev;
    assert!(rev1 > 0);

    let delta = live.take_external_sections_delta().expect("dirty delta");
    assert!(!delta.cleared);
    assert_eq!(delta.upserts, BTreeSet::from([coord_a, coord_b]));
    assert!(delta.removals.is_empty());

    // Apply to a renderer-side mirror: only the touched coords clone.
    let mut mirror: BTreeMap<SectionCoord, ExternalSectionMesh> = BTreeMap::new();
    apply_external_sections_delta(&mut mirror, &delta, &live.external_sections);
    assert_eq!(mirror.len(), 2);
    assert_eq!(mirror[&coord_a].digest, 1);
    assert_eq!(mirror[&coord_b].digest, 2);

    // Drained: steady frames drain nothing again.
    assert!(live.take_external_sections_delta().is_none());

    // Latest write wins: update A, remove B, insert C in one window.
    live.insert_external_section(coord_a, ext_mesh(10));
    live.remove_external_section(coord_b);
    live.insert_external_section(coord_c, ext_mesh(3));
    let delta = live.take_external_sections_delta().expect("churn delta");
    assert_eq!(delta.upserts, BTreeSet::from([coord_a, coord_c]));
    assert_eq!(delta.removals, BTreeSet::from([coord_b]));
    apply_external_sections_delta(&mut mirror, &delta, &live.external_sections);
    assert_eq!(mirror.len(), 2);
    assert_eq!(mirror[&coord_a].digest, 10, "freshest payload wins");
    assert!(!mirror.contains_key(&coord_b));
    assert_eq!(mirror[&coord_c].digest, 3);

    // Insert-then-remove inside one window lands only in `removals`;
    // remove-then-insert lands only in `upserts`.
    live.insert_external_section(coord_a, ext_mesh(11));
    live.remove_external_section(coord_a);
    live.insert_external_section(coord_b, ext_mesh(12));
    let delta = live.take_external_sections_delta().expect("flip-flop delta");
    assert_eq!(delta.upserts, BTreeSet::from([coord_b]));
    assert_eq!(delta.removals, BTreeSet::from([coord_a]));
    apply_external_sections_delta(&mut mirror, &delta, &live.external_sections);
    assert!(!mirror.contains_key(&coord_a));
    assert_eq!(mirror[&coord_b].digest, 12);
}

#[test]
fn external_sections_clear_supersedes_pending_delta() {
    let mut live = LiveSession::offline_with_null_audio();
    let coord_a = SectionCoord(0, 1, 0);
    let coord_b = SectionCoord(0, 2, 0);
    live.insert_external_section(coord_a, ext_mesh(1));
    live.insert_external_section(coord_b, ext_mesh(2));

    // A dimension reset clears the map — the drained delta carries only
    // `cleared`, superseding every queued per-coord change.
    live.clear_external_sections();
    let delta = live.take_external_sections_delta().expect("clear delta");
    assert!(delta.cleared);
    assert!(delta.upserts.is_empty() && delta.removals.is_empty());

    let mut mirror: BTreeMap<SectionCoord, ExternalSectionMesh> = BTreeMap::new();
    mirror.insert(coord_a, ext_mesh(99));
    mirror.insert(coord_b, ext_mesh(98));
    apply_external_sections_delta(&mut mirror, &delta, &live.external_sections);
    assert!(mirror.is_empty(), "clear delta must wipe the mirror");
}

#[test]
fn external_sections_sync_counters_and_zero_clone_steady_frames() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    let mut live = LiveSession::offline_with_null_audio();
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    let coord_a = SectionCoord(0, 1, 0);
    live.insert_external_section(coord_a, ext_mesh(7));

    // First attach mirrors the whole map exactly once.
    renderer.sync_external_sections_from(&mut live);
    assert_eq!(renderer.external_section_full_syncs, 1);
    assert_eq!(renderer.external_sections.len(), 1);
    assert_eq!(renderer.external_sections[&coord_a].digest, 7);

    // Steady frames: revision unchanged → skip counter grows, zero clones.
    for _ in 0..8 {
        renderer.sync_external_sections_from(&mut live);
    }
    assert_eq!(renderer.external_section_full_syncs, 1);
    assert_eq!(renderer.external_section_delta_syncs, 0);
    assert_eq!(renderer.external_section_sync_skips, 8);

    // A single upsert applies through the delta path only.
    let coord_b = SectionCoord(0, 2, 0);
    live.insert_external_section(coord_b, ext_mesh(8));
    renderer.sync_external_sections_from(&mut live);
    assert_eq!(renderer.external_section_full_syncs, 1);
    assert_eq!(renderer.external_section_delta_syncs, 1);
    assert_eq!(renderer.external_sections.len(), 2);

    // Removal likewise.
    live.remove_external_section(coord_a);
    renderer.sync_external_sections_from(&mut live);
    assert_eq!(renderer.external_section_delta_syncs, 2);
    assert!(!renderer.external_sections.contains_key(&coord_a));
    assert_eq!(renderer.external_sections[&coord_b].digest, 8);
}

// ---------------------------------------------------------------------------
// 3. Capability-gated subgroup compute cull WGSL
// ---------------------------------------------------------------------------

#[test]
fn subgroup_cull_wgsl_validates_only_under_subgroup_capability() {
    // Baseline stays valid under the standard capability set.
    validate_wgsl_with_caps("SECTION_CULL_WGSL", SECTION_CULL_WGSL, Capabilities::all())
        .expect("baseline module must validate");

    // The subgroup variant shares the baseline prefix (structs, bindings,
    // helpers) — splice proof so they can never drift apart.
    let subgroup = section_cull_subgroup_wgsl();
    let anchor = SECTION_CULL_WGSL
        .find("@compute @workgroup_size(64)\nfn cs_cull")
        .expect("entry anchor");
    assert_eq!(
        &subgroup[..anchor],
        &SECTION_CULL_WGSL[..anchor],
        "subgroup variant must inherit the shared prefix verbatim"
    );
    assert!(subgroup.contains("subgroupAdd"));
    assert!(subgroup.contains("subgroupBroadcastFirst"));

    // Valid under the subgroup capability…
    validate_wgsl_with_caps("section_cull_subgroup_wgsl", &subgroup, Capabilities::all())
        .expect("subgroup module must validate with Capabilities::SUBGROUP");
    // …and rejected without it: the capability gate is real, so devices
    // lacking Features::SUBGROUP can never silently run this module.
    let without = Capabilities::all() & !Capabilities::SUBGROUP;
    let err = validate_wgsl_with_caps("section_cull_subgroup_wgsl", &subgroup, without)
        .expect_err("subgroup ops must fail validation without the capability");
    assert!(
        err.contains("SUBGROUP") || err.contains("subgroup"),
        "unexpected validation failure: {err}"
    );

    // The selector: `false` borrows the baseline, `true` builds the
    // subgroup module — same bind layout and entry point either way.
    match section_cull_wgsl(false) {
        std::borrow::Cow::Borrowed(s) => assert_eq!(s, SECTION_CULL_WGSL),
        std::borrow::Cow::Owned(_) => panic!("baseline selector must stay borrowed"),
    }
    let gated = section_cull_wgsl(true);
    assert!(gated.contains("subgroupAdd"));
    assert!(gated.contains("fn cs_cull"));

    // Pin the toolchain limitation the design documents: Naga 30 does
    // not implement `enable subgroups;` — the feature grant is the gate.
    let directive_err = wgsl::parse_str("enable subgroups;\nfn f() {}\n")
        .expect_err("enable subgroups; is unimplemented in Naga 30");
    let msg = directive_err.message().to_string();
    assert!(
        msg.contains("not yet supported") || msg.contains("not yet implemented"),
        "unexpected enable-directive error: {directive_err:?}"
    );
}

#[test]
fn gpu_hiz_culler_picks_subgroup_pipeline_from_device_features() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    let device = &ctx.device;
    let has_subgroup = device.features().contains(wgpu::Features::SUBGROUP);
    let culler = GpuHizCuller::new(device, 64, 64);
    assert_eq!(
        culler.subgroup_cull_active(),
        has_subgroup,
        "subgroup pipeline presence must track Features::SUBGROUP exactly \
         (adapter={}, report.subgroup_ops={})",
        ctx.report.name,
        ctx.report.subgroup_ops,
    );
}
