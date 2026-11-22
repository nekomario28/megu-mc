// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! CPU-side, fixed-tick visual effect simulation and bounded render payloads.
//! Consumers choose their own shaders and GPU buffers. No texture, shader or
//! gameplay-specific asset is part of this module.

use std::cmp::Reverse;
use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::sync::Arc;

pub const VFX1_CHANNEL: &str = "mc-spine:vfx_v1";
pub const MAX_VFX1_PACKET_BYTES: usize = 64 * 1024;
pub const MAX_VFX1_COMMANDS: usize = 64;
pub const MAX_CURVE_STOPS: usize = 16;
pub const MAX_TRAIL_POINTS: usize = 128;
const MAX_COORD: f32 = 30_000_000.0;
const MAX_PARTICLE_LIFETIME: u16 = 4096;

type Vec3 = [f32; 3];
type Rgba = [f32; 4];

fn finite_between(v: f32, lo: f32, hi: f32) -> bool {
    v.is_finite() && (lo..=hi).contains(&v)
}

fn valid_pos(v: Vec3) -> bool {
    v.into_iter()
        .all(|x| finite_between(x, -MAX_COORD, MAX_COORD))
}

fn valid_vector(v: Vec3, limit: f32) -> bool {
    v.into_iter().all(|x| finite_between(x, -limit, limit))
}

fn valid_color(v: Rgba) -> bool {
    v.into_iter().all(|x| finite_between(x, 0.0, 1.0))
}

fn clamped(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        lo
    }
}

