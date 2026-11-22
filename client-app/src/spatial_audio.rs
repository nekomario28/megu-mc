// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Raytraced 3D spatial audio and acoustic simulation DSP engine.
//!
//! The engine couples a deterministic voxel raytracer (Amanatides-Woo DDA,
//! Fibonacci-sphere ray bundles, per-material acoustic coefficients) with a
//! real-time binaural signal path (Woodworth ITD, head-shadow ILD, occlusion
//! low-pass, Doppler, underwater muffle) plus an early-reflection multitap and
//! a Hadamard feedback-delay-network late reverb whose decay times come
//! straight out of the raytraced Sabine/Eyring estimates.
//!
//! Everything here is pure `std` Rust with no wall-clock access and no random
//! state: identical worlds, geometry, and inputs produce bit-identical output.
//! Callers amortize the expensive half themselves — [`SpatialAudioEngine::analyze`]
//! re-traces the scene and is meant to run on sound start or when the listener
//! crosses a cell boundary, while [`SpatialAudioEngine::process`] streams mono
//! PCM (world sounds or decoded `VCR1` voice frames) to stereo every audio
//! callback.

use std::collections::BTreeMap;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};

/// Speed of sound in air at sea level (m/s). Minecraft blocks are one metre,
/// so path lengths in blocks convert directly to delay times.
pub const SPEED_OF_SOUND_M_S: f32 = 343.0;
/// Default PCM sample rate for [`SpatialAudioEngine`]; matches the `VCR1`
/// voice pipeline (960-sample frames = 20 ms).
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// Most occluded direct-path cutoff: a thick stone wall leaves a deep rumble.
pub const OCCLUSION_MIN_CUTOFF_HZ: f32 = 350.0;
/// Clear-line-of-sight direct-path cutoff.
pub const OCCLUSION_MAX_CUTOFF_HZ: f32 = 20_000.0;
/// Hard ceiling on output sample magnitude after the soft limiter.
pub const OUTPUT_LIMIT: f32 = 0.98;
/// Early-reflection capture window: taps beyond 80 ms belong to the tail.
const EARLY_WINDOW_S: f32 = 0.08;
/// Strongest early reflections kept per scene.
const MAX_EARLY_TAPS: usize = 12;
/// Ray march limit; a ray surviving to this distance is "escaped to open sky".
const MAX_RAY_DIST_M: f32 = 80.0;
/// Woodworth spherical-head radius (m). ITD peaks at `r/c * (1 + pi/2)`, about
/// 0.68 ms — the classic ~32-sample interaural delay at 48 kHz.
const HEAD_RADIUS_M: f32 = 0.09;
/// Distance at which [`SpatialAudioEngine`] distance rolloff is unity.
const ROLLOFF_REF_M: f32 = 1.0;
/// Submerged listener muffle corner frequency.
const UNDERWATER_CUTOFF_HZ: f32 = 600.0;
/// Band-split corner between "low" and "mid" reverb energy (Hz).
const BAND_LOW_HZ: f32 = 300.0;
/// Band-split corner between "mid" and "high" reverb energy (Hz).
const BAND_HIGH_HZ: f32 = 4_000.0;
/// Golden angle `pi * (3 - sqrt(5))` for the deterministic Fibonacci-sphere
/// ray bundle (`f32::sqrt` is not const, so the literal is baked in).
const GOLDEN_ANGLE: f32 = 2.399_963_1;

fn sanitize_sample(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-8.0, 8.0)
    } else {
        0.0
    }
}

/// Flush non-finite and denormal state so feedback paths can neither poison
/// the mix with NaN nor spend cycles on subnormal arithmetic.
fn guard_state(x: f32) -> f32 {
    if x.is_finite() && x.abs() >= 1e-24 {
        x
    } else {
        0.0
    }
}

/// Soft-clip knee: below this magnitude the limiter is a strict identity —
/// the vast majority of mixed samples never pay for the saturating branch.
const SOFT_CLIP_KNEE: f32 = 0.85;

/// Linear-guarded rational soft clipper. `|x| <= SOFT_CLIP_KNEE` returns `x`
/// bit-exactly; beyond the knee a first-order Padé saturator
/// `knee + h·u/(h + u)` (with `h = OUTPUT_LIMIT - knee`, `u = |x| - knee`)
/// takes over. The rational form is C1-continuous at the knee (both sides
/// have slope 1 there), monotone, and strictly bounded in
/// `(-OUTPUT_LIMIT, OUTPUT_LIMIT)` ⊂ `[-1.0, 1.0]` for any finite input — one
/// reciprocal replaces the transcendental `tanh`.
#[doc(hidden)]
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    let ax = x.abs();
    if ax <= SOFT_CLIP_KNEE {
        x
    } else {
        let head = OUTPUT_LIMIT - SOFT_CLIP_KNEE;
        let u = ax - SOFT_CLIP_KNEE;
        (SOFT_CLIP_KNEE + head * u / (head + u)).copysign(x)
    }
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(-db / 20.0)
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm3(v: [f32; 3]) -> f32 {
    dot3(v, v).sqrt()
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let n = norm3(v);
    if n > 1e-9 {
        [v[0] / n, v[1] / n, v[2] / n]
    } else {
        [0.0, 0.0, 1.0]
    }
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

// ---------------------------------------------------------------------------
// Quality tiers
// ---------------------------------------------------------------------------

/// Quality preset controlling ray count, bounce depth, reverb width, and the
/// optional Ultra-only physical extras. `Off` is a hard bypass: no voxel
/// queries, no DSP state touched, bit-exact mono→stereo passthrough.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpatialAudioPreset {
    /// Zero raycasts and bit-exact passthrough — for minimal clients and to
    /// A/B against the unprocessed mix.
    Off,
    /// 16 rays × 2 bounces, 4-channel FDN, occlusion LP enabled.
    Performance,
    /// 32 rays × 3 bounces, 8-channel Hadamard FDN.
    Realistic,
    /// 64 Fibonacci rays × 4 bounces, 8-channel FDN, edge diffraction probes,
    /// and distance air absorption.
    Ultra,
}

impl SpatialAudioPreset {
    /// Fibonacci-sphere rays traced per [`AcousticTracer::compute_scene`].
    pub const fn ray_count(self) -> usize {
        match self {
            Self::Off => 0,
            Self::Performance => 16,
            Self::Realistic => 32,
            Self::Ultra => 64,
        }
    }

    /// Specular/diffuse bounces each ray may survive.
    pub const fn max_bounces(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Performance => 2,
            Self::Realistic => 3,
            Self::Ultra => 4,
        }
    }

    /// Feedback delay network channel count.
    pub const fn fdn_channels(self) -> usize {
        match self {
            Self::Off | Self::Performance => 4,
            Self::Realistic | Self::Ultra => 8,
        }
    }

    /// `true` on Ultra: orthogonal doorway/corner probes let occluded sources
    /// wrap around geometry instead of cutting out.
    pub const fn diffraction_enabled(self) -> bool {
        matches!(self, Self::Ultra)
    }

    /// `true` on Ultra: high-frequency air attenuation with distance.
    pub const fn air_absorption_enabled(self) -> bool {
        matches!(self, Self::Ultra)
    }

    /// `false` only for [`SpatialAudioPreset::Off`].
    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

// ---------------------------------------------------------------------------
// Physical material acoustics
// ---------------------------------------------------------------------------

/// Frequency band of the three-band acoustic model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreqBand {
    /// ~250 Hz region (below [`BAND_LOW_HZ`]).
    Low,
    /// ~1 kHz region.
    Mid,
    /// ~6 kHz region (above [`BAND_HIGH_HZ`]).
    High,
}

/// Broad acoustic behaviour class a block belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterialClass {
    /// Open air: rays pass freely and escaping rays end the reverb budget.
    Air,
    /// Stone, deepslate, ores, terracotta, bricks — long bright reverb.
    StoneLike,
    /// Iron/copper/gold/anvil — ultra-low absorption, metallic sheen.
    Metal,
    /// Glass/ice/amethyst — reflective mids and highs, partial transmission.
    Glass,
    /// Planks/logs/bookshelves — warm balanced absorption.
    Wood,
    /// Dirt/grass/sand/gravel/snow — high absorption, dry outdoors.
    Earth,
    /// Wool/carpet/moss/leaves — near-anechoic damping for soundproof rooms.
    SoftAbsorber,
    /// Water/lava — passes rays but muffles high frequencies.
    Liquid,
}

impl MaterialClass {
    /// Stable 1-byte code used by `WorldAcousticSnapshot` histograms so the
    /// mixer and the GUI agree on which families dominate a scene without
    /// re-classifying names.
    pub const fn code(self) -> u8 {
        match self {
            Self::Air => 0,
            Self::StoneLike => 1,
            Self::Metal => 2,
            Self::Glass => 3,
            Self::Wood => 4,
            Self::Earth => 5,
            Self::SoftAbsorber => 6,
            Self::Liquid => 7,
        }
    }

    /// Inverse of [`Self::code`]; out-of-range codes read as `Air` so a
    /// truncated/older snapshot can never misclassify air as a solid.
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::StoneLike,
            2 => Self::Metal,
            3 => Self::Glass,
            4 => Self::Wood,
            5 => Self::Earth,
            6 => Self::SoftAbsorber,
            7 => Self::Liquid,
            _ => Self::Air,
        }
    }
}

/// Per-material acoustic coefficients. Absorption and scattering follow the
/// Sabine convention (`0.0` = perfectly reflective, `1.0` = perfectly
/// absorptive/diffuse); `transmission_loss_db` is the broadband loss a direct
/// path pays per metre of the material traversed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AcousticMaterial {
    /// Behaviour class this coefficient set describes.
    pub class: MaterialClass,
    /// Sabine absorption around 250 Hz.
    pub absorption_low: f32,
    /// Sabine absorption around 1 kHz.
    pub absorption_mid: f32,
    /// Sabine absorption around 6 kHz.
    pub absorption_high: f32,
    /// Fraction of reflected energy scattered diffusely vs specular.
    pub scattering: f32,
    /// Occlusion loss per block of direct-path traversal (dB).
    pub transmission_loss_db: f32,
}

impl AcousticMaterial {
    /// Open air: no absorption, no occlusion. Escaping rays contribute zero
    /// reverb energy, which is why outdoors never sounds like a cave.
    pub const AIR: Self = Self {
        class: MaterialClass::Air,
        absorption_low: 0.0,
        absorption_mid: 0.0,
        absorption_high: 0.0,
        scattering: 0.0,
        transmission_loss_db: 0.0,
    };

    /// Canonical coefficient set for a [`MaterialClass`].
    pub const fn from_class(class: MaterialClass) -> Self {
        match class {
            MaterialClass::Air => Self::AIR,
            MaterialClass::StoneLike => Self {
                class,
                absorption_low: 0.04,
                absorption_mid: 0.06,
                absorption_high: 0.08,
                scattering: 0.35,
                transmission_loss_db: 12.0,
            },
            MaterialClass::Metal => Self {
                class,
                absorption_low: 0.05,
                absorption_mid: 0.04,
                absorption_high: 0.03,
                scattering: 0.50,
                transmission_loss_db: 14.0,
            },
            MaterialClass::Glass => Self {
                class,
                absorption_low: 0.18,
                absorption_mid: 0.06,
                absorption_high: 0.04,
                scattering: 0.15,
                transmission_loss_db: 8.0,
            },
            MaterialClass::Wood => Self {
                class,
                absorption_low: 0.18,
                absorption_mid: 0.22,
                absorption_high: 0.30,
                scattering: 0.30,
                transmission_loss_db: 7.0,
            },
            MaterialClass::Earth => Self {
                class,
                absorption_low: 0.45,
                absorption_mid: 0.60,
                absorption_high: 0.70,
                scattering: 0.60,
                transmission_loss_db: 9.0,
            },
            MaterialClass::SoftAbsorber => Self {
                class,
                absorption_low: 0.75,
                absorption_mid: 0.88,
                absorption_high: 0.92,
                scattering: 0.70,
                transmission_loss_db: 16.0,
            },
            MaterialClass::Liquid => Self {
                class,
                absorption_low: 0.08,
                absorption_mid: 0.12,
                absorption_high: 0.50,
                scattering: 0.10,
                transmission_loss_db: 1.0,
            },
        }
    }

