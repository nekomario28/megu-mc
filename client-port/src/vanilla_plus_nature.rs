// SPDX-License-Identifier: LGPL-3.0-or-later

//! Optional **Vanilla+ Nature & Sky** ambience pack for Megu.
//!
//! This module is **opt-in and default OFF**: [`VanillaPlusNatureConfig::default`]
//! has `enabled == false`, so the vanilla look and feel never change unless
//! the user explicitly turns the pack on. When enabled it layers four gentle,
//! low-cost upgrades on top of the classic blocky aesthetic:
//!
//! * **Layered voxel clouds** — a deterministic integer-grid cloud field that
//!   stays 100% square and pixel-consistent like vanilla clouds (never a
//!   blurry raymarched volume). Two to three stacked strata produce a wider
//!   base with a stepped, puffy upper cap, and hidden faces between adjacent
//!   cloud voxels are suppressed so quad counts stay tiny.
//! * **Subtle nature ambience** — a strictly bounded, rate-limited particle
//!   trickle (falling leaves incl. autumn/cherry tints, sakura petal
//!   spirals, dusk fireflies, drifting winter snow motes, rain ripples on
//!   exposed water, waterfall mist) whose cap follows the active
//!   [`VfxBudgetTier`]. This is ambience, not particle spam. An optional
//!   live [`SeasonAmbienceInput`] (fed by an `SSN1` Season Mod sync from
//!   Serene Seasons / Fabric Seasons / Ecliptic Seasons) modulates palette,
//!   cadence and particle families; the default seasonless input leaves
//!   behaviour identical to running no season mod at all.
//! * **Gentle weather-driven wind sway** — a small vertex offset for plants,
//!   crops, leaves and hanging lanterns. Grass and crop bases are pinned at
//!   `local_y == 0.0` so roots never detach; lanterns pivot from their top.
//!   [`MEGU_WIND_SWAY_WGSL_FN`] is a syntactically valid WGSL helper that
//!   terrain shaders can inline for the identical motion.
//! * **Minimal emissive mob eyes** — a lookup table that maps the vanilla
//!   mobs whose eyes or cores naturally glow to a subtle RGB tint and
//!   intensity. It is a glow mask hint only: zero model, rig or animation
//!   changes.
//!
//! Modded content plugs in through [`NatureModRegistry`] plus the
//! `classify_*` / `*_with_atmosphere` helpers at the bottom of this file:
//! modded leaf/canopy blocks, swayable plants and lanterns, emissive mobs,
//! and foreign biomes/dimensions are classified deterministically from
//! resource ids, `c:`/`forge:`/`minecraft:` tags and a caller-supplied
//! `atmosphere_density`. Everything unknown — a solid modded block, a
//! vacuum space dimension, an underground cave — fails closed to `None` or
//! an empty output, never to a guessed effect.
//!
//! Everything is pure `std`, deterministic, and reads no wall clock: all
//! animation derives from `world_time_ticks`, `dt_sec` and integer position
//! hashes. No Mojang or third-party geometry or texture data is referenced
//! anywhere in this file.

use std::collections::{BTreeMap, BTreeSet};
use std::f32::consts::{PI, TAU};

use crate::vfx_engine::VfxHardwareTier;

// ---------------------------------------------------------------------------
// Shared constants (fail-closed bounds)
// ---------------------------------------------------------------------------

/// Maximum absolute world coordinate accepted for cloud sampling. Matches the
/// classic far-lands guard used by [`crate::vfx_engine`].
const MAX_COORD: f32 = 30_000_000.0;

/// Cloud voxel footprint in blocks (vanilla clouds are chunky, not texel-fine).
pub const CLOUD_CELL_BLOCKS: f32 = 12.0;
/// Vertical thickness of one cloud stratum in blocks.
pub const CLOUD_LAYER_HEIGHT: f32 = 4.0;
/// Horizontal half-window of cloud cells generated around the camera.
pub const CLOUD_RADIUS_CELLS: i32 = 6;
/// Wind scroll speed in blocks per tick.
pub const CLOUD_SCROLL_BLOCKS_PER_TICK: f32 = 0.4;
/// Largest possible number of emitted cloud voxels (window x strata).
pub const MAX_CLOUD_CELLS: usize =
    ((2 * CLOUD_RADIUS_CELLS + 1) * (2 * CLOUD_RADIUS_CELLS + 1)) as usize * 3;

/// Exposed-face bitmask: face towards +X is visible.
pub const FACE_POS_X: u8 = 0b00_0001;
/// Exposed-face bitmask: face towards -X is visible.
pub const FACE_NEG_X: u8 = 0b00_0010;
/// Exposed-face bitmask: face towards +Y (top) is visible.
pub const FACE_POS_Y: u8 = 0b00_0100;
/// Exposed-face bitmask: face towards -Y (bottom) is visible.
pub const FACE_NEG_Y: u8 = 0b00_1000;
/// Exposed-face bitmask: face towards +Z is visible.
pub const FACE_POS_Z: u8 = 0b01_0000;
/// Exposed-face bitmask: face towards -Z is visible.
pub const FACE_NEG_Z: u8 = 0b10_0000;
/// All six faces exposed (isolated voxel).
pub const ALL_FACES: u8 = 0b11_1111;

const MIN_CLOUD_HEIGHT: f32 = 48.0;
const MAX_CLOUD_HEIGHT: f32 = 512.0;
const DEFAULT_CLOUD_HEIGHT: f32 = 192.0;

/// Per-site-list bound on spawn hints; extra sites are ignored.
pub const MAX_ENV_SITES: usize = 24;
/// Clamp for `dt_sec` so a hitch cannot explode the simulation.
const MAX_AMBIENCE_DT_SEC: f32 = 0.25;

const LOW_SPEC_AMBIENCE_CAP: usize = 32;
const BALANCED_AMBIENCE_CAP: usize = 96;
const HIGH_AMBIENCE_CAP: usize = 192;

const LEAF_PERIOD_MAX: u64 = 24;
const LEAF_PERIOD_MIN: u64 = 9;
const SAKURA_BASE_PERIOD: u64 = 16;
const EVERGREEN_NEEDLE_PERIOD: u64 = 30;
const SNOW_MOTE_PERIOD: u64 = 6;
const SNOW_MOTES_PER_EVENT: usize = 3;
const FIREFLY_PERIOD: u64 = 15;
const FIREFLIES_PER_EVENT: usize = 3;
const RIPPLE_PERIOD_RAIN: u64 = 5;
const RIPPLE_PERIOD_THUNDER: u64 = 3;
const MIST_PERIOD: u64 = 8;
const MIST_MIN_DROP: f32 = 3.0;

const WIND_SWAY_FREQUENCY: f32 = 1.4;
const WIND_TIME_WRAP_TICKS: u64 = 14_400;

// Deterministic hash seeds: one per feature so fields stay independent.
const SEED_CLOUD_A: u64 = 0x9e37_79b9_7f4a_7c15;
const SEED_CLOUD_B: u64 = 0xc2b2_ae3d_27d4_eb4f;
const SEED_LEAF: u64 = 0x1656_67b1_9e37_79f9;
const SEED_PETAL: u64 = 0x3c79_a492_7bf9_2c61;
const SEED_NEEDLE: u64 = 0x51f2_7d4b_b39f_0e97;
const SEED_SNOW: u64 = 0x6a09_e667_f3bc_c909;
const SEED_FIREFLY: u64 = 0x85eb_ca6b_f27d_4a3d;
const SEED_RIPPLE: u64 = 0x27d4_eb2f_1656_67c5;
const SEED_MIST: u64 = 0x94d0_49bb_1331_11eb;
const SEED_SWAY: u64 = 0x632d_e59f_0a5a_2b71;

// Density thresholds per stratum: base is widest, each cap a smaller subset.
const DENSITY_BASE: f32 = 0.60;
const DENSITY_CAP_1: f32 = 0.72;
const DENSITY_CAP_2: f32 = 0.84;

// Storm coverage ramp: thresholds relax towards full overcast as
// `storm` runs 0.0 (clear) -> 1.0 (thunder).
const DENSITY_STORM_BASE: f32 = 0.30;
const DENSITY_STORM_CAP_1: f32 = 0.52;
const DENSITY_STORM_CAP_2: f32 = 0.70;

/// Altocumulus deck lift above the cumulus base altitude, in blocks. Exactly
/// one stratum thickness so the mid deck hugs the cumulus tops like an
/// altocumulus-castellanus turret — the classic stepped puffy cap.
pub const ALTOCUMULUS_DECK_LIFT: f32 = CLOUD_LAYER_HEIGHT;
/// Cirrus deck lift above the cumulus base altitude, in blocks. Decks stack
/// contiguously (base 0, altocumulus at the cumulus top, cirrus at the
/// altocumulus top) so every column reads as one stepped cloud column and
/// same-column shared faces are genuinely adjacent — which is also what the
/// hidden-face and stacked-cap invariants demand.
pub const CIRRUS_DECK_LIFT: f32 = ALTOCUMULUS_DECK_LIFT + DECK_THICKNESS[1];
/// Per-deck vertical thickness in blocks `[cumulus, altocumulus, cirrus]` —
/// the wispy cirrus is thinnest.
pub const DECK_THICKNESS: [f32; 3] = [CLOUD_LAYER_HEIGHT, 3.0, 2.0];
/// Per-deck parallax wind-drift multiplier vs [`CLOUD_SCROLL_BLOCKS_PER_TICK`]:
/// higher decks shear faster across the sky.
pub const DECK_WIND_SPEED: [f32; 3] = [1.0, 1.5, 2.25];
/// Per-deck translucency alpha `[cumulus, altocumulus, cirrus]`: the low
/// cumulus is nearly opaque while the high cirrus lets the sky through.
pub const DECK_ALPHA: [f32; 3] = [0.82, 0.58, 0.35];
/// How much sun each deck blocks at ground level for [`cloud_shadow_factor`].
const DECK_GROUND_SHADE: [f32; 3] = [0.45, 0.30, 0.15];

// Deterministic hash seeds for the upper decks and the cue-driven particles.
const SEED_CLOUD_ALTO: u64 = 0x4f1b_bcdc_9fb5_0793;
const SEED_CLOUD_CIRRUS: u64 = 0x7fb5_8165_1cf7_3b21;
const SEED_DUST: u64 = 0xd6e8_feb8_6659_fd93;
const SEED_EMBER: u64 = 0xa24b_aed4_963e_e407;

const DUST_MOTE_PERIOD: u64 = 8;
const EMBER_PERIOD: u64 = 6;
/// Sky light at or below which sealed-underground dust motes may drift.
const DUST_MAX_SKY_LIGHT: u8 = 11;

// ---------------------------------------------------------------------------
// Small numeric helpers (mirroring vfx_engine conventions)
// ---------------------------------------------------------------------------

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

fn clamped(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        lo
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamped((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Deterministic integer-coordinate hash -> `[0, 1]`. No RNG, no clock,
/// stable across rebuilds and platforms.
fn hash01(x: i64, z: i64, seed: u64) -> f32 {
    let mut h = (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (z as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ seed.wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 32;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    ((h >> 40) & 0x00ff_ffff) as f32 / 16_777_215.0
}

/// Smooth value noise over the integer grid: hash corners + smoothstepped
/// bilinear interpolation. Gives cohesive blobs, not per-cell static.
fn value_noise_2d(x: f64, z: f64, seed: u64) -> f32 {
    let xi = x.floor() as i64;
    let zi = z.floor() as i64;
    let xf = (x - xi as f64) as f32;
    let zf = (z - zi as f64) as f32;
    let u = xf * xf * (3.0 - 2.0 * xf);
    let v = zf * zf * (3.0 - 2.0 * zf);
    let a = hash01(xi, zi, seed);
    let b = hash01(xi + 1, zi, seed);
    let c = hash01(xi, zi + 1, seed);
    let d = hash01(xi + 1, zi + 1, seed);
    lerp(lerp(a, b, u), lerp(c, d, u), v)
}

/// Two-octave cloud coverage field in `[0, 1]` keyed purely by integer cell.
fn cloud_density(cx: i64, cz: i64) -> f32 {
    let n1 = value_noise_2d(cx as f64 / 9.0, cz as f64 / 9.0, SEED_CLOUD_A);
    let n2 = value_noise_2d(cx as f64 / 3.0, cz as f64 / 3.0, SEED_CLOUD_B);
    0.7 * n1 + 0.3 * n2
}

/// Which atmospheric deck a `layer` slot plays for a given deck count:
/// `1` deck is classic low cumulus, `2` decks add high cirrus, `3` decks
/// insert mid altocumulus between the two. Roles: `0` cumulus, `1`
/// altocumulus, `2` cirrus.
fn deck_role(layer: u8, layers: u8) -> usize {
    match layers.clamp(1, 3) {
        1 => 0,
        2 => {
            if layer == 0 {
                0
            } else {
                2
            }
        }
        _ => usize::from(layer.min(2)),
    }
}

/// Per-deck coverage field in `[0, 1]`: each deck has its own texture so
/// silhouettes differ between strata — cumulus blobs, patchy altocumulus
/// mottling, and long cirrus streaks stretched along the wind axis.
fn cloud_density_deck(role: usize, cx: i64, cz: i64) -> f32 {
    match role {
        0 => cloud_density(cx, cz),
        1 => {
            let n1 = value_noise_2d(cx as f64 / 6.0, cz as f64 / 6.0, SEED_CLOUD_ALTO);
            let n2 = value_noise_2d(cx as f64 / 2.5, cz as f64 / 2.5, SEED_CLOUD_ALTO ^ 0x5a5a);
            0.65 * n1 + 0.35 * n2
        }
        _ => {
            // Cirrus streaks stretch along X, the direction the layer advects.
            let n1 = value_noise_2d(cx as f64 / 14.0, cz as f64 / 5.0, SEED_CLOUD_CIRRUS);
            let n2 = value_noise_2d(cx as f64 / 4.0, cz as f64 / 2.0, SEED_CLOUD_CIRRUS ^ 0x5a5a);
            0.70 * n1 + 0.30 * n2
        }
    }
}

/// Whole-cell parallax shift for a deck at `scroll` scrolled blocks: higher
/// decks' coverage pattern slides faster than the shared advection (wind
/// shear), always quantized to whole cells so the field stays chunky.
fn deck_cell_shift(role: usize, scroll: f64) -> i64 {
    let extra = scroll * f64::from(DECK_WIND_SPEED[role] - 1.0);
    (extra / f64::from(CLOUD_CELL_BLOCKS)).floor() as i64
}

/// Fully-resolved description of one atmospheric cloud deck.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudDeckSpec {
    /// Altitude offset above the base cloud height, in blocks.
    pub altitude_offset: f32,
    /// Vertical deck thickness in blocks.
    pub thickness: f32,
    /// Parallax wind-drift multiplier vs [`CLOUD_SCROLL_BLOCKS_PER_TICK`].
    pub wind_speed: f32,
    /// Translucency alpha in `0..=1` (`1.0` opaque; cirrus is wispy).
    pub opacity: f32,
    /// Coverage density threshold in clear weather.
    pub clear_threshold: f32,
    /// Coverage density threshold under a full storm (lower = more deck).
    pub storm_threshold: f32,
}

impl CloudDeckSpec {
    /// Density threshold interpolated clear -> storm for `storm` in `0..=1`.
    pub fn threshold(&self, storm: f32) -> f32 {
        lerp(
            self.clear_threshold,
            self.storm_threshold,
            clamped(storm, 0.0, 1.0),
        )
    }
}

/// Spec table indexed by deck ROLE (`0` cumulus, `1` altocumulus, `2`
/// cirrus) — see [`deck_role`] for the slot -> role map.
const DECK_SPECS: [CloudDeckSpec; 3] = [
    CloudDeckSpec {
        altitude_offset: 0.0,
        thickness: DECK_THICKNESS[0],
        wind_speed: DECK_WIND_SPEED[0],
        opacity: DECK_ALPHA[0],
        clear_threshold: DENSITY_BASE,
        storm_threshold: DENSITY_STORM_BASE,
    },
    CloudDeckSpec {
        altitude_offset: ALTOCUMULUS_DECK_LIFT,
        thickness: DECK_THICKNESS[1],
        wind_speed: DECK_WIND_SPEED[1],
        opacity: DECK_ALPHA[1],
        clear_threshold: DENSITY_CAP_1,
        storm_threshold: DENSITY_STORM_CAP_1,
    },
    CloudDeckSpec {
        altitude_offset: CIRRUS_DECK_LIFT,
        thickness: DECK_THICKNESS[2],
        wind_speed: DECK_WIND_SPEED[2],
        opacity: DECK_ALPHA[2],
        clear_threshold: DENSITY_CAP_2,
        storm_threshold: DENSITY_STORM_CAP_2,
    },
];

/// The [`CloudDeckSpec`] for `layer` under a config of `layers` decks
/// (`effective_cloud_layers`). See [`deck_role`] for the slot -> deck map.
pub fn cloud_deck_spec(layer: u8, layers: u8) -> CloudDeckSpec {
    DECK_SPECS[deck_role(layer, layers)]
}

/// Pattern gain per deck role `[cumulus, altocumulus, cirrus]`: how strongly
/// the deck's own texture field opens extra coverage on top of the shared
/// cumulus field. The shared field keeps every deck's footprint nested over
/// dense base columns (a cap can never exist without a base); the pattern
/// term sculpts the boundary — altocumulus caps gather in mottled patches,
/// cirrus streaks stretch along the wind axis — and it is sampled under the
/// deck's own parallax shift so coverage edges drift at deck speed.
const DECK_PATTERN_GAIN: [f32; 3] = [0.0, 0.05, 0.15];

/// `true` when a deck's coverage field exceeds its weather-interpolated
/// threshold at cell `(cx, cz)`. This is THE cloud density test —
/// `generate_layered_voxel_clouds` and `cloud_shadow_factor` share it
/// verbatim so ground shadows match the sky 1:1.
///
/// Coverage is the shared cumulus field plus a bounded pattern gain: the
/// deck texture can only ever *add* coverage over dense base columns, never
/// float a cap over empty sky (nesting is also enforced structurally by
/// [`column_occupied`]). The pattern samples at the deck's parallax-shifted
/// coordinate so cap boundaries advance faster on higher decks.
fn deck_occupied(role: usize, cx: i64, cz: i64, scroll: f64, storm: f32) -> bool {
    let field = if DECK_PATTERN_GAIN[role] == 0.0 {
        cloud_density(cx, cz)
    } else {
        let shift = deck_cell_shift(role, scroll);
        cloud_density(cx, cz)
            + DECK_PATTERN_GAIN[role]
                * cloud_density_deck(role, cx.saturating_add(shift), cz)
    };
    field > DECK_SPECS[role].threshold(storm)
}

/// Nested column occupancy: a deck exists only where every deck beneath it
/// also exists in the same column (wider base, stepped upper decks).
fn column_occupied(layer: u8, layers: u8, cx: i64, cz: i64, scroll: f64, storm: f32) -> bool {
    (0..=layer).all(|l| deck_occupied(deck_role(l, layers), cx, cz, scroll, storm))
}

/// Storm coverage scalar in `0..=1` for a [`WeatherKind`]: clear `0.0`,
/// rain `0.55`, thunder `1.0`.
fn weather_storm(weather: WeatherKind) -> f32 {
    match weather {
        WeatherKind::Clear => 0.0,
        WeatherKind::Rain => 0.55,
        WeatherKind::Thunder => 1.0,
    }
}

/// The shared horizontal wind field in blocks per tick: the X component is
/// the cloud-advection headwind — the same scroll the voxel clouds and the
/// [`cloud_shadow_factor`] field use — and Z is a slow gusty crosswind that
/// strengthens with [`WeatherKind::wind_strength`]. `Clear` returns exactly
/// `(-CLOUD_SCROLL_BLOCKS_PER_TICK, 0.0)`. Grass, leaf, lantern and banner
/// sway plus ambience particle drift all pull from this one vector so the
/// whole scene leans the same way under the same sky.
pub fn shared_wind_vector(world_time_ticks: u64, weather: WeatherKind) -> [f32; 2] {
    // Normalize weather strength so Clear contributes no boost or gust.
    let w = ((weather.wind_strength() - 0.15) / (WeatherKind::Thunder.wind_strength() - 0.15))
        .clamp(0.0, 1.0);
    let phase = (world_time_ticks % WIND_TIME_WRAP_TICKS) as f32 * 0.05;
    [
        -CLOUD_SCROLL_BLOCKS_PER_TICK * (1.0 + 0.55 * w),
        0.25 * w * (phase * 0.37 + 1.3).sin(),
    ]
}

/// Fraction of open-sky sun reaching the ground at `(world_x, world_z)` —
/// `1.0` full sun, lower under a cloud deck.
///
/// Uses the exact same integer-cell occupancy pipeline as
/// [`generate_layered_voxel_clouds`]: the advection scroll, the per-deck
/// parallax cell shift, the per-deck density fields and the nested
/// column rule are shared verbatim, so drifting ground shadows match the
/// overhead multi-layer clouds 1:1. Rain and thunder deepen the shade the
/// same way the storm coverage ramp thickens the decks.
pub fn cloud_shadow_factor(
    world_x: f32,
    world_z: f32,
    world_time_ticks: u64,
    weather: WeatherKind,
    layers: u8,
) -> f32 {
    let x = f64::from(finite_or(world_x, 0.0).clamp(-MAX_COORD, MAX_COORD));
    let z = f64::from(finite_or(world_z, 0.0).clamp(-MAX_COORD, MAX_COORD));
    let scroll = world_time_ticks as f64 * f64::from(CLOUD_SCROLL_BLOCKS_PER_TICK);
    let cell = f64::from(CLOUD_CELL_BLOCKS);
    let cx = ((x + scroll) / cell).floor() as i64;
    let cz = (z / cell).floor() as i64;
    let storm = weather_storm(weather);

    let decks = layers.clamp(1, 3);
    let mut light = 1.0f32;
    for layer in 0..decks {
        if !column_occupied(layer, decks, cx, cz, scroll, storm) {
            break; // nested decks: nothing above a missing lower deck
        }
        let role = deck_role(layer, decks);
        light *= 1.0 - DECK_GROUND_SHADE[role];
    }
    // Storm decks are deep slate: precipitation darkens what shade exists.
    (light * (1.0 - 0.25 * storm)).clamp(0.0, 1.0)
}

/// Per-block sway phase offset so neighbouring plants never move in lockstep.
fn block_phase(block: [i32; 3]) -> f32 {
    hash01(
        (block[0] as i64).wrapping_mul(31) + block[1] as i64,
        (block[2] as i64).wrapping_mul(17) + block[1] as i64,
        SEED_SWAY,
    ) * TAU
}

/// Strips a `namespace:` prefix and any trailing state suffix from an id.
fn resource_path(id: &str) -> &str {
    let path = id.rsplit(':').next().unwrap_or(id);
    path.trim()
}

// ---------------------------------------------------------------------------
// Master opt-in configuration
// ---------------------------------------------------------------------------

/// Per-user configuration for the Vanilla+ Nature & Sky pack. `Default` is
/// fully vanilla: `enabled == false`.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaPlusNatureConfig {
    /// Master opt-in switch. **`false` by default** — the pack never changes
    /// the vanilla look unless explicitly enabled.
    pub enabled: bool,
    /// Draw layered voxel clouds on top of (or instead of) the flat vanilla
    /// cloud plane.
    pub layered_voxel_clouds: bool,
    /// Number of vertical cloud strata, clamped to `1..=3` when read via
    /// [`Self::effective_cloud_layers`].
    pub cloud_layers: u8,
    /// Cloud base altitude in blocks. Default `192.0` (vanilla height),
    /// clamped to `48.0..=512.0` via [`Self::effective_cloud_height`].
    pub cloud_height: f32,
    /// Falling leaves, dusk fireflies, rain ripples and waterfall mist.
    pub nature_ambience: bool,
    /// Weather-linked sway for grass, crops, leaves and hanging lanterns.
    pub gentle_wind_sway: bool,
    /// Subtle glow mask on vanilla mobs with naturally glowing features.
    /// Tint and intensity only — zero geometry or animation changes.
    pub emissive_mob_eyes: bool,
}

impl Default for VanillaPlusNatureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            layered_voxel_clouds: true,
            cloud_layers: 2,
            cloud_height: DEFAULT_CLOUD_HEIGHT,
            nature_ambience: true,
            gentle_wind_sway: true,
            emissive_mob_eyes: true,
        }
    }
}

