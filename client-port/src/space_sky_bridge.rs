// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Data-driven 3D cubic planet/orbit sky bridge: a fail-closed coordinator
//! that lets verified space-mod dimensions (Ad Astra, Create: Aeronautics,
//! orbital dimensions) render neighbouring voxel planets, rings and parent
//! bodies, while **never** mis-adapting unknown fantasy dimensions and never
//! double-rendering skies over external sky shaders or sky packs.
//!
//! Three fail-closed rules make mis-adaptation structurally impossible:
//!
//! * **Rule 1 — unknown dimensions stay vanilla.** [`derive_dimension_policy`]
//!   returns [`DimensionAdaptationPolicy::vanilla_safe_default`] whenever the
//!   dimension carries no parsed [`PlanetDimensionMeta`] (Twilight Forest,
//!   The Aether, any unregistered dimension) or the player disabled
//!   auto-adaptation. The default keeps standard clouds, unit wind sway, unit
//!   acoustic air transmission, normal seasons and *zero* synthetic planets.
//! * **Rule 2 — external sky owners always win.** When
//!   [`ExternalOverrideFlags`] reports a custom sky shader or a custom sky mod
//!   pack, `render_builtin_planets` is forced `false` and no sky bodies are
//!   emitted, while non-visual physics/audio cues (vacuum suit muffle, wind,
//!   seasons) still derive from the verified metadata.
//! * **Rule 3 — physical coherence for verified space dimensions.** Vacuum
//!   (`atmosphere_density < 0.05`) disables clouds, wind, seasons and airborne
//!   audio, enables the vacuum suit muffle handed to the spatial audio engine,
//!   and keeps crisp daytime stars; thin atmospheres get gentle wind and
//!   reduced air transmission; breathable planets keep clouds and biome-ruled
//!   seasons; orbit renders the parent body below the player.
//!
//! All geometry reuses [`crate::vanilla_plus_3d::ExtrudedMesh`]: blocky
//! 6-face-lit voxel planets with stepped relief, optional thin atmosphere rim
//! shells and tilted concentric voxel rings — square, layered, unmistakably
//! Minecraft. Everything is pure `std`, deterministic, and reads no wall
//! clock: placements are pure functions of `world_time_ticks` and `player_y`.
//!
//! `gravity_scale` follows the `create-adapter` orbital vocabulary
//! (`1.0` = Overworld, `0.165` = Moon, `0.38` = Mars, `<= 0.05` = orbit); the
//! Ad Astra `gravity` JSON key is accepted as m/s² and normalised by
//! [`SURFACE_GRAVITY_MS2`].

use std::collections::BTreeMap;
use std::f32::consts::PI;

use crate::model_baker::MAX_JSON_DEPTH;
use crate::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad, MAX_UPGRADE_QUADS};

// ---------------------------------------------------------------------------
// Limits and constants
// ---------------------------------------------------------------------------

/// Hard cap on `sky_bodies` per dimension (planets, moons, binary suns).
pub const MAX_SKY_BODIES: usize = 16;

/// Smallest accepted apparent angular radius for a sky body, in degrees.
pub const MIN_ANGULAR_RADIUS_DEG: f32 = 0.5;
/// Largest accepted stored angular radius, in degrees (a 65° body already
/// dominates the sky; the parent-body altitude boost clamps separately).
pub const MAX_ANGULAR_RADIUS_DEG: f32 = 65.0;
/// Largest apparent angular radius a parent body may reach when the player
/// hugs the orbital floor, in degrees.
pub const MAX_APPARENT_ANGULAR_RADIUS_DEG: f32 = 90.0;

/// Densest accepted `atmosphere_density` (1.0 = Earth, 2.5 = Venus-like).
pub const MAX_ATMOSPHERE_DENSITY: f32 = 4.0;
/// Below this density a dimension is a vacuum for sky/weather/audio purposes.
pub const VACUUM_DENSITY_THRESHOLD: f32 = 0.05;
/// At or above this density a dimension supports clouds and full acoustics.
pub const BREATHABLE_DENSITY_THRESHOLD: f32 = 0.50;
/// `gravity_scale` at or below this marks a microgravity/orbital dimension,
/// matching the `0.0..=0.05` orbit band used by the transition router.
pub const ORBIT_GRAVITY_SCALE_MAX: f32 = 0.05;

/// Base gravitational acceleration (m/s²) that a `gravity_scale` of `1.0`
/// stands for — identical to `create_adapter::orbital::SURFACE_GRAVITY`.
/// Ad Astra-style `gravity` JSON fields arrive in m/s² and are divided by
/// this constant.
pub const SURFACE_GRAVITY_MS2: f32 = 9.81;

/// One in-game day; also the default orbit period for a sky body.
pub const TICKS_PER_DAY: u64 = 24_000;
/// Default orbit period when a sky body omits `orbit_period_ticks`.
pub const DEFAULT_ORBIT_PERIOD_TICKS: u64 = TICKS_PER_DAY;
/// Largest accepted `star_brightness_boost`.
pub const MAX_STAR_BRIGHTNESS_BOOST: f32 = 8.0;
/// Largest accepted `gravity_scale` (2.5 covers super-Earths).
pub const MAX_GRAVITY_SCALE: f32 = 2.5;

/// Highest accepted wind sway (dense atmospheres may exceed Earth's 1.0).
pub const MAX_WIND_SWAY_SCALE: f32 = 1.5;

/// Reference thin-atmosphere density the spec examples pin (Mars).
const THIN_REF_DENSITY: f32 = 0.12;
/// Wind sway at the reference thin density.
const THIN_REF_WIND: f32 = 0.20;
/// Acoustic air transmission at the reference thin density.
const THIN_REF_ACOUSTIC: f32 = 0.35;
/// Wind sway where the thin band meets the breathable band (continuous).
const THIN_WIND_MAX: f32 = 0.5;

/// Tint slot on emitted quads carrying the body's `surface_rgb`.
pub const PLANET_TINT_SURFACE: i32 = 0;
/// Tint slot on emitted quads carrying `atmosphere_rim_rgb`.
pub const PLANET_TINT_ATMOSPHERE: i32 = 1;
/// Tint slot on emitted quads carrying `ring.rgb`.
pub const PLANET_TINT_RING: i32 = 2;
/// Tint slot on untinted relief detail (crater/polar-cap shading).
pub const PLANET_TINT_RELIEF: i32 = 3;

