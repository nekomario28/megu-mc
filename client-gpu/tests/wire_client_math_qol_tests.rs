// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane WIRE-CLIENT-MATH-QOL proofs on the real adapter:
//!
//! 1. `LiveSession` streams every frame through the Jain–Chlamtac `P²`
//!    quantiles (P95/P99 — the 1% low), the anti-windup
//!    `PidBudgetController`, and the discounted-UCB bandit — the getters
//!    and counters prove the engines execute on real frames.
//! 2. The renderer's canonical `morton3d_signed`, the Mara–McGuire
//!    `projected_sphere_diameter_px` sub-pixel fast reject, and the
//!    `LipschitzTileClassifier` uniform-tile sweep run inside
//!    `cull_live_sections` — a distant section is dropped (< 0.5 px)
//!    while a near one is kept, `subpixel_culled_sections` is the live
//!    counter, and the Lipschitz verdict matches `box_in_frustum` over a
//!    real frustum sweep (zero false culls).
//! 3. `qol_gameplay`'s looking-at HUD projects the live crosshair target,
//!    `occlusion_table`'s coalesced queue drains real block edits into the
//!    light engine, and `command`'s dispatcher filters the bar's command
//!    candidates — all through the production `client-gpu`/`client-app`
//!    paths.

use megu_client::protocol::{
    decode_chunk_packet, ClientPlayEvent, DimensionMeta, PlayEvent, TeleportSync,
};
use megu_client::world::{BlockTable, WorldState};
use megu_client::zsh_command_bar::{CommandBarAction, ZshCommandBar};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    frustum_planes_from_view_proj, is_subpixel_section, morton3d, probe_adapters,
    request_gpu_device, section_frustum_visible, section_morton_key, GpuDevice, GpuRenderer,
    OFFSCREEN_FORMAT,
};
use megu_client_gpu::Camera;
use megu_client_mesh::{box_in_frustum, RenderBackendPreference, SectionCoord, EDGE};
use megu_core::math_opt_engine::{
    morton3d_encode, morton3d_signed, projected_sphere_diameter_px, DiscountedUcbAutotuner,
    P2QuantileEstimator, PidBudgetController,
};
use megu_core::math_opt_foundation::{LipschitzTileClassifier, TileSignDecision};
use megu_kernel::qol_gameplay::HudTarget;
use megu_ui::state::UiState;

// ---------------------------------------------------------------------------
// Shared fixtures (mirrors the gpu-indirect wire-test fixture)
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
    feed_stone_chunks(&mut live, chunks);
    live
}

/// The ingest-and-mesh tail of [`session_with_stone_chunks`], split out so a
/// test can swap `live.world` for a registry-backed `WorldState` before the
/// columns land.
fn feed_stone_chunks(live: &mut LiveSession, chunks: &[(i32, i32)]) {
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
}

/// A headless GPU device, or `None` when the host has no adapter (the
/// same graceful skip the other GPU tests use).
fn try_gpu() -> Option<GpuDevice> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP wire-client-math-qol test: probe_adapters found 0 adapters");
        return None;
    }
    Some(
        request_gpu_device(RenderBackendPreference::Auto, None, None)
            .expect("probe found adapters but request_gpu_device failed"),
    )
}

// ---------------------------------------------------------------------------
// 1. P² quantiles + anti-windup PID + D-UCB execute on real session frames
// ---------------------------------------------------------------------------