    /// Map a Minecraft block id (`"minecraft:stone"`, `"stone"`, optionally
    /// with `[properties]`) to its acoustic material by keyword family.
    /// Unknown solid blocks default to [`MaterialClass::StoneLike`], matching
    /// the most common voxel in a cave.
    pub fn from_block_name(name: &str) -> Self {
        let short = name.rsplit(':').next().unwrap_or(name);
        let short = short.split('[').next().unwrap_or(short);

        if matches!(short, "air" | "cave_air" | "void_air") {
            return Self::AIR;
        }
        // Keyword guards that must win over generic family substrings:
        // "bedrock" contains "bed" (wood/soft) and "sandstone" contains
        // "sand" (earth), and both are unmistakably stone.
        if short.contains("bedrock") || short.contains("sandstone") {
            return Self::from_class(MaterialClass::StoneLike);
        }
        let has = |keys: &[&str]| keys.iter().any(|k| short.contains(k));
        let class = if has(&["water", "lava", "bubble_column", "kelp", "seagrass"]) {
            MaterialClass::Liquid
        } else if has(&[
            "wool",
            "carpet",
            "moss",
            "hay_block",
            "leaves",
            "sponge",
            "cobweb",
            "vine",
        ]) {
            MaterialClass::SoftAbsorber
        } else if has(&["glass", "ice", "amethyst", "pane", "sea_lantern"]) {
            MaterialClass::Glass
        } else if has(&[
            "iron",
            "copper",
            "gold",
            "anvil",
            "chain",
            "netherite",
            "bell",
            "lantern",
            "cauldron",
            "hopper",
            "minecart",
            "lodestone",
            // Modded metal families: Create's brass, alloy steels, common
            // ore metals. "casing" stays out — andesite_casing is stone.
            "brass",
            "bronze",
            "steel",
            "aluminum",
            "aluminium",
            "platinum",
            "metal",
        ]) {
            MaterialClass::Metal
        } else if has(&[
            "plank",
            "log",
            "stem",
            "hyphae",
            "wood",
            "bookshelf",
            "jukebox",
            "note_block",
            "chest",
            "barrel",
            "ladder",
            "sign",
            "door",
            "trapdoor",
            "fence",
            "button",
            "bed",
            "banner",
            "loom",
            "lectern",
            "composter",
            "crafting_table",
            "cartography_table",
            "fletching_table",
            "smithing_table",
        ]) {
            MaterialClass::Wood
        } else if has(&[
            "dirt",
            "grass",
            "gravel",
            "snow",
            "clay",
            "mud",
            "mycelium",
            "podzol",
            "farmland",
            "soul_sand",
            "soul_soil",
            "coarse_dirt",
            "sand",
        ]) {
            MaterialClass::Earth
        } else {
            MaterialClass::StoneLike
        };
        Self::from_class(class)
    }

    /// Absorption coefficient in one [`FreqBand`].
    pub fn absorption(&self, band: FreqBand) -> f32 {
        match band {
            FreqBand::Low => self.absorption_low,
            FreqBand::Mid => self.absorption_mid,
            FreqBand::High => self.absorption_high,
        }
    }

    /// Mean absorption across the three bands — used for Eyring RT60.
    pub fn mean_absorption(&self) -> f32 {
        (self.absorption_low + self.absorption_mid + self.absorption_high) / 3.0
    }

    /// `true` when the block stops a DDA ray (everything except air/liquid).
    pub fn is_solid(&self) -> bool {
        !matches!(self.class, MaterialClass::Air | MaterialClass::Liquid)
    }
}

/// O(1) `block_state_id -> AcousticMaterial` table built once when the block
/// registry loads.
///
/// Vanilla block tags (`#minecraft:wool`, `#minecraft:leaves`,
/// `#minecraft:planks`, `#minecraft:impermeable`, `#c:storage_blocks/iron`,
/// `#minecraft:logs`...) resolve to the same keyword families
/// [`AcousticMaterial::from_block_name`] encodes — and the name fallback
/// keeps working for unknown Mod blocks (`create:brass_casing` → Metal,
/// `biomesoplenty:maple_leaves` → SoftAbsorber,
/// `chipped:stained_glass` → Glass). Classification runs once per state at
/// build time, so the DDA raytracer and the snapshot histogram only ever do
/// an indexed read.
#[derive(Clone, Debug, Default)]
pub struct AcousticMaterialLut {
    materials: Vec<AcousticMaterial>,
}

impl AcousticMaterialLut {
    /// Build the table from the per-state registry name slice; `names[i]`
    /// must be the name of state `i` (empty string when unknown).
    pub fn from_state_names(names: &[String]) -> Self {
        Self {
            materials: names
                .iter()
                .map(|n| {
                    if n.is_empty() {
                        // Never-named states (builtin fallback / sparse table)
                        // behave like the dominant solid, matching the legacy
                        // opaque-cube default — never air, which would let
                        // rays pierce real walls.
                        AcousticMaterial::from_class(MaterialClass::StoneLike)
                    } else {
                        AcousticMaterial::from_block_name(n)
                    }
                })
                .collect(),
        }
    }

    /// Single state-name convenience constructor (tests, tiny tables).
    pub fn from_names_iter<'a>(names: impl Iterator<Item = &'a str>) -> Self {
        Self {
            materials: names
                .map(|n| {
                    if n.is_empty() {
                        AcousticMaterial::from_class(MaterialClass::StoneLike)
                    } else {
                        AcousticMaterial::from_block_name(n)
                    }
                })
                .collect(),
        }
    }

    /// Material for `state_id`; out-of-range ids read as StoneLike.
    #[inline]
    pub fn get(&self, state_id: u32) -> AcousticMaterial {
        self.materials
            .get(state_id as usize)
            .copied()
            .unwrap_or_else(|| AcousticMaterial::from_class(MaterialClass::StoneLike))
    }

    /// Number of states the LUT covers.
    pub fn len(&self) -> usize {
        self.materials.len()
    }

    /// `true` when no registry names were supplied.
    pub fn is_empty(&self) -> bool {
        self.materials.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Voxel world interface + dense test/offline grid
// ---------------------------------------------------------------------------

/// Read-only acoustic view of the voxel world. The live client implements
/// this over its section cache; tests use [`DenseVoxelGrid`]. Coordinates are
/// integer block cells; a cell occupies `[x, x+1)` on each axis.
pub trait VoxelAcousticWorld {
    /// Material of the block cell at `(x, y, z)`. Out-of-bounds cells should
    /// report [`AcousticMaterial::AIR`] so rays escaping the loaded region
    /// count as open sky rather than an invisible wall.
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial;

    /// `true` when a ray may travel through the cell (air or liquid).
    fn is_ray_open(&self, x: i32, y: i32, z: i32) -> bool {
        !self.acoustic_material(x, y, z).is_solid()
    }

    /// `true` when the whole 16×16×16-cell section `(sx, sy, sz)` — section
    /// coordinates, i.e. `cell.div_euclid(16)` — is uniform air. The DDA
    /// uses this as a second marching level: a ray inside a uniform-air
    /// section warps straight to its exit face instead of querying every
    /// cell. The default `false` keeps every existing implementation on the
    /// exact per-cell march, so adopting it is opt-in only.
    fn is_uniform_air_section(&self, _sx: i32, _sy: i32, _sz: i32) -> bool {
        false
    }
}

/// Simple bounds-checked dense voxel store — handy for tests, tools, and the
/// integration seam until the live section cache exposes block ids.
pub struct DenseVoxelGrid {
    origin: [i32; 3],
    sx: usize,
    sy: usize,
    sz: usize,
    cells: Vec<AcousticMaterial>,
}

impl DenseVoxelGrid {
    /// Grid covering `size` cells starting at integer cell `origin`, pre-filled
    /// with `fill`. Everything outside the bounds reports [`AcousticMaterial::AIR`].
    pub fn new(origin: [i32; 3], size: [usize; 3], fill: AcousticMaterial) -> Self {
        let len = size[0].saturating_mul(size[1]).saturating_mul(size[2]);
        Self {
            origin,
            sx: size[0],
            sy: size[1],
            sz: size[2],
            cells: vec![fill; len],
        }
    }

    /// Inclusive-inclusive box size helper: `[usize; 3]` cell extents.
    pub fn size(&self) -> [usize; 3] {
        [self.sx, self.sy, self.sz]
    }

    fn index(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        let (lx, ly, lz) = (
            x.wrapping_sub(self.origin[0]),
            y.wrapping_sub(self.origin[1]),
            z.wrapping_sub(self.origin[2]),
        );
        if lx < 0 || ly < 0 || lz < 0 {
            return None;
        }
        let (ux, uy, uz) = (lx as usize, ly as usize, lz as usize);
        if ux >= self.sx || uy >= self.sy || uz >= self.sz {
            return None;
        }
        Some((uy * self.sz + uz) * self.sx + ux)
    }

    /// Material at a cell, or [`AcousticMaterial::AIR`] outside the grid.
    pub fn get(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        self.index(x, y, z)
            .map(|i| self.cells[i])
            .unwrap_or(AcousticMaterial::AIR)
    }

    /// Set one cell; out-of-bounds writes are ignored.
    pub fn set(&mut self, x: i32, y: i32, z: i32, material: AcousticMaterial) {
        if let Some(i) = self.index(x, y, z) {
            self.cells[i] = material;
        }
    }

    /// Set one cell by block id (see [`AcousticMaterial::from_block_name`]).
    pub fn set_named(&mut self, x: i32, y: i32, z: i32, block_name: &str) {
        self.set(x, y, z, AcousticMaterial::from_block_name(block_name));
    }

    /// Fill the inclusive cell box `min..=max` with `material`.
    pub fn fill_box(&mut self, min: [i32; 3], max: [i32; 3], material: AcousticMaterial) {
        for y in min[1]..=max[1] {
            for z in min[2]..=max[2] {
                for x in min[0]..=max[0] {
                    self.set(x, y, z, material);
                }
            }
        }
    }

    /// Fill an inclusive cell box by block id.
    pub fn fill_box_named(&mut self, min: [i32; 3], max: [i32; 3], block_name: &str) {
        self.fill_box(min, max, AcousticMaterial::from_block_name(block_name));
    }
}

impl VoxelAcousticWorld for DenseVoxelGrid {
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        self.get(x, y, z)
    }
}

// ---------------------------------------------------------------------------
// Raycast results, scene description
// ---------------------------------------------------------------------------

/// What happened to one cast ray: either it struck a solid block face or it
/// reached [`MAX_RAY_DIST_M`] in open cells and is counted as escaped sky.
enum CastResult {
    Hit {
        dist: f32,
        point: [f32; 3],
        normal: [f32; 3],
        material: AcousticMaterial,
        liquid_high_loss: f32,
    },
    Escaped,
}

/// Direct-path (listener ↔ source) occlusion summary.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OcclusionInfo {
    /// Effective transmission loss in dB after the best diffraction detour
    /// (equal to the raw loss when no detour helps).
    pub transmission_loss_db: f32,
    /// Raw straight-line loss before diffraction probes ran.
    pub direct_loss_db: f32,
    /// Frequency-dependent low-pass to apply to the direct path (Hz).
    pub cutoff_hz: f32,
    /// `true` when a bent path materially beat the straight-line path.
    pub diffracted: bool,
    /// `true` when even the best detour leaves >= 60 dB of loss — the direct
    /// voice of the source is effectively gone and only its reverb remains.
    pub blocked: bool,
}

