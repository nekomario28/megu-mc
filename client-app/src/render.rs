// SPDX-License-Identifier: LGPL-3.0-or-later
//! Zero-dependency 3D software rasterizer + RFC 2083 PNG encoder.
//!
//! Consumes the packed 16-byte `PackedVertex` buffers and `sort_translucent`
//! quad indices produced by `client-port`, projects them through a perspective
//! camera, shades with face normal + AO + sky/block light and a deterministic
//! block-ID color palette, then alpha-blends translucent quads back-to-front.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::vfx_engine::{
    Vfx1Packet, VfxBudget, VfxCodecError, VfxFrameOutput, VfxHardwareTier, VfxWorld,
};
use megu_client_mesh::{
    aabb_distance_sq, box_facing_mask, bucket_draw_mask, depth_key, BakedModelQuad,
    DirectionBuckets, DrawOrder, GraphCuller, HiZPyramid, MultidrawScratch,
    RenderBackendPreference, RenderRegions, SectionCoord, DIRECTION_BUCKETS,
    DIRECTION_BUCKET_UNASSIGNED,
};

use megu_client_mesh::{PackDimension, ShaderOptionOverrides, ShaderPackSource};
use megu_client_mesh::ber_instancing::{
    build_ber_mesh, VanillaBerFacing, VanillaBerKind, VanillaBerState, VanillaParticleQuad,
    VanillaParticleWorld,
};
use megu_client_mesh::entity_rig::{build_entity_rig_mesh, classify_entity_rig, EntityPose};
use megu_client_mesh::vanilla_plus_3d::ExtrudedMesh;
use megu_kernel::debug_chords::DebugLine;

use crate::audio_qol::ZoomState;
use crate::lod::LodConfig;
use crate::protocol::{zlib_compress_stored, DecodedParticleOptions, VisualLevelEvent};
use crate::render_graph::{
    plan_frame, plan_frame_with_shaderpack, plan_frame_with_vfx_tier,
    vfx_tier_for_pipeline, FramePlanInput, GpuPipelineKind, RenderGraphPlan,
};
use crate::settings::{ClientSettings, CloudMode, ParticleLevel};
use crate::shaderpack_runtime::{
    activate_shaderpack, ActiveShaderSession, ShaderActivationOutcome, ShaderActivationRequest,
};
use crate::world::{
    BlockEntityVisualState, EntityVisualState, LevelParticleSpawn, TrackedBossBar, WorldState,
};

const NEAR_Z: f64 = 0.08;
const FAR_Z: f64 = 1024.0;
const DEFAULT_FOV_DEG: f64 = 70.0;

// Packed vertex `attributes` bit layout (see megu_client_mesh.h).
const ATTR_DIRECTION_MASK: u32 = 0x7;
const ATTR_AO_SHIFT: u32 = 3;
const ATTR_BLOCK_LIGHT_SHIFT: u32 = 5;
const ATTR_SKY_LIGHT_SHIFT: u32 = 9;
const ATTR_LAYER_SHIFT: u32 = 13;
const ATTR_FLUID_KIND_SHIFT: u32 = 15;
const ATTR_DIAGONAL_BIT: u32 = 1 << 17;

// client-port layers.
const LAYER_TRANSLUCENT: u32 = 2;
const LAYER_FLUID: u32 = 3;

const FLUID_WATER: u32 = 1;
const FLUID_LAVA: u32 = 2;

/// Everything [`render_frame_with_config`] honors from `ClientSettings`.
/// FOV, smooth lighting and the draw-distance cap take effect in this
/// renderer; the remaining fields are carried 1:1 so a windowed frontend
/// can honor them without re-mapping (the headless rasterizer has no
/// cloud, particle, entity, or present pass to apply them to).
#[derive(Clone, Debug, PartialEq)]
pub struct RenderConfig {
    /// Vertical field of view in degrees.
    pub fov_deg: f64,
    /// Per-vertex ambient occlusion; off keeps flat per-face shading only.
    pub smooth_lighting: bool,
    /// Chebyshev chunk-column draw cap around the camera; `None` draws
    /// everything the visibility culler reaches.
    pub view_distance: Option<i32>,
    /// Cloud drawing preference (no cloud pass in the headless rasterizer).
    pub clouds: CloudMode,
    /// Particle emission preference (no particle pass in the headless rasterizer).
    pub particles: ParticleLevel,
    /// Entity shadow preference (no entity pass in the headless rasterizer).
    pub entity_shadows: bool,
    /// VSync preference for windowed presentation.
    pub vsync: bool,
    /// Frame pacing cap for windowed presentation (`None` = uncapped).
    pub max_fps: Option<u32>,
    /// Frame pacing cap while the window is unfocused (`0` = no
    /// background throttling).
    pub background_fps_limit: u16,
    /// GPU pipeline selection forwarded to the hardware backend;
    /// `classic-low-spec` pins the minimal-hardware path for legacy Intel
    /// integrated GPUs (pure wgpu Vulkan 1.1 / DX12 / Metal core).
    pub gpu_backend: RenderBackendPreference,
    /// Selected shaderpack path forwarded to the shaderpack runtime;
    /// `None` keeps built-in rendering.
    pub shader_pack: Option<String>,
    /// Optional `profile.<NAME>` inside the pack's `shaders.properties` to
    /// activate on load.
    pub shader_profile: Option<String>,
}

impl RenderConfig {
    /// Map a resolved `ClientSettings` onto the renderer: `view_distance`
    /// becomes the full-detail draw cap and `fov` the projection angle.
    pub fn from_settings(settings: &ClientSettings) -> Self {
        Self {
            fov_deg: f64::from(settings.fov),
            smooth_lighting: settings.smooth_lighting,
            view_distance: Some(i32::from(settings.view_distance)),
            clouds: settings.clouds,
            particles: settings.particles,
            entity_shadows: settings.entity_shadows,
            vsync: settings.vsync,
            max_fps: settings.max_fps,
            background_fps_limit: settings.background_fps_limit,
            gpu_backend: settings.gpu_backend,
            shader_pack: settings.shader_pack.clone(),
            shader_profile: settings.shader_profile.clone(),
        }
    }

    /// Frame pacing cap for the current window focus state, in frames per
    /// second (`0` = uncapped, matching the `max_fps`/`background_fps_limit`
    /// convention). While unfocused, an enabled `background_fps_limit`
    /// replaces the foreground cap; `0` or a focused window keeps the
    /// normal `max_fps` pacing.
    pub fn effective_fps_cap(&self, window_focused: bool) -> u32 {
        if !window_focused && self.background_fps_limit > 0 {
            u32::from(self.background_fps_limit)
        } else {
            self.max_fps.unwrap_or(0)
        }
    }

    /// Dirty-section rebuild allowance for the current window focus
    /// state. While unfocused with background throttling enabled, at most
    /// one section rebuilds per frame so a hidden window never burns
    /// meshing work; focused (or throttling disabled) leaves the budget
    /// to the [`FrameBudgetGovernor`], expressed as `usize::MAX`.
    pub fn effective_rebuilds_per_frame(&self, window_focused: bool) -> usize {
        if !window_focused && self.background_fps_limit > 0 {
            1
        } else {
            usize::MAX
        }
    }
}

impl Default for RenderConfig {
    /// Legacy behavior: 70 deg FOV, smooth lighting, no distance cap.
    fn default() -> Self {
        Self {
            fov_deg: DEFAULT_FOV_DEG,
            smooth_lighting: true,
            view_distance: None,
            clouds: CloudMode::Fancy,
            particles: ParticleLevel::All,
            entity_shadows: true,
            vsync: true,
            max_fps: None,
            background_fps_limit: crate::settings::DEFAULT_BACKGROUND_FPS_LIMIT,
            gpu_backend: RenderBackendPreference::Auto,
            shader_pack: None,
            shader_profile: None,
        }
    }
}

/// Read-only entity data consumed by a visual render pass.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderEntitySnapshot {
    pub eid: i32,
    pub entity_type: i32,
    pub position: [f64; 3],
    pub visual: EntityVisualState,
}

/// Particle request and its decoded particle-specific options.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderParticleSnapshot {
    pub spawn: LevelParticleSpawn,
    pub options: Option<DecodedParticleOptions>,
}

/// Read-only projection of live world visual state for a frontend render frame.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderWorldSnapshot {
    pub entities: Vec<RenderEntitySnapshot>,
    pub pending_particles: Vec<RenderParticleSnapshot>,
    pub block_entities: Vec<([i32; 3], BlockEntityVisualState)>,
    pub boss_bars: Vec<([u8; 16], TrackedBossBar)>,
    pub rain_level: f32,
    pub thunder_level: f32,
    pub world_age: i64,
    pub time_of_day: i64,
    pub level_events: Vec<(VisualLevelEvent, [i32; 3])>,
}

/// Build an owned, stable snapshot without mutating or draining the live world queues.
pub fn project_render_snapshot(world: &WorldState) -> RenderWorldSnapshot {
    let entities = world
        .entities
        .values()
        .filter_map(|entity| {
            world
                .entity_visual_state(entity.eid)
                .map(|visual| RenderEntitySnapshot {
                    eid: entity.eid,
                    entity_type: entity.entity_type,
                    position: entity.pos,
                    visual,
                })
        })
        .collect();
    let pending_particles = world
        .level_particles
        .iter()
        .enumerate()
        .map(|(index, spawn)| RenderParticleSnapshot {
            spawn: *spawn,
            options: world
                .level_particle_options
                .get(index)
                .map(|(_, options)| options.clone()),
        })
        .collect();
    let block_entities = world
        .block_entities
        .keys()
        .filter_map(|pos| {
            world
                .block_entity_visual_state(*pos)
                .map(|visual| (*pos, visual))
        })
        .collect();
    RenderWorldSnapshot {
        entities,
        pending_particles,
        block_entities,
        boss_bars: world
            .boss_bars
            .iter()
            .map(|(uuid, bar)| (*uuid, bar.clone()))
            .collect(),
        rain_level: world.rain_level,
        thunder_level: world.thunder_level,
        world_age: world.world_age,
        time_of_day: world.time_of_day,
        level_events: world.visual_level_events().collect(),
    }
}

// ---------------------------------------------------------------------------
// Shaderpack session wiring
// ---------------------------------------------------------------------------

/// Inputs [`RenderConfig::resolve_shaderpack`] needs beyond the frame
/// input: the already-opened pack source, the active dimension, the UI
/// locale for `lang/*.lang` strings, and the distant-horizon LOD config.
#[derive(Clone, Copy)]
pub struct ShaderpackResolveInput<'a> {
    /// Opened shaderpack source; `None` when the configured pack could not
    /// be read from disk — resolution still degrades gracefully to
    /// built-in rendering with a notice.
    pub source: Option<&'a ShaderPackSource>,
    /// Dimension whose `world<N>` program overrides to plan.
    pub dimension: PackDimension,
    /// UI locale for pack `lang/*.lang` strings (`"en_US"`, `"ja_JP"`, ...).
    pub locale: &'a str,
    /// Active distant-horizon LOD config, when enabled.
    pub lod_config: Option<LodConfig>,
}

impl Default for ShaderpackResolveInput<'_> {
    fn default() -> Self {
        Self {
            source: None,
            dimension: PackDimension::Overworld,
            locale: "en_US",
            lod_config: None,
        }
    }
}

/// Shaderpack state resolved for the active GPU pipeline: the frame plan
/// to execute, the live pack session when one is running, and the
/// player-facing notice when a requested pack could not run.
#[derive(Clone, Debug)]
pub struct ShaderpackRenderState {
    /// Deterministic frame plan the renderer executes — the built-in
    /// vanilla plan, or the same plan layered with the pack's ordered
    /// passes via [`plan_frame_with_shaderpack`].
    pub frame_plan: RenderGraphPlan,
    /// Active shaderpack session when the pack passed validation; `None`
    /// for built-in rendering.
    pub session: Option<ActiveShaderSession>,
    /// Player-facing explanation shown when a requested pack fell back to
    /// built-in rendering (`None` when the pack is active or none was
    /// configured).
    pub user_notice: Option<String>,
    /// Missing GPU capabilities or validation errors reported on fallback,
    /// in deterministic order.
    pub missing_capabilities: Vec<String>,
}

