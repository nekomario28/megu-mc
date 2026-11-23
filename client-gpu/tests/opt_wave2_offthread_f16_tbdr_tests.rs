//! Client optimization wave-2 coverage: `MeshJob::SectionMesh`
//! background execution + revision dedup, the same-frame near-player
//! priority-0 inline lane, TBDR `StoreOp::Discard` depth policy, and
//! the capability-gated `enable f16;` WGSL selection (both variants
//! Naga-validated so regressions surface on hosts without `SHADER_F16`).

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client_gpu::live_session::hw_adaptive_engine::GpuComputeParadigm;
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::mesh_pool::{MeshJob, MeshJobPool, MeshResult, SectionMeshLane};
use megu_client_gpu::renderer::{
    optional_device_features, select_depth_store_op, sky_wgsl, sky_wgsl_f16, terrain_wgsl,
    terrain_wgsl_f16,
};
use megu_client_gpu::subworld_render::COMPOSITE_TERRAIN_WGSL;
use megu_client_mesh::builtin_shadow_wgsl::MEGU_SKY_ATMOSPHERE_WGSL;
use megu_client_mesh::{
    BuiltSectionMesh, SectionCoord, SectionMeshInput, DIRECTION_BUCKETS,
};

/// Protocol 767 `level_chunk_with_light` body, dimension -64..320 (24
/// sections). Section index 4 covers world y 0..16 — a single
/// solid-stone section surrounded by air.
fn varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn chunk_body(cx: i32, cz: i32, stone_section: i32) -> Vec<u8> {
    let mut sections = Vec::new();
    for section in 0..24 {
        let filled = section == stone_section;
        sections.extend_from_slice(&(if filled { 4096i16 } else { 0 }).to_be_bytes());
        sections.push(0);
        varint(&mut sections, if filled { 1 } else { 0 });
        varint(&mut sections, 0);
        sections.push(0);
        varint(&mut sections, 0);
        varint(&mut sections, 0);
    }
    let mut body = Vec::new();
    body.extend_from_slice(&cx.to_be_bytes());
    body.extend_from_slice(&cz.to_be_bytes());
    body.extend_from_slice(&[10, 0]);
    varint(&mut body, sections.len() as u32);
    body.extend_from_slice(&sections);
    for _ in 0..7 {
        varint(&mut body, 0);
    }
    body
}

/// Ingest chunk packets but leave them dirty so `poll` performs the
/// ordered rebuild + off-thread lane itself.
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

/// A `SectionMeshInput` snapshot for `coord` from an ingested-but-dirty
/// session — what `submit_offthread_sections` hands to `MeshJobPool`.
fn snapshot_input(live: &LiveSession, coord: SectionCoord, revision: u64) -> SectionMeshInput {
    live.world
        .mesh_cache
        .snapshot_section_input(coord, revision)
        .expect("section loaded -> snapshot")
}

fn fake_built(coord: SectionCoord, revision: u64) -> BuiltSectionMesh {
    BuiltSectionMesh {
        coord,
        revision,
        opaque_packed: Vec::new(),
        opaque_bucket_ranges: [(0, 0); DIRECTION_BUCKETS],
        translucent_quads: Vec::new(),
        visibility_mask: 0,
    }
}

// ----------------------------------------------------------------
// Off-thread SectionMesh execution
// ----------------------------------------------------------------