impl OcclusionInfo {
    fn clear() -> Self {
        Self {
            transmission_loss_db: 0.0,
            direct_loss_db: 0.0,
            cutoff_hz: OCCLUSION_MAX_CUTOFF_HZ,
            diffracted: false,
            blocked: false,
        }
    }
}

/// One early-reflection image: a delayed, band-weighted copy arriving from
/// `direction` (unit vector in world space, pointing from the listener toward
/// the apparent reflector).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EarlyReflectionTap {
    /// Arrival delay relative to the direct path, in samples.
    pub delay_samples: u32,
    /// World-space direction the reflection appears to come from.
    pub direction: [f32; 3],
    /// Linear energy in the low band (0..1).
    pub energy_low: f32,
    /// Linear energy in the mid band (0..1).
    pub energy_mid: f32,
    /// Linear energy in the high band (0..1).
    pub energy_high: f32,
}

/// Full raytraced acoustic description of one (source, listener) pairing:
/// early taps, three-band RT60, wet send, openness, and direct occlusion.
#[derive(Clone, Debug)]
pub struct AcousticScene {
    /// Up to [`MAX_EARLY_TAPS`] strongest early reflections, loudest first.
    pub early_reflections: Vec<EarlyReflectionTap>,
    /// Eyring RT60 around 250 Hz (seconds).
    pub rt60_low_s: f32,
    /// Eyring RT60 around 1 kHz (seconds).
    pub rt60_mid_s: f32,
    /// Eyring RT60 around 6 kHz (seconds).
    pub rt60_high_s: f32,
    /// Late-reverb send level `0..=1` derived from confinement and
    /// reflectivity (dry outdoors ≈ 0, live cave ≈ 0.8+).
    pub wet_gain: f32,
    /// Fraction of rays that escaped to open sky (`0..=1`). Outdoors with a
    /// ground plane sits above 0.9; a sealed cave is ~0.
    pub outdoor_openness: f32,
    /// Direct-path occlusion between source and listener.
    pub occlusion: OcclusionInfo,
    /// Rays actually cast — always `preset.ray_count()`, `0` when
    /// [`SpatialAudioPreset::Off`] so tests can prove the bypass is free.
    pub rays_traced: u32,
}

impl Default for AcousticScene {
    /// Dry open-air scene: zero occlusion, zero reverb, fully open sky.
    fn default() -> Self {
        Self {
            early_reflections: Vec::new(),
            rt60_low_s: 0.0,
            rt60_mid_s: 0.0,
            rt60_high_s: 0.0,
            wet_gain: 0.0,
            outdoor_openness: 1.0,
            occlusion: OcclusionInfo::clear(),
            rays_traced: 0,
        }
    }
}

/// One [`AcousticSceneCache`] slot.
struct SceneCacheEntry {
    /// World block revision the scene was traced against.
    revision: u64,
    /// Last-access stamp from the cache's monotonic logical clock.
    tick: u64,
    scene: AcousticScene,
}

/// Bounded LRU cache of raytraced [`AcousticScene`]s keyed by 2×2×2 m cell
/// and world block revision. Sound scenes change slowly — a listener pacing
/// inside the same cell re-hears the same room — so each cell pays for the
/// Fibonacci bundle once per world revision instead of once per `analyze`.
/// The clock is a purely logical counter (no `SystemTime`), so cache
/// behaviour is deterministic and replayable.
pub struct AcousticSceneCache {
    capacity: usize,
    clock: u64,
    hits: u64,
    misses: u64,
    map: BTreeMap<[i32; 3], SceneCacheEntry>,
}

impl AcousticSceneCache {
    /// Smallest accepted capacity — below this cells thrash anyway.
    pub const MIN_CAPACITY: usize = 64;
    /// Largest accepted capacity — bounds worst-case RAM per cache.
    pub const MAX_CAPACITY: usize = 256;

    /// Empty cache holding up to `capacity` cells (clamped to
    /// [`Self::MIN_CAPACITY`]..=[`Self::MAX_CAPACITY`]).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(Self::MIN_CAPACITY, Self::MAX_CAPACITY),
            clock: 0,
            hits: 0,
            misses: 0,
            map: BTreeMap::new(),
        }
    }

    /// The 2×2×2 m cache cell containing `position`.
    pub fn cell_of(position: [f32; 3]) -> [i32; 3] {
        [
            (position[0] / 2.0).floor() as i32,
            (position[1] / 2.0).floor() as i32,
            (position[2] / 2.0).floor() as i32,
        ]
    }

    /// Configured capacity after clamping.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Cells currently cached.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Successful lookups so far.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Failed lookups so far.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Look up `cell`; `None` when absent or traced against a different
    /// `world_block_revision` (a revision bump invalidates every stale
    /// entry on sight and on the next insert).
    pub fn get(&mut self, cell: [i32; 3], world_block_revision: u64) -> Option<&AcousticScene> {
        match self.map.get_mut(&cell) {
            Some(entry) if entry.revision == world_block_revision => {
                self.hits += 1;
                self.clock += 1;
                entry.tick = self.clock;
                Some(&entry.scene)
            }
            _ => {
                self.misses += 1;
                None
            }
        }
    }

    /// Cache `scene` for `cell` at `world_block_revision`, dropping every
    /// stale-revision entry and evicting the least-recently-used cell when
    /// full.
    pub fn insert(&mut self, cell: [i32; 3], world_block_revision: u64, scene: AcousticScene) {
        self.map
            .retain(|_, e| e.revision == world_block_revision);
        self.clock += 1;
        let tick = self.clock;
        if self.map.len() >= self.capacity && !self.map.contains_key(&cell) {
            if let Some((&victim, _)) = self
                .map
                .iter()
                .min_by_key(|(_, e)| e.tick)
            {
                self.map.remove(&victim);
            }
        }
        self.map.insert(
            cell,
            SceneCacheEntry {
                revision: world_block_revision,
                tick,
                scene,
            },
        );
    }

    /// Look up `cell`, tracing and caching with `trace` on a miss. The
    /// returned scene is always the one stored for the current revision.
    pub fn get_or_trace(
        &mut self,
        cell: [i32; 3],
        world_block_revision: u64,
        trace: impl FnOnce() -> AcousticScene,
    ) -> &AcousticScene {
        if self.map.get(&cell).is_some_and(|e| e.revision == world_block_revision) {
            // Reuse `get` for the tick/hit bookkeeping.
            return self.get(cell, world_block_revision).expect("checked above");
        }
        self.misses += 1;
        self.insert(cell, world_block_revision, trace());
        &self.map.get(&cell).expect("just inserted").scene
    }
}

// ---------------------------------------------------------------------------
// DDA raytracer
// ---------------------------------------------------------------------------

fn fibonacci_dirs(n: usize) -> Vec<[f32; 3]> {
    let mut dirs = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f32 + 0.5) / n as f32;
        let y = 1.0 - 2.0 * t;
        let r = (1.0 - y * y).max(0.0).sqrt();
        let theta = i as f32 * GOLDEN_ANGLE;
        dirs.push([r * theta.cos(), y, r * theta.sin()]);
    }
    dirs
}

/// Deterministic unit vector used to perturb specular reflections by a
/// material's scattering coefficient — a keyed hash of (ray, bounce, face),
/// never RNG, so impulse responses reproduce exactly.
fn scatter_dir(normal: [f32; 3], seed: u32) -> [f32; 3] {
    let mut h = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 16;
    let u = (h & 0xFFFF) as f32 / 65_535.0;
    let v = (h >> 16) as f32 / 65_535.0;
    // Cosine-weighted hemisphere around `normal` via orthonormal basis.
    let r = u.sqrt();
    let theta = 2.0 * PI * v;
    let (tx, ty) = (r * theta.cos(), r * theta.sin());
    let up = if normal[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let tangent = normalize3(cross3(normal, up));
    let bitangent = cross3(normal, tangent);
    normalize3([
        normal[0] * (1.0 - u).max(0.0).sqrt() + tx * tangent[0] + ty * bitangent[0],
        normal[1] * (1.0 - u).max(0.0).sqrt() + tx * tangent[1] + ty * bitangent[1],
        normal[2] * (1.0 - u).max(0.0).sqrt() + tx * tangent[2] + ty * bitangent[2],
    ])
}

/// Geometry of one diffraction probe candidate.
enum Bend {
    /// Single-bend detour through `p1`: enough for thin edges.
    One { p1: [f32; 3] },
    /// Two-bend detour through `p1` → `p2`: the path a portal or doorway
    /// actually takes — out of the direct line on both faces of the slab,
    /// travelling inside the opening between them.
    Two { p1: [f32; 3], p2: [f32; 3] },
}

/// A diffraction probe candidate plus its lower bound on penalized loss:
/// the bend tax plus the detour penalty, before any traversal loss is known.
struct DiffractionCand {
    bend: Bend,
    /// `4.0 + 0.5·detour` (one bend) or `8.0 + 0.5·detour` (two bends) — the
    /// loss this path pays even if every leg crosses open air.
    lower_bound: f32,
    /// `(bent_len - seg_len).max(0)` — folded into the candidate loss.
    detour: f32,
}

/// Stateless DDA raytracer over a [`VoxelAcousticWorld`]. One tracer serves as
/// many `compute_scene` calls as needed; all cost is in the casts.
pub struct AcousticTracer<'w> {
    world: &'w dyn VoxelAcousticWorld,
    preset: SpatialAudioPreset,
    /// Telemetry: how many 16³ uniform-air sections the macro-DDA warped
    /// through instead of stepping cell by cell.
    macro_dda_skips: AtomicU64,
    /// Telemetry: `segment_loss_db` rays cast by the diffraction prober
    /// (excludes the initial straight-line cast).
    diffraction_rays_cast: AtomicU64,
}

impl<'w> AcousticTracer<'w> {
    /// Tracer bound to `world` casting at `preset` quality.
    pub fn new(world: &'w dyn VoxelAcousticWorld, preset: SpatialAudioPreset) -> Self {
        Self {
            world,
            preset,
            macro_dda_skips: AtomicU64::new(0),
            diffraction_rays_cast: AtomicU64::new(0),
        }
    }

    /// Uniform-air 16³ sections skipped by the two-level DDA so far.
    pub fn macro_dda_skips(&self) -> u64 {
        self.macro_dda_skips.load(Ordering::Relaxed)
    }

    /// Rays the diffraction prober has cast so far (branch-and-bound prunes
    /// candidates before they cost a ray; the count proves it).
    pub fn diffraction_rays_cast(&self) -> u64 {
        self.diffraction_rays_cast.load(Ordering::Relaxed)
    }

    /// Count one diffraction probe ray.
    #[inline]
    fn count_diffraction_ray(&self) {
        self.diffraction_rays_cast.fetch_add(1, Ordering::Relaxed);
    }