fn normalized_age(t: f32) -> f32 {
    clamped(t, 0.0, 1.0)
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit_or(a: Vec3, fallback: Vec3) -> Vec3 {
    let length_sq = a.iter().map(|v| v * v).sum::<f32>();
    if length_sq.is_finite() && length_sq > 1.0e-12 {
        scale(a, length_sq.sqrt().recip())
    } else {
        fallback
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorStop {
    pub t: f32,
    pub rgba: Rgba,
}

/// Sorted color keys, sampled with linear interpolation. Empty input is white.
#[derive(Clone, Debug, PartialEq)]
pub struct ColorRamp {
    stops: Vec<ColorStop>,
}

impl Default for ColorRamp {
    fn default() -> Self {
        Self::new(vec![ColorStop {
            t: 0.0,
            rgba: [1.0; 4],
        }])
    }
}

impl ColorRamp {
    pub fn new(mut stops: Vec<ColorStop>) -> Self {
        for stop in &mut stops {
            stop.t = normalized_age(stop.t);
            stop.rgba = stop.rgba.map(|x| clamped(x, 0.0, 1.0));
        }
        stops.sort_by(|a, b| a.t.total_cmp(&b.t));
        stops.dedup_by(|b, a| a.t == b.t);
        stops.truncate(MAX_CURVE_STOPS);
        if stops.is_empty() {
            stops.push(ColorStop {
                t: 0.0,
                rgba: [1.0; 4],
            });
        }
        Self { stops }
    }

    pub fn stops(&self) -> &[ColorStop] {
        &self.stops
    }

    pub fn sample(&self, t: f32) -> Rgba {
        let t = normalized_age(t);
        let upper = self.stops.partition_point(|key| key.t < t);
        if upper == 0 {
            return self.stops[0].rgba;
        }
        if upper == self.stops.len() {
            return self.stops[self.stops.len() - 1].rgba;
        }
        let a = self.stops[upper - 1];
        let b = self.stops[upper];
        let weight = (t - a.t) / (b.t - a.t);
        std::array::from_fn(|i| a.rgba[i] + (b.rgba[i] - a.rgba[i]) * weight)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScalarStop {
    pub t: f32,
    pub value: f32,
}

/// Interpolated nonnegative scalar keys, capped at 64 for safe rendering.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarCurve {
    stops: Vec<ScalarStop>,
}

impl Default for ScalarCurve {
    fn default() -> Self {
        Self::new(vec![ScalarStop { t: 0.0, value: 1.0 }])
    }
}

impl ScalarCurve {
    pub fn new(mut stops: Vec<ScalarStop>) -> Self {
        for stop in &mut stops {
            stop.t = normalized_age(stop.t);
            stop.value = clamped(stop.value, 0.0, 64.0);
        }
        stops.sort_by(|a, b| a.t.total_cmp(&b.t));
        stops.dedup_by(|b, a| a.t == b.t);
        stops.truncate(MAX_CURVE_STOPS);
        if stops.is_empty() {
            stops.push(ScalarStop { t: 0.0, value: 1.0 });
        }
        Self { stops }
    }

    pub fn stops(&self) -> &[ScalarStop] {
        &self.stops
    }

    pub fn sample(&self, t: f32) -> f32 {
        let t = normalized_age(t);
        let upper = self.stops.partition_point(|key| key.t < t);
        if upper == 0 {
            return self.stops[0].value;
        }
        if upper == self.stops.len() {
            return self.stops[self.stops.len() - 1].value;
        }
        let a = self.stops[upper - 1];
        let b = self.stops[upper];
        a.value + (b.value - a.value) * (t - a.t) / (b.t - a.t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ParticleOrientation {
    CameraBillboard = 0,
    VelocityStretched = 1,
    FlatGroundDecal = 2,
    AxisAligned = 3,
}

impl TryFrom<u8> for ParticleOrientation {
    type Error = VfxError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::CameraBillboard),
            1 => Ok(Self::VelocityStretched),
            2 => Ok(Self::FlatGroundDecal),
            3 => Ok(Self::AxisAligned),
            _ => Err(VfxError::InvalidValue),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParticleEmitterSpec {
    pub pos: Vec3,
    pub initial_velocity: Vec3,
    pub position_spread: Vec3,
    pub velocity_spread: Vec3,
    pub spawn_rate_per_tick: f32,
    pub particle_lifetime_ticks: u16,
    pub emitter_ttl_ticks: u16,
    pub gravity_per_tick: f32,
    /// Fraction of velocity retained after each tick, between 0 and 1.
    pub drag: f32,
    pub spin_per_tick: f32,
    pub orientation: ParticleOrientation,
    pub color: ColorRamp,
    pub size: ScalarCurve,
    pub priority: u8,
}

impl ParticleEmitterSpec {
    fn validate(&self) -> Result<(), VfxError> {
        if !valid_pos(self.pos)
            || !valid_vector(self.initial_velocity, 1024.0)
            || !valid_vector(self.position_spread, 256.0)
            || !valid_vector(self.velocity_spread, 256.0)
            || !finite_between(self.spawn_rate_per_tick, 0.0, 4096.0)
            || !(1..=MAX_PARTICLE_LIFETIME).contains(&self.particle_lifetime_ticks)
            || self.emitter_ttl_ticks == 0
            || !finite_between(self.gravity_per_tick, -64.0, 64.0)
            || !finite_between(self.drag, 0.0, 1.0)
            || !finite_between(self.spin_per_tick, -128.0, 128.0)
        {
            return Err(VfxError::InvalidValue);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveParticle {
    pub pos: Vec3,
    pub velocity: Vec3,
    pub rotation: f32,
    pub age_ticks: u16,
    pub lifetime_ticks: u16,
    pub orientation: ParticleOrientation,
    pub gravity_per_tick: f32,
    pub drag: f32,
    pub spin_per_tick: f32,
    pub color: Arc<ColorRamp>,
    pub size: Arc<ScalarCurve>,
}

impl ActiveParticle {
    pub fn draw(&self) -> ParticleDraw {
        let t = f32::from(self.age_ticks) / f32::from(self.lifetime_ticks.max(1));
        ParticleDraw {
            pos: self.pos,
            velocity: self.velocity,
            rotation: self.rotation,
            orientation: self.orientation,
            size: self.size.sample(t),
            rgba: self.color.sample(t),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleDraw {
    pub pos: Vec3,
    pub velocity: Vec3,
    pub rotation: f32,
    pub orientation: ParticleOrientation,
    pub size: f32,
    pub rgba: Rgba,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeamEffect {
    pub start: Vec3,
    pub end: Vec3,
    pub width: f32,
    /// Cycles per fixed tick.
    pub pulse_frequency: f32,
    pub core_rgba: Rgba,
    pub glow_rgba: Rgba,
    pub ttl_ticks: u16,
    pub priority: u8,
}

impl BeamEffect {
    fn validate(&self) -> Result<(), VfxError> {
        if !valid_pos(self.start)
            || !valid_pos(self.end)
            || !finite_between(self.width, 0.0, 128.0)
            || !finite_between(self.pulse_frequency, 0.0, 128.0)
            || !valid_color(self.core_rgba)
            || !valid_color(self.glow_rgba)
            || self.ttl_ticks == 0
        {
            return Err(VfxError::InvalidValue);
        }
        Ok(())
    }

    /// Stable endpoints and deterministic pulse multiplier for the renderer.
    pub fn draw(&self, elapsed_ticks: u16) -> BeamDraw {
        let phase = (f64::from(elapsed_ticks) * f64::from(self.pulse_frequency)).fract() as f32;
        BeamDraw {
            start: self.start,
            end: self.end,
            width: self.width * (1.0 + 0.15 * (TAU * phase).sin()),
            core_rgba: self.core_rgba,
            glow_rgba: self.glow_rgba,
        }
    }

    /// Camera-facing core and wider outer-glow quads, each in strip order.
    pub fn quad_vertices(&self, camera_forward: Vec3, elapsed_ticks: u16) -> BeamQuads {
        let draw = self.draw(elapsed_ticks);
        let tangent = unit_or(sub(draw.end, draw.start), [1.0, 0.0, 0.0]);
        let forward = unit_or(camera_forward, [0.0, 0.0, 1.0]);
        let side = unit_or(
            cross(tangent, forward),
            unit_or(cross(tangent, [0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]),
        );
        let vertices_for = |width: f32, rgba: Rgba| {
            let offset = scale(side, width * 0.5);
            [
                RibbonVertex {
                    pos: add(draw.start, offset),
                    rgba,
                    uv: [0.0, 0.0],
                },
                RibbonVertex {
                    pos: sub(draw.start, offset),
                    rgba,
                    uv: [0.0, 1.0],
                },
                RibbonVertex {
                    pos: add(draw.end, offset),
                    rgba,
                    uv: [1.0, 0.0],
                },
                RibbonVertex {
                    pos: sub(draw.end, offset),
                    rgba,
                    uv: [1.0, 1.0],
                },
            ]
        };
        BeamQuads {
            core: vertices_for(draw.width, draw.core_rgba),
            glow: vertices_for(draw.width * 2.5, draw.glow_rgba),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeamDraw {
    pub start: Vec3,
    pub end: Vec3,
    pub width: f32,
    pub core_rgba: Rgba,
    pub glow_rgba: Rgba,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeamQuads {
    pub core: [RibbonVertex; 4],
    pub glow: [RibbonVertex; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrailAnchor {
    pub pos: Vec3,
    pub age_ticks: u16,
}

/// Bounded oldest-first anchor ring. Each consecutive pair is one segment.
#[derive(Clone, Debug, PartialEq)]
pub struct RibbonTrail {
    pub width: f32,
    pub point_lifetime_ticks: u16,
    pub priority: u8,
    pub color: ColorRamp,
    pub width_curve: ScalarCurve,
    max_points: usize,
    points: VecDeque<TrailAnchor>,
}

impl RibbonTrail {
    pub fn new(
        max_points: usize,
        width: f32,
        point_lifetime_ticks: u16,
        color: ColorRamp,
        width_curve: ScalarCurve,
        priority: u8,
    ) -> Result<Self, VfxError> {
        if !(2..=MAX_TRAIL_POINTS).contains(&max_points)
            || !finite_between(width, 0.0, 128.0)
            || !(1..=MAX_PARTICLE_LIFETIME).contains(&point_lifetime_ticks)
        {
            return Err(VfxError::InvalidValue);
        }
        Ok(Self {
            width,
            point_lifetime_ticks,
            priority,
            color,
            width_curve,
            max_points,
            points: VecDeque::with_capacity(max_points),
        })
    }

    pub fn max_points(&self) -> usize {
        self.max_points
    }

    fn validate(&self) -> Result<(), VfxError> {
        if !(2..=MAX_TRAIL_POINTS).contains(&self.max_points)
            || !finite_between(self.width, 0.0, 128.0)
            || !(1..=MAX_PARTICLE_LIFETIME).contains(&self.point_lifetime_ticks)
            || self.points.len() > self.max_points
            || self
                .points
                .iter()
                .any(|p| !valid_pos(p.pos) || p.age_ticks >= self.point_lifetime_ticks)
        {
            return Err(VfxError::InvalidValue);
        }
        Ok(())
    }

    pub fn points(&self) -> &VecDeque<TrailAnchor> {
        &self.points
    }

    pub fn push_point(&mut self, pos: Vec3) -> Result<(), VfxError> {
        if !valid_pos(pos) {
            return Err(VfxError::InvalidValue);
        }
        if self.points.len() == self.max_points {
            self.points.pop_front();
        }
        self.points.push_back(TrailAnchor { pos, age_ticks: 0 });
        Ok(())
    }

    fn tick(&mut self) {
        for anchor in &mut self.points {
            anchor.age_ticks = anchor.age_ticks.saturating_add(1);
        }
        while self
            .points
            .front()
            .is_some_and(|p| p.age_ticks >= self.point_lifetime_ticks)
        {
            self.points.pop_front();
        }
    }

    /// Two vertices per anchor; consecutive pairs form a triangle strip.
    /// The caller supplies the camera forward direction for facing.
    pub fn strip_vertices(&self, camera_forward: Vec3) -> Vec<RibbonVertex> {
        let count = self.points.len();
        if count < 2 {
            return Vec::new();
        }
        let forward = unit_or(camera_forward, [0.0, 0.0, 1.0]);
        let mut vertices = Vec::with_capacity(count * 2);
        for i in 0..count {
            let anchor = self.points[i];
            let direction = if i == 0 {
                sub(self.points[1].pos, anchor.pos)
            } else if i + 1 == count {
                sub(anchor.pos, self.points[i - 1].pos)
            } else {
                sub(self.points[i + 1].pos, self.points[i - 1].pos)
            };
            let tangent = unit_or(direction, [1.0, 0.0, 0.0]);
            let side = unit_or(
                cross(tangent, forward),
                unit_or(cross(tangent, [0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]),
            );
            let spatial_age = (count - 1 - i) as f32 / (count - 1) as f32;
            let temporal_age = f32::from(anchor.age_ticks) / f32::from(self.point_lifetime_ticks);
            let age = spatial_age.max(temporal_age).min(1.0);
            let half_width = 0.5 * self.width * self.width_curve.sample(age) * (1.0 - age);
            let mut rgba = self.color.sample(age);
            rgba[3] *= 1.0 - age;
            let offset = scale(side, half_width);
            let u = i as f32 / (count - 1) as f32;
            vertices.push(RibbonVertex {
                pos: add(anchor.pos, offset),
                rgba,
                uv: [u, 0.0],
            });
            vertices.push(RibbonVertex {
                pos: sub(anchor.pos, offset),
                rgba,
                uv: [u, 1.0],
            });
        }
        vertices
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RibbonVertex {
    pub pos: Vec3,
    pub rgba: Rgba,
    pub uv: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct RibbonDraw {
    pub id: u32,
    pub vertices: Vec<RibbonVertex>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityAura {
    pub entity_id: u64,
    pub pos: Vec3,
    pub inner_radius: f32,
    pub outer_radius: f32,
    pub rgba: Rgba,
    pub ttl_ticks: u16,
    pub priority: u8,
}

impl EntityAura {
    fn validate(&self) -> Result<(), VfxError> {
        if !valid_pos(self.pos)
            || !finite_between(self.inner_radius, 0.0, 1024.0)
            || !finite_between(self.outer_radius, self.inner_radius, 1024.0)
            || !valid_color(self.rgba)
            || self.ttl_ticks == 0
        {
            return Err(VfxError::InvalidValue);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicColoredLight {
    pub pos: Vec3,
    pub rgb: [f32; 3],
    pub radius: f32,
    pub intensity: f32,
    pub ttl_ticks: u16,
    pub priority: u8,
}

impl DynamicColoredLight {
    fn validate(&self) -> Result<(), VfxError> {
        if !valid_pos(self.pos)
            || !self.rgb.into_iter().all(|c| finite_between(c, 0.0, 1.0))
            || !finite_between(self.radius, 0.0, 1024.0)
            || !finite_between(self.intensity, 0.0, 64.0)
            || self.ttl_ticks == 0
        {
            return Err(VfxError::InvalidValue);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VfxFrameOutput {
    pub particles: Vec<ParticleDraw>,
    pub beams: Vec<BeamDraw>,
    pub beam_quads: Vec<BeamQuads>,
    pub ribbons: Vec<RibbonDraw>,
    pub auras: Vec<EntityAura>,
    pub lights: Vec<DynamicColoredLight>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfxHardwareTier {
    LowSpec,
    Balanced,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VfxBudget {
    pub max_active_particles: usize,
    pub max_spawns_per_tick: usize,
    pub max_emitters: usize,
    pub max_beams: usize,
    pub max_ribbons: usize,
    pub max_ribbon_segments: usize,
    pub max_auras: usize,
    pub max_dynamic_lights: usize,
}

impl VfxBudget {
    pub const fn for_tier(tier: VfxHardwareTier) -> Self {
        match tier {
            VfxHardwareTier::LowSpec => Self {
                max_active_particles: 256,
                max_spawns_per_tick: 64,
                max_emitters: 16,
                max_beams: 12,
                max_ribbons: 12,
                max_ribbon_segments: 96,
                max_auras: 16,
                max_dynamic_lights: 8,
            },
            VfxHardwareTier::Balanced => Self {
                max_active_particles: 2048,
                max_spawns_per_tick: 256,
                max_emitters: 64,
                max_beams: 64,
                max_ribbons: 48,
                max_ribbon_segments: 1024,
                max_auras: 64,
                max_dynamic_lights: 32,
            },
            VfxHardwareTier::High => Self {
                max_active_particles: 8192,
                max_spawns_per_tick: 512,
                max_emitters: 128,
                max_beams: 256,
                max_ribbons: 128,
                max_ribbon_segments: 4096,
                max_auras: 128,
                max_dynamic_lights: 128,
            },
        }
    }

    fn bounded(self) -> Self {
        let maximum = Self::for_tier(VfxHardwareTier::High);
        Self {
            max_active_particles: self.max_active_particles.min(maximum.max_active_particles),
            max_spawns_per_tick: self
                .max_spawns_per_tick
                .min(maximum.max_spawns_per_tick)
                .min(self.max_active_particles),
            max_emitters: self.max_emitters.min(maximum.max_emitters),
            max_beams: self.max_beams.min(maximum.max_beams),
            max_ribbons: self.max_ribbons.min(maximum.max_ribbons),
            max_ribbon_segments: self.max_ribbon_segments.min(maximum.max_ribbon_segments),
            max_auras: self.max_auras.min(maximum.max_auras),
            max_dynamic_lights: self.max_dynamic_lights.min(maximum.max_dynamic_lights),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfxError {
    InvalidValue,
}

struct Tracked<T> {
    id: u64,
    priority: u8,
    seq: u64,
    value: T,
}

struct EmitterState {
    spec: ParticleEmitterSpec,
    color: Arc<ColorRamp>,
    size: Arc<ScalarCurve>,
    accumulator: f64,
    spawn_index: u64,
    ticks_left: u16,
}

struct BeamState {
    beam: BeamEffect,
    elapsed: u16,
    ticks_left: u16,
}

/// Fixed-tick VFX state. All retained collections and frame outputs have
/// hard caps, including when callers supply their own budget.
pub struct VfxWorld {
    budget: VfxBudget,
    sequence: u64,
    emitters: Vec<Tracked<EmitterState>>,
    particles: Vec<Tracked<ActiveParticle>>,
    beams: Vec<Tracked<BeamState>>,
    ribbons: Vec<Tracked<RibbonTrail>>,
    auras: Vec<Tracked<EntityAura>>,
    lights: Vec<Tracked<DynamicColoredLight>>,
}

impl VfxWorld {
    pub fn new(tier: VfxHardwareTier) -> Self {
        Self::with_budget(VfxBudget::for_tier(tier))
    }

    pub fn with_budget(budget: VfxBudget) -> Self {
        Self {
            budget: budget.bounded(),
            sequence: 0,
            emitters: Vec::new(),
            particles: Vec::new(),
            beams: Vec::new(),
            ribbons: Vec::new(),
            auras: Vec::new(),
            lights: Vec::new(),
        }
    }

    pub fn budget(&self) -> VfxBudget {
        self.budget
    }

    fn next_sequence(&mut self) -> u64 {
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        seq
    }

    fn admit<T>(list: &mut Vec<Tracked<T>>, cap: usize, entry: Tracked<T>) -> bool {
        if cap == 0 {
            return false;
        }
        if list.len() >= cap {
            let (index, lowest) = list
                .iter()
                .enumerate()
                .min_by_key(|(_, item)| (item.priority, item.seq))
                .expect("nonzero capacity and full list");
            if lowest.priority > entry.priority {
                return false;
            }
            list.remove(index);
        }
        list.push(entry);
        true
    }

    fn upsert<T>(list: &mut Vec<Tracked<T>>, cap: usize, entry: Tracked<T>) -> bool {
        if let Some(index) = list.iter().position(|item| item.id == entry.id) {
            list.remove(index);
        }
        Self::admit(list, cap, entry)
    }

    pub fn spawn_emitter(&mut self, id: u32, spec: ParticleEmitterSpec) -> Result<bool, VfxError> {
        spec.validate()?;
        let ticks_left = spec.emitter_ttl_ticks;
        let entry = Tracked {
            id: u64::from(id),
            priority: spec.priority,
            seq: self.next_sequence(),
            value: EmitterState {
                color: Arc::new(spec.color.clone()),
                size: Arc::new(spec.size.clone()),
                spec,
                accumulator: 0.0,
                spawn_index: 0,
                ticks_left,
            },
        };
        let admitted = Self::upsert(&mut self.emitters, self.budget.max_emitters, entry);
        // High-priority effects receive the finite per-tick spawn allowance first.
        self.emitters.sort_by_key(|e| (Reverse(e.priority), e.seq));
        Ok(admitted)
    }

    pub fn spawn_beam(&mut self, id: u32, beam: BeamEffect) -> Result<bool, VfxError> {
        beam.validate()?;
        let entry = Tracked {
            id: u64::from(id),
            priority: beam.priority,
            seq: self.next_sequence(),
            value: BeamState {
                ticks_left: beam.ttl_ticks,
                beam,
                elapsed: 0,
            },
        };
        Ok(Self::upsert(&mut self.beams, self.budget.max_beams, entry))
    }

    pub fn spawn_ribbon(&mut self, id: u32, ribbon: RibbonTrail) -> Result<bool, VfxError> {
        ribbon.validate()?;
        let entry = Tracked {
            id: u64::from(id),
            priority: ribbon.priority,
            seq: self.next_sequence(),
            value: ribbon,
        };
        let admitted = Self::upsert(&mut self.ribbons, self.budget.max_ribbons, entry);
        self.trim_ribbons();
        Ok(admitted)
    }

    pub fn append_ribbon_point(&mut self, id: u32, pos: Vec3) -> Result<bool, VfxError> {
        if !valid_pos(pos) {
            return Err(VfxError::InvalidValue);
        }
        let Some(entry) = self.ribbons.iter_mut().find(|r| r.id == u64::from(id)) else {
            return Ok(false);
        };
        entry.value.push_point(pos)?;
        self.trim_ribbons();
        Ok(true)
    }

    fn trim_ribbons(&mut self) {
        let mut count: usize = self
            .ribbons
            .iter()
            .map(|r| r.value.points.len().saturating_sub(1))
            .sum();
        while count > self.budget.max_ribbon_segments {
            let Some((index, _)) = self
                .ribbons
                .iter()
                .enumerate()
                .filter(|(_, r)| r.value.points.len() > 1)
                .min_by_key(|(_, r)| (r.priority, r.seq))
            else {
                break;
            };
            self.ribbons[index].value.points.pop_front();
            count -= 1;
        }
    }

    pub fn attach_aura(&mut self, aura: EntityAura) -> Result<bool, VfxError> {
        aura.validate()?;
        let entry = Tracked {
            id: aura.entity_id,
            priority: aura.priority,
            seq: self.next_sequence(),
            value: aura,
        };
        Ok(Self::upsert(&mut self.auras, self.budget.max_auras, entry))
    }

    pub fn set_entity_position(&mut self, entity_id: u64, pos: Vec3) -> Result<bool, VfxError> {
        if !valid_pos(pos) {
            return Err(VfxError::InvalidValue);
        }
        let Some(aura) = self.auras.iter_mut().find(|a| a.id == entity_id) else {
            return Ok(false);
        };
        aura.value.pos = pos;
        Ok(true)
    }

    pub fn spawn_light(&mut self, id: u32, light: DynamicColoredLight) -> Result<bool, VfxError> {
        light.validate()?;
        let entry = Tracked {
            id: u64::from(id),
            priority: light.priority,
            seq: self.next_sequence(),
            value: light,
        };
        Ok(Self::upsert(
            &mut self.lights,
            self.budget.max_dynamic_lights,
            entry,
        ))
    }

    pub fn apply(&mut self, command: VfxCommand) -> Result<bool, VfxError> {
        match command {
            VfxCommand::SpawnEmitter { id, spec } => self.spawn_emitter(id, spec),
            VfxCommand::SpawnBeam { id, beam } => self.spawn_beam(id, beam),
            VfxCommand::SpawnRibbon { id, ribbon } => self.spawn_ribbon(id, ribbon),
            VfxCommand::AttachAura(aura) => self.attach_aura(aura),
            VfxCommand::SpawnLight { id, light } => self.spawn_light(id, light),
        }
    }

    /// Advances one deterministic fixed tick. Existing particles move before
    /// new emission, and no catch-up burst is ever accumulated across frames.
    pub fn tick(&mut self) {
        for particle in &mut self.particles {
            let p = &mut particle.value;
            p.age_ticks = p.age_ticks.saturating_add(1);
            p.velocity[1] -= p.gravity_per_tick;
            p.velocity = scale(p.velocity, p.drag);
            p.pos = add(p.pos, p.velocity);
            p.rotation = (p.rotation + p.spin_per_tick).rem_euclid(TAU);
        }
        self.particles
            .retain(|p| p.value.age_ticks < p.value.lifetime_ticks);

        let mut spawn_allowance = self.budget.max_spawns_per_tick;
        for index in 0..self.emitters.len() {
            let (id, priority) = {
                let e = &self.emitters[index];
                (e.id, e.priority)
            };
            let emitter = &mut self.emitters[index].value;
            emitter.accumulator += f64::from(emitter.spec.spawn_rate_per_tick);
            let requested = (emitter.accumulator.floor() as usize).min(spawn_allowance);
            emitter.accumulator = emitter.accumulator.fract();
            spawn_allowance -= requested;
            for _ in 0..requested {
                let seed = id ^ emitter.spawn_index;
                emitter.spawn_index = emitter.spawn_index.wrapping_add(1);
                let jitter = [
                    random_signed(seed, 0),
                    random_signed(seed, 1),
                    random_signed(seed, 2),
                ];
                let spec = &emitter.spec;
                let particle = ActiveParticle {
                    pos: add(spec.pos, component_mul(jitter, spec.position_spread)),
                    velocity: add(
                        spec.initial_velocity,
                        component_mul(jitter, spec.velocity_spread),
                    ),
                    rotation: 0.0,
                    age_ticks: 0,
                    lifetime_ticks: spec.particle_lifetime_ticks,
                    orientation: spec.orientation,
                    gravity_per_tick: spec.gravity_per_tick,
                    drag: spec.drag,
                    spin_per_tick: spec.spin_per_tick,
                    color: Arc::clone(&emitter.color),
                    size: Arc::clone(&emitter.size),
                };
                let seq = self.sequence;
                self.sequence = self.sequence.wrapping_add(1);
                Self::admit(
                    &mut self.particles,
                    self.budget.max_active_particles,
                    Tracked {
                        id: seq,
                        priority,
                        seq,
                        value: particle,
                    },
                );
            }
            emitter.ticks_left -= 1;
        }
        self.emitters.retain(|e| e.value.ticks_left > 0);

        for beam in &mut self.beams {
            beam.value.elapsed = beam.value.elapsed.saturating_add(1);
            beam.value.ticks_left -= 1;
        }
        self.beams.retain(|b| b.value.ticks_left > 0);

        for ribbon in &mut self.ribbons {
            ribbon.value.tick();
        }
        for aura in &mut self.auras {
            aura.value.ttl_ticks -= 1;
        }
        self.auras.retain(|a| a.value.ttl_ticks > 0);
        for light in &mut self.lights {
            light.value.ttl_ticks -= 1;
        }
        self.lights.retain(|l| l.value.ttl_ticks > 0);
    }

    pub fn frame_output(&self, camera_forward: Vec3) -> VfxFrameOutput {
        VfxFrameOutput {
            particles: self.particles.iter().map(|p| p.value.draw()).collect(),
            beams: self
                .beams
                .iter()
                .map(|b| b.value.beam.draw(b.value.elapsed))
                .collect(),
            beam_quads: self
                .beams
                .iter()
                .map(|b| b.value.beam.quad_vertices(camera_forward, b.value.elapsed))
                .collect(),
            ribbons: self
                .ribbons
                .iter()
                .filter(|r| r.value.points.len() > 1)
                .map(|r| RibbonDraw {
                    id: r.id as u32,
                    vertices: r.value.strip_vertices(camera_forward),
                })
                .collect(),
            auras: self.auras.iter().map(|a| a.value).collect(),
            lights: self.lights.iter().map(|l| l.value).collect(),
        }
    }
}

fn component_mul(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
}

// Local integer mixer gives the same spawn spread on all platforms.
fn random_signed(seed: u64, axis: u64) -> f32 {
    let mut x = seed ^ axis.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    ((x >> 40) as f32 / 16_777_215.0) * 2.0 - 1.0
}

/// One wire command. IDs are stable replacement keys scoped to their type.
#[derive(Clone, Debug, PartialEq)]
pub enum VfxCommand {
    SpawnEmitter { id: u32, spec: ParticleEmitterSpec },
    SpawnBeam { id: u32, beam: BeamEffect },
    SpawnRibbon { id: u32, ribbon: RibbonTrail },
    AttachAura(EntityAura),
    SpawnLight { id: u32, light: DynamicColoredLight },
}

/// VFX1 is a self-delimiting packet carrying at most 64 commands.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vfx1Packet {
    pub commands: Vec<VfxCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfxCodecError {
    InvalidMagic,
    UnexpectedEnd,
    InvalidValue,
    UnknownOpcode(u8),
    TooManyCommands,
    PacketTooLarge,
    TrailingBytes,
}

impl From<VfxError> for VfxCodecError {
    fn from(_: VfxError) -> Self {
        Self::InvalidValue
    }
}

impl Vfx1Packet {
    pub fn encode(&self) -> Result<Vec<u8>, VfxCodecError> {
        if self.commands.len() > MAX_VFX1_COMMANDS {
            return Err(VfxCodecError::TooManyCommands);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"VFX1");
        out.push(self.commands.len() as u8);
        for command in &self.commands {
            encode_command(&mut out, command)?;
            if out.len() > MAX_VFX1_PACKET_BYTES {
                return Err(VfxCodecError::PacketTooLarge);
            }
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, VfxCodecError> {
        if bytes.len() > MAX_VFX1_PACKET_BYTES {
            return Err(VfxCodecError::PacketTooLarge);
        }
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != b"VFX1" {
            return Err(VfxCodecError::InvalidMagic);
        }
        let count = usize::from(reader.u8()?);
        if count > MAX_VFX1_COMMANDS {
            return Err(VfxCodecError::TooManyCommands);
        }
        let mut commands = Vec::with_capacity(count);
        for _ in 0..count {
            commands.push(decode_command(&mut reader)?);
        }
        if reader.remaining() != 0 {
            return Err(VfxCodecError::TrailingBytes);
        }
        Ok(Self { commands })
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_vec3(out: &mut Vec<u8>, value: Vec3) {
    for component in value {
        put_f32(out, component);
    }
}

fn put_rgba(out: &mut Vec<u8>, value: Rgba) {
    for component in value {
        put_f32(out, component);
    }
}

fn put_ramp(out: &mut Vec<u8>, ramp: &ColorRamp) {
    out.push(ramp.stops.len() as u8);
    for stop in &ramp.stops {
        put_f32(out, stop.t);
        put_rgba(out, stop.rgba);
    }
}

fn put_curve(out: &mut Vec<u8>, curve: &ScalarCurve) {
    out.push(curve.stops.len() as u8);
    for stop in &curve.stops {
        put_f32(out, stop.t);
        put_f32(out, stop.value);
    }
}

fn encode_command(out: &mut Vec<u8>, command: &VfxCommand) -> Result<(), VfxCodecError> {
    match command {
        VfxCommand::SpawnEmitter { id, spec } => {
            spec.validate()?;
            out.push(1);
            put_u32(out, *id);
            put_vec3(out, spec.pos);
            put_vec3(out, spec.initial_velocity);
            put_vec3(out, spec.position_spread);
            put_vec3(out, spec.velocity_spread);
            put_f32(out, spec.spawn_rate_per_tick);
            put_u16(out, spec.particle_lifetime_ticks);
            put_u16(out, spec.emitter_ttl_ticks);
            put_f32(out, spec.gravity_per_tick);
            put_f32(out, spec.drag);
            put_f32(out, spec.spin_per_tick);
            out.push(spec.orientation as u8);
            out.push(spec.priority);
            put_ramp(out, &spec.color);
            put_curve(out, &spec.size);
        }
        VfxCommand::SpawnBeam { id, beam } => {
            beam.validate()?;
            out.push(2);
            put_u32(out, *id);
            put_vec3(out, beam.start);
            put_vec3(out, beam.end);
            put_f32(out, beam.width);
            put_f32(out, beam.pulse_frequency);
            put_rgba(out, beam.core_rgba);
            put_rgba(out, beam.glow_rgba);
            put_u16(out, beam.ttl_ticks);
            out.push(beam.priority);
        }
        VfxCommand::SpawnRibbon { id, ribbon } => {
            ribbon.validate()?;
            // An initial spawn carries fresh anchors; aged anchors belong to
            // local fixed-tick simulation and are never transmitted.
            if ribbon.points.iter().any(|p| p.age_ticks != 0) {
                return Err(VfxCodecError::InvalidValue);
            }
            out.push(3);
            put_u32(out, *id);
            put_f32(out, ribbon.width);
            put_u16(out, ribbon.max_points as u16);
            put_u16(out, ribbon.point_lifetime_ticks);
            out.push(ribbon.priority);
            put_ramp(out, &ribbon.color);
            put_curve(out, &ribbon.width_curve);
            out.push(ribbon.points.len() as u8);
            for point in &ribbon.points {
                put_vec3(out, point.pos);
            }
        }
        VfxCommand::AttachAura(aura) => {
            aura.validate()?;
            out.push(4);
            put_u64(out, aura.entity_id);
            put_vec3(out, aura.pos);
            put_f32(out, aura.inner_radius);
            put_f32(out, aura.outer_radius);
            put_rgba(out, aura.rgba);
            put_u16(out, aura.ttl_ticks);
            out.push(aura.priority);
        }
        VfxCommand::SpawnLight { id, light } => {
            light.validate()?;
            out.push(5);
            put_u32(out, *id);
            put_vec3(out, light.pos);
            put_vec3(out, light.rgb);
            put_f32(out, light.radius);
            put_f32(out, light.intensity);
            put_u16(out, light.ttl_ticks);
            out.push(light.priority);
        }
    }
    Ok(())
}

struct Reader<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.cursor
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], VfxCodecError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or(VfxCodecError::UnexpectedEnd)?;
        let bytes = self
            .data
            .get(self.cursor..end)
            .ok_or(VfxCodecError::UnexpectedEnd)?;
        self.cursor = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, VfxCodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, VfxCodecError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("fixed-length slice"),
        ))
    }

    fn u32(&mut self) -> Result<u32, VfxCodecError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed-length slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, VfxCodecError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed-length slice"),
        ))
    }

    fn f32(&mut self) -> Result<f32, VfxCodecError> {
        let value = f32::from_le_bytes(self.take(4)?.try_into().expect("fixed-length slice"));
        if !value.is_finite() {
            return Err(VfxCodecError::InvalidValue);
        }
        Ok(value)
    }

    fn vec3(&mut self) -> Result<Vec3, VfxCodecError> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    fn rgba(&mut self) -> Result<Rgba, VfxCodecError> {
        let result = [self.f32()?, self.f32()?, self.f32()?, self.f32()?];
        if !valid_color(result) {
            return Err(VfxCodecError::InvalidValue);
        }
        Ok(result)
    }

    fn ramp(&mut self) -> Result<ColorRamp, VfxCodecError> {
        let count = usize::from(self.u8()?);
        if !(1..=MAX_CURVE_STOPS).contains(&count) {
            return Err(VfxCodecError::InvalidValue);
        }
        let mut stops = Vec::with_capacity(count);
        let mut previous_t = -1.0;
        for _ in 0..count {
            let t = self.f32()?;
            let rgba = self.rgba()?;
            if !finite_between(t, 0.0, 1.0) || t <= previous_t {
                return Err(VfxCodecError::InvalidValue);
            }
            previous_t = t;
            stops.push(ColorStop { t, rgba });
        }
        Ok(ColorRamp::new(stops))
    }

    fn curve(&mut self) -> Result<ScalarCurve, VfxCodecError> {
        let count = usize::from(self.u8()?);
        if !(1..=MAX_CURVE_STOPS).contains(&count) {
            return Err(VfxCodecError::InvalidValue);
        }
        let mut stops = Vec::with_capacity(count);
        let mut previous_t = -1.0;
        for _ in 0..count {
            let t = self.f32()?;
            let value = self.f32()?;
            if !finite_between(t, 0.0, 1.0) || t <= previous_t || !finite_between(value, 0.0, 64.0)
            {
                return Err(VfxCodecError::InvalidValue);
            }
            previous_t = t;
            stops.push(ScalarStop { t, value });
        }
        Ok(ScalarCurve::new(stops))
    }
}

fn decode_command(reader: &mut Reader<'_>) -> Result<VfxCommand, VfxCodecError> {
    match reader.u8()? {
        1 => {
            let id = reader.u32()?;
            let spec = ParticleEmitterSpec {
                pos: reader.vec3()?,
                initial_velocity: reader.vec3()?,
                position_spread: reader.vec3()?,
                velocity_spread: reader.vec3()?,
                spawn_rate_per_tick: reader.f32()?,
                particle_lifetime_ticks: reader.u16()?,
                emitter_ttl_ticks: reader.u16()?,
                gravity_per_tick: reader.f32()?,
                drag: reader.f32()?,
                spin_per_tick: reader.f32()?,
                orientation: ParticleOrientation::try_from(reader.u8()?)?,
                priority: reader.u8()?,
                color: reader.ramp()?,
                size: reader.curve()?,
            };
            spec.validate()?;
            Ok(VfxCommand::SpawnEmitter { id, spec })
        }
        2 => {
            let id = reader.u32()?;
            let beam = BeamEffect {
                start: reader.vec3()?,
                end: reader.vec3()?,
                width: reader.f32()?,
                pulse_frequency: reader.f32()?,
                core_rgba: reader.rgba()?,
                glow_rgba: reader.rgba()?,
                ttl_ticks: reader.u16()?,
                priority: reader.u8()?,
            };
            beam.validate()?;
            Ok(VfxCommand::SpawnBeam { id, beam })
        }
        3 => {
            let id = reader.u32()?;
            let width = reader.f32()?;
            let max_points = usize::from(reader.u16()?);
            let lifetime_ticks = reader.u16()?;
            let priority = reader.u8()?;
            let color = reader.ramp()?;
            let width_curve = reader.curve()?;
            let count = usize::from(reader.u8()?);
            if count > max_points || count > MAX_TRAIL_POINTS {
                return Err(VfxCodecError::InvalidValue);
            }
            let mut ribbon = RibbonTrail::new(
                max_points,
                width,
                lifetime_ticks,
                color,
                width_curve,
                priority,
            )?;
            for _ in 0..count {
                ribbon.push_point(reader.vec3()?)?;
            }
            Ok(VfxCommand::SpawnRibbon { id, ribbon })
        }
        4 => {
            let aura = EntityAura {
                entity_id: reader.u64()?,
                pos: reader.vec3()?,
                inner_radius: reader.f32()?,
                outer_radius: reader.f32()?,
                rgba: reader.rgba()?,
                ttl_ticks: reader.u16()?,
                priority: reader.u8()?,
            };
            aura.validate()?;
            Ok(VfxCommand::AttachAura(aura))
        }
        5 => {
            let id = reader.u32()?;
            let light = DynamicColoredLight {
                pos: reader.vec3()?,
                rgb: reader.vec3()?,
                radius: reader.f32()?,
                intensity: reader.f32()?,
                ttl_ticks: reader.u16()?,
                priority: reader.u8()?,
            };
            light.validate()?;
            Ok(VfxCommand::SpawnLight { id, light })
        }
        opcode => Err(VfxCodecError::UnknownOpcode(opcode)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp() -> ColorRamp {
        ColorRamp::new(vec![
            ColorStop {
                t: 0.0,
                rgba: [1.0, 0.0, 0.0, 1.0],
            },
            ColorStop {
                t: 1.0,
                rgba: [0.0, 0.0, 1.0, 0.0],
            },
        ])
    }

    fn curve() -> ScalarCurve {
        ScalarCurve::new(vec![
            ScalarStop { t: 0.0, value: 2.0 },
            ScalarStop { t: 1.0, value: 0.0 },
        ])
    }

    fn emitter(orientation: ParticleOrientation) -> ParticleEmitterSpec {
        ParticleEmitterSpec {
            pos: [1.0, 2.0, 3.0],
            initial_velocity: [2.0, 3.0, 0.0],
            position_spread: [0.0; 3],
            velocity_spread: [0.0; 3],
            spawn_rate_per_tick: 1.0,
            particle_lifetime_ticks: 3,
            emitter_ttl_ticks: 3,
            gravity_per_tick: 1.0,
            drag: 0.5,
            spin_per_tick: 0.25,
            orientation,
            color: ramp(),
            size: curve(),
            priority: 1,
        }
    }

    fn beam(priority: u8) -> BeamEffect {
        BeamEffect {
            start: [0.0, 0.0, 0.0],
            end: [3.0, 0.0, 0.0],
            width: 2.0,
            pulse_frequency: 0.25,
            core_rgba: [1.0; 4],
            glow_rgba: [0.2, 0.3, 0.4, 0.5],
            ttl_ticks: 5,
            priority,
        }
    }

    fn ribbon(priority: u8) -> RibbonTrail {
        RibbonTrail::new(3, 2.0, 5, ramp(), curve(), priority).unwrap()
    }

    fn aura() -> EntityAura {
        EntityAura {
            entity_id: 42,
            pos: [2.0, 3.0, 4.0],
            inner_radius: 1.0,
            outer_radius: 2.0,
            rgba: [1.0, 0.0, 0.5, 0.8],
            ttl_ticks: 3,
            priority: 1,
        }
    }

    fn light(priority: u8) -> DynamicColoredLight {
        DynamicColoredLight {
            pos: [3.0, 4.0, 5.0],
            rgb: [0.2, 0.4, 0.8],
            radius: 8.0,
            intensity: 2.0,
            ttl_ticks: 2,
            priority,
        }
    }

    #[test]
    fn color_ramp_interpolates_and_clamps_age() {
        let colors = ramp();
        assert_eq!(colors.sample(0.5), [0.5, 0.0, 0.5, 0.5]);
        assert_eq!(colors.sample(-10.0), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(colors.sample(99.0), [0.0, 0.0, 1.0, 0.0]);
        assert_eq!(colors.sample(f32::NAN), colors.sample(0.0));
        assert_eq!(colors.sample(f32::INFINITY), colors.sample(0.0));
    }

    #[test]
    fn curves_sanitize_invalid_stops_and_sort() {
        let sizes = ScalarCurve::new(vec![
            ScalarStop { t: 1.0, value: 4.0 },
            ScalarStop {
                t: f32::NAN,
                value: -3.0,
            },
            ScalarStop { t: 0.0, value: 1.0 },
        ]);
        assert!(sizes.sample(0.5).is_finite());
        assert!((0.0..=64.0).contains(&sizes.sample(0.5)));
        let colors = ColorRamp::new(vec![ColorStop {
            t: f32::INFINITY,
            rgba: [f32::NAN, -1.0, 99.0, f32::INFINITY],
        }]);
        assert_eq!(colors.sample(0.5), [0.0, 0.0, 1.0, 0.0]);
        assert_eq!(ScalarCurve::new(vec![]).sample(0.4), 1.0);
    }

    #[test]
    fn deterministic_fractional_spawn_rate() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        let mut spec = emitter(ParticleOrientation::CameraBillboard);
        spec.spawn_rate_per_tick = 1.5;
        spec.emitter_ttl_ticks = 4;
        assert!(world.spawn_emitter(1, spec).unwrap());
        world.tick();
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).particles.len(), 1);
        world.tick();
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).particles.len(), 3);
    }

    #[test]
    fn magic_circle_decal_orientation_preserved() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        world
            .spawn_emitter(15, emitter(ParticleOrientation::FlatGroundDecal))
            .unwrap();
        world.tick();
        assert_eq!(
            world.frame_output([0.0, 0.0, 1.0]).particles[0].orientation,
            ParticleOrientation::FlatGroundDecal
        );
    }

    #[test]
    fn physics_gravity_drag_spin_and_expiration() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        let mut spec = emitter(ParticleOrientation::VelocityStretched);
        spec.emitter_ttl_ticks = 1;
        spec.particle_lifetime_ticks = 2;
        world.spawn_emitter(1, spec).unwrap();
        world.tick();
        world.tick();
        let particle = world.frame_output([0.0, 0.0, 1.0]).particles[0];
        assert_eq!(particle.pos, [2.0, 3.0, 3.0]);
        assert_eq!(particle.velocity, [1.0, 1.0, 0.0]);
        assert_eq!(particle.rotation, 0.25);
        assert_eq!(particle.orientation, ParticleOrientation::VelocityStretched);
        world.tick();
        assert!(world.frame_output([0.0, 0.0, 1.0]).particles.is_empty());
    }

    #[test]
    fn deterministic_beam_pulse_and_ttl() {
        let b = beam(1);
        assert_eq!(b.draw(0).width, 2.0);
        assert!((b.draw(1).width - 2.3).abs() < 1e-6);
        assert_eq!(b.draw(1), b.draw(1));
        let quads = b.quad_vertices([0.0, 0.0, 1.0], 1);
        assert_eq!(quads.core[0].rgba, b.core_rgba);
        assert_eq!(quads.glow[0].rgba, b.glow_rgba);
        assert!((quads.core[0].pos[1] - quads.core[1].pos[1]).abs() > 2.0);
        assert!(
            (quads.glow[0].pos[1] - quads.glow[1].pos[1]).abs()
                > (quads.core[0].pos[1] - quads.core[1].pos[1]).abs()
        );
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        world.spawn_beam(1, b).unwrap();
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).beam_quads.len(), 1);
        for _ in 0..5 {
            world.tick();
        }
        assert!(world.frame_output([0.0, 0.0, 1.0]).beams.is_empty());
    }

    #[test]
    fn beam_degenerate_endpoints_still_generate_finite_geometry() {
        let mut effect = beam(1);
        effect.end = effect.start;
        let geometry = effect.quad_vertices([0.0, f32::NAN, 0.0], 12);
        assert!(geometry
            .core
            .iter()
            .chain(geometry.glow.iter())
            .all(|vertex| vertex.pos.iter().all(|value| value.is_finite())));
    }

    #[test]
    fn mutated_ribbon_values_are_rejected_on_admission() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        let mut trail = ribbon(1);
        trail.width = f32::NAN;
        assert_eq!(world.spawn_ribbon(1, trail), Err(VfxError::InvalidValue));
        assert!(world.frame_output([0.0, 0.0, 1.0]).ribbons.is_empty());
    }

    #[test]
    fn ribbon_ring_buffer_strip_and_taper() {
        let mut trail = ribbon(1);
        trail.push_point([0.0, 0.0, 0.0]).unwrap();
        trail.push_point([1.0, 0.0, 0.0]).unwrap();
        trail.push_point([2.0, 0.0, 0.0]).unwrap();
        trail.push_point([3.0, 0.0, 0.0]).unwrap();
        assert_eq!(trail.points().len(), 3);
        assert_eq!(trail.points()[0].pos, [1.0, 0.0, 0.0]);
        let vertices = trail.strip_vertices([0.0, 0.0, 1.0]);
        assert_eq!(vertices.len(), 6);
        assert_eq!(vertices[0].rgba[3], 0.0);
        assert!(vertices[4].rgba[3] > vertices[0].rgba[3]);
        assert_ne!(vertices[4].pos, vertices[5].pos);
        assert_eq!(vertices[0].uv, [0.0, 0.0]);
        assert_eq!(vertices[5].uv, [1.0, 1.0]);
    }

    #[test]
    fn ribbon_collinear_view_direction_remains_finite() {
        let mut trail = ribbon(1);
        trail.push_point([0.0, 0.0, 0.0]).unwrap();
        trail.push_point([1.0, 0.0, 0.0]).unwrap();
        assert!(trail
            .strip_vertices([1.0, 0.0, 0.0])
            .iter()
            .all(|v| v.pos.iter().all(|x| x.is_finite())));
        assert!(trail
            .strip_vertices([f32::NAN, 0.0, 0.0])
            .iter()
            .all(|v| v.pos.iter().all(|x| x.is_finite())));
    }

    #[test]
    fn ribbon_segment_budget_evicts_lower_priority_oldest() {
        let mut budget = VfxBudget::for_tier(VfxHardwareTier::LowSpec);
        budget.max_ribbon_segments = 1;
        let mut world = VfxWorld::with_budget(budget);
        for (id, priority) in [(1, 1), (2, 10)] {
            let mut trail = ribbon(priority);
            trail.push_point([0.0, 0.0, 0.0]).unwrap();
            trail.push_point([1.0, 0.0, 0.0]).unwrap();
            world.spawn_ribbon(id, trail).unwrap();
        }
        let output = world.frame_output([0.0, 0.0, 1.0]);
        assert_eq!(output.ribbons.len(), 1);
        assert_eq!(output.ribbons[0].id, 2);
        assert_eq!(output.ribbons[0].vertices.len(), 4);
    }

    #[test]
    fn aura_is_attached_to_entity_and_expires() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        world.attach_aura(aura()).unwrap();
        assert!(world.set_entity_position(42, [8.0, 2.0, 1.0]).unwrap());
        assert_eq!(
            world.frame_output([0.0, 0.0, 1.0]).auras[0].pos,
            [8.0, 2.0, 1.0]
        );
        for _ in 0..3 {
            world.tick();
        }
        assert!(world.frame_output([0.0, 0.0, 1.0]).auras.is_empty());
    }

    #[test]
    fn colored_light_collection_is_bounded_and_expires() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        for id in 0..12 {
            world.spawn_light(id, light(1)).unwrap();
        }
        let frame = world.frame_output([0.0, 0.0, 1.0]);
        assert_eq!(frame.lights.len(), 8);
        assert_eq!(frame.lights[0].rgb, [0.2, 0.4, 0.8]);
        world.tick();
        world.tick();
        assert!(world.frame_output([0.0, 0.0, 1.0]).lights.is_empty());
    }

    #[test]
    fn low_spec_particle_work_budget_caps_heavy_spam() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        for id in 0..16 {
            let mut spec = emitter(ParticleOrientation::CameraBillboard);
            spec.spawn_rate_per_tick = 4096.0;
            spec.particle_lifetime_ticks = 4096;
            spec.emitter_ttl_ticks = 100;
            world.spawn_emitter(id, spec).unwrap();
        }
        world.tick();
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).particles.len(), 64);
        for _ in 0..10 {
            world.tick();
        }
        assert!(world.frame_output([0.0, 0.0, 1.0]).particles.len() <= 256);
    }

    #[test]
    fn oldest_and_lowest_priority_particle_eviction() {
        let mut budget = VfxBudget::for_tier(VfxHardwareTier::LowSpec);
        budget.max_active_particles = 2;
        budget.max_spawns_per_tick = 2;
        let mut world = VfxWorld::with_budget(budget);
        let mut low = emitter(ParticleOrientation::CameraBillboard);
        low.emitter_ttl_ticks = 1;
        low.particle_lifetime_ticks = 10;
        world.spawn_emitter(1, low).unwrap();
        world.tick();
        let mut high = emitter(ParticleOrientation::CameraBillboard);
        high.pos = [9.0, 0.0, 0.0];
        high.priority = 8;
        high.emitter_ttl_ticks = 3;
        high.particle_lifetime_ticks = 10;
        world.spawn_emitter(2, high).unwrap();
        world.tick();
        world.tick();
        let positions: Vec<_> = world
            .frame_output([0.0, 0.0, 1.0])
            .particles
            .into_iter()
            .map(|p| p.pos[0])
            .collect();
        assert_eq!(positions.len(), 2);
        assert!(positions.iter().all(|x| *x >= 9.0));
    }

    #[test]
    fn low_priority_cannot_replace_high_priority_beam() {
        let mut budget = VfxBudget::for_tier(VfxHardwareTier::LowSpec);
        budget.max_beams = 1;
        let mut world = VfxWorld::with_budget(budget);
        world.spawn_beam(1, beam(9)).unwrap();
        assert!(!world.spawn_beam(2, beam(0)).unwrap());
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).beams.len(), 1);
    }

    #[test]
    fn wire_roundtrip_all_five_variants_and_reencoding() {
        let mut trail = ribbon(2);
        trail.push_point([0.0, 0.0, 0.0]).unwrap();
        trail.push_point([1.0, 2.0, 3.0]).unwrap();
        let packet = Vfx1Packet {
            commands: vec![
                VfxCommand::SpawnEmitter {
                    id: 1,
                    spec: emitter(ParticleOrientation::FlatGroundDecal),
                },
                VfxCommand::SpawnBeam {
                    id: 2,
                    beam: beam(2),
                },
                VfxCommand::SpawnRibbon {
                    id: 3,
                    ribbon: trail,
                },
                VfxCommand::AttachAura(aura()),
                VfxCommand::SpawnLight {
                    id: 4,
                    light: light(3),
                },
            ],
        };
        let bytes = packet.encode().unwrap();
        assert_eq!(&bytes[..4], b"VFX1");
        assert_eq!(VFX1_CHANNEL, "mc-spine:vfx_v1");
        let decoded = Vfx1Packet::decode(&bytes).unwrap();
        assert_eq!(decoded, packet);
        assert_eq!(decoded.encode().unwrap(), bytes);
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        for command in decoded.commands {
            assert!(world.apply(command).unwrap());
        }
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).ribbons.len(), 1);
        assert_eq!(world.frame_output([0.0, 0.0, 1.0]).lights.len(), 1);
    }

    #[test]
    fn hostile_wire_bad_magic_truncation_and_trailing_bytes() {
        let packet = Vfx1Packet {
            commands: vec![VfxCommand::SpawnBeam {
                id: 5,
                beam: beam(1),
            }],
        };
        let valid = packet.encode().unwrap();
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert_eq!(
            Vfx1Packet::decode(&bad_magic),
            Err(VfxCodecError::InvalidMagic)
        );
        assert_eq!(
            Vfx1Packet::decode(&valid[..valid.len() - 1]),
            Err(VfxCodecError::UnexpectedEnd)
        );
        let mut extra = valid;
        extra.push(0);
        assert_eq!(
            Vfx1Packet::decode(&extra),
            Err(VfxCodecError::TrailingBytes)
        );
        assert_eq!(
            Vfx1Packet::decode(b"VFX1\x01\xff"),
            Err(VfxCodecError::UnknownOpcode(255))
        );
        for prefix in 0..extra.len() - 1 {
            assert!(Vfx1Packet::decode(&extra[..prefix]).is_err());
        }
    }

    #[test]
    fn hostile_wire_nan_inf_and_invalid_ranges() {
        let packet = Vfx1Packet {
            commands: vec![VfxCommand::SpawnLight {
                id: 5,
                light: light(1),
            }],
        };
        let mut bytes = packet.encode().unwrap();
        // VFX1 header (5), opcode (1), id (4), then position xyz.
        bytes[10..14].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(Vfx1Packet::decode(&bytes), Err(VfxCodecError::InvalidValue));
        bytes[10..14].copy_from_slice(&f32::INFINITY.to_le_bytes());
        assert_eq!(Vfx1Packet::decode(&bytes), Err(VfxCodecError::InvalidValue));
        bytes[10..14].copy_from_slice(&0.0f32.to_le_bytes());
        // Light rgb begins at byte 22 and must be in [0,1].
        bytes[22..26].copy_from_slice(&(-1.0f32).to_le_bytes());
        assert_eq!(Vfx1Packet::decode(&bytes), Err(VfxCodecError::InvalidValue));
    }

    #[test]
    fn hostile_wire_rejects_counts_and_oversized_packets() {
        assert_eq!(
            Vfx1Packet::decode(b"VFX1\x41"),
            Err(VfxCodecError::TooManyCommands)
        );
        assert_eq!(
            Vfx1Packet::decode(&vec![0; MAX_VFX1_PACKET_BYTES + 1]),
            Err(VfxCodecError::PacketTooLarge)
        );
        let packet = Vfx1Packet {
            commands: vec![VfxCommand::SpawnEmitter {
                id: 5,
                spec: emitter(ParticleOrientation::AxisAligned),
            }],
        };
        let mut bytes = packet.encode().unwrap();
        // VFX1 header + opcode + id + four vectors + seven scalar bytes/fields.
        bytes[80] = (MAX_CURVE_STOPS + 1) as u8;
        assert_eq!(Vfx1Packet::decode(&bytes), Err(VfxCodecError::InvalidValue));
        assert_eq!(
            Vfx1Packet {
                commands: vec![VfxCommand::AttachAura(aura()); MAX_VFX1_COMMANDS + 1],
            }
            .encode(),
            Err(VfxCodecError::TooManyCommands)
        );
    }

    #[test]
    fn invalid_source_values_are_rejected_before_emission_or_encoding() {
        let mut world = VfxWorld::new(VfxHardwareTier::LowSpec);
        let mut bad = emitter(ParticleOrientation::AxisAligned);
        bad.spawn_rate_per_tick = f32::NAN;
        assert_eq!(
            world.spawn_emitter(1, bad.clone()),
            Err(VfxError::InvalidValue)
        );
        assert_eq!(
            Vfx1Packet {
                commands: vec![VfxCommand::SpawnEmitter { id: 1, spec: bad }],
            }
            .encode(),
            Err(VfxCodecError::InvalidValue)
        );
        let mut bad_light = light(0);
        bad_light.intensity = f32::INFINITY;
        assert_eq!(world.spawn_light(1, bad_light), Err(VfxError::InvalidValue));
    }

    #[test]
    fn user_budget_is_always_clamped_to_hard_ceiling() {
        let mut budget = VfxBudget::for_tier(VfxHardwareTier::High);
        budget.max_active_particles = usize::MAX;
        budget.max_spawns_per_tick = usize::MAX;
        budget.max_ribbon_segments = usize::MAX;
        let world = VfxWorld::with_budget(budget);
        assert_eq!(world.budget(), VfxBudget::for_tier(VfxHardwareTier::High));
    }
}