#[test]
fn mesh_job_section_mesh_executes_on_worker() {
    let live = session_with_dirty_stone_chunks(&[(0, 0)]);
    // Section index 4 of column (0,0) is SectionCoord(0,0,0) — the solid
    // stone slab at world y 0..16.
    let coord = SectionCoord(0, 0, 0);
    let input = snapshot_input(&live, coord, 1);

    let pool = MeshJobPool::new(1);
    assert!(pool.try_submit(MeshJob::SectionMesh { input }));

    let mut built: Option<BuiltSectionMesh> = None;
    for _ in 0..400 {
        pool.drain(|result| {
            if let MeshResult::SectionMesh { built: b } = result {
                built = Some(b);
            }
        });
        if built.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let built = built.expect("worker returned a SectionMesh result");
    assert_eq!(built.coord, coord);
    assert_eq!(built.revision, 1);
    // Stone slab -> non-empty opaque head, empty translucent tail.
    assert!(!built.opaque_packed.is_empty());
}

// ----------------------------------------------------------------
// Revision dedup + stale-result dropping (SectionMeshLane)
// ----------------------------------------------------------------

#[test]
fn section_mesh_lane_deduplicates_and_drops_stale() {
    let coord = SectionCoord(0, 0, 0);
    let mut lane = SectionMeshLane::default();

    // Fresh dirty mark -> epoch 0, submission tagged rev 1.
    lane.mark_dirty(coord);
    assert!(lane.can_submit(coord));
    let rev = lane.pending_revision(coord);
    assert_eq!(lane.note_submitted(coord), rev);
    // One job per coord — a second submission is refused while in flight.
    assert!(!lane.can_submit(coord));

    // Content re-dirtied after the snapshot -> the returned build is stale.
    lane.mark_dirty(coord);
    assert!(!lane.accept(&fake_built(coord, rev)));
    // The stale drop freed the in-flight slot for a resubmission.
    assert!(lane.can_submit(coord));

    // Fresh submission at the new epoch accepts.
    let rev2 = lane.pending_revision(coord);
    assert_eq!(lane.note_submitted(coord), rev2);
    assert!(lane.accept(&fake_built(coord, rev2)));
    assert!(lane.can_submit(coord));
}

#[test]
fn section_mesh_lane_remove_clears_in_flight() {
    let coord = SectionCoord(1, 0, 0);
    let mut lane = SectionMeshLane::default();
    lane.mark_dirty(coord);
    let rev = lane.pending_revision(coord);
    lane.note_submitted(coord);
    // Unload: tracking leaves; a late result drops as stale.
    lane.remove(coord);
    assert!(!lane.accept(&fake_built(coord, rev)));
}

// ----------------------------------------------------------------
// Priority-0 near-player inline lane
// ----------------------------------------------------------------

#[test]
fn poll_rebuilds_near_player_sections_inline_same_frame() {
    // Camera [8,20,24] stands ~8 blocks from section (0,0,0). One poll
    // must mesh it synchronously — the priority-0 lane degenerates the
    // frustum so the nearest section wins even outside the view — and
    // derive its world artifacts, so `packed_mesh` is populated without
    // any worker round-trip.
    let mut live = session_with_dirty_stone_chunks(&[(0, 0)]);
    live.poll([8.0, 20.0, 24.0], &[]);
    assert!(
        live.priority_zero_meshes >= 1,
        "near-player section must rebuild inline in the same poll"
    );
    assert!(
        live.priority_zero_meshes <= 2,
        "priority-0 lane is bounded per poll (got {})",
        live.priority_zero_meshes
    );
    assert!(
        live.world.packed_meshes().next().is_some(),
        "priority-0 rebuild publishes through the world artifact path"
    );
}

#[test]
fn poll_submits_distant_sections_off_thread() {
    // Only non-air sections enter the mesh cache (one per test chunk),
    // so use columns 50+ blocks away — outside PRIORITY_ZERO_NEAR_RADIUS
    // and past the 1-per-poll inline budget. The remaining dirty tail
    // must go to the SectionMesh worker pool and publish accepted
    // payloads via external_sections.
    let mut live =
        session_with_dirty_stone_chunks(&[(4, 0), (5, 0), (6, 0), (7, 0)]);
    live.governor_rebuild_cap = 1;
    let camera = [8.0, 20.0, 24.0];
    // Prime the lane: first poll submits bounded jobs.
    live.poll(camera, &[]);
    assert!(
        live.offthread_mesh_submitted >= 1,
        "tail beyond the inline budget must go off-thread"
    );
    // Keep polling — drain each poll — until the workers' results land.
    let mut accepted = false;
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(2));
        live.poll(camera, &[]);
        if live.offthread_mesh_completed >= 1 {
            accepted = true;
            break;
        }
    }
    assert!(
        accepted,
        "a SectionMesh worker result must publish within ~1s"
    );
    assert!(!live.external_sections.is_empty());
}

// ----------------------------------------------------------------
// TBDR depth StoreOp policy
// ----------------------------------------------------------------