impl VanillaPlusNatureConfig {
    /// `cloud_layers` clamped to `1..=3`.
    pub fn effective_cloud_layers(&self) -> u8 {
        self.cloud_layers.clamp(1, 3)
    }

    /// `cloud_height` clamped to `48.0..=512.0` (non-finite -> 192).
    pub fn effective_cloud_height(&self) -> f32 {
        finite_or(self.cloud_height, DEFAULT_CLOUD_HEIGHT).clamp(MIN_CLOUD_HEIGHT, MAX_CLOUD_HEIGHT)
    }
}

// ---------------------------------------------------------------------------
// Budget tier & weather primitives
// ---------------------------------------------------------------------------

/// Ambience budget tier, mirroring [`VfxHardwareTier`] semantics at a far
/// smaller (deliberately subtle) scale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VfxBudgetTier {
    /// Potato-grade: hard cap 32 live ambience particles, 2 spawns/tick.
    LowSpec,
    /// Default: hard cap 96 live ambience particles, 6 spawns/tick.
    #[default]
    Balanced,
    /// Strong GPU: hard cap 192 live ambience particles, 12 spawns/tick.
    High,
}

impl VfxBudgetTier {
    /// Hard cap on simultaneously live ambience particles.
    pub const fn max_particles(self) -> usize {
        match self {
            Self::LowSpec => LOW_SPEC_AMBIENCE_CAP,
            Self::Balanced => BALANCED_AMBIENCE_CAP,
            Self::High => HIGH_AMBIENCE_CAP,
        }
    }

    /// Rate limit on new ambience particles per tick.
    pub const fn max_spawns_per_tick(self) -> usize {
        match self {
            Self::LowSpec => 2,
            Self::Balanced => 6,
            Self::High => 12,
        }
    }
}

impl From<VfxHardwareTier> for VfxBudgetTier {
    fn from(tier: VfxHardwareTier) -> Self {
        match tier {
            VfxHardwareTier::LowSpec => Self::LowSpec,
            VfxHardwareTier::Balanced => Self::Balanced,
            VfxHardwareTier::High => Self::High,
        }
    }
}

/// Coarse precipitation state driving ambience and wind strength.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WeatherKind {
    /// No precipitation: gentlest breeze.
    #[default]
    Clear,
    /// Rainfall: moderate wind, ripples on exposed water.
    Rain,
    /// Thunderstorm: strongest wind, faster ripples.
    Thunder,
}

impl WeatherKind {
    /// `true` when rain or a thunderstorm is active.
    pub const fn is_precipitating(self) -> bool {
        !matches!(self, Self::Clear)
    }

    /// Wind amplitude scale: clear breeze 0.15, rain 0.45, thunder 0.85.
    pub const fn wind_strength(self) -> f32 {
        match self {
            Self::Clear => 0.15,
            Self::Rain => 0.45,
            Self::Thunder => 0.85,
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-rain weather forecast & broken-cloud sky conditions
// ---------------------------------------------------------------------------

/// Rain-front forecast horizon: on the Megu server `rain_time` counts the
/// remaining weather ticks down from at most 6000 — five real-time
/// minutes of heads-up before rain starts or clears.
pub const RAIN_FORECAST_HORIZON_TICKS: u32 = 6_000;

/// Live weather inputs the client knows right now. Pure data: the caller
/// fills it each frame from the vanilla rain ramp plus (on the Megu
/// server) the `rain_time` countdown. External vanilla servers never
/// share their weather clock, so they leave `ticks_until_rain_change` at
/// `None` and [`evaluate_weather_cloud_conditions`] falls back to the
/// early rain-strength ramp.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WeatherForecastState {
    /// Live vanilla rain ramp in `0.0..=1.0` (non-finite reads as `0.0`).
    pub rain_strength: f32,
    /// Ticks until the next rain flip when the server reports `rain_time`
    /// (`0..=`[`RAIN_FORECAST_HORIZON_TICKS`]; larger values clamp to the
    /// horizon). `None` on external vanilla servers.
    pub ticks_until_rain_change: Option<u32>,
    /// `true` while precipitation is actually falling.
    pub currently_raining: bool,
    /// Biome humidity curve: `0.0` arid desert, `0.5` plains/forest,
    /// `1.0` jungle/swamp (non-finite reads as `0.5`).
    pub biome_humidity: f32,
}

/// Resolved cloud/sky mood for one frame: how much broken cloud coverage
/// the sky shows, how dark storm-cloud bellies read, how bright the
/// sunlit silver linings glow, and how much the pre-front breeze picks
/// up. Every field is finite and bounded for any input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudSkyConditions {
    /// Broken-cloud coverage `0.12..=0.95`: sparse wisps in a dry desert,
    /// patchy cumulus in clear plains, gathering thick decks ahead of
    /// rain, near-overcast during rain.
    pub broken_cloud_coverage: f32,
    /// Beer-Lambert underside darkening `0.0..=1.0`: optically thick
    /// storm bellies read as dramatic cool slate-grey.
    pub underside_darkness: f32,
    /// Silver-lining rim strength `0.0..=1.0`: bright sunlit edges where
    /// the sun still peeks between broken cloud gaps.
    pub silver_lining_strength: f32,
    /// Pre-front wind multiplier `1.0..=1.8`: the breeze picks up
    /// naturally as a rain front approaches.
    pub pre_rain_wind_boost: f32,
}

impl Default for CloudSkyConditions {
    /// Fair-weather baseline: sparse coverage, no darkening or lining,
    /// unboosted breeze.
    fn default() -> Self {
        Self {
            broken_cloud_coverage: 0.12,
            underside_darkness: 0.0,
            silver_lining_strength: 0.0,
            pre_rain_wind_boost: 1.0,
        }
    }
}

/// Evaluates [`WeatherForecastState`] into [`CloudSkyConditions`].
///
/// Two data paths converge on one `gather` ramp in `0..=1` (how strongly
/// a rain front is on top of us):
///
/// * **Megu server** — `ticks_until_rain_change = Some(t)` gives a real
///   forecast: with `currently_raining == false` the decks gather over
///   the countdown, most of the buildup landing in the last ~2 minutes;
///   with `currently_raining == true` the same countdown instead becomes
///   a `clearing` ramp that parts the deck and kindles silver linings
///   before the rain actually ends.
/// * **External vanilla server** — `None`: the early `rain_strength`
///   ramp `0.01..0.35` already gathers and darkens clouds visibly before
///   heavy rain hits, and `biome_humidity` keeps deserts clearer than
///   jungles under the same sky.
///
/// Pure, finite and deterministic — no wall clock, no mod hardcoding.
pub fn evaluate_weather_cloud_conditions(state: &WeatherForecastState) -> CloudSkyConditions {
    let rain = clamped(state.rain_strength, 0.0, 1.0);
    let humidity = finite_or(state.biome_humidity, 0.5).clamp(0.0, 1.0);

    // The vanilla early-ramp fallback works on every server.
    let mut gather = smoothstep(0.01, 0.35, rain);
    let mut clearing = 0.0f32;
    let mut approaching = 0.0f32;
    if state.currently_raining {
        // Falling rain keeps a strong overcast floor even at low ramp.
        gather = gather.max(0.60 + 0.40 * rain);
    }
    if let Some(t) = state.ticks_until_rain_change {
        // Front proximity: 0 at the 5-minute horizon -> 1 imminent.
        let near = 1.0 - t.min(RAIN_FORECAST_HORIZON_TICKS) as f32
            / RAIN_FORECAST_HORIZON_TICKS as f32;
        if state.currently_raining {
            // Rain is about to clear: the deck parts in the last stretch.
            clearing = smoothstep(0.50, 0.95, near);
        } else {
            // Rain is coming: decks gather smoothly over the approach.
            approaching = smoothstep(0.0, 1.0, near);
            gather = gather.max(approaching);
        }
    }

    // A departing front opens sunlit gaps again before it fully leaves.
    let overcast = (gather * (1.0 - 0.45 * clearing)).clamp(0.0, 1.0);

    // Humidity lifts both the clear-sky floor and how thick a gathering
    // deck can get; Beer-Lambert belly darkening deepens with it too.
    let coverage =
        (0.12 + 0.16 * humidity + overcast * (0.52 + 0.24 * humidity)).clamp(0.12, 0.95);
    let underside = (overcast * (0.55 + 0.45 * humidity)).clamp(0.0, 1.0);
    // Sunbeams need broken sky: parting fronts blaze, mid-gather gaps
    // glint; a closed overcast shows almost no lining.
    let silver = (0.70 * clearing + 0.35 * gather * (1.0 - overcast) * (0.4 + 0.6 * humidity))
        .clamp(0.0, 1.0);
    let wind = 1.0 + 0.8 * gather.max(approaching);

    CloudSkyConditions {
        broken_cloud_coverage: coverage,
        underside_darkness: underside,
        silver_lining_strength: silver,
        pre_rain_wind_boost: wind.clamp(1.0, 1.8),
    }
}

// ---------------------------------------------------------------------------
// Seasonal ambience input (live Season Mod / SSN1 coupling)
// ---------------------------------------------------------------------------

/// The four broad seasons the ambience engine modulates by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveSeasonKind {
    /// 春 — sakura bloom, fresh green canopies.
    Spring,
    /// 夏 — lush canopy, dusk fireflies. Neutral default.
    #[default]
    Summer,
    /// 秋 — 紅葉 crimson/amber foliage and boosted leaf cadence.
    Autumn,
    /// 冬 — dormant deciduous canopy, drifting 粉雪 motes.
    Winter,
}

impl ActiveSeasonKind {
    /// Broad season for a Serene-Seasons-style sub-season index (`0..=11`,
    /// three sub-seasons per season). Out-of-range fails closed to the
    /// neutral [`Self::Summer`].
    pub const fn from_sub_season_index(idx: u8) -> Self {
        match idx {
            0..=2 => Self::Spring,
            3..=5 => Self::Summer,
            6..=8 => Self::Autumn,
            9..=11 => Self::Winter,
            _ => Self::Summer,
        }
    }
}

/// Seasonal ambience state pushed by a live Season Mod sync — the `SSN1`
/// frame on `megu:season_v1` produced by the Serene Seasons / Fabric
/// Seasons / Ecliptic Seasons adapters.
///
/// `Default` is the neutral, seasonless vanilla fallback: every derived
/// modulation ([`Self::autumn_ramp`], [`Self::snow_amount`], ...) reads as
/// "no season mod installed", so callers get exactly the pre-season
/// behaviour. All modulation methods run [`Self::sanitized`] first, so a
/// struct-literal input with out-of-range fields still fails closed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeasonAmbienceInput {
    /// Broad season in effect. Ignored while `is_seasonless`.
    pub season: ActiveSeasonKind,
    /// Sub-season calendar index `0..=11` (EarlySpring .. LateWinter);
    /// values `>= 12` sanitize to the seasonless fallback.
    pub sub_season_index: u8,
    /// Progress through the current sub-season in `0.0..=1.0`.
    pub sub_season_progress: f32,
    /// Wet/dry-season climate: stays lush and green, never snows and never
    /// enters deciduous dormancy.
    pub is_tropical: bool,
    /// No season calendar on this dimension: every modulation is neutral.
    pub is_seasonless: bool,
    /// Seasonal foliage tint multiplier in `0.0..=2.0` (`1.0` = neutral).
    pub foliage_tint: [f32; 3],
    /// Seasonal grass tint multiplier in `0.0..=2.0` (`1.0` = neutral).
    pub grass_tint: [f32; 3],
    /// Server-driven seasonal snow/freeze override on this dimension.
    pub snow_override: bool,
}

impl Default for SeasonAmbienceInput {
    /// Neutral seasonless input: identical to running no Season Mod.
    fn default() -> Self {
        Self::seasonless()
    }
}

impl SeasonAmbienceInput {
    /// The neutral fallback: every derived modulation reports its
    /// no-season-mod value.
    pub const fn seasonless() -> Self {
        Self {
            season: ActiveSeasonKind::Summer,
            sub_season_index: 0,
            sub_season_progress: 0.0,
            is_tropical: false,
            is_seasonless: true,
            foliage_tint: [1.0; 3],
            grass_tint: [1.0; 3],
            snow_override: false,
        }
    }

