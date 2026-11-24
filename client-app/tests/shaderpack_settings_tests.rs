// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane S1 coverage: `shader_pack`/`shader_profile` TOML + CLI plumbing and
//! RenderGraph integration for universal external shaderpacks and Megu
//! native WGSL packs.
//!
//! Every assertion is deterministic: packs are built from in-memory
//! [`ShaderPackSource`] maps, so no test touches the filesystem beyond the
//! per-process scratch directory used for settings-file precedence.

use std::path::PathBuf;

use megu_client::cli::parse_args;
use megu_client::lod::LodConfig;
use megu_client::render::{RenderConfig, ShaderpackResolveInput};
use megu_client::render_graph::{
    plan_frame, plan_frame_with_shaderpack, FramePlanInput, GpuPipelineKind,
};
use megu_client::settings::{
    is_valid_shader_selector, ClientSettings, QualityPreset,
};
use megu_client::shaderpack_runtime::{
    activate_shaderpack, ActiveShaderSession, ShaderActivationOutcome,
    ShaderActivationRequest,
};
use megu_client::shaders::{HIZ_DOWNSAMPLE_COMPUTE_WGSL, TERRAIN_OPAQUE_WGSL};
use megu_client_mesh::{
    PackDimension, PassKind, PipelineStage, ShaderOptionOverrides, ShaderPackFamily,
    ShaderPackSource,
};

fn frame_input() -> FramePlanInput {
    FramePlanInput {
        width: 1920,
        height: 1080,
        visible_sections: 120,
        translucent_sections: 12,
        meshlet_count: 2048,
        block_entity_instances: 64,
        block_entity_mesh_kinds: 4,
    }
}

fn source_from(entries: &[(&str, &str)]) -> ShaderPackSource {
    ShaderPackSource::from_memory(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec())),
    )
    .expect("memory shaderpack")
}

fn activation_request<'a>(pack_name: &'a str, profile: Option<String>) -> ShaderActivationRequest<'a> {
    ShaderActivationRequest {
        pack_name,
        dimension: PackDimension::Overworld,
        locale: "en_US",
        overrides: ShaderOptionOverrides {
            profile,
            options: Default::default(),
        },
        lod_config: Some(LodConfig::default()),
    }
}

fn classic_optifine_pack() -> ShaderPackSource {
    source_from(&[
        (
            "shaders/shaders.properties",
            "profile.LOW = SHADOW_RES=1024\nprofile.HIGH = SHADOW_RES=2048\n",
        ),
        (
            "shaders/gbuffers_terrain.vsh",
            "#version 120\nvoid main(){ gl_Position = ftransform(); }\n",
        ),
        (
            "shaders/gbuffers_terrain.fsh",
            "#version 120\n/* DRAWBUFFERS:012 */\nvoid main(){ gl_FragData[0] = vec4(1.0); }\n",
        ),
        (
            "shaders/composite.vsh",
            "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
        ),
        (
            "shaders/composite.fsh",
            "#version 330 compatibility\n#define SHADOW_RES 512\nuniform sampler2D colortex0;\nvoid main(){ gl_FragData[0] = texture(colortex0, vec2(0.0)); }\n",
        ),
        (
            "shaders/final.vsh",
            "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
        ),
        (
            "shaders/final.fsh",
            "#version 330 compatibility\nuniform sampler2D colortex0;\nvoid main(){ gl_FragColor = texture(colortex0, vec2(0.0)); }\n",
        ),
    ])
}

fn nextgen_iris_pack() -> ShaderPackSource {
    source_from(&[
        (
            "shaders/shaders.properties",
            "iris.features.required = COMPUTE_SHADERS SSBO CUSTOM_IMAGES\nbufferObject.0 = 65536\nimage.voxel = voxelImg rgba16f rgba16f float true false 64 64 64\n",
        ),
        ("shaders/item.properties", "item.42 = minecraft:torch\n"),
        ("shaders/begin.csh", "#version 460 core\nconst ivec3 workGroups = ivec3(8, 8, 1);\nvoid main(){}\n"),
        ("shaders/shadow.vsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/shadow.fsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/shadowcomp.csh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/prepare.csh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/gbuffers_terrain.vsh", "#version 460 core\nvoid main(){}\n"),
        (
            "shaders/gbuffers_terrain.fsh",
            "#version 460 core\n/* RENDERTARGETS:0,1,2,3 */\nvoid main(){}\n",
        ),
        ("shaders/deferred.csh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/composite.vsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/composite.fsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/final.vsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/final.fsh", "#version 460 core\nvoid main(){}\n"),
    ])
}