#[test]
fn depth_store_op_discards_only_on_tbdr_without_depth_readers() {
    use wgpu::StoreOp;
    // Tile GPU, nobody reads depth afterwards -> discard the tile.
    assert_eq!(
        select_depth_store_op(GpuComputeParadigm::TileBasedTbdr, false),
        StoreOp::Discard
    );
    // Same adapter but Hi-Z (or post) consumes depth -> must store.
    assert_eq!(
        select_depth_store_op(GpuComputeParadigm::TileBasedTbdr, true),
        StoreOp::Store
    );
    // Immediate-mode renderers keep storing either way.
    for paradigm in [
        GpuComputeParadigm::DiscretePcieHighBw,
        GpuComputeParadigm::UnifiedApuSharedRam,
        GpuComputeParadigm::LowPowerIntegratedWgpu,
        GpuComputeParadigm::HeadlessNoGpu,
    ] {
        assert_eq!(select_depth_store_op(paradigm, false), StoreOp::Store);
        assert_eq!(select_depth_store_op(paradigm, true), StoreOp::Store);
    }
}

// ----------------------------------------------------------------
// Capability probing: optional device features
// ----------------------------------------------------------------

#[test]
fn optional_device_features_intersects_adapter_support() {
    use wgpu::Features;
    let both = Features::SHADER_F16 | Features::SUBGROUP;
    assert_eq!(optional_device_features(both), both);
    // Unsupported bits are never requested — adapters without the
    // features create devices cleanly.
    assert_eq!(
        optional_device_features(Features::empty()),
        Features::empty()
    );
    // Only the optional bits pass through — anything else the adapter
    // supports is requested separately by the caller.
    let unrelated = Features::DEPTH_CLIP_CONTROL;
    assert_eq!(optional_device_features(both | unrelated), both);
}

// ----------------------------------------------------------------
// Capability-gated WGSL selection + Naga validation of both variants
// ----------------------------------------------------------------

#[test]
fn wgsl_selectors_gate_on_shader_f16() {
    // f32 fallbacks are the shared static sources.
    assert_eq!(terrain_wgsl(false).as_ref(), COMPOSITE_TERRAIN_WGSL);
    assert_eq!(sky_wgsl(false).as_ref(), MEGU_SKY_ATMOSPHERE_WGSL);
    // f16 variants carry the enable directive; fallbacks do not.
    assert!(terrain_wgsl(true).contains("enable f16;"));
    assert!(!terrain_wgsl(false).contains("enable f16;"));
    assert!(sky_wgsl(true).contains("enable f16;"));
    assert!(!sky_wgsl(false).contains("enable f16;"));
    // Builders and selectors agree.
    assert_eq!(terrain_wgsl(true).as_ref(), terrain_wgsl_f16().as_str());
    assert_eq!(sky_wgsl(true).as_ref(), sky_wgsl_f16().as_str());
}

#[test]
fn naga_validates_f32_terrain_shader() {
    use wgpu::naga::{front::wgsl, valid::*};
    let module = wgsl::parse_str(COMPOSITE_TERRAIN_WGSL).expect("f32 terrain WGSL parses");
    Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .expect("f32 terrain WGSL validates");
    let names: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("vs")));
    assert!(names.iter().any(|n| n.contains("fs")));
}

#[test]
fn naga_validates_f16_terrain_shader_with_float16_capability() {
    use wgpu::naga::{front::wgsl, valid::*};
    let src = terrain_wgsl_f16();
    let module = wgsl::parse_str(&src).expect("f16 terrain WGSL parses");
    // The f16 variant must NOT validate without the capability.
    assert!(
        Validator::new(ValidationFlags::all(), Capabilities::empty())
            .validate(&module)
            .is_err(),
        "f16 shader requires Capabilities::SHADER_FLOAT16"
    );
    Validator::new(ValidationFlags::all(), Capabilities::SHADER_FLOAT16)
        .validate(&module)
        .expect("f16 terrain WGSL validates with SHADER_FLOAT16");
    let names: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("vs")));
    assert!(names.iter().any(|n| n.contains("fs")));
}

#[test]
fn naga_validates_f16_sky_shader_with_float16_capability() {
    use wgpu::naga::{front::wgsl, valid::*};
    let src = sky_wgsl_f16();
    let module = wgsl::parse_str(&src).expect("f16 sky WGSL parses");
    Validator::new(ValidationFlags::all(), Capabilities::SHADER_FLOAT16)
        .validate(&module)
        .expect("f16 sky WGSL validates with SHADER_FLOAT16");
    let names: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
    assert!(names.iter().any(|n| n.contains("vs")));
    assert!(names.iter().any(|n| n.contains("fs")));
    // The f32 sky variant stays the zero-capability default.
    let f32_module = wgsl::parse_str(MEGU_SKY_ATMOSPHERE_WGSL).expect("f32 sky WGSL parses");
    Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&f32_module)
        .expect("f32 sky WGSL validates");
}