    /// Builds the ambience input from one decoded `SSN1` season frame:
    /// sub-season index `0..=11`, sub-season progress, climate flags and
    /// quantized `255 = 1.0` tint multipliers. An out-of-range index fails
    /// closed to the seasonless fallback.
    pub fn from_wire_sync(
        sub_season_idx: u8,
        progress_in_sub_season: f32,
        tropical_dim: bool,
        snow_enabled: bool,
        foliage_rgb: [u8; 3],
        grass_rgb: [u8; 3],
    ) -> Self {
        if sub_season_idx >= 12 {
            return Self::seasonless();
        }
        Self {
            season: ActiveSeasonKind::from_sub_season_index(sub_season_idx),
            sub_season_index: sub_season_idx,
            sub_season_progress: progress_in_sub_season,
            is_tropical: tropical_dim,
            is_seasonless: false,
            foliage_tint: [
                f32::from(foliage_rgb[0]) / 255.0,
                f32::from(foliage_rgb[1]) / 255.0,
                f32::from(foliage_rgb[2]) / 255.0,
            ],
            grass_tint: [
                f32::from(grass_rgb[0]) / 255.0,
                f32::from(grass_rgb[1]) / 255.0,
                f32::from(grass_rgb[2]) / 255.0,
            ],
            snow_override: snow_enabled,
        }
        .sanitized()
    }

    /// A copy with every field forced into its documented range.
    pub fn sanitized(&self) -> Self {
        if self.sub_season_index >= 12 {
            return Self::seasonless();
        }
        let tint = |t: [f32; 3]| {
            [
                finite_or(t[0], 1.0).clamp(0.0, 2.0),
                finite_or(t[1], 1.0).clamp(0.0, 2.0),
                finite_or(t[2], 1.0).clamp(0.0, 2.0),
            ]
        };
        Self {
            season: self.season,
            sub_season_index: self.sub_season_index,
            sub_season_progress: clamped(self.sub_season_progress, 0.0, 1.0),
            is_tropical: self.is_tropical,
            is_seasonless: self.is_seasonless,
            foliage_tint: tint(self.foliage_tint),
            grass_tint: tint(self.grass_tint),
            snow_override: self.snow_override,
        }
    }

    /// Progress through the broad season in `0.0..=1.0` (the three
    /// sub-seasons stitched together). Returns `0.5` when seasonless.
    pub fn season_progress(&self) -> f32 {
        let s = self.sanitized();
        if s.is_seasonless {
            return 0.5;
        }
        ((f32::from(s.sub_season_index % 3) + s.sub_season_progress) / 3.0).clamp(0.0, 1.0)
    }

    /// Autumn colour ramp in `0.0..=1.0`: `0` outside autumn (tropical and
    /// seasonless climates never ramp), rising smoothly through the season
    /// and easing back out across early winter so the canopy never pops.
    pub fn autumn_ramp(&self) -> f32 {
        let s = self.sanitized();
        if s.is_seasonless || s.is_tropical {
            return 0.0;
        }
        let sp = s.season_progress();
        match s.season {
            ActiveSeasonKind::Spring => 0.0,
            // A whisper of pre-colour in late summer.
            ActiveSeasonKind::Summer => 0.12 * smoothstep(0.55, 1.0, sp),
            ActiveSeasonKind::Autumn => 0.25 + 0.75 * smoothstep(0.0, 0.85, sp),
            // Last russet leaves fade out across early winter.
            ActiveSeasonKind::Winter => 0.85 * (1.0 - smoothstep(0.0, 0.45, sp)),
        }
    }

    /// Deciduous leaf-shed activity in `0.0..=1.0`: `1` means full cadence.
    /// Winter sends temperate canopies dormant (bare), while tropical and
    /// seasonless climates stay active year-round. Evergreen canopy sites
    /// ignore this scale entirely.
    pub fn deciduous_activity(&self) -> f32 {
        let s = self.sanitized();
        if s.is_seasonless || s.is_tropical {
            return 1.0;
        }
        let sp = s.season_progress();
        match s.season {
            ActiveSeasonKind::Spring => 1.0,
            ActiveSeasonKind::Summer => 0.9,
            // Shedding eases off as the canopy thins into late autumn.
            ActiveSeasonKind::Autumn => 1.0 - 0.55 * smoothstep(0.55, 1.0, sp),
            // Bare by mid-winter.
            ActiveSeasonKind::Winter => 0.45 * (1.0 - smoothstep(0.05, 0.55, sp)),
        }
    }

    /// Firefly activity scale in `0.0..=1.0`: full through summer, tapering
    /// off in late autumn, silent in winter. Tropical and seasonless
    /// climates keep the dusk glow year-round.
    pub fn firefly_scale(&self) -> f32 {
        let s = self.sanitized();
        if s.is_seasonless || s.is_tropical {
            return 1.0;
        }
        let sp = s.season_progress();
        match s.season {
            ActiveSeasonKind::Spring => 0.65,
            ActiveSeasonKind::Summer => 1.0,
            ActiveSeasonKind::Autumn => 1.0 - 0.92 * smoothstep(0.45, 0.95, sp),
            ActiveSeasonKind::Winter => 0.0,
        }
    }

    /// Sakura bloom strength in `0.0..=1.0`: `0` outside spring, peaking at
    /// mid-spring so petal cadence roughly doubles during the 桜吹雪 bloom.
    pub fn sakura_bloom(&self) -> f32 {
        let s = self.sanitized();
        if s.is_seasonless || s.is_tropical {
            return 0.0;
        }
        match s.season {
            ActiveSeasonKind::Spring => {
                let sp = s.season_progress();
                let bell = smoothstep(0.10, 0.45, sp) * (1.0 - smoothstep(0.60, 0.95, sp));
                0.35 + 0.65 * bell
            }
            _ => 0.0,
        }
    }

    /// Drifting snow mote amount in `0.0..=1.0`: full under `snow_override`,
    /// moderate and building through winter, and always `0` in tropical or
    /// seasonless climates — never snow in a jungle or a space dimension.
    pub fn snow_amount(&self) -> f32 {
        let s = self.sanitized();
        if s.is_seasonless || s.is_tropical {
            return 0.0;
        }
        let mut amount: f32 = if s.snow_override { 1.0 } else { 0.0 };
        if matches!(s.season, ActiveSeasonKind::Winter) {
            amount = amount.max(0.45 + 0.55 * s.season_progress());
        }
        amount
    }

    /// Horizontal drift multiplier for falling leaves: autumn wind spreads
    /// the flutter up to `1.6x` wider, everything else stays at `1.0`.
    pub fn leaf_drift_scale(&self) -> f32 {
        1.0 + 0.6 * self.autumn_ramp()
    }
}

/// Sakura petal spawn period in ticks: baseline 16, tightened towards ~6
/// while [`SeasonAmbienceInput::sakura_bloom`] peaks.
fn sakura_period(bloom: f32) -> u64 {
    let p = SAKURA_BASE_PERIOD as f32 / (1.0 + 1.6 * clamped(bloom, 0.0, 1.0));
    (p.round() as u64).clamp(4, SAKURA_BASE_PERIOD)
}

/// Warm sakura-pink petal tint with soft per-particle variation, multiplied
/// by the seasonal foliage tint (`[1,1,1]` = unchanged).
fn sakura_petal_rgb(h1: f32, h2: f32, tint: [f32; 3]) -> [f32; 3] {
    [
        ((0.97 + 0.03 * h1) * tint[0]).clamp(0.0, 1.0),
        ((0.62 + 0.16 * h2) * tint[1]).clamp(0.0, 1.0),
        ((0.76 + 0.12 * h1) * tint[2]).clamp(0.0, 1.0),
    ]
}

/// Seasonal leaf colour for a temperate canopy. `ramp` in `0..=1` walks the
/// tint from spring green through gold and vermilion to late-autumn
/// crimson; `h` jitters the blend per particle; `tint` is the seasonal
/// foliage multiplier (`[1,1,1]` = unchanged).
fn autumn_leaf_rgb(ramp: f32, h: f32, tint: [f32; 3]) -> [f32; 3] {
    let t = clamped(ramp, 0.0, 1.0) * (0.55 + 0.45 * h);
    const GOLD: [f32; 3] = [0.88, 0.64, 0.14];
    const VERMILION: [f32; 3] = [0.86, 0.38, 0.10];
    const CRIMSON: [f32; 3] = [0.70, 0.15, 0.09];
    let target = if t < 0.5 {
        lerp3(GOLD, VERMILION, t * 2.0)
    } else {
        lerp3(VERMILION, CRIMSON, (t - 0.5) * 2.0)
    };
    let rgb = lerp3([0.30, 0.52, 0.20], target, t);
    [
        (rgb[0] * tint[0]).clamp(0.0, 1.0),
        (rgb[1] * tint[1]).clamp(0.0, 1.0),
        (rgb[2] * tint[2]).clamp(0.0, 1.0),
    ]
}

// ---------------------------------------------------------------------------
// Layered voxel clouds
// ---------------------------------------------------------------------------

/// One visible cloud voxel on the integer grid. Faces shared with a
/// neighbouring cloud voxel are suppressed via [`Self::exposed_faces`], so
/// the emitted quad count stays tiny.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelCloudCell {
    /// World-space minimum corner of the voxel.
    pub min: [f32; 3],
    /// Voxel extents: `(CLOUD_CELL_BLOCKS, CLOUD_LAYER_HEIGHT, CLOUD_CELL_BLOCKS)`.
    pub size: [f32; 3],
    /// Stratum index: `0` is the wide base, `1..` are stepped puffy caps.
    pub layer: u8,
    /// Bitmask of `FACE_*` bits for faces not shared with a neighbour.
    pub exposed_faces: u8,
}

impl VoxelCloudCell {
    /// Number of quads a mesher has to emit for this voxel (0..=6).
    pub fn quad_count(&self) -> u32 {
        self.exposed_faces.count_ones()
    }
}

/// Time-of-day and weather lighting for the cloud layer. All fields are
/// already gamma-ready linear-ish tints in `0..=1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudLighting {
    /// Up-facing quad tint: crisp white by day, deep slate by night, warm
    /// peach/amber at the horizon during sunrise and sunset.
    pub top_rgb: [f32; 3],
    /// Side-facing quad tint.
    pub side_rgb: [f32; 3],
    /// Down-facing quad tint: soft cool underside by day, heavily darkened
    /// under rain or a thunderstorm.
    pub bottom_rgb: [f32; 3],
    /// Warm horizon-rim strength in `0..=1` (peaks at `sun_elevation ~= 0`).
    pub rim_warmth: f32,
}

/// Computes the cloud-layer tint for a sun elevation in `-1..=1` (negative:
/// below the horizon, i.e. night) and precipitation strengths in `0..=1`.
pub fn compute_cloud_lighting(
    sun_elevation: f32,
    rain_strength: f32,
    thunder_strength: f32,
) -> CloudLighting {
    let e = clamped(sun_elevation, -1.0, 1.0);
    let rain = clamped(rain_strength, 0.0, 1.0);
    let thunder = clamped(thunder_strength, 0.0, 1.0);

    // 0 = deep night, 1 = full day. A little margin keeps late dusk moody.
    let day = smoothstep(-0.10, 0.22, e);
    // Warm rim only while the sun is near AND above the horizon: a midnight
    // sun_elevation of -0.4 stays cool, not peach.
    let dusk = 1.0 - smoothstep(0.0, 0.4, e.abs());
    let above = smoothstep(-0.16, -0.02, e);
    let rim = dusk * above;

    let night_top = [0.10, 0.12, 0.21];
    let night_side = [0.07, 0.09, 0.17];
    let night_bottom = [0.05, 0.07, 0.13];
    let day_top = [0.99, 1.0, 1.0];
    let day_side = [0.84, 0.88, 0.95];
    let day_bottom = [0.62, 0.68, 0.80];
    let amber = [1.0, 0.62, 0.34];
    // Golden under-lit rim (sun below the deck) and the cool silver-blue
    // rim the moon paints while the sun is fully down.
    let amber_under = [0.98, 0.55, 0.30];
    let moon_silver = [0.60, 0.70, 0.92];

    let mut top = lerp3(night_top, day_top, day);
    let mut side = lerp3(night_side, day_side, day);
    let mut bottom = lerp3(night_bottom, day_bottom, day);

    top = lerp3(top, amber, rim * 0.55);
    side = lerp3(side, amber, rim * 0.30);
    // Sunrise/sunset light strikes the undersides first: warm under-lit rims.
    bottom = lerp3(bottom, amber_under, rim * 0.30);
    // Night: a subtle cool silver-blue moonlight rim, strongest right at the
    // dusk/dawn band where the moon still catches the deck edges.
    let night_f = 1.0 - day;
    let moon_rim = night_f * (0.05 + 0.08 * dusk * (1.0 - above));
    top = lerp3(top, moon_silver, moon_rim);
    side = lerp3(side, moon_silver, night_f * 0.04);

    // Storm underside darkening; the top greys only slightly.
    let storm = (rain * 0.55 + thunder).clamp(0.0, 1.0);
    let under_mul = 1.0 - 0.65 * storm;
    let top_mul = 1.0 - 0.25 * storm;
    let side_mul = 1.0 - 0.40 * storm;
    for i in 0..3 {
        top[i] *= top_mul;
        side[i] *= side_mul;
    }
    let bottom = [
        bottom[0] * under_mul,
        bottom[1] * under_mul,
        bottom[2] * under_mul,
    ];

    CloudLighting {
        top_rgb: top,
        side_rgb: side,
        bottom_rgb: bottom,
        rim_warmth: rim,
    }
}

/// Generates the layered voxel cloud field around `(camera_x, camera_z)`.
///
/// Deterministic for identical arguments: the field is an integer-grid value
/// noise keyed by cell coordinates, scrolled by `world_time_ticks` at
/// [`CLOUD_SCROLL_BLOCKS_PER_TICK`]. Cells drift smoothly within a cell
/// period and appear/vanish only at whole-cell boundaries — exactly the
/// chunky vanilla feel. Hidden faces between adjacent cloud voxels (and
/// between stacked strata) are suppressed in [`VoxelCloudCell::exposed_faces`].
///
/// Each `layer` slot is a coherent atmospheric deck — `1` deck is classic
/// low cumulus, `2` decks add high cirrus, `3` decks insert mid altocumulus
/// (see [`cloud_deck_spec`] for altitudes, thicknesses, translucency and
/// parallax speeds). Deck coverage nests inside the deck below, so the base
/// always reads wider and the upper decks step up like castellanus turrets;
/// [`cloud_shadow_factor`] evaluates the identical occupancy pipeline for
/// synchronized drifting ground shadows.
///
/// Returns an empty `Vec` unless `config.enabled && config.layered_voxel_clouds`.
/// Output is bounded by [`MAX_CLOUD_CELLS`] and all positions are finite.
pub fn generate_layered_voxel_clouds(
    config: &VanillaPlusNatureConfig,
    camera_x: f32,
    camera_z: f32,
    world_time_ticks: u64,
) -> Vec<VoxelCloudCell> {
    layered_clouds_inner(config, camera_x, camera_z, world_time_ticks, 0.0)
}

/// Weather-aware variant of [`generate_layered_voxel_clouds`]: the storm
/// coverage ramp relaxes each deck's density threshold — clear skies are
/// scattered, rain thickens the decks and thunder builds a near-overcast
/// storm deck.
pub fn generate_layered_voxel_clouds_with_weather(
    config: &VanillaPlusNatureConfig,
    camera_x: f32,
    camera_z: f32,
    world_time_ticks: u64,
    weather: WeatherKind,
) -> Vec<VoxelCloudCell> {
    layered_clouds_inner(
        config,
        camera_x,
        camera_z,
        world_time_ticks,
        weather_storm(weather),
    )
}

fn layered_clouds_inner(
    config: &VanillaPlusNatureConfig,
    camera_x: f32,
    camera_z: f32,
    world_time_ticks: u64,
    storm: f32,
) -> Vec<VoxelCloudCell> {
    if !config.enabled || !config.layered_voxel_clouds {
        return Vec::new();
    }
    let storm = clamped(storm, 0.0, 1.0);
    let layers = config.effective_cloud_layers();
    let height = config.effective_cloud_height();

    let cam_x = finite_or(camera_x, 0.0).clamp(-MAX_COORD, MAX_COORD) as f64;
    let cam_z = finite_or(camera_z, 0.0).clamp(-MAX_COORD, MAX_COORD) as f64;
    // u64::MAX ticks * 0.4 stays far below i64/f64 range limits.
    let scroll = world_time_ticks as f64 * CLOUD_SCROLL_BLOCKS_PER_TICK as f64;

    let cell = CLOUD_CELL_BLOCKS as f64;
    let cx0 = ((cam_x + scroll) / cell).floor() as i64;
    let cz0 = (cam_z / cell).floor() as i64;

    // Occupancy set for hidden-face suppression across all decks. Decks nest
    // inside the deck beneath them in the same column, so coverage always
    // reads widest at the base and steps in with altitude.
    let mut occupied: BTreeSet<(i64, u8, i64)> = BTreeSet::new();
    let mut cells: Vec<VoxelCloudCell> = Vec::new();
    for dz in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
        for dx in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
            let cx = cx0 + i64::from(dx);
            let cz = cz0 + i64::from(dz);
            for layer in 0..layers {
                if column_occupied(layer, layers, cx, cz, scroll, storm) {
                    occupied.insert((cx, layer, cz));
                } else {
                    break; // nested decks: nothing above a missing lower deck
                }
            }
        }
    }

    for dz in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
        for dx in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
            let cx = cx0 + i64::from(dx);
            let cz = cz0 + i64::from(dz);
            // A fully-enclosed voxel is never emitted; gate bottom-up so an
            // emitted upper-deck voxel always has an emitted voxel below it.
            let mut emitted_below = false;
            for layer in 0..layers {
                if !occupied.contains(&(cx, layer, cz)) {
                    break;
                }
                let mut mask = ALL_FACES;
                if occupied.contains(&(cx + 1, layer, cz)) {
                    mask &= !FACE_POS_X;
                }
                if occupied.contains(&(cx - 1, layer, cz)) {
                    mask &= !FACE_NEG_X;
                }
                if occupied.contains(&(cx, layer + 1, cz)) {
                    mask &= !FACE_POS_Y;
                }
                if layer > 0 && occupied.contains(&(cx, layer - 1, cz)) {
                    mask &= !FACE_NEG_Y;
                }
                if occupied.contains(&(cx, layer, cz + 1)) {
                    mask &= !FACE_POS_Z;
                }
                if occupied.contains(&(cx, layer, cz - 1)) {
                    mask &= !FACE_NEG_Z;
                }
                if mask == 0 || (layer > 0 && !emitted_below) {
                    break;
                }
                emitted_below = true;
                let spec = cloud_deck_spec(layer, layers);
                cells.push(VoxelCloudCell {
                    min: [
                        (cx as f64 * cell - scroll) as f32,
                        height + spec.altitude_offset,
                        (cz as f64 * cell) as f32,
                    ],
                    size: [CLOUD_CELL_BLOCKS, spec.thickness, CLOUD_CELL_BLOCKS],
                    layer,
                    exposed_faces: mask,
                });
            }
        }
    }
    cells
}

// ---------------------------------------------------------------------------
// Subtle nature ambience particles
// ---------------------------------------------------------------------------