#[test]
fn live_session_streams_p2_pid_and_ducb_through_real_frames() {
    let mut live = session_with_stone_chunks(&[(0, 0)]);
    let mut ui = UiState::new();

    // Pre-bootstrap: the five-marker P² estimate is `None`.
    assert!(live.p95_frame_ms().is_none());
    assert!(live.p99_frame_ms().is_none());
    assert_eq!(live.ducb_samples(), 0);
    assert_eq!(live.frame_budget_scale(), 1.0);

    for _ in 0..8 {
        live.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);
    }
    let p95 = live
        .p95_frame_ms()
        .expect("P² P95 bootstraps after five frame samples");
    assert!(
        (p95 - 1000.0 / 60.0).abs() < 2.0,
        "P² P95 of a ~16.7 ms stream should land near 16.7 ms, got {p95}"
    );
    assert!(
        live.p99_frame_ms().is_some(),
        "P² P99 (1% low) must report once bootstrapped"
    );
    assert!(
        live.ducb_samples() >= 8,
        "every frame feeds the discounted-UCB bandit, got {}",
        live.ducb_samples()
    );
    assert!(live.ducb_selected_arm() < 4, "arm index stays < 4");
    // A ~16.7 ms stream at a 16.7 ms target keeps the PID near full budget.
    assert!(live.frame_budget_scale() > 0.5);
    let baseline_budget = live.mesh_rebuild_budget();

    // Sustained over-budget frames push the anti-windup PID down: the
    // rebuild budget must shrink but never drop below one section.
    for _ in 0..40 {
        live.record_frame_and_refresh_minihud(0.100, &mut ui);
    }
    let scale = live.frame_budget_scale();
    assert!(
        scale < 1.0,
        "sustained 100 ms frames must contract the PID scale, got {scale}"
    );
    assert!(scale >= 0.05 - 1e-9, "anti-windup floor is 0.05");
    let throttled = live.mesh_rebuild_budget();
    assert!(
        throttled >= 1 && throttled < baseline_budget,
        "PID must shrink the rebuild allowance ({throttled} < {baseline_budget})"
    );

    // The standalone engines agree with the live values — same engines,
    // same numbers, directly exercised.
    let mut p2 = P2QuantileEstimator::new(0.99);
    for _ in 0..8 {
        p2.observe(1000.0 / 60.0);
    }
    assert_eq!(p2.count(), 8);
    assert!((p2.estimate().unwrap() - 1000.0 / 60.0).abs() < 1e-6);
    let mut pid = PidBudgetController::new(1000.0 / 60.0);
    assert!(pid.update(100.0) < 1.0, "over-budget input contracts the scale");
    let mut bandit = DiscountedUcbAutotuner::<4>::new();
    bandit.observe(0, 1_000_000);
    assert!(bandit.select_arm() < 4);
}

// ---------------------------------------------------------------------------
// 2. Morton Z-order + sub-pixel reject + Lipschitz tile sweep
// ---------------------------------------------------------------------------

