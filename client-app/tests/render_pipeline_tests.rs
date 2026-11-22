// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane RENDER-WGSL coverage: WGSL structural validation, `RenderGraph`
//! per-pipeline pass planning + binding consistency, and the live software
//! frame path exercising `HiZPyramid`, `BakedModelQuad`, and `ZoomState`.

use megu_client::audio_qol::ZoomState;
use megu_client::protocol::{
    DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta,
};
use megu_client::render::{
    frustum_planes, rasterize_baked_model_quads, render_frame,
    render_frame_with_hiz_and_zoom, BakedModelDraw, RenderConfig,
};
use megu_client::render_graph::{
    plan_frame, FramePlanInput, GpuPipelineKind, InstanceSubmission, PassSubmission, RenderPassId,
};
use megu_client::shaders::{
    validate_wgsl_module, WgslStage, WgslValidationError, SHADER_SUITE,
};
use megu_client::world::{BlockTable, WorldState};
use megu_client_mesh::{ActiveRenderBackend, BakedModelQuad, MAX_INSTANCES_PER_UBO_16KB};

// ---------------------------------------------------------------------------
// WGSL structural validation
// ---------------------------------------------------------------------------

#[test]
fn every_suite_shader_passes_structural_validation() {
    assert_eq!(SHADER_SUITE.len(), 6, "expected the full WGSL suite");
    for spec in SHADER_SUITE {
        let info = validate_wgsl_module(spec.source, spec.required_stages)
            .unwrap_or_else(|e| panic!("{} failed validation: {e}", spec.name));
        // Every required stage resolves to a real entry-point fn.
        for stage in spec.required_stages {
            assert!(
                info.entry_points.iter().any(|(s, name)| s == stage && !name.is_empty()),
                "{}: no {stage:?} entry point",
                spec.name,
            );
        }
        // The suite's resource contract: every bound var is group 0.
        assert!(
            info.bindings.iter().all(|b| b.group == 0),
            "{}: resources must live in group 0",
            spec.name,
        );
    }
}

#[test]
fn validator_catches_broken_modules() {
    // Unbalanced braces.
    let err = validate_wgsl_module(
        "@vertex fn v() -> i32 { let x = 1; ",
        &[WgslStage::Vertex],
    )
    .unwrap_err();
    assert!(matches!(err, WgslValidationError::UnbalancedDelimiter { .. }));

    // Mismatched closer.
    let err = validate_wgsl_module(
        "@vertex fn v() { let x = (1; }",
        &[WgslStage::Vertex],
    )
    .unwrap_err();
    assert!(matches!(err, WgslValidationError::UnbalancedDelimiter { .. }));

    // Missing required entry point.
    let err = validate_wgsl_module(
        "@vertex fn v() {}",
        &[WgslStage::Vertex, WgslStage::Fragment],
    )
    .unwrap_err();
    assert_eq!(
        err,
        WgslValidationError::MissingEntryPoint {
            stage: WgslStage::Fragment
        }
    );

    // Dangling @group without a var.
    let err = validate_wgsl_module(
        "@group(0) fn f() {}",
        &[WgslStage::Vertex],
    )
    .unwrap_err();
    assert!(matches!(err, WgslValidationError::MalformedAttribute { .. }));

    // @group without a paired @binding on the same var.
    let err = validate_wgsl_module(
        "@group(0) var<uniform> u: U;\nstruct U { x: f32 };",
        &[],
    )
    .unwrap_err();
    assert!(matches!(err, WgslValidationError::MalformedAttribute { .. }));

    // Duplicate (group, binding) pair.
    let err = validate_wgsl_module(
        "@group(0) @binding(0) var<uniform> a: U;\n@group(0) @binding(0) var<uniform> b: U;\nstruct U { x: f32 };",
        &[],
    )
    .unwrap_err();
    assert_eq!(
        err,
        WgslValidationError::DuplicateBinding {
            group: 0,
            binding: 0
        }
    );

    // Unterminated block comment; comment braces must not count either.
    let err = validate_wgsl_module("/* never closed {(", &[WgslStage::Vertex]).unwrap_err();
    assert!(matches!(err, WgslValidationError::UnterminatedComment { .. }));
    let info = validate_wgsl_module(
        "// { [ ( comment braces are inert\n@compute @workgroup_size(1) fn c() {}",
        &[WgslStage::Compute],
    )
    .unwrap();
    assert_eq!(info.entry_points.len(), 1);
    assert_eq!(info.entry_points[0].0, WgslStage::Compute);
}

