// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Clean-room universal external shaderpack runtime bridge for `client-app`.
//!
//! Unifies four specification-based shaderpack families under one bounded,
//! deterministic activation pipeline (zero third-party shaders bundled):
//!
//! 1. **Classic OptiFine packs** (GLSL 120..330, `gbuffers_*`/`composite*`/
//!    `final`, `colortex`/`depthtex`/`shadowtex`, `DRAWBUFFERS`, `lang/*.lang`).
//! 2. **Next-gen Iris 1.7..1.11+ packs** (GLSL 430..460, `.csh` compute,
//!    `bufferObject.*` SSBOs, `image.*` custom images, `begin`/`shadowcomp`/
//!    `prepare`/`deferred`, `RENDERTARGETS`, dual-hand light,
//!    `block/item/entity.properties`).
//! 3. **DistantHorizons / LOD hybrid packs** (`dh_terrain`/`dh_water`/
//!    `dh_shadow` bound to Megu's native [`LodConfig`] horizon renderer).
//! 4. **Megu native WGSL packs** (`megu-shader.toml` + `.wgsl` passes validated
//!    by [`crate::shaders::validate_wgsl_module`] with zero GLSL translation).
//!
//! Any pack whose required GPU capabilities exceed the active [`GpuPipelineKind`]
//! (for example a compute/SSBO pack selected on a legacy Intel HD
//! `ClassicLowSpec` GPU) fails closed to [`ShaderActivationOutcome::FallbackToVanilla`]
//! with human-readable missing-capability diagnostics — never crashing.

use mc_spine_client_mesh::{
    is_megu_wgsl_pack, load_megu_wgsl_pack, plan_shader_pipeline_with_options, GpuShaderCaps,
    MeguWgslPack, PackDimension, PassKind, PipelineStage, ShaderOptionOverrides, ShaderPackFamily,
    ShaderPackLang, ShaderPackSource, ShaderPipelinePlan,
};

use crate::lod::LodConfig;
use crate::render_graph::{plan_frame, FramePlanInput, GpuPipelineKind, RenderGraphPlan};
use crate::shaders::{validate_wgsl_module, WgslStage};

/// Derive [`GpuShaderCaps`] from `client-app`'s resolved [`GpuPipelineKind`].
///
/// * `MeshShader` / `ComputeIndirect`: modern Vulkan/Metal/DX12 profile
///   (GLSL 460, compute, SSBO, custom images, 128 MiB SSBO, 8 color targets).
/// * `Classic`: desktop graphics-only profile (GLSL 410, 8 color targets, no
///   compute/SSBO/images) — runs classic OptiFine packs smoothly.
/// * `ClassicLowSpec`: legacy Intel HD / minimal iGPU profile (GLSL 410, 4
///   color targets, no compute/SSBO/images).
pub fn gpu_shader_caps_for_pipeline(pipeline: GpuPipelineKind) -> GpuShaderCaps {
    match pipeline {
        GpuPipelineKind::MeshShader | GpuPipelineKind::ComputeIndirect => {
            GpuShaderCaps::modern_compute()
        }
        GpuPipelineKind::Classic => GpuShaderCaps {
            max_glsl_version: 410,
            supports_compute: false,
            supports_ssbo: false,
            supports_custom_images: false,
            max_ssbo_bytes: 0,
            max_color_attachments: 8,
        },
        GpuPipelineKind::ClassicLowSpec => GpuShaderCaps::legacy_opengl(),
    }
}

/// Player dual-hand dynamic light state exported to Iris/OptiFine shader
/// uniforms (`heldItemId`, `heldBlockLightValue`, `heldItemId2`,
/// `heldBlockLightValue2`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DualHandLightState {
    pub main_hand_item_id: i32,
    pub main_hand_light: u8,
    pub off_hand_item_id: i32,
    pub off_hand_light: u8,
}

impl DualHandLightState {
    /// Resolve item material IDs from an active pack's `item.properties` map
    /// (falling back to `-1` when unmapped, matching Iris/OptiFine convention).
    pub fn from_held_items(
        plan: &ShaderPipelinePlan,
        main_hand_id: Option<&str>,
        main_hand_light: u8,
        off_hand_id: Option<&str>,
        off_hand_light: u8,
    ) -> Self {
        let lookup = |item: Option<&str>| -> i32 {
            let Some(name) = item else {
                return -1;
            };
            plan.item_ids
                .as_ref()
                .and_then(|map| map.lookup(name, &std::collections::BTreeMap::new()))
                .map_or(-1, |id| id as i32)
        };
        Self {
            main_hand_item_id: lookup(main_hand_id),
            main_hand_light: main_hand_light.min(15),
            off_hand_item_id: lookup(off_hand_id),
            off_hand_light: off_hand_light.min(15),
        }
    }
}