/// One bounded ambience particle. `kind` carries the per-kind colour/phase
/// data; `pos`/`vel`/`age_sec`/`ttl_sec` are the shared kinematics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NatureParticle {
    pub kind: NatureParticleKind,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub age_sec: f32,
    pub ttl_sec: f32,
}

impl NatureParticle {
    /// `true` once the particle should be dropped.
    pub fn expired(&self) -> bool {
        self.age_sec >= self.ttl_sec
            || matches!(self.kind, NatureParticleKind::RainRipple { radius, max_radius } if radius >= max_radius)
    }
}

/// The ambience particle families. Each stays deliberately quiet:
/// small counts, soft colours, no screen-space noise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NatureParticleKind {
    /// A leaf drifting down from an exposed canopy block. `rgb` carries the
    /// tint: green in spring/summer, gold-to-crimson with the autumn ramp.
    /// `flutter_phase` desynchronises the side-to-side sway; `drift` scales
    /// the horizontal wander (`1.0` normal, wider in autumn wind).
    FallingLeaf {
        rgb: [f32; 3],
        flutter_phase: f32,
        drift: f32,
    },
    /// A sakura petal (桜) spiralling down from a blossom canopy:
    /// a gentle horizontal corkscrew with a slower descent than leaves and
    /// soft warm sakura-pink tint variations. `spiral_phase` desynchronises
    /// the swirl.
    SakuraPetal { rgb: [f32; 3], spiral_phase: f32 },
    /// A soft-pulsing ember of light in warm/temperate biomes at night.
    /// `buoyancy` is `0.0` for a true hovering firefly; positive values make
    /// the mote ride a thermal updraft — the channel nether embers use.
    Firefly {
        glow_rgb: [f32; 3],
        pulse_phase: f32,
        buoyancy: f32,
    },
    /// An expanding ring on a rain-exposed water surface.
    RainRipple { radius: f32, max_radius: f32 },
    /// A soft white puff at the base of a vertical water drop.
    WaterfallMist { alpha: f32 },
    /// A powder-snow mote (粉雪) settling slowly through cold air.
    /// `shimmer_phase` desynchronises the crisp frost glimmer the renderer
    /// reads off the particle.
    SnowMote { shimmer_phase: f32 },
}

/// World-sampled spawn hints for one ambience tick. Callers fill the site
/// lists from whatever chunk data they already have; each list is read up to
/// [`MAX_ENV_SITES`] entries and non-finite positions are skipped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NatureAmbienceEnv {
    /// Current precipitation state.
    pub weather: WeatherKind,
    /// Dusk/night flag gating fireflies.
    pub is_night: bool,
    /// `true` in warm/temperate biomes (swamp, mangrove, plains, meadow).
    /// Freezing and desert biomes must pass `false`.
    pub firefly_biome: bool,
    /// `true` under cherry-blossom canopy: `leaf_blocks` shed sakura petals
    /// instead of tinted leaves.
    pub cherry_blossom: bool,
    /// Seasonal autumn amount in `0..=1` (manual override, combined with the
    /// live [`SeasonAmbienceInput::autumn_ramp`]).
    pub autumn_factor: f32,
    /// Live season state from an `SSN1` Season Mod sync. `seasonless` by
    /// default — identical behaviour to running no season mod.
    pub season: SeasonAmbienceInput,
    /// Camera/player anchor for free-roaming spawns (fireflies, snow motes).
    pub origin: [f32; 3],
    /// Exposed leaf/canopy block centres that can shed a leaf.
    pub leaf_blocks: Vec<[f32; 3]>,
    /// Blossom canopy centres (cherry, sakura, flowering azalea, modded
    /// `*blossom*`/`*sakura*`/`*cherry*` canopies) that shed spiral petals —
    /// boosted during the mid-spring bloom, dormant in deep winter.
    pub blossom_blocks: Vec<[f32; 3]>,
    /// Conifer/evergreen canopy centres that keep shedding needles at a
    /// subtle constant cadence, unaffected by winter dormancy.
    pub evergreen_blocks: Vec<[f32; 3]>,
    /// Rain-exposed water surface points (ripples only while precipitating).
    pub rain_exposed_water: Vec<[f32; 3]>,
    /// `(base position, vertical drop height)` of water columns; mist only
    /// forms for drops of at least 3 blocks.
    pub waterfall_bases: Vec<([f32; 3], f32)>,
}

/// Dimension/context cues the retained ambience sim needs but the
/// per-tick [`NatureAmbienceEnv`] site lists do not carry: whether the
/// anchor sits in a nether-style dimension (searing air, rising embers) or
/// sealed underground below the sky (floating dust motes), and the
/// effective sky light at the anchor.
///
/// `Default` is the neutral overworld surface — no embers, no dust motes —
/// so a caller that never installs cues gets exactly the pre-cue behaviour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DimensionAmbienceCues {
    /// Nether-style dimension: hot dry air, no sky, no weather — embers
    /// rise on thermals regardless of the day/night flag.
    pub nether: bool,
    /// Anchor sealed underground (deep cave, no direct sky): pale dust
    /// motes may float through the gloom when `sky_light` is low.
    pub underground: bool,
    /// Effective sky light at the ambience anchor `0..=15` (`0` = fully
    /// sealed cave; dust motes need `<= 11`).
    pub sky_light: u8,
}

impl Default for DimensionAmbienceCues {
    fn default() -> Self {
        Self {
            nether: false,
            underground: false,
            sky_light: 15,
        }
    }
}

/// Retained ambience state: a strictly capped particle `Vec` plus the
/// deterministic tick counter used for rate-limited spawning and the
/// installed [`DimensionAmbienceCues`].
#[derive(Clone, Debug)]
pub struct NatureAmbienceState {
    tier: VfxBudgetTier,
    tick_index: u64,
    particles: Vec<NatureParticle>,
    cues: DimensionAmbienceCues,
}

impl Default for NatureAmbienceState {
    fn default() -> Self {
        Self::new(VfxBudgetTier::Balanced)
    }
}

impl NatureAmbienceState {
    pub fn new(tier: VfxBudgetTier) -> Self {
        Self {
            tier,
            tick_index: 0,
            particles: Vec::new(),
            cues: DimensionAmbienceCues::default(),
        }
    }

    pub const fn tier(&self) -> VfxBudgetTier {
        self.tier
    }

    /// The installed dimension/context cues (neutral overworld surface by
    /// default).
    pub fn dimension_cues(&self) -> DimensionAmbienceCues {
        self.cues
    }

    /// Install dimension/context cues: set `nether` for rising ember
    /// ambience, `underground` + a low `sky_light` for floating cave dust
    /// motes. `sky_light` is clamped to `0..=15`.
    pub fn set_dimension_cues(&mut self, cues: DimensionAmbienceCues) {
        self.cues = DimensionAmbienceCues {
            sky_light: cues.sky_light.min(15),
            ..cues
        };
    }

    /// Re-tiers the budget; live particles beyond the new cap are trimmed
    /// (oldest first) so the invariant holds immediately.
    pub fn set_tier(&mut self, tier: VfxBudgetTier) {
        self.tier = tier;
        let cap = tier.max_particles();
        if self.particles.len() > cap {
            let excess = self.particles.len() - cap;
            self.particles.drain(..excess);
        }
    }

    pub fn particles(&self) -> &[NatureParticle] {
        &self.particles
    }

