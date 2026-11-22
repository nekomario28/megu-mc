// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Smooth biome/dimension atmosphere blending and chunk fade-in polish,
//! plus the geometry bridges that turn client-port's layered voxel clouds
//! and 3D cubic planets/suns into `TerrainVertex` buffers.
//!
//! * [`BiomeAtmosphereBlender`] — exponential `dt_sec`-driven lerp of sky
//!   zenith/horizon, fog color/density, and underwater/cave depth tint so
//!   crossing a biome border or ascending into orbit never hard-cuts.
//! * [`ChunkFadeTracker`] — per-`(cx, cz)` arrival ages producing a smooth
//!   `0.0 -> 1.0` fade over a quarter second.
//! * [`voxel_clouds_to_geometry`], [`planet_body_to_geometry`],
//!   [`planet_subworld_transform`] — convert `generate_layered_voxel_clouds`
//!   and `build_3d_planet_mesh` output straight into GPU-ready geometry.

use std::collections::BTreeMap;

use mc_spine_client_mesh::space_sky_bridge::{
    build_3d_planet_mesh, DimensionAdaptationPolicy, PlanetSkyBody, SkyBodyPlacement,
    PLANET_TINT_ATMOSPHERE, PLANET_TINT_RELIEF, PLANET_TINT_RING, PLANET_TINT_SURFACE,
};
use mc_spine_client_mesh::vanilla_plus_3d::ExtrudedMesh;
use mc_spine_client_mesh::vanilla_plus_nature::{
    CloudLighting, VoxelCloudCell, FACE_NEG_X, FACE_NEG_Y, FACE_NEG_Z, FACE_POS_X, FACE_POS_Y,
    FACE_POS_Z,
};

use crate::subworld_render::{box_face_corners, SubWorldTransform, TerrainVertex};

// ---------------------------------------------------------------------------
// Atmosphere parameters + exponential blender
// ---------------------------------------------------------------------------

/// One fully-resolved atmosphere state: what the sky pass, fog pass and
/// lit-geometry shading consume for one frame. All fields are sanitized on
/// write (non-finite reads as the neutral default for that field).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtmosphereParams {
    /// Sky color at the zenith.
    pub sky_zenith_rgb: [f32; 3],
    /// Sky color at the horizon (also the clear color).
    pub sky_horizon_rgb: [f32; 3],
    /// Fog color the distance dissolve blends toward.
    pub fog_rgb: [f32; 3],
    /// Exponential fog density (`0.0` = no haze, vacuum keeps this at 0).
    pub fog_density: f32,
    /// Underwater/cave depth tint multiplied into shaded color.
    pub depth_tint_rgb: [f32; 3],
    /// Depth tint strength `0.0..=1.0` (`0` = dry air).
    pub depth_tint_strength: f32,
    /// Hemisphere ambient light color.
    pub ambient_rgb: [f32; 3],
    /// Directional sun intensity multiplier.
    pub sun_intensity: f32,
}

impl Default for AtmosphereParams {
    /// Overworld noon: vanilla-ish blue sky, mild haze, no depth tint.
    fn default() -> Self {
        Self {
            sky_zenith_rgb: [0.30, 0.55, 0.98],
            sky_horizon_rgb: [0.62, 0.75, 0.92],
            fog_rgb: [0.62, 0.75, 0.92],
            fog_density: 0.0022,
            depth_tint_rgb: [0.45, 0.65, 0.85],
            depth_tint_strength: 0.0,
            ambient_rgb: [0.42, 0.46, 0.55],
            sun_intensity: 1.0,
        }
    }
}