fn dh_lod_pack() -> ShaderPackSource {
    source_from(&[
        ("shaders/shadow.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/shadow.fsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_shadow.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_shadow.fsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/gbuffers_terrain.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/gbuffers_terrain.fsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_terrain.vsh", "#version 330 core\nvoid main(){}\n"),
        (
            "shaders/dh_terrain.fsh",
            "#version 330 core\nuniform sampler2D dhDepthTex0;\nvoid main(){}\n",
        ),
        ("shaders/dh_water.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_water.fsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/final.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/final.fsh", "#version 330 core\nvoid main(){}\n"),
    ])
}

fn megu_wgsl_pack() -> ShaderPackSource {
    source_from(&[
        (
            "megu-shader.toml",
            r#"
[pack]
name = "Megu Native Fast"
author = "megu"
version = "1.0.0"
api = 1
supports_lod = true

[pass.terrain_opaque]
stage = "gbuffers"
kind = "graphics"
file = "terrain.wgsl"
targets = ["color0"]

[pass.hiz_cull]
stage = "begin"
kind = "compute"
file = "hiz.wgsl"
workgroups = [16, 16, 1]
"#,
        ),
        ("terrain.wgsl", TERRAIN_OPAQUE_WGSL),
        ("hiz.wgsl", HIZ_DOWNSAMPLE_COMPUTE_WGSL),
    ])
}

fn active_session(
    source: &ShaderPackSource,
    pipeline: GpuPipelineKind,
    req: &ShaderActivationRequest<'_>,
) -> ActiveShaderSession {
    let outcome = activate_shaderpack(source, pipeline, &frame_input(), req);
    let ShaderActivationOutcome::Active(session) = outcome else {
        panic!("expected pack to activate on {pipeline:?}: {outcome:?}");
    };
    *session
}

fn pack_pass_names(plan: &megu_client::render_graph::RenderGraphPlan) -> Vec<&str> {
    plan.shaderpack_passes.iter().map(|p| p.name.as_str()).collect()
}

// ---------------------------------------------------------------------------
// Settings: defaults, TOML roundtrip, graceful rejection
// ---------------------------------------------------------------------------

#[test]
fn shader_selectors_default_none_on_every_preset_and_omit_from_toml() {
    for preset in [
        QualityPreset::Fast,
        QualityPreset::Balanced,
        QualityPreset::High,
        QualityPreset::Ultra,
        QualityPreset::Custom,
    ] {
        let s = ClientSettings::from_preset(preset);
        assert_eq!(s.shader_pack, None, "{preset:?}");
        assert_eq!(s.shader_profile, None, "{preset:?}");
        let text = s.to_toml();
        assert!(!text.contains("shader_pack"), "{preset:?}: {text}");
        assert!(!text.contains("shader_profile"), "{preset:?}: {text}");
        // Files written before the keys existed parse to None with no warning.
        let (back, warnings) = ClientSettings::from_toml(&text);
        assert_eq!(back, s, "{preset:?}");
        assert!(warnings.is_empty(), "{preset:?}: {warnings:?}");
    }
}