#[test]
fn validator_reports_binding_kinds() {
    let info = validate_wgsl_module(
        megu_client::shaders::MESHLET_CULL_COMPUTE_WGSL,
        &[WgslStage::Compute],
    )
    .unwrap();
    let kinds: Vec<_> = info.bindings.iter().map(|b| (b.binding, b.kind)).collect();
    use megu_client::shaders::WgslBindingKind as K;
    assert_eq!(
        kinds,
        vec![
            (0, K::Uniform),
            (1, K::StorageRead),
            (2, K::StorageReadWrite),
            (3, K::StorageReadWrite),
            (4, K::Texture),
        ]
    );
}

// ---------------------------------------------------------------------------
// RenderGraph planning
// ---------------------------------------------------------------------------

fn plan_input() -> FramePlanInput {
    FramePlanInput {
        width: 640,
        height: 360,
        visible_sections: 42,
        meshlet_count: 512,
        block_entity_instances: 600,
        block_entity_mesh_kinds: 3,
        translucent_sections: 7,
    }
}

fn pass_ids(plan: &megu_client::render_graph::RenderGraphPlan) -> Vec<RenderPassId> {
    plan.passes.iter().map(|p| p.id).collect()
}

#[test]
fn render_graph_plans_distinct_valid_pass_sequences_per_pipeline() {
    let input = plan_input();
    let plans: Vec<_> = GpuPipelineKind::ALL
        .iter()
        .map(|&p| (p, plan_frame(p, &input)))
        .collect();

    let expected = [
        (GpuPipelineKind::MeshShader, 8),
        (GpuPipelineKind::ComputeIndirect, 8),
        // Classic keeps the Hi-Z chain but does CPU meshlet culling.
        (GpuPipelineKind::Classic, 7),
        // ClassicLowSpec (legacy Intel HD, 16 KiB UBO): no compute at all —
        // no Hi-Z build and no GPU meshlet cull.
        (GpuPipelineKind::ClassicLowSpec, 6),
    ];
    let mut sequences = Vec::new();
    for (kind, plan) in &plans {
        let ids = pass_ids(plan);
        sequences.push(
            plan.passes
                .iter()
                .map(|p| (p.id, p.submission))
                .collect::<Vec<_>>(),
        );
        // Canonical order, first pass always DepthPrepass, last HudOverlay.
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "{kind:?} passes are out of order");
        assert_eq!(ids.first(), Some(&RenderPassId::DepthPrepass));
        assert_eq!(ids.last(), Some(&RenderPassId::HudOverlay));
        let (_, want_passes) = expected
            .iter()
            .find(|(k, _)| k == kind)
            .expect("pipeline covered");
        assert_eq!(ids.len(), *want_passes, "{kind:?} pass count");
    }
    // Every pipeline produces a distinct (pass, submission) sequence.
    for i in 0..sequences.len() {
        for j in i + 1..sequences.len() {
            assert_ne!(
                sequences[i], sequences[j],
                "pipelines {i} and {j} produced identical sequences"
            );
        }
    }
    // Hi-Z build is skipped only on ClassicLowSpec; MeshletCull runs only on
    // the GPU-cull paths.
    for (kind, plan) in &plans {
        let has_hiz = plan.pass(RenderPassId::HiZBuild).is_some();
        let has_cull = plan.pass(RenderPassId::MeshletCull).is_some();
        assert_eq!(has_hiz, *kind != GpuPipelineKind::ClassicLowSpec, "{kind:?}");
        assert_eq!(
            has_cull,
            matches!(
                kind,
                GpuPipelineKind::MeshShader | GpuPipelineKind::ComputeIndirect
            ),
            "{kind:?}"
        );
    }
}

#[test]
fn render_graph_instanced_submission_follows_pipeline() {
    let input = plan_input();
    for kind in GpuPipelineKind::ALL {
        let plan = plan_frame(kind, &input);
        let pass = plan.pass(RenderPassId::InstancedBlockEntities).unwrap();
        match kind {
            GpuPipelineKind::ClassicLowSpec => {
                assert_eq!(pass.submission, PassSubmission::UboBatches16k);
                // 600 instances over 16 KiB-safe batches: ceil(600/256) = 3.
                assert_eq!(
                    pass.estimated_draw_calls,
                    600_u32.div_ceil(MAX_INSTANCES_PER_UBO_16KB as u32),
                    "UBO batch draw calls"
                );
            }
            _ => {
                // One indirect command per active mesh kind on SSBO/MDI GPUs.
                assert_eq!(pass.estimated_draw_calls, input.block_entity_mesh_kinds);
            }
        }
    }
    // GpuPipelineKind mirrors ActiveRenderBackend 1:1.
    for backend in [
        ActiveRenderBackend::MeshShader,
        ActiveRenderBackend::ComputeIndirect,
        ActiveRenderBackend::Classic,
        ActiveRenderBackend::ClassicLowSpec,
    ] {
        let kind = GpuPipelineKind::from(backend);
        assert_eq!(kind.as_str(), backend.as_str());
    }
    assert_eq!(
        GpuPipelineKind::ClassicLowSpec.instanced_submission(),
        InstanceSubmission::UboBatches16k
    );
}