    pub fn len(&self) -> usize {
        self.particles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    fn try_spawn(&mut self, quota: &mut usize, particle: NatureParticle) {
        if *quota == 0 || self.particles.len() >= self.tier.max_particles() {
            return;
        }
        *quota -= 1;
        self.particles.push(particle);
    }
}

fn valid_site(p: &[f32; 3]) -> bool {
    p.iter().all(|c| c.is_finite() && c.abs() <= MAX_COORD)
}

/// How strongly the shared wind field advects each particle family's
/// position, `0..=1`: fluttering leaves and petals ride the wind most,
/// hovering motes least, anchored ripples and mist not at all. Applied to
/// position only — the authored `vel` kinematics stay canonical.
fn wind_advection(kind: &NatureParticleKind) -> f32 {
    match kind {
        NatureParticleKind::FallingLeaf { .. } => 0.5,
        NatureParticleKind::SakuraPetal { .. } => 0.55,
        NatureParticleKind::Firefly { .. } => 0.2,
        NatureParticleKind::SnowMote { .. } => 0.15,
        NatureParticleKind::RainRipple { .. } | NatureParticleKind::WaterfallMist { .. } => 0.0,
    }
}

/// Advances the ambience simulation by one tick.
///
/// Ageing and motion are deterministic functions of `dt_sec` (clamped to
/// `0..=0.25`) and each particle's stored phase; spawning is a deterministic
/// function of `tick_index` and site index — no RNG, no clock. The particle
/// count can never exceed `state.tier().max_particles()` and no more than
/// `max_spawns_per_tick()` particles appear in one call. When the pack or
/// the ambience toggle is off, existing particles still age out naturally
/// but nothing new spawns.
pub fn tick_nature_ambience(
    state: &mut NatureAmbienceState,
    config: &VanillaPlusNatureConfig,
    env: &NatureAmbienceEnv,
    dt_sec: f32,
) {
    let dt = if dt_sec.is_finite() {
        dt_sec.clamp(0.0, MAX_AMBIENCE_DT_SEC)
    } else {
        0.0
    };
    state.tick_index = state.tick_index.wrapping_add(1);
    let tick = state.tick_index;
    // One shared wind field for every drifting family: the same vector that
    // advects the cloud decks and drives block sway pushes particles too.
    let wind = shared_wind_vector(tick, env.weather);

    for p in state.particles.iter_mut() {
        p.age_sec += dt;
        match &mut p.kind {
            NatureParticleKind::FallingLeaf {
                flutter_phase,
                drift,
                ..
            } => {
                let ph = *flutter_phase;
                let d = finite_or(*drift, 1.0).clamp(0.4, 2.5);
                p.vel = [
                    0.28 * d * (p.age_sec * 2.1 + ph).sin() + 0.10 * (d - 1.0),
                    -0.5,
                    0.28 * d * (p.age_sec * 1.7 + ph * 1.31).cos(),
                ];
            }
            NatureParticleKind::SakuraPetal { spiral_phase, .. } => {
                let ph = *spiral_phase;
                // Gentle corkscrew: the horizontal velocity rotates around
                // the fall axis while the descent stays slower than a leaf's.
                p.vel = [
                    0.30 * (p.age_sec * 2.3 + ph).cos(),
                    -0.34 + 0.07 * (p.age_sec * 1.9 + ph).sin(),
                    -0.30 * (p.age_sec * 2.3 + ph).sin(),
                ];
            }
            NatureParticleKind::Firefly {
                pulse_phase,
                buoyancy,
                ..
            } => {
                let ph = *pulse_phase;
                let lift = finite_or(*buoyancy, 0.0).clamp(-0.2, 0.6);
                p.vel = [
                    0.22 * (p.age_sec * 0.9 + ph).sin(),
                    0.16 * (p.age_sec * 1.3 + ph * 2.0).sin() + lift,
                    0.22 * (p.age_sec * 0.8 + ph).cos(),
                ];
            }
            NatureParticleKind::RainRipple { radius, .. } => {
                *radius += 1.8 * dt;
            }
            NatureParticleKind::WaterfallMist { alpha } => {
                p.vel = [0.05 * (p.age_sec * 1.4).sin(), 0.35, 0.05];
                let t = clamped(p.age_sec / p.ttl_sec, 0.0, 1.0);
                *alpha = 0.35 * (PI * t).sin().max(0.0);
            }
            NatureParticleKind::SnowMote { shimmer_phase } => {
                let ph = *shimmer_phase;
                p.vel = [
                    0.15 * (p.age_sec * 0.8 + ph).sin()
                        + 0.05 * (p.age_sec * 0.37 + ph * 2.0).sin(),
                    -0.26 - 0.05 * (p.age_sec * 0.6 + ph).sin(),
                    0.15 * (p.age_sec * 0.7 + ph).cos(),
                ];
            }
        }
        // Shared-wind advection: the particle's own `vel` keeps its authored
        // kinematics (spiral, hover, settle) while the whole family also
        // drifts with the same field that moves the clouds and swayed
        // foliage. `wind_advection` is the per-kind susceptibility.
        let adv = wind_advection(&p.kind);
        p.pos[0] += (p.vel[0] + wind[0] * adv) * dt;
        p.pos[1] += p.vel[1] * dt;
        p.pos[2] += (p.vel[2] + wind[1] * adv) * dt;
    }
    state.particles.retain(|p| !p.expired());

    if !config.enabled || !config.nature_ambience {
        return;
    }
    let mut quota = state.tier.max_spawns_per_tick();

    // Seasonal modulation: the sanitized env season combines with the
    // manual `autumn_factor` override. A seasonless input leaves every
    // scale at its neutral value.
    let season = env.season.sanitized();
    let autumn = clamped(env.autumn_factor, 0.0, 1.0).max(season.autumn_ramp());
    let drift = season.leaf_drift_scale();
    let deciduous = season.deciduous_activity();
    let tint = season.foliage_tint;

    // Falling leaves: temperate deciduous canopy. Autumn shortens the
    // period (more leaves) through `autumn`, winter dormancy thins the
    // spawn rate deterministically, and `cherry_blossom` swaps the family
    // to spiral sakura petals.
    if !env.leaf_blocks.is_empty() && deciduous > 0.0 {
        let leaf_period =
            (LEAF_PERIOD_MAX as f32 - (LEAF_PERIOD_MAX - LEAF_PERIOD_MIN) as f32 * autumn)
                .round() as u64;
        let petal_period = sakura_period(season.sakura_bloom());
        let period = if env.cherry_blossom {
            petal_period
        } else {
            leaf_period
        };
        for (i, site) in env.leaf_blocks.iter().take(MAX_ENV_SITES).enumerate() {
            if !valid_site(site) || !(tick + i as u64 * 7).is_multiple_of(period) {
                continue;
            }
            let h1 = hash01(i as i64, tick as i64, SEED_LEAF);
            let h2 = hash01(tick as i64, i as i64, SEED_LEAF ^ 0x5a5a);
            // Winter dormancy thins deciduous shedding deterministically.
            if hash01(i as i64, tick as i64, SEED_LEAF ^ 0x77dd) >= deciduous {
                continue;
            }
            let pos = [
                site[0] + (h1 - 0.5) * 0.8,
                site[1],
                site[2] + (h2 - 0.5) * 0.8,
            ];
            let kind = if env.cherry_blossom {
                NatureParticleKind::SakuraPetal {
                    rgb: sakura_petal_rgb(h1, h2, tint),
                    spiral_phase: h1 * TAU,
                }
            } else {
                NatureParticleKind::FallingLeaf {
                    rgb: autumn_leaf_rgb(autumn, h1, tint),
                    flutter_phase: h1 * TAU,
                    drift,
                }
            };
            let falling = matches!(kind, NatureParticleKind::FallingLeaf { .. });
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind,
                    pos,
                    vel: [0.0, if falling { -0.5 } else { -0.34 }, 0.0],
                    age_sec: 0.0,
                    ttl_sec: if falling {
                        5.0 + 4.0 * h2
                    } else {
                        4.5 + 3.5 * h2
                    },
                },
            );
        }
    }

    // Sakura petals (桜吹雪): dedicated blossom canopy sites — cherry,
    // sakura, flowering azalea and modded `*blossom*`/`*sakura*` leaves —
    // boosted during the mid-spring bloom and dormant in deep winter.
    if !env.blossom_blocks.is_empty() && deciduous > 0.0 {
        let period = sakura_period(season.sakura_bloom());
        for (i, site) in env.blossom_blocks.iter().take(MAX_ENV_SITES).enumerate() {
            if !valid_site(site) || !(tick + i as u64 * 11).is_multiple_of(period) {
                continue;
            }
            let h1 = hash01(i as i64, tick as i64, SEED_PETAL);
            let h2 = hash01(tick as i64, i as i64, SEED_PETAL ^ 0x5a5a);
            // Blossom canopies are deciduous too: bare in deep winter.
            if h2 >= deciduous {
                continue;
            }
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::SakuraPetal {
                        rgb: sakura_petal_rgb(h1, h2, tint),
                        spiral_phase: h1 * TAU,
                    },
                    pos: [
                        site[0] + (h1 - 0.5) * 0.9,
                        site[1],
                        site[2] + (h2 - 0.5) * 0.9,
                    ],
                    vel: [0.0, -0.34, 0.0],
                    age_sec: 0.0,
                    ttl_sec: 4.5 + 3.5 * h2,
                },
            );
        }
    }

    // Evergreen needles: a subtle constant trickle that ignores both the
    // autumn ramp and winter dormancy — conifers keep their needles.
    for (i, site) in env.evergreen_blocks.iter().take(MAX_ENV_SITES).enumerate() {
        if !valid_site(site) || !(tick + i as u64 * 13).is_multiple_of(EVERGREEN_NEEDLE_PERIOD) {
            continue;
        }
        let h1 = hash01(i as i64, tick as i64, SEED_NEEDLE);
        let h2 = hash01(tick as i64, i as i64, SEED_NEEDLE ^ 0x5a5a);
        state.try_spawn(
            &mut quota,
            NatureParticle {
                kind: NatureParticleKind::FallingLeaf {
                    rgb: [
                        ((0.16 + 0.06 * h1) * tint[0]).clamp(0.0, 1.0),
                        ((0.34 + 0.08 * h2) * tint[1]).clamp(0.0, 1.0),
                        (0.18 * tint[2]).clamp(0.0, 1.0),
                    ],
                    flutter_phase: h1 * TAU,
                    drift: 0.8,
                },
                pos: [
                    site[0] + (h1 - 0.5) * 0.8,
                    site[1],
                    site[2] + (h2 - 0.5) * 0.8,
                ],
                vel: [0.0, -0.5, 0.0],
                age_sec: 0.0,
                ttl_sec: 5.0 + 4.0 * h2,
            },
        );
    }

    // Fireflies: dusk/night only, warm/temperate biomes only, scaled by the
    // season (full summer, tapering late autumn, silent winter).
    let firefly_scale = season.firefly_scale();
    if env.is_night
        && env.firefly_biome
        && firefly_scale > 0.0
        && tick.is_multiple_of(FIREFLY_PERIOD)
    {
        let origin = [
            finite_or(env.origin[0], 0.0).clamp(-MAX_COORD, MAX_COORD),
            finite_or(env.origin[1], 64.0).clamp(-64.0, 384.0),
            finite_or(env.origin[2], 0.0).clamp(-MAX_COORD, MAX_COORD),
        ];
        for k in 0..FIREFLIES_PER_EVENT {
            if hash01(k as i64, tick as i64, SEED_FIREFLY ^ 0x5151) >= firefly_scale {
                continue;
            }
            let h1 = hash01(tick as i64, k as i64, SEED_FIREFLY);
            let h2 = hash01(k as i64, tick as i64, SEED_FIREFLY ^ 0x33cc);
            let angle = h1 * TAU;
            let radius = 5.0 + 9.0 * h2;
            let pos = [
                origin[0] + angle.cos() * radius,
                origin[1] + 0.5 + 2.5 * h1,
                origin[2] + angle.sin() * radius,
            ];
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::Firefly {
                        glow_rgb: [0.80 + 0.20 * h1, 1.0, 0.30 + 0.15 * h2],
                        pulse_phase: h2 * TAU,
                        buoyancy: 0.0,
                    },
                    pos,
                    vel: [0.0; 3],
                    age_sec: 0.0,
                    ttl_sec: 8.0 + 6.0 * h2,
                },
            );
        }
    }

    // Drifting powder snow (粉雪): only under a snow-enabled or wintry
    // season state — never in tropical or seasonless climates.
    let snow = season.snow_amount();
    if snow > 0.0 && tick.is_multiple_of(SNOW_MOTE_PERIOD) {
        let origin = [
            finite_or(env.origin[0], 0.0).clamp(-MAX_COORD, MAX_COORD),
            finite_or(env.origin[1], 64.0).clamp(-64.0, 384.0),
            finite_or(env.origin[2], 0.0).clamp(-MAX_COORD, MAX_COORD),
        ];
        for k in 0..SNOW_MOTES_PER_EVENT {
            let h1 = hash01(tick as i64, k as i64, SEED_SNOW);
            let h2 = hash01(k as i64, tick as i64, SEED_SNOW ^ 0x77aa);
            if h2 >= snow {
                continue;
            }
            let angle = h1 * TAU;
            let radius = 3.0 + 11.0 * h2;
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::SnowMote {
                        shimmer_phase: h1 * TAU,
                    },
                    pos: [
                        origin[0] + angle.cos() * radius,
                        origin[1] + 1.0 + 6.0 * h1,
                        origin[2] + angle.sin() * radius,
                    ],
                    vel: [0.0, -0.26, 0.0],
                    age_sec: 0.0,
                    ttl_sec: 5.5 + 4.0 * h2,
                },
            );
        }
    }

    // Deep-cave dust motes: sealed underground with little sky light, pale
    // motes drift through the gloom — sharing the firefly glow channel with
    // a dim grey-brown tint and neutral buoyancy so they float in place.
    // Spawn counts scale with the budget tier like every other family.
    let cues = state.cues;
    let cue_count = match state.tier {
        VfxBudgetTier::LowSpec => 1usize,
        VfxBudgetTier::Balanced => 2,
        VfxBudgetTier::High => 3,
    };
    if cues.underground && cues.sky_light <= DUST_MAX_SKY_LIGHT && tick.is_multiple_of(DUST_MOTE_PERIOD)
    {
        let origin = [
            finite_or(env.origin[0], 0.0).clamp(-MAX_COORD, MAX_COORD),
            finite_or(env.origin[1], 64.0).clamp(-64.0, 384.0),
            finite_or(env.origin[2], 0.0).clamp(-MAX_COORD, MAX_COORD),
        ];
        for k in 0..cue_count {
            let h1 = hash01(tick as i64, k as i64, SEED_DUST);
            let h2 = hash01(k as i64, tick as i64, SEED_DUST ^ 0x33cc);
            let angle = h1 * TAU;
            let radius = 1.5 + 5.5 * h2;
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::Firefly {
                        glow_rgb: [0.42 + 0.10 * h1, 0.40 + 0.08 * h2, 0.36 + 0.06 * h1],
                        pulse_phase: h2 * TAU,
                        buoyancy: 0.0,
                    },
                    pos: [
                        origin[0] + angle.cos() * radius,
                        origin[1] + 0.4 + 3.0 * h2,
                        origin[2] + angle.sin() * radius,
                    ],
                    vel: [0.0; 3],
                    age_sec: 0.0,
                    ttl_sec: 7.0 + 5.0 * h2,
                },
            );
        }
    }

    // Nether embers: hot sparks riding thermals in nether-style dimensions,
    // glowing through the firefly channel with positive buoyancy so they
    // rise. Independent of the day/night flag — there is no sky cycle below.
    if cues.nether && tick.is_multiple_of(EMBER_PERIOD) {
        let origin = [
            finite_or(env.origin[0], 0.0).clamp(-MAX_COORD, MAX_COORD),
            finite_or(env.origin[1], 64.0).clamp(-64.0, 384.0),
            finite_or(env.origin[2], 0.0).clamp(-MAX_COORD, MAX_COORD),
        ];
        for k in 0..cue_count {
            let h1 = hash01(tick as i64, k as i64, SEED_EMBER);
            let h2 = hash01(k as i64, tick as i64, SEED_EMBER ^ 0x77aa);
            let angle = h1 * TAU;
            let radius = 2.0 + 7.0 * h2;
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::Firefly {
                        glow_rgb: [1.00, 0.30 + 0.22 * h1, 0.05 + 0.08 * h2],
                        pulse_phase: h2 * TAU,
                        buoyancy: 0.22 + 0.25 * h1,
                    },
                    pos: [
                        origin[0] + angle.cos() * radius,
                        origin[1] + 0.5 + 3.5 * h1,
                        origin[2] + angle.sin() * radius,
                    ],
                    vel: [0.0; 3],
                    age_sec: 0.0,
                    ttl_sec: 4.0 + 3.5 * h2,
                },
            );
        }
    }

    // Rain ripples: precipitation only, exposed water surfaces only.
    if env.weather.is_precipitating() {
        let period = match env.weather {
            WeatherKind::Thunder => RIPPLE_PERIOD_THUNDER,
            _ => RIPPLE_PERIOD_RAIN,
        };
        for (i, site) in env
            .rain_exposed_water
            .iter()
            .take(MAX_ENV_SITES)
            .enumerate()
        {
            if !valid_site(site) || !(tick + i as u64 * 3).is_multiple_of(period) {
                continue;
            }
            let h = hash01(i as i64, tick as i64, SEED_RIPPLE);
            let max_radius = 0.7 + 0.5 * h;
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::RainRipple {
                        radius: 0.08,
                        max_radius,
                    },
                    pos: *site,
                    vel: [0.0; 3],
                    age_sec: 0.0,
                    ttl_sec: max_radius / 1.8 + 0.5,
                },
            );
        }
    }

    // Waterfall mist: only at the base of real drops (>= 3 blocks).
    for (i, (base, drop_height)) in env.waterfall_bases.iter().take(MAX_ENV_SITES).enumerate() {
        if !valid_site(base)
            || !drop_height.is_finite()
            || *drop_height < MIST_MIN_DROP
            || !(tick + i as u64 * 5).is_multiple_of(MIST_PERIOD)
        {
            continue;
        }
        let h = hash01(i as i64, tick as i64, SEED_MIST);
        state.try_spawn(
            &mut quota,
            NatureParticle {
                kind: NatureParticleKind::WaterfallMist { alpha: 0.0 },
                pos: [
                    base[0] + (h - 0.5) * 0.6,
                    base[1] + 0.2,
                    base[2] + (0.5 - h) * 0.6,
                ],
                vel: [0.0, 0.35, 0.0],
                age_sec: 0.0,
                ttl_sec: 2.5 + 1.5 * h,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Gentle weather-driven wind sway
// ---------------------------------------------------------------------------

/// Plant category choosing how the sway amplitude distributes over local Y.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindSwayCategory {
    /// Grass, ferns, flowers, saplings: base pinned, tip sways.
    ShortGrass,
    /// Crops (wheat, carrots, ...): same pinned-base profile as grass.
    Crop,
    /// Leaf blocks: a tiny uniform rustle, no pivot.
    Leaves,
    /// Lanterns and chains hanging from a top pivot point.
    HangingLantern,
    /// Banners: top-pinned cloth that flutters — a travelling wave down the
    /// free edge makes banners visibly flappier than a rigid lantern.
    Banner,
}

/// Time-decoupled wind parameters produced once per frame/tick and shared by
/// every swayed vertex.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindSwayParams {
    /// Scaled amplitude in blocks. `0.0` when the pack or the sway toggle is
    /// off — a zero-strength parameter set always yields zero offsets.
    pub strength: f32,
    /// Animation time in seconds derived from `world_time_ticks`.
    pub time_sec: f32,
    /// Angular frequency in radians per second.
    pub frequency: f32,
}

/// Builds sway parameters for the current weather and tick. `strength` is
/// `0.0` unless `config.enabled && config.gentle_wind_sway`, so callers can
/// feed the result straight into [`compute_wind_sway`] without branching.
pub fn wind_sway_params(
    config: &VanillaPlusNatureConfig,
    weather: WeatherKind,
    world_time_ticks: u64,
) -> WindSwayParams {
    let strength = if config.enabled && config.gentle_wind_sway {
        weather.wind_strength()
    } else {
        0.0
    };
    WindSwayParams {
        strength,
        time_sec: (world_time_ticks % WIND_TIME_WRAP_TICKS) as f32 * 0.05,
        frequency: WIND_SWAY_FREQUENCY,
    }
}

/// Per-vertex sway offset in blocks.
///
/// * `ShortGrass`/`Crop`: amplitude scales with `local_y.clamp(0,1)`, so the
///   base at `local_y == 0.0` is always pinned at exactly `[0,0,0]` — roots
///   never detach.
/// * `Leaves`: a small uniform 3-axis rustle.
/// * `HangingLantern`: amplitude scales with `1 - local_y` clamped to `0..=1`,
///   so the top pivot at `local_y == 1.0` is pinned and the body swings below.
/// * `Banner`: same top pin as a lantern, but the free cloth edge carries a
///   secondary wave travelling down `local_y` so banners flutter instead of
///   swinging like a rigid pendulum.
///
/// Output is always finite; non-finite `local` components fall back to the
/// pinned position (0.0 for plants, 1.0 for lanterns and banners).
pub fn compute_wind_sway(
    params: &WindSwayParams,
    category: WindSwayCategory,
    block: [i32; 3],
    local: [f32; 3],
) -> [f32; 3] {
    let phase = params.time_sec * params.frequency + block_phase(block);
    let s0 = phase.sin();
    let s1 = (phase * 0.63 + 1.3).sin();
    match category {
        WindSwayCategory::ShortGrass | WindSwayCategory::Crop => {
            let attach = finite_or(local[1], 0.0).clamp(0.0, 1.0);
            let amp = params.strength * 0.12 * attach;
            [amp * s0, 0.0, amp * s1 * 0.6]
        }
        WindSwayCategory::Leaves => {
            let lx = finite_or(local[0], 0.0);
            let ly = finite_or(local[1], 0.0);
            let lz = finite_or(local[2], 0.0);
            let amp = params.strength * 0.05;
            [
                amp * (phase * 1.7 + lx * 3.1).sin(),
                amp * 0.4 * (phase * 1.9 + ly * 2.3).cos(),
                amp * (phase * 1.6 + lz * 2.7).sin(),
            ]
        }
        WindSwayCategory::HangingLantern => {
            let attach = (1.0 - finite_or(local[1], 1.0)).clamp(0.0, 1.0);
            let amp = params.strength * 0.10 * attach;
            [amp * s0, 0.0, amp * s1]
        }
        WindSwayCategory::Banner => {
            // Top-pinned cloth: the free edge flutters with a wave that
            // travels down the banner, so lower `local_y` ripples more.
            let attach = (1.0 - finite_or(local[1], 1.0)).clamp(0.0, 1.0);
            let ly = finite_or(local[1], 1.0).clamp(0.0, 1.0);
            let flutter = 0.7 * s0 + 0.3 * (phase * 2.1 - ly * 4.0).sin();
            let amp = params.strength * 0.14 * attach;
            [
                amp * flutter,
                0.0,
                amp * (s1 + 0.4 * (phase * 2.3 - ly * 3.0).cos()),
            ]
        }
    }
}

/// Syntactically valid WGSL helper computing the same sway on the GPU.
/// `kind` is the [`WindSwayCategory`] discriminant (`0` grass, `1` crop,
/// `2` leaves, `3` hanging lantern); `block_phase` is the per-block hash phase
/// computed identically to [`block_phase`].
///
/// This text is byte-stable: the cutout shadow caster embeds it verbatim so
/// leaf shadows move with the visible canopy. New sway kinds get their own
/// constants — see [`MEGU_WIND_SWAY_BANNER_WGSL_FN`] for `kind == 4`.
pub const MEGU_WIND_SWAY_WGSL_FN: &str = r#"
fn megu_wind_sway(local_pos: vec3<f32>, block_phase: f32, time_sec: f32,
                  strength: f32, kind: u32) -> vec3<f32> {
    let phase = time_sec * 1.4 + block_phase;
    let s0 = sin(phase);
    let s1 = sin(phase * 0.63 + 1.3);
    var attach = clamp(local_pos.y, 0.0, 1.0);
    var amp = strength * 0.12;
    if (kind == 2u) {
        attach = 1.0;
        amp = strength * 0.05;
    } else if (kind == 3u) {
        attach = clamp(1.0 - local_pos.y, 0.0, 1.0);
        amp = strength * 0.10;
    }
    if (kind == 2u) {
        return vec3<f32>(amp * sin(phase * 1.7 + local_pos.x * 3.1),
                         amp * 0.4 * cos(phase * 1.9 + local_pos.y * 2.3),
                         amp * sin(phase * 1.6 + local_pos.z * 2.7));
    }
    return vec3<f32>(amp * attach * s0, 0.0, amp * attach * s1 * 0.6);
}
"#;

/// Banner-capable extension of [`MEGU_WIND_SWAY_WGSL_FN`]: identical motion
/// for kinds `0..=3`, plus `kind == 4u` ([`WindSwayCategory::Banner`]) —
/// top-pinned cloth whose free edge flutters with a wave travelling down
/// `local_y`. Kept as a separate const because the cutout shadow caster
/// embeds the base function verbatim.
pub const MEGU_WIND_SWAY_BANNER_WGSL_FN: &str = r#"
fn megu_wind_sway_banner(local_pos: vec3<f32>, block_phase: f32, time_sec: f32,
                         strength: f32, kind: u32) -> vec3<f32> {
    let phase = time_sec * 1.4 + block_phase;
    let s0 = sin(phase);
    let s1 = sin(phase * 0.63 + 1.3);
    var attach = clamp(local_pos.y, 0.0, 1.0);
    var amp = strength * 0.12;
    if (kind == 2u) {
        attach = 1.0;
        amp = strength * 0.05;
    } else if (kind == 3u) {
        attach = clamp(1.0 - local_pos.y, 0.0, 1.0);
        amp = strength * 0.10;
    } else if (kind == 4u) {
        attach = clamp(1.0 - local_pos.y, 0.0, 1.0);
        amp = strength * 0.14;
    }
    if (kind == 2u) {
        return vec3<f32>(amp * sin(phase * 1.7 + local_pos.x * 3.1),
                         amp * 0.4 * cos(phase * 1.9 + local_pos.y * 2.3),
                         amp * sin(phase * 1.6 + local_pos.z * 2.7));
    }
    if (kind == 4u) {
        let flutter = 0.7 * s0 + 0.3 * sin(phase * 2.1 - local_pos.y * 4.0);
        return vec3<f32>(amp * attach * flutter, 0.0,
                         amp * attach * (s1 + 0.4 * cos(phase * 2.3 - local_pos.y * 3.0)));
    }
    return vec3<f32>(amp * attach * s0, 0.0, amp * attach * s1 * 0.6);
}
"#;

// ---------------------------------------------------------------------------
// Minimal emissive mob eyes
// ---------------------------------------------------------------------------

/// Which naturally-glowing vanilla feature the hint applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MobEmissiveFeature {
    /// Eye glow mask only (enderman, spiders, phantom, drowned).
    Eyes,
    /// Flame rods / burning core (blaze, magma cube).
    FlameOrCore,
    /// Full soft body glow (glow squid, allay).
    BodyGlow,
}

/// A subtle emissive tint for one mob feature: colour plus `0..=1`
/// intensity. Purely a glow-mask hint — no geometry, rig or UV change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MobEmissiveHint {
    pub feature: MobEmissiveFeature,
    pub rgb: [f32; 3],
    pub intensity: f32,
}

/// Maps vanilla mobs with naturally glowing features to a subtle emissive
/// hint. `entity_id` may be namespaced (`"minecraft:enderman"`); only the
/// path component is matched. Returns `None` for everything else — the table
/// is intentionally minimal.
pub fn emissive_mob_feature(entity_id: &str) -> Option<MobEmissiveHint> {
    let path = resource_path(entity_id);
    let hint = |feature, rgb, intensity| {
        Some(MobEmissiveHint {
            feature,
            rgb,
            intensity,
        })
    };
    match path {
        "enderman" => hint(MobEmissiveFeature::Eyes, [0.72, 0.28, 0.88], 0.85),
        "spider" | "cave_spider" => hint(MobEmissiveFeature::Eyes, [0.90, 0.10, 0.08], 0.70),
        "phantom" => hint(MobEmissiveFeature::Eyes, [0.62, 0.88, 0.72], 0.60),
        "drowned" => hint(MobEmissiveFeature::Eyes, [0.20, 0.80, 0.80], 0.55),
        "blaze" => hint(MobEmissiveFeature::FlameOrCore, [1.00, 0.60, 0.10], 0.90),
        "magma_cube" => hint(MobEmissiveFeature::FlameOrCore, [1.00, 0.35, 0.05], 0.80),
        "warden" => hint(MobEmissiveFeature::FlameOrCore, [0.20, 0.90, 0.85], 0.80),
        "glow_squid" => hint(MobEmissiveFeature::BodyGlow, [0.30, 0.90, 0.85], 0.70),
        "allay" => hint(MobEmissiveFeature::BodyGlow, [0.55, 0.80, 1.00], 0.50),
        _ => None,
    }
}