    /// Two-level DDA warp: when the cell `voxel` was just stepped into sits
    /// inside a 16³ section the world reports as uniform air, every remaining
    /// in-section cell is a guaranteed no-op. Rewind `voxel`/`t_max` to the
    /// bit-identical state the naive march would hold just before the
    /// section-exit crossing and return that crossing's parametric distance;
    /// `None` when the current section is not uniform air.
    ///
    /// Exactness: each axis's `t_max` only ever advances by `+= inv[a]`, so
    /// replaying the same additions in the same order reproduces the naive
    /// accumulator bitwise — including tie ordering, which keeps the loop's
    /// lowest-index argmin picking the same exit crossing and therefore the
    /// same hit cell, distance, and normal.
    fn section_warp(
        &self,
        step: [i32; 3],
        inv: [f32; 3],
        voxel: &mut [i32; 3],
        t_max: &mut [f32; 3],
    ) -> Option<f32> {
        let sec = [voxel[0] >> 4, voxel[1] >> 4, voxel[2] >> 4];
        if !self
            .world
            .is_uniform_air_section(sec[0], sec[1], sec[2])
        {
            return None;
        }
        self.macro_dda_skips.fetch_add(1, Ordering::Relaxed);

        // Per-axis accumulated distance to the crossing that leaves the
        // section: last inside cell on `a`, then `remaining` more `+= inv`
        // from the current pending distance.
        let mut exit_dist = [f32::INFINITY; 3];
        let mut last_in = [0_i32; 3];
        for a in 0..3 {
            last_in[a] = if step[a] > 0 {
                (sec[a] + 1) * 16 - 1
            } else {
                sec[a] * 16
            };
            if inv[a].is_finite() {
                let mut d = t_max[a];
                let interior = (last_in[a] - voxel[a]) * step[a];
                for _ in 0..interior {
                    d += inv[a];
                }
                exit_dist[a] = d;
            }
        }
        // Same lowest-index-wins tie order as the march loop.
        let a_star = if exit_dist[0] <= exit_dist[1] && exit_dist[0] <= exit_dist[2] {
            0
        } else if exit_dist[1] <= exit_dist[2] {
            1
        } else {
            2
        };
        let t_star = exit_dist[a_star];
        for a in 0..3 {
            if a == a_star {
                voxel[a] = last_in[a];
                t_max[a] = t_star;
            } else if inv[a].is_finite() {
                // Interior crossings strictly below `t_star` — plus, on lower
                // indices, the exact-tie crossings the naive march resolves
                // before the exit — are replayed with their `+= inv` adds so
                // the pending distances stay bit-identical. Crossings on
                // higher-index ties stay pending; the loop resolves them.
                while t_max[a] < t_star || (t_max[a] == t_star && a < a_star) {
                    t_max[a] += inv[a];
                    voxel[a] += step[a];
                }
            }
        }
        Some(t_star)
    }