#[test]
fn render_graph_binding_layouts_match_their_wgsl_modules() {
    let input = plan_input();
    for kind in GpuPipelineKind::ALL {
        let plan = plan_frame(kind, &input);
        plan.validate_binding_layouts()
            .unwrap_or_else(|e| panic!("{kind:?} binding mismatch: {e}"));
    }
}

#[test]
fn render_graph_is_deterministic_and_budgets_resources() {
    let input = plan_input();
    for kind in GpuPipelineKind::ALL {
        let a = plan_frame(kind, &input);
        let b = plan_frame(kind, &input);
        assert_eq!(pass_ids(&a), pass_ids(&b));
        assert_eq!(a.total_estimated_draw_calls(), b.total_estimated_draw_calls());
        assert_eq!(a.vram_estimate_bytes(), b.vram_estimate_bytes());
        assert!(a.total_estimated_draw_calls() > 0);
        assert!(a.vram_estimate_bytes() > 0);
        // Depth target + color target alone must exceed w*h*8 bytes.
        assert!(a.vram_estimate_bytes() >= u64::from(input.width) * u64::from(input.height) * 8);
    }
}

// ---------------------------------------------------------------------------
// Live software frame: HiZPyramid + BakedModelQuad + ZoomState
// ---------------------------------------------------------------------------

fn test_dimension() -> DimensionMeta {
    DimensionMeta {
        name: "minecraft:overworld".to_string(),
        min_y: 0,
        height: 64,
        has_skylight: true,
    }
}

fn uniform_section(state: u32) -> DecodedSection {
    DecodedSection {
        non_air: if state == 0 {
            0
        } else {
            megu_client_mesh::SECTION_CELLS as i16
        },
        states: Box::new([state; megu_client_mesh::SECTION_CELLS]),
        biomes: [0; 64],
    }
}

fn chunk(cx: i32, cz: i32, sections: Vec<DecodedSection>) -> DecodedChunk {
    DecodedChunk {
        cx,
        cz,
        sections,
        block_entities: Vec::new(),
        light: DecodedLightData {
            sky_mask: Vec::new(),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky_arrays: Vec::new(),
            block_arrays: Vec::new(),
        },
    }
}

/// A 32x32 solid wall at z in [0, 16] — sections (0..=1, 0..=1, 0) — with a
/// second solid section directly behind it at (0, 0, 1) covering z [16, 32].
/// Everything else is air so the `GraphCuller` BFS rings the wall through
/// the open sections above and still reaches (and draws) the hidden section
/// — only the Hi-Z pyramid can prove it occluded. The wall is two sections
/// wide/tall rather than one so the hidden section's conservative mip-texel
/// footprint lands entirely on wall-covered depth pixels.
fn wall_world() -> WorldState {
    let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
    for cx in 0..2 {
        for cz in -4..=1 {
            world
                .apply_chunk(chunk(
                    cx,
                    cz,
                    vec![
                        uniform_section(u32::from(cz == 0 || (cx == 0 && cz == 1))),
                        uniform_section(u32::from(cz == 0)),
                        uniform_section(0),
                        uniform_section(0),
                    ],
                ))
                .unwrap();
        }
    }
    world
}

fn baked_quad() -> BakedModelQuad {
    // One full-block face quad in block-local space, facing +z toward the
    // camera: corners ordered like the model baker's south face.
    BakedModelQuad {
        positions: [
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0],
        ],
        normal: [0.0, 0.0, 1.0],
        texture: "test:stone".to_string(),
        uv: [[0.0, 0.0], [16.0, 0.0], [16.0, 16.0], [0.0, 16.0]],
        cullface: None,
        tintindex: -1,
        shade: true,
    }
}

fn rebuild(world: &mut WorldState, eye: [f32; 3], yaw: f32, pitch: f32) {
    let planes = frustum_planes(eye, yaw, pitch, 16.0 / 9.0);
    world
        .rebuild_dirty_bounded_ordered(usize::MAX, eye, &planes)
        .unwrap();
}