/// How an active shaderpack integrates with Megu's native horizon LOD system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LodShaderIntegration {
    /// Distant horizon LOD is turned off in client settings.
    Disabled,
    /// The pack supplies native LOD shading passes (`dh_terrain`, `dh_water`,
    /// `dh_shadow` or a Megu WGSL `lod` stage pass).
    PackCustomLod {
        terrain_pass: Option<String>,
        water_pass: Option<String>,
        shadow_pass: Option<String>,
    },
    /// Distant horizon LOD is active, and the pack defines no custom LOD
    /// passes — Megu's built-in LOD renderer draws distant terrain cleanly.
    MeguNativeLodFallback,
}

/// Which concrete pack representation is active inside [`ActiveShaderSession`].
#[derive(Clone, Debug)]
pub enum LoadedPackKind {
    /// External OptiFine / Iris / DistantHorizons GLSL pack.
    IrisOptiFine(Box<ShaderPipelinePlan>),
    /// Megu native WGSL pack (zero GLSL translation overhead).
    MeguWgsl(MeguWgslPack),
}

/// A validated, ready-to-execute shaderpack session bound to a GPU pipeline
/// and frame render graph.
#[derive(Clone, Debug)]
pub struct ActiveShaderSession {
    /// Display name of the active pack.
    pub pack_name: String,
    /// Detected architecture family.
    pub family: ShaderPackFamily,
    /// Dimension whose program overrides were planned.
    pub dimension: PackDimension,
    /// Localized option/profile/screen strings (`lang/<locale>.lang` + `en_US`).
    pub lang: ShaderPackLang,
    /// How distant horizon LOD interacts with this pack.
    pub lod_integration: LodShaderIntegration,
    /// Whether the pack requests dual-hand dynamic light uniforms.
    pub dual_hand_light: bool,
    /// Ordered pass names across all active stages.
    pub ordered_pass_names: Vec<String>,
    /// Underlying Megu render graph for geometry submission & VRAM budgeting.
    pub base_render_graph: RenderGraphPlan,
    /// Underlying planned pack (OptiFine/Iris or Megu native WGSL).
    pub pack: LoadedPackKind,
}

/// Result of attempting to activate a shaderpack against the host GPU pipeline.
#[derive(Clone, Debug)]
pub enum ShaderActivationOutcome {
    /// The pack passed every capability and structural check and is active.
    Active(Box<ActiveShaderSession>),
    /// The pack could not run on this GPU or failed validation; rendering
    /// safely falls back to Megu's built-in vanilla pipeline without crashing.
    FallbackToVanilla {
        /// Requested pack name.
        pack_name: String,
        /// Detected family, if parsing reached classification.
        family: Option<ShaderPackFamily>,
        /// Deterministic list of unmet GPU capabilities or validation errors.
        missing_capabilities: Vec<String>,
        /// Player-friendly explanation shown in settings/toast.
        user_notice: String,
        /// Ready-to-run built-in vanilla render graph.
        vanilla_graph: RenderGraphPlan,
    },
}

impl ShaderActivationOutcome {
    /// True when the requested pack activated cleanly.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    /// True when the runtime safely fell back to vanilla rendering.
    pub fn is_fallback(&self) -> bool {
        matches!(self, Self::FallbackToVanilla { .. })
    }
}

/// Options controlling shaderpack activation.
#[derive(Clone, Debug)]
pub struct ShaderActivationRequest<'a> {
    /// Human-readable pack name (e.g. filename or directory stem).
    pub pack_name: &'a str,
    /// Active dimension (`world0`, `world-1`, `world1`, or custom).
    pub dimension: PackDimension,
    /// UI locale for `lang/*.lang` resolution (e.g. `"ja_JP"`, `"en_US"`).
    pub locale: &'a str,
    /// Selected `profile.<NAME>` and explicit option overrides.
    pub overrides: ShaderOptionOverrides,
    /// Active distant-horizon LOD config, if enabled.
    pub lod_config: Option<LodConfig>,
}