/// Config-aware wrapper around [`emissive_mob_feature`]: returns `None`
/// unless `config.enabled && config.emissive_mob_eyes`.
pub fn emissive_mob_hint(
    config: &VanillaPlusNatureConfig,
    entity_id: &str,
) -> Option<MobEmissiveHint> {
    if config.enabled && config.emissive_mob_eyes {
        emissive_mob_feature(entity_id)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Mod integration: registries, block classifiers, biome ambience
// ---------------------------------------------------------------------------

/// Fallback leaf tint for auto-classified canopy with no registered colour.
/// Matches the spring/summer base used by the ambience leaf spawner.
pub const FALLBACK_LEAF_RGB: [f32; 3] = [0.30, 0.52, 0.20];
/// Warm yellow-green firefly tint for humid overworld-style mod biomes
/// (swamps, plains, forests).
pub const WARM_FIREFLY_RGB: [f32; 3] = [0.90, 1.00, 0.35];
/// Soft cyan-violet firefly tint for `#c:is_magical` biomes (Twilight
/// Forest groves, Ars Nouveau starbuncle clearings, ...).
pub const MAGICAL_FIREFLY_RGB: [f32; 3] = [0.60, 0.70, 1.00];

/// Minimum atmosphere density for fireflies and rain ripples.
const MIN_AMBIENT_ATMOSPHERE: f32 = 0.25;
/// Minimum atmosphere density for layered clouds and falling leaves.
const MIN_SKY_ATMOSPHERE: f32 = 0.15;
/// Biome temperature at or below which fireflies and ripples stay off.
const FIREFLY_MIN_TEMPERATURE: f32 = 0.15;
/// Temperature above which a biome with no precipitation counts as dry.
const DRY_INFERENCE_TEMPERATURE: f32 = 0.95;

/// How falling-leaf particles from one canopy block should be tinted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeafParticleStyle {
    /// Base particle colour in linear `0..=1` rgb.
    pub rgb: [f32; 3],
    /// Whether the seasonal autumn factor may blend this leaf towards amber.
    /// Evergreen, flowering and dead canopies register `false`.
    pub allow_autumn_tint: bool,
    /// Whether this canopy sheds spiral sakura petals (桜) instead of plain
    /// leaves: cherry, sakura, blossom, flowering-azalea and fruit-blossom
    /// canopies — vanilla and modded — register `true`.
    pub sakura_petals: bool,
}

/// Registry of modded blocks and entities that join the nature systems.
///
/// Lookups try the exact (namespaced) id first and fall back to the bare
/// path component, so a pack may register either `"mymod:maple_leaves"` or
/// `"maple_leaves"`. Every colour is sanitised on registration — a NaN or
/// out-of-range component can never reach a particle or glow mask.
#[derive(Clone, Debug, Default)]
pub struct NatureModRegistry {
    mod_leaves: BTreeMap<String, LeafParticleStyle>,
    mod_emissive_mobs: BTreeMap<String, MobEmissiveHint>,
}

impl NatureModRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `block_id` (namespaced, e.g. `"biomesoplenty:maple_leaves"`,
    /// `"twilightforest:rainbow_oak_leaves"`, `"quark:lavender_blossom_leaves"`,
    /// or a bare path) as a leaf-shedding canopy block with an explicit tint.
    /// Non-finite rgb components fall back to [`FALLBACK_LEAF_RGB`] and the
    /// result is clamped to `0..=1`.
    pub fn register_mod_leaf(&mut self, block_id: &str, rgb: [f32; 3], allow_autumn_tint: bool) {
        let key = block_id.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        let safe = [
            finite_or(rgb[0], FALLBACK_LEAF_RGB[0]).clamp(0.0, 1.0),
            finite_or(rgb[1], FALLBACK_LEAF_RGB[1]).clamp(0.0, 1.0),
            finite_or(rgb[2], FALLBACK_LEAF_RGB[2]).clamp(0.0, 1.0),
        ];
        let sakura_petals = is_blossom_canopy_name(resource_path(&key));
        self.mod_leaves.insert(
            key,
            LeafParticleStyle {
                rgb: safe,
                allow_autumn_tint,
                sakura_petals,
            },
        );
    }

    /// Registers an emissive glow hint for a modded entity, e.g.
    /// `"twilightforest:lich"`, `"alexsmobs:enderiophage"` or
    /// `"ars_nouveau:starbuncle"`. rgb and intensity are clamped to `0..=1`
    /// (non-finite components become `0.0`). Glow mask only — registering a
    /// mob never changes its geometry, rig or animation.
    pub fn register_mod_emissive_mob(&mut self, entity_id: &str, hint: MobEmissiveHint) {
        let key = entity_id.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        let safe = MobEmissiveHint {
            feature: hint.feature,
            rgb: [
                finite_or(hint.rgb[0], 0.0).clamp(0.0, 1.0),
                finite_or(hint.rgb[1], 0.0).clamp(0.0, 1.0),
                finite_or(hint.rgb[2], 0.0).clamp(0.0, 1.0),
            ],
            intensity: finite_or(hint.intensity, 0.0).clamp(0.0, 1.0),
        };
        self.mod_emissive_mobs.insert(key, safe);
    }

    /// Registered leaf style for `block_id`: exact namespaced match first,
    /// then the bare path. `None` when unregistered.
    pub fn registered_leaf(&self, block_id: &str) -> Option<LeafParticleStyle> {
        let key = block_id.trim().to_ascii_lowercase();
        self.mod_leaves
            .get(key.as_str())
            .or_else(|| self.mod_leaves.get(resource_path(&key)))
            .copied()
    }

    /// Registered emissive hint for `entity_id`: exact namespaced match
    /// first, then the bare path. `None` when unregistered.
    pub fn registered_emissive_mob(&self, entity_id: &str) -> Option<MobEmissiveHint> {
        let key = entity_id.trim().to_ascii_lowercase();
        self.mod_emissive_mobs
            .get(key.as_str())
            .or_else(|| self.mod_emissive_mobs.get(resource_path(&key)))
            .copied()
    }
}

/// `true` when `path` ends with `_<token>` — an underscore-delimited suffix,
/// so `"glowfern"` never matches `fern` and `"catalog"` never matches `log`.
fn ends_with_token(path: &str, token: &str) -> bool {
    path.len() > token.len()
        && path.ends_with(token)
        && path.as_bytes()[path.len() - token.len() - 1] == b'_'
}

fn contains_any(path: &str, hints: &[&str]) -> bool {
    hints.iter().any(|h| path.contains(*h))
}

/// Normalises a biome/block tag: trims whitespace and a leading `#`.
fn normalize_tag(tag: &str) -> &str {
    tag.trim().trim_start_matches('#')
}

/// `true` when any tag equals one of `accepted` (after normalising and a
/// case-insensitive compare, so `#C:Leaves` still counts).
fn has_any_tag(tags: &[&str], accepted: &[&str]) -> bool {
    tags.iter().any(|t| {
        let n = normalize_tag(t);
        accepted.iter().any(|a| n.eq_ignore_ascii_case(a))
    })
}

/// Canopy tags that mark a block as leaf-shedding across loaders.
const LEAF_BLOCK_TAGS: &[&str] = &["minecraft:leaves", "c:leaves", "forge:leaves", "leaves"];
/// Crop block tags across loaders.
const CROP_BLOCK_TAGS: &[&str] = &[
    "minecraft:crops",
    "c:crops",
    "forge:crops",
    "minecraft:crop",
    "c:crop",
];
/// Small-plant block tags: flowers and saplings sway like short grass.
const PLANT_BLOCK_TAGS: &[&str] = &[
    "minecraft:flowers",
    "minecraft:small_flowers",
    "minecraft:tall_flowers",
    "c:flowers",
    "forge:flowers",
    "minecraft:saplings",
    "c:saplings",
    "forge:saplings",
    "c:plants",
    "forge:plants",
];

/// Suffix tokens that mark a block as a small pinned-root plant. Matching is
/// underscore-delimited via [`ends_with_token`], so `cauliflower`, `seagrass`
/// and `primrose` stay unclassified.
const PLANT_SUFFIX_TOKENS: &[&str] = &[
    "grass", "fern", "flower", "flowers", "blossom", "sapling", "seedling", "sprout", "shoot",
    "stalk", "crop", "crops", "cane", "reed", "tulip", "rose", "daisy", "lily", "orchid", "poppy",
    "mushroom", "fungus",
];

/// Name endings that mark a block as a solid cube and veto every positive
/// classification — the hard fail-closed layer. A block that is tagged as a
/// crop but clearly named `*_ore` or `*_planks` stays silent rather than
/// swaying. Exact names (`"stone"`, `"dirt"`) match too.
const SOLID_NAME_HINTS: &[&str] = &[
    "stone",
    "deepslate",
    "andesite",
    "diorite",
    "granite",
    "tuff",
    "calcite",
    "slate",
    "shale",
    "basalt",
    "blackstone",
    "netherrack",
    "obsidian",
    "bedrock",
    "dripstone",
    "planks",
    "log",
    "wood",
    "stem",
    "hyphae",
    "beam",
    "pillar",
    "brick",
    "bricks",
    "tile",
    "tiles",
    "terracotta",
    "concrete",
    "concrete_powder",
    "glass",
    "sand",
    "sandstone",
    "gravel",
    "dirt",
    "mud",
    "clay",
    "ice",
    "snow",
    "rock",
    "cobblestone",
    "prismarine",
    "quartz",
    "amethyst",
    "crystal",
    "gem",
    "ore",
    "metal",
    "alloy",
    "ingot",
    "copper",
    "iron",
    "gold",
    "steel",
    "brass",
    "zinc",
    "titanium",
    "casing",
    "machine",
    "block",
    "crate",
    "barrel",
    "chest",
    "furnace",
    "anvil",
    "lamp",
    "slab",
    "stairs",
    "stair",
    "wall",
    "fence",
    "gate",
    "door",
    "trapdoor",
    "rail",
    "shelf",
    "pot",
    "vase",
];

/// `true` when the bare path names an obviously solid block. Deliberately
/// conservative: the cost of a false veto is a plant that does not sway,
/// never a cube that does.
fn is_solid_block_name(path: &str) -> bool {
    SOLID_NAME_HINTS
        .iter()
        .any(|h| path == *h || ends_with_token(path, h))
}

/// Name substrings for canopy that stays green year-round (or that autumn
/// cannot improve).
const EVERGREEN_NAME_HINTS: &[&str] = &[
    "spruce", "fir", "pine", "conifer", "cedar", "redwood", "cypress", "juniper", "holly",
];

/// Name substrings marking a blossom canopy that sheds spiral sakura
/// petals (桜): vanilla cherry, flowering azalea and the sakura/blossom
/// leaves shipped by Biomes O' Plenty, Oh The Biomes We've Gone, Regions
/// Unexplored, Autumnity, Terralith and friends.
const BLOSSOM_NAME_HINTS: &[&str] = &[
    "cherry", "sakura", "blossom", "azalea", "flowering", "jacaranda",
];

/// `true` when the bare path names a blossom canopy that sheds sakura
/// petals instead of plain leaves.
fn is_blossom_canopy_name(path: &str) -> bool {
    contains_any(path, BLOSSOM_NAME_HINTS)
}

/// Derives a leaf style for an unregistered but leaf-classified block. The
/// tint is a deterministic keyword guess with a plain green fallback —
/// fail-closed, never non-finite, and evergreen/flowering/dead names opt out
/// of the autumn blend.
fn derive_leaf_style(path: &str) -> LeafParticleStyle {
    let evergreen = contains_any(path, EVERGREEN_NAME_HINTS);
    let blossom = is_blossom_canopy_name(path);
    let rgb = if path.contains("maple") {
        [0.85, 0.45, 0.12]
    } else if path.contains("lavender")
        || path.contains("violet")
        || path.contains("jacaranda")
        || path.contains("purple")
    {
        // Checked before `blossom`: `lavender_blossom_leaves` is violet, not pink.
        [0.62, 0.52, 0.85]
    } else if path.contains("azalea") || path.contains("flowering") {
        // Flowering azalea and fruit-blossom canopies: a paler blush than cherry.
        [0.90, 0.72, 0.82]
    } else if blossom {
        [0.95, 0.70, 0.80]
    } else if path.contains("rainbow") {
        [0.80, 0.55, 0.90]
    } else if evergreen {
        [0.18, 0.36, 0.20]
    } else if path.contains("birch") || path.contains("aspen") || path.contains("ginkgo") {
        [0.62, 0.74, 0.30]
    } else if path.contains("jungle") || path.contains("palm") || path.contains("tropical") {
        [0.22, 0.58, 0.24]
    } else if path.contains("mangrove") {
        [0.20, 0.46, 0.30]
    } else if path.contains("dead") || path.contains("wither") || path.contains("brown") {
        [0.45, 0.34, 0.18]
    } else {
        FALLBACK_LEAF_RGB
    };
    let allow_autumn_tint = !(evergreen
        || blossom
        || path.contains("palm")
        || path.contains("jungle")
        || path.contains("dead")
        || path.contains("wither"));
    LeafParticleStyle {
        rgb,
        allow_autumn_tint,
        sakura_petals: blossom,
    }
}

/// Classifies a (possibly modded) block for falling-leaf particles.
///
/// Order: explicit [`NatureModRegistry`] entries first, then a hard veto for
/// obviously solid names, then `#minecraft:leaves` / `#c:leaves` /
/// `#forge:leaves` tags, then a `*_leaves` id suffix. Anything else returns
/// `None` — a `stone` or `create:brass_casing` can never shed leaves, even
/// with a misleading tag. Unregistered matches get a name-derived tint via
/// [`derive_leaf_style`], falling back to [`FALLBACK_LEAF_RGB`].
pub fn classify_leaf_block(
    block_id: &str,
    tags: &[&str],
    reg: &NatureModRegistry,
) -> Option<LeafParticleStyle> {
    let path = resource_path(block_id).to_ascii_lowercase();
    if path.is_empty() {
        return None;
    }
    if let Some(style) = reg.registered_leaf(block_id) {
        return Some(style);
    }
    if is_solid_block_name(&path) {
        return None;
    }
    if has_any_tag(tags, LEAF_BLOCK_TAGS) || path == "leaves" || ends_with_token(&path, "leaves") {
        return Some(derive_leaf_style(&path));
    }
    None
}

/// Alias for the classifier API: the same categories drive both
/// [`classify_wind_sway_block`] and [`compute_wind_sway`].
pub type WindSwayKind = WindSwayCategory;

/// Classifies a (possibly modded) block for weather wind sway.
///
/// * `#minecraft:crops` / `#c:crops` (and `forge:` variants) -> [`WindSwayCategory::Crop`]
/// * `#minecraft:leaves` / `#c:leaves`, registered leaves, `*_leaves` -> [`WindSwayCategory::Leaves`]
/// * `#minecraft:flowers` / `#c:flowers` / `#minecraft:saplings` (and `forge:`/unprefixed
///   variants) plus `*_tall_grass` / `*_fern` / `*_flower` / `*_sapling` and friends
///   -> [`WindSwayCategory::ShortGrass`] — all root-pinned at `local_y == 0.0`
/// * `*_lantern` -> [`WindSwayCategory::HangingLantern`], top-pinned at `local_y == 1.0`
/// * `*_banner` / `*_wall_banner` -> [`WindSwayCategory::Banner`], top-pinned cloth
///   that flutters with a wave travelling down the free edge
///
/// Fail-closed: `sea_lantern`/`jack_o_lantern` (solid cubes despite the
/// suffix), everything matching [`is_solid_block_name`] (`stone`, `planks`,
/// `*_ore`, `create:brass_casing`, ...), and all unrecognised ids return
/// `None`. The solid veto runs before the tag and suffix matches so a wrong
/// tag cannot make a cube sway.
pub fn classify_wind_sway_block(
    block_id: &str,
    tags: &[&str],
    reg: &NatureModRegistry,
) -> Option<WindSwayKind> {
    let path = resource_path(block_id).to_ascii_lowercase();
    if path.is_empty() {
        return None;
    }
    if reg.registered_leaf(block_id).is_some() {
        return Some(WindSwayCategory::Leaves);
    }
    // Solid cubes that merely end in `_lantern`.
    if path == "sea_lantern" || path == "jack_o_lantern" {
        return None;
    }
    if is_solid_block_name(&path) {
        return None;
    }
    if path == "lantern" || ends_with_token(&path, "lantern") {
        return Some(WindSwayCategory::HangingLantern);
    }
    if path == "banner" || ends_with_token(&path, "banner") {
        return Some(WindSwayCategory::Banner);
    }
    if has_any_tag(tags, CROP_BLOCK_TAGS) {
        return Some(WindSwayCategory::Crop);
    }
    if has_any_tag(tags, LEAF_BLOCK_TAGS) || path == "leaves" || ends_with_token(&path, "leaves") {
        return Some(WindSwayCategory::Leaves);
    }
    if has_any_tag(tags, PLANT_BLOCK_TAGS)
        || PLANT_SUFFIX_TOKENS
            .iter()
            .any(|t| ends_with_token(&path, t))
    {
        return Some(WindSwayCategory::ShortGrass);
    }
    None
}

/// Facts the caller already knows about a (possibly modded) biome. Slices
/// are borrowed — classification is a pure function over them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ModBiomeFacts<'a> {
    /// Namespaced biome id, e.g. `"twilightforest:enchanted_forest"`.
    pub biome_id: &'a str,
    /// Loader biome tags, `#` prefix optional: `#c:is_swamp`, `#c:is_forest`,
    /// `#c:is_magical`, `#c:is_cold`, `#c:is_snowy`, `#c:is_dry`,
    /// `#c:is_desert`, `#c:is_nether`, `#c:is_end`, `#c:is_underground`.
    pub tags: &'a [&'a str],
    /// Vanilla-style temperature; `<= 0.15` means frozen/snowy. Non-finite
    /// values are treated as frozen (fail closed).
    pub temperature: f32,
    /// Whether the biome reports normal precipitation.
    pub has_precipitation: bool,
    /// Atmosphere density: `1.0` for a normal overworld/fantasy biome,
    /// `0.0` for a hard-vacuum space dimension. Non-finite or negative
    /// values are treated as `0.0` (fail closed).
    pub atmosphere_density: f32,
}

/// Deterministic, fail-closed ambience decision for one biome. `allow_*`
/// gates are absolute: when a flag is `false` the caller must emit nothing
/// of that family, whatever the local block lists say.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BiomeAmbienceProfile {
    /// `true` when dusk fireflies may spawn.
    pub allow_fireflies: bool,
    /// Tint for allowed fireflies: warm yellow-green in humid biomes, soft
    /// cyan-violet in magical ones. Meaningless when `allow_fireflies` is
    /// `false` but still finite and deterministic.
    pub firefly_rgb: [f32; 3],
    /// `true` when canopy blocks may shed leaves (off in vacuum, nether, end).
    pub allow_falling_leaves: bool,
    /// `true` when rain ripples may form on exposed water.
    pub allow_rain_ripples: bool,
    /// `true` when layered voxel clouds may render (off in vacuum, nether,
    /// end and underground).
    pub allow_clouds: bool,
    /// Scale for [`wind_sway_params`] strength: `atmosphere_density` clamped
    /// to `0..=1.5`, forced to `0.0` in vacuum, underground, nether and end.
    pub wind_multiplier: f32,
}