/// Altitude halving distance (blocks) for the parent-body apparent-size
/// boost: at `player_y = 0` a parent body appears 2.5x its stored radius,
/// easing smoothly toward 1x as the player climbs.
const PARENT_ALTITUDE_BOOST: f32 = 1.5;
const PARENT_ALTITUDE_FALLOFF: f32 = 192.0;

/// Lateral precession of the below-horizon parent body, in unit-sphere units.
const PARENT_PRECESSION: f32 = 0.2;

// ---------------------------------------------------------------------------
// Data-driven metadata
// ---------------------------------------------------------------------------

/// A flat, stepped pixel-art planetary ring: `inner_scale`/`outer_scale` are
/// multiples of the planet's half-width, `tilt_deg` tips the ring plane about
/// the east-west axis, and `alpha` is the band opacity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanetaryRingSpec {
    /// Inner edge of the ring band, in planet half-widths (`>= 1.0` floats
    /// clear of the surface; clamped `1.0..=8.0`).
    pub inner_scale: f32,
    /// Outer edge of the ring band, in planet half-widths (clamped to
    /// `inner + 0.05..=8.0`).
    pub outer_scale: f32,
    /// Ring-plane tilt about the east-west axis, clamped `-80..=80` degrees.
    pub tilt_deg: f32,
    /// Ring tint, clamped `0..=1` per channel.
    pub rgb: [f32; 3],
    /// Band opacity, clamped `0..=1`.
    pub alpha: f32,
}

impl PlanetaryRingSpec {
    /// Inner scale clamped to `1.0..=8.0`.
    pub fn effective_inner_scale(&self) -> f32 {
        finite_or(self.inner_scale, 1.35).clamp(1.0, 8.0)
    }

    /// Outer scale kept strictly above the effective inner scale.
    pub fn effective_outer_scale(&self) -> f32 {
        finite_or(self.outer_scale, 2.0).clamp(self.effective_inner_scale() + 0.05, 8.0)
    }

    /// Tilt clamped to `-80..=80` degrees.
    pub fn effective_tilt_deg(&self) -> f32 {
        finite_or(self.tilt_deg, 0.0).clamp(-80.0, 80.0)
    }

    /// Alpha clamped to `0..=1` (non-finite falls back to fully transparent
    /// so a corrupt ring cannot render as a solid slab).
    pub fn effective_alpha(&self) -> f32 {
        finite_or(self.alpha, 0.0).clamp(0.0, 1.0)
    }

    /// Ring colour clamped to `0..=1` per channel.
    pub fn effective_rgb(&self) -> [f32; 3] {
        sanitized_rgb(self.rgb)
    }
}

/// One renderable body hanging in the dimension's sky: a neighbouring
/// planet, moon, binary sun, or the below-horizon parent body.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanetSkyBody {
    /// Stable identifier (`"ad_astra:earth"`, `"binary_sun_b"`, ...).
    pub id: String,
    /// Stored angular radius in degrees, clamped `0.5..=65.0` when read via
    /// [`Self::effective_angular_radius_deg`].
    pub angular_radius_deg: f32,
    /// Sidereal orbit period in ticks (`0` = fixed in the sky).
    pub orbit_period_ticks: u64,
    /// Phase offset in `0..=1` of the orbit circle.
    pub orbit_phase_offset: f32,
    /// Orbit plane tilt in degrees; `0` hugs the horizon circle, `90` arcs
    /// through the zenith.
    pub orbit_inclination_deg: f32,
    /// Day-side surface tint, clamped `0..=1` per channel.
    pub surface_rgb: [f32; 3],
    /// Optional thin halo shell tint (`None` = airless body).
    pub atmosphere_rim_rgb: Option<[f32; 3]>,
    /// Optional concentric voxel ring.
    pub ring: Option<PlanetaryRingSpec>,
    /// `true` when this body is the parent planet seen below the player from
    /// its orbit dimension.
    pub is_below_horizon_parent: bool,
}

impl PlanetSkyBody {
    /// Angular radius clamped to `0.5..=65.0` degrees.
    pub fn effective_angular_radius_deg(&self) -> f32 {
        finite_or(self.angular_radius_deg, 8.0)
            .clamp(MIN_ANGULAR_RADIUS_DEG, MAX_ANGULAR_RADIUS_DEG)
    }

    /// Inclination clamped to `-89..=89` degrees.
    pub fn effective_orbit_inclination_deg(&self) -> f32 {
        finite_or(self.orbit_inclination_deg, 0.0).clamp(-89.0, 89.0)
    }

    /// Phase offset wrapped to `0..=1`.
    pub fn effective_orbit_phase_offset(&self) -> f32 {
        finite_or(self.orbit_phase_offset, 0.0).rem_euclid(1.0)
    }

    /// Surface colour clamped to `0..=1` per channel.
    pub fn effective_surface_rgb(&self) -> [f32; 3] {
        sanitized_rgb(self.surface_rgb)
    }

    /// Rim colour clamped to `0..=1` per channel, when present.
    pub fn effective_rim_rgb(&self) -> Option<[f32; 3]> {
        self.atmosphere_rim_rgb.map(sanitized_rgb)
    }
}

/// Verified metadata for one dimension that is *known* to be a planet,
/// moon or orbital shell. Parsing is the gatekeeper: dimensions without a
/// `dimension_id` or with malformed data never produce this struct, and
/// therefore can never be mis-adapted.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanetDimensionMeta {
    /// Canonical dimension id (`"ad_astra:moon"`, `"minecraft:overworld"`).
    pub dimension_id: String,
    /// Gravity multiplier (`1.0` Earth, `0.165` Moon, `0.38` Mars,
    /// `<= 0.05` orbit). Clamped `0.0..=2.5` when read.
    pub gravity_scale: f32,
    /// `0.0` vacuum → `1.0` Earth → up to `2.5` Venus-like; clamped
    /// `0.0..=4.0` when read.
    pub atmosphere_density: f32,
    /// Breathable air flag (Ad Astra `oxygen`).
    pub has_oxygen: bool,
    /// Whether the dimension has a day/night sky-light cycle.
    pub has_sky_light: bool,
    /// Orbital/microgravity shell around a parent body.
    pub is_orbit: bool,
    /// Multiplicative sky tint; `[1,1,1]` leaves the normal colour alone.
    pub sky_tint_rgb: [f32; 3],
    /// Extra star brightness for dark skies, clamped `0..=8.0`.
    pub star_brightness_boost: f32,
    /// The body this dimension orbits (the huge Overworld seen below from
    /// Earth orbit).
    pub parent_body: Option<PlanetSkyBody>,
    /// Neighbouring planets, moons and binary suns, capped at
    /// [`MAX_SKY_BODIES`].
    pub sky_bodies: Vec<PlanetSkyBody>,
}