#[test]
fn shader_selectors_toml_roundtrip_and_reject_traversal_gracefully() {
    let s = ClientSettings {
        shader_pack: Some("shaderpacks/ComplementaryUnbound.zip".to_owned()),
        shader_profile: Some("HIGH".to_owned()),
        ..ClientSettings::default()
    };
    let text = s.to_toml();
    assert!(text.contains("shader_pack = \"shaderpacks/ComplementaryUnbound.zip\""));
    assert!(text.contains("shader_profile = \"HIGH\""));
    let (back, warnings) = ClientSettings::from_toml(&text);
    assert_eq!(back, s);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // Whitespace is normalized; ordinary nested paths and names containing
    // ".." as a substring (not a path component) are still fine.
    let (s, warnings) = ClientSettings::from_toml(
        "shader_pack = \"  shaderpacks/a..b.zip  \"\nshader_profile = 'MEDIUM'\n",
    );
    assert_eq!(s.shader_pack.as_deref(), Some("shaderpacks/a..b.zip"));
    assert_eq!(s.shader_profile.as_deref(), Some("MEDIUM"));
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // Empty strings and `..` path components warn and fall back to None.
    for (key, doc) in [
        ("shader_pack", "shader_pack = \"\"\n"),
        ("shader_pack", "shader_pack = \"   \"\n"),
        ("shader_pack", "shader_pack = \"../escape.zip\"\n"),
        ("shader_pack", "shader_pack = \"packs/../../escape.zip\"\n"),
        ("shader_pack", "shader_pack = \"..\\\\escape.zip\"\n"),
        ("shader_profile", "shader_profile = \"\"\n"),
        ("shader_profile", "shader_profile = \"..\"\n"),
        ("shader_profile", "shader_profile = \"../profiles/HIGH\"\n"),
    ] {
        let (s, warnings) = ClientSettings::from_toml(doc);
        assert_eq!(s.shader_pack, None, "{doc}");
        assert_eq!(s.shader_profile, None, "{doc}");
        assert!(
            warnings.iter().any(|w| w.contains(key)),
            "{doc}: expected warning for {key}, got {warnings:?}"
        );
    }

    // apply_toml overlays only defined keys; a malformed value warns and
    // leaves an existing valid selection untouched.
    let mut s = ClientSettings {
        shader_pack: Some("shaderpacks/good.zip".to_owned()),
        ..ClientSettings::default()
    };
    s.apply_toml("view_distance = 20\n");
    assert_eq!(s.shader_pack.as_deref(), Some("shaderpacks/good.zip"));
    let warnings = s.apply_toml("shader_pack = \"../evil\"\n");
    assert_eq!(s.shader_pack.as_deref(), Some("shaderpacks/good.zip"));
    assert!(warnings.iter().any(|w| w.contains("shader_pack")));
    s.apply_toml("shader_profile = \"LOW\"\n");
    assert_eq!(s.shader_profile.as_deref(), Some("LOW"));

    // The shared selector helper follows the same contract.
    assert!(is_valid_shader_selector("shaderpacks/x.zip"));
    assert!(!is_valid_shader_selector(""));
    assert!(!is_valid_shader_selector("   "));
    assert!(!is_valid_shader_selector("../x"));
    assert!(!is_valid_shader_selector("a/../x"));
    assert!(is_valid_shader_selector("a..b"));
}