// Biome-class tags across loaders; each family accepts c:/minecraft:/forge:.
const NETHER_BIOME_TAGS: &[&str] = &["c:is_nether", "minecraft:is_nether", "forge:is_nether"];
const END_BIOME_TAGS: &[&str] = &["c:is_end", "minecraft:is_end", "forge:is_end"];
const UNDERGROUND_BIOME_TAGS: &[&str] = &[
    "c:is_underground",
    "minecraft:is_underground",
    "forge:is_underground",
];
const DRY_BIOME_TAGS: &[&str] = &[
    "c:is_dry",
    "minecraft:is_dry",
    "forge:is_dry",
    "c:is_desert",
    "minecraft:is_desert",
    "forge:is_desert",
    "c:is_wasteland",
    "forge:is_wasteland",
];
const COLD_BIOME_TAGS: &[&str] = &[
    "c:is_cold",
    "minecraft:is_cold",
    "forge:is_cold",
    "c:is_snowy",
    "minecraft:is_snowy",
    "forge:is_snowy",
    "c:is_icy",
    "forge:is_icy",
];
const MAGICAL_BIOME_TAGS: &[&str] = &[
    "c:is_magical",
    "minecraft:is_magical",
    "forge:is_magical",
    "c:is_magic",
    "forge:is_magic",
];

/// Mod namespaces whose biomes read as magical even without a tag; used only
/// to pick the cyan-violet firefly tint, never to unlock fireflies.
const MAGICAL_NAMESPACES: &[&str] = &["twilightforest", "ars_nouveau", "botania", "aether"];

/// Classifies a (possibly modded) biome into a [`BiomeAmbienceProfile`].
///
/// Pure and deterministic over `facts`: same facts, same profile. Hostile
/// classes are detected from both tags and conservative id heuristics —
/// `nether`/`the_end`/`end_*` paths, `cave`/`deep_dark`/`underground` paths,
/// `snow`/`frozen`/`ice` paths — so an untagged biome still fails closed.
/// A `has_precipitation == false` biome above [`DRY_INFERENCE_TEMPERATURE`]
/// counts as dry even without a `is_dry`/`is_desert` tag.
pub fn classify_mod_biome_ambience(facts: &ModBiomeFacts) -> BiomeAmbienceProfile {
    let id = facts.biome_id.trim().to_ascii_lowercase();
    let namespace = id.split(':').next().unwrap_or("");
    let path = resource_path(&id);
    let density = finite_or(facts.atmosphere_density, 0.0).max(0.0);
    let temperature = finite_or(facts.temperature, 0.0);

    let is_nether = has_any_tag(facts.tags, NETHER_BIOME_TAGS)
        || path.contains("nether")
        || matches!(
            path,
            "soul_sand_valley" | "crimson_forest" | "warped_forest" | "basalt_deltas"
        );
    let is_end = has_any_tag(facts.tags, END_BIOME_TAGS)
        || path.starts_with("end_")
        || path.ends_with("_end")
        || matches!(path, "the_end" | "end" | "the_void");
    let is_underground = has_any_tag(facts.tags, UNDERGROUND_BIOME_TAGS)
        || path.contains("cave")
        || path.contains("deep_dark")
        || path.contains("underground");
    let is_dry = has_any_tag(facts.tags, DRY_BIOME_TAGS)
        || (!facts.has_precipitation && temperature > DRY_INFERENCE_TEMPERATURE);
    let is_cold = temperature <= FIREFLY_MIN_TEMPERATURE
        || has_any_tag(facts.tags, COLD_BIOME_TAGS)
        || path.contains("snow")
        || path.contains("frozen")
        || path.contains("ice_spikes");
    let is_magical = has_any_tag(facts.tags, MAGICAL_BIOME_TAGS)
        || MAGICAL_NAMESPACES.contains(&namespace)
        || path.contains("magic")
        || path.contains("enchant")
        || path.contains("fey")
        || path.contains("fairy");

    let firefly_rgb = if is_magical {
        MAGICAL_FIREFLY_RGB
    } else {
        WARM_FIREFLY_RGB
    };
    let allow_fireflies = !is_nether
        && !is_end
        && !is_underground
        && !is_dry
        && !is_cold
        && temperature > FIREFLY_MIN_TEMPERATURE
        && density >= MIN_AMBIENT_ATMOSPHERE;
    let allow_falling_leaves = !is_nether && !is_end && density >= MIN_SKY_ATMOSPHERE;
    let allow_rain_ripples = facts.has_precipitation
        && temperature > FIREFLY_MIN_TEMPERATURE
        && density >= MIN_AMBIENT_ATMOSPHERE
        && !is_nether
        && !is_end;
    let allow_clouds = density >= MIN_SKY_ATMOSPHERE && !is_nether && !is_end && !is_underground;
    let wind_multiplier = if is_nether || is_end || is_underground {
        0.0
    } else {
        density.clamp(0.0, 1.5)
    };

    BiomeAmbienceProfile {
        allow_fireflies,
        firefly_rgb,
        allow_falling_leaves,
        allow_rain_ripples,
        allow_clouds,
        wind_multiplier,
    }
}

/// Registry-aware wrapper around [`emissive_mob_feature`]: returns `None`
/// unless `config.enabled && config.emissive_mob_eyes`, then checks the
/// [`NatureModRegistry`] (exact id first, then bare path) before the vanilla
/// table — so a mod can both add its own glowing mobs and override a vanilla
/// tint. Glow mask only, zero geometry changes either way.
pub fn emissive_mob_hint_with_registry(
    entity_id: &str,
    config: &VanillaPlusNatureConfig,
    reg: &NatureModRegistry,
) -> Option<MobEmissiveHint> {
    if !(config.enabled && config.emissive_mob_eyes) {
        return None;
    }
    reg.registered_emissive_mob(entity_id)
        .or_else(|| emissive_mob_feature(entity_id))
}

/// Atmosphere-gated variant of [`generate_layered_voxel_clouds`]: returns an
/// empty `Vec` when `allow_clouds` is `false` or the atmosphere is too thin
/// (`atmosphere_density < 0.15`, non-finite counting as `0.0`) — zero clouds
/// in space, nether, end and underground dimensions. Otherwise identical to
/// the base generator, including the `config.enabled` gate.
pub fn generate_layered_voxel_clouds_with_atmosphere(
    config: &VanillaPlusNatureConfig,
    camera_x: f32,
    camera_z: f32,
    world_time_ticks: u64,
    atmosphere_density: f32,
    allow_clouds: bool,
) -> Vec<VoxelCloudCell> {
    if !allow_clouds || finite_or(atmosphere_density, 0.0) < MIN_SKY_ATMOSPHERE {
        return Vec::new();
    }
    generate_layered_voxel_clouds(config, camera_x, camera_z, world_time_ticks)
}

/// Atmosphere-scaled variant of [`wind_sway_params`]: multiplies the sway
/// strength by `atmosphere_density.clamp(0.0, 1.0)`, so sway fades smoothly
/// as the atmosphere thins and is exactly `0.0` in vacuum. The
/// `config.enabled && config.gentle_wind_sway` gate still applies.
pub fn wind_sway_params_with_atmosphere(
    config: &VanillaPlusNatureConfig,
    weather: WeatherKind,
    world_time_ticks: u64,
    atmosphere_density: f32,
) -> WindSwayParams {
    let mut params = wind_sway_params(config, weather, world_time_ticks);
    params.strength *= finite_or(atmosphere_density, 0.0).clamp(0.0, 1.0);
    params
}

// ---------------------------------------------------------------------------
// Voxel DDA point-light shadow occlusion
// ---------------------------------------------------------------------------

/// Hard bound on traversal steps for [`voxel_point_light_visibility`]:
/// a point light never needs to see past two dozen voxel faces, so the
/// query stays O(1) microsecond-cheap for CPU/GPU dynamic-light culling.
pub const MAX_LIGHT_SHADOW_STEPS: u32 = 24;

/// Micro-offset (in blocks) applied to the two side taps of
/// [`voxel_point_light_soft_visibility`].
const SOFT_SHADOW_TAP_OFFSET: f32 = 0.24;

/// Bounded Amanatides–Woo 3D voxel DDA from `light_pos` toward
/// `target_pos`: walks the integer voxel grid, testing
/// `is_opaque(x, y, z)` on every intermediate cell, and returns
///
/// * `1.0` — the light reaches `target_pos` unoccluded,
/// * `0.0` — a solid cell blocks the segment (no torch/lantern/redstone
///   light leaks through walls), an input is non-finite, or the
///   `max_steps` budget ran out before the target cell was reached —
///   fail closed everywhere: a light whose ray cannot be resolved
///   contributes no light.
///
/// The light's own cell and the target surface cell are **not** tested:
/// an emitter sitting inside a non-full block must still shine, and a
/// lit block must not occlude itself. Rays that graze a voxel edge or
/// corner step the tied axes together, so a diagonal that only touches a
/// solid cell's corner is not counted as blocked. `max_steps` is clamped
/// to [`MAX_LIGHT_SHADOW_STEPS`]. Pure `std`, NaN-safe, zero allocations.
pub fn voxel_point_light_visibility<F>(
    light_pos: [f32; 3],
    target_pos: [f32; 3],
    max_steps: u32,
    mut is_opaque: F,
) -> f32
where
    F: FnMut(i32, i32, i32) -> bool,
{
    if !light_pos.iter().chain(target_pos.iter()).all(|v| v.is_finite()) {
        return 0.0;
    }
    let delta = [
        target_pos[0] - light_pos[0],
        target_pos[1] - light_pos[1],
        target_pos[2] - light_pos[2],
    ];
    let len_sq = delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2];
    if !len_sq.is_finite() || len_sq < 1.0e-12 {
        return 1.0; // same spot: nothing stands between light and target
    }

    let mut cell = [
        light_pos[0].floor() as i32,
        light_pos[1].floor() as i32,
        light_pos[2].floor() as i32,
    ];
    let target_cell = [
        target_pos[0].floor() as i32,
        target_pos[1].floor() as i32,
        target_pos[2].floor() as i32,
    ];
    if cell == target_cell {
        return 1.0;
    }

    let len = len_sq.sqrt();
    let dir = [delta[0] / len, delta[1] / len, delta[2] / len];
    let mut step = [0i32; 3];
    let mut t_max = [f32::INFINITY; 3];
    let mut t_delta = [f32::INFINITY; 3];
    for axis in 0..3 {
        let d = dir[axis];
        if d > 0.0 {
            step[axis] = 1;
            t_max[axis] = (cell[axis].saturating_add(1) as f32 - light_pos[axis]) / d;
            t_delta[axis] = 1.0 / d;
        } else if d < 0.0 {
            step[axis] = -1;
            t_max[axis] = (light_pos[axis] - cell[axis] as f32) / -d;
            t_delta[axis] = 1.0 / -d;
        }
    }

    for _ in 0..max_steps.min(MAX_LIGHT_SHADOW_STEPS) {
        // Cross every boundary sharing the smallest t: a ray grazing an
        // edge or corner steps the tied axes together and never enters
        // the merely-touched cells.
        let nearest = t_max[0].min(t_max[1]).min(t_max[2]);
        for axis in 0..3 {
            if t_max[axis] <= nearest {
                t_max[axis] += t_delta[axis];
                cell[axis] = cell[axis].saturating_add(step[axis]);
            }
        }
        if cell == target_cell {
            return 1.0;
        }
        if is_opaque(cell[0], cell[1], cell[2]) {
            return 0.0;
        }
    }
    // Step budget exhausted before the target cell: cannot prove the ray
    // arrives, so the light contributes nothing.
    0.0
}