fn clean(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

fn clean_rgb(rgb: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    [
        clean(rgb[0], fallback[0]),
        clean(rgb[1], fallback[1]),
        clean(rgb[2], fallback[2]),
    ]
}

/// Bound a sanitized rgb triple: colors are linear-space light values, so
/// anything outside `0.0..=4.0` (negative albedo, absurd HDR) is clamped —
/// matching the `fog_density`/`sun_intensity` bounds below.
fn clamp_rgb(rgb: [f32; 3]) -> [f32; 3] {
    [
        rgb[0].clamp(0.0, 4.0),
        rgb[1].clamp(0.0, 4.0),
        rgb[2].clamp(0.0, 4.0),
    ]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale3(rgb: [f32; 3], k: f32) -> [f32; 3] {
    [rgb[0] * k, rgb[1] * k, rgb[2] * k]
}

impl AtmosphereParams {
    /// Vacuum/orbit: star-black sky, zero haze, stark sun.
    pub fn vacuum_orbit() -> Self {
        Self {
            sky_zenith_rgb: [0.005, 0.008, 0.02],
            sky_horizon_rgb: [0.01, 0.012, 0.03],
            fog_rgb: [0.01, 0.012, 0.03],
            fog_density: 0.0,
            depth_tint_rgb: [1.0, 1.0, 1.0],
            depth_tint_strength: 0.0,
            ambient_rgb: [0.06, 0.07, 0.10],
            sun_intensity: 1.25,
        }
    }

    /// Underwater: foggy teal with a strong depth tint.
    pub fn underwater() -> Self {
        Self {
            sky_zenith_rgb: [0.05, 0.20, 0.32],
            sky_horizon_rgb: [0.10, 0.30, 0.42],
            fog_rgb: [0.10, 0.30, 0.42],
            fog_density: 0.02,
            depth_tint_rgb: [0.40, 0.70, 0.85],
            depth_tint_strength: 0.6,
            ambient_rgb: [0.20, 0.32, 0.40],
            sun_intensity: 0.55,
        }
    }

    /// The atmosphere a [`DimensionAdaptationPolicy`] implies: vacuum
    /// bodies (orbit / no oxygen / no air transmission) get the stark
    /// orbital look, breathable dimensions get the overworld tinted by the
    /// policy's `sky_tint_rgb`.
    pub fn for_policy(policy: &DimensionAdaptationPolicy) -> Self {
        if policy.is_orbit || !policy.has_oxygen || policy.acoustic_air_transmission < 0.05 {
            let mut params = Self::vacuum_orbit();
            let tint = policy.sky_tint_rgb;
            params.sky_zenith_rgb = scale3(
                params.sky_zenith_rgb,
                0.5 + tint[0].max(tint[1]).max(tint[2]),
            );
            params.sky_horizon_rgb = scale3(
                params.sky_horizon_rgb,
                0.5 + tint[0].max(tint[1]).max(tint[2]),
            );
            params.fog_rgb = params.sky_horizon_rgb;
            params.ambient_rgb = [
                params.ambient_rgb[0] * (0.5 + tint[0]),
                params.ambient_rgb[1] * (0.5 + tint[1]),
                params.ambient_rgb[2] * (0.5 + tint[2]),
            ];
            params
        } else {
            let mut params = Self::default();
            let tint = policy.sky_tint_rgb;
            params.sky_zenith_rgb = [
                (params.sky_zenith_rgb[0] * tint[0]).clamp(0.0, 1.0),
                (params.sky_zenith_rgb[1] * tint[1]).clamp(0.0, 1.0),
                (params.sky_zenith_rgb[2] * tint[2]).clamp(0.0, 1.0),
            ];
            params.sky_horizon_rgb = [
                (params.sky_horizon_rgb[0] * tint[0]).clamp(0.0, 1.0),
                (params.sky_horizon_rgb[1] * tint[1]).clamp(0.0, 1.0),
                (params.sky_horizon_rgb[2] * tint[2]).clamp(0.0, 1.0),
            ];
            params.fog_rgb = params.sky_horizon_rgb;
            // Thinner atmospheres see farther; never thinner than vacuum 0.
            params.fog_density *= policy.acoustic_air_transmission.clamp(0.0, 1.0);
            params
        }
    }

    /// Sanitized copy: non-finite fields read as the neutral value and rgb
    /// channels are bounded to `0.0..=4.0`, so a corrupt biome record cannot
    /// paint NaN or absurd light values into a frame uniform.
    pub fn sanitized(&self) -> Self {
        let neutral = Self::default();
        Self {
            sky_zenith_rgb: clamp_rgb(clean_rgb(self.sky_zenith_rgb, neutral.sky_zenith_rgb)),
            sky_horizon_rgb: clamp_rgb(clean_rgb(self.sky_horizon_rgb, neutral.sky_horizon_rgb)),
            fog_rgb: clamp_rgb(clean_rgb(self.fog_rgb, neutral.fog_rgb)),
            fog_density: clean(self.fog_density, neutral.fog_density).clamp(0.0, 4.0),
            depth_tint_rgb: clamp_rgb(clean_rgb(self.depth_tint_rgb, neutral.depth_tint_rgb)),
            depth_tint_strength: clean(self.depth_tint_strength, 0.0).clamp(0.0, 1.0),
            ambient_rgb: clamp_rgb(clean_rgb(self.ambient_rgb, neutral.ambient_rgb)),
            sun_intensity: clean(self.sun_intensity, 1.0).clamp(0.0, 16.0),
        }
    }
}

/// Exponential time-blend between atmosphere states (biome borders,
/// Overworld -> orbit ascents, submersion). `advance(dt_sec)` moves
/// `current` a fraction `1 - exp(-dt/tau)` of the way to `target` — smooth,
/// monotonic, and guaranteed NaN-free.
#[derive(Clone, Debug)]
pub struct BiomeAtmosphereBlender {
    current: AtmosphereParams,
    target: AtmosphereParams,
    /// Time constant in seconds (`DEFAULT_TAU_SECS` unless overridden).
    pub tau_secs: f32,
}

impl BiomeAtmosphereBlender {
    /// Default blend time constant: ~1.2 s feels smooth without lagging
    /// behind sprint-speed biome crossings.
    pub const DEFAULT_TAU_SECS: f32 = 1.2;

    /// Blender already showing `initial` (no transition in flight).
    pub fn new(initial: AtmosphereParams) -> Self {
        let initial = initial.sanitized();
        Self {
            current: initial,
            target: initial,
            tau_secs: Self::DEFAULT_TAU_SECS,
        }
    }

    /// Point the blend at a new biome/dimension atmosphere.
    pub fn set_target(&mut self, target: AtmosphereParams) {
        self.target = target.sanitized();
    }

    /// The blend destination.
    pub fn target(&self) -> AtmosphereParams {
        self.target
    }

    /// The sanitized atmosphere for this frame (post-`advance`).
    pub fn current(&self) -> AtmosphereParams {
        self.current
    }

    /// `true` when `current` is within `eps` of `target` on every channel.
    /// The channel lists live on the stack — no heap allocation.
    pub fn is_settled(&self, eps: f32) -> bool {
        fn channels(p: &AtmosphereParams) -> [f32; 18] {
            [
                p.sky_zenith_rgb[0],
                p.sky_zenith_rgb[1],
                p.sky_zenith_rgb[2],
                p.sky_horizon_rgb[0],
                p.sky_horizon_rgb[1],
                p.sky_horizon_rgb[2],
                p.fog_rgb[0],
                p.fog_rgb[1],
                p.fog_rgb[2],
                p.fog_density,
                p.depth_tint_rgb[0],
                p.depth_tint_rgb[1],
                p.depth_tint_rgb[2],
                p.depth_tint_strength,
                p.ambient_rgb[0],
                p.ambient_rgb[1],
                p.ambient_rgb[2],
                p.sun_intensity,
            ]
        }
        let current = channels(&self.current);
        let target = channels(&self.target);
        current
            .iter()
            .zip(target.iter())
            .all(|(a, b)| (a - b).abs() <= eps)
    }

    /// Jump straight to `target` (dimension load, respawn).
    pub fn snap_to_target(&mut self) {
        self.current = self.target;
    }

    /// Advance the blend by `dt_sec` frame seconds and return the new
    /// current state. `dt_sec <= 0` or non-finite leaves `current` untouched.
    pub fn advance(&mut self, dt_sec: f32) -> AtmosphereParams {
        let tau = if self.tau_secs.is_finite() && self.tau_secs > 0.0 {
            self.tau_secs
        } else {
            Self::DEFAULT_TAU_SECS
        };
        let t = if dt_sec.is_finite() && dt_sec > 0.0 {
            1.0 - (-dt_sec / tau).exp()
        } else {
            0.0
        };
        if t <= 0.0 {
            return self.current;
        }
        let (c, d) = (self.current, self.target);
        self.current = AtmosphereParams {
            sky_zenith_rgb: lerp3(c.sky_zenith_rgb, d.sky_zenith_rgb, t),
            sky_horizon_rgb: lerp3(c.sky_horizon_rgb, d.sky_horizon_rgb, t),
            fog_rgb: lerp3(c.fog_rgb, d.fog_rgb, t),
            fog_density: c.fog_density + (d.fog_density - c.fog_density) * t,
            depth_tint_rgb: lerp3(c.depth_tint_rgb, d.depth_tint_rgb, t),
            depth_tint_strength: c.depth_tint_strength
                + (d.depth_tint_strength - c.depth_tint_strength) * t,
            ambient_rgb: lerp3(c.ambient_rgb, d.ambient_rgb, t),
            sun_intensity: c.sun_intensity + (d.sun_intensity - c.sun_intensity) * t,
        }
        .sanitized();
        self.current
    }
}

// ---------------------------------------------------------------------------
// Chunk fade-in tracker
// ---------------------------------------------------------------------------

/// Tracks when each `(cx, cz)` chunk column first appeared and produces a
/// smooth `0.0 -> 1.0` alpha ramp so newly arrived chunks dissolve in
/// instead of popping. Ages advance only through `tick(dt_sec)` — no wall
/// clock is ever read.
#[derive(Clone, Debug)]
pub struct ChunkFadeTracker {
    ages: BTreeMap<(i32, i32), f32>,
    /// Seconds a chunk takes to fully fade in (`DEFAULT_FADE_SECONDS`).
    pub fade_seconds: f32,
}

impl Default for ChunkFadeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkFadeTracker {
    /// Vanilla-feel dissolve: a quarter second per newly arrived chunk.
    pub const DEFAULT_FADE_SECONDS: f32 = 0.25;
    /// Hard bound on simultaneously tracked chunk columns. Generous enough
    /// for the largest sane view distance (~64 chunk radius); chunks noted
    /// while the tracker is saturated simply render fully opaque.
    const MAX_TRACKED_CHUNKS: usize = 8192;

    /// Empty tracker with the default fade duration.
    pub fn new() -> Self {
        Self {
            ages: BTreeMap::new(),
            fade_seconds: Self::DEFAULT_FADE_SECONDS,
        }
    }

    /// Register a chunk as just-arrived. Returns `true` when the key was
    /// newly tracked; re-noting an already-fading chunk keeps its progress
    /// (streaming systems re-announce visible chunks every frame).
    ///
    /// The map is bounded: at [`Self::MAX_TRACKED_CHUNKS`] finished fades
    /// are evicted first (they are observationally identical to untracked
    /// chunks), and a still-saturated tracker refuses the note so memory
    /// can never grow without bound across teleports or missed unloads.
    pub fn note_chunk(&mut self, cx: i32, cz: i32) -> bool {
        if self.ages.contains_key(&(cx, cz)) {
            return false;
        }
        if self.ages.len() >= Self::MAX_TRACKED_CHUNKS {
            self.evict_finished();
            if self.ages.len() >= Self::MAX_TRACKED_CHUNKS {
                return false;
            }
        }
        self.ages.insert((cx, cz), 0.0);
        true
    }

    /// The fade duration `fade_alpha`/`is_fading` currently resolve to.
    fn resolved_fade_seconds(&self) -> f32 {
        if self.fade_seconds.is_finite() && self.fade_seconds > 0.0 {
            self.fade_seconds
        } else {
            Self::DEFAULT_FADE_SECONDS
        }
    }

    /// Drop entries whose fade already completed. A finished entry reports
    /// `fade_alpha == 1.0` and `is_fading == false` — exactly like an
    /// untracked chunk — so eviction cannot change rendered output, only
    /// `len()`.
    fn evict_finished(&mut self) {
        let duration = self.resolved_fade_seconds();
        self.ages.retain(|_, age| *age < duration);
    }

    /// Stop tracking a chunk (unload). Returns `true` when it was tracked.
    pub fn forget(&mut self, cx: i32, cz: i32) -> bool {
        self.ages.remove(&(cx, cz)).is_some()
    }

    /// Advance all tracked ages by `dt_sec` frame seconds.
    pub fn tick(&mut self, dt_sec: f32) {
        if !dt_sec.is_finite() || dt_sec <= 0.0 {
            return;
        }
        for age in self.ages.values_mut() {
            *age += dt_sec;
        }
    }

    /// `0.0 -> 1.0` fade for a chunk column. Untracked chunks report `1.0`
    /// so anything that bypasses `note_chunk` stays fully opaque.
    pub fn fade_alpha(&self, cx: i32, cz: i32) -> f32 {
        let duration = self.resolved_fade_seconds();
        match self.ages.get(&(cx, cz)) {
            Some(age) => (age / duration).clamp(0.0, 1.0),
            None => 1.0,
        }
    }

    /// `true` while a chunk is still fading in.
    pub fn is_fading(&self, cx: i32, cz: i32) -> bool {
        self.ages.contains_key(&(cx, cz)) && self.fade_alpha(cx, cz) < 1.0
    }

    /// `true` when any tracked chunk's fade is still animating — the
    /// opaque-terrain RenderBundle path consults this because fade values
    /// bake into the recorded draw uniforms.
    pub fn any_fading(&self) -> bool {
        let duration = self.resolved_fade_seconds();
        self.ages.values().any(|age| *age < duration)
    }

    /// Drop every chunk not accepted by `keep` (world unloads, view-radius
    /// shrink). `keep` receives `(cx, cz)`.
    pub fn retain<F: FnMut((i32, i32)) -> bool>(&mut self, mut keep: F) {
        self.ages.retain(|key, _| keep(*key));
    }

    /// Tracked chunk count.
    pub fn len(&self) -> usize {
        self.ages.len()
    }

    /// `true` when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.ages.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Layered voxel clouds -> TerrainVertex geometry
// ---------------------------------------------------------------------------

/// Global alpha a cloud entry should carry into the translucent lit pass.
pub const CLOUD_RENDER_ALPHA: f32 = 0.8;

const CLOUD_FACE_BITS: [u8; 6] = [
    FACE_POS_X, FACE_NEG_X, FACE_POS_Y, FACE_NEG_Y, FACE_POS_Z, FACE_NEG_Z,
];

/// Sanitize one cloud-lighting channel: `CloudLighting` fields are public,
/// so a hand-built value could carry NaN or negative light into vertex
/// albedo. Non-finite reads as `0.0`, negatives clamp to `0.0`.
fn cloud_channel(v: f32) -> f32 {
    if v.is_finite() {
        v.max(0.0)
    } else {
        0.0
    }
}

/// Convert [`generate_layered_voxel_clouds`] cells into lit geometry: one
/// quad per exposed face, top faces tinted `lighting.top_rgb`, bottoms
/// `bottom_rgb`, sides `side_rgb` (upper strata slightly brighter for the
/// puffy-cap look). Upload the result as a [`SubWorldRenderEntry`] with
/// `alpha = CLOUD_RENDER_ALPHA` and an identity transform — cell positions
/// are already world-space.
///
/// [`generate_layered_voxel_clouds`]: mc_spine_client_mesh::vanilla_plus_nature::generate_layered_voxel_clouds
pub fn voxel_clouds_to_geometry(
    cells: &[VoxelCloudCell],
    lighting: &CloudLighting,
) -> (Vec<TerrainVertex>, Vec<u32>) {
    let quads: usize = cells.iter().map(|c| c.quad_count() as usize).sum();
    let mut vertices = Vec::with_capacity(quads * 4);
    let mut indices = Vec::with_capacity(quads * 6);
    for cell in cells {
        let max = [
            cell.min[0] + cell.size[0],
            cell.min[1] + cell.size[1],
            cell.min[2] + cell.size[2],
        ];
        if !cell.min.iter().chain(max.iter()).all(|v| v.is_finite())
            || !cell.size.iter().all(|s| s.is_finite() && *s > 0.0)
        {
            continue;
        }
        let cap_boost = 1.0 + 0.04 * f32::from(cell.layer);
        let faces = box_face_corners(cell.min, max);
        for (face, (normal, corners)) in faces.iter().enumerate() {
            if cell.exposed_faces & CLOUD_FACE_BITS[face] == 0 {
                continue;
            }
            let rgb = match face {
                2 => lighting.top_rgb,
                3 => lighting.bottom_rgb,
                _ => lighting.side_rgb,
            };
            let albedo = [
                (cloud_channel(rgb[0]) * cap_boost).min(1.0),
                (cloud_channel(rgb[1]) * cap_boost).min(1.0),
                (cloud_channel(rgb[2]) * cap_boost).min(1.0),
            ];
            let base = vertices.len() as u32;
            for (corner, &position) in corners.iter().enumerate() {
                vertices.push(TerrainVertex {
                    position,
                    normal: *normal,
                    albedo,
                    uv: [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]][corner],
                    atlas_index: u32::MAX,
                });
            }
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
    (vertices, indices)
}

// ---------------------------------------------------------------------------
// ExtrudedMesh / cubic planets -> TerrainVertex geometry
// ---------------------------------------------------------------------------

/// Convert any [`ExtrudedMesh`] (`positions`/`normal`/`tint_index` quads)
/// into lit geometry, resolving each `tint_index` through `albedo_of`.
/// `tint_index < 0` (untinted) still queries `albedo_of` so the caller
/// controls the fallback color.
pub fn extruded_mesh_to_geometry(
    mesh: &ExtrudedMesh,
    albedo_of: impl Fn(i32) -> [f32; 3],
) -> (Vec<TerrainVertex>, Vec<u32>) {
    let mut vertices = Vec::with_capacity(mesh.quads.len() * 4);
    let mut indices = Vec::with_capacity(mesh.quads.len() * 6);
    for quad in &mesh.quads {
        // `ExtrudedMesh` fields are public: skip quads whose geometry would
        // put non-finite data into a vertex buffer, and bound the resolved
        // albedo to linear `0..=1`.
        if !quad.positions.iter().flatten().all(|c| c.is_finite())
            || !quad.normal.iter().all(|c| c.is_finite())
            || !quad.uv.iter().flatten().all(|c| c.is_finite())
        {
            continue;
        }
        let albedo = albedo_of(quad.tint_index).map(|c| {
            if c.is_finite() {
                c.clamp(0.0, 1.0)
            } else {
                0.0
            }
        });
        let base = vertices.len() as u32;
        for (&position, &uv) in quad.positions.iter().zip(quad.uv.iter()) {
            vertices.push(TerrainVertex {
                position,
                normal: quad.normal,
                albedo,
                uv,
                // ExtrudedMesh has UVs but no texture identifier. Callers
                // can select a sprite on the returned vertices.
                atlas_index: u32::MAX,
            });
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// Convert a [`PlanetSkyBody`] into GPU-ready lit geometry through the real
/// [`build_3d_planet_mesh`]: surface plates take `surface_rgb`, the
/// atmosphere rim shell takes `rim_rgb` (surface when absent), the ring
/// band takes `ring.rgb` dimmed by its alpha, and relief steps take a
/// slightly darkened surface so the voxel-sphere silhouette reads in shade.
/// Geometry is in `0..=1` local space centred on `(0.5, 0.5, 0.5)` —
/// pair it with [`planet_subworld_transform`] for sky placement.
pub fn planet_body_to_geometry(body: &PlanetSkyBody) -> (Vec<TerrainVertex>, Vec<u32>) {
    let mesh = build_3d_planet_mesh(body);
    let surface = body.effective_surface_rgb();
    let rim = body.effective_rim_rgb().unwrap_or(surface);
    let (ring_rgb, ring_alpha) = body
        .ring
        .map(|r| (r.effective_rgb(), r.effective_alpha()))
        .unwrap_or(([0.0; 3], 0.0));
    let relief = scale3(surface, 0.85);
    extruded_mesh_to_geometry(&mesh, |tint| match tint {
        PLANET_TINT_SURFACE => surface,
        PLANET_TINT_ATMOSPHERE => rim,
        PLANET_TINT_RING => scale3(ring_rgb, ring_alpha.max(0.15)),
        PLANET_TINT_RELIEF => relief,
        _ => surface,
    })
}

/// Sky placement for a body's `0..=1` local-space mesh: the mesh centre is
/// placed `distance` blocks along `placement.direction` and scaled so its
/// silhouette subtends `placement.apparent_angular_radius_deg`.
/// `distance` is the far-plane draw distance the sky pass uses (keep it
/// inside `camera.far`).
pub fn planet_subworld_transform(placement: &SkyBodyPlacement, distance: f32) -> SubWorldTransform {
    let distance = if distance.is_finite() && distance > 1.0 {
        distance
    } else {
        256.0
    };
    // `SkyBodyPlacement` fields are public: a hand-built placement could
    // carry a zero/degenerate or non-finite direction, or a NaN radius
    // (`f32::clamp` propagates NaN). Fall back to a sane sky point and a
    // mid-size apparent radius so the transform can never go non-finite.
    let radius_deg = if placement.apparent_angular_radius_deg.is_finite() {
        placement.apparent_angular_radius_deg.clamp(0.5, 90.0)
    } else {
        8.0
    };
    // Unit cube -> world diameter subtending the apparent angular diameter.
    let scale = 2.0 * distance * (radius_deg.to_radians() * 0.5).tan();
    let d = placement.direction;
    let len_sq = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    let dir = if len_sq.is_finite() && len_sq > 1.0e-12 {
        let inv = len_sq.sqrt().recip();
        [d[0] * inv, d[1] * inv, d[2] * inv]
    } else {
        [0.0, 0.0, -1.0]
    };
    let center = [dir[0] * distance, dir[1] * distance, dir[2] * distance];
    // Centre of the 0..1 mesh lands exactly on the sky-sphere point.
    let half = scale * 0.5;
    SubWorldTransform {
        position: [center[0] - half, center[1] - half, center[2] - half],
        quaternion: [0.0, 0.0, 0.0, 1.0],
        scale,
    }
}