    /// Amanatides-Woo DDA from `origin` along unit `dir`, stopping at the
    /// first solid cell or [`MAX_RAY_DIST_M`]. Liquids are traversable but
    /// bleed high-band energy per cell crossed.
    fn cast(&self, origin: [f32; 3], dir: [f32; 3]) -> CastResult {
        let mut voxel = [
            origin[0].floor() as i32,
            origin[1].floor() as i32,
            origin[2].floor() as i32,
        ];
        let step = [
            if dir[0] >= 0.0 { 1 } else { -1 },
            if dir[1] >= 0.0 { 1 } else { -1 },
            if dir[2] >= 0.0 { 1 } else { -1 },
        ];
        let inv = [
            if dir[0].abs() < 1e-9 {
                f32::INFINITY
            } else {
                1.0 / dir[0].abs()
            },
            if dir[1].abs() < 1e-9 {
                f32::INFINITY
            } else {
                1.0 / dir[1].abs()
            },
            if dir[2].abs() < 1e-9 {
                f32::INFINITY
            } else {
                1.0 / dir[2].abs()
            },
        ];
        let mut t_max = [f32::INFINITY; 3];
        for a in 0..3 {
            if inv[a].is_finite() {
                let boundary = if step[a] > 0 {
                    voxel[a] as f32 + 1.0 - origin[a]
                } else {
                    origin[a] - voxel[a] as f32
                };
                t_max[a] = boundary * inv[a];
            }
        }
        let mut liquid_high_loss = 1.0_f32;
        loop {
            let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
                0
            } else if t_max[1] <= t_max[2] {
                1
            } else {
                2
            };
            let t = t_max[axis];
            if t > MAX_RAY_DIST_M {
                return CastResult::Escaped;
            }
            voxel[axis] += step[axis];
            t_max[axis] += inv[axis];
            let mut normal = [0.0_f32; 3];
            normal[axis] = -(step[axis] as f32);
            let material = self.world.acoustic_material(voxel[0], voxel[1], voxel[2]);
            match material.class {
                MaterialClass::Air => {
                    // Macro-DDA: a whole uniform-air section is crossed in
                    // one warp — bit-identical to stepping every cell.
                    if let Some(t_star) =
                        self.section_warp(step, inv, &mut voxel, &mut t_max)
                    {
                        if t_star > MAX_RAY_DIST_M {
                            return CastResult::Escaped;
                        }
                    }
                }
                MaterialClass::Liquid => {
                    // Water carries bass well but eats highs: ~5%/m.
                    liquid_high_loss *= 0.95;
                }
                _ => {
                    return CastResult::Hit {
                        dist: t,
                        point: [
                            origin[0] + dir[0] * t,
                            origin[1] + dir[1] * t,
                            origin[2] + dir[2] * t,
                        ],
                        normal,
                        material,
                        liquid_high_loss,
                    };
                }
            }
        }
    }

    /// Sum of `transmission_loss_db` over every cell the `from → to` segment
    /// enters. Air contributes 0 dB, liquids ~1 dB, solids their rated loss.
    fn segment_loss_db(&self, from: [f32; 3], to: [f32; 3]) -> f32 {
        let delta = sub3(to, from);
        let seg_len = norm3(delta);
        if seg_len < 1e-6 {
            return 0.0;
        }
        let dir = [delta[0] / seg_len, delta[1] / seg_len, delta[2] / seg_len];
        let mut voxel = [
            from[0].floor() as i32,
            from[1].floor() as i32,
            from[2].floor() as i32,
        ];
        let step = [
            if dir[0] >= 0.0 { 1 } else { -1 },
            if dir[1] >= 0.0 { 1 } else { -1 },
            if dir[2] >= 0.0 { 1 } else { -1 },
        ];
        let inv = [
            if dir[0].abs() < 1e-9 {
                f32::INFINITY
            } else {
                1.0 / dir[0].abs()
            },
            if dir[1].abs() < 1e-9 {
                f32::INFINITY
            } else {
                1.0 / dir[1].abs()
            },
            if dir[2].abs() < 1e-9 {
                f32::INFINITY
            } else {
                1.0 / dir[2].abs()
            },
        ];
        let mut t_max = [f32::INFINITY; 3];
        for a in 0..3 {
            if inv[a].is_finite() {
                let boundary = if step[a] > 0 {
                    voxel[a] as f32 + 1.0 - from[a]
                } else {
                    from[a] - voxel[a] as f32
                };
                t_max[a] = boundary * inv[a];
            }
        }
        let mut loss = 0.0_f32;
        // March until we reach the destination cell; each *entered* cell adds
        // its per-block transmission loss once.
        loop {
            let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
                0
            } else if t_max[1] <= t_max[2] {
                1
            } else {
                2
            };
            let t = t_max[axis];
            if t >= seg_len || t > MAX_RAY_DIST_M {
                return loss;
            }
            voxel[axis] += step[axis];
            t_max[axis] += inv[axis];
            loss += self
                .world
                .acoustic_material(voxel[0], voxel[1], voxel[2])
                .transmission_loss_db;
            if loss > 240.0 {
                return loss;
            }
            // Macro-DDA through uniform-air sections: every skipped cell is
            // air (0 dB), so the accumulated loss is bit-identical.
            if let Some(t_star) = self.section_warp(step, inv, &mut voxel, &mut t_max) {
                if t_star >= seg_len || t_star > MAX_RAY_DIST_M {
                    return loss;
                }
            }
        }
    }

    /// Direct-path occlusion between two points, with optional single-bend
    /// diffraction probes (Ultra): orthogonal offsets around the segment
    /// midpoint stand in for doorway/portal openings, and a bent path that
    /// clears the wall beats the straight one by a bending penalty.
    ///
    /// Candidates are explored distance-ordered — lowest geometric lower
    /// bound (bend tax + detour penalty, before any traversal loss) first —
    /// and a candidate whose bound already meets the best loss found is
    /// pruned before it costs a single ray. Once the sorted order reaches a
    /// bound `>= effective`, every remaining candidate is pruned at once.
    /// The traversal-loss arithmetic and early-out tests are unchanged, so
    /// the result is exactly the exhaustive search's minimum.
    pub fn occlusion_between(&self, from: [f32; 3], to: [f32; 3]) -> OcclusionInfo {
        if !self.preset.enabled() {
            return OcclusionInfo::clear();
        }
        let direct = self.segment_loss_db(from, to);
        let mut effective = direct;
        let mut diffracted = false;
        if self.preset.diffraction_enabled() && direct > 2.0 {
            let seg = sub3(to, from);
            let seg_len = norm3(seg);
            const FRACTIONS: [f32; 5] = [0.35, 0.5, 0.65, 0.8, 0.95];
            const OFFSETS: [f32; 5] = [1.0, 2.0, 3.0, 4.5, 6.0];
            let base_at = |f: f32| {
                [
                    from[0] + seg[0] * f,
                    from[1] + seg[1] * f,
                    from[2] + seg[2] * f,
                ]
            };
            let mut cands = Vec::with_capacity(450);
            for &f in &FRACTIONS {
                for &mag in &OFFSETS {
                    for axis in 0..3 {
                        for &sign in &[-1.0_f32, 1.0] {
                            let mut p1 = base_at(f);
                            p1[axis] += sign * mag;
                            let bent_len =
                                norm3(sub3(p1, from)) + norm3(sub3(to, p1));
                            let detour = (bent_len - seg_len).max(0.0);
                            cands.push(DiffractionCand {
                                bend: Bend::One { p1 },
                                lower_bound: 4.0 + 0.5 * detour,
                                detour,
                            });
                        }
                    }
                }
            }
            for (fi, &f1) in FRACTIONS.iter().enumerate() {
                for &f2 in FRACTIONS.iter().skip(fi + 1) {
                    for &mag in &OFFSETS {
                        for axis in 0..3 {
                            for &sign in &[-1.0_f32, 1.0] {
                                let mut p1 = base_at(f1);
                                let mut p2 = base_at(f2);
                                p1[axis] += sign * mag;
                                p2[axis] += sign * mag;
                                let bent_len = norm3(sub3(p1, from))
                                    + norm3(sub3(p2, p1))
                                    + norm3(sub3(to, p2));
                                let detour = (bent_len - seg_len).max(0.0);
                                cands.push(DiffractionCand {
                                    bend: Bend::Two { p1, p2 },
                                    lower_bound: 8.0 + 0.5 * detour,
                                    detour,
                                });
                            }
                        }
                    }
                }
            }
            // Stable sort keeps build order inside equal bounds — fully
            // deterministic.
            cands.sort_by(|a, b| {
                a.lower_bound
                    .partial_cmp(&b.lower_bound)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for cand in &cands {
                // Sorted ascending: the first bound that already meets the
                // incumbent prunes every candidate after it in one test.
                if cand.lower_bound >= effective {
                    break;
                }
                match cand.bend {
                    Bend::One { p1 } => {
                        self.count_diffraction_ray();
                        let l1 = self.segment_loss_db(from, p1);
                        if l1 >= effective {
                            continue;
                        }
                        self.count_diffraction_ray();
                        let l2 = self.segment_loss_db(p1, to);
                        if l1 + l2 >= effective {
                            continue;
                        }
                        // Diffracted energy pays a ~4 dB bend tax plus mild
                        // extra-path absorption, so a clear doorway still
                        // beats a straight wall and never amplifies.
                        let candidate = l1 + l2 + 4.0 + 0.5 * cand.detour;
                        if candidate < effective {
                            effective = candidate;
                            diffracted = true;
                        }
                    }
                    Bend::Two { p1, p2 } => {
                        self.count_diffraction_ray();
                        let l1 = self.segment_loss_db(from, p1);
                        if l1 >= effective {
                            continue;
                        }
                        self.count_diffraction_ray();
                        let l2 = self.segment_loss_db(p1, p2);
                        if l1 + l2 >= effective {
                            continue;
                        }
                        self.count_diffraction_ray();
                        let l3 = self.segment_loss_db(p2, to);
                        if l1 + l2 + l3 >= effective {
                            continue;
                        }
                        let candidate = l1 + l2 + l3 + 8.0 + 0.5 * cand.detour;
                        if candidate < effective {
                            effective = candidate;
                            diffracted = true;
                        }
                    }
                }
            }
        }
        let cutoff = (OCCLUSION_MAX_CUTOFF_HZ * 2.0_f32.powf(-effective / 15.0))
            .clamp(OCCLUSION_MIN_CUTOFF_HZ, OCCLUSION_MAX_CUTOFF_HZ);
        OcclusionInfo {
            transmission_loss_db: effective,
            direct_loss_db: direct,
            cutoff_hz: cutoff,
            diffracted,
            blocked: effective >= 60.0,
        }
    }

    /// Reference exhaustive diffraction search — the pre-branch-and-bound
    /// enumeration of every one- and two-bend probe in fixed order. Kept as
    /// the executable spec the pruned search is verified against.
    #[doc(hidden)]
    pub fn occlusion_between_exhaustive(
        &self,
        from: [f32; 3],
        to: [f32; 3],
    ) -> OcclusionInfo {
        if !self.preset.enabled() {
            return OcclusionInfo::clear();
        }
        let direct = self.segment_loss_db(from, to);
        let mut effective = direct;
        let mut diffracted = false;
        if self.preset.diffraction_enabled() && direct > 2.0 {
            let seg = sub3(to, from);
            let seg_len = norm3(seg);
            const FRACTIONS: [f32; 5] = [0.35, 0.5, 0.65, 0.8, 0.95];
            const OFFSETS: [f32; 5] = [1.0, 2.0, 3.0, 4.5, 6.0];
            let base_at = |f: f32| {
                [
                    from[0] + seg[0] * f,
                    from[1] + seg[1] * f,
                    from[2] + seg[2] * f,
                ]
            };
            for &f in &FRACTIONS {
                for &mag in &OFFSETS {
                    for axis in 0..3 {
                        for &sign in &[-1.0_f32, 1.0] {
                            let mut probe = base_at(f);
                            probe[axis] += sign * mag;
                            self.count_diffraction_ray();
                            let l1 = self.segment_loss_db(from, probe);
                            if l1 >= effective {
                                continue;
                            }
                            self.count_diffraction_ray();
                            let l2 = self.segment_loss_db(probe, to);
                            if l1 + l2 >= effective {
                                continue;
                            }
                            let bent_len = norm3(sub3(probe, from)) + norm3(sub3(to, probe));
                            let candidate = l1 + l2 + 4.0 + 0.5 * (bent_len - seg_len).max(0.0);
                            if candidate < effective {
                                effective = candidate;
                                diffracted = true;
                            }
                        }
                    }
                }
            }
            for (fi, &f1) in FRACTIONS.iter().enumerate() {
                for &f2 in FRACTIONS.iter().skip(fi + 1) {
                    for &mag in &OFFSETS {
                        for axis in 0..3 {
                            for &sign in &[-1.0_f32, 1.0] {
                                let mut p1 = base_at(f1);
                                let mut p2 = base_at(f2);
                                p1[axis] += sign * mag;
                                p2[axis] += sign * mag;
                                self.count_diffraction_ray();
                                let l1 = self.segment_loss_db(from, p1);
                                if l1 >= effective {
                                    continue;
                                }
                                self.count_diffraction_ray();
                                let l2 = self.segment_loss_db(p1, p2);
                                if l1 + l2 >= effective {
                                    continue;
                                }
                                self.count_diffraction_ray();
                                let l3 = self.segment_loss_db(p2, to);
                                if l1 + l2 + l3 >= effective {
                                    continue;
                                }
                                let bent_len = norm3(sub3(p1, from))
                                    + norm3(sub3(p2, p1))
                                    + norm3(sub3(to, p2));
                                let candidate =
                                    l1 + l2 + l3 + 8.0 + 0.5 * (bent_len - seg_len).max(0.0);
                                if candidate < effective {
                                    effective = candidate;
                                    diffracted = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        let cutoff = (OCCLUSION_MAX_CUTOFF_HZ * 2.0_f32.powf(-effective / 15.0))
            .clamp(OCCLUSION_MIN_CUTOFF_HZ, OCCLUSION_MAX_CUTOFF_HZ);
        OcclusionInfo {
            transmission_loss_db: effective,
            direct_loss_db: direct,
            cutoff_hz: cutoff,
            diffracted,
            blocked: effective >= 60.0,
        }
    }

    /// Trace the full acoustic scene: Fibonacci ray bundle for
    /// reflections/RT60/openness plus the listener↔source direct occlusion.
    /// `listener` is only used to compute early-tap directions of arrival.
    pub fn compute_scene(
        &self,
        source: [f32; 3],
        listener: [f32; 3],
        sample_rate: u32,
    ) -> AcousticScene {
        let mut scene = AcousticScene::default();
        if !self.preset.enabled() {
            return scene;
        }
        scene.occlusion = self.occlusion_between(listener, source);
        scene.rays_traced = self.preset.ray_count() as u32;

        // A source buried in solid geometry has no acoustic scene at all.
        let sv = [
            source[0].floor() as i32,
            source[1].floor() as i32,
            source[2].floor() as i32,
        ];
        if self.world.acoustic_material(sv[0], sv[1], sv[2]).is_solid() {
            scene.outdoor_openness = 0.0;
            return scene;
        }

        let dirs = fibonacci_dirs(self.preset.ray_count());
        let max_bounces = self.preset.max_bounces();
        let er_window_m = EARLY_WINDOW_S * SPEED_OF_SOUND_M_S;
        let direct_m = norm3(sub3(listener, source));

        let mut escaped = 0usize;
        let mut hits_total = 0usize;
        let mut path_sum = 0.0_f32;
        let mut alpha_sum = [0.0_f32; 3];
        let mut taps: Vec<EarlyReflectionTap> = Vec::with_capacity(MAX_EARLY_TAPS);

        for (ray_i, &dir0) in dirs.iter().enumerate() {
            let mut origin = source;
            let mut dir = dir0;
            let mut energy = [1.0_f32, 1.0, 1.0];
            let mut path = 0.0_f32;
            let mut did_escape = false;
            let mut done = false;

            for bounce in 0..=max_bounces {
                let cast_origin = [
                    origin[0] + dir[0] * 1e-4,
                    origin[1] + dir[1] * 1e-4,
                    origin[2] + dir[2] * 1e-4,
                ];
                match self.cast(cast_origin, dir) {
                    CastResult::Escaped => {
                        did_escape = true;
                        break;
                    }
                    CastResult::Hit {
                        dist,
                        point,
                        normal,
                        material,
                        liquid_high_loss,
                    } => {
                        path += dist;
                        hits_total += 1;
                        path_sum += dist;
                        alpha_sum[0] += material.absorption_low;
                        alpha_sum[1] += material.absorption_mid;
                        alpha_sum[2] += material.absorption_high;
                        for (b, e) in energy.iter_mut().enumerate() {
                            let alpha = match b {
                                0 => material.absorption_low,
                                1 => material.absorption_mid,
                                _ => material.absorption_high,
                            };
                            *e *= 1.0 - alpha;
                        }
                        energy[2] *= liquid_high_loss;

                        if path - direct_m >= 0.0
                            && path <= er_window_m + direct_m
                            && energy[1] > 0.01
                        {
                            // Spherical spreading: reflection energy falls off
                            // as 1/d^2, so a far wall bounce never drowns the
                            // direct path (amplitude ∝ 1/d after the sqrt in
                            // the tap reader).
                            let spread = if path > ROLLOFF_REF_M {
                                let s = ROLLOFF_REF_M / path;
                                s * s
                            } else {
                                1.0
                            };
                            if energy[1] * spread > 0.01 {
                                let doa = normalize3(sub3(point, listener));
                                taps.push(EarlyReflectionTap {
                                    delay_samples: ((path - direct_m).max(0.0) / SPEED_OF_SOUND_M_S
                                        * sample_rate as f32)
                                        as u32,
                                    direction: doa,
                                    energy_low: energy[0] * spread,
                                    energy_mid: energy[1] * spread,
                                    energy_high: energy[2] * spread,
                                });
                            }
                        }

                        if energy[1] < 0.005 {
                            done = true;
                            break;
                        }
                        if bounce == max_bounces {
                            break;
                        }
                        // Specular reflection blended toward a deterministic
                        // diffuse lobe by the material's scattering.
                        let d_dot_n = dot3(dir, normal);
                        let spec = [
                            dir[0] - 2.0 * d_dot_n * normal[0],
                            dir[1] - 2.0 * d_dot_n * normal[1],
                            dir[2] - 2.0 * d_dot_n * normal[2],
                        ];
                        let seed = (ray_i as u32) << 8 | bounce;
                        let diff = scatter_dir(normal, seed);
                        let s = material.scattering.clamp(0.0, 0.9);
                        let mut nd = normalize3([
                            spec[0] * (1.0 - s) + diff[0] * s,
                            spec[1] * (1.0 - s) + diff[1] * s,
                            spec[2] * (1.0 - s) + diff[2] * s,
                        ]);
                        if dot3(nd, normal) <= 0.02 {
                            nd = normal;
                        }
                        dir = nd;
                        origin = [
                            point[0] + normal[0] * 1e-4,
                            point[1] + normal[1] * 1e-4,
                            point[2] + normal[2] * 1e-4,
                        ];
                    }
                }
            }
            // Bounce budget exhausted mid-flight: one last cast decides
            // whether the surviving energy escapes or dies in the walls.
            if !did_escape && !done {
                let cast_origin = [
                    origin[0] + dir[0] * 1e-4,
                    origin[1] + dir[1] * 1e-4,
                    origin[2] + dir[2] * 1e-4,
                ];
                if matches!(self.cast(cast_origin, dir), CastResult::Escaped) {
                    did_escape = true;
                }
            }
            if did_escape {
                escaped += 1;
            }
        }

        scene.outdoor_openness = escaped as f32 / dirs.len().max(1) as f32;

        // Eyring RT60 per band: rt60 = -0.161*V / (S*ln(1-a)), and with the
        // diffuse-field mean free path L = 4V/S this collapses to
        // -0.04025*L / ln(1-a_band). Confinement (1 - openness) scales the
        // result so an open field never rings like a room.
        if hits_total > 0 {
            let mean_free_path = path_sum / hits_total as f32;
            let confinement = 1.0 - scene.outdoor_openness;
            let rt60 = |band: usize| {
                let a = (alpha_sum[band] / hits_total as f32).clamp(0.001, 0.999);
                (-0.040_25 * mean_free_path / (1.0 - a).ln() * confinement).clamp(0.0, 12.0)
            };
            scene.rt60_low_s = rt60(0);
            scene.rt60_mid_s = rt60(1);
            scene.rt60_high_s = rt60(2);
            let mean_alpha_mid = alpha_sum[1] / hits_total as f32;
            let liveness = 0.35 + 0.65 * (scene.rt60_mid_s / 2.5).clamp(0.0, 1.0);
            scene.wet_gain =
                (confinement * (1.0 - mean_alpha_mid.clamp(0.0, 1.0)) * liveness).clamp(0.0, 0.85);
        }

        // Keep the strongest MAX_EARLY_TAPS taps by mid-band energy.
        taps.sort_by(|a, b| {
            b.energy_mid
                .partial_cmp(&a.energy_mid)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        taps.truncate(MAX_EARLY_TAPS);
        taps.sort_by_key(|t| t.delay_samples);
        scene.early_reflections = taps;
        scene
    }
}

// ---------------------------------------------------------------------------
// Real-time DSP
// ---------------------------------------------------------------------------

/// Listener pose in world space. `yaw_deg = 0` faces +Z and positive yaw
/// rotates toward -X (Minecraft-style); `pitch_deg > 0` looks up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ListenerPose {
    /// Ear position in block coordinates (typically eye height).
    pub position: [f32; 3],
    /// Horizontal facing, degrees; 0 = +Z, positive turns right (-X).
    pub yaw_deg: f32,
    /// Vertical facing, degrees; positive looks up.
    pub pitch_deg: f32,
    /// Velocity in blocks/second, for Doppler.
    pub velocity: [f32; 3],
    /// `true` when the listener's ears are submerged (water/lava) — applies
    /// the ~600 Hz underwater muffle to the whole mix.
    pub submerged: bool,
}

/// A positioned sound emitter in world space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SoundSource {
    /// Emission point in block coordinates.
    pub position: [f32; 3],
    /// Velocity in blocks/second, for Doppler.
    pub velocity: [f32; 3],
}

/// One-pole low-pass with a per-frame slewed cutoff so parameter changes do
/// not zipper. Cutoffs at/above ~0.48·fs are treated as a true bypass.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct OnePoleLp {
    y: f32,
    a: f32,
    cutoff: f32,
    target: f32,
}

impl OnePoleLp {
    /// Filter parked at `cutoff_hz` with zero state and no slew applied yet.
    pub fn new(cutoff_hz: f32) -> Self {
        Self {
            y: 0.0,
            a: 0.0,
            cutoff: cutoff_hz,
            target: cutoff_hz,
        }
    }

    /// Slew destination for [`Self::settle`], clamped to the audible band.
    pub fn set_target(&mut self, cutoff_hz: f32) {
        self.target = cutoff_hz.clamp(20.0, OCCLUSION_MAX_CUTOFF_HZ);
    }

    /// Advance the cutoff toward the target; `rate` ~0..1 per frame.
    pub fn settle(&mut self, rate: f32, sample_rate: f32) {
        self.cutoff += (self.target - self.cutoff) * rate.clamp(0.0, 1.0);
        self.a = if self.cutoff >= 0.48 * sample_rate {
            1.0
        } else {
            1.0 - (-2.0 * PI * self.cutoff / sample_rate).exp()
        };
    }

    /// One sample through the filter. When the slewed cutoff has reached the
    /// bypass region (`a >= 1.0`, i.e. cutoff ≥ 0.48·fs) the filter is a
    /// bit-exact identity: `y + 1·(x − y)` is replaced by `x` directly, which
    /// is both faster and free of the residual rounding the general form
    /// leaves behind. State stays current so a later cutoff drop resumes the
    /// recursive form seamlessly.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        if self.a >= 1.0 {
            self.y = guard_state(x);
            return self.y;
        }
        self.y += self.a * (x - self.y);
        self.y = guard_state(self.y);
        self.y
    }
}

/// Hardcoded Sylvester Hadamard butterfly for power-of-two sizes (4 or 8).
/// Orthogonal mixing is what keeps the FDN from ringing metallically.
fn hadamard_apply(v: &mut [f32]) {
    let n = v.len();
    let mut h = 1;
    while h < n {
        let mut i = 0;
        while i < n {
            for j in i..i + h {
                let (x, y) = (v[j], v[j + h]);
                v[j] = x + y;
                v[j + h] = x - y;
            }
            i += 2 * h;
        }
        h *= 2;
    }
    let scale = 1.0 / (n as f32).sqrt();
    for x in v.iter_mut() {
        *x *= scale;
    }
}

fn is_prime(n: usize) -> bool {
    if n < 2 {
        return false;
    }
    if n.is_multiple_of(2) {
        return n == 2;
    }
    let mut d = 3usize;
    while d.saturating_mul(d) <= n {
        if n.is_multiple_of(d) {
            return false;
        }
        d += 2;
    }
    true
}

/// Smallest prime >= `n` — mutually prime delay-line lengths keep the FDN
/// echo density aperiodic.
fn next_prime(mut n: usize) -> usize {
    while !is_prime(n) {
        n += 1;
    }
    n
}

/// Maximum FDN lane count — every per-channel array is a fixed `[f32; 8]`
/// lane so the whole step loop is contiguous and auto-vectorizes.
const MAX_FDN_CHANNELS: usize = 8;

/// Feedback delay network late reverb: `n` mutually prime delay lines packed
/// into one flat buffer, a three-band damping split per channel driven by the
/// raytraced RT60s, and Hadamard feedback mixing. All per-channel state lives
/// in fixed lanes indexed `0..n` — no per-step pointer chasing.
struct Fdn {
    n: usize,
    /// All delay lines concatenated; `line_off[i]..line_off[i]+line_len[i]`
    /// is channel `i`'s ring.
    buf: Vec<f32>,
    line_off: [usize; MAX_FDN_CHANNELS],
    line_len: [usize; MAX_FDN_CHANNELS],
    pos: [usize; MAX_FDN_CHANNELS],
    g_lo: [f32; MAX_FDN_CHANNELS],
    g_mid: [f32; MAX_FDN_CHANNELS],
    g_hi: [f32; MAX_FDN_CHANNELS],
    split_lo: [OnePoleLp; MAX_FDN_CHANNELS],
    split_hi: [OnePoleLp; MAX_FDN_CHANNELS],
    in_w: [f32; MAX_FDN_CHANNELS],
    out_l: [f32; MAX_FDN_CHANNELS],
    out_r: [f32; MAX_FDN_CHANNELS],
    outs: [f32; MAX_FDN_CHANNELS],
    fb: [f32; MAX_FDN_CHANNELS],
}

impl Fdn {
    fn new(channels: usize, sample_rate: f32) -> Self {
        let n = channels.min(MAX_FDN_CHANNELS);
        let mut fdn = Self {
            n,
            buf: Vec::new(),
            line_off: [0; MAX_FDN_CHANNELS],
            line_len: [0; MAX_FDN_CHANNELS],
            pos: [0; MAX_FDN_CHANNELS],
            g_lo: [0.0; MAX_FDN_CHANNELS],
            g_mid: [0.0; MAX_FDN_CHANNELS],
            g_hi: [0.0; MAX_FDN_CHANNELS],
            split_lo: [OnePoleLp::new(BAND_LOW_HZ); MAX_FDN_CHANNELS],
            split_hi: [OnePoleLp::new(BAND_HIGH_HZ); MAX_FDN_CHANNELS],
            in_w: [0.0; MAX_FDN_CHANNELS],
            out_l: [0.0; MAX_FDN_CHANNELS],
            out_r: [0.0; MAX_FDN_CHANNELS],
            outs: [0.0; MAX_FDN_CHANNELS],
            fb: [0.0; MAX_FDN_CHANNELS],
        };
        for c in &mut fdn.split_lo {
            c.settle(1.0, sample_rate);
        }
        for c in &mut fdn.split_hi {
            c.settle(1.0, sample_rate);
        }
        // Spread delay times 30..90 ms, then snap each up to a prime length.
        for i in 0..n {
            let ms = if n > 1 {
                30.0 + 60.0 * i as f32 / (n - 1) as f32
            } else {
                45.0
            };
            let len = next_prime((ms * sample_rate / 1000.0) as usize);
            fdn.line_off[i] = fdn.buf.len();
            fdn.line_len[i] = len;
            fdn.buf.resize(fdn.buf.len() + len, 0.0);
            // Alternate-sign input weights and equal-power pans decorrelate
            // the channels going in and recombine smoothly coming out.
            fdn.in_w[i] = if i % 2 == 0 { 1.0 } else { -1.0 };
            let pan = if n > 1 {
                -1.0 + 2.0 * i as f32 / (n - 1) as f32
            } else {
                0.0
            };
            fdn.out_l[i] = ((1.0 - pan) * 0.5).sqrt();
            fdn.out_r[i] = ((1.0 + pan) * 0.5).sqrt();
        }
        fdn
    }

    /// Convert Eyring RT60s into per-line, per-band feedback gains:
    /// `g = 10^(-3 d / (rt60 fs))` is the standard FDN decay mapping.
    fn set_rt60(&mut self, rt60: [f32; 3], sample_rate: f32) {
        let [rl, rm, rh] = rt60.map(|t| t.clamp(0.03, 10.0));
        for i in 0..self.n {
            let d = self.line_len[i] as f32;
            self.g_lo[i] = 10.0_f32.powf(-3.0 * d / (rl * sample_rate));
            self.g_mid[i] = 10.0_f32.powf(-3.0 * d / (rm * sample_rate));
            self.g_hi[i] = 10.0_f32.powf(-3.0 * d / (rh * sample_rate));
        }
    }

    /// One mono sample in → one decorrelated stereo reverb sample out.
    fn step(&mut self, input: f32) -> (f32, f32) {
        let n = self.n;
        for i in 0..n {
            self.outs[i] = self.buf[self.line_off[i] + self.pos[i]];
        }
        for i in 0..n {
            let x = self.outs[i];
            let lo = self.split_lo[i].process(x);
            let rest = x - lo;
            let mid = self.split_hi[i].process(rest);
            let hi = rest - mid;
            self.fb[i] = guard_state(self.g_lo[i] * lo + self.g_mid[i] * mid + self.g_hi[i] * hi);
        }
        hadamard_apply(&mut self.fb[..n]);
        let mut wet_l = 0.0;
        let mut wet_r = 0.0;
        for i in 0..n {
            let write = guard_state(input * self.in_w[i] + self.fb[i]);
            self.buf[self.line_off[i] + self.pos[i]] = write;
            self.pos[i] += 1;
            if self.pos[i] == self.line_len[i] {
                self.pos[i] = 0;
            }
            wet_l += self.outs[i] * self.out_l[i];
            wet_r += self.outs[i] * self.out_r[i];
        }
        let norm = 0.5 / (n as f32).sqrt();
        (wet_l * norm, wet_r * norm)
    }
}

/// One listener-shared 8-channel Hadamard FDN. Instead of paying one FDN per
/// concurrently sounding voice, the mixer accumulates every voice's
/// `s * wet_send` into a single reverb input and steps the shared network
/// once. Every stage of the FDN — delay lines, one-pole splits, feedback
/// gains, Hadamard mixing — is linear and time-invariant, so processing the
/// summed send yields the same wet field as summing N independent FDN
/// outputs: exact LTI superposition with a fraction of the state and CPU.
pub struct SharedListenerFdnBus {
    fdn: Fdn,
    in_acc: f32,
    sample_rate: f32,
}

impl SharedListenerFdnBus {
    /// Shared 8-channel bus at `sample_rate`, initially dry (zero RT60).
    pub fn new(sample_rate: u32) -> Self {
        let fs = sample_rate as f32;
        let mut fdn = Fdn::new(8, fs);
        fdn.set_rt60([0.0, 0.0, 0.0], fs);
        Self {
            fdn,
            in_acc: 0.0,
            sample_rate: fs,
        }
    }

    /// Update decay times from the listener's current scene RT60s
    /// (`[low, mid, high]` in seconds).
    pub fn set_rt60(&mut self, rt60_s: [f32; 3]) {
        self.fdn.set_rt60(rt60_s, self.sample_rate);
    }

    /// Accumulate one voice's reverb send for the current sample. Cheap
    /// enough to call per voice per sample; [`Self::step`] runs once after
    /// all voices have contributed.
    #[inline]
    pub fn send(&mut self, sample: f32, wet_send: f32) {
        self.in_acc += sample * wet_send;
    }

    /// Advance the shared FDN one sample on the accumulated sends and return
    /// the `(left, right)` wet contribution. The accumulator resets to zero
    /// for the next sample.
    #[inline]
    pub fn step(&mut self) -> (f32, f32) {
        let input = std::mem::take(&mut self.in_acc);
        self.fdn.step(input)
    }

    /// Advance the shared FDN one sample on a pre-summed input (e.g. the
    /// per-sample accumulator the mixer fills from every voice's wet send)
    /// without touching [`Self::send`]/`in_acc`. The tail keeps decaying on
    /// `step_input(0.0)` after the last voice finishes.
    #[inline]
    pub fn step_input(&mut self, input: f32) -> (f32, f32) {
        self.fdn.step(input)
    }
}

/// Direction buckets for automatic subtitle placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubtitleDirection {
    /// Source sits left of the listener's facing.
    Left,
    /// Source sits roughly straight ahead.
    Center,
    /// Source sits right of the listener's facing.
    Right,
    /// Source sits behind the listener.
    Behind,
}

/// What the client subtitle line needs: which side indicator to draw and how
/// loud the sound is perceived (after distance, occlusion, and reverb).
#[derive(Clone, Debug, PartialEq)]
pub struct SpatialSubtitleCue {
    /// Direction bucket for the `<`/`>` glyph.
    pub direction: SubtitleDirection,
    /// Perceived loudness `0..=1` of the direct path.
    pub loudness: f32,
    /// Listener↔source distance in blocks.
    pub distance_m: f32,
    /// Sound-event name for the HUD row (e.g. `entity.player.levelup`). The
    /// engine fills this empty — it knows geometry, not event names — and the
    /// mixer stamps the admitted event's label onto each queued cue.
    pub label: String,
}

/// Early-reflection tap resolved once per audio block: the history-ring read
/// offset plus the folded per-ear gains (band-weighted amplitude ×
/// equal-power pan), so the 48 kHz loop is two FMA ops per tap.
#[derive(Clone, Copy, Debug)]
struct PrecomputedTap {
    delay_off: f32,
    gain_l: f32,
    gain_r: f32,
}

impl PrecomputedTap {
    const ZERO: Self = Self {
        delay_off: 0.0,
        gain_l: 0.0,
        gain_r: 0.0,
    };
}

/// Resolve one [`EarlyReflectionTap`] into block-head delay + per-ear gains.
/// Mirrors the math that used to run per sample — identical inputs give
/// identical gains, just hoisted out of the loop.
#[inline]
fn precompute_tap(tap: &EarlyReflectionTap, yaw_deg: f32) -> PrecomputedTap {
    let amp = (0.5 * tap.energy_mid + 0.3 * tap.energy_low + 0.2 * tap.energy_high).sqrt();
    let rel = rotate_yaw(tap.direction, yaw_deg);
    let pan = rel[0].atan2(rel[2]).clamp(-PI * 0.5, PI * 0.5) / (PI * 0.5);
    PrecomputedTap {
        delay_off: tap.delay_samples as f32,
        gain_l: amp * ((1.0 - pan) * 0.5).sqrt(),
        gain_r: amp * ((1.0 + pan) * 0.5).sqrt(),
    }
}

/// Test-facing view of the block-head early-reflection precompute: returns
/// `[delay_off, gain_l, gain_r]` per tap, in scene order.
#[doc(hidden)]
pub fn precomputed_er_gains(taps: &[EarlyReflectionTap], yaw_deg: f32) -> Vec<[f32; 3]> {
    taps.iter()
        .map(|t| {
            let p = precompute_tap(t, yaw_deg);
            [p.delay_off, p.gain_l, p.gain_r]
        })
        .collect()
}

/// The real-time half of the engine. Owns all DSP state for one spatialized
/// sound: shared history buffer, per-ear filter chains, early taps, and the
/// FDN late field. Create one per concurrently sounding source (or per voice
/// channel), call [`Self::analyze`] when geometry or the world changes, then
/// drive [`Self::process`]/[`Self::process_voice_frame`] per audio block.
pub struct SpatialAudioEngine {
    sample_rate: f32,
    preset: SpatialAudioPreset,
    scene: AcousticScene,
    listener: ListenerPose,
    source: SoundSource,
    underwater: bool,
    hist: Vec<f32>,
    hist_mask: usize,
    wpos: usize,
    /// Fractional read offset behind the write head: propagation delay plus
    /// accumulated Doppler drift.
    read_off: f32,
    doppler_ratio: f32,
    hs_l: OnePoleLp,
    hs_r: OnePoleLp,
    oc_l1: OnePoleLp,
    oc_l2: OnePoleLp,
    oc_r1: OnePoleLp,
    oc_r2: OnePoleLp,
    air_l: OnePoleLp,
    air_r: OnePoleLp,
    uw_l1: OnePoleLp,
    uw_l2: OnePoleLp,
    uw_r1: OnePoleLp,
    uw_r2: OnePoleLp,
    fdn: Option<Fdn>,
    // Smoothed per-frame parameters.
    itd_l: f32,
    itd_r: f32,
    gain_l: f32,
    gain_r: f32,
    direct_gain: f32,
    wet_send: f32,
    er_gain: f32,
    az_rel: f32,
    distance_m: f32,
    base_delay: f32,
    /// Vanilla audible range (`attenuation_distance * max(volume, 1)` or the
    /// server `range` override). `INFINITY` disables range fading.
    max_range_m: f32,
}

impl SpatialAudioEngine {
    /// Engine at `sample_rate` (e.g. 48 000 for `VCR1` PCM) and `preset`
    /// quality. DSP state starts silent; the first [`Self::analyze`] supplies
    /// real scene parameters, and before it the engine plays dry open air.
    pub fn new(sample_rate: u32, preset: SpatialAudioPreset) -> Self {
        let fs = sample_rate as f32;
        let hist_len = (sample_rate as usize / 3).next_power_of_two().max(1024);
        let fdn = if preset.enabled() {
            let mut f = Fdn::new(preset.fdn_channels(), fs);
            f.set_rt60([0.0, 0.0, 0.0], fs);
            Some(f)
        } else {
            None
        };
        Self {
            sample_rate: fs,
            preset,
            scene: AcousticScene::default(),
            listener: ListenerPose::default(),
            source: SoundSource::default(),
            underwater: false,
            hist: vec![0.0; hist_len],
            hist_mask: hist_len - 1,
            wpos: 0,
            read_off: 8.0,
            doppler_ratio: 1.0,
            hs_l: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            hs_r: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            oc_l1: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            oc_l2: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            oc_r1: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            oc_r2: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            air_l: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            air_r: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            uw_l1: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            uw_l2: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            uw_r1: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            uw_r2: OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ),
            fdn,
            itd_l: 0.0,
            itd_r: 0.0,
            gain_l: 1.0,
            gain_r: 1.0,
            direct_gain: 1.0,
            wet_send: 0.0,
            er_gain: 1.0,
            az_rel: 0.0,
            distance_m: 0.0,
            base_delay: 8.0,
            max_range_m: f32::INFINITY,
        }
    }

    /// Active quality tier.
    pub fn preset(&self) -> SpatialAudioPreset {
        self.preset
    }

    /// Engine sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate as u32
    }

    /// Scene captured by the last [`Self::analyze`] (or open-air default).
    pub fn scene(&self) -> &AcousticScene {
        &self.scene
    }

    /// Re-trace the world for this (source, listener) pairing. Amortize this —
    /// it is the expensive half — by calling on sound start, when either end
    /// crosses a cell boundary, or a few times per second for moving sources.
    /// Also auto-detects a submerged listener from the head cell material.
    pub fn analyze(
        &mut self,
        world: &dyn VoxelAcousticWorld,
        source: &SoundSource,
        listener: &ListenerPose,
    ) -> &AcousticScene {
        self.source = *source;
        self.listener = *listener;
        let head = listener.position;
        self.underwater = listener.submerged
            || matches!(
                world
                    .acoustic_material(
                        head[0].floor() as i32,
                        head[1].floor() as i32,
                        head[2].floor() as i32,
                    )
                    .class,
                MaterialClass::Liquid
            );
        let tracer = AcousticTracer::new(world, self.preset);
        self.scene =
            tracer.compute_scene(source.position, listener.position, self.sample_rate as u32);
        if let Some(fdn) = &mut self.fdn {
            fdn.set_rt60(
                [
                    self.scene.rt60_low_s,
                    self.scene.rt60_mid_s,
                    self.scene.rt60_high_s,
                ],
                self.sample_rate,
            );
        }
        self.update_geometry();
        &self.scene
    }

    /// Swap in a scene traced elsewhere (e.g. cached per cell) without
    /// re-raytracing.
    pub fn set_scene(&mut self, scene: AcousticScene) {
        if let Some(fdn) = &mut self.fdn {
            fdn.set_rt60(
                [scene.rt60_low_s, scene.rt60_mid_s, scene.rt60_high_s],
                self.sample_rate,
            );
        }
        self.scene = scene;
    }

    /// Cheap pose update — head turns never need a re-trace.
    pub fn set_listener(&mut self, listener: ListenerPose) {
        self.underwater = listener.submerged;
        self.listener = listener;
        self.update_geometry();
    }

    /// Cheap source update — moving emitters only need geometry, not rays.
    pub fn set_source(&mut self, source: SoundSource) {
        self.source = source;
        self.update_geometry();
    }

    /// Set the Vanilla audible range in blocks: `attenuation_distance *
    /// max(volume, 1.0)` computed by the caller (or the server's explicit
    /// `range` override). Beyond it the direct path and reverb send fade to
    /// silence; inside it the physical rolloff model runs unchanged.
    /// Non-finite or non-positive input disables range fading.
    pub fn set_max_range(&mut self, range_m: f32) {
        self.max_range_m = if range_m.is_finite() && range_m > 0.0 {
            range_m
        } else {
            f32::INFINITY
        };
        self.update_geometry();
    }

    /// Current audible range (`INFINITY` when unset).
    pub fn max_range(&self) -> f32 {
        self.max_range_m
    }

    /// Listener-space spherical geometry: azimuth (positive = right),
    /// elevation, and distance.
    fn spherical(&self) -> (f32, f32, f32) {
        let d = sub3(self.source.position, self.listener.position);
        let dist = norm3(d);
        if dist < 1e-6 {
            return (0.0, 0.0, 0.0);
        }
        let u = [d[0] / dist, d[1] / dist, d[2] / dist];
        let (yaw, pitch) = (
            self.listener.yaw_deg.to_radians(),
            self.listener.pitch_deg.to_radians(),
        );
        let (sy, cy) = yaw.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let fwd = [cp * sy, sp, cp * cy];
        let mut right = cross3(fwd, [0.0, 1.0, 0.0]);
        if norm3(right) < 1e-6 {
            right = [cy, 0.0, -sy];
        } else {
            right = normalize3(right);
        }
        let up = cross3(right, fwd);
        let rel_x = dot3(u, right);
        let rel_y = dot3(u, up);
        let rel_z = dot3(u, fwd);
        (rel_x.atan2(rel_z), rel_y.clamp(-1.0, 1.0).asin(), dist)
    }

    /// Recompute every per-frame target from the stored geometry and scene:
    /// Woodworth ITD, head-shadow ILD, pinna/behind shading, distance rolloff,
    /// Doppler ratio, occlusion filter targets, and send levels.
    fn update_geometry(&mut self) {
        let (az, el, dist) = self.spherical();
        self.az_rel = az;
        self.distance_m = dist;

        // Woodworth spherical-head ITD: r/c * (az + sin az) up to pi/2,
        // mirrored beyond it. Peak ≈ 0.68 ms at az = 90 deg.
        let abs_az = az.abs();
        let eff = abs_az.min(PI - abs_az);
        let itd_s = HEAD_RADIUS_M / SPEED_OF_SOUND_M_S * (eff + eff.sin());
        let itd = (itd_s * self.sample_rate).max(0.0);
        // az > 0 → source on the right → right ear near, left ear delayed.
        if az >= 0.0 {
            self.itd_l = itd;
            self.itd_r = 0.0;
        } else {
            self.itd_l = 0.0;
            self.itd_r = itd;
        }

        // Head shadow: far-ear LP sweeps 19 kHz → ~1.4 kHz across the shadow.
        let shadow = eff.sin().clamp(0.0, 1.0);
        let far_cutoff = 19_000.0 + (1_400.0 - 19_000.0) * shadow;
        let (far_l, far_r) = if az >= 0.0 {
            (true, false)
        } else {
            (false, true)
        };
        self.hs_l
            .set_target(if far_l { far_cutoff } else { 19_000.0 });
        self.hs_r
            .set_target(if far_r { far_cutoff } else { 19_000.0 });

        // ILD: far ear loses up to ~8 dB broadband, plus the LP above.
        let far_gain = 1.0 - 0.6 * shadow;
        let mut gain_l = if far_l { far_gain } else { 1.0 };
        let mut gain_r = if far_r { far_gain } else { 1.0 };
        // Sources behind the head read slightly darker on both ears (pinna).
        let behind = (-az.cos()).clamp(0.0, 1.0);
        let behind_atten = 1.0 - 0.18 * behind;
        // Subtle elevation shelf: overhead a touch brighter, below duller.
        let pinna = 1.0 + 0.10 * el.sin();
        gain_l *= behind_atten * pinna;
        gain_r *= behind_atten * pinna;
        self.gain_l = gain_l;
        self.gain_r = gain_r;

        // Distance rolloff (linear, 1 m reference) and propagation delay.
        let rolloff = if dist > ROLLOFF_REF_M {
            ROLLOFF_REF_M / (ROLLOFF_REF_M + (dist - ROLLOFF_REF_M))
        } else {
            1.0
        };
        self.base_delay =
            (dist / SPEED_OF_SOUND_M_S * self.sample_rate).clamp(8.0, (self.hist_mask - 64) as f32);

        // Occlusion: scene already folded diffraction into the effective dB.
        let occ = &self.scene.occlusion;
        let occ_lin = db_to_linear(occ.transmission_loss_db).max(0.001);
        for f in [
            &mut self.oc_l1,
            &mut self.oc_l2,
            &mut self.oc_r1,
            &mut self.oc_r2,
        ] {
            f.set_target(occ.cutoff_hz);
        }
        // Vanilla range cutoff: full attenuation once the listener leaves
        // `attenuation_distance * max(volume, 1)` (or the server range
        // override). The fade is linear like Vanilla's distance curve
        // applied to the audible range.
        let range_atten = if self.max_range_m.is_finite() {
            ((self.max_range_m - dist) / self.max_range_m).clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.direct_gain = (rolloff * occ_lin * range_atten).clamp(0.0, 1.0);
        // Reflected energy sneaks around walls better than the direct path —
        // half-strength dB penalty on the reverb send, milder still on taps.
        self.wet_send =
            self.scene.wet_gain * db_to_linear(occ.transmission_loss_db * 0.5) * range_atten;
        self.er_gain = db_to_linear(occ.transmission_loss_db * 0.33);

        // Air absorption (Ultra): broad-band air robs highs over distance.
        let air_fc = if self.preset.air_absorption_enabled() {
            OCCLUSION_MAX_CUTOFF_HZ * (-0.006 * dist).exp()
        } else {
            OCCLUSION_MAX_CUTOFF_HZ
        };
        self.air_l.set_target(air_fc.max(UNDERWATER_CUTOFF_HZ));
        self.air_r.set_target(air_fc.max(UNDERWATER_CUTOFF_HZ));

        // Underwater muffle targets.
        let uw_fc = if self.underwater {
            UNDERWATER_CUTOFF_HZ
        } else {
            OCCLUSION_MAX_CUTOFF_HZ
        };
        for f in [
            &mut self.uw_l1,
            &mut self.uw_l2,
            &mut self.uw_r1,
            &mut self.uw_r2,
        ] {
            f.set_target(uw_fc);
        }

        // Doppler: radial closing speed → playback ratio, clamped so extreme
        // (but finite) velocities can never run the delay line off its ends.
        let rel_v = sub3(self.source.velocity, self.listener.velocity);
        let to_source = if dist > 1e-6 {
            let d = sub3(self.source.position, self.listener.position);
            [d[0] / dist, d[1] / dist, d[2] / dist]
        } else {
            [0.0, 0.0, 0.0]
        };
        let radial = dot3(rel_v, to_source);
        let ratio = SPEED_OF_SOUND_M_S / (SPEED_OF_SOUND_M_S + radial);
        self.doppler_ratio = if ratio.is_finite() {
            ratio.clamp(0.5, 1.8)
        } else {
            1.0
        };
        // Re-anchor the read offset near its geometric delay; the drift term
        // continues from wherever Doppler left it.
        let drift = self.read_off - self.base_delay;
        self.read_off =
            (self.base_delay + drift.clamp(-4.0, 256.0)).clamp(8.0, (self.hist_mask - 64) as f32);
    }

    /// Fractional read of the shared mono history `off` samples behind the
    /// write head (linear interpolation). The ring length is a power of two,
    /// so two's-complement masking replaces a 64-bit `rem_euclid` division.
    #[inline]
    fn hist_read(&self, off: f32) -> f32 {
        debug_assert!(self.hist.len().is_power_of_two());
        let pos = self.wpos as f32 - off;
        let base = pos.floor();
        let i0 = (base as isize as usize) & self.hist_mask;
        let i1 = (i0 + 1) & self.hist_mask;
        let frac = (pos - base).clamp(0.0, 1.0);
        self.hist[i0] + (self.hist[i1] - self.hist[i0]) * frac
    }

    /// Per-frame slew: filter cutoffs ease toward their targets so moving
    /// sources/opening doors do not zipper.
    fn settle_filters(&mut self, frame_len: usize) {
        let rate = (frame_len as f32 * 0.01).clamp(0.05, 1.0);
        let fs = self.sample_rate;
        for f in [
            &mut self.hs_l,
            &mut self.hs_r,
            &mut self.oc_l1,
            &mut self.oc_l2,
            &mut self.oc_r1,
            &mut self.oc_r2,
            &mut self.air_l,
            &mut self.air_r,
            &mut self.uw_l1,
            &mut self.uw_l2,
            &mut self.uw_r1,
            &mut self.uw_r2,
        ] {
            f.settle(rate, fs);
        }
    }

    /// Process one mono PCM block into stereo. `out` is cleared and filled
    /// with `2 * input.len()` interleaved `[L, R, ...]` samples. `Off` preset
    /// is a bit-exact mono→stereo copy; every other path is fully sanitized
    /// and limited so pathological input can never emit NaN or exceed
    /// [`OUTPUT_LIMIT`].
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.process_inner(input, out, None);
    }

    /// Like [`Self::process`], but instead of stepping this voice's private
    /// FDN it accumulates `s * wet_send * send_scale` per sample into
    /// `wet_send_acc` (length `input.len()`); `send_scale` carries the
    /// voice's mix gain that the old per-voice FDN output was multiplied by
    /// downstream. The mixer then runs one [`SharedListenerFdnBus`] per
    /// listener on the summed send, so the cave tail keeps decaying after
    /// short voices finish — and every voice reverberates through the same
    /// acoustic field.
    pub fn process_shared(
        &mut self,
        input: &[f32],
        out: &mut Vec<f32>,
        wet_send_acc: &mut [f32],
        send_scale: f32,
    ) {
        debug_assert_eq!(wet_send_acc.len(), input.len());
        self.process_inner(input, out, Some((wet_send_acc, send_scale)));
    }

    fn process_inner(
        &mut self,
        input: &[f32],
        out: &mut Vec<f32>,
        wet_acc: Option<(&mut [f32], f32)>,
    ) {
        out.clear();
        out.reserve(input.len() * 2);
        if !self.preset.enabled() {
            for &s in input {
                out.push(s);
                out.push(s);
            }
            return;
        }
        self.settle_filters(input.len());
        let drift_per_sample = 1.0 - self.doppler_ratio;
        let max_off = (self.hist_mask - 64) as f32;
        let yaw_deg = self.listener.yaw_deg;

        // Block-head early-reflection precompute: listener-yaw rotation, pan
        // azimuth, band-weighted amplitude, and equal-power ear gains are
        // constant for the whole block, so the per-sample loop below only
        // reads the ring and runs two FMA ops per tap.
        let mut er_taps = [PrecomputedTap::ZERO; MAX_EARLY_TAPS];
        let n_taps = self.scene.early_reflections.len().min(MAX_EARLY_TAPS);
        for (pre, tap) in er_taps
            .iter_mut()
            .zip(self.scene.early_reflections.iter())
        {
            *pre = precompute_tap(tap, yaw_deg);
        }

        let mut wet_acc = wet_acc;
        for (i, &raw) in input.iter().enumerate() {
            let s = sanitize_sample(raw);
            self.hist[self.wpos] = s;

            // Doppler: the read offset drifts toward/away from the write head.
            self.read_off = (self.read_off + drift_per_sample).clamp(8.0, max_off);
            let off = self.read_off;

            // Direct path, per ear: fractional delay → head-shadow LP →
            // occlusion LP ×2 → air LP → ear gain.
            let mut l = self.hist_read(off + self.itd_l);
            let mut r = self.hist_read(off + self.itd_r);
            l = self.oc_l2.process(self.oc_l1.process(self.hs_l.process(l)));
            r = self.oc_r2.process(self.oc_r1.process(self.hs_r.process(r)));
            l = self.air_l.process(l) * self.gain_l;
            r = self.air_r.process(r) * self.gain_r;
            l *= self.direct_gain;
            r *= self.direct_gain;

            // Early reflections share the history buffer; each tap's per-ear
            // gain was resolved once at block head, leaving two FMAs here.
            let mut er_l = 0.0;
            let mut er_r = 0.0;
            for tap in &er_taps[..n_taps] {
                let v = self.hist_read(off + tap.delay_off);
                er_l += v * tap.gain_l;
                er_r += v * tap.gain_r;
            }

            // Late reverb: either the private FDN (standalone engine use) or
            // the shared listener bus (mixer path) fed by the same input.
            let (wet_l, wet_r) = if let Some((acc, scale)) = wet_acc.as_mut() {
                acc[i] += s * self.wet_send * *scale;
                (0.0, 0.0)
            } else {
                match &mut self.fdn {
                    Some(fdn) => fdn.step(s * self.wet_send),
                    None => (0.0, 0.0),
                }
            };

            let mut out_l = l + er_l * self.er_gain + wet_l;
            let mut out_r = r + er_r * self.er_gain + wet_r;
            out_l = self.uw_l2.process(self.uw_l1.process(out_l));
            out_r = self.uw_r2.process(self.uw_r1.process(out_r));
            out.push(soft_clip(out_l));
            out.push(soft_clip(out_r));

            self.wpos = (self.wpos + 1) & self.hist_mask;
        }
    }

    /// Convenience wrapper around [`Self::process`] returning the stereo
    /// interleaved block — the shape `VCR1` voice frames want (mono decoded
    /// PCM in, stereo `[L, R, ...]` out, e.g. 960 → 1920 samples).
    pub fn process_voice_frame(&mut self, mono_pcm: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(mono_pcm.len() * 2);
        self.process(mono_pcm, &mut out);
        out
    }

    /// Directional + loudness summary for the client subtitle line, derived
    /// from the same geometry the DSP uses. Zero work when no analyze ran.
    pub fn subtitle_cue(&self) -> SpatialSubtitleCue {
        let az = self.az_rel;
        let direction = if az.abs() < 0.6 {
            SubtitleDirection::Center
        } else if az.abs() > 2.55 {
            SubtitleDirection::Behind
        } else if az > 0.0 {
            SubtitleDirection::Right
        } else {
            SubtitleDirection::Left
        };
        SpatialSubtitleCue {
            direction,
            loudness: (self.direct_gain * 1.6 + self.scene.wet_gain * 0.15).clamp(0.0, 1.0),
            distance_m: self.distance_m,
            label: String::new(),
        }
    }
}

/// Rotate a world-space direction into the listener's yaw plane: returns the
/// direction expressed with +Z = listener forward (used for tap panning).
fn rotate_yaw(dir: [f32; 3], yaw_deg: f32) -> [f32; 3] {
    let yaw = yaw_deg.to_radians();
    let (sy, cy) = yaw.sin_cos();
    // Inverse of the forward/right basis used in `spherical` (pitch folded
    // away — panning is horizontal anyway).
    let fwd = [sy, 0.0, cy];
    let right = normalize3(cross3(fwd, [0.0, 1.0, 0.0]));
    [dot3(dir, right), dir[1], dot3(dir, fwd)]
}