#[test]
fn renderer_morton_signed_subpixel_and_lipschitz_engines() {
    // `section_morton_key` IS `morton3d_signed` — the renderer's sort key
    // delegates to the canonical signed-coordinate engine.
    for &(x, y, z) in &[(0, 0, 0), (1, 2, 3), (-3, 5, -7), (1_048_575, 0, -1_048_576)] {
        assert_eq!(
            section_morton_key(SectionCoord(x, y, z)),
            morton3d_signed(x, y, z),
            "section key must equal morton3d_signed at {x},{y},{z}"
        );
    }
    // Signed bias keeps the origin ordering: coord -1 sorts before 0.
    assert_ne!(
        section_morton_key(SectionCoord(0, 0, -1)),
        section_morton_key(SectionCoord(0, 0, 0))
    );
    // Out-of-range axes saturate at the 2^20 edge instead of wrapping.
    assert_eq!(morton3d_signed(i32::MAX, 0, 0), morton3d_signed((1 << 20) - 1, 0, 0));
    assert_eq!(morton3d_signed(i32::MIN, 0, 0), morton3d_signed(-1 << 20, 0, 0));
    // The `morton3d` facade is the canonical `morton3d_encode` SWAR spread.
    assert_eq!(morton3d(5, 6, 7), morton3d_encode(5, 6, 7));

    // `projected_sphere_diameter_px` semantics on the section bounding
    // sphere (half the EDGE³ diagonal).
    let radius = EDGE as f32 * 0.5 * 3.0_f32.sqrt();
    let focal = 720.0; // ~90° vertical FOV at 720p
    let near = projected_sphere_diameter_px([0.0, 0.0, 32.0], radius, focal);
    assert!(near > 100.0, "a 32-block-away section is huge, got {near}");
    let far = projected_sphere_diameter_px([0.0, 0.0, 60_000.0], radius, focal);
    assert!(far < 0.5, "a 60k-block-away section is sub-pixel, got {far}");
    // Camera inside the sphere → INFINITY (never cull).
    assert_eq!(
        projected_sphere_diameter_px([0.0, 0.0, 1.0], radius, focal),
        f32::INFINITY
    );
    // Non-finite focal → fail-closed INFINITY.
    assert_eq!(
        projected_sphere_diameter_px([0.0, 0.0, 64.0], radius, f32::NAN),
        f32::INFINITY
    );

    // `is_subpixel_section`: distant sections are dropped, near and
    // camera-enclosing sections are kept.
    assert!(!is_subpixel_section([0.0, 0.0, 0.0], [16.0, 0.0, 0.0], focal));
    assert!(is_subpixel_section([0.0, 0.0, 0.0], [60_000.0, 0.0, 0.0], focal));
    assert!(!is_subpixel_section([8.0, 8.0, 8.0], [0.0; 3], focal));

    // `LipschitzTileClassifier` sign contract on a linear field, then the
    // production sweep: `section_frustum_visible` must match the exact
    // `box_in_frustum` p-vertex verdict on every section around a real
    // camera (the uniform-tile proof is conservative, never a false cull).
    assert_eq!(
        LipschitzTileClassifier::classify_subtile_lipschitz(100.0, 1.0, [8.0; 3]),
        TileSignDecision::UniformPositive
    );
    assert_eq!(
        LipschitzTileClassifier::classify_subtile_lipschitz(-100.0, 1.0, [8.0; 3]),
        TileSignDecision::UniformNegative
    );
    assert_eq!(
        LipschitzTileClassifier::classify_subtile_lipschitz(1.0, 1.0, [8.0; 3]),
        TileSignDecision::MixedRequiresEvaluation
    );

    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    let planes = frustum_planes_from_view_proj(&camera.view_projection(16.0 / 9.0));
    let mut checked = 0u32;
    let edge = EDGE as f32;
    for x in -4..=4 {
        for y in -2..=3 {
            for z in -4..=4 {
                let min = [x as f32 * edge, y as f32 * edge, z as f32 * edge];
                assert_eq!(
                    section_frustum_visible(min, &planes),
                    box_in_frustum(min, &planes),
                    "Lipschitz sweep must match box_in_frustum at {min:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 300, "the sweep must exercise hundreds of sections");
}

/// The sub-pixel fast reject runs inside the real `cull_live_sections`
/// loop: a section ~3.2 km out is dropped while the near section draws.
#[test]
fn live_cull_path_executes_subpixel_reject_on_real_adapter() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    // Chunk (200,200) puts a stone section ~4525 blocks away — under half
    // a pixel at the 64 px frame height used below.
    let live = session_with_stone_chunks(&[(0, 0), (200, 200)]);
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    assert!(
        renderer.update_live_terrain(&live.world) >= 1,
        "the sections must be resident"
    );
    let camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    renderer
        .render_offscreen_rgba(64, 64, &camera, None)
        .expect("offscreen frame");
    assert!(
        renderer.subpixel_culled_sections >= 1,
        "the ~4.5k-block section must hit the O(1) sub-pixel reject, got {}",
        renderer.subpixel_culled_sections
    );
}

// ---------------------------------------------------------------------------
// 3. qol_gameplay + occlusion_table + command through live paths
// ---------------------------------------------------------------------------

/// `qol_gameplay` projects the live crosshair target: a steep look-down
/// onto the stone section yields a `HudTarget::Block` HUD snapshot the
/// same frame `looking_at_block`/`looking_at_entity` refresh.
#[test]
fn live_session_looking_at_hud_uses_qol_gameplay_projection() {
    let mut live = LiveSession::offline_with_null_audio();
    // Eye at (8, 19.62, 8) looking straight down: the stone section's top
    // block (y=15) sits 4.62 blocks out — inside the 4.5-block reach only
    // just, so aim slightly below center for a comfortable hit margin.
    live.apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
        x: 8.0,
        y: 18.0,
        z: 8.0,
        yaw: 0.0,
        pitch: 80.0,
        flags: 0,
        teleport_id: 1,
    })))
    .unwrap();
    let chunk = decode_chunk_packet(&chunk_body(0, 0, 4), 24, 64).expect("decoded real chunk");
    live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
        .unwrap();

    assert!(live.looking_at_hud.is_none());
    let mut ui = UiState::new();
    live.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);

    let info = live
        .looking_at_hud
        .as_ref()
        .expect("qol_gameplay must project the block under the crosshair");
    assert!(
        matches!(info.target, HudTarget::Block(_)),
        "the crosshair hit a block, got {:?}",
        info.target
    );
    assert!(!info.name.is_empty(), "HUD snapshot carries a display name");
    assert_eq!(info.mod_badge, "Minecraft");
    // The legacy feeds refreshed in the same pass stay consistent.
    assert!(live.looking_at_block.is_some());
    assert!(live.looking_at_entity.is_none());
}

/// Minimal version pack: `blocks.json` alone. With no
/// `block_state_facts.bin`/`block_states.json` the kernel derives a
/// structural facts table — `air` passes light, every other state an
/// opaque full cube — which is enough to build `WorldState`'s registry
/// and `LightEngine` on a checkout whose generated `assets/*_1211*`
/// pack is absent.
fn write_stone_light_pack() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("megu-wire-qol-light-pack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create synthetic pack dir");
    std::fs::write(
        dir.join("blocks.json"),
        r#"[
            {"name": "air", "defaultState": 0, "minStateId": 0, "maxStateId": 0},
            {"name": "stone", "defaultState": 1, "minStateId": 1, "maxStateId": 1}
        ]"#,
    )
    .expect("write blocks.json");
    // Write a matching 2-state `block_state_facts.bin` (6 bytes per state) so
    // `Registry::from_version_dir` never falls back to a host-installed
    // 26k-state `assets/block_state_facts_1211.bin` when present locally.
    const FACT_IS_AIR: u8 = 1;
    const FACT_BLOCKS_MOTION: u8 = 1 << 2;
    const FACT_CAN_OCCLUDE: u8 = 1 << 3;
    const FACT_SOLID_RENDER: u8 = 1 << 4;
    const FACT_FLUID_EMPTY: u8 = 1 << 7;
    let facts: [u8; 12] = [
        FACT_IS_AIR | FACT_FLUID_EMPTY,
        0,
        0,
        0,
        0,
        0,
        FACT_SOLID_RENDER | FACT_CAN_OCCLUDE | FACT_BLOCKS_MOTION,
        15,
        0x3f,
        0,
        0,
        0,
    ];
    std::fs::write(dir.join("block_state_facts.bin"), facts)
        .expect("write block_state_facts.bin");
    dir
}