impl PlanetDimensionMeta {
    /// `gravity_scale` clamped to `0..=2.5` (non-finite falls back to `1.0`
    /// so a corrupt value degrades to Overworld-normal rather than orbit).
    pub fn effective_gravity_scale(&self) -> f32 {
        finite_or(self.gravity_scale, 1.0).clamp(0.0, MAX_GRAVITY_SCALE)
    }

    /// `atmosphere_density` clamped to `0..=4.0` (non-finite falls back to
    /// `0.0`: fail-closed toward vacuum, which only *removes* features).
    pub fn effective_atmosphere_density(&self) -> f32 {
        finite_or(self.atmosphere_density, 0.0).clamp(0.0, MAX_ATMOSPHERE_DENSITY)
    }

    /// Whether this dimension behaves as an orbital shell: declared
    /// `is_orbit` or a microgravity `gravity_scale <= 0.05`.
    pub fn effective_is_orbit(&self) -> bool {
        self.is_orbit || self.effective_gravity_scale() <= ORBIT_GRAVITY_SCALE_MAX
    }

    /// Sky tint clamped to `0..=1` per channel.
    pub fn effective_sky_tint_rgb(&self) -> [f32; 3] {
        sanitized_rgb(self.sky_tint_rgb)
    }

    /// Star boost clamped to `0..=8.0` (non-finite falls back to `1.0`).
    pub fn effective_star_brightness_boost(&self) -> f32 {
        finite_or(self.star_brightness_boost, 1.0).clamp(0.0, MAX_STAR_BRIGHTNESS_BOOST)
    }
}

// ---------------------------------------------------------------------------
// Sky placement
// ---------------------------------------------------------------------------

/// Deterministic placement of one body on the sky sphere. Every field is
/// finite: directions are unit vectors, the apparent radius is positive, and
/// `sun_direction` is the unit vector toward the dimension's sun for
/// day-side lighting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyBodyPlacement {
    /// Unit direction from the player toward the body on the sky sphere.
    pub direction: [f32; 3],
    /// Apparent angular radius in degrees. For a below-horizon parent body
    /// this grows smoothly as `player_y` drops toward the orbital floor.
    pub apparent_angular_radius_deg: f32,
    /// Unit direction toward the dimension's sun at `world_time_ticks`
    /// (sunrise east, noon zenith, midnight nadir — the vanilla cycle).
    pub sun_direction: [f32; 3],
    /// Position along the orbit circle in `0..=1`.
    pub orbit_fraction: f32,
    /// `direction.y > 0` — false for the below-horizon parent.
    pub above_horizon: bool,
}

/// Pure function: where `body` sits on the sky sphere at
/// `world_time_ticks`, viewed from `player_y` blocks above the dimension
/// floor. Zero `orbit_period_ticks` pins the body at its phase offset;
/// non-finite `player_y` is treated as the dimension floor. Never produces
/// NaN or infinity.
pub fn compute_sky_body_placement(
    body: &PlanetSkyBody,
    world_time_ticks: u64,
    player_y: f32,
) -> SkyBodyPlacement {
    let orbit_fraction = if body.orbit_period_ticks == 0 {
        body.effective_orbit_phase_offset()
    } else {
        let t =
            (world_time_ticks % body.orbit_period_ticks) as f32 / body.orbit_period_ticks as f32;
        (t + body.effective_orbit_phase_offset()).rem_euclid(1.0)
    };
    let theta = orbit_fraction * 2.0 * PI;
    let (sin_t, cos_t) = theta.sin_cos();

    let direction = if body.is_below_horizon_parent {
        // The parent planet hangs below the station, slowly precessing so a
        // long orbit still feels like it is going somewhere.
        normalize_or(
            [cos_t * PARENT_PRECESSION, -1.0, sin_t * PARENT_PRECESSION],
            [0.0, -1.0, 0.0],
        )
    } else {
        // Great-circle orbit tilted by the inclination about the east-west
        // axis: inclination 0 circles the horizon, 90 sweeps the zenith.
        let i = body.effective_orbit_inclination_deg().to_radians();
        normalize_or([cos_t, sin_t * i.sin(), sin_t * i.cos()], [1.0, 0.0, 0.0])
    };

    let base_deg = body.effective_angular_radius_deg();
    let apparent = if body.is_below_horizon_parent {
        let altitude = finite_or(player_y, 0.0).max(0.0);
        let boost = 1.0 + PARENT_ALTITUDE_BOOST / (1.0 + altitude / PARENT_ALTITUDE_FALLOFF);
        (base_deg * boost).clamp(MIN_ANGULAR_RADIUS_DEG, MAX_APPARENT_ANGULAR_RADIUS_DEG)
    } else {
        base_deg
    };

    // Vanilla sun cycle: t=0 sunrise east, t=6000 noon zenith, t=12000
    // sunset west, t=18000 midnight nadir.
    let sun_theta = 2.0 * PI * ((world_time_ticks % TICKS_PER_DAY) as f32 / TICKS_PER_DAY as f32);
    let sun_direction = [sun_theta.cos(), sun_theta.sin(), 0.0];

    SkyBodyPlacement {
        direction,
        apparent_angular_radius_deg: apparent,
        sun_direction,
        orbit_fraction,
        above_horizon: direction[1] > 0.0,
    }
}

// ---------------------------------------------------------------------------
// Fail-closed cross-subsystem adaptation
// ---------------------------------------------------------------------------