// ---------------------------------------------------------------------------
// CLI: flags, precedence, fail-closed validation
// ---------------------------------------------------------------------------

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-shaderpack-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn shader_flags_parse_follow_precedence_and_fail_closed() {
    let dir = scratch_dir("flags");

    // Flag alone applies to the resolved settings like every other
    // explicit override.
    let mut cfg = parse_args(&[
        "--shader-pack",
        "shaderpacks/cli.zip",
        "--shader-profile",
        "HIGH",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.shader_pack.as_deref(), Some("shaderpacks/cli.zip"));
    assert_eq!(cfg.shader_profile.as_deref(), Some("HIGH"));
    assert_eq!(
        cfg.client_settings.shader_pack.as_deref(),
        Some("shaderpacks/cli.zip")
    );
    assert_eq!(
        cfg.client_settings.shader_profile.as_deref(),
        Some("HIGH")
    );
    assert_eq!(cfg.client_settings.preset, QualityPreset::Custom);

    // Standard precedence: preset base -> TOML keys -> explicit flags.
    let file = dir.join("custom.toml");
    std::fs::write(
        &file,
        "shader_pack = \"shaderpacks/file.zip\"\nshader_profile = \"LOW\"\n",
    )
    .unwrap();
    let mut cfg = parse_args(&["--settings", file.to_str().unwrap()]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(
        cfg.client_settings.shader_pack.as_deref(),
        Some("shaderpacks/file.zip")
    );
    assert_eq!(
        cfg.client_settings.shader_profile.as_deref(),
        Some("LOW")
    );

    let mut cfg = parse_args(&[
        "--preset",
        "fast",
        "--settings",
        file.to_str().unwrap(),
        "--shader-pack",
        "shaderpacks/cli-wins.zip",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(
        cfg.client_settings.shader_pack.as_deref(),
        Some("shaderpacks/cli-wins.zip")
    );
    // The TOML key the flag does not cover still applies.
    assert_eq!(
        cfg.client_settings.shader_profile.as_deref(),
        Some("LOW")
    );

    // Inline `--flag=value` form works too.
    let cfg = parse_args(&["--shader-pack=shaderpacks/inline.zip"]).unwrap();
    assert_eq!(cfg.shader_pack.as_deref(), Some("shaderpacks/inline.zip"));

    // Empty or `..` traversal values fail closed with a useful message.
    for argv in [
        vec!["--shader-pack", ""],
        vec!["--shader-pack", "../evil.zip"],
        vec!["--shader-pack", "packs/../../evil.zip"],
        vec!["--shader-pack="],
        vec!["--shader-profile", ""],
        vec!["--shader-profile", ".."],
        vec!["--shader-profile", "../x"],
    ] {
        let err = parse_args(&argv).unwrap_err();
        assert!(err.contains("invalid --shader"), "{argv:?}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// plan_frame_with_shaderpack: layering across all four pack families
// ---------------------------------------------------------------------------

#[test]
fn vanilla_plan_frame_carries_no_shaderpack_layer() {
    for kind in GpuPipelineKind::ALL {
        let plan = plan_frame(kind, &frame_input());
        assert!(plan.shaderpack_passes.is_empty(), "{kind:?}");
        assert_eq!(plan.active_shaderpack, None, "{kind:?}");
    }
}

#[test]
fn classic_optifine_pack_layers_ordered_graphics_passes() {
    let pack = classic_optifine_pack();
    let req = activation_request("classic-sample", Some("HIGH".to_owned()));
    let session = active_session(&pack, GpuPipelineKind::Classic, &req);
    assert_eq!(session.family, ShaderPackFamily::ClassicOptiFine);

    let plan = plan_frame_with_shaderpack(GpuPipelineKind::Classic, &frame_input(), &session);
    assert_eq!(plan.active_shaderpack.as_deref(), Some("classic-sample"));
    assert_eq!(
        pack_pass_names(&plan),
        session
            .ordered_pass_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        pack_pass_names(&plan),
        vec!["gbuffers_terrain", "composite", "final"]
    );

    // Base vanilla passes stay in canonical order underneath the layer.
    let vanilla = plan_frame(GpuPipelineKind::Classic, &frame_input());
    assert_eq!(plan.passes.len(), vanilla.passes.len());
    assert!(plan
        .passes
        .iter()
        .zip(vanilla.passes.iter())
        .all(|(a, b)| a.id == b.id));

    let stages: Vec<_> = plan
        .shaderpack_passes
        .iter()
        .map(|p| (p.name.as_str(), p.stage, p.kind))
        .collect();
    assert_eq!(
        stages,
        vec![
            ("gbuffers_terrain", PipelineStage::Gbuffers, PassKind::Graphics),
            ("composite", PipelineStage::Composite, PassKind::Graphics),
            ("final", PipelineStage::Final, PassKind::Graphics),
        ]
    );
    // gbuffers re-issues per-section draws on the classic submission path;
    // fullscreen composite/final cost one draw each.
    assert_eq!(
        plan.shaderpack_passes[0].estimated_draw_calls,
        frame_input().visible_sections
    );
    assert_eq!(plan.shaderpack_passes[1].estimated_draw_calls, 1);
    assert_eq!(plan.shaderpack_passes[2].estimated_draw_calls, 1);
    // Budget layering: totals exceed the vanilla plan's.
    assert!(plan.total_estimated_draw_calls() > vanilla.total_estimated_draw_calls());
    assert!(plan.vram_estimate_bytes() > vanilla.vram_estimate_bytes());

    // The profile override still flows through activation into the plan.
    let again = plan_frame_with_shaderpack(GpuPipelineKind::Classic, &frame_input(), &session);
    assert_eq!(pack_pass_names(&again), pack_pass_names(&plan));
    assert_eq!(
        again.total_estimated_draw_calls(),
        plan.total_estimated_draw_calls()
    );
}

#[test]
fn nextgen_iris_pack_layers_compute_and_graphics_passes() {
    let pack = nextgen_iris_pack();
    let req = activation_request("nextgen-ray-style", None);
    let session = active_session(&pack, GpuPipelineKind::MeshShader, &req);
    assert_eq!(session.family, ShaderPackFamily::IrisModern);

    let plan = plan_frame_with_shaderpack(GpuPipelineKind::MeshShader, &frame_input(), &session);
    assert_eq!(plan.active_shaderpack.as_deref(), Some("nextgen-ray-style"));
    assert_eq!(
        pack_pass_names(&plan),
        vec![
            "begin",
            "shadow",
            "shadowcomp",
            "prepare",
            "gbuffers_terrain",
            "deferred",
            "composite",
            "final"
        ]
    );

    let by_name = |name: &str| {
        plan.shaderpack_passes
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("missing pack pass {name}"))
    };
    // `.csh` programs plan as compute dispatches with declared workgroups.
    for name in ["begin", "shadowcomp", "prepare", "deferred"] {
        assert_eq!(by_name(name).kind, PassKind::Compute, "{name}");
    }
    assert_eq!(
        by_name("begin").work_groups,
        Some([8, 8, 1]),
        "workgroup counts come from `const ivec3 workGroups`"
    );
    assert_eq!(by_name("begin").estimated_draw_calls, 8 * 8);
    assert_eq!(by_name("begin").stage, PipelineStage::Begin);
    assert_eq!(by_name("shadowcomp").stage, PipelineStage::Shadow);
    assert_eq!(by_name("prepare").stage, PipelineStage::Prepare);
    assert_eq!(by_name("deferred").stage, PipelineStage::Deferred);
    // Graphics programs keep their stages and render-target lists.
    assert_eq!(by_name("shadow").kind, PassKind::Graphics);
    assert_eq!(by_name("shadow").stage, PipelineStage::Shadow);
    assert_eq!(by_name("gbuffers_terrain").stage, PipelineStage::Gbuffers);
    assert_eq!(
        by_name("gbuffers_terrain").render_targets,
        vec!["0", "1", "2", "3"]
    );
    assert_eq!(by_name("composite").stage, PipelineStage::Composite);
    assert_eq!(by_name("final").stage, PipelineStage::Final);
}

#[test]
fn dh_lod_pack_layers_shadow_and_distant_horizon_stages() {
    let pack = dh_lod_pack();
    let req = activation_request("dh-hybrid", None);
    let session = active_session(&pack, GpuPipelineKind::ComputeIndirect, &req);
    assert_eq!(session.family, ShaderPackFamily::DistantHorizonsHybrid);

    let plan =
        plan_frame_with_shaderpack(GpuPipelineKind::ComputeIndirect, &frame_input(), &session);
    assert_eq!(
        pack_pass_names(&plan),
        vec![
            "shadow",
            "dh_shadow",
            "gbuffers_terrain",
            "dh_terrain",
            "dh_water",
            "final"
        ]
    );
    let by_name = |name: &str| {
        plan.shaderpack_passes
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("missing pack pass {name}"))
    };
    assert_eq!(by_name("shadow").stage, PipelineStage::Shadow);
    assert_eq!(by_name("dh_shadow").stage, PipelineStage::Shadow);
    assert_eq!(by_name("gbuffers_terrain").stage, PipelineStage::Gbuffers);
    assert_eq!(by_name("dh_terrain").stage, PipelineStage::DistantHorizons);
    assert_eq!(by_name("dh_water").stage, PipelineStage::DistantHorizons);
    assert_eq!(by_name("final").stage, PipelineStage::Final);
    // Stage ordering is non-decreasing across the layered sequence.
    let stages: Vec<_> = plan
        .shaderpack_passes
        .iter()
        .map(|p| p.stage)
        .collect();
    let sorted = {
        let mut s = stages.clone();
        s.sort_unstable();
        s
    };
    assert_eq!(stages, sorted);
}

#[test]
fn megu_native_wgsl_pack_layers_toml_passes() {
    let pack = megu_wgsl_pack();
    let req = activation_request("megu-native", None);
    let session = active_session(&pack, GpuPipelineKind::MeshShader, &req);
    assert_eq!(session.family, ShaderPackFamily::MeguNativeWgsl);
    assert_eq!(session.pack_name, "Megu Native Fast");

    let plan = plan_frame_with_shaderpack(GpuPipelineKind::MeshShader, &frame_input(), &session);
    assert_eq!(plan.active_shaderpack.as_deref(), Some("Megu Native Fast"));
    assert_eq!(pack_pass_names(&plan), vec!["hiz_cull", "terrain_opaque"]);
    let hiz = &plan.shaderpack_passes[0];
    assert_eq!(hiz.kind, PassKind::Compute);
    assert_eq!(hiz.stage, PipelineStage::Begin);
    assert_eq!(hiz.work_groups, Some([16, 16, 1]));
    assert_eq!(hiz.estimated_draw_calls, 16 * 16);
    let terrain = &plan.shaderpack_passes[1];
    assert_eq!(terrain.kind, PassKind::Graphics);
    assert_eq!(terrain.stage, PipelineStage::Gbuffers);
    assert_eq!(terrain.render_targets, vec!["color0"]);
}

// ---------------------------------------------------------------------------
// render.rs wiring: RenderConfig -> resolve_shaderpack -> safe fallback
// ---------------------------------------------------------------------------

#[test]
fn resolve_shaderpack_vanilla_path_uses_builtin_plan() {
    let config = RenderConfig::default();
    let input = frame_input();
    for kind in GpuPipelineKind::ALL {
        let state = config.resolve_shaderpack(
            kind,
            &input,
            &ShaderpackResolveInput::default(),
        );
        assert!(state.session.is_none(), "{kind:?}");
        assert_eq!(state.user_notice, None, "{kind:?}");
        assert!(state.missing_capabilities.is_empty(), "{kind:?}");
        assert!(state.frame_plan.shaderpack_passes.is_empty(), "{kind:?}");
        assert_eq!(
            state.frame_plan.total_estimated_draw_calls(),
            plan_frame(kind, &input).total_estimated_draw_calls(),
            "{kind:?}"
        );
    }
}

#[test]
fn resolve_shaderpack_active_pack_layers_plan() {
    let pack = megu_wgsl_pack();
    let settings = ClientSettings {
        shader_pack: Some("shaderpacks/megu-native".to_owned()),
        ..ClientSettings::default()
    };
    let config = RenderConfig::from_settings(&settings);
    assert_eq!(
        config.shader_pack.as_deref(),
        Some("shaderpacks/megu-native"),
        "from_settings carries the selection 1:1"
    );

    let input = frame_input();
    let state = config.resolve_shaderpack(
        GpuPipelineKind::MeshShader,
        &input,
        &ShaderpackResolveInput {
            source: Some(&pack),
            dimension: PackDimension::Overworld,
            locale: "en_US",
            lod_config: Some(LodConfig::default()),
        },
    );
    let session = state.session.expect("pack activates on MeshShader");
    assert_eq!(session.pack_name, "Megu Native Fast");
    assert_eq!(state.user_notice, None);
    assert_eq!(
        pack_pass_names(&state.frame_plan),
        vec!["hiz_cull", "terrain_opaque"]
    );
    assert_eq!(
        state.frame_plan.active_shaderpack.as_deref(),
        Some("Megu Native Fast")
    );
}

#[test]
fn resolve_shaderpack_falls_back_cleanly_on_classic_low_spec() {
    let pack = nextgen_iris_pack();
    let settings = ClientSettings {
        shader_pack: Some("shaderpacks/nextgen-ray-style.zip".to_owned()),
        gpu_backend: megu_client_mesh::RenderBackendPreference::ClassicLowSpec,
        ..ClientSettings::default()
    };
    let config = RenderConfig::from_settings(&settings);
    let input = frame_input();

    // Activating a compute/SSBO/GLSL-460 pack on legacy Intel HD never
    // panics: the state surfaces a clean notice and runs the built-in
    // ClassicLowSpec plan untouched.
    let state = config.resolve_shaderpack(
        GpuPipelineKind::ClassicLowSpec,
        &input,
        &ShaderpackResolveInput {
            source: Some(&pack),
            dimension: PackDimension::Overworld,
            locale: "en_US",
            lod_config: None,
        },
    );
    assert!(state.session.is_none());
    let notice = state.user_notice.expect("fallback surfaces user_notice");
    assert!(notice.contains("built-in rendering"), "{notice}");
    assert!(notice.contains("nextgen-ray-style"), "{notice}");
    assert!(!state.missing_capabilities.is_empty());
    assert!(state
        .missing_capabilities
        .iter()
        .any(|m| m.contains("compute")));
    let vanilla = plan_frame(GpuPipelineKind::ClassicLowSpec, &input);
    assert!(state.frame_plan.shaderpack_passes.is_empty());
    assert_eq!(state.frame_plan.pipeline, vanilla.pipeline);
    assert_eq!(
        state
            .frame_plan
            .passes
            .iter()
            .map(|p| p.id)
            .collect::<Vec<_>>(),
        vanilla.passes.iter().map(|p| p.id).collect::<Vec<_>>()
    );

    // The same pack resolves Active on a capable pipeline.
    let state = config.resolve_shaderpack(
        GpuPipelineKind::MeshShader,
        &input,
        &ShaderpackResolveInput {
            source: Some(&pack),
            dimension: PackDimension::Overworld,
            locale: "en_US",
            lod_config: None,
        },
    );
    assert!(state.session.is_some());
    assert_eq!(state.user_notice, None);
    assert_eq!(state.frame_plan.shaderpack_passes.len(), 8);

    // A pack selected but unreadable from disk degrades the same way.
    let state = config.resolve_shaderpack(
        GpuPipelineKind::MeshShader,
        &input,
        &ShaderpackResolveInput::default(),
    );
    assert!(state.session.is_none());
    let notice = state.user_notice.expect("unreadable source notices");
    assert!(notice.contains("built-in rendering"), "{notice}");
    assert!(!state.missing_capabilities.is_empty());

    // The profile selector flows into the activation overrides.
    let settings = ClientSettings {
        shader_pack: Some("shaderpacks/megu-native".to_owned()),
        shader_profile: Some("LOW".to_owned()),
        ..ClientSettings::default()
    };
    let config = RenderConfig::from_settings(&settings);
    assert_eq!(config.shader_profile.as_deref(), Some("LOW"));
}

#[test]
fn resolve_shaderpack_profile_flows_into_activation() {
    let pack = classic_optifine_pack();
    let settings = ClientSettings {
        shader_pack: Some("shaderpacks/classic.zip".to_owned()),
        shader_profile: Some("HIGH".to_owned()),
        ..ClientSettings::default()
    };
    let config = RenderConfig::from_settings(&settings);
    let input = frame_input();
    let state = config.resolve_shaderpack(
        GpuPipelineKind::Classic,
        &input,
        &ShaderpackResolveInput {
            source: Some(&pack),
            ..ShaderpackResolveInput::default()
        },
    );
    let session = state.session.expect("classic pack activates");
    let megu_client::shaderpack_runtime::LoadedPackKind::IrisOptiFine(plan) = &session.pack
    else {
        panic!("expected IrisOptiFine plan");
    };
    // `profile.HIGH` rewrote SHADOW_RES inside the composite program.
    let comp = plan
        .passes
        .iter()
        .find(|p| p.name == "composite")
        .expect("composite pass");
    let frag = &comp.shaders[&megu_client_mesh::ShaderKind::Fragment];
    assert!(frag.source.contains("#define SHADOW_RES 2048"));
}