#[test]
fn hiz_frame_culls_behind_wall_section_with_zero_pixel_corruption() {
    let mut world = wall_world();
    let eye = [16.0, 16.0, -50.0];
    rebuild(&mut world, [16.0, 16.0, -50.0], 0.0, 0.0);

    // Baseline: the graph culler reaches all five solid sections (the
    // behind-wall section is frustum+graph visible via the air ring; only
    // Hi-Z can prove it hidden).
    let plain = render_frame(&world, eye, 0.0, 0.0, 160, 90).unwrap();
    assert_eq!(plain.stats.drawn_sections, 5, "baseline must draw all solids");
    assert_eq!(plain.stats.hiz_culled_sections, 0);

    // Unzoomed ZoomState(70) keeps the default FOV, so the Hi-Z frame must
    // be pixel-identical to the plain frame while skipping the occluded
    // section entirely.
    let mut zoom = ZoomState::new(70.0);
    let quad = baked_quad();
    let models = [BakedModelDraw {
        quads: std::slice::from_ref(&quad),
        origin: [6.0, 6.0, -20.0],
        scale: 1.0,
    }];
    let frame =
        render_frame_with_hiz_and_zoom(&world, eye, 0.0, 0.0, 160, 90, &RenderConfig::default(), &mut zoom, 0.0, &models)
            .unwrap();
    assert_eq!(frame.stats.hiz_tested_sections, 5);
    assert_eq!(frame.stats.hiz_culled_sections, 1);
    assert_eq!(frame.stats.drawn_sections, 4);
    assert_eq!(frame.stats.model_quads_drawn, 1);

    // Non-blank: the wall face plus the placed model quad drew pixels.
    assert!(frame.stats.pixels_shaded > 0);
    assert!(frame.rgb.as_chunks::<3>().0.iter().any(|p| *p != [168, 205, 240]));

    // Zero corruption: without the model draw, the Hi-Z frame must equal
    // the plain frame byte-for-byte.
    let mut zoom2 = ZoomState::new(70.0);
    let frame2 = render_frame_with_hiz_and_zoom(
        &world,
        eye,
        0.0,
        0.0,
        160,
        90,
        &RenderConfig::default(),
        &mut zoom2,
        0.0,
        &[],
    )
    .unwrap();
    assert_eq!(frame2.rgb, plain.rgb, "Hi-Z must never drop visible pixels");

    // A model placed inside the culled region would still be submitted by
    // the caller; the model itself draws through the same depth test, so a
    // model placed behind the wall contributes no pixels either.
    let mut zoom3 = ZoomState::new(70.0);
    let hidden = render_frame_with_hiz_and_zoom(
        &world,
        eye,
        0.0,
        0.0,
        160,
        90,
        &RenderConfig::default(),
        &mut zoom3,
        0.0,
        &[BakedModelDraw {
            quads: std::slice::from_ref(&quad),
            origin: [4.0, 4.0, 24.0],
            scale: 1.0,
        }],
    )
    .unwrap();
    assert_eq!(hidden.rgb, plain.rgb, "occluded model must not draw pixels");
}

#[test]
fn zoom_state_narrows_fov_and_scales_in_the_live_frame() {
    let mut world = wall_world();
    let eye = [16.0, 16.0, -50.0];
    rebuild(&mut world, [16.0, 16.0, -50.0], 0.0, 0.0);

    let wide = render_frame(&world, eye, 0.0, 0.0, 160, 90).unwrap();
    let mut zoom = ZoomState::new(70.0);
    zoom.set_zoomed(true);
    // Settle the exponential interpolation ~fully.
    let mut fov = 0.0f32;
    for _ in 0..60 {
        fov = zoom.tick(1.0 / 30.0);
    }
    assert!(fov < 70.0 / 3.0, "zoom should narrow FOV, got {fov}");

    let mut zoom2 = ZoomState::new(70.0);
    zoom2.set_zoomed(true);
    for _ in 0..60 {
        zoom2.tick(1.0 / 30.0);
    }
    let zoomed = render_frame_with_hiz_and_zoom(
        &world,
        eye,
        0.0,
        0.0,
        160,
        90,
        &RenderConfig::default(),
        &mut zoom2,
        0.0,
        &[],
    )
    .unwrap();
    // Zoomed-in the wall fills more of the frame: more pixels shaded.
    assert!(zoomed.stats.pixels_shaded > wide.stats.pixels_shaded);
    assert!(zoomed.rgb.as_chunks::<3>().0.iter().any(|p| *p != [168, 205, 240]));
}

#[test]
fn rasterize_baked_model_quads_produces_nonblank_pixels() {
    let quads = [baked_quad()];
    // Quad faces +z toward a camera looking +z: its normal points away, but
    // the software rasterizer shades both windings, so it draws.
    let frame = rasterize_baked_model_quads(
        &quads,
        [0.0, 0.0, 0.0],
        [0.5, 0.5, -4.0],
        0.0,
        0.0,
        64,
        64,
        70.0,
    )
    .unwrap();
    assert_eq!(frame.stats.model_quads_drawn, 1);
    assert!(frame.stats.pixels_shaded > 0);
    assert!(frame.rgb.as_chunks::<3>().0.iter().any(|p| *p != [168, 205, 240]));
}