/// 3-tap penumbra variant of [`voxel_point_light_visibility`]: the center
/// ray plus two rays offset by [`SOFT_SHADOW_TAP_OFFSET`] around the
/// target are averaged into `0.0`, `1/3`, `2/3` or `1.0`. Surfaces
/// half-shaded by a voxel edge land on the smooth fractions instead of a
/// hard 0/1 — a cheap deterministic penumbra for point-light rims.
pub fn voxel_point_light_soft_visibility<F>(
    light_pos: [f32; 3],
    target_pos: [f32; 3],
    max_steps: u32,
    mut is_opaque: F,
) -> f32
where
    F: FnMut(i32, i32, i32) -> bool,
{
    let mut probe = |dx: f32, dz: f32| {
        voxel_point_light_visibility(
            light_pos,
            [
                target_pos[0] + dx,
                target_pos[1],
                target_pos[2] + dz,
            ],
            max_steps,
            &mut is_opaque,
        )
    };
    let o = SOFT_SHADOW_TAP_OFFSET;
    (probe(0.0, 0.0) + probe(o, o) + probe(-o, o)) / 3.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud_cfg(layers: u8) -> VanillaPlusNatureConfig {
        VanillaPlusNatureConfig {
            enabled: true,
            layered_voxel_clouds: true,
            cloud_layers: layers,
            ..VanillaPlusNatureConfig::default()
        }
    }

    fn ambience_cfg() -> VanillaPlusNatureConfig {
        VanillaPlusNatureConfig {
            enabled: true,
            nature_ambience: true,
            ..VanillaPlusNatureConfig::default()
        }
    }

    fn grey_env() -> NatureAmbienceEnv {
        NatureAmbienceEnv::default()
    }

    fn advance(state: &mut NatureAmbienceState, ticks: usize) {
        let env = grey_env();
        let cfg = ambience_cfg();
        for _ in 0..ticks {
            tick_nature_ambience(state, &cfg, &env, 0.05);
        }
    }

    // -- Multi-deck clouds ------------------------------------------------

    #[test]
    fn deck_specs_have_distinct_altitude_thickness_speed_and_alpha() {
        let specs: Vec<CloudDeckSpec> =
            (0..3).map(|l| cloud_deck_spec(l, 3)).collect();
        // Distinct altitudes: cumulus base, altocumulus cap, cirrus veil.
        assert!(specs[0].altitude_offset < specs[1].altitude_offset);
        assert!(specs[1].altitude_offset < specs[2].altitude_offset);
        // Distinct thickness: wispy cirrus is thinnest.
        assert!(specs[0].thickness > specs[1].thickness);
        assert!(specs[1].thickness > specs[2].thickness);
        // Parallax: higher decks shear faster.
        assert!(specs[0].wind_speed < specs[1].wind_speed);
        assert!(specs[1].wind_speed < specs[2].wind_speed);
        // Translucency: lower decks more opaque, cirrus most translucent.
        assert!(specs[0].opacity > specs[1].opacity);
        assert!(specs[1].opacity > specs[2].opacity);
        // Deck roles: 2-deck maps layer 1 straight to cirrus.
        assert_eq!(deck_role(1, 2), 2);
        assert_eq!(deck_role(0, 3), 0);
        assert_eq!(deck_role(1, 3), 1);
        assert_eq!(deck_role(2, 3), 2);
    }

    #[test]
    fn multi_deck_cells_sit_at_distinct_altitudes_and_thicknesses() {
        let cells = generate_layered_voxel_clouds(&cloud_cfg(3), 0.0, 0.0, 6_000);
        assert!(!cells.is_empty());
        let height = cloud_cfg(3).effective_cloud_height();
        for c in &cells {
            let spec = cloud_deck_spec(c.layer, 3);
            assert!((c.min[1] - (height + spec.altitude_offset)).abs() < 1e-3);
            assert!((c.size[1] - spec.thickness).abs() < 1e-3);
        }
        // Where multiple decks coexist their slab bases differ.
        let alt: Vec<f32> = cells.iter().map(|c| c.min[1]).collect();
        let unique: std::collections::BTreeSet<u32> =
            alt.iter().map(|v| v.to_bits()).collect();
        if cells.iter().any(|c| c.layer == 1) {
            assert!(unique.len() >= 2, "cap deck must sit at a distinct altitude");
        }
    }

    #[test]
    fn upper_decks_parallax_shift_grows_with_speed() {
        // Far enough into the day that deck shifts are non-trivial.
        let scroll = 6000.0 * 0.4;
        let s0 = deck_cell_shift(0, scroll);
        let s1 = deck_cell_shift(1, scroll);
        let s2 = deck_cell_shift(2, scroll);
        assert_eq!(s0, 0, "cumulus rides the shared advection unshifted");
        assert!(s1 > s0 && s2 > s1, "higher decks must shear faster");
        // Whole-cell quantized: effective drift = scroll + shift*cell.
        let drift = |role: usize| scroll + deck_cell_shift(role, scroll) as f64 * 12.0;
        assert!(drift(2) > drift(1) && drift(1) > drift(0));
    }

    #[test]
    fn storm_weather_thickens_all_decks() {
        let clear = generate_layered_voxel_clouds_with_weather(
            &cloud_cfg(3), 0.0, 0.0, 6_000, WeatherKind::Clear);
        let rain = generate_layered_voxel_clouds_with_weather(
            &cloud_cfg(3), 0.0, 0.0, 6_000, WeatherKind::Rain);
        let thunder = generate_layered_voxel_clouds_with_weather(
            &cloud_cfg(3), 0.0, 0.0, 6_000, WeatherKind::Thunder);
        assert!(clear.len() <= rain.len(), "rain must not thin coverage");
        assert!(rain.len() <= thunder.len(), "thunder must not thin coverage");
        assert!(thunder.len() > clear.len(), "storm deck must visibly thicken");
        // Storm ramp is also monotone per column: a clear-sky cap cannot
        // vanish under storm.
        for cap in clear.iter().filter(|c| c.layer > 0) {
            assert!(thunder.iter().any(|t| t.layer == cap.layer
                && (t.min[0] - cap.min[0]).abs() < 1e-3
                && (t.min[2] - cap.min[2]).abs() < 1e-3));
        }
    }

    // -- Shared wind + ground shadows -------------------------------------

    #[test]
    fn shared_wind_vector_strength_and_determinism() {
        let clear = shared_wind_vector(1234, WeatherKind::Clear);
        assert_eq!(clear, [-CLOUD_SCROLL_BLOCKS_PER_TICK, 0.0]);
        let rain = shared_wind_vector(1234, WeatherKind::Rain);
        let thunder = shared_wind_vector(1234, WeatherKind::Thunder);
        // Stronger weather -> stronger headwind magnitude.
        assert!(rain[0].abs() > clear[0].abs());
        assert!(thunder[0].abs() > rain[0].abs());
        // Deterministic, finite.
        assert_eq!(thunder, shared_wind_vector(1234, WeatherKind::Thunder));
        for v in thunder {
            assert!(v.is_finite());
        }
    }

    #[test]
    fn cloud_shadow_factor_is_bounded_deterministic_and_storm_darkened() {
        for ticks in [0u64, 6000, 24_001] {
            for ix in -8..=8 {
                for iz in -8..=8 {
                    let x = ix as f32 * 37.5;
                    let z = iz as f32 * 41.25;
                    for layers in 1..=3u8 {
                        let c = cloud_shadow_factor(x, z, ticks, WeatherKind::Clear, layers);
                        let r = cloud_shadow_factor(x, z, ticks, WeatherKind::Rain, layers);
                        let t = cloud_shadow_factor(x, z, ticks, WeatherKind::Thunder, layers);
                        assert!((0.0..=1.0).contains(&c));
                        assert!((0.0..=1.0).contains(&r));
                        assert!((0.0..=1.0).contains(&t));
                        assert!(r <= c + 1e-6, "rain can only darken");
                        assert!(t <= r + 1e-6, "thunder can only darken");
                    }
                }
            }
        }
        // Deterministic.
        let a = cloud_shadow_factor(17.0, -31.0, 42, WeatherKind::Rain, 3);
        let b = cloud_shadow_factor(17.0, -31.0, 42, WeatherKind::Rain, 3);
        assert_eq!(a, b);
        // Non-finite inputs fail closed into a bounded value.
        let nan = cloud_shadow_factor(f32::NAN, f32::INFINITY, u64::MAX, WeatherKind::Thunder, 3);
        assert!((0.0..=1.0).contains(&nan));
    }

    #[test]
    fn cloud_shadow_drifts_in_lockstep_with_shared_wind() {
        // The cumulus-only shadow field advects exactly with wind[0]:
        // sampling downstream `wind[0]*dt` blocks later must reproduce the
        // identical factor (integer-cell field, no resampling error).
        let ticks = 60_000u64;
        let dt = 37u64;
        for layers in [1u8] {
            for &(x, z) in &[(13.0f32, 7.0f32), (-88.0, 123.5), (512.75, -640.0)] {
                let w = shared_wind_vector(ticks, WeatherKind::Clear);
                let before = cloud_shadow_factor(x, z, ticks, WeatherKind::Clear, layers);
                let after = cloud_shadow_factor(
                    x + w[0] * dt as f32,
                    z,
                    ticks + dt,
                    WeatherKind::Clear,
                    layers,
                );
                assert_eq!(before, after, "shadow must ride the shared wind vector");
            }
        }
    }

    #[test]
    fn cloud_shadow_matches_generated_sky_columns() {
        // Where the generator puts a cloud cell, ground shade must drop.
        let config = cloud_cfg(3);
        let ticks = 6_000u64;
        let cells = generate_layered_voxel_clouds(&config, 0.0, 0.0, ticks);
        assert!(!cells.is_empty());
        let shaded = cells.iter().any(|c| {
            let x = c.min[0] + c.size[0] * 0.5;
            let z = c.min[2] + c.size[2] * 0.5;
            cloud_shadow_factor(x, z, ticks, WeatherKind::Clear, 3) < 1.0
        });
        assert!(shaded, "a live cloud column must cast a ground shadow");
    }

    // -- Time-of-day cloud lighting ---------------------------------------

    #[test]
    fn cloud_lighting_runs_warm_at_dusk_cool_at_night_slate_in_storm() {
        let noon = compute_cloud_lighting(0.9, 0.0, 0.0);
        // Noon: crisp white top, near-neutral.
        assert!(noon.top_rgb[1] > 0.85 && (noon.top_rgb[0] - noon.top_rgb[2]).abs() < 0.25);
        assert!(noon.rim_warmth < 0.15);

        let sunset = compute_cloud_lighting(0.02, 0.0, 0.0);
        // Sunset: warm amber rim on top AND under-lit warm bottom — the
        // underside hue flips from cool (b > r at noon) to warm (r > b).
        assert!(sunset.rim_warmth > noon.rim_warmth);
        assert!(sunset.top_rgb[0] > sunset.top_rgb[2], "dusk top must lean warm");
        assert!(noon.bottom_rgb[0] < noon.bottom_rgb[2], "noon undersides stay cool");
        assert!(sunset.bottom_rgb[0] > sunset.bottom_rgb[2],
            "dusk undersides get the under-lit golden rim");

        let night = compute_cloud_lighting(-0.8, 0.0, 0.0);
        // Night: dark, cool silver-blue (b >= r), moonlit rim nudges top.
        let lum = night.top_rgb[0] + night.top_rgb[1] + night.top_rgb[2];
        assert!(lum < 1.2, "night tops must be dim");
        assert!(night.top_rgb[2] >= night.top_rgb[0], "night leans cool");

        let storm = compute_cloud_lighting(0.9, 0.0, 1.0);
        // Storm underside is deep slate: strictly darker than fair-weather.
        assert!(storm.bottom_rgb[0] < noon.bottom_rgb[0] * 0.6);
        assert!(storm.top_rgb[0] < noon.top_rgb[0]);
        for rgb in [storm.top_rgb, storm.side_rgb, storm.bottom_rgb] {
            assert!(rgb.iter().all(|c| (0.0..=1.0).contains(c)));
        }
    }

    // -- Cue-driven ambience particles -------------------------------------

    fn count_kinds(state: &NatureAmbienceState) -> (usize, usize, usize) {
        let mut dust = 0usize;
        let mut ember = 0usize;
        let mut firefly = 0usize;
        for p in state.particles() {
            if let NatureParticleKind::Firefly { glow_rgb, buoyancy, .. } = p.kind {
                if buoyancy > 0.05 {
                    ember += 1;
                } else if glow_rgb[1] < 0.5 {
                    dust += 1;
                } else {
                    firefly += 1;
                }
            }
        }
        (dust, ember, firefly)
    }

    #[test]
    fn deep_cave_dust_motes_spawn_only_in_gloom() {
        // Sealed cave, low sky light -> dust motes appear.
        let mut cave = NatureAmbienceState::new(VfxBudgetTier::High);
        cave.set_dimension_cues(DimensionAmbienceCues {
            underground: true,
            sky_light: 6,
            ..DimensionAmbienceCues::default()
        });
        advance(&mut cave, DUST_MOTE_PERIOD as usize);
        let (dust, ember, _) = count_kinds(&cave);
        assert!(dust > 0, "gloom cave must float dust motes");
        assert_eq!(ember, 0);

        // Same cave but bright sky light -> no dust.
        let mut lit = NatureAmbienceState::new(VfxBudgetTier::High);
        lit.set_dimension_cues(DimensionAmbienceCues {
            underground: true,
            sky_light: 15,
            ..DimensionAmbienceCues::default()
        });
        advance(&mut lit, 3 * DUST_MOTE_PERIOD as usize);
        assert_eq!(count_kinds(&lit).0, 0, "lit caves keep the air clear");

        // Underground flag off -> no dust even in darkness.
        let mut open = NatureAmbienceState::new(VfxBudgetTier::High);
        open.set_dimension_cues(DimensionAmbienceCues {
            underground: false,
            sky_light: 0,
            ..DimensionAmbienceCues::default()
        });
        advance(&mut open, 3 * DUST_MOTE_PERIOD as usize);
        assert_eq!(count_kinds(&open).0, 0);
    }

    #[test]
    fn nether_embers_rise_on_buoyancy() {
        let mut nether = NatureAmbienceState::new(VfxBudgetTier::High);
        nether.set_dimension_cues(DimensionAmbienceCues {
            nether: true,
            ..DimensionAmbienceCues::default()
        });
        advance(&mut nether, EMBER_PERIOD as usize + 1);
        let (dust, ember, firefly) = count_kinds(&nether);
        assert!(ember > 0, "nether cues must spark rising embers");
        assert_eq!((dust, firefly), (0, 0));
        // Buoyancy carries them upward: every live ember's vertical
        // velocity is positive once kinematics have run.
        for p in nether.particles() {
            if let NatureParticleKind::Firefly { buoyancy, .. } = p.kind {
                if buoyancy > 0.05 {
                    assert!(p.vel[1] > 0.0, "ember must ride its thermal: vy={}", p.vel[1]);
                }
            }
        }
    }

    #[test]
    fn cue_particles_respect_vfx_budget_tier_caps() {
        let mut low = NatureAmbienceState::new(VfxBudgetTier::LowSpec);
        low.set_dimension_cues(DimensionAmbienceCues {
            nether: true,
            underground: true,
            sky_light: 4,
        });
        advance(&mut low, 400);
        assert!(low.len() <= VfxBudgetTier::LowSpec.max_particles());
        assert!(!low.is_empty(), "cue families must actually spawn");

        let mut high = NatureAmbienceState::new(VfxBudgetTier::High);
        high.set_dimension_cues(DimensionAmbienceCues {
            nether: true,
            underground: true,
            sky_light: 4,
        });
        advance(&mut high, 400);
        assert!(high.len() <= VfxBudgetTier::High.max_particles());
        // The bigger tier sustains strictly more live cue particles.
        assert!(high.len() > low.len());
    }

    // -- Banner sway --------------------------------------------------------

    #[test]
    fn banner_blocks_classify_and_flutter_top_pinned() {
        let reg = NatureModRegistry::new();
        for id in [
            "minecraft:white_banner",
            "minecraft:red_wall_banner",
            "some_mod:war_banner",
        ] {
            assert_eq!(
                classify_wind_sway_block(id, &[], &reg),
                Some(WindSwayCategory::Banner),
                "{id} must classify as a banner"
            );
        }
        // Non-banners unaffected.
        assert_eq!(
            classify_wind_sway_block("minecraft:soul_lantern", &[], &reg),
            Some(WindSwayCategory::HangingLantern)
        );
        assert_eq!(classify_wind_sway_block("minecraft:stone", &[], &reg), None);

        // Top of the cloth (local_y == 1.0) is pinned; the free edge moves.
        let params = WindSwayParams {
            strength: 1.0,
            time_sec: 3.2,
            frequency: WIND_SWAY_FREQUENCY,
        };
        let pinned = compute_wind_sway(&params, WindSwayCategory::Banner, [0, 0, 0], [0.5, 1.0, 0.5]);
        assert_eq!(pinned, [0.0, 0.0, 0.0], "banner top pivot must stay pinned");
        let free = compute_wind_sway(&params, WindSwayCategory::Banner, [0, 0, 0], [0.5, 0.0, 0.5]);
        assert!(free[0].abs() > 1e-4 || free[2].abs() > 1e-4,
            "free banner edge must sway under wind");
        // Flutter is a wave, not a pendulum: mid-cloth differs from bottom.
        let mid = compute_wind_sway(&params, WindSwayCategory::Banner, [0, 0, 0], [0.5, 0.5, 0.5]);
        assert!(mid.iter().zip(free.iter()).any(|(a, b)| (a - b).abs() > 1e-4));
        // Zero strength -> zero sway (pack/toggle off).
        let off = WindSwayParams { strength: 0.0, ..params };
        assert_eq!(compute_wind_sway(&off, WindSwayCategory::Banner, [0, 0, 0], [0.5, 0.0, 0.5]),
            [0.0, 0.0, 0.0]);
    }

    #[test]
    fn banner_wgsl_extends_the_byte_stable_base_helper() {
        // The base helper stays verbatim (the cutout shadow caster embeds
        // it); the banner extension adds kind 4 without touching it.
        assert!(!MEGU_WIND_SWAY_WGSL_FN.contains("kind == 4u"));
        assert!(MEGU_WIND_SWAY_BANNER_WGSL_FN.contains("kind == 4u"));
        assert!(MEGU_WIND_SWAY_BANNER_WGSL_FN.contains("megu_wind_sway_banner"));
        // Both are brace/paren-balanced enough to parse.
        for src in [MEGU_WIND_SWAY_WGSL_FN, MEGU_WIND_SWAY_BANNER_WGSL_FN] {
            assert_eq!(src.matches('{').count(), src.matches('}').count());
            assert_eq!(src.matches('(').count(), src.matches(')').count());
        }
    }

    // -- Pre-rain weather forecast & cloud sky conditions -----------------

    fn forecast(
        rain: f32,
        ticks: Option<u32>,
        raining: bool,
        humidity: f32,
    ) -> CloudSkyConditions {
        evaluate_weather_cloud_conditions(&WeatherForecastState {
            rain_strength: rain,
            ticks_until_rain_change: ticks,
            currently_raining: raining,
            biome_humidity: humidity,
        })
    }

    #[test]
    fn pre_rain_forecast_gathers_dark_decks_and_wind() {
        // Megu server clock: one minute before the rain front hits vs a
        // countdown beyond the 5-minute horizon (clear dry sky).
        let soon = forecast(0.0, Some(1_200), false, 0.5);
        let far = forecast(0.0, Some(24_000), false, 0.5);
        assert!(
            soon.broken_cloud_coverage > far.broken_cloud_coverage + 0.3,
            "a minute out the deck must be markedly thicker: {} vs {}",
            soon.broken_cloud_coverage,
            far.broken_cloud_coverage
        );
        assert!(soon.underside_darkness > far.underside_darkness + 0.3);
        assert!(soon.pre_rain_wind_boost > far.pre_rain_wind_boost);
        assert!((far.broken_cloud_coverage - 0.2).abs() < 1e-6);
        assert_eq!(far.underside_darkness, 0.0);
        assert_eq!(far.pre_rain_wind_boost, 1.0);
        // Every output is bounded and finite.
        for c in [soon, far] {
            assert!((0.12..=0.95).contains(&c.broken_cloud_coverage));
            assert!((0.0..=1.0).contains(&c.underside_darkness));
            assert!((0.0..=1.0).contains(&c.silver_lining_strength));
            assert!((1.0..=1.8).contains(&c.pre_rain_wind_boost));
        }
        // Deterministic.
        assert_eq!(soon, forecast(0.0, Some(1_200), false, 0.5));
    }

    #[test]
    fn vanilla_fallback_ramp_and_humidity_drive_cloud_mood() {
        // External vanilla server (no forecast clock): the early rain
        // ramp still builds coverage and darkens undersides ahead of
        // heavy rain.
        let desert = forecast(0.15, None, true, 0.0);
        let jungle = forecast(0.15, None, true, 1.0);
        assert!(
            desert.underside_darkness > 0.1,
            "early ramp must already darken storm bellies"
        );
        assert!(desert.broken_cloud_coverage > 0.12);
        assert!(
            jungle.broken_cloud_coverage > desert.broken_cloud_coverage + 0.2,
            "jungle must stay far cloudier than desert: {} vs {}",
            jungle.broken_cloud_coverage,
            desert.broken_cloud_coverage
        );
        assert!(jungle.underside_darkness > desert.underside_darkness);

        // Bone-dry calm: sparse wisps at the coverage floor, no boost.
        let dry_calm = forecast(0.0, None, false, 0.0);
        assert!((dry_calm.broken_cloud_coverage - 0.12).abs() < 1e-6);
        assert_eq!(dry_calm.underside_darkness, 0.0);
        assert_eq!(dry_calm.silver_lining_strength, 0.0);
        assert_eq!(dry_calm.pre_rain_wind_boost, 1.0);

        // Megu parting front: rain with the clear-out countdown nearly
        // done kindles silver linings vs steady rain far from clearing.
        let parting = forecast(1.0, Some(300), true, 0.5);
        let steady = forecast(1.0, Some(5_900), true, 0.5);
        assert!(parting.silver_lining_strength > steady.silver_lining_strength);
        assert!(parting.broken_cloud_coverage < steady.broken_cloud_coverage);
    }

    // -- Voxel DDA point-light shadow occlusion ----------------------------

    #[test]
    fn voxel_dda_visibility_corridor_wall_and_hostile_inputs() {
        // Clear corridor: torch light reaches the surface.
        let clear = voxel_point_light_visibility(
            [0.5, 64.6, 0.5],
            [4.5, 64.5, 0.5],
            24,
            |_x, _y, _z| false,
        );
        assert_eq!(clear, 1.0);

        // A solid stone wall column at x=2 fully blocks the same ray.
        let wall = voxel_point_light_visibility(
            [0.5, 64.6, 0.5],
            [4.5, 64.5, 0.5],
            24,
            |x, y, z| x == 2 && y == 64 && z == 0,
        );
        assert_eq!(wall, 0.0, "no light may leak through a wall");

        // Diagonal shaft through open air stays lit.
        let diag = voxel_point_light_visibility(
            [-3.5, 70.5, -2.5],
            [6.5, 64.5, 5.5],
            24,
            |_, _, _| false,
        );
        assert_eq!(diag, 1.0);

        // Same cell: a lit block never occludes itself, even if opaque.
        assert_eq!(
            voxel_point_light_visibility([1.2, 64.3, 0.5], [1.8, 64.9, 0.9], 24, |_, _, _| true),
            1.0
        );
        // Non-finite inputs fail closed without panic.
        for (light, target) in [
            ([f32::NAN, 0.0, 0.0], [4.5, 64.5, 0.5]),
            ([0.0, 0.0, 0.0], [f32::INFINITY, 0.0, 0.0]),
            ([f32::NEG_INFINITY, 1.0, 0.0], [0.0, 0.0, 0.0]),
        ] {
            assert_eq!(
                voxel_point_light_visibility(light, target, 24, |_, _, _| false),
                0.0
            );
        }
        // Step budget exhausted before the target: fail closed.
        assert_eq!(
            voxel_point_light_visibility([0.5, 0.5, 0.5], [30.5, 0.5, 0.5], 4, |_, _, _| false),
            0.0
        );
        // max_steps clamps to the kernel bound: a short hop still resolves.
        assert_eq!(
            voxel_point_light_visibility(
                [0.5, 0.5, 0.5],
                [5.5, 0.5, 0.5],
                u32::MAX,
                |_, _, _| false
            ),
            1.0
        );
    }

    #[test]
    fn voxel_dda_soft_visibility_smooths_wall_edges() {
        // Wall column one row over: the centre ray threads the gap while
        // both offset taps clip the wall -> a 1/3 penumbra instead of a
        // hard 0/1 edge.
        let light = [0.5, 64.5, 0.9];
        let target = [4.5, 64.5, 0.9];
        let wall = |x: i32, y: i32, z: i32| x == 3 && y == 64 && z == 1;
        let hard = voxel_point_light_visibility(light, target, 24, wall);
        assert_eq!(hard, 1.0, "centre ray must thread the gap");
        let soft = voxel_point_light_soft_visibility(light, target, 24, wall);
        assert!(
            (soft - 1.0 / 3.0).abs() < 1e-6,
            "two blocked taps must yield a 1/3 penumbra, got {soft}"
        );
        // Soft visibility stays bounded and deterministic.
        assert!((0.0..=1.0).contains(&soft));
        assert_eq!(soft, voxel_point_light_soft_visibility(light, target, 24, wall));
    }
}