/// `occlusion_table`'s `CoalescedLightQueue` drains real block edits into
/// the `LightEngine`: batched section updates coalesce last-write-wins
/// into one engine call per chunk, and `local_light_edits` counts the
/// applied edits — the counter proves the queue→batch→engine path ran.
#[test]
fn world_block_updates_drain_coalesced_light_queue() {
    // `queue_local_light_edit`/`flush_light_queue` only run with a kernel
    // registry + `LightEngine`, and `WorldState::new` finds none on the
    // public clean-room checkout — the `assets/*_1211*` pack is a generated
    // Mojang fixture excluded from git, so the offline session's edits
    // silently dropped and the counter stayed zero. The synthetic pack
    // gives the session a real engine, keeping every assertion below a
    // proof that the drain ran rather than a vacuous `>=` pass.
    let pack = write_stone_light_pack();
    let mut live = LiveSession::offline_with_null_audio();
    live.world =
        WorldState::new_from_version_dir(DimensionMeta::default(), BlockTable::load(None), &pack)
            .expect("synthetic version pack opens");
    feed_stone_chunks(&mut live, &[(0, 0)]);
    let baseline = live.world.local_light_edits;

    // A `SectionBlocksUpdate` batch carrying three edits — two to the
    // same cell (last-write-wins coalesces them) — flows through
    // `CoalescedLightQueue::drain` + `ChunkLightBatch::to_light_edits`.
    live.apply_event(ClientPlayEvent::Play(PlayEvent::SectionBlocksUpdate(vec![
        ([4, 4, 4], 0),
        ([8, 8, 8], 0),
        ([4, 4, 4], 0),
    ])))
    .unwrap();

    // The queue dedups the same-cell rewrite: exactly two distinct cells
    // applied for three pushed edits.
    assert_eq!(
        live.world.local_light_edits,
        baseline + 2,
        "three pushed edits coalesce last-write-wins into two engine edits"
    );
    // A single `BlockUpdate` exercises the same queue→flush machinery.
    live.apply_event(ClientPlayEvent::Play(PlayEvent::BlockUpdate {
        pos: [8, 8, 9],
        state_id: 0,
    }))
    .unwrap();
    assert_eq!(
        live.world.local_light_edits,
        baseline + 3,
        "a live block update must propagate light through the queue"
    );
    let _ = std::fs::remove_dir_all(&pack);
}

/// `command`'s `CommandDispatcher` filters the bar's command candidates
/// through `client-app`'s production path: a literal-prefix query narrows
/// to the wire-accepted set, while the session dispatcher itself answers
/// `complete_suggestions` through `client-gpu`.
#[test]
fn command_dispatcher_filters_bar_candidates_and_session_wire_path() {
    // client-app path: `ZshCommandBar`'s Commands context consults the
    // kernel dispatcher — `"/gi"` narrows to the `/gi*` literal set.
    let mut bar = ZshCommandBar::new();
    bar.set_text("/gi");
    let action = bar.handle_char('\t');
    match action {
        CommandBarAction::Completed { text, .. } => {
            assert!(
                text.starts_with("/gi"),
                "dispatcher-filtered completion must stay in the /gi set, got {text}"
            );
        }
        other => panic!("a /gi query must complete through the dispatcher, got {other:?}"),
    }

    // client-gpu path: `LiveSession.commands` (the same seeded dispatcher
    // `window.rs` queries for chat suggestions) answers the same query.
    let live = session_with_stone_chunks(&[]);
    let suggestions = live
        .commands
        .complete_suggestions("gi", 2)
        .expect("session dispatcher completes");
    let mut found = Vec::new();
    for index in 0..suggestions.len() {
        found.push(suggestions.get(index).unwrap_or_default().to_string());
    }
    assert!(
        found.iter().any(|item| item == "give"),
        "the wire path must offer `give` for `gi`, got {found:?}"
    );
}