/// External facts the coordinator cannot observe on its own: who already
/// owns the sky visuals, and whether the player opted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalOverrideFlags {
    /// An Iris/shader pack owns the sky pass (any `gbuffers_skybasic`-class
    /// override).
    pub has_custom_sky_shader: bool,
    /// A resource/mod pack supplies its own skybox or sky geometry
    /// (Stellar View, custom `dimension_sky` packs).
    pub has_custom_sky_mod_pack: bool,
    /// Player opt-in for space-dimension auto-adaptation. `false` pins the
    /// vanilla-safe default no matter how good the metadata is.
    pub player_allow_auto_adapt: bool,
}

impl Default for ExternalOverrideFlags {
    fn default() -> Self {
        Self {
            has_custom_sky_shader: false,
            has_custom_sky_mod_pack: false,
            player_allow_auto_adapt: true,
        }
    }
}

/// The resolved cross-subsystem policy for the current dimension: what the
/// sky renderer, weather/wind pass, seasons engine and spatial audio engine
/// should each do. [`Self::vanilla_safe_default`] is provably identical to
/// vanilla behaviour.
#[derive(Clone, Debug, PartialEq)]
pub struct DimensionAdaptationPolicy {
    /// Whether the built-in cubic planet renderer may draw. Forced `false`
    /// when an external sky shader/pack owns the sky.
    pub render_builtin_planets: bool,
    /// Bodies to render (always empty unless `render_builtin_planets`).
    pub sky_bodies: Vec<PlanetSkyBody>,
    /// The below-horizon parent body, when present and rendering is allowed.
    pub parent_body: Option<PlanetSkyBody>,
    /// Voxel cloud layer on/off (off in vacuum and orbit).
    pub enable_voxel_clouds: bool,
    /// Multiplier on flag/crop/foliage wind sway (`1.0` = vanilla).
    pub wind_sway_scale: f32,
    /// Whether the seasons engine may recolour/snow this dimension.
    pub allow_seasons: bool,
    /// Airborne sound transmission `0..=1` for the spatial audio engine
    /// (`0` = vacuum: no sound travels through air).
    pub acoustic_air_transmission: f32,
    /// Suit/contact-conduction muffle for vacuum dimensions.
    pub vacuum_suit_muffle: bool,
    /// Stars stay visible in daylight (crisp airless-sky stars).
    pub daytime_stars_visible: bool,
    /// Multiplicative sky tint applied by the sky pass.
    pub sky_tint_rgb: [f32; 3],
    /// Star brightness multiplier for the star pass.
    pub star_brightness_boost: f32,
    /// Resolved gravity multiplier, echoed for gameplay consumers.
    pub gravity_scale: f32,
    /// Resolved breathable-air flag, echoed for HUD/audio consumers.
    pub has_oxygen: bool,
    /// Resolved orbital-shell flag, echoed for gameplay consumers.
    pub is_orbit: bool,
}

impl DimensionAdaptationPolicy {
    /// The policy every non-space dimension must get: standard clouds, unit
    /// wind, unit acoustic transmission, seasons per normal biome rules, no
    /// muffle, no daytime stars, and zero synthetic planets.
    pub fn vanilla_safe_default() -> Self {
        Self {
            render_builtin_planets: false,
            sky_bodies: Vec::new(),
            parent_body: None,
            enable_voxel_clouds: true,
            wind_sway_scale: 1.0,
            allow_seasons: true,
            acoustic_air_transmission: 1.0,
            vacuum_suit_muffle: false,
            daytime_stars_visible: false,
            sky_tint_rgb: [1.0, 1.0, 1.0],
            star_brightness_boost: 1.0,
            gravity_scale: 1.0,
            has_oxygen: true,
            is_orbit: false,
        }
    }
}

/// Resolve the policy for a dimension. `meta` is `Some` only when
/// [`load_planet_meta_json`] (or an equivalent verified source) accepted the
/// dimension as a real planet/orbit; anything else gets the vanilla default.
pub fn derive_dimension_policy(
    meta: Option<&PlanetDimensionMeta>,
    flags: ExternalOverrideFlags,
) -> DimensionAdaptationPolicy {
    // Rule 1: unknown dimensions and player opt-out stay exactly vanilla.
    let Some(meta) = meta else {
        return DimensionAdaptationPolicy::vanilla_safe_default();
    };
    if !flags.player_allow_auto_adapt {
        return DimensionAdaptationPolicy::vanilla_safe_default();
    }

    let mut policy = DimensionAdaptationPolicy::vanilla_safe_default();
    let density = meta.effective_atmosphere_density();
    policy.gravity_scale = meta.effective_gravity_scale();
    policy.is_orbit = meta.effective_is_orbit();
    policy.has_oxygen = meta.has_oxygen;
    policy.sky_tint_rgb = meta.effective_sky_tint_rgb();
    policy.star_brightness_boost = meta.effective_star_brightness_boost();

    // Rule 2: external sky owners keep exclusive control of sky visuals;
    // physics/audio cues below still apply.
    let external_sky = flags.has_custom_sky_shader || flags.has_custom_sky_mod_pack;
    policy.render_builtin_planets = !external_sky;
    if policy.render_builtin_planets {
        policy.sky_bodies = meta
            .sky_bodies
            .iter()
            .take(MAX_SKY_BODIES)
            .cloned()
            .collect();
        policy.parent_body = meta.parent_body.clone();
    }

    // Rule 3: physical coherence by atmosphere band.
    if policy.is_orbit || density < VACUUM_DENSITY_THRESHOLD {
        policy.enable_voxel_clouds = false;
        policy.wind_sway_scale = 0.0;
        policy.allow_seasons = false;
        policy.acoustic_air_transmission = 0.0;
        policy.vacuum_suit_muffle = true;
        policy.daytime_stars_visible = true;
    } else if density < BREATHABLE_DENSITY_THRESHOLD {
        policy.enable_voxel_clouds = false;
        policy.wind_sway_scale = (density * (THIN_REF_WIND / THIN_REF_DENSITY)).min(THIN_WIND_MAX);
        policy.allow_seasons = false;
        policy.acoustic_air_transmission =
            (density * (THIN_REF_ACOUSTIC / THIN_REF_DENSITY)).min(1.0);
        policy.vacuum_suit_muffle = false;
        policy.daytime_stars_visible = false;
    } else {
        policy.enable_voxel_clouds = true;
        policy.wind_sway_scale = density.min(MAX_WIND_SWAY_SCALE);
        policy.allow_seasons = meta.has_sky_light;
        policy.acoustic_air_transmission = 1.0;
        policy.vacuum_suit_muffle = false;
        policy.daytime_stars_visible = false;
    }
    policy
}