impl RenderConfig {
    /// Wire the configured shaderpack into deterministic frame planning.
    ///
    /// With no `shader_pack` configured this is exactly the built-in
    /// [`plan_frame`] path. With a pack set, activation runs through
    /// [`activate_shaderpack`]; every rejection — an unreadable source, GPU
    /// capabilities the pack needs but the pipeline lacks, failed WGSL
    /// validation — resolves to the built-in plan with
    /// [`ShaderpackRenderState::user_notice`] populated. This path never
    /// panics and never hard-fails, so an incompatible pack (e.g. a
    /// compute/SSBO pack on `ClassicLowSpec`) always degrades to built-in
    /// rendering instead of crashing.
    pub fn resolve_shaderpack(
        &self,
        pipeline: GpuPipelineKind,
        input: &FramePlanInput,
        shaderpack: &ShaderpackResolveInput<'_>,
    ) -> ShaderpackRenderState {
        let vanilla_plan = plan_frame(pipeline, input);
        let Some(pack_name) = self.shader_pack.as_deref() else {
            return ShaderpackRenderState {
                frame_plan: vanilla_plan,
                session: None,
                user_notice: None,
                missing_capabilities: Vec::new(),
            };
        };
        let Some(source) = shaderpack.source else {
            return ShaderpackRenderState {
                frame_plan: vanilla_plan,
                session: None,
                user_notice: Some(format!(
                    "Shaderpack '{pack_name}' could not be opened; \
                     switched back to built-in rendering."
                )),
                missing_capabilities: vec!["shaderpack source could not be loaded".to_owned()],
            };
        };
        let req = ShaderActivationRequest {
            pack_name,
            dimension: shaderpack.dimension,
            locale: shaderpack.locale,
            overrides: ShaderOptionOverrides {
                profile: self.shader_profile.clone(),
                options: Default::default(),
            },
            lod_config: shaderpack.lod_config,
        };
        match activate_shaderpack(source, pipeline, input, &req) {
            ShaderActivationOutcome::Active(session) => ShaderpackRenderState {
                frame_plan: plan_frame_with_shaderpack(pipeline, input, &session),
                session: Some(*session),
                user_notice: None,
                missing_capabilities: Vec::new(),
            },
            ShaderActivationOutcome::FallbackToVanilla {
                missing_capabilities,
                user_notice,
                vanilla_graph,
                ..
            } => ShaderpackRenderState {
                frame_plan: vanilla_graph,
                session: None,
                user_notice: Some(user_notice),
                missing_capabilities,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Always-on frame budget governor
// ---------------------------------------------------------------------------

/// Throttle stage the [`FrameBudgetGovernor`] is holding at, cheapest work
/// first. The numeric stage (`0..=3`) is what the governor tracks; the names
/// say what that stage trims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottleStage {
    /// Stage 0: every user setting passes through untouched.
    Full,
    /// Stage 1: dirty-section rebuilds per frame are capped.
    TrimRebuilds,
    /// Stage 2: far-terrain LOD columns are trimmed too.
    TrimLod,
    /// Stage 3: minimum work — rebuilds, LOD, and view distance all cut.
    MinBudget,
}

impl ThrottleStage {
    const COUNT: u8 = 4;

    fn from_stage(stage: u8) -> Self {
        match stage {
            0 => Self::Full,
            1 => Self::TrimRebuilds,
            2 => Self::TrimLod,
            _ => Self::MinBudget,
        }
    }

    /// The `0..=3` stage number (`Full` = 0).
    pub const fn stage(self) -> u8 {
        match self {
            Self::Full => 0,
            Self::TrimRebuilds => 1,
            Self::TrimLod => 2,
            Self::MinBudget => 3,
        }
    }
}

/// Per-frame work budget the governor hands back: how much work the render
/// loop may spend this frame without breaching the frame-time target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRenderBudget {
    /// Dirty section rebuilds allowed this frame (`usize::MAX` = uncapped).
    pub max_dirty_rebuilds_per_frame: usize,
    /// LOD columns the far-terrain pass may keep this frame.
    pub effective_lod_chunks: u16,
    /// Full-detail view distance cap this frame.
    pub effective_view_distance: u8,
}

/// Weight given to the newest frame in the rolling mean: the average settles
/// over roughly the last ~15 frames rather than jumping on one spike.
const FRAME_EWMA_ALPHA: f64 = 0.15;
/// Over 110% of the frame budget the frame is overloaded; under 60% there is
/// room to give work back. Between them the stage holds — that deadband is
/// what keeps the budget from flapping.
const FRAME_OVERLOAD_RATIO: f64 = 1.10;
const FRAME_RECOVER_RATIO: f64 = 0.60;
/// Consecutive over-budget frames before one stage-up step, so a single GC
/// or disk spike never engages the throttle.
const FRAME_ESCALATE_AFTER: u32 = 3;
/// Consecutive comfortable frames before one stage-down step — slower than
/// escalation so recovery does not immediately re-overload.
const FRAME_RECOVER_AFTER: u32 = 10;
/// Frame budget when `max_fps` is uncapped (roughly 60 fps).
const DEFAULT_FRAME_BUDGET_MS: f64 = 1000.0 / 60.0;

/// Always-on frame budget throttle, mirroring the server's `AutoLoadBalancer`
/// design: no settings toggle, no wall clock — the caller feeds one
/// `Duration` per frame and reads [`FrameRenderBudget`]s back. The EWMA +
/// consecutive-frame guards keep single-frame GC/disk spikes from engaging
/// the throttle, and hysteresis steps the stage down one at a time once
/// frame times stay comfortably under budget.
#[derive(Clone, Debug)]
pub struct FrameBudgetGovernor {
    ewma_ms: f64,
    primed: bool,
    stage: u8,
    over_frames: u32,
    under_frames: u32,
    budget_ms: f64,
}

impl FrameBudgetGovernor {
    /// Governor for a `max_fps` cap (`None` = uncapped ~60 fps budget).
    pub fn new(max_fps: Option<u32>) -> Self {
        Self {
            ewma_ms: 0.0,
            primed: false,
            stage: 0,
            over_frames: 0,
            under_frames: 0,
            budget_ms: max_fps
                .map(|fps| 1000.0 / fps.max(1) as f64)
                .unwrap_or(DEFAULT_FRAME_BUDGET_MS),
        }
    }

    /// Record one frame's cost and return the stage now in force. One call
    /// moves at most one stage in one direction, which is what makes the
    /// adaptation smooth rather than oscillating.
    pub fn observe_frame(&mut self, frame_time: Duration) -> ThrottleStage {
        let ms = frame_time.as_secs_f64() * 1000.0;
        self.ewma_ms = if self.primed {
            self.ewma_ms + (ms - self.ewma_ms) * FRAME_EWMA_ALPHA
        } else {
            self.primed = true;
            ms
        };
        if ms > self.budget_ms * FRAME_OVERLOAD_RATIO {
            self.over_frames += 1;
            self.under_frames = 0;
            if self.over_frames >= FRAME_ESCALATE_AFTER
                && self.ewma_ms > self.budget_ms * FRAME_OVERLOAD_RATIO
            {
                self.stage = (self.stage + 1).min(ThrottleStage::COUNT - 1);
                self.over_frames = 0;
            }
        } else if ms < self.budget_ms * FRAME_RECOVER_RATIO {
            self.under_frames += 1;
            self.over_frames = 0;
            if self.under_frames >= FRAME_RECOVER_AFTER
                && self.ewma_ms < self.budget_ms * FRAME_RECOVER_RATIO
            {
                self.stage = self.stage.saturating_sub(1);
                self.under_frames = 0;
            }
        } else {
            // Deadband frame: neither overloaded nor comfortable, so both
            // consecutive-run counters restart — a stage move always needs
            // an unbroken run of qualifying frames.
            self.over_frames = 0;
            self.under_frames = 0;
        }
        self.stage()
    }

    /// The stage currently in force.
    pub fn stage(&self) -> ThrottleStage {
        ThrottleStage::from_stage(self.stage)
    }

    /// The smoothed (EWMA) frame time the governor is reacting to.
    pub fn frame_ms(&self) -> f64 {
        self.ewma_ms
    }

    /// The configured frame budget in milliseconds.
    pub fn budget_ms(&self) -> f64 {
        self.budget_ms
    }

    /// The work budget this frame should run with, given the user's base
    /// `view_distance` and far-terrain `lod_chunks`. At [`ThrottleStage::Full`]
    /// every field is the exact user setting, untouched.
    pub fn effective_budget(
        &self,
        base_view_distance: u8,
        base_lod_chunks: u16,
    ) -> FrameRenderBudget {
        match self.stage() {
            ThrottleStage::Full => FrameRenderBudget {
                max_dirty_rebuilds_per_frame: usize::MAX,
                effective_lod_chunks: base_lod_chunks,
                effective_view_distance: base_view_distance,
            },
            ThrottleStage::TrimRebuilds => FrameRenderBudget {
                max_dirty_rebuilds_per_frame: 8,
                effective_lod_chunks: base_lod_chunks,
                effective_view_distance: base_view_distance,
            },
            ThrottleStage::TrimLod => FrameRenderBudget {
                max_dirty_rebuilds_per_frame: 4,
                effective_lod_chunks: base_lod_chunks / 2,
                effective_view_distance: base_view_distance,
            },
            ThrottleStage::MinBudget => FrameRenderBudget {
                max_dirty_rebuilds_per_frame: 2,
                effective_lod_chunks: base_lod_chunks / 4,
                effective_view_distance: base_view_distance.saturating_sub(4).max(2),
            },
        }
    }
}

/// Client-side owner of the bounded fixed-tick VFX simulation. The caller
/// routes `megu:vfx_v1` custom payload bytes here and submits the
/// resulting [`VfxFrameOutput`] alongside the terrain frame plan.
pub struct ClientVfxHost {
    pipeline: GpuPipelineKind,
    tier: VfxHardwareTier,
    world: VfxWorld,
}

impl ClientVfxHost {
    pub fn new(pipeline: GpuPipelineKind) -> Self {
        let tier = vfx_tier_for_pipeline(pipeline);
        Self {
            pipeline,
            tier,
            world: VfxWorld::new(tier),
        }
    }

    pub fn budget(&self) -> VfxBudget {
        self.world.budget()
    }

    pub fn budget_tier(&self) -> VfxHardwareTier {
        self.tier
    }

    /// Decode one complete VFX1 payload before applying commands. Returns
    /// the number admitted under the current effect caps.
    pub fn ingest_vfx1_payload(&mut self, bytes: &[u8]) -> Result<usize, VfxCodecError> {
        let packet = Vfx1Packet::decode(bytes)?;
        let mut admitted = 0;
        for command in packet.commands {
            if self.world.apply(command).map_err(VfxCodecError::from)? {
                admitted += 1;
            }
        }
        Ok(admitted)
    }

    /// Advance one fixed simulation tick. Use `frame_output` when a render
    /// frame arrives without another simulation tick.
    pub fn tick(&mut self) {
        self.world.tick();
    }

    pub fn frame_output(&self, camera_forward: [f32; 3]) -> VfxFrameOutput {
        self.world.frame_output(camera_forward)
    }

    pub fn step(&mut self, camera_forward: [f32; 3]) -> VfxFrameOutput {
        self.tick();
        self.frame_output(camera_forward)
    }

    /// Plan an opt-in translucent VFX pass using the active governor tier.
    pub fn plan_frame_with_vfx(
        &self,
        input: &FramePlanInput,
        output: &VfxFrameOutput,
    ) -> RenderGraphPlan {
        plan_frame_with_vfx_tier(self.pipeline, input, output, self.tier)
    }

    /// Adjust effect caps to the current frame governor stage. The VFX
    /// engine exposes immutable budgets, so changing tiers clears the
    /// previous tier's transient effects. New payloads use the new cap.
    pub fn sync_governor(&mut self, governor: &FrameBudgetGovernor) {
        let base = vfx_tier_for_pipeline(self.pipeline);
        let tier = match governor.stage() {
            ThrottleStage::Full | ThrottleStage::TrimRebuilds => base,
            ThrottleStage::TrimLod => match base {
                VfxHardwareTier::High => VfxHardwareTier::Balanced,
                VfxHardwareTier::Balanced | VfxHardwareTier::LowSpec => VfxHardwareTier::LowSpec,
            },
            ThrottleStage::MinBudget => VfxHardwareTier::LowSpec,
        };
        if self.tier != tier {
            self.tier = tier;
            self.world = VfxWorld::new(tier);
        }
    }

    /// Record frame time and apply the resulting quality stage immediately.
    pub fn observe_frame(
        &mut self,
        governor: &mut FrameBudgetGovernor,
        frame_time: Duration,
    ) -> ThrottleStage {
        let stage = governor.observe_frame(frame_time);
        self.sync_governor(governor);
        stage
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderStats {
    pub render_ms: f64,
    pub triangles_drawn: u64,
    pub pixels_shaded: u64,
    /// Renderable sections the `GraphCuller` BFS reached and that had
    /// packed geometry submitted to the rasterizer.
    pub drawn_sections: u64,
    /// Renderable sections rejected by frustum, occlusion, or the
    /// draw-distance cap.
    pub culled_sections: u64,
    /// Batched multi-draw submissions for the regions holding drawn
    /// sections (one `MultiDrawCommand` per region x facing bucket).
    pub region_draw_calls: u64,
    /// Non-empty direction buckets submitted across drawn sections.
    pub buckets_drawn: u64,
    /// Non-empty direction buckets skipped as backfacing across drawn
    /// sections.
    pub buckets_culled: u64,
    /// Sections whose 16^3 AABB was tested against the `HiZPyramid` during
    /// a Hi-Z frame (0 when Hi-Z is off).
    pub hiz_tested_sections: u64,
    /// Sections the `HiZPyramid` proved fully occluded — skipped before
    /// rasterization with zero effect on the output pixels.
    pub hiz_culled_sections: u64,
    /// `BakedModelQuad`s rasterized by the placed-model pass.
    pub model_quads_drawn: u64,
}

/// A rendered RGB frame ready for PNG encoding.
pub struct RenderedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB8 pixels, row-major.
    pub rgb: Vec<u8>,
    pub stats: RenderStats,
}

impl RenderedFrame {
    pub fn encode_png(&self) -> Vec<u8> {
        encode_png_rgb(self.width, self.height, &self.rgb)
    }

    pub fn write_png(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        let png = self.encode_png();
        std::fs::write(path, png).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }
}

#[derive(Clone, Copy, Default)]
struct CamVertex {
    /// Camera-space position: x right, y up, z forward (positive = in front).
    pos: [f64; 3],
    color: [f32; 3],
    layer: u32,
    fluid_kind: u32,
    diagonal: bool,
}

#[derive(Clone, Copy)]
struct ScreenVertex {
    x: f64,
    y: f64,
    z: f64, // view-space depth, smaller is nearer
    color: [f32; 3],
}

struct Rasterizer {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    depth: Vec<f32>,
    focal: f64,
    /// Depth-prepass mode: triangles write only `depth` (no color, no
    /// `pixels_shaded` accounting) so the resulting buffer can feed a
    /// `HiZPyramid` build before the shading pass runs.
    depth_only: bool,
    stats: RenderStats,
}

impl Rasterizer {
    fn new(width: u32, height: u32, fov_deg: f64) -> Self {
        let (w, h) = (width as usize, height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        // Sky gradient: deep blue at the top horizon to pale at the bottom.
        for y in 0..h {
            let t = y as f64 / h.saturating_sub(1).max(1) as f64;
            let top = [86.0f64, 132.0, 214.0];
            let bottom = [168.0f64, 205.0, 240.0];
            let c = [
                top[0] + (bottom[0] - top[0]) * t,
                top[1] + (bottom[1] - top[1]) * t,
                top[2] + (bottom[2] - top[2]) * t,
            ];
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = c[0] as u8;
                rgb[i + 1] = c[1] as u8;
                rgb[i + 2] = c[2] as u8;
            }
        }
        Self {
            width: w,
            height: h,
            rgb,
            depth: vec![f32::INFINITY; w * h],
            focal: (f64::from(height) * 0.5) / (fov_deg.to_radians() * 0.5).tan(),
            depth_only: false,
            stats: RenderStats::default(),
        }
    }

    fn project(&self, v: &CamVertex) -> ScreenVertex {
        let z = v.pos[2];
        let scale = self.focal / z;
        ScreenVertex {
            x: self.width as f64 * 0.5 + v.pos[0] * scale,
            y: self.height as f64 * 0.5 - v.pos[1] * scale,
            z,
            color: v.color,
        }
    }

    /// Rasterize one camera-space triangle. `translucent` selects alpha
    /// blending without a depth write.
    fn triangle(&mut self, verts: &[CamVertex; 3], translucent: bool, alpha: f32) {
        let alpha = f64::from(alpha);
        let p = [
            self.project(&verts[0]),
            self.project(&verts[1]),
            self.project(&verts[2]),
        ];
        // 2x signed screen area; degenerate triangles are skipped.
        let area = (p[1].y - p[2].y) * (p[0].x - p[2].x) + (p[2].x - p[1].x) * (p[0].y - p[2].y);
        if area.abs() < 1e-9 {
            return;
        }
        let min_x = p.iter().map(|v| v.x).fold(f64::INFINITY, f64::min).floor() as isize;
        let max_x = p
            .iter()
            .map(|v| v.x)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil() as isize;
        let min_y = p.iter().map(|v| v.y).fold(f64::INFINITY, f64::min).floor() as isize;
        let max_y = p
            .iter()
            .map(|v| v.y)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil() as isize;
        let x0 = min_x.max(0) as usize;
        let x1 = (max_x + 1).min(self.width as isize).max(0) as usize;
        let y0 = min_y.max(0) as usize;
        let y1 = (max_y + 1).min(self.height as isize).max(0) as usize;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.stats.triangles_drawn += 1;

        let inv = [1.0 / p[0].z, 1.0 / p[1].z, 1.0 / p[2].z];
        for py in y0..y1 {
            let fy = py as f64 + 0.5;
            for px in x0..x1 {
                let fx = px as f64 + 0.5;
                let w0 =
                    ((p[1].y - p[2].y) * (fx - p[2].x) + (p[2].x - p[1].x) * (fy - p[2].y)) / area;
                let w1 =
                    ((p[2].y - p[0].y) * (fx - p[2].x) + (p[0].x - p[2].x) * (fy - p[2].y)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let iz = w0 * inv[0] + w1 * inv[1] + w2 * inv[2];
                if iz <= 0.0 {
                    continue;
                }
                let depth = 1.0 / iz;
                let idx = py * self.width + px;
                if depth >= self.depth[idx] as f64 {
                    continue;
                }
                if self.depth_only {
                    self.depth[idx] = depth as f32;
                    continue;
                }
                let wsum = iz;
                let shade = |c: usize| {
                    (w0 * p[0].color[c] as f64 * inv[0]
                        + w1 * p[1].color[c] as f64 * inv[1]
                        + w2 * p[2].color[c] as f64 * inv[2])
                        / wsum
                };
                let dst = idx * 3;
                let s = [shade(0), shade(1), shade(2)];
                if translucent {
                    for (channel, &sv) in s.iter().enumerate() {
                        let src = sv.clamp(0.0, 1.0) * 255.0;
                        let d = f64::from(self.rgb[dst + channel]);
                        self.rgb[dst + channel] =
                            (src * alpha + d * (1.0 - alpha)).clamp(0.0, 255.0) as u8;
                    }
                } else {
                    for (channel, &sv) in s.iter().enumerate() {
                        self.rgb[dst + channel] = (sv.clamp(0.0, 1.0) * 255.0).round() as u8;
                    }
                    self.depth[idx] = depth as f32;
                }
                self.stats.pixels_shaded += 1;
            }
        }
    }

    /// Clip a camera-space triangle against the near plane `z >= NEAR_Z` and
    /// rasterize the resulting polygon as a triangle fan.
    fn clipped_triangle(&mut self, tri: &[CamVertex; 3], translucent: bool, alpha: f32) {
        let mut poly: Vec<CamVertex> = Vec::with_capacity(4);
        for i in 0..3 {
            let a = tri[i];
            let b = tri[(i + 1) % 3];
            let da = a.pos[2] - NEAR_Z;
            let db = b.pos[2] - NEAR_Z;
            if da >= 0.0 {
                poly.push(a);
            }
            if (da >= 0.0) != (db >= 0.0) {
                let t = da / (da - db);
                let mut hit = a;
                for axis in 0..3 {
                    hit.pos[axis] += (b.pos[axis] - a.pos[axis]) * t;
                }
                for c in 0..3 {
                    hit.color[c] += (b.color[c] - a.color[c]) * t as f32;
                }
                poly.push(hit);
            }
        }
        if poly.len() < 3 {
            return;
        }
        // Drop vertices still beyond the far plane when the whole polygon is.
        if poly.iter().all(|v| v.pos[2] > FAR_Z) {
            return;
        }
        for fan in 1..poly.len() - 1 {
            self.triangle(&[poly[0], poly[fan], poly[fan + 1]], translucent, alpha);
        }
    }

    /// The depth buffer re-mapped to `[0, 1]` (`0` = `NEAR_Z`, `1` =
    /// `FAR_Z`/untouched), exactly the `z_ndc = a + b/z` mapping
    /// [`view_proj_matrix`] produces, for `HiZPyramid::from_depth_buffer`.
    fn depth_01(&self) -> Vec<f32> {
        self.depth
            .iter()
            .map(|&z| {
                let z = f64::from(z);
                if !z.is_finite() || z <= 0.0 {
                    1.0
                } else {
                    ((FAR_Z * (z - NEAR_Z) / ((FAR_Z - NEAR_Z) * z)) as f32).clamp(0.0, 1.0)
                }
            })
            .collect()
    }

    /// Restore every depth texel to the untouched (far) value after a
    /// prepass, so the shading pass re-resolves visibility from scratch.
    fn reset_depth(&mut self) {
        self.depth.fill(f32::INFINITY);
    }
}

// ---------------------------------------------------------------------------
// Vertex decode + shading
// ---------------------------------------------------------------------------

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [f32; 3] {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f64;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r as f32, g as f32, b as f32]
}

/// Deterministic block-ID color palette: golden-ratio hue per state id.
fn palette_color(state_id: u32) -> [f32; 3] {
    let hue = (state_id as f64 * 0.618_033_988_749_895).fract();
    hsv_to_rgb(hue, 0.5, 0.85)
}

fn tint_color(tint: u32) -> Option<[f32; 3]> {
    if tint == 0 {
        return None;
    }
    Some([
        ((tint >> 16) & 0xff) as f32 / 255.0,
        ((tint >> 8) & 0xff) as f32 / 255.0,
        (tint & 0xff) as f32 / 255.0,
    ])
}

/// Per-face brightness in vanilla order: down, up, north, south, west, east.
const FACE_SHADE: [f32; 6] = [0.45, 1.0, 0.75, 0.85, 0.7, 0.95];
const AO_SHADE: [f32; 4] = [0.4, 0.6, 0.8, 1.0];

fn decode_vertex(pv: &PackedVertex, origin: [f64; 3], smooth_lighting: bool) -> CamVertex {
    let attr = pv.attributes;
    let lx = f64::from(pv.position & 0x1f);
    let lz = f64::from((pv.position >> 5) & 0x1f);
    let ly = f64::from(pv.position >> 10) / 1024.0;
    let direction = (attr & ATTR_DIRECTION_MASK) as usize;
    let ao = ((attr >> ATTR_AO_SHIFT) & 0x3) as usize;
    let block_light = ((attr >> ATTR_BLOCK_LIGHT_SHIFT) & 0xf) as f32;
    let sky_light = ((attr >> ATTR_SKY_LIGHT_SHIFT) & 0xf) as f32;
    let layer = (attr >> ATTR_LAYER_SHIFT) & 0x3;
    let fluid_kind = (attr >> ATTR_FLUID_KIND_SHIFT) & 0x3;

    let base = tint_color(pv.tint).unwrap_or_else(|| palette_color(pv.state_id));
    let light = (block_light.max(sky_light) / 15.0).clamp(0.0, 1.0);
    let illum = 0.25 + 0.75 * light;
    // Smooth lighting off = flat per-face shading without per-vertex AO.
    let ao_shade = if smooth_lighting { AO_SHADE[ao] } else { 1.0 };
    let shade = FACE_SHADE[direction.min(5)] * ao_shade * illum;
    CamVertex {
        pos: [origin[0] + lx, origin[1] + ly, origin[2] + lz],
        color: [base[0] * shade, base[1] * shade, base[2] * shade],
        layer,
        fluid_kind,
        diagonal: attr & ATTR_DIAGONAL_BIT != 0,
    }
}

fn is_translucent_quad(v: &CamVertex) -> bool {
    v.layer == LAYER_TRANSLUCENT || (v.layer == LAYER_FLUID && v.fluid_kind == FLUID_WATER)
}

fn translucent_alpha(v: &CamVertex) -> f32 {
    match (v.layer, v.fluid_kind) {
        (LAYER_FLUID, FLUID_LAVA) => 0.95,
        (LAYER_FLUID, _) => 0.75,
        _ => 0.55,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Camera basis for `yaw`/`pitch` (degrees, server convention): forward into
/// the screen, right = forward x up, up = right x forward.
fn camera_basis(yaw: f32, pitch: f32) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let yaw = f64::from(yaw).to_radians();
    let pitch = f64::from(pitch).to_radians();
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let fwd = [-sy * cp, -sp, cy * cp];
    let right = [-fwd[2], 0.0, fwd[0]];
    let right_len = (right[0] * right[0] + right[2] * right[2]).sqrt();
    if right_len > 1e-9 {
        let r = [right[0] / right_len, 0.0, right[2] / right_len];
        let u = [
            r[1] * fwd[2] - r[2] * fwd[1],
            r[2] * fwd[0] - r[0] * fwd[2],
            r[0] * fwd[1] - r[1] * fwd[0],
        ];
        (fwd, r, u)
    } else {
        // Looking straight up/down: fall back to a stable right vector.
        (fwd, [1.0, 0.0, 0.0], [0.0, 0.0, -1.0])
    }
}

/// The six world-space frustum planes of a camera pose at the default FOV,
/// in `megu_client_mesh::box_in_frustum` form (`dot(n, p) + d >= 0`
/// inside). `aspect` is render width / height. Used by the `GraphCuller`
/// walk and to prioritize `MeshCache::rebuild_dirty_ordered`.
pub fn frustum_planes(camera: [f32; 3], yaw: f32, pitch: f32, aspect: f64) -> [[f32; 4]; 6] {
    frustum_planes_fov(camera, yaw, pitch, aspect, DEFAULT_FOV_DEG)
}

/// [`frustum_planes`] with an explicit vertical field of view in degrees.
pub fn frustum_planes_fov(
    camera: [f32; 3],
    yaw: f32,
    pitch: f32,
    aspect: f64,
    fov_deg: f64,
) -> [[f32; 4]; 6] {
    let eye = [
        f64::from(camera[0]),
        f64::from(camera[1]),
        f64::from(camera[2]),
    ];
    let (fwd, right, up) = camera_basis(yaw, pitch);
    let tan_v = (fov_deg.to_radians() * 0.5).tan();
    let tan_h = tan_v * aspect;
    // Side planes pass through the eye: a camera-space point (x, y, z) is
    // inside when |x| <= z*tan_h and |y| <= z*tan_v.
    let through_eye = |n: [f64; 3]| -> [f32; 4] {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let (nx, ny, nz) = (n[0] / len, n[1] / len, n[2] / len);
        [
            nx as f32,
            ny as f32,
            nz as f32,
            (-(nx * eye[0] + ny * eye[1] + nz * eye[2])) as f32,
        ]
    };
    let d_fwd = fwd[0] * eye[0] + fwd[1] * eye[1] + fwd[2] * eye[2];
    [
        through_eye([
            right[0] + fwd[0] * tan_h,
            right[1] + fwd[1] * tan_h,
            right[2] + fwd[2] * tan_h,
        ]),
        through_eye([
            fwd[0] * tan_h - right[0],
            fwd[1] * tan_h - right[1],
            fwd[2] * tan_h - right[2],
        ]),
        through_eye([
            up[0] + fwd[0] * tan_v,
            up[1] + fwd[1] * tan_v,
            up[2] + fwd[2] * tan_v,
        ]),
        through_eye([
            fwd[0] * tan_v - up[0],
            fwd[1] * tan_v - up[1],
            fwd[2] * tan_v - up[2],
        ]),
        [
            fwd[0] as f32,
            fwd[1] as f32,
            fwd[2] as f32,
            (-d_fwd - NEAR_Z) as f32,
        ],
        [
            -fwd[0] as f32,
            -fwd[1] as f32,
            -fwd[2] as f32,
            (d_fwd + FAR_Z) as f32,
        ],
    ]
}

/// Row-major 4x4 `clip = view_proj * [x, y, z, 1]` matching the
/// rasterizer's perspective projection: camera-space `x` right, `y` up, `z`
/// forward; NDC `x`/`y` map linearly onto `width`/`height` pixels with
/// top-left origin, and NDC `z` is the Vulkan-style `[0, 1]` range
/// (`0` = `NEAR_Z`, `1` = `FAR_Z`) that `HiZPyramid::cull_world_aabb`
/// reads.
fn view_proj_matrix(
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
    fov_deg: f64,
) -> [[f32; 4]; 4] {
    let (fwd, right, up) = camera_basis(yaw, pitch);
    // view: world -> camera.
    let view = [
        [
            right[0],
            right[1],
            right[2],
            -(right[0] * eye[0] + right[1] * eye[1] + right[2] * eye[2]),
        ],
        [
            up[0],
            up[1],
            up[2],
            -(up[0] * eye[0] + up[1] * eye[1] + up[2] * eye[2]),
        ],
        [
            fwd[0],
            fwd[1],
            fwd[2],
            -(fwd[0] * eye[0] + fwd[1] * eye[1] + fwd[2] * eye[2]),
        ],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let focal = (f64::from(height) * 0.5) / (fov_deg.to_radians() * 0.5).tan();
    // Rasterizer convention: sx = W/2 + cx*f/z, sy = H/2 - cy*f/z, and the
    // Hi-Z pixel mapping py = (ndc*0.5 + 0.5) * dim counts from the top, so
    // clip y flips sign.
    let fx = 2.0 * focal / f64::from(width);
    let fy = -2.0 * focal / f64::from(height);
    let a = FAR_Z / (FAR_Z - NEAR_Z);
    let b = -FAR_Z * NEAR_Z / (FAR_Z - NEAR_Z);
    // proj: camera -> clip with w = z_cam, ndc_z in [0, 1].
    let proj = [
        [fx, 0.0, 0.0, 0.0],
        [0.0, fy, 0.0, 0.0],
        [0.0, 0.0, a, b],
        [0.0, 0.0, 1.0, 0.0],
    ];
    let mut out = [[0.0f32; 4]; 4];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            let mut v = 0.0f64;
            for k in 0..4 {
                v += proj[r][k] * view[k][c];
            }
            *cell = v as f32;
        }
    }
    out
}

/// Rasterize the opaque quads of one packed section across the direction
/// buckets `draw_mask` keeps, returning whether any translucent quads were
/// seen (they queue the section for the later sorted pass). Bucket
/// accounting is charged only when `count_stats` — the Hi-Z depth prepass
/// re-runs the same routine and must not double-count.
#[allow(clippy::too_many_arguments)]
fn rasterize_section_opaque(
    raster: &mut Rasterizer,
    packed: &[PackedVertex],
    buckets: &DirectionBuckets,
    draw_mask: u8,
    origin: [f64; 3],
    config: &RenderConfig,
    to_camera: &dyn Fn([f64; 3]) -> [f64; 3],
    count_stats: bool,
) -> bool {
    let mut section_has_translucent = false;
    for bucket in 0..DIRECTION_BUCKETS {
        let (offset, count) = (
            buckets.offset[bucket] as usize,
            buckets.count[bucket] as usize,
        );
        if count == 0 {
            continue;
        }
        if bucket != DIRECTION_BUCKET_UNASSIGNED && draw_mask & (1 << bucket) == 0 {
            if count_stats {
                raster.stats.buckets_culled += 1;
            }
            continue;
        }
        if count_stats {
            raster.stats.buckets_drawn += 1;
        }
        for qi in offset..offset + count {
            let quad = &packed[qi * 4..qi * 4 + 4];
            let mut verts = [CamVertex::default(); 4];
            let mut any_translucent = false;
            for (i, pv) in quad.iter().enumerate() {
                verts[i] = decode_vertex(pv, origin, config.smooth_lighting);
                any_translucent |= is_translucent_quad(&verts[i]);
            }
            // All four vertices of a quad share layer/fluid/diagonal.
            if any_translucent {
                section_has_translucent = true;
                continue;
            }
            let diagonal = verts[0].diagonal;
            for vtx in verts.iter_mut() {
                vtx.pos = to_camera(vtx.pos);
            }
            let tris: [[usize; 3]; 2] = if diagonal {
                [[0, 1, 3], [1, 2, 3]]
            } else {
                [[0, 1, 2], [0, 2, 3]]
            };
            for tri in tris {
                raster.clipped_triangle(
                    &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                    false,
                    1.0,
                );
            }
        }
    }
    section_has_translucent
}

/// One placed batch of resource-pack model geometry: `BakedModelQuad`s in
/// block-local `0..=1` space drawn at `origin` with a uniform `scale`.
/// Resource packs produce these through
/// `megu_client_mesh::model_baker::bake_model`.
#[derive(Clone, Debug)]
pub struct BakedModelDraw<'a> {
    /// Quads in block-local model space (e.g. `BakedModel::quads`).
    pub quads: &'a [BakedModelQuad],
    /// World-space origin of the block the model occupies.
    pub origin: [f64; 3],
    /// Uniform scale applied to model-space coordinates (`1.0` = one block).
    pub scale: f64,
}

/// Dominant face index of a baked quad's rotated normal, in `Direction`
/// order (down/up/north/south/west/east), for `FACE_SHADE` lookup.
fn normal_face_index(n: [f32; 3]) -> usize {
    let a = [n[0].abs(), n[1].abs(), n[2].abs()];
    if a[1] >= a[0] && a[1] >= a[2] {
        if n[1] > 0.0 {
            1
        } else {
            0
        }
    } else if a[2] >= a[0] {
        if n[2] > 0.0 {
            3
        } else {
            2
        }
    } else if n[0] > 0.0 {
        5
    } else {
        4
    }
}

/// Deterministic color for a `BakedModelQuad`: an FNV-1a hash of the
/// resolved texture id picks a stable hue (the resource-pack atlas lookup
/// replaces this in the Vulkan path), `shade` applies the vanilla per-face
/// brightness, and `tintindex >= 0` multiplies in a deterministic biome-class
/// tint like the CPU `tint` word does for packed quads.
fn baked_quad_color(quad: &BakedModelQuad) -> [f32; 3] {
    let hash = megu_core::hash::fnv1a64(quad.texture.as_bytes());
    let base = hsv_to_rgb((hash as f64 * 0.618_033_988_749_895).fract(), 0.55, 0.9);
    let shade = if quad.shade {
        FACE_SHADE[normal_face_index(quad.normal)]
    } else {
        1.0
    };
    let mut rgb = [base[0] * shade, base[1] * shade, base[2] * shade];
    if quad.tintindex >= 0 {
        let tint = hsv_to_rgb((f64::from(quad.tintindex) * 0.31 + 0.33).fract(), 0.6, 1.0);
        for (channel, &t) in tint.iter().enumerate() {
            rgb[channel] *= t;
        }
    }
    rgb
}

/// Rasterize `quads` (block-local `0..=1` space) placed at `origin` and
/// scaled by `scale` into `raster`, through the same projection, clipping,
/// and depth test as terrain. Returns the number of quads submitted.
fn rasterize_baked_quads(
    raster: &mut Rasterizer,
    quads: &[BakedModelQuad],
    origin: [f64; 3],
    scale: f64,
    to_camera: &dyn Fn([f64; 3]) -> [f64; 3],
) -> usize {
    let mut drawn = 0usize;
    for quad in quads {
        let color = baked_quad_color(quad);
        let mut verts = [CamVertex::default(); 4];
        for (i, p) in quad.positions.iter().enumerate() {
            verts[i] = CamVertex {
                pos: to_camera([
                    origin[0] + f64::from(p[0]) * scale,
                    origin[1] + f64::from(p[1]) * scale,
                    origin[2] + f64::from(p[2]) * scale,
                ]),
                color,
                layer: 0,
                fluid_kind: 0,
                diagonal: false,
            };
        }
        for tri in [[0, 1, 2], [0, 2, 3]] {
            raster.clipped_triangle(
                &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                false,
                1.0,
            );
        }
        drawn += 1;
    }
    drawn
}

/// Render placed `BakedModelQuad` resource-pack geometry to a fresh RGB
/// frame through the same camera/projection/shading path as
/// [`render_frame`]. `quads` are in block-local `0..=1` space, drawn at
/// `origin`; `fov_deg` is the vertical field of view in degrees.
#[allow(clippy::too_many_arguments)]
pub fn rasterize_baked_model_quads(
    quads: &[BakedModelQuad],
    origin: [f64; 3],
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
    fov_deg: f64,
) -> Result<RenderedFrame, String> {
    if width == 0 || height == 0 {
        return Err("render dimensions must be non-zero".to_string());
    }
    let started = Instant::now();
    let mut raster = Rasterizer::new(width, height, fov_deg);
    let (fwd, right, up) = camera_basis(yaw, pitch);
    let to_camera = |p: [f64; 3]| -> [f64; 3] {
        let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        [
            d[0] * right[0] + d[1] * right[1] + d[2] * right[2],
            d[0] * up[0] + d[1] * up[1] + d[2] * up[2],
            d[0] * fwd[0] + d[1] * fwd[1] + d[2] * fwd[2],
        ]
    };
    raster.stats.model_quads_drawn =
        rasterize_baked_quads(&mut raster, quads, origin, 1.0, &to_camera) as u64;
    raster.stats.render_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(RenderedFrame {
        width,
        height,
        rgb: raster.rgb,
        stats: raster.stats,
    })
}

/// Render one camera frame of the live world to an RGB buffer.
/// `eye` is the camera position (player pos + eye height); `yaw`/`pitch` are
/// degrees using the server's convention (`forward = (-sin yaw·cos pitch,
/// -sin pitch, cos yaw·cos pitch)`). Equivalent to
/// [`render_frame_with_config`] with [`RenderConfig::default`].
pub fn render_frame(
    world: &WorldState,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
) -> Result<RenderedFrame, String> {
    render_frame_with_config(
        world,
        eye,
        yaw,
        pitch,
        width,
        height,
        &RenderConfig::default(),
    )
}

/// [`render_frame`] honoring a resolved [`RenderConfig`]: `fov_deg` drives
/// the projection, `smooth_lighting` the per-vertex AO term, and
/// `view_distance` caps drawn sections to a Chebyshev chunk-column radius
/// around the camera column.
pub fn render_frame_with_config(
    world: &WorldState,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
    config: &RenderConfig,
) -> Result<RenderedFrame, String> {
    render_frame_impl(
        world,
        eye,
        yaw,
        pitch,
        width,
        height,
        config,
        config.fov_deg,
        false,
        &[],
    )
}

/// [`render_frame_with_config`] with a Hi-Z occlusion pass, the smooth zoom
/// controller, and placed resource-pack model geometry:
///
/// * `zoom` is ticked by `zoom_dt_seconds`; the interpolated
///   [`ZoomState::fov`] replaces `config.fov_deg` for this frame.
/// * `models` draws `BakedModelQuad` batches (block entities, torches, mod
///   models) through the same depth-tested pipeline after opaque terrain.
/// * A depth-only prepass rasterizes opaque geometry into the depth buffer,
///   builds a `HiZPyramid`, then the shading pass skips every section whose
///   16^3 AABB the pyramid proves fully occluded (`hiz_tested_sections` /
///   `hiz_culled_sections` in [`RenderStats`]). Hi-Z culling is
///   conservative: a culled section could not have contributed a single
///   pixel, so the RGB output matches a non-Hi-Z frame exactly.
#[allow(clippy::too_many_arguments)]
pub fn render_frame_with_hiz_and_zoom(
    world: &WorldState,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
    config: &RenderConfig,
    zoom: &mut ZoomState,
    zoom_dt_seconds: f32,
    models: &[BakedModelDraw<'_>],
) -> Result<RenderedFrame, String> {
    let fov_deg = f64::from(zoom.tick(zoom_dt_seconds));
    render_frame_impl(
        world,
        eye,
        yaw,
        pitch,
        width,
        height,
        config,
        fov_deg,
        true,
        models,
    )
}

/// Shared frame body for [`render_frame_with_config`] and
/// [`render_frame_with_hiz_and_zoom`]. `fov_deg` is the resolved projection
/// FOV (config or zoom-interpolated); `use_hiz` runs the depth-prepass +
/// `HiZPyramid` occlusion pass; `models` is the placed-model draw list.
#[allow(clippy::too_many_arguments)]
fn render_frame_impl(
    world: &WorldState,
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
    width: u32,
    height: u32,
    config: &RenderConfig,
    fov_deg: f64,
    use_hiz: bool,
    models: &[BakedModelDraw<'_>],
) -> Result<RenderedFrame, String> {
    if width == 0 || height == 0 {
        return Err("render dimensions must be non-zero".to_string());
    }
    let started = Instant::now();
    let mut raster = Rasterizer::new(width, height, fov_deg);

    let (fwd, right, up) = camera_basis(yaw, pitch);
    let eye32 = [eye[0] as f32, eye[1] as f32, eye[2] as f32];
    let planes = frustum_planes_fov(
        eye32,
        yaw,
        pitch,
        f64::from(width) / f64::from(height),
        fov_deg,
    );

    // Transform world vertices into camera space.
    let to_camera = |p: [f64; 3]| -> [f64; 3] {
        let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        [
            d[0] * right[0] + d[1] * right[1] + d[2] * right[2],
            d[0] * up[0] + d[1] * up[1] + d[2] * up[2],
            d[0] * fwd[0] + d[1] * fwd[1] + d[2] * fwd[2],
        ]
    };

    // Frustum x BFS graph cull over the section visibility graph. Only
    // sections the walk reaches are rasterized; everything else is counted
    // as culled.
    let camera_section = SectionCoord(
        (eye[0] / 16.0).floor() as i32,
        (eye[1] / 16.0).floor() as i32,
        (eye[2] / 16.0).floor() as i32,
    );
    let cull = GraphCuller::new(camera_section, &planes).search(world.section_graphs());
    raster.stats.culled_sections = cull.culled as u64;
    // Draw-distance cap: sections in chunk columns farther than
    // `view_distance` from the camera column are culled like any other
    // reject (they belong to the far-terrain LOD path instead).
    let within_view_distance = |coord: &SectionCoord| -> bool {
        match config.view_distance {
            None => true,
            Some(distance) => {
                let dc = i64::from(coord.0 - camera_section.0)
                    .abs()
                    .max(i64::from(coord.2 - camera_section.2).abs());
                dc <= i64::from(distance)
            }
        }
    };

    // Opaque draw order: nearest sections first so the depth buffer (and
    // hardware Early-Z hierarchies) reject the overdraw behind them before
    // shading it. Depth-tested output is identical to unsorted order.
    let mut visible_order: Vec<SectionCoord> = cull.visible.clone();
    visible_order.sort_by_key(|coord| {
        let min = [
            coord.0 as f32 * 16.0,
            coord.1 as f32 * 16.0,
            coord.2 as f32 * 16.0,
        ];
        let edge = 16.0;
        depth_key(aabb_distance_sq(
            min,
            [min[0] + edge, min[1] + edge, min[2] + edge],
            eye32,
        ))
    });
    // Translucent draw list: (squared camera distance, coord, bucket draw mask).
    let mut translucent_sections: Vec<(f64, SectionCoord, u8)> = Vec::new();
    // Multi-draw accounting: per-region set of drawn-section slab identities
    // (vertex_offset, index_offset) so MultiDraw submissions are only counted
    // when they contain at least one drawn section's draw.
    let mut drawn_slabs: HashMap<[i32; 3], HashSet<(u32, u32)>> = HashMap::new();

    // Hi-Z depth prepass: rasterize opaque geometry depth-only, build the
    // max-depth pyramid, then re-run the shading pass with every section's
    // AABB occlusion-tested first. The pyramid is conservative — a section
    // it culls could not show a single pixel, so the RGB output is
    // identical to a frame rendered without it.
    let mut hiz: Option<(HiZPyramid, [[f32; 4]; 4])> = None;
    if use_hiz {
        raster.depth_only = true;
        for &coord in &visible_order {
            if !within_view_distance(&coord) {
                continue;
            }
            let Some(packed) = world.packed_mesh(coord) else {
                continue;
            };
            if packed.len() < 4 || packed.len() % 4 != 0 {
                continue;
            }
            let Some(buckets) = world.direction_buckets(coord) else {
                continue;
            };
            let origin = [
                f64::from(coord.0) * 16.0,
                f64::from(coord.1) * 16.0,
                f64::from(coord.2) * 16.0,
            ];
            let draw_mask = bucket_draw_mask(box_facing_mask(
                [origin[0] as f32, origin[1] as f32, origin[2] as f32],
                eye32,
            ));
            rasterize_section_opaque(
                &mut raster,
                packed,
                &buckets,
                draw_mask,
                origin,
                config,
                &to_camera,
                false,
            );
        }
        raster.depth_only = false;
        let depth01 = raster.depth_01();
        raster.reset_depth();
        if let Ok(pyramid) =
            HiZPyramid::from_depth_buffer(raster.width, raster.height, &depth01)
        {
            hiz = Some((
                pyramid,
                view_proj_matrix(eye, yaw, pitch, width, height, fov_deg),
            ));
        }
    }

    for &coord in &visible_order {
        if !within_view_distance(&coord) {
            raster.stats.culled_sections += 1;
            continue;
        }
        let Some(packed) = world.packed_mesh(coord) else {
            continue;
        };
        if packed.len() < 4 || packed.len() % 4 != 0 {
            continue;
        }
        let Some(buckets) = world.direction_buckets(coord) else {
            continue;
        };
        let origin = [
            f64::from(coord.0) * 16.0,
            f64::from(coord.1) * 16.0,
            f64::from(coord.2) * 16.0,
        ];
        if let Some((pyramid, view_proj)) = &hiz {
            raster.stats.hiz_tested_sections += 1;
            if pyramid.cull_world_aabb(
                view_proj,
                [origin[0] as f32, origin[1] as f32, origin[2] as f32],
                [
                    origin[0] as f32 + 16.0,
                    origin[1] as f32 + 16.0,
                    origin[2] as f32 + 16.0,
                ],
            ) {
                raster.stats.hiz_culled_sections += 1;
                continue;
            }
        }
        raster.stats.drawn_sections += 1;
        if let Some(slab) = world
            .render_regions()
            .region(coord)
            .and_then(|region| region.slab(coord))
        {
            drawn_slabs
                .entry(RenderRegions::origin_of(coord))
                .or_default()
                .insert((slab.vertex_offset, slab.index_offset));
        }

        // Directional face bucketing: keep only the direction buckets whose
        // section side faces the camera; the unassigned bucket always draws.
        let draw_mask = bucket_draw_mask(box_facing_mask(
            [origin[0] as f32, origin[1] as f32, origin[2] as f32],
            eye32,
        ));
        let section_has_translucent = rasterize_section_opaque(
            &mut raster,
            packed,
            &buckets,
            draw_mask,
            origin,
            config,
            &to_camera,
            true,
        );
        if section_has_translucent {
            let center = [
                origin[0] + 8.0 - eye[0],
                origin[1] + 8.0 - eye[1],
                origin[2] + 8.0 - eye[2],
            ];
            let dist2 = center[0] * center[0] + center[1] * center[1] + center[2] * center[2];
            translucent_sections.push((dist2, coord, draw_mask));
        }
    }

    // Placed resource-pack model geometry (block entities, torches, mod
    // models): rasterized after opaque terrain so the shared depth buffer
    // resolves intersections. Models submit unconditionally — the section
    // Hi-Z test already dropped every section AABB the pyramid proved
    // occluded, and a placed model inside one is hidden with it.
    for draw in models {
        raster.stats.model_quads_drawn +=
            rasterize_baked_quads(&mut raster, draw.quads, draw.origin, draw.scale, &to_camera)
                as u64;
    }

    // Translucent pass: far sections first, `sort_translucent` order within.
    translucent_sections.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, coord, draw_mask) in translucent_sections {
        let Some(packed) = world.packed_mesh(coord) else {
            continue;
        };
        let origin = [
            f64::from(coord.0) * 16.0,
            f64::from(coord.1) * 16.0,
            f64::from(coord.2) * 16.0,
        ];
        let quad_count = packed.len() / 4;
        // Iterate the stored `sort_translucent` indices in place — no
        // per-section `Vec` materialization in the hot loop.
        let stored_order = world.translucent_order(coord).unwrap_or(&[]);
        let steps = if stored_order.is_empty() {
            quad_count
        } else {
            stored_order.len()
        };
        for n in 0..steps {
            let qi = if stored_order.is_empty() {
                n
            } else {
                stored_order[n] as usize
            };
            if qi >= quad_count {
                continue;
            }
            let quad = &packed[qi * 4..qi * 4 + 4];
            // Direction-bucket backface culling applies to translucent quads
            // too: the packed direction attribute is the quad's bucket.
            let direction = quad[0].attributes & ATTR_DIRECTION_MASK;
            if draw_mask & (1 << direction) == 0 {
                continue;
            }
            let mut verts = [CamVertex::default(); 4];
            for (i, pv) in quad.iter().enumerate() {
                verts[i] = decode_vertex(pv, origin, config.smooth_lighting);
            }
            if !is_translucent_quad(&verts[0]) {
                continue;
            }
            let alpha = translucent_alpha(&verts[0]);
            for vtx in verts.iter_mut() {
                vtx.pos = to_camera(vtx.pos);
            }
            let tris: [[usize; 3]; 2] = if verts[0].diagonal {
                [[0, 1, 3], [1, 2, 3]]
            } else {
                [[0, 1, 2], [0, 2, 3]]
            };
            for tri in tris {
                raster.clipped_triangle(
                    &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                    true,
                    alpha,
                );
            }
        }
    }

    // One multi-draw submission per (region, facing bucket), ordered
    // front-to-back for Early-Z; a submission counts only when at least one
    // of its indirect draws belongs to a section drawn this frame — culled
    // sections never reach the GPU.
    let mut multidraw_scratch = MultidrawScratch::new();
    world
        .render_regions()
        .build_multidraw_into(eye32, DrawOrder::FrontToBack, &mut multidraw_scratch);
    raster.stats.region_draw_calls = multidraw_scratch
        .commands()
        .iter()
        .filter(|cmd| {
            drawn_slabs.get(&cmd.region).is_some_and(|slabs| {
                cmd.draws
                    .iter()
                    .any(|d| slabs.contains(&(d.base_vertex as u32, d.first_index)))
            })
        })
        .count() as u64;

    raster.stats.render_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(RenderedFrame {
        width,
        height,
        rgb: raster.rgb,
        stats: raster.stats,
    })
}

// ---------------------------------------------------------------------------
// Additive overlay renderers: entity rigs, block entities, particles,
// debug wireframes
// ---------------------------------------------------------------------------

/// Camera pose for the additive overlay renderers ([`render_entity_rig`],
/// [`render_vanilla_ber`], [`render_debug_lines`], [`render_particle_quads`]).
/// `eye`/`yaw`/`pitch` match [`render_frame`]: eye is the world-space
/// camera position and yaw/pitch are degrees in the server convention.
/// The projection itself lives on [`FrameBuffer::new`]'s `fov_deg`, so a
/// caller resolving zoom through [`ZoomState`] constructs the framebuffer
/// with the effective FOV once and passes only the pose per call.
#[derive(Clone, Copy, Debug, Default)]
pub struct OverlayCamera {
    /// World-space camera position.
    pub eye: [f64; 3],
    /// Horizontal look angle, degrees.
    pub yaw: f32,
    /// Vertical look angle, degrees.
    pub pitch: f32,
}

/// An offscreen RGB framebuffer with the same perspective projection,
/// near-plane clipping, and depth test as [`render_frame`], plus the sky
/// gradient background. The additive overlay renderers draw into it;
/// [`FrameBuffer::into_rendered_frame`] / [`FrameBuffer::encode_png`] hand
/// the result to the existing PNG path. Overlay calls are purely additive:
/// they never touch [`render_frame`] outputs.
pub struct FrameBuffer {
    raster: Rasterizer,
}

impl FrameBuffer {
    /// Framebuffer `width`x`height` pixels at vertical FOV `fov_deg`
    /// degrees. Zero dimensions and non-finite/out-of-range FOV values are
    /// sanitized so overlay calls can never panic or poison the depth
    /// buffer.
    pub fn new(width: u32, height: u32, fov_deg: f64) -> Self {
        let fov_deg = if fov_deg.is_finite() && (1.0..179.0).contains(&fov_deg) {
            fov_deg
        } else {
            DEFAULT_FOV_DEG
        };
        Self {
            raster: Rasterizer::new(width.max(1), height.max(1), fov_deg),
        }
    }

    /// Framebuffer width in pixels.
    pub fn width(&self) -> u32 {
        self.raster.width as u32
    }

    /// Framebuffer height in pixels.
    pub fn height(&self) -> u32 {
        self.raster.height as u32
    }

    /// Tightly packed RGB8 pixels, row-major — the same layout as
    /// [`RenderedFrame::rgb`].
    pub fn rgb(&self) -> &[u8] {
        &self.raster.rgb
    }

    /// Rasterizer accounting accumulated by the overlay passes.
    pub fn stats(&self) -> RenderStats {
        self.raster.stats
    }

    /// Consume the framebuffer into a [`RenderedFrame`] for the existing
    /// PNG encoder and stats surface.
    pub fn into_rendered_frame(self) -> RenderedFrame {
        RenderedFrame {
            width: self.width(),
            height: self.height(),
            rgb: self.raster.rgb,
            stats: self.raster.stats,
        }
    }

    /// Encode the current pixels as a PNG (RFC 2083). Deterministic:
    /// identical pixels always produce identical bytes.
    pub fn encode_png(&self) -> Vec<u8> {
        encode_png_rgb(self.width(), self.height(), &self.raster.rgb)
    }
}

/// World→camera transform shared by every overlay pass: camera-space
/// `x` right, `y` up, `z` forward (positive = in front), exactly like the
/// terrain path's `to_camera` closure.
struct OverlayView {
    eye: [f64; 3],
    right: [f64; 3],
    up: [f64; 3],
    fwd: [f64; 3],
}

impl OverlayView {
    fn new(camera: &OverlayCamera) -> Self {
        let (fwd, right, up) = camera_basis(camera.yaw, camera.pitch);
        Self {
            eye: camera.eye,
            right,
            up,
            fwd,
        }
    }

    #[inline]
    fn to_camera(&self, p: [f64; 3]) -> [f64; 3] {
        let d = [p[0] - self.eye[0], p[1] - self.eye[1], p[2] - self.eye[2]];
        [
            d[0] * self.right[0] + d[1] * self.right[1] + d[2] * self.right[2],
            d[0] * self.up[0] + d[1] * self.up[1] + d[2] * self.up[2],
            d[0] * self.fwd[0] + d[1] * self.fwd[1] + d[2] * self.fwd[2],
        ]
    }
}

/// Rotate a mesh-local position or normal about the Y axis by `yaw_rad`
/// (right-handed, the `ber_rotate_y` convention: `x' = x·cos + z·sin`,
/// `z' = −x·sin + z·cos`).
#[inline]
fn rotate_yaw(p: [f32; 3], sin: f32, cos: f32) -> [f32; 3] {
    [
        p[0] * cos + p[2] * sin,
        p[1],
        -p[0] * sin + p[2] * cos,
    ]
}

/// Resolve an [`ExtrudedQuad::tint_index`] to a linear `0..=1` albedo:
/// `None` keeps the caller's `base_rgb`, packed 24-bit values (the beacon
/// beam convention) decode as sRGB, and small non-negative indices are
/// material tags resolved to a deterministic hue.
fn extruded_tint_albedo(tint_index: i32) -> Option<[f32; 3]> {
    if tint_index < 0 {
        return None;
    }
    if tint_index > 0xFF {
        return Some([
            ((tint_index >> 16) & 0xFF) as f32 / 255.0,
            ((tint_index >> 8) & 0xFF) as f32 / 255.0,
            (tint_index & 0xFF) as f32 / 255.0,
        ]);
    }
    Some(hsv_to_rgb(
        (f64::from(tint_index) * 0.618_033_988_749_895 + 0.13).fract(),
        0.55,
        0.95,
    ))
}

/// Shared body of [`render_extruded_mesh_at`]: mesh positions are block
/// units in a Y-up local space, rotated `yaw_rad` about the Y axis through
/// `world_offset` and depth-tested like terrain. `translucent` blends at
/// 60% alpha without depth writes (the slime translucent shell).
#[allow(clippy::too_many_arguments)]
fn render_extruded_mesh_pass(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    mesh: &ExtrudedMesh,
    world_offset: [f32; 3],
    yaw_rad: f32,
    base_rgb: [u8; 3],
    translucent: bool,
) {
    let view = OverlayView::new(&camera);
    let (sin, cos) = yaw_rad.sin_cos();
    let offset = [
        f64::from(world_offset[0]),
        f64::from(world_offset[1]),
        f64::from(world_offset[2]),
    ];
    let base = [
        f32::from(base_rgb[0]) / 255.0,
        f32::from(base_rgb[1]) / 255.0,
        f32::from(base_rgb[2]) / 255.0,
    ];
    let alpha = if translucent { 0.6 } else { 1.0 };
    let raster = &mut framebuffer.raster;
    for quad in &mesh.quads {
        // Fail closed on non-finite geometry: a NaN vertex would slip past
        // every screen-space rejection and poison the depth buffer.
        if !quad.positions.iter().flatten().all(|v| v.is_finite())
            || !quad.normal.iter().all(|v| v.is_finite())
        {
            continue;
        }
        let shade = FACE_SHADE[normal_face_index(rotate_yaw(quad.normal, sin, cos))];
        let albedo = extruded_tint_albedo(quad.tint_index).unwrap_or(base);
        let color = [albedo[0] * shade, albedo[1] * shade, albedo[2] * shade];
        let mut verts = [CamVertex::default(); 4];
        let mut finite = true;
        for (vertex, p) in verts.iter_mut().zip(quad.positions.iter()) {
            let local = rotate_yaw(*p, sin, cos);
            vertex.pos = view.to_camera([
                offset[0] + f64::from(local[0]),
                offset[1] + f64::from(local[1]),
                offset[2] + f64::from(local[2]),
            ]);
            vertex.color = color;
            finite &= vertex.pos.iter().all(|v| v.is_finite());
        }
        if !finite {
            continue;
        }
        for tri in [[0usize, 1, 2], [0, 2, 3]] {
            raster.clipped_triangle(
                &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                translucent,
                alpha,
            );
        }
    }
}

/// Project and depth-test every quad of `mesh` into `framebuffer`.
///
/// Mesh positions are block units in a Y-up local space — entity rigs
/// have feet at `y = 0` and are horizontally centered, BER meshes and 3D
/// crops/ores fill the `0..=1` block volume — placed at `world_offset`
/// and rotated `yaw_rad` about the Y axis through `world_offset`.
/// `base_rgb` shades untinted quads; `tint_index >= 0` quads resolve
/// through [`extruded_tint_albedo`]. Non-finite mesh data, offsets, or
/// camera poses draw nothing.
pub fn render_extruded_mesh_at(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    mesh: &ExtrudedMesh,
    world_offset: [f32; 3],
    yaw_rad: f32,
    base_rgb: [u8; 3],
) {
    render_extruded_mesh_pass(framebuffer, camera, mesh, world_offset, yaw_rad, base_rgb, false);
}

/// Deterministic per-entity-type albedo so captures tell rigs apart: the
/// same golden-ratio hue palette terrain state ids use.
fn entity_base_color(entity_type_id: u16) -> [u8; 3] {
    palette_color(u32::from(entity_type_id)).map(|c| (c * 255.0).round() as u8)
}

/// Render one protocol-767 entity as its procedural articulated rig —
/// [`classify_entity_rig`] + [`build_entity_rig_mesh`] into
/// [`render_extruded_mesh_at`]. Unknown ids draw nothing; `pos` is the
/// entity's feet position and `yaw_rad` its facing in radians (0 = the
/// rig's authored forward). Non-finite poses are sanitized by the rig
/// builder; non-finite positions draw nothing.
pub fn render_entity_rig_facing(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    entity_type_id: u16,
    pos: [f32; 3],
    pose: &EntityPose,
    yaw_rad: f32,
) {
    let Some(spec) = classify_entity_rig(entity_type_id) else {
        return;
    };
    let mesh = build_entity_rig_mesh(spec, *pose);
    render_extruded_mesh_pass(
        framebuffer,
        camera,
        &mesh,
        pos,
        yaw_rad,
        entity_base_color(entity_type_id),
        spec.translucent_shell,
    );
}

/// [`render_entity_rig_facing`] with `yaw_rad = 0`.
pub fn render_entity_rig(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    entity_type_id: u16,
    pos: [f32; 3],
    pose: &EntityPose,
) {
    render_entity_rig_facing(framebuffer, camera, entity_type_id, pos, pose, 0.0);
}

/// Canonical default [`VanillaBerState`] per family for
/// [`render_vanilla_ber`]: closed south-facing chest, idle sign and
/// banner, lit beacon, foot of a bed, floor skull, dark item frame,
/// lit campfire, full brewing stand, bookless lectern, still decorated
/// pot, and closed shulker box.
fn default_vanilla_ber_state(kind: VanillaBerKind) -> VanillaBerState {
    use VanillaBerFacing::*;
    match kind {
        VanillaBerKind::Chest => VanillaBerState::Chest {
            lid_open: 0.0,
            double: false,
            facing: South,
        },
        VanillaBerKind::Sign => VanillaBerState::Sign {
            wall: false,
            hanging: false,
            glowing: false,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Banner => VanillaBerState::Banner {
            wall: false,
            sway_rad: 0.0,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Beacon => VanillaBerState::Beacon {
            beam_height: 8.0,
            active: true,
            rgb: [0.35, 0.6, 1.0],
        },
        VanillaBerKind::Bed => VanillaBerState::Bed {
            is_head: false,
            facing: South,
        },
        VanillaBerKind::Skull => VanillaBerState::Skull {
            wall: false,
            yaw_deg: 0.0,
        },
        VanillaBerKind::ItemFrame => VanillaBerState::ItemFrame {
            facing: South,
            glow: false,
            rotation: 0,
        },
        VanillaBerKind::Campfire => VanillaBerState::Campfire {
            lit: true,
            item_count: 2,
        },
        VanillaBerKind::BrewingStand => VanillaBerState::BrewingStand { bottle_mask: 0b111 },
        VanillaBerKind::Lectern => VanillaBerState::Lectern {
            has_book: false,
            facing: South,
        },
        VanillaBerKind::DecoratedPot => VanillaBerState::DecoratedPot {
            facing: South,
            wobble: 0.0,
        },
        VanillaBerKind::ShulkerBox => VanillaBerState::ShulkerBox {
            open_progress: 0.0,
            facing: South,
        },
    }
}

/// Deterministic albedo per BER family; `tint_index` quads (beam,
/// fittings, contents) override it through [`extruded_tint_albedo`].
fn vanilla_ber_base_color(kind: VanillaBerKind) -> [u8; 3] {
    match kind {
        VanillaBerKind::Chest => [162, 116, 63],
        VanillaBerKind::Sign => [172, 134, 82],
        VanillaBerKind::Banner => [196, 62, 62],
        VanillaBerKind::Beacon => [214, 214, 214],
        VanillaBerKind::Bed => [178, 58, 58],
        VanillaBerKind::Skull => [216, 216, 208],
        VanillaBerKind::ItemFrame => [146, 104, 58],
        VanillaBerKind::Campfire => [104, 72, 48],
        VanillaBerKind::BrewingStand => [88, 70, 108],
        VanillaBerKind::Lectern => [138, 98, 58],
        VanillaBerKind::DecoratedPot => [168, 138, 108],
        VanillaBerKind::ShulkerBox => [138, 92, 158],
    }
}

/// Render one vanilla block entity family at `pos` (the block's min
/// corner) using the canonical default [`VanillaBerState`]. For animated
/// or directional states use [`render_vanilla_ber_state`].
pub fn render_vanilla_ber(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    kind: &VanillaBerKind,
    pos: [f32; 3],
) {
    render_vanilla_ber_state(
        framebuffer,
        camera,
        default_vanilla_ber_state(*kind),
        pos,
        vanilla_ber_base_color(*kind),
    );
}

/// [`render_vanilla_ber`] with an explicit [`VanillaBerState`] (lid angle,
/// facing, beam, contents) and albedo. [`build_ber_mesh`] derives the kind
/// from the state so a mismatched pair cannot occur; an empty mesh draws
/// nothing.
pub fn render_vanilla_ber_state(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    state: VanillaBerState,
    pos: [f32; 3],
    base_rgb: [u8; 3],
) {
    let mesh = build_ber_mesh(state.kind(), state);
    render_extruded_mesh_at(framebuffer, camera, &mesh, pos, 0.0, base_rgb);
}

/// Clip a camera-space segment to the `NEAR_Z..=FAR_Z` depth slab,
/// returning the surviving sub-segment (`None` when fully outside).
fn clip_depth_slab(a: [f64; 3], b: [f64; 3]) -> Option<([f64; 3], [f64; 3])> {
    let dz = b[2] - a[2];
    if dz.abs() < 1e-12 {
        return (NEAR_Z..=FAR_Z).contains(&a[2]).then_some((a, b));
    }
    let t_near = (NEAR_Z - a[2]) / dz;
    let t_far = (FAR_Z - a[2]) / dz;
    let (t_enter, t_exit) = if dz > 0.0 {
        (t_near, t_far)
    } else {
        (t_far, t_near)
    };
    let t0 = t_enter.max(0.0);
    let t1 = t_exit.min(1.0);
    if t0 >= t1 {
        return None;
    }
    let at = |t: f64| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    Some((at(t0), at(t1)))
}

/// Render world-space [`DebugLine`] wireframes — the F3+G chunk-border
/// grid from [`megu_kernel::debug_chords::build_chunk_border_lines`]
/// and the F3+B hitboxes from `build_entity_hitbox_lines` — into
/// `framebuffer`.
///
/// Each segment is clipped to the `[NEAR_Z, FAR_Z]` depth slab, sampled
/// in camera space, projected, and blended at its `rgba` alpha wherever
/// it passes the depth test. Lines never write depth, so wireframes stay
/// visible through each other while terrain and meshes still occlude
/// them. Non-finite endpoints, zero-alpha lines, and non-finite camera
/// poses draw nothing.
pub fn render_debug_lines(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    lines: &[DebugLine],
) {
    let view = OverlayView::new(&camera);
    let raster = &mut framebuffer.raster;
    let (width, height, focal) = (raster.width, raster.height, raster.focal);
    let step_cap = 8 * (width + height).max(1);
    for line in lines {
        if line.rgba[3] == 0 {
            continue;
        }
        let start = [
            f64::from(line.start[0]),
            f64::from(line.start[1]),
            f64::from(line.start[2]),
        ];
        let end = [
            f64::from(line.end[0]),
            f64::from(line.end[1]),
            f64::from(line.end[2]),
        ];
        if !start.iter().chain(end.iter()).all(|v| v.is_finite()) {
            continue;
        }
        let a = view.to_camera(start);
        let b = view.to_camera(end);
        if !a.iter().chain(b.iter()).all(|v| v.is_finite()) {
            continue;
        }
        let Some((a, b)) = clip_depth_slab(a, b) else {
            continue;
        };
        let project = |p: [f64; 3]| -> (f64, f64) {
            let scale = focal / p[2];
            (
                width as f64 * 0.5 + p[0] * scale,
                height as f64 * 0.5 - p[1] * scale,
            )
        };
        let (sx0, sy0) = project(a);
        let (sx1, sy1) = project(b);
        if !(sx0.is_finite() && sy0.is_finite() && sx1.is_finite() && sy1.is_finite()) {
            continue;
        }
        // Sample count follows the screen-space span, clamped to a margin
        // so endpoints far off-screen still yield dense on-screen sampling.
        let margin = |v: f64, dim: usize| v.clamp(-(dim as f64), 2.0 * dim as f64);
        let span = (margin(sx1, width) - margin(sx0, width))
            .abs()
            .max((margin(sy1, height) - margin(sy0, height)).abs());
        let steps = ((span * 2.0).ceil() as usize + 1).clamp(1, step_cap);
        let alpha = f64::from(line.rgba[3]) / 255.0;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            let p = [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ];
            let scale = focal / p[2];
            let sx = width as f64 * 0.5 + p[0] * scale;
            let sy = height as f64 * 0.5 - p[1] * scale;
            if !(sx.is_finite() && sy.is_finite()) {
                continue;
            }
            let ix = sx.floor() as isize;
            let iy = sy.floor() as isize;
            if ix < 0 || iy < 0 || ix >= width as isize || iy >= height as isize {
                continue;
            }
            let idx = iy as usize * width + ix as usize;
            if p[2].partial_cmp(&f64::from(raster.depth[idx]))
                != Some(std::cmp::Ordering::Less)
            {
                continue;
            }
            let dst = idx * 3;
            if alpha >= 1.0 {
                raster.rgb[dst] = line.rgba[0];
                raster.rgb[dst + 1] = line.rgba[1];
                raster.rgb[dst + 2] = line.rgba[2];
            } else {
                for (channel, &src) in line.rgba[..3].iter().enumerate() {
                    let d = f64::from(raster.rgb[dst + channel]);
                    raster.rgb[dst + channel] =
                        (f64::from(src) * alpha + d * (1.0 - alpha)).clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

/// Render camera-facing particle quads — [`VanillaParticleQuad`]s from
/// [`VanillaParticleWorld::billboard_quads`] — as translucent billboards,
/// far-to-near so alpha blending composites in the right order. Quads
/// with non-finite vertices or zero alpha draw nothing.
pub fn render_particle_quads(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    quads: &[VanillaParticleQuad],
) {
    let view = OverlayView::new(&camera);
    let mut sorted: Vec<([CamVertex; 4], f32, f32)> = Vec::with_capacity(quads.len());
    for quad in quads {
        let alpha = quad.color[3].clamp(0.0, 1.0);
        if alpha <= 0.0 || !quad.positions.iter().flatten().all(|v| v.is_finite()) {
            continue;
        }
        let color = [
            quad.color[0].clamp(0.0, 1.0),
            quad.color[1].clamp(0.0, 1.0),
            quad.color[2].clamp(0.0, 1.0),
        ];
        let mut verts = [CamVertex::default(); 4];
        let mut depth_sum = 0.0f64;
        let mut finite = true;
        for (vertex, p) in verts.iter_mut().zip(quad.positions.iter()) {
            vertex.pos = view.to_camera([
                f64::from(p[0]),
                f64::from(p[1]),
                f64::from(p[2]),
            ]);
            vertex.color = color;
            finite &= vertex.pos.iter().all(|v| v.is_finite());
            depth_sum += vertex.pos[2];
        }
        if finite {
            sorted.push((verts, (depth_sum * 0.25) as f32, alpha));
        }
    }
    // Far-to-near: translucent blending composites in draw order.
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let raster = &mut framebuffer.raster;
    for (verts, _, alpha) in sorted {
        for tri in [[0usize, 1, 2], [0, 2, 3]] {
            raster.clipped_triangle(
                &[verts[tri[0]], verts[tri[1]], verts[tri[2]]],
                true,
                alpha,
            );
        }
    }
}

/// Advance-free convenience for [`render_particle_quads`]: billboard
/// `particles` against the camera basis and render them — equivalent to
/// calling [`VanillaParticleWorld::billboard_quads`] followed by
/// [`render_particle_quads`].
pub fn render_vanilla_particles(
    framebuffer: &mut FrameBuffer,
    camera: OverlayCamera,
    particles: &VanillaParticleWorld,
) {
    let (_, right, up) = camera_basis(camera.yaw, camera.pitch);
    let quads = particles.billboard_quads(
        [right[0] as f32, right[1] as f32, right[2] as f32],
        [up[0] as f32, up[1] as f32, up[2] as f32],
    );
    render_particle_quads(framebuffer, camera, &quads);
}

// ---------------------------------------------------------------------------
// PNG encoding (RFC 2083, 8-bit RGB, zlib stored blocks via RFC 1950)
// ---------------------------------------------------------------------------

fn png_chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&megu_core::hash::crc32_ieee(&crc_input).to_be_bytes());
    out
}

/// Encode tightly packed RGB8 pixels as a PNG (color type 2, no interlace).
pub fn encode_png_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let stride = width as usize * 3;
    debug_assert_eq!(rgb.len(), stride * height as usize);
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb.chunks_exact(stride) {
        raw.push(0); // filter type 0: None
        raw.extend_from_slice(row);
    }
    let idat = zlib_compress_stored(&raw);

    let mut out = Vec::with_capacity(8 + 25 + 12 + idat.len() + 12);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor RGB
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&png_chunk(b"IDAT", &idat));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}

// ---------------------------------------------------------------------------
// Tests: graph occlusion culling, bucket culling, multi-draw region batching
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use megu_client_mesh::SECTION_CELLS;

    use crate::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
    use crate::settings::QualityPreset;
    use crate::world::{BlockTable, WorldState};

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
            non_air: if state == 0 { 0 } else { SECTION_CELLS as i16 },
            states: Box::new([state; SECTION_CELLS]),
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

    /// 4x4-chunk terrain: a solid floor layer (y sections 0-1) carved by a
    /// 2x2-section cave pocket at y=1, under open air (y=2-3). Camera sits in
    /// the cave; the sealed underground sections must be occlusion-culled.
    fn cave_world() -> WorldState {
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        for cx in 0..4 {
            for cz in 0..4 {
                let in_cave = (1..=2).contains(&cx) && (1..=2).contains(&cz);
                let sections = vec![
                    uniform_section(1),
                    uniform_section(if in_cave { 0 } else { 1 }),
                    uniform_section(0),
                    uniform_section(0),
                ];
                world.apply_chunk(chunk(cx, cz, sections)).unwrap();
            }
        }
        world
    }

    #[test]
    fn graph_culler_culls_occluded_underground_sections() {
        let mut world = cave_world();
        let eye = [32.0, 24.0, 32.0];
        let cam32 = [32.0, 24.0, 32.0];
        let planes = frustum_planes(cam32, 0.0, 55.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let frame = render_frame(&world, eye, 0.0, 55.0, 160, 90).unwrap();
        assert!(frame.stats.drawn_sections > 0, "no sections drawn");
        // 12 of the 16 sealed floor sections are fully buried (4 form the
        // cave floor and are drawn); the frustum may cull even more.
        assert!(
            frame.stats.culled_sections >= 12,
            "expected >= 12 occluded underground sections, got {}",
            frame.stats.culled_sections
        );
        assert!(frame.stats.triangles_drawn > 0);
        // All drawn sections share one 8x4x8 region; MultiDraw submissions
        // never exceed the per-section call count they replace.
        assert!(frame.stats.region_draw_calls > 0);
        assert!(frame.stats.region_draw_calls <= frame.stats.drawn_sections);
    }

    #[test]
    fn backfacing_direction_buckets_are_skipped() {
        // One floating stone section surrounded by air emits all six axis
        // buckets; the camera below-north sees only Down + North.
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        let mut sections = vec![uniform_section(0); 4];
        sections[2] = uniform_section(1);
        world.apply_chunk(chunk(0, 0, sections)).unwrap();
        let eye = [8.0, 24.0, -16.0];
        let cam32 = [8.0, 24.0, -16.0];
        let planes = frustum_planes(cam32, 0.0, 0.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let frame = render_frame(&world, eye, 0.0, 0.0, 160, 90).unwrap();
        assert_eq!(frame.stats.drawn_sections, 1);
        // Facing = Down|North -> 2 buckets drawn, 4 backfacing culled.
        assert_eq!(frame.stats.buckets_drawn, 2);
        assert_eq!(frame.stats.buckets_culled, 4);
        assert!(frame.stats.region_draw_calls >= 1);
    }

    #[test]
    fn render_regions_batch_sections_into_fewer_draw_calls() {
        // 2x2 chunks, floor slab at y=0 under air: four drawn sections sharing
        // the same facing bucket collapse into one MultiDraw submission.
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        for cx in 0..2 {
            for cz in 0..2 {
                let sections = vec![
                    uniform_section(1),
                    uniform_section(0),
                    uniform_section(0),
                    uniform_section(0),
                ];
                world.apply_chunk(chunk(cx, cz, sections)).unwrap();
            }
        }
        let eye = [16.0, 70.0, 16.0];
        let cam32 = [16.0, 70.0, 16.0];
        let planes = frustum_planes(cam32, 0.0, 60.0, 16.0 / 9.0);
        world
            .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
            .unwrap();

        let frame = render_frame(&world, eye, 0.0, 60.0, 160, 90).unwrap();
        assert!(
            frame.stats.drawn_sections >= 2,
            "expected several drawn floor sections, got {}",
            frame.stats.drawn_sections
        );
        assert!(
            frame.stats.region_draw_calls < frame.stats.drawn_sections,
            "expected batching: {} calls vs {} sections",
            frame.stats.region_draw_calls,
            frame.stats.drawn_sections
        );
        assert!(frame.stats.buckets_drawn > 0);
        // Each floor section also emits back-facing side/bottom buckets that
        // must be skipped while the camera is above the slab.
        assert!(frame.stats.buckets_culled > 0);
    }

    /// `RenderConfig::from_settings` mirrors the resolved `ClientSettings`,
    /// and `RenderConfig::default()` keeps the legacy frame behavior.
    #[test]
    fn render_config_from_settings_maps_all_fields() {
        let mut settings = ClientSettings::from_preset(QualityPreset::Fast);
        settings.fov = 100;
        settings.clouds = CloudMode::Off;
        settings.gpu_backend = RenderBackendPreference::ClassicLowSpec;
        let config = RenderConfig::from_settings(&settings);
        assert_eq!(config.fov_deg, 100.0);
        assert!(!config.smooth_lighting);
        assert_eq!(config.view_distance, Some(8));
        assert_eq!(config.clouds, CloudMode::Off);
        assert_eq!(config.particles, ParticleLevel::Decreased);
        assert!(!config.entity_shadows);
        assert_eq!(config.gpu_backend, RenderBackendPreference::ClassicLowSpec);
        // Fast leaves vsync/max_fps at the shared defaults.
        assert!(config.vsync);
        assert_eq!(config.max_fps, None);
        // Default (Balanced) settings differ from the legacy renderer
        // default only by gaining the explicit distance cap.
        assert_eq!(
            RenderConfig::from_settings(&ClientSettings::default()),
            RenderConfig {
                view_distance: Some(12),
                ..RenderConfig::default()
            }
        );
    }

    /// `decode_vertex` only darkens by per-vertex AO when smooth lighting is
    /// enabled; with it off the face shade is uniform.
    #[test]
    fn smooth_lighting_toggles_vertex_ao() {
        let mut pv = PackedVertex {
            position: 0,
            state_id: 1,
            attributes: 0,
            tint: 0,
        };
        // direction = Up (index 1), ao = 0 (darkest), full sky light.
        pv.attributes = 1 | (0 << ATTR_AO_SHIFT) | (15 << ATTR_SKY_LIGHT_SHIFT);
        let smooth = decode_vertex(&pv, [0.0; 3], true);
        let flat = decode_vertex(&pv, [0.0; 3], false);
        assert!(flat.color[0] > smooth.color[0]);
        // Full sky light + ao 0: smooth shade = base * AO_SHADE[0], flat = base.
        let base = palette_color(1);
        assert_eq!(smooth.color[0], base[0] * AO_SHADE[0]);
        assert_eq!(flat.color[0], base[0]);
    }

    /// The view-distance cap removes in-frustum chunk columns beyond the
    /// resolved distance without affecting nearer sections.
    #[test]
    fn view_distance_caps_sections_beyond_distance() {
        let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
        // A contiguous 9-column strip so the graph culler reaches the far
        // end; solid floor at y sections 0-1 under air.
        for cz in 0..=8 {
            let sections = vec![
                uniform_section(1),
                uniform_section(1),
                uniform_section(0),
                uniform_section(0),
            ];
            world.apply_chunk(chunk(0, cz, sections)).unwrap();
        }
        // Camera just above the floor looking along +z: every column's
        // exposed top faces are inside the frustum, including the ones the
        // view-distance cap must reject.
        let cam32 = [8.0, 40.0, 8.0];
        let planes = frustum_planes(cam32, 0.0, 0.0, 16.0 / 9.0);
        // One call admits only the per-frame upload allowance, so drain the
        // dirty queue like the live loop does before measuring the cap.
        while world.pending_mesh_sections() > 0 {
            world
                .rebuild_dirty_bounded_ordered(usize::MAX, cam32, &planes)
                .unwrap();
        }

        let eye = [8.0, 40.0, 8.0];
        let full = render_frame(&world, eye, 0.0, 0.0, 160, 90).unwrap();
        let settings = ClientSettings {
            view_distance: 6,
            ..ClientSettings::default()
        };
        let config = RenderConfig::from_settings(&settings);
        let limited =
            render_frame_with_config(&world, eye, 0.0, 0.0, 160, 90, &config).unwrap();

        assert!(
            limited.stats.drawn_sections < full.stats.drawn_sections,
            "cap drew {} of {} sections",
            limited.stats.drawn_sections,
            full.stats.drawn_sections
        );
        assert!(limited.stats.culled_sections > full.stats.culled_sections);
    }

    /// Stage 0 (`Full`) returns the exact user settings untouched, and the
    /// budget tracks `1000 / max_fps` (16.67 ms when unlimited).
    #[test]
    fn governor_full_stage_passes_user_settings_through() {
        let governor = FrameBudgetGovernor::new(Some(60));
        assert_eq!(governor.stage(), ThrottleStage::Full);
        assert!((governor.budget_ms() - 1000.0 / 60.0).abs() < 1e-9);
        let budget = governor.effective_budget(16, 96);
        assert_eq!(budget.max_dirty_rebuilds_per_frame, usize::MAX);
        assert_eq!(budget.effective_lod_chunks, 96);
        assert_eq!(budget.effective_view_distance, 16);

        let unlimited = FrameBudgetGovernor::new(None);
        assert!((unlimited.budget_ms() - 1000.0 / 60.0).abs() < 1e-9);
        let capped = FrameBudgetGovernor::new(Some(144));
        assert!((capped.budget_ms() - 1000.0 / 144.0).abs() < 1e-9);
    }

    /// One huge GC/disk-style frame — even one that lingers in the EWMA —
    /// never moves the stage off Full.
    #[test]
    fn governor_ignores_single_frame_spike() {
        let mut governor = FrameBudgetGovernor::new(Some(60));
        for _ in 0..20 {
            governor.observe_frame(Duration::from_millis(8));
        }
        governor.observe_frame(Duration::from_millis(300));
        assert_eq!(governor.stage(), ThrottleStage::Full);
        // Even though the spike keeps the EWMA elevated for several frames,
        // the consecutive-over-budget guard never completes.
        for _ in 0..8 {
            governor.observe_frame(Duration::from_millis(8));
            assert_eq!(governor.stage(), ThrottleStage::Full);
        }
    }

    /// Sustained overload escalates one stage at a time (0 -> 1 -> 2 -> 3),
    /// each stage tightening a different slice of the frame budget.
    #[test]
    fn governor_escalates_through_stages_under_sustained_overload() {
        let mut governor = FrameBudgetGovernor::new(Some(60));
        for (want, rebuilds, lod, view) in [
            (ThrottleStage::TrimRebuilds, 8, 96, 16),
            (ThrottleStage::TrimLod, 4, 48, 16),
            (ThrottleStage::MinBudget, 2, 24, 12),
        ] {
            for _ in 0..3 {
                governor.observe_frame(Duration::from_millis(30));
            }
            assert_eq!(governor.stage(), want);
            let budget = governor.effective_budget(16, 96);
            assert_eq!(budget.max_dirty_rebuilds_per_frame, rebuilds);
            assert_eq!(budget.effective_lod_chunks, lod);
            assert_eq!(budget.effective_view_distance, view);
        }
        // Stage 3 is the ceiling: further overload stays put.
        for _ in 0..6 {
            governor.observe_frame(Duration::from_millis(30));
        }
        assert_eq!(governor.stage(), ThrottleStage::MinBudget);
        // The view-distance floor never drops below 2.
        assert_eq!(governor.effective_budget(4, 8).effective_view_distance, 2);
        assert_eq!(governor.effective_budget(4, 3).effective_lod_chunks, 0);
    }

    /// Once frame time stays comfortably under budget the governor steps
    /// down one stage at a time (3 -> 2 -> 1 -> 0) — never faster.
    #[test]
    fn governor_steps_down_one_stage_at_a_time_on_recovery() {
        let mut governor = FrameBudgetGovernor::new(Some(60));
        for _ in 0..9 {
            governor.observe_frame(Duration::from_millis(30));
        }
        assert_eq!(governor.stage(), ThrottleStage::MinBudget);

        let mut seen = vec![governor.stage()];
        for _ in 0..300 {
            governor.observe_frame(Duration::from_millis(5));
            if governor.stage() != *seen.last().unwrap() {
                seen.push(governor.stage());
            }
        }
        assert_eq!(
            seen,
            vec![
                ThrottleStage::MinBudget,
                ThrottleStage::TrimLod,
                ThrottleStage::TrimRebuilds,
                ThrottleStage::Full,
            ],
            "recovery must walk down exactly one stage at a time"
        );
        let budget = governor.effective_budget(16, 96);
        assert_eq!(budget.max_dirty_rebuilds_per_frame, usize::MAX);
        assert_eq!(budget.effective_view_distance, 16);
    }
}