/// Activate an external OptiFine/Iris shaderpack or Megu native WGSL pack
/// against `pipeline` and `frame_input`, with automatic fail-closed fallback
/// to vanilla rendering when the host GPU lacks required capabilities.
pub fn activate_shaderpack(
    source: &ShaderPackSource,
    pipeline: GpuPipelineKind,
    frame_input: &FramePlanInput,
    req: &ShaderActivationRequest<'_>,
) -> ShaderActivationOutcome {
    let caps = gpu_shader_caps_for_pipeline(pipeline);
    let base_graph = plan_frame(pipeline, frame_input);
    let lang = source.load_lang(req.locale);

    if is_megu_wgsl_pack(source) {
        return activate_megu_wgsl(source, &caps, base_graph, lang, req);
    }

    let plan = match plan_shader_pipeline_with_options(source, req.dimension, &caps, &req.overrides)
    {
        Ok(p) => p,
        Err(err) => {
            let reason = err.to_string();
            return ShaderActivationOutcome::FallbackToVanilla {
                pack_name: req.pack_name.to_owned(),
                family: None,
                missing_capabilities: vec![reason.clone()],
                user_notice: format!(
                    "Shaderpack '{}' could not be loaded ({reason}); switched back to built-in rendering.",
                    req.pack_name
                ),
                vanilla_graph: base_graph,
            };
        }
    };

    if !plan.is_compatible() {
        let missing = plan.compatibility.missing().to_vec();
        let summary = missing.join("; ");
        return ShaderActivationOutcome::FallbackToVanilla {
            pack_name: req.pack_name.to_owned(),
            family: Some(plan.family),
            missing_capabilities: missing,
            user_notice: format!(
                "Shaderpack '{}' is not supported on this GPU ({summary}); switched back to built-in rendering.",
                req.pack_name
            ),
            vanilla_graph: base_graph,
        };
    }

    let lod_integration = resolve_iris_lod_integration(&plan, req.lod_config);
    let dual_hand_light = plan.uniforms.uses_dual_hand_light();
    let ordered_pass_names = plan.passes.iter().map(|p| p.name.clone()).collect();
    let family = plan.family;

    ShaderActivationOutcome::Active(Box::new(ActiveShaderSession {
        pack_name: req.pack_name.to_owned(),
        family,
        dimension: req.dimension,
        lang,
        lod_integration,
        dual_hand_light,
        ordered_pass_names,
        base_render_graph: base_graph,
        pack: LoadedPackKind::IrisOptiFine(Box::new(plan)),
    }))
}

fn activate_megu_wgsl(
    source: &ShaderPackSource,
    caps: &GpuShaderCaps,
    base_graph: RenderGraphPlan,
    lang: ShaderPackLang,
    req: &ShaderActivationRequest<'_>,
) -> ShaderActivationOutcome {
    let wgsl_pack = match load_megu_wgsl_pack(source, caps) {
        Ok(p) => p,
        Err(err) => {
            let reason = err.to_string();
            return ShaderActivationOutcome::FallbackToVanilla {
                pack_name: req.pack_name.to_owned(),
                family: Some(ShaderPackFamily::MeguNativeWgsl),
                missing_capabilities: vec![reason.clone()],
                user_notice: format!(
                    "WGSL shaderpack '{}' failed to load ({reason}); switched back to built-in rendering.",
                    req.pack_name
                ),
                vanilla_graph: base_graph,
            };
        }
    };

    let mut missing = wgsl_pack.compatibility.missing().to_vec();
    for pass in &wgsl_pack.passes {
        let expected_stages: &[WgslStage] = match pass.kind {
            PassKind::Compute => &[WgslStage::Compute],
            PassKind::Graphics => &[WgslStage::Vertex, WgslStage::Fragment],
        };
        if let Err(err) = validate_wgsl_module(&pass.wgsl_source, expected_stages) {
            missing.push(format!("WGSL validation failed in {}: {err}", pass.file));
        }
    }
    if !missing.is_empty() {
        let summary = missing.join("; ");
        return ShaderActivationOutcome::FallbackToVanilla {
            pack_name: wgsl_pack.name,
            family: Some(ShaderPackFamily::MeguNativeWgsl),
            missing_capabilities: missing,
            user_notice: format!(
                "WGSL shaderpack '{}' is not supported on this GPU ({summary}); switched back to built-in rendering.",
                req.pack_name
            ),
            vanilla_graph: base_graph,
        };
    }

    let lod_integration = match req.lod_config {
        None => LodShaderIntegration::Disabled,
        Some(_) => {
            let lod_pass = wgsl_pack
                .passes
                .iter()
                .find(|p| p.stage == PipelineStage::DistantHorizons)
                .map(|p| p.name.clone());
            if lod_pass.is_some() || wgsl_pack.supports_lod {
                LodShaderIntegration::PackCustomLod {
                    terrain_pass: lod_pass,
                    water_pass: None,
                    shadow_pass: wgsl_pack
                        .passes
                        .iter()
                        .find(|p| p.stage == PipelineStage::Shadow)
                        .map(|p| p.name.clone()),
                }
            } else {
                LodShaderIntegration::MeguNativeLodFallback
            }
        }
    };
    let ordered_pass_names = wgsl_pack.passes.iter().map(|p| p.name.clone()).collect();
    let display_name = wgsl_pack.name.clone();

    ShaderActivationOutcome::Active(Box::new(ActiveShaderSession {
        pack_name: display_name,
        family: ShaderPackFamily::MeguNativeWgsl,
        dimension: req.dimension,
        lang,
        lod_integration,
        dual_hand_light: false,
        ordered_pass_names,
        base_render_graph: base_graph,
        pack: LoadedPackKind::MeguWgsl(wgsl_pack),
    }))
}