// ---------------------------------------------------------------------------
// 3D cubic voxel planet geometry
// ---------------------------------------------------------------------------

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    norm(sub(a, b))
}

fn normalize_or(a: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let l = norm(a);
    if l > 1e-12 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        fallback
    }
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

fn sanitized_rgb(rgb: [f32; 3]) -> [f32; 3] {
    [
        finite_or(rgb[0], 0.0).clamp(0.0, 1.0),
        finite_or(rgb[1], 0.0).clamp(0.0, 1.0),
        finite_or(rgb[2], 0.0).clamp(0.0, 1.0),
    ]
}

/// Outward normal of a CCW-wound convex quad, or `None` when degenerate —
/// the same convention the baked-model upload path expects.
fn quad_normal(p: &[[f32; 3]; 4]) -> Option<[f32; 3]> {
    let n = cross(sub(p[1], p[0]), sub(p[2], p[0]));
    let l = norm(n);
    (l > 1e-12).then(|| [n[0] / l, n[1] / l, n[2] / l])
}

/// Push one outward-CCW quad with edge-length UVs; degenerate quads are
/// dropped and emission stops at [`MAX_UPGRADE_QUADS`], matching
/// `vanilla_plus_3d`'s fail-safe bounds.
fn push_quad(mesh: &mut ExtrudedMesh, positions: [[f32; 3]; 4], tint_index: i32) {
    if mesh.quads.len() >= MAX_UPGRADE_QUADS {
        return;
    }
    let Some(normal) = quad_normal(&positions) else {
        return;
    };
    let u = dist(positions[0], positions[1]).min(1.0);
    let v = dist(positions[1], positions[2]).min(1.0);
    mesh.quads.push(ExtrudedQuad {
        positions,
        normal,
        uv: [[0.0, 0.0], [u, 0.0], [u, v], [0.0, v]],
        tint_index,
    });
}

/// Emit all six faces of the axis-aligned box `from..to`, each outward-CCW.
fn push_box6(mesh: &mut ExtrudedMesh, from: [f32; 3], to: [f32; 3], tint_index: i32) {
    let (x0, y0, z0) = (from[0], from[1], from[2]);
    let (x1, y1, z1) = (to[0], to[1], to[2]);
    // +Y / -Y
    push_quad(
        mesh,
        [[x0, y1, z0], [x0, y1, z1], [x1, y1, z1], [x1, y1, z0]],
        tint_index,
    );
    push_quad(
        mesh,
        [[x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1]],
        tint_index,
    );
    // +Z / -Z
    push_quad(
        mesh,
        [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]],
        tint_index,
    );
    push_quad(
        mesh,
        [[x0, y0, z0], [x0, y1, z0], [x1, y1, z0], [x1, y0, z0]],
        tint_index,
    );
    // +X / -X
    push_quad(
        mesh,
        [[x1, y0, z1], [x1, y0, z0], [x1, y1, z0], [x1, y1, z1]],
        tint_index,
    );
    push_quad(
        mesh,
        [[x0, y0, z0], [x0, y0, z1], [x0, y1, z1], [x0, y1, z0]],
        tint_index,
    );
}

/// Emit a plate centred `off` blocks along `axis` from the block centre,
/// `size` wide and `thick` deep — the stepped relief/facet primitive.
fn push_axis_plate(
    mesh: &mut ExtrudedMesh,
    axis: [f32; 3],
    off: f32,
    size: f32,
    thick: f32,
    tint_index: i32,
) {
    let c = [
        0.5 + axis[0] * off,
        0.5 + axis[1] * off,
        0.5 + axis[2] * off,
    ];
    let h = [
        if axis[0] != 0.0 { thick } else { size } * 0.5,
        if axis[1] != 0.0 { thick } else { size } * 0.5,
        if axis[2] != 0.0 { thick } else { size } * 0.5,
    ];
    push_box6(
        mesh,
        [c[0] - h[0], c[1] - h[1], c[2] - h[2]],
        [c[0] + h[0], c[1] + h[1], c[2] + h[2]],
        tint_index,
    );
}

const CARDINALS: [[f32; 3]; 6] = [
    [0.0, -1.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, -1.0],
    [0.0, 0.0, 1.0],
    [-1.0, 0.0, 0.0],
    [1.0, 0.0, 0.0],
];

/// Half-width of the planet core; relief steps reach `0.31`, so ring scales
/// are multiples of this.
const CORE_HALF: f32 = 0.18;

/// Number of concentric square bands in a voxel ring.
const RING_STEPS: usize = 3;

/// Emit one flat square ring band in the horizontal `y = 0.5` plane:
/// `h_out`/`h_in` are outer/inner half-extents, `thick` the slab height.
fn push_ring_band(mesh: &mut ExtrudedMesh, h_out: f32, h_in: f32, thick: f32, tint: i32) {
    let y0 = 0.5 - thick * 0.5;
    let y1 = 0.5 + thick * 0.5;
    let (a, b) = (0.5 - h_out, 0.5 + h_out);
    let (c, d) = (0.5 - h_in, 0.5 + h_in);
    // Four bars around the square annulus.
    push_box6(mesh, [a, y0, a], [b, y1, c], tint);
    push_box6(mesh, [a, y0, d], [b, y1, b], tint);
    push_box6(mesh, [a, y0, c], [c, y1, d], tint);
    push_box6(mesh, [d, y0, c], [b, y1, d], tint);
}

/// Build the flat/stepped concentric voxel ring for `ring` as a separate
/// mesh in the horizontal plane; the caller tilts it.
fn build_ring_mesh(ring: &PlanetaryRingSpec) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let inner = ring.effective_inner_scale() * CORE_HALF;
    let outer = ring.effective_outer_scale() * CORE_HALF;
    let band = (outer - inner) / RING_STEPS as f32;
    for step in 0..RING_STEPS {
        let h_out = outer - band * step as f32;
        let h_in = h_out - band * 0.72; // leave pixel gaps between bands
        push_ring_band(&mut mesh, h_out, h_in.max(inner), 0.03, PLANET_TINT_RING);
    }
    mesh
}

