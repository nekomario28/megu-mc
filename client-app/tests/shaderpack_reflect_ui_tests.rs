// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane X1 coverage: per-pass GLSL reflection binding tables bridged into
//! [`ActiveShaderSession`] during [`activate_shaderpack`], plus the
//! plain-data [`ShaderpackUiSnapshot`] consumed by the shaderpack option
//! screen UI.
//!
//! Packs are deterministic in-memory [`ShaderPackSource`] maps; no test
//! touches the filesystem or real GPU resources.

use std::collections::BTreeSet;

use megu_client::lod::LodConfig;
use megu_client::render_graph::{FramePlanInput, GpuPipelineKind};
use megu_client::shaderpack_runtime::{
    activate_shaderpack, ActiveShaderSession, ShaderActivationOutcome,
    ShaderActivationRequest, ShaderpackUiSnapshot,
};
use megu_client::shaders::{HIZ_DOWNSAMPLE_COMPUTE_WGSL, TERRAIN_OPAQUE_WGSL};
use megu_client_mesh::shaderpack_reflect::{
    ImageAccess, SamplerSemantic, UniformCategory, WgslResourceKind,
};
use megu_client_mesh::{
    PackDimension, PipelineStage, ShaderOptionOverrides, ShaderPackFamily, ShaderPackSource,
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

fn request<'a>(pack_name: &'a str, locale: &'a str, profile: Option<String>) -> ShaderActivationRequest<'a> {
    ShaderActivationRequest {
        pack_name,
        dimension: PackDimension::Overworld,
        locale,
        overrides: ShaderOptionOverrides {
            profile,
            options: Default::default(),
        },
        lod_config: Some(LodConfig::default()),
    }
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

fn classic_pack() -> ShaderPackSource {
    source_from(&[
        (
            "shaders/shaders.properties",
            "screen = BLOOM SHADOW_RES\nsliders = SHADOW_RES\n",
        ),
        (
            "shaders/gbuffers_terrain.vsh",
            "#version 120\nuniform float viewWidth;\nvoid main(){ gl_Position = ftransform(); }\n",
        ),
        (
            "shaders/gbuffers_terrain.fsh",
            "#version 120\n/* DRAWBUFFERS:012 */\nuniform sampler2D gcolor;\nuniform sampler2D depthtex0;\nuniform float viewWidth;\nvoid main(){ gl_FragData[0] = texture2D(gcolor, vec2(0.0)); }\n",
        ),
        (
            "shaders/composite.vsh",
            "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
        ),
        (
            "shaders/composite.fsh",
            "#version 330 compatibility\n#define SHADOW_RES 512\nuniform sampler2D colortex0;\nuniform sampler2D shadowtex0;\nvoid main(){ gl_FragData[0] = texture(colortex0, vec2(0.0)); }\n",
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
        (
            "shaders/begin.csh",
            "#version 460 core\nconst ivec3 workGroups = ivec3(8, 8, 1);\nlayout(local_size_x = 16, local_size_y = 8) in;\nlayout(std430, binding = 3) readonly buffer VoxelData { vec4 positions[]; } voxelData;\nlayout(rgba16f, binding = 7) uniform readonly image2D historyImage;\nlayout(rgba8, binding = 8) writeonly uniform image2D voxelImg;\nvoid main(){}\n",
        ),
        ("shaders/shadow.vsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/shadow.fsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/shadowcomp.csh", "#version 460 core\nconst ivec3 workGroups = ivec3(4, 4, 1);\nvoid main(){}\n"),
        ("shaders/prepare.csh", "#version 460 core\nconst ivec3 workGroups = ivec3(4, 4, 1);\nvoid main(){}\n"),
        ("shaders/gbuffers_terrain.vsh", "#version 460 core\nuniform mat4 gbufferModelView;\nvoid main(){}\n"),
        (
            "shaders/gbuffers_terrain.fsh",
            "#version 460 core\n/* RENDERTARGETS:0,1,2,3 */\nuniform int heldItemId;\nuniform int heldBlockLightValue;\nuniform int heldItemId2;\nuniform int heldBlockLightValue2;\nuniform sampler2D colortex0;\nvoid main(){}\n",
        ),
        ("shaders/deferred.csh", "#version 460 core\nconst ivec3 workGroups = ivec3(4, 4, 1);\nvoid main(){}\n"),
        ("shaders/composite.vsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/composite.fsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/final.vsh", "#version 460 core\nvoid main(){}\n"),
        ("shaders/final.fsh", "#version 460 core\nvoid main(){}\n"),
    ])
}

fn dh_pack() -> ShaderPackSource {
    source_from(&[
        ("shaders/shadow.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/shadow.fsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_shadow.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_shadow.fsh", "#version 330 core\nuniform sampler2D shadowtex0;\nvoid main(){}\n"),
        ("shaders/gbuffers_terrain.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/gbuffers_terrain.fsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_terrain.vsh", "#version 330 core\nuniform float dhFarPlane;\nvoid main(){}\n"),
        (
            "shaders/dh_terrain.fsh",
            "#version 330 core\nuniform sampler2D dhDepthTex0;\nuniform sampler2D colortex4;\nvoid main(){}\n",
        ),
        ("shaders/dh_water.vsh", "#version 330 core\nvoid main(){}\n"),
        ("shaders/dh_water.fsh", "#version 330 core\nuniform sampler2D dhDepthTex1;\nvoid main(){}\n"),
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

#[test]
fn classic_glsl120_pack_reflects_gcolor_depthtex0_and_merges_program_units() {
    let req = request("classic-120", "en_US", None);
    let session = active_session(&classic_pack(), GpuPipelineKind::Classic, &req);
    assert_eq!(session.family, ShaderPackFamily::ClassicOptiFine);
    assert_eq!(
        session
            .reflected_passes
            .iter()
            .map(|p| p.pass_name.as_str())
            .collect::<Vec<_>>(),
        session
            .ordered_pass_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );

    let terrain = session
        .reflected_passes
        .iter()
        .find(|p| p.pass_name == "gbuffers_terrain")
        .expect("gbuffers_terrain reflection");
    assert_eq!(terrain.stage, PipelineStage::Gbuffers);
    assert_eq!(terrain.bindings.glsl_version, Some(120));

    // Legacy aliases classify onto attachments: gcolor -> color0,
    // depthtex0 -> depth0.
    let gcolor = terrain
        .bindings
        .samplers
        .iter()
        .find(|s| s.name == "gcolor")
        .expect("gcolor sampler");
    assert_eq!(gcolor.semantic, SamplerSemantic::Color(0));
    let depth = terrain
        .bindings
        .samplers
        .iter()
        .find(|s| s.name == "depthtex0")
        .expect("depthtex0 sampler");
    assert_eq!(depth.semantic, SamplerSemantic::Depth(0));
    assert!(terrain.attachments.color.contains(&0));
    assert!(terrain.attachments.depth.contains(&0));

    // viewWidth declared in both vertex and fragment units deduplicates to
    // one program-level uniform with the Camera category.
    let view_width: Vec<_> = terrain
        .bindings
        .uniforms
        .iter()
        .filter(|u| u.name == "viewWidth")
        .collect();
    assert_eq!(view_width.len(), 1);
    assert_eq!(view_width[0].category, UniformCategory::Camera);

    // WGSL table: each sampled texture gets a texture + sampler pair in
    // group 1, with unique slots across the table.
    let texture = terrain
        .wgsl_table
        .entries
        .iter()
        .find(|e| e.name == "gcolor")
        .expect("gcolor wgsl entry");
    assert_eq!(texture.group, 1);
    assert_eq!(texture.kind, WgslResourceKind::SampledTexture);
    assert!(terrain
        .wgsl_table
        .entries
        .iter()
        .any(|e| e.name == "gcolor__sampler" && e.kind == WgslResourceKind::Sampler));
    let slots: BTreeSet<_> = terrain
        .wgsl_table
        .entries
        .iter()
        .map(|e| (e.group, e.binding))
        .collect();
    assert_eq!(slots.len(), terrain.wgsl_table.entries.len());

    // Composite picks up shadowtex0 + colortex0, classified GLSL 330.
    let composite = session
        .reflected_passes
        .iter()
        .find(|p| p.pass_name == "composite")
        .expect("composite reflection");
    assert_eq!(composite.bindings.glsl_version, Some(330));
    assert!(composite.attachments.color.contains(&0));
    assert!(composite.attachments.shadow_depth.contains(&0));
}

#[test]
fn iris460_pack_reflects_compute_workgroup_ssbo_storage_image_and_dual_hand() {
    let req = request("iris-460", "en_US", None);
    let session = active_session(&nextgen_iris_pack(), GpuPipelineKind::MeshShader, &req);
    assert_eq!(session.family, ShaderPackFamily::IrisModern);
    assert!(session.dual_hand_light);

    let begin = session
        .reflected_passes
        .iter()
        .find(|p| p.pass_name == "begin")
        .expect("begin compute reflection");
    assert_eq!(begin.stage, PipelineStage::Begin);
    assert_eq!(begin.bindings.glsl_version, Some(460));
    assert_eq!(begin.bindings.workgroup_size, Some([16, 8, 1]));

    // SSBO + explicit binding is carried into group 2.
    let ssbo = begin
        .bindings
        .storage_buffers
        .iter()
        .find(|b| b.block_name == "VoxelData")
        .expect("VoxelData SSBO");
    assert_eq!(ssbo.declared_binding, Some(3));
    assert!(ssbo.readonly);
    assert_eq!(ssbo.instance_name.as_deref(), Some("voxelData"));
    let ssbo_entry = begin
        .wgsl_table
        .entries
        .iter()
        .find(|e| e.name == "VoxelData")
        .expect("VoxelData wgsl entry");
    assert_eq!(ssbo_entry.group, 2);
    assert_eq!(ssbo_entry.binding, 3);
    assert_eq!(ssbo_entry.kind, WgslResourceKind::StorageBuffer);

    // Storage images keep format + access and land in group 3 on their
    // explicit GLSL bindings.
    let history = begin
        .bindings
        .storage_images
        .iter()
        .find(|i| i.name == "historyImage")
        .expect("historyImage");
    assert_eq!(history.format.as_deref(), Some("rgba16f"));
    assert_eq!(history.access, ImageAccess::ReadOnly);
    let voxel = begin
        .bindings
        .storage_images
        .iter()
        .find(|i| i.name == "voxelImg")
        .expect("voxelImg");
    assert_eq!(voxel.format.as_deref(), Some("rgba8"));
    assert_eq!(voxel.access, ImageAccess::WriteOnly);
    for (name, binding) in [("historyImage", 7u32), ("voxelImg", 8u32)] {
        let entry = begin
            .wgsl_table
            .entries
            .iter()
            .find(|e| e.name == name)
            .expect("storage image entry");
        assert_eq!((entry.group, entry.binding), (3, binding), "{name}");
        assert_eq!(entry.kind, WgslResourceKind::StorageImage);
    }

    // Dual-hand lighting uniforms categorize on the gbuffers pass.
    let terrain = session
        .reflected_passes
        .iter()
        .find(|p| p.pass_name == "gbuffers_terrain")
        .expect("gbuffers_terrain reflection");
    for name in [
        "heldItemId",
        "heldBlockLightValue",
        "heldItemId2",
        "heldBlockLightValue2",
    ] {
        let uniform = terrain
            .bindings
            .uniforms
            .iter()
            .find(|u| u.name == name)
            .unwrap_or_else(|| panic!("{name} uniform"));
        assert_eq!(uniform.category, UniformCategory::HandLighting, "{name}");
    }
    assert!(terrain.attachments.color.contains(&0));

    // Every reflected pass has a collision-free WGSL slot allocation.
    for pass in &session.reflected_passes {
        let slots: BTreeSet<_> = pass
            .wgsl_table
            .entries
            .iter()
            .map(|e| (e.group, e.binding))
            .collect();
        assert_eq!(
            slots.len(),
            pass.wgsl_table.entries.len(),
            "{}",
            pass.pass_name
        );
        assert_eq!(pass.attachments, pass.wgsl_table.attachments);
    }
}

#[test]
fn distant_horizons_pack_reflects_dh_depth_and_lod_stage_passes() {
    let req = request("dh-hybrid", "en_US", None);
    let session = active_session(&dh_pack(), GpuPipelineKind::ComputeIndirect, &req);
    assert_eq!(session.family, ShaderPackFamily::DistantHorizonsHybrid);

    // dh_shadow draws during the shadow-map phase; dh_terrain/dh_water run
    // in the DistantHorizons stage after gbuffers.
    for (name, stage) in [
        ("dh_shadow", PipelineStage::Shadow),
        ("dh_terrain", PipelineStage::DistantHorizons),
        ("dh_water", PipelineStage::DistantHorizons),
    ] {
        let pass = session
            .reflected_passes
            .iter()
            .find(|p| p.pass_name == name)
            .unwrap_or_else(|| panic!("{name} reflection"));
        assert_eq!(pass.stage, stage, "{name}");
    }

    let terrain = session
        .reflected_passes
        .iter()
        .find(|p| p.pass_name == "dh_terrain")
        .expect("dh_terrain reflection");
    let dh_depth = terrain
        .bindings
        .samplers
        .iter()
        .find(|s| s.name == "dhDepthTex0")
        .expect("dhDepthTex0 sampler");
    assert_eq!(dh_depth.semantic, SamplerSemantic::DistantHorizonsDepth(0));
    assert!(terrain.attachments.dh_depth.contains(&0));
    assert!(terrain.attachments.color.contains(&4));
    // dhFarPlane is a DistantHorizons-category uniform, not a sampler.
    let far_plane = terrain
        .bindings
        .uniforms
        .iter()
        .find(|u| u.name == "dhFarPlane")
        .expect("dhFarPlane uniform");
    assert_eq!(far_plane.category, UniformCategory::DistantHorizons);

    let water = session
        .reflected_passes
        .iter()
        .find(|p| p.pass_name == "dh_water")
        .expect("dh_water reflection");
    assert!(water.attachments.dh_depth.contains(&1));
}

#[test]
fn megu_native_wgsl_pack_skips_glsl_reflection_entirely() {
    let req = request("megu-native", "en_US", None);
    let session = active_session(&megu_wgsl_pack(), GpuPipelineKind::MeshShader, &req);
    assert_eq!(session.family, ShaderPackFamily::MeguNativeWgsl);
    assert!(session.reflected_passes.is_empty());
    assert_eq!(session.ordered_pass_names, vec!["hiz_cull", "terrain_opaque"]);

    // The UI snapshot still exports cleanly for a WGSL pack.
    let snapshot = session.ui_snapshot();
    assert_eq!(snapshot.pack_name, "Megu Native Fast");
    assert_eq!(snapshot.family, Some(ShaderPackFamily::MeguNativeWgsl));
    assert!(snapshot.options.is_empty());
    assert!(snapshot.profiles.is_empty());
    assert!(snapshot.screens.is_empty());
    assert!(snapshot.fallback_notice.is_none());
}

#[test]
fn ui_snapshot_localizes_options_profiles_screens_and_values() {
    let pack = source_from(&[
        (
            "shaders/shaders.properties",
            "profile.LOW = !BLOOM SHADOW_RES=1024\nprofile.HIGH = BLOOM SHADOW_RES=2048\nscreen = BLOOM SHADOW_RES [detail]\nscreen.detail = FOG_QUALITY\nsliders = SHADOW_RES\n",
        ),
        (
            "shaders/lang/en_US.lang",
            "option.BLOOM=Bloom\noption.SHADOW_RES=Shadow Resolution\noption.FOG_QUALITY=Fog Quality\nvalue.SHADOW_RES.512=Low\nvalue.SHADOW_RES.1024=Medium\nvalue.SHADOW_RES.2048=High\nprofile.LOW=Low\nprofile.HIGH=High Quality\nscreen.detail=Detail Settings\n",
        ),
        (
            "shaders/lang/ja_JP.lang",
            "option.BLOOM=ブルーム\nprofile.HIGH=高画質\nscreen.detail=詳細設定\n",
        ),
        (
            "shaders/gbuffers_terrain.vsh",
            "#version 120\nvoid main(){ gl_Position = ftransform(); }\n",
        ),
        (
            "shaders/gbuffers_terrain.fsh",
            "#version 120\nvoid main(){ gl_FragData[0] = vec4(1.0); }\n",
        ),
        (
            "shaders/composite.vsh",
            "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
        ),
        (
            "shaders/composite.fsh",
            "#version 330 compatibility\n// #define BLOOM\n#define SHADOW_RES 512\n#define FOG_QUALITY 2\nvoid main(){ gl_FragData[0] = vec4(1.0); }\n",
        ),
        (
            "shaders/final.vsh",
            "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
        ),
        (
            "shaders/final.fsh",
            "#version 330 compatibility\nvoid main(){ gl_FragColor = vec4(1.0); }\n",
        ),
    ]);
    let req = request("localized-pack", "ja_JP", Some("HIGH".to_owned()));
    let session = active_session(&pack, GpuPipelineKind::Classic, &req);
    let snapshot = session.ui_snapshot();

    assert_eq!(snapshot.pack_name, "localized-pack");
    assert_eq!(snapshot.family, Some(ShaderPackFamily::ClassicOptiFine));
    assert_eq!(snapshot.locale, "ja_JP");
    assert!(snapshot.fallback_notice.is_none());

    // Option rows: localized label where translated, en_US fallback
    // otherwise; current values reflect the applied HIGH profile.
    let bloom = snapshot
        .options
        .iter()
        .find(|o| o.key == "BLOOM")
        .expect("BLOOM option");
    assert_eq!(bloom.label, "ブルーム");
    assert_eq!(bloom.current_value.as_deref(), Some(""));
    assert!(bloom.values.is_empty());
    assert!(!bloom.is_slider);

    let shadow_res = snapshot
        .options
        .iter()
        .find(|o| o.key == "SHADOW_RES")
        .expect("SHADOW_RES option");
    assert_eq!(shadow_res.label, "Shadow Resolution");
    assert_eq!(shadow_res.current_value.as_deref(), Some("2048"));
    assert!(shadow_res.is_slider);
    assert_eq!(
        shadow_res
            .values
            .iter()
            .map(|v| v.value.as_str())
            .collect::<Vec<_>>(),
        vec!["512", "1024", "2048"]
    );
    assert_eq!(shadow_res.values[1].label, "Medium");

    let fog = snapshot
        .options
        .iter()
        .find(|o| o.key == "FOG_QUALITY")
        .expect("FOG_QUALITY option");
    assert_eq!(fog.label, "Fog Quality");
    assert_eq!(fog.current_value.as_deref(), Some("2"));

    // Profiles carry resolved option maps in override convention.
    assert_eq!(
        snapshot
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        vec!["HIGH", "LOW"]
    );
    let high = &snapshot.profiles[0];
    assert_eq!(high.label, "高画質");
    assert_eq!(high.options.get("BLOOM"), Some(&Some(String::new())));
    assert_eq!(
        high.options.get("SHADOW_RES"),
        Some(&Some("2048".to_owned()))
    );
    let low = &snapshot.profiles[1];
    assert_eq!(low.label, "Low");
    assert_eq!(low.options.get("BLOOM"), Some(&None));
    assert_eq!(
        low.options.get("SHADOW_RES"),
        Some(&Some("1024".to_owned()))
    );

    // Screens: root first with raw entry order preserved, then named
    // sub-screens with localized labels.
    assert_eq!(snapshot.screens.len(), 2);
    assert_eq!(snapshot.screens[0].name, "");
    assert_eq!(
        snapshot.screens[0].entries,
        vec!["BLOOM", "SHADOW_RES", "[detail]"]
    );
    assert_eq!(snapshot.screens[1].name, "detail");
    assert_eq!(snapshot.screens[1].label, "詳細設定");
    assert_eq!(snapshot.screens[1].entries, vec!["FOG_QUALITY"]);

    // The merged lang table drops straight into the UI's label map.
    assert_eq!(
        snapshot.labels.get("option.BLOOM").map(String::as_str),
        Some("ブルーム")
    );
    assert_eq!(
        snapshot.labels.get("profile.HIGH").map(String::as_str),
        Some("高画質")
    );
}

#[test]
fn fallback_outcome_exports_banner_snapshot_without_options() {
    // A compute/SSBO/custom-image pack cannot run on legacy Intel HD; the
    // outcome snapshot carries the non-jargon banner instead of panicking.
    let req = request("iris-460", "en_US", None);
    let outcome = activate_shaderpack(
        &nextgen_iris_pack(),
        GpuPipelineKind::ClassicLowSpec,
        &frame_input(),
        &req,
    );
    assert!(outcome.is_fallback());

    let snapshot = outcome.ui_snapshot();
    assert_eq!(snapshot.pack_name, "iris-460");
    assert_eq!(snapshot.family, Some(ShaderPackFamily::IrisModern));
    assert!(snapshot.options.is_empty());
    assert!(snapshot.profiles.is_empty());
    assert!(snapshot.screens.is_empty());
    let notice = snapshot.fallback_notice.expect("fallback banner");
    assert!(notice.contains("built-in graphics"), "{notice}");
    assert!(!notice.contains("SSBO"));

    // Direct builder path for hosts that only keep the outcome fields.
    let direct = ShaderpackUiSnapshot::fallback("broken-pack", None, "Using built-in graphics instead.");
    assert_eq!(direct.pack_name, "broken-pack");
    assert_eq!(direct.family, None);
    assert_eq!(
        direct.fallback_notice.as_deref(),
        Some("Using built-in graphics instead.")
    );
    assert!(direct.labels.is_empty());
}