fn resolve_iris_lod_integration(
    plan: &ShaderPipelinePlan,
    lod_config: Option<LodConfig>,
) -> LodShaderIntegration {
    if lod_config.is_none() {
        return LodShaderIntegration::Disabled;
    }
    let terrain_pass = plan
        .passes
        .iter()
        .find(|p| p.name == "dh_terrain")
        .map(|p| p.name.clone());
    let water_pass = plan
        .passes
        .iter()
        .find(|p| p.name == "dh_water")
        .map(|p| p.name.clone());
    let shadow_pass = plan
        .passes
        .iter()
        .find(|p| p.name == "dh_shadow")
        .map(|p| p.name.clone());
    if terrain_pass.is_some() || water_pass.is_some() || shadow_pass.is_some() {
        LodShaderIntegration::PackCustomLod {
            terrain_pass,
            water_pass,
            shadow_pass,
        }
    } else {
        LodShaderIntegration::MeguNativeLodFallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shaders::{HIZ_DOWNSAMPLE_COMPUTE_WGSL, TERRAIN_OPAQUE_WGSL};

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

    #[test]
    fn classic_optifine_pack_runs_on_classic_gpu_with_multilingual_ui_and_profile_overrides() {
        let pack = source_from(&[
            (
                "shaders/shaders.properties",
                "profile.LOW = !BLOOM SHADOW_RES=1024\nprofile.HIGH = BLOOM SHADOW_RES=2048\n",
            ),
            (
                "shaders/lang/en_US.lang",
                "option.BLOOM=Bloom\noption.SHADOW_RES=Shadow Resolution\nprofile.HIGH=High Quality\n",
            ),
            (
                "shaders/lang/ja_JP.lang",
                "option.BLOOM=ブルーム\nprofile.HIGH=高画質\n",
            ),
            (
                "shaders/gbuffers_terrain.vsh",
                "#version 120\nvoid main(){ gl_Position = ftransform(); }\n",
            ),
            (
                "shaders/gbuffers_terrain.fsh",
                "#version 120\n/* DRAWBUFFERS:012 */\nuniform sampler2D gcolor;\nuniform sampler2D depthtex0;\nvoid main(){ gl_FragData[0] = texture2D(gcolor, vec2(0.0)); }\n",
            ),
            (
                "shaders/composite.vsh",
                "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
            ),
            (
                "shaders/composite.fsh",
                "#version 330 compatibility\n// #define BLOOM\n#define SHADOW_RES 512\nuniform sampler2D colortex0;\nuniform sampler2D shadowtex0;\nvoid main(){ gl_FragData[0] = texture(colortex0, vec2(0.0)); }\n",
            ),
            (
                "shaders/final.vsh",
                "#version 330 compatibility\nvoid main(){ gl_Position = vec4(0.0); }\n",
            ),
            (
                "shaders/final.fsh",
                "#version 330 compatibility\nuniform sampler2D colortex0;\nvoid main(){ gl_FragColor = texture(colortex0, vec2(0.0)); }\n",
            ),
        ]);

        let req = ShaderActivationRequest {
            pack_name: "classic-sample",
            dimension: PackDimension::Overworld,
            locale: "ja_JP",
            overrides: ShaderOptionOverrides {
                profile: Some("HIGH".to_owned()),
                options: Default::default(),
            },
            lod_config: Some(LodConfig::default()),
        };

        let outcome = activate_shaderpack(&pack, GpuPipelineKind::Classic, &frame_input(), &req);
        let ShaderActivationOutcome::Active(session) = outcome else {
            panic!("expected classic pack to activate on Classic GPU: {outcome:?}");
        };
        assert_eq!(session.family, ShaderPackFamily::ClassicOptiFine);
        assert_eq!(session.lang.option_label("BLOOM"), "ブルーム");
        // Missing Japanese key falls back cleanly to en_US.
        assert_eq!(session.lang.option_label("SHADOW_RES"), "Shadow Resolution");
        assert_eq!(session.lang.profile_label("HIGH"), "高画質");
        assert_eq!(
            session.lod_integration,
            LodShaderIntegration::MeguNativeLodFallback
        );
        assert_eq!(
            session.ordered_pass_names,
            vec!["gbuffers_terrain", "composite", "final"]
        );

        // Verify HIGH profile enabled BLOOM and overrode SHADOW_RES=2048.
        let LoadedPackKind::IrisOptiFine(plan) = &session.pack else {
            panic!("expected IrisOptiFine plan");
        };
        let comp = plan
            .passes
            .iter()
            .find(|p| p.name == "composite")
            .expect("composite pass");
        let frag = &comp.shaders[&mc_spine_client_mesh::ShaderKind::Fragment];
        assert!(frag.source.contains("#define BLOOM"));
        assert!(frag.source.contains("#define SHADOW_RES 2048"));
        assert!(plan.uniforms.color_attachments.contains(&0));
        assert!(plan.uniforms.depth_attachments.contains(&0));
        assert!(plan.uniforms.shadow_depth_attachments.contains(&0));
    }

    #[test]
    fn nextgen_iris_compute_ssbo_pack_activates_on_modern_gpu_and_falls_back_on_legacy_intel() {
        let pack = source_from(&[
            (
                "shaders/shaders.properties",
                "iris.features.required = COMPUTE_SHADERS SSBO CUSTOM_IMAGES\nbufferObject.0 = 65536\nimage.voxel = voxelImg rgba16f rgba16f float true false 64 64 64\n",
            ),
            ("shaders/item.properties", "item.42 = minecraft:torch\nitem.99 = minecraft:lantern\n"),
            ("shaders/begin.csh", "#version 460 core\nconst ivec3 workGroups = ivec3(8, 8, 1);\nvoid main(){}\n"),
            ("shaders/shadow.vsh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/shadow.fsh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/shadowcomp.csh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/prepare.csh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/gbuffers_terrain.vsh", "#version 460 core\nvoid main(){}\n"),
            (
                "shaders/gbuffers_terrain.fsh",
                "#version 460 core\n/* RENDERTARGETS:0,1,2,3 */\nuniform int heldItemId;\nuniform int heldBlockLightValue;\nuniform int heldItemId2;\nuniform int heldBlockLightValue2;\nvoid main(){}\n",
            ),
            ("shaders/deferred.csh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/composite.vsh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/composite.fsh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/final.vsh", "#version 460 core\nvoid main(){}\n"),
            ("shaders/final.fsh", "#version 460 core\nvoid main(){}\n"),
        ]);

        let req = ShaderActivationRequest {
            pack_name: "nextgen-ray-style",
            dimension: PackDimension::Overworld,
            locale: "en_US",
            overrides: ShaderOptionOverrides::default(),
            lod_config: None,
        };

        // Modern MeshShader GPU runs it cleanly.
        let modern = activate_shaderpack(&pack, GpuPipelineKind::MeshShader, &frame_input(), &req);
        let ShaderActivationOutcome::Active(session) = modern else {
            panic!("expected nextgen pack to activate on MeshShader: {modern:?}");
        };
        assert_eq!(session.family, ShaderPackFamily::IrisModern);
        assert!(session.dual_hand_light);
        assert_eq!(
            session.ordered_pass_names,
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
        let LoadedPackKind::IrisOptiFine(plan) = &session.pack else {
            panic!("expected IrisOptiFine");
        };
        let hands = DualHandLightState::from_held_items(
            plan,
            Some("minecraft:torch"),
            14,
            Some("minecraft:lantern"),
            15,
        );
        assert_eq!(hands.main_hand_item_id, 42);
        assert_eq!(hands.off_hand_item_id, 99);
        assert_eq!(hands.main_hand_light, 14);
        assert_eq!(hands.off_hand_light, 15);

        // Legacy Intel HD (ClassicLowSpec) safely falls back to vanilla without crashing.
        let legacy =
            activate_shaderpack(&pack, GpuPipelineKind::ClassicLowSpec, &frame_input(), &req);
        let ShaderActivationOutcome::FallbackToVanilla {
            pack_name,
            family,
            missing_capabilities,
            user_notice,
            vanilla_graph,
        } = legacy
        else {
            panic!("expected ClassicLowSpec fallback for compute/SSBO pack");
        };
        assert_eq!(pack_name, "nextgen-ray-style");
        assert_eq!(family, Some(ShaderPackFamily::IrisModern));
        assert!(missing_capabilities.iter().any(|m| m.contains("compute")));
        assert!(missing_capabilities.iter().any(|m| m.contains("SSBO")));
        assert!(missing_capabilities.iter().any(|m| m.contains("GLSL 460")));
        assert!(user_notice.contains("built-in rendering"));
        assert_eq!(vanilla_graph.pipeline, GpuPipelineKind::ClassicLowSpec);
        assert!(!vanilla_graph.passes.is_empty());
    }

    #[test]
    fn distant_horizons_lod_pack_orders_dh_shadow_terrain_water_and_binds_lod() {
        let pack = source_from(&[
            ("shaders/shadow.vsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/shadow.fsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/dh_shadow.vsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/dh_shadow.fsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/gbuffers_terrain.vsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/gbuffers_terrain.fsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/dh_terrain.vsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/dh_terrain.fsh", "#version 330 core\nuniform sampler2D dhDepthTex0;\nvoid main(){}\n"),
            ("shaders/dh_water.vsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/dh_water.fsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/final.vsh", "#version 330 core\nvoid main(){}\n"),
            ("shaders/final.fsh", "#version 330 core\nvoid main(){}\n"),
        ]);
        let req = ShaderActivationRequest {
            pack_name: "dh-hybrid",
            dimension: PackDimension::Overworld,
            locale: "en_US",
            overrides: ShaderOptionOverrides::default(),
            lod_config: Some(LodConfig::default()),
        };
        let outcome =
            activate_shaderpack(&pack, GpuPipelineKind::ComputeIndirect, &frame_input(), &req);
        let ShaderActivationOutcome::Active(session) = outcome else {
            panic!("expected DH hybrid pack to activate: {outcome:?}");
        };
        assert_eq!(session.family, ShaderPackFamily::DistantHorizonsHybrid);
        assert_eq!(
            session.ordered_pass_names,
            vec![
                "shadow",
                "dh_shadow",
                "gbuffers_terrain",
                "dh_terrain",
                "dh_water",
                "final"
            ]
        );
        assert_eq!(
            session.lod_integration,
            LodShaderIntegration::PackCustomLod {
                terrain_pass: Some("dh_terrain".to_owned()),
                water_pass: Some("dh_water".to_owned()),
                shadow_pass: Some("dh_shadow".to_owned()),
            }
        );
    }

    #[test]
    fn megu_native_wgsl_pack_validates_modules_and_falls_back_when_compute_missing() {
        let pack = source_from(&[
            (
                "megu-shader.toml",
                r#"
[pack]
name = "Megu Native Fast"
author = "mc-spine"
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
        ]);
        let req = ShaderActivationRequest {
            pack_name: "megu-native",
            dimension: PackDimension::Overworld,
            locale: "ja_JP",
            overrides: ShaderOptionOverrides::default(),
            lod_config: Some(LodConfig::default()),
        };
        let modern = activate_shaderpack(&pack, GpuPipelineKind::MeshShader, &frame_input(), &req);
        let ShaderActivationOutcome::Active(session) = modern else {
            panic!("expected native WGSL pack to activate on MeshShader: {modern:?}");
        };
        assert_eq!(session.family, ShaderPackFamily::MeguNativeWgsl);
        assert_eq!(session.pack_name, "Megu Native Fast");
        assert_eq!(session.ordered_pass_names, vec!["hiz_cull", "terrain_opaque"]);
        assert!(matches!(
            session.lod_integration,
            LodShaderIntegration::PackCustomLod { .. }
        ));

        // On ClassicLowSpec, the compute WGSL pass triggers a clean fallback to vanilla.
        let legacy =
            activate_shaderpack(&pack, GpuPipelineKind::ClassicLowSpec, &frame_input(), &req);
        assert!(legacy.is_fallback());
    }
}