/// Build the 3D cubic voxel planet for `body` in block-local `0..=1` space
/// centred on `(0.5, 0.5, 0.5)`, ready for the same upload path as
/// `vanilla_plus_3d` celestial meshes: a cubic core plus stepped relief
/// plates on all six axes (polar caps / crater steps), an optional thin
/// atmosphere rim shell, and an optional tilted concentric voxel ring.
/// Outward normals are unit and UVs stay in `0..=1`.
pub fn build_3d_planet_mesh(body: &PlanetSkyBody) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();

    // Cubic core.
    push_box6(
        &mut mesh,
        [0.5 - CORE_HALF, 0.5 - CORE_HALF, 0.5 - CORE_HALF],
        [0.5 + CORE_HALF, 0.5 + CORE_HALF, 0.5 + CORE_HALF],
        PLANET_TINT_SURFACE,
    );

    // Stepped relief: three shrinking plates on each axis give the iconic
    // blocky "voxel sphere" silhouette — polar caps and crater steps.
    // (offset from centre, plate size, plate thickness)
    const RELIEF: [(f32, f32, f32); 3] =
        [(0.19, 0.50, 0.07), (0.245, 0.34, 0.07), (0.29, 0.18, 0.06)];
    for axis in CARDINALS {
        for &(off, size, thick) in &RELIEF {
            push_axis_plate(&mut mesh, axis, off, size, thick, PLANET_TINT_SURFACE);
        }
    }

    // Optional thin atmosphere rim shell: six faint plates just outside the
    // relief, on a separate tint slot so the shader can blend it.
    if body.effective_rim_rgb().is_some() {
        for axis in CARDINALS {
            push_axis_plate(&mut mesh, axis, 0.335, 0.78, 0.03, PLANET_TINT_ATMOSPHERE);
            push_axis_plate(&mut mesh, axis, 0.36, 0.56, 0.03, PLANET_TINT_ATMOSPHERE);
        }
    }

    // Optional tilted concentric voxel ring. The flat band is built in the
    // horizontal plane, then rotated about the centre by `tilt_deg` — a
    // pure rotation preserves winding, outwardness and unit normals.
    if let Some(ring) = &body.ring {
        let mut ring_mesh = build_ring_mesh(ring);
        let t = ring.effective_tilt_deg().to_radians();
        let (s, c) = t.sin_cos();
        let m = [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]];
        ring_mesh.affine(m, [0.5, 0.5, 0.5], [0.5, 0.5, 0.5]);
        mesh.append(&ring_mesh);
    }
    mesh
}

// ---------------------------------------------------------------------------
// Fail-closed JSON loader
// ---------------------------------------------------------------------------

/// Minimal JSON value. The parser below is self-contained so this module
/// stays usable without pulling `model_baker`'s private parser.
#[derive(Clone, Debug, PartialEq)]
enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(BTreeMap<String, JVal>),
}

impl JVal {
    fn as_obj(&self) -> Option<&BTreeMap<String, JVal>> {
        match self {
            Self::Obj(m) => Some(m),
            _ => None,
        }
    }

    fn as_arr(&self) -> Option<&[JVal]> {
        match self {
            Self::Arr(v) => Some(v),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct JParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> JParser<'a> {
    fn err(&self, msg: &str) -> String {
        format!("JSON error at byte {}: {msg}", self.i)
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<JVal, String> {
        if depth >= MAX_JSON_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(JVal::Str(self.string()?)),
            Some(b't') => self.literal(b"true", JVal::Bool(true)),
            Some(b'f') => self.literal(b"false", JVal::Bool(false)),
            Some(b'n') => self.literal(b"null", JVal::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    fn literal(&mut self, lit: &[u8], v: JVal) -> Result<JVal, String> {
        if self.b.len() >= self.i + lit.len() && &self.b[self.i..self.i + lit.len()] == lit {
            self.i += lit.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn object(&mut self, depth: usize) -> Result<JVal, String> {
        self.i += 1; // '{'
        let mut map = BTreeMap::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(JVal::Obj(map));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object key"));
            }
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':'"));
            }
            self.i += 1;
            let v = self.value(depth + 1)?;
            map.insert(key, v);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(JVal::Obj(map));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<JVal, String> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(JVal::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(JVal::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // '"'
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => self.escape(&mut out)?,
                0x00..=0x1f => return Err(self.err("unescaped control character")),
                _ => {
                    // Consume the full UTF-8 sequence the lead byte starts.
                    let start = self.i - 1;
                    let len = utf8_len(c).ok_or_else(|| self.err("invalid UTF-8 lead byte"))?;
                    if self.i - 1 + len > self.b.len() {
                        return Err(self.err("truncated UTF-8"));
                    }
                    self.i = start + len;
                    out.push_str(
                        std::str::from_utf8(&self.b[start..self.i])
                            .map_err(|_| self.err("invalid UTF-8"))?,
                    );
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        let Some(c) = self.peek() else {
            return Err(self.err("truncated escape"));
        };
        self.i += 1;
        match c {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let hi = self.hex4()?;
                let cp = if (0xd800..0xdc00).contains(&hi) {
                    if self.peek() == Some(b'\\') {
                        self.i += 1;
                        if self.peek() != Some(b'u') {
                            return Err(self.err("expected low surrogate escape"));
                        }
                        self.i += 1;
                        let lo = self.hex4()?;
                        if !(0xdc00..0xe000).contains(&lo) {
                            return Err(self.err("invalid low surrogate"));
                        }
                        0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                    } else {
                        return Err(self.err("unpaired surrogate"));
                    }
                } else {
                    hi
                };
                out.push(char::from_u32(cp).ok_or_else(|| self.err("invalid scalar value"))?);
            }
            _ => return Err(self.err("invalid escape")),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(c) = self.peek() else {
                return Err(self.err("truncated \\u escape"));
            };
            let d = (c as char)
                .to_digit(16)
                .ok_or_else(|| self.err("bad hex digit"))?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<JVal, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zeros are not valid JSON"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
            }
            _ => return Err(self.err("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected a digit in the exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        let text =
            std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("invalid number"))?;
        let v: f64 = text.parse().map_err(|_| self.err("invalid number"))?;
        if !v.is_finite() {
            return Err(self.err("number is out of range"));
        }
        Ok(JVal::Num(v))
    }

    fn finish(&mut self) -> Result<(), String> {
        self.ws();
        if self.i != self.b.len() {
            return Err(self.err("trailing content after JSON value"));
        }
        Ok(())
    }
}

fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

fn parse_json(text: &str) -> Result<JVal, String> {
    let mut p = JParser {
        b: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.finish()?;
    Ok(v)
}

// ---- field extraction helpers: present-but-wrong-type is an error --------

fn get<'a>(obj: &'a BTreeMap<String, JVal>, keys: &[&str]) -> Option<&'a JVal> {
    keys.iter().find_map(|k| obj.get(*k))
}

/// Required string field: missing *or* non-string/empty is an error.
fn req_str(obj: &BTreeMap<String, JVal>, keys: &[&str], field: &str) -> Result<String, String> {
    match get(obj, keys) {
        Some(JVal::Str(s)) if !s.is_empty() => Ok(s.clone()),
        Some(_) => Err(format!("field `{field}` must be a non-empty string")),
        None => Err(format!("missing required field `{field}`")),
    }
}

/// Optional number: absent → `None`; present-but-not-a-number → `Err`.
fn opt_num(
    obj: &BTreeMap<String, JVal>,
    keys: &[&str],
    field: &str,
) -> Result<Option<f64>, String> {
    match get(obj, keys) {
        None | Some(JVal::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("field `{field}` must be a number")),
    }
}

/// Optional bool: absent → `None`; present-but-not-a-bool → `Err`.
fn opt_bool(
    obj: &BTreeMap<String, JVal>,
    keys: &[&str],
    field: &str,
) -> Result<Option<bool>, String> {
    match get(obj, keys) {
        None | Some(JVal::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("field `{field}` must be a boolean")),
    }
}

/// Parse an RGB field given as `[r,g,b]` (0..=1 or 0..=255 scale) or a
/// `"#rrggbb"` string; absent → `Ok(None)`, wrong type → `Err`.
fn opt_rgb(
    obj: &BTreeMap<String, JVal>,
    keys: &[&str],
    field: &str,
) -> Result<Option<[f32; 3]>, String> {
    let Some(v) = get(obj, keys) else {
        return Ok(None);
    };
    if let JVal::Str(s) = v {
        let h = s.strip_prefix('#').unwrap_or(s);
        if h.len() == 6 {
            let n = u32::from_str_radix(h, 16)
                .map_err(|_| format!("field `{field}` is not a hex colour"))?;
            return Ok(Some([
                ((n >> 16) & 0xff) as f32 / 255.0,
                ((n >> 8) & 0xff) as f32 / 255.0,
                (n & 0xff) as f32 / 255.0,
            ]));
        }
        return Err(format!("field `{field}` must be [r,g,b] or \"#rrggbb\""));
    }
    let arr = v
        .as_arr()
        .ok_or_else(|| format!("field `{field}` must be [r,g,b] or \"#rrggbb\""))?;
    if arr.len() != 3 {
        return Err(format!("field `{field}` must have exactly 3 channels"));
    }
    let mut rgb = [0.0f32; 3];
    let mut max = 0.0f64;
    for (i, c) in arr.iter().enumerate() {
        let n = c
            .as_f64()
            .ok_or_else(|| format!("field `{field}` channel {i} must be a number"))?;
        max = max.max(n);
        rgb[i] = n as f32;
    }
    // Allow both 0..=1 and 0..=255 authored values.
    if max > 1.0 {
        for c in &mut rgb {
            *c /= 255.0;
        }
    }
    Ok(Some(rgb))
}

fn parse_ring(v: &JVal) -> Result<PlanetaryRingSpec, String> {
    let obj = v
        .as_obj()
        .ok_or_else(|| "field `ring` must be an object".to_string())?;
    let inner = opt_num(obj, &["inner_scale", "inner"], "ring.inner_scale")?.unwrap_or(1.35) as f32;
    let outer = opt_num(obj, &["outer_scale", "outer"], "ring.outer_scale")?.unwrap_or(2.0) as f32;
    if inner <= 0.0 {
        return Err("field `ring.inner_scale` must be positive".to_string());
    }
    if outer <= inner {
        return Err("field `ring.outer_scale` must exceed `ring.inner_scale`".to_string());
    }
    let tilt = opt_num(obj, &["tilt_deg", "tilt"], "ring.tilt_deg")?.unwrap_or(18.0) as f32;
    let rgb = opt_rgb(obj, &["rgb", "color", "colour"], "ring.rgb")?.unwrap_or([0.82, 0.78, 0.62]);
    let alpha = opt_num(obj, &["alpha"], "ring.alpha")?.unwrap_or(0.7) as f32;
    Ok(PlanetaryRingSpec {
        inner_scale: inner,
        outer_scale: outer,
        tilt_deg: tilt,
        rgb,
        alpha,
    })
}

fn parse_body(v: &JVal, force_below_horizon: bool) -> Result<PlanetSkyBody, String> {
    let obj = v
        .as_obj()
        .ok_or_else(|| "each sky body must be an object".to_string())?;
    let id = req_str(obj, &["id", "name"], "sky_bodies[].id")?;
    let radius = opt_num(
        obj,
        &["angular_radius_deg", "angular_radius", "radius_deg"],
        "angular_radius_deg",
    )?
    .unwrap_or(8.0) as f32;
    let period = opt_num(
        obj,
        &["orbit_period_ticks", "orbit_period", "period_ticks"],
        "orbit_period_ticks",
    )?;
    let orbit_period_ticks = match period {
        None => DEFAULT_ORBIT_PERIOD_TICKS,
        Some(n) if n < 0.0 => {
            return Err("field `orbit_period_ticks` must not be negative".to_string());
        }
        Some(n) if n > u64::MAX as f64 => {
            return Err("field `orbit_period_ticks` is out of range".to_string());
        }
        Some(n) => n.round() as u64,
    };
    let phase = opt_num(
        obj,
        &["orbit_phase_offset", "phase_offset", "phase"],
        "orbit_phase_offset",
    )?
    .unwrap_or(0.0) as f32;
    let incl = opt_num(
        obj,
        &["orbit_inclination_deg", "inclination_deg", "inclination"],
        "orbit_inclination_deg",
    )?
    .unwrap_or(0.0) as f32;
    let surface = opt_rgb(
        obj,
        &["surface_rgb", "rgb", "color", "colour"],
        "surface_rgb",
    )?
    .unwrap_or([0.62, 0.62, 0.66]);
    let rim = opt_rgb(
        obj,
        &["atmosphere_rim_rgb", "rim_rgb", "atmosphere_rgb"],
        "atmosphere_rim_rgb",
    )?;
    let ring = match get(obj, &["ring", "rings"]) {
        None | Some(JVal::Null) => None,
        Some(r) => Some(parse_ring(r)?),
    };
    let below = opt_bool(
        obj,
        &["is_below_horizon_parent", "below_horizon_parent"],
        "is_below_horizon_parent",
    )?
    .unwrap_or(force_below_horizon);
    Ok(PlanetSkyBody {
        id,
        angular_radius_deg: radius,
        orbit_period_ticks,
        orbit_phase_offset: phase,
        orbit_inclination_deg: incl,
        surface_rgb: surface,
        atmosphere_rim_rgb: rim,
        ring,
        is_below_horizon_parent: below,
    })
}

/// Parse planet/orbit dimension metadata from JSON. Accepts both the
/// `megu` space schema (`dimension_id`, `gravity_scale`,
/// `atmosphere_density`, `sky_tint_rgb`, `parent_body`, `sky_bodies`) and
/// Ad Astra-style keys (`dimension`, `gravity` in m/s², `oxygen`,
/// `temperature`, `solar_power`, `orbit`, `tier` — the descriptive keys are
/// tolerated, `gravity`/`oxygen`/`orbit` are interpreted).
///
/// Fail-closed: malformed JSON, non-finite numbers, negative orbit periods,
/// more than [`MAX_SKY_BODIES`] bodies, a missing `dimension_id`, or a
/// present-but-wrong-typed field all return `Err` rather than a guess.
pub fn load_planet_meta_json(json_str: &str) -> Result<PlanetDimensionMeta, String> {
    let v = parse_json(json_str)?;
    let obj = v
        .as_obj()
        .ok_or_else(|| "planet metadata must be a JSON object".to_string())?;

    let dimension_id = req_str(obj, &["dimension_id", "dimension"], "dimension_id")?;

    // `gravity_scale`/`gravity_multiplier` arrive as multipliers; Ad Astra's
    // `gravity` is m/s² and normalises by SURFACE_GRAVITY_MS2 (Moon 1.62 →
    // 0.165, Mars 3.72 → 0.38, orbit 0 → 0).
    let gravity_scale = if let Some(g) = opt_num(
        obj,
        &["gravity_scale", "gravity_multiplier"],
        "gravity_scale",
    )? {
        g as f32
    } else if let Some(g) = opt_num(obj, &["gravity"], "gravity")? {
        (g as f32) / SURFACE_GRAVITY_MS2
    } else {
        1.0
    };
    if gravity_scale < 0.0 {
        return Err("field `gravity_scale` must not be negative".to_string());
    }

    let has_oxygen = opt_bool(obj, &["has_oxygen", "oxygen"], "has_oxygen")?.unwrap_or(false);
    let is_orbit_decl = opt_bool(obj, &["is_orbit"], "is_orbit")?.unwrap_or(false)
        || matches!(get(obj, &["orbit"]), Some(JVal::Bool(true)));
    let is_orbit = is_orbit_decl
        || gravity_scale <= ORBIT_GRAVITY_SCALE_MAX
        || dimension_id.ends_with("_orbit");

    // Explicit `atmosphere_density` (or `atmosphere` as number/bool) wins;
    // otherwise derive conservatively: breathable oxygen → Earth-like, too
    // little gravity to hold air → vacuum, otherwise thin/Mars-like.
    let atmosphere_density = match opt_num(obj, &["atmosphere_density"], "atmosphere_density")? {
        Some(d) => d as f32,
        None => match get(obj, &["atmosphere"]) {
            Some(JVal::Num(d)) => *d as f32,
            Some(JVal::Bool(b)) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Some(_) => {
                return Err("field `atmosphere` must be a number or boolean".to_string());
            }
            None if has_oxygen => 1.0,
            None if gravity_scale <= 0.2 => 0.0,
            None => THIN_REF_DENSITY,
        },
    };

    let has_sky_light =
        opt_bool(obj, &["has_sky_light", "sky_light"], "has_sky_light")?.unwrap_or(true);
    let sky_tint_rgb = opt_rgb(
        obj,
        &["sky_tint_rgb", "sky_tint", "sky_color"],
        "sky_tint_rgb",
    )?
    .unwrap_or([1.0, 1.0, 1.0]);
    let star_brightness_boost = opt_num(
        obj,
        &["star_brightness_boost", "star_brightness"],
        "star_brightness_boost",
    )?
    .unwrap_or(1.0) as f32;

    let parent_body = match get(obj, &["parent_body", "parent"]) {
        None | Some(JVal::Null) => None,
        Some(b) => Some(parse_body(b, true)?),
    };

    let sky_bodies = match get(obj, &["sky_bodies", "bodies"]) {
        None | Some(JVal::Null) => Vec::new(),
        Some(v) => {
            let arr = v
                .as_arr()
                .ok_or_else(|| "field `sky_bodies` must be an array".to_string())?;
            if arr.len() > MAX_SKY_BODIES {
                return Err(format!(
                    "field `sky_bodies` has {} entries, max is {MAX_SKY_BODIES}",
                    arr.len()
                ));
            }
            arr.iter()
                .map(|b| parse_body(b, false))
                .collect::<Result<_, _>>()?
        }
    };

    Ok(PlanetDimensionMeta {
        dimension_id,
        gravity_scale,
        atmosphere_density,
        has_oxygen,
        has_sky_light,
        is_orbit,
        sky_tint_rgb,
        star_brightness_boost,
        parent_body,
        sky_bodies,
    })
}
