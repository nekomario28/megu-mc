// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal, data-driven dynamic light emitter resolver.
//!
//! Resolves emission color, world-space origin, light level, radius, and
//! temporal modulation for **any** vanilla block, redstone component, modded
//! block/item, or entity light source — with zero hardcoded mod ID lists.
//!
//! Color and origin come from the emissive pixels of the actual texture when
//! available (`derive_emissive_light_from_rgba` +
//! `uv_centroid_to_block_origin`); when textures are not passed, a generic
//! physical/material spectrum inference runs on the block id token stream and
//! blockstate properties, which naturally covers vanilla, redstone, and every
//! third-party mod without per-mod tables.

use std::f32::consts::TAU;

// ---------------------------------------------------------------------------
// 1. Texture -> emissive light extraction
// ---------------------------------------------------------------------------

/// Emissive color and centroid extracted from an RGBA texture's brightest
/// pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DerivedTextureLight {
    /// Linear `[0.0..=1.0]` normalized RGB hue/color.
    pub rgb: [f32; 3],
    /// `[u, v]` weighted centroid of glowing pixels in `[0.0..=1.0]^2`.
    /// `v = 0.0` is the top of the texture, `v = 1.0` the bottom.
    pub uv_centroid: [f32; 2],
    /// Fraction of non-transparent pixels that are emissive (`0.0..=1.0`).
    pub emissive_coverage: f32,
}

fn luma_709(r: u8, g: u8, b: u8) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

fn saturation(r: u8, g: u8, b: u8) -> f32 {
    let max_c = r.max(g).max(b) as f32;
    let min_c = r.min(g).min(b) as f32;
    let denom = max_c.max(0.001);
    ((max_c - min_c) / denom).clamp(0.0, 1.0)
}

/// Pure-`std`, deterministic RGBA -> emissive light derivation.
///
/// Handles arbitrary sizes. For vertically-stacked animated strips
/// (`height > width`), only frame 0 (`width × width` region at the top) is
/// read, matching Minecraft's animation strip convention.
///
/// Returns `None` when the buffer is empty/malformed or fully transparent.
pub fn derive_emissive_light_from_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Option<DerivedTextureLight> {
    if width == 0 || height == 0 || rgba.is_empty() {
        return None;
    }
    // Frame 0 is width x width when height > width (animated strip).
    let frame_h = if height > width { width } else { height };
    let stride = width as usize;
    let frame_len = stride * frame_h as usize;
    if rgba.len() < frame_len * 4 {
        return None;
    }

    let mut scores: Vec<(f32, usize, usize, [f32; 3])> = Vec::new();
    let mut opaque_count = 0usize;
    let mut max_score = 0.0f32;

    for y in 0..frame_h {
        for x in 0..width {
            let i = (y as usize * stride + x as usize) * 4;
            let (r, g, b, a) = (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]);
            if a < 32 {
                continue;
            }
            opaque_count += 1;
            let luma = luma_709(r, g, b);
            let sat = saturation(r, g, b);
            let score = luma * (0.40 + 0.60 * sat);
            if score > max_score {
                max_score = score;
            }
            scores.push((score, x as usize, y as usize, [r as f32, g as f32, b as f32]));
        }
    }

    if opaque_count == 0 || max_score <= 0.0 {
        return None;
    }

    let cutoff = 0.72 * max_score;
    let mut wsum = 0.0f32;
    let mut rsum = 0.0f32;
    let mut gsum = 0.0f32;
    let mut bsum = 0.0f32;
    let mut usum = 0.0f32;
    let mut vsum = 0.0f32;
    let mut selected = 0usize;

    for (score, x, y, rgb) in &scores {
        if *score < cutoff {
            continue;
        }
        // Weight by emissive strength so the hottest core dominates.
        let w = *score;
        wsum += w;
        rsum += w * rgb[0];
        gsum += w * rgb[1];
        bsum += w * rgb[2];
        usum += w * (*x as f32 + 0.5) / width as f32;
        vsum += w * (*y as f32 + 0.5) / frame_h as f32;
        selected += 1;
    }

    if selected == 0 || wsum <= 0.0 {
        return None;
    }

    let mut rgb = [rsum / wsum / 255.0, gsum / wsum / 255.0, bsum / wsum / 255.0];
    // Normalize so max channel == 1.0, preserving saturation for vivid hue.
    let max_c = rgb[0].max(rgb[1]).max(rgb[2]);
    if max_c > 0.0 {
        for c in rgb.iter_mut() {
            *c /= max_c;
        }
    }
    // Gentle floor on the weakest channel to avoid crushed channels.
    let min_c = rgb[0].min(rgb[1]).min(rgb[2]);
    if min_c > 0.35 {
        let boost = (0.35 / min_c).min(1.0);
        for c in rgb.iter_mut() {
            *c *= boost;
        }
    }

    // emissive_coverage: fraction of opaque pixels at/above the cutoff.
    let coverage = selected as f32 / opaque_count as f32;

    Some(DerivedTextureLight {
        rgb,
        uv_centroid: [usum / wsum, vsum / wsum],
        emissive_coverage: coverage.clamp(0.0, 1.0),
    })
}

// ---------------------------------------------------------------------------
// 2. UV centroid -> block-local 3D origin
// ---------------------------------------------------------------------------

/// Coarse shape hint used to map a 2D texture centroid to a block-local
/// emission origin. Selected generically by the resolver from id/property
/// cues — not by a mod allowlist.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EmitterShapeHint {
    /// Light fills the whole cube (glowstone, lamp core).
    FullCube,
    /// Upright fixture: torch, candle, lantern, rod, chimney, modded lamp.
    UprightFixture,
    /// Wall-attached fixture (wall torch, wall lantern, bracket).
    WallMounted { facing_xz: [f32; 2] },
    /// Flat, floor-level emitter (wire, repeater, comparator, conduit plate).
    FloorFlat,
    /// Suspended emitter (hanging lantern, chain bulb).
    CeilingHanging,
}

/// Convert a glowing-pixel `[u, v]` centroid into a `[0, 1]^3` block-local
/// light origin.
pub fn uv_centroid_to_block_origin(
    uv_centroid: [f32; 2],
    shape: EmitterShapeHint,
) -> [f32; 3] {
    let [u, v] = uv_centroid;
    let u = u.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    // Texture v=0 is top; block-local y=1 is top. `1 - v` gives top-height.
    let top_height = 1.0 - v;
    match shape {
        EmitterShapeHint::FullCube => {
            // Center cube, pull subtly toward the glowing face region.
            [
                0.5 + (u - 0.5) * 0.15,
                0.5 + (top_height - 0.5) * 0.25,
                0.5 + (u - 0.5) * 0.15,
            ]
        }
        EmitterShapeHint::UprightFixture => {
            // Flame/bulb height comes from the texture; horizontal stays
            // centered so a torch's light hugs the glowing core, not the base.
            [0.5, top_height.clamp(0.1, 0.92), 0.5]
        }
        EmitterShapeHint::WallMounted { facing_xz } => {
            let len = (facing_xz[0] * facing_xz[0] + facing_xz[1] * facing_xz[1]).sqrt();
            let (fx, fz) = if len > 0.0 {
                (facing_xz[0] / len, facing_xz[1] / len)
            } else {
                (0.0, 0.0)
            };
            // Mounted on the wall face; push 0.30 into the block interior.
            [
                (0.5 + fx * 0.30).clamp(0.05, 0.95),
                top_height.clamp(0.15, 0.95),
                (0.5 + fz * 0.30).clamp(0.05, 0.95),
            ]
        }
        EmitterShapeHint::FloorFlat => {
            [
                (0.15 + u * 0.70).clamp(0.05, 0.95),
                0.08,
                (0.15 + (1.0 - u) * 0.70).clamp(0.05, 0.95),
            ]
        }
        EmitterShapeHint::CeilingHanging => {
            [
                0.5,
                // Hanging emitters glow below their mount: y sits near the
                // texture's glowing band but biased downward from top.
                (0.15 + top_height * 0.55).clamp(0.08, 0.7),
                0.5,
            ]
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Modulation
// ---------------------------------------------------------------------------

/// Temporal light modulation applied at render time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightModulation {
    /// Constant output (glowstone, redstone lamp, froglight, electric lamp).
    Steady,
    /// Open-flame flicker (torch, campfire, brazier, burning entity).
    OpenFlame { flicker_amplitude: f32 },
    /// Smooth periodic pulse (redstone ore, sculk sensor, active spawner).
    SignalPulse { pulse_hz: f32, amplitude: f32 },
}

// ---------------------------------------------------------------------------
// 4. Resolved emitter
// ---------------------------------------------------------------------------

/// Fully resolved light emitter, ready for the dynamic light system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedLightEmitter {
    /// Linear `[0.0..=1.0]` emission color.
    pub rgb: [f32; 3],
    /// `[0, 1]^3` exact emission origin within the block/item.
    pub local_origin: [f32; 3],
    /// Effective light level `1..=15`.
    pub light_level: u8,
    /// World-space radius in blocks (`2.0..=15.0`).
    pub radius: f32,
    /// Linear radiance scale (`0.2..=2.2`).
    pub intensity: f32,
    /// Temporal modulation.
    pub modulation: LightModulation,
    /// Whether this emitter quenches when submerged/waterlogged.
    pub quenches_underwater: bool,
}

/// Evaluate modulation deterministically at `tick_time_sec` with a stable
/// `phase_seed`. Always finite and `>= 0.0`.
pub fn evaluate_modulated_intensity(
    emitter: &ResolvedLightEmitter,
    tick_time_sec: f32,
    phase_seed: f32,
) -> f32 {
    let base = emitter.intensity;
    if !base.is_finite() || base <= 0.0 || !tick_time_sec.is_finite() {
        return if base.is_finite() { base.max(0.0) } else { 0.0 };
    }
    match emitter.modulation {
        LightModulation::Steady => base,
        LightModulation::OpenFlame { flicker_amplitude } => {
            // Layered deterministic sines give organic flicker.
            let t = tick_time_sec * 9.0 + phase_seed * 12.9898;
            let s = (t.sin() * 0.6 + (t * 2.17 + phase_seed).sin() * 0.4)
                * flicker_amplitude.clamp(0.0, 0.9);
            (base * (1.0 + s)).max(0.0)
        }
        LightModulation::SignalPulse { pulse_hz, amplitude } => {
            let t = tick_time_sec * pulse_hz.max(0.0) * TAU + phase_seed;
            // Smooth 0..1 sine pulse.
            let pulse = 0.5 + 0.5 * t.sin();
            let a = amplitude.clamp(0.0, 1.5);
            base * (1.0 + a * (pulse - 0.5))
        }
    }
    .max(0.0)
}

// ---------------------------------------------------------------------------
// 5. Generic inference helpers (zero mod-id hardcoding)
// ---------------------------------------------------------------------------

/// Extract a block's token stream: lowercase `namespace:name` plus
/// property keys/values, so spectrum/shape/modulation inference keys off
/// material vocabulary, not mod identity.
fn token_stream<'a>(block_id: &'a str, properties: &'a [(&'a str, &'a str)]) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in block_id.split([':', '/', '_', '-']) {
        if !part.is_empty() {
            tokens.push(part.to_ascii_lowercase());
        }
    }
    for (k, v) in properties {
        for part in k.split(['_', '-']) {
            if !part.is_empty() {
                tokens.push(part.to_ascii_lowercase());
            }
        }
        for part in v.split(['_', '-', '|']) {
            if !part.is_empty() {
                tokens.push(part.to_ascii_lowercase());
            }
        }
    }
    tokens
}

fn has_any(tokens: &[String], needles: &[&str]) -> bool {
    tokens.iter().any(|t| needles.contains(&t.as_str()))
}

/// Generic physical spectrum inference: material vocabulary -> plausible
/// emission hue. Covers redstone/crimson, soul/sculk/cyan, amethyst/portal
/// purple, verdant/slime, copper, magma/fire, ender/chorus, and warm
/// blackbody defaults — for ANY namespace.
fn infer_spectrum_rgb(tokens: &[String], level: u8) -> [f32; 3] {
    // Normalize level to a warm blackbody brightness floor.
    let warmth = (level as f32 / 15.0).clamp(0.15, 1.0);
    let blackbody = [1.0, 0.55 + 0.25 * warmth, 0.25 + 0.30 * warmth];

    if has_any(tokens, &["soul", "sculk", "cyan", "ice", "frost", "glacier"]) {
        [0.30, 0.85, 1.0]
    } else if has_any(tokens, &["copper", "amber", "lantern", "lamp", "bulb",
        "filament", "neon", "led", "halogen", "incandescent", "solar",
        "light"]) {
        [1.0, 0.60 + 0.15 * warmth, 0.30]
    } else if has_any(tokens, &["redstone", "crimson", "nylium", "weeping", "wart"]) {
        [1.0, 0.16 + 0.08 * warmth, 0.10]
    } else if has_any(tokens, &["amethyst", "ender", "chorus", "portal", "shard",
        "prismarine", "spore", "netherite", "warp", "void", "astral"]) {
        [0.72, 0.34, 1.0]
    } else if has_any(tokens, &["emerald", "verdant", "slime", "ocean",
        "toxic", "radioactive", "uranium", "mutagen", "fungus", "lichen",
        "bee", "honey", "sap", "bamboo"]) {
        [0.38, 1.0, 0.52]
    } else if has_any(tokens, &["magma", "lava", "fire", "flame", "blaze",
        "inferno", "ember", "forge", "furnace", "kiln", "smelter", "reactor",
        "plasma"]) {
        [1.0, 0.45 + 0.2 * warmth, 0.10]
    } else if has_any(tokens, &["gold", "torch", "candle", "campfire", "brazier",
        "bonfire", "torchflower"]) {
        [1.0, 0.62, 0.22]
    } else if has_any(tokens, &["glow", "froglight", "moss"]) {
        [0.9, 1.0, 0.75]
    } else {
        blackbody
    }
}

/// Shape hint from material vocabulary.
fn infer_shape_hint(tokens: &[String]) -> EmitterShapeHint {
    if has_any(tokens, &["wire", "repeater", "comparator", "conduit", "plate",
        "flat", "rail", "track", "circuit", "node", "pad", "mat", "panel"]) {
        return EmitterShapeHint::FloorFlat;
    }
    if has_any(tokens, &["wall", "bracket", "sconce"]) {
        return EmitterShapeHint::WallMounted { facing_xz: [0.0, 0.0] };
    }
    if has_any(tokens, &["hanging", "chain", "chandelier", "pendant", "suspended"]) {
        return EmitterShapeHint::CeilingHanging;
    }
    if has_any(tokens, &["glowstone", "lamp", "light", "lantern", "bulb", "core",
        "crystal", "shard", "gem", "block", "cube"]) && !has_any(tokens, &[
        "torch", "candle", "rod", "chimney", "pillar", "stick", "staff"]) {
        return EmitterShapeHint::FullCube;
    }
    EmitterShapeHint::UprightFixture
}

/// Modulation from material vocabulary.
fn infer_modulation(tokens: &[String]) -> LightModulation {
    // Flame cues take precedence: a redstone torch still flickers like fire.
    if has_any(tokens, &["torch", "candle", "campfire", "brazier", "bonfire",
        "fire", "flame", "ember", "inferno", "lava", "magma", "blaze",
        "forge", "kiln", "smelter", "burning"]) {
        return LightModulation::OpenFlame { flicker_amplitude: 0.18 };
    }
    if has_any(tokens, &["redstone", "sculk", "sensor", "spawner", "repeater",
        "comparator", "observer", "circuit", "pulse", "signal", "beacon",
        "charge", "reactor", "conduit", "cell", "battery"]) {
        return LightModulation::SignalPulse { pulse_hz: 1.6, amplitude: 0.35 };
    }
    LightModulation::Steady
}

/// Does this emitter quench underwater? Open-flame sources do; enclosed
/// electric/magic/glass emitters and underwater-safe blocks do not.
fn infer_quenches(tokens: &[String], modulation: LightModulation) -> bool {
    if matches!(modulation, LightModulation::OpenFlame { .. }) {
        // Lanterns/campfires enclosed in glass or blocks survive water;
        // bare flames and torches do not.
        !has_any(tokens, &["lantern", "enclosed", "glass", "soul", "sea",
            "pickle", "conduit", "jellyfish", "deep", "aquatic", "coral"])
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// 6. Block input / resolver
// ---------------------------------------------------------------------------

/// Everything the resolver needs about a placed block. All fields are raw
/// registry/state data — no mod allowlists.
pub struct BlockLightInput<'a> {
    /// Registry id, e.g. `"minecraft:redstone_wire"`, `"mymod:plasma_conduit"`.
    pub block_id: &'a str,
    /// Generic blockstate key/value pairs.
    pub properties: &'a [(&'a str, &'a str)],
    /// Registry luminance `0..=15` (`BlockFacts.luminance` or bundle data).
    pub registry_luminance: u8,
    /// Actual block texture pixels from vanilla/mod atlas, when available.
    pub texture_rgba: Option<(&'a [u8], u32, u32)>,
    /// Whether the block position is submerged in water.
    pub submerged: bool,
}

fn property<'a>(props: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// Resolve a placed block's light emitter, or `None` when it is dark.
///
/// State-gate and redstone/power handling are fully generic: any block
/// exposing `lit`, `power`, `powered`, `active`, `charges`, `ominous`, or
/// `waterlogged` properties participates automatically.
pub fn resolve_block_light_emitter(input: &BlockLightInput<'_>) -> Option<ResolvedLightEmitter> {
    let props = input.properties;
    let lit = property(props, "lit");
    let waterlogged = property(props, "waterlogged") == Some("true");

    // Unlit gates: an explicit lit=false with zero registry luminance means
    // the block can never emit in this state.
    if lit == Some("false") && input.registry_luminance == 0 {
        return None;
    }

    let mut effective_level = input.registry_luminance.min(15);

    // --- Generic signal/power state (redstone + modded equivalents) ---
    if effective_level == 0 {
        if let Some(power_str) = property(props, "power") {
            if let Ok(n) = power_str.parse::<u8>() {
                if (1..=15).contains(&n) {
                    effective_level = ((n as f32 / 15.0) * 5.0)
                        .ceil()
                        .clamp(1.0, 5.0) as u8;
                }
            }
        }
    }
    if effective_level == 0 {
        let powered = property(props, "powered") == Some("true")
            || property(props, "active") == Some("true")
            || property(props, "ominous") == Some("true");
        if powered {
            effective_level = 4;
        }
    }
    if effective_level == 0 {
        if let Some(charges_str) = property(props, "charges") {
            if let Ok(n) = charges_str.parse::<u8>() {
                if (1..=4).contains(&n) {
                    effective_level = (n * 3 + 3).min(15);
                }
            }
        }
    }
    // A lit=true flag on a zero-luminance registry block still emits weakly
    // (e.g. modded lamps whose registry entry lacks luminance).
    if effective_level == 0 && lit == Some("true") {
        effective_level = 4;
    }

    if effective_level == 0 {
        return None;
    }

    let tokens = token_stream(input.block_id, props);
    let level_f = effective_level as f32 / 15.0;

    // --- Color & origin ---
    let (mut rgb, local_origin) = if let Some((bytes, w, h)) = input.texture_rgba {
        match derive_emissive_light_from_rgba(bytes, w, h) {
            Some(derived) => {
                let shape = infer_shape_hint(&tokens);
                (derived.rgb, uv_centroid_to_block_origin(derived.uv_centroid, shape))
            }
            None => (infer_spectrum_rgb(&tokens, effective_level), [0.5; 3]),
        }
    } else {
        let shape = infer_shape_hint(&tokens);
        // Without pixels, approximate the glowing region via token cues:
        // upright fixtures glow in the upper half.
        let origin = match shape {
            EmitterShapeHint::FullCube => [0.5, 0.5, 0.5],
            EmitterShapeHint::UprightFixture => [0.5, 0.62, 0.5],
            EmitterShapeHint::WallMounted { .. } => [0.5, 0.62, 0.5],
            EmitterShapeHint::FloorFlat => [0.5, 0.08, 0.5],
            EmitterShapeHint::CeilingHanging => [0.5, 0.45, 0.5],
        };
        (infer_spectrum_rgb(&tokens, effective_level), origin)
    };

    // Texture-derived colors may need a level-based brightness sanity pass:
    // scale so perceived luminance tracks the registry level.
    let luma = luma_709(
        (rgb[0] * 255.0) as u8,
        (rgb[1] * 255.0) as u8,
        (rgb[2] * 255.0) as u8,
    );
    let target_luma = 0.25 + 0.75 * level_f;
    if luma < target_luma * 0.35 {
        let gain = (target_luma * 0.35 / luma.max(0.02)).min(4.0);
        for c in rgb.iter_mut() {
            *c = (*c * gain).min(1.0);
        }
    }

    // --- Radius / intensity ---
    let radius = (1.0 + level_f * 14.0).clamp(2.0, 15.0);
    let intensity = (0.2 + level_f * 1.8).clamp(0.2, 2.2);

    let modulation = infer_modulation(&tokens);
    let quenches = infer_quenches(&tokens, modulation)
        && (input.submerged || waterlogged);

    Some(ResolvedLightEmitter {
        rgb,
        local_origin,
        light_level: effective_level,
        radius,
        intensity,
        modulation,
        quenches_underwater: quenches,
    })
}

// ---------------------------------------------------------------------------
// 7. Item resolver
// ---------------------------------------------------------------------------

/// Resolve a held/dropped item's light contribution.
///
/// `declared_light_level` comes from the block or mod registry when the item
/// is light-bearing; `texture_rgba` supplies exact color when available.
/// Generic fallbacks cover common light-bearing item vocabulary for any
/// namespace (`torch`, `lantern`, `gem`, `crystal`, `cell`, ...).
pub fn resolve_item_light_emitter(
    item_id: &str,
    declared_light_level: Option<u8>,
    texture_rgba: Option<(&[u8], u32, u32)>,
    submerged: bool,
) -> Option<ResolvedLightEmitter> {
    let tokens: Vec<String> = item_id
        .split([':', '/', '_', '-'])
        .filter(|p| !p.is_empty())
        .map(|p| p.to_ascii_lowercase())
        .collect();

    let mut level = declared_light_level.unwrap_or(0).min(15);
    if level == 0 {
        // Generic light-bearing item vocabulary.
        if has_any(&tokens, &["torch", "lantern", "candle", "glow", "gem",
            "crystal", "shard", "orb", "core", "cell", "battery", "bulb",
            "flare", "ember", "spark", "lanternfish", "pearl", "heart",
            "totem", "elytra", "firefly", "wisp", "lumen", "phosphor"]) {
            level = 8;
        } else if has_any(&tokens, &["redstone", "dust", "repeater",
            "comparator", "torch"]) {
            level = 7;
        }
    }
    if level == 0 {
        return None;
    }

    let texture_rgb = texture_rgba.and_then(|(bytes, w, h)| {
        derive_emissive_light_from_rgba(bytes, w, h).map(|d| d.rgb)
    });
    let rgb = texture_rgb.unwrap_or_else(|| infer_spectrum_rgb(&tokens, level));
    let shape = infer_shape_hint(&tokens);
    let origin = match shape {
        EmitterShapeHint::UprightFixture | EmitterShapeHint::WallMounted { .. } => [0.5, 0.6, 0.5],
        _ => [0.5, 0.5, 0.5],
    };

    let level_f = level as f32 / 15.0;
    let modulation = infer_modulation(&tokens);
    let quenches = infer_quenches(&tokens, modulation) && submerged;

    Some(ResolvedLightEmitter {
        rgb,
        local_origin: origin,
        light_level: level,
        radius: (1.0 + level_f * 14.0).clamp(2.0, 15.0),
        intensity: (0.2 + level_f * 1.8).clamp(0.2, 2.2),
        modulation,
        quenches_underwater: quenches,
    })
}

// ---------------------------------------------------------------------------
// 8. Entity resolver
// ---------------------------------------------------------------------------

/// Resolve an entity's light contribution.
///
/// Handles burning entities (`on_fire` -> open flame), spectral glowing
/// entities (`glowing_effect` -> spectral white-cyan), and luminous mobs via
/// `declared_light_level` + `texture_rgba` for any namespace.
pub fn resolve_entity_light_emitter(
    entity_id: &str,
    on_fire: bool,
    glowing_effect: bool,
    declared_light_level: Option<u8>,
    texture_rgba: Option<(&[u8], u32, u32)>,
) -> Option<ResolvedLightEmitter> {
    let mut tokens: Vec<String> = entity_id
        .split([':', '/', '_', '-'])
        .filter(|p| !p.is_empty())
        .map(|p| p.to_ascii_lowercase())
        .collect();

    let texture_rgb = texture_rgba.and_then(|(bytes, w, h)| {
        derive_emissive_light_from_rgba(bytes, w, h).map(|d| d.rgb)
    });

    // Priority: explicit fire > glowing effect > declared luminance.
    if on_fire {
        tokens.push("fire".to_string());
        tokens.push("flame".to_string());
        let rgb = texture_rgb.unwrap_or([1.0, 0.55, 0.12]);
        return Some(ResolvedLightEmitter {
            rgb,
            local_origin: [0.5, 0.55, 0.5],
            light_level: 15,
            radius: 14.0,
            intensity: 1.6,
            modulation: LightModulation::OpenFlame { flicker_amplitude: 0.25 },
            quenches_underwater: true,
        });
    }

    if glowing_effect {
        // Spectral glow: pale cyan-white regardless of mob texture.
        let rgb = texture_rgb.unwrap_or([0.75, 0.95, 1.0]);
        return Some(ResolvedLightEmitter {
            rgb,
            local_origin: [0.5, 0.6, 0.5],
            light_level: 10,
            radius: 9.0,
            intensity: 0.9,
            modulation: LightModulation::Steady,
            quenches_underwater: false,
        });
    }

    let mut level = declared_light_level.unwrap_or(0).min(15);
    if level == 0
        && has_any(&tokens, &["blaze", "magma", "lava", "fire", "phoenix",
            "dragon", "wisp", "will_o", "spectre", "lumen", "ember",
            "inferno", "glow", "shroomlight", "allay"])
    {
        level = 8;
    }
    if level == 0 {
        return None;
    }

    let rgb = texture_rgb.unwrap_or_else(|| infer_spectrum_rgb(&tokens, level));
    let level_f = level as f32 / 15.0;
    Some(ResolvedLightEmitter {
        rgb,
        local_origin: [0.5, 0.55, 0.5],
        light_level: level,
        radius: (1.0 + level_f * 14.0).clamp(2.0, 15.0),
        intensity: (0.2 + level_f * 1.8).clamp(0.2, 2.2),
        modulation: infer_modulation(&tokens),
        quenches_underwater: false,
    })
}

// ---------------------------------------------------------------------------
// 9. Convenience: block id -> shape hint map for callers that need the
//    shape enum directly (e.g. mesher integration).
// ---------------------------------------------------------------------------

/// Public wrapper: guess the shape hint for a block/item id from tokens.
pub fn shape_hint_for_id(id: &str, properties: &[(&str, &str)]) -> EmitterShapeHint {
    let tokens = token_stream(id, properties);
    infer_shape_hint(&tokens)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // 'static property slices so BlockLightInput borrows don't hit temporaries.
    const LIT_FALSE: &[(&str, &str)] = &[("lit", "false")];
    const LIT_TRUE: &[(&str, &str)] = &[("lit", "true")];
    const WATERLOGGED_TRUE: &[(&str, &str)] = &[("waterlogged", "true")];
    const POWER_0: &[(&str, &str)] = &[("power", "0")];
    const POWER_1: &[(&str, &str)] = &[("power", "1")];
    const POWER_15: &[(&str, &str)] = &[("power", "15")];

    /// Build a synthetic 16x16 RGBA texture. `paint(y_range, rgba)` fills rows.
    fn tex16(paint: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; 16 * 16 * 4];
        for y in 0..16usize {
            for x in 0..16usize {
                let i = (y * 16 + x) * 4;
                v[i..i + 4].copy_from_slice(&paint(x, y));
            }
        }
        v
    }

    /// Synthetic torch texture: brown stick bottom half, bright orange flame top.
    fn torch_texture() -> Vec<u8> {
        tex16(|_x, y| {
            if y <= 5 {
                // flame: bright orange/yellow core
                if y <= 3 {
                    [255, 220, 80, 255] // hot core
                } else {
                    [255, 140, 30, 255] // orange flame body
                }
            } else {
                [120, 80, 40, 255] // brown stick
            }
        })
    }

    fn input<'a>(
        id: &'a str,
        props: &'a [(&'a str, &'a str)],
        lum: u8,
        tex: Option<(&'a [u8], u32, u32)>,
        submerged: bool,
    ) -> BlockLightInput<'a> {
        BlockLightInput {
            block_id: id,
            properties: props,
            registry_luminance: lum,
            texture_rgba: tex,
            submerged,
        }
    }

    // -- 1. Texture derivation ------------------------------------------------

    #[test]
    fn torch_texture_yields_warm_orange_and_flame_origin() {
        let tex = torch_texture();
        let derived = derive_emissive_light_from_rgba(&tex, 16, 16).unwrap();
        // Warm orange: red dominant, green mid, blue low.
        assert!(derived.rgb[0] > 0.9, "rgb={:?}", derived.rgb);
        assert!(derived.rgb[1] > 0.4 && derived.rgb[1] < 0.95, "rgb={:?}", derived.rgb);
        assert!(derived.rgb[2] < 0.4, "rgb={:?}", derived.rgb);
        // Flame occupies top rows (v small). Centroid v < 0.45.
        assert!(derived.uv_centroid[1] < 0.45, "uv={:?}", derived.uv_centroid);

        let origin = uv_centroid_to_block_origin(
            derived.uv_centroid,
            EmitterShapeHint::UprightFixture,
        );
        // Flame tip maps to y > 0.60, NOT the stick base at y~0.1.
        assert!(origin[1] > 0.60, "origin={:?}", origin);
        assert!(origin[1] < 0.95);
    }

    #[test]
    fn emissive_extraction_handles_transparency_and_bad_input() {
        assert!(derive_emissive_light_from_rgba(&[], 16, 16).is_none());
        assert!(derive_emissive_light_from_rgba(&[0u8; 4], 0, 16).is_none());
        // Fully transparent.
        let mut transparent = vec![0u8; 16 * 16 * 4];
        for px in transparent.chunks_mut(4) {
            px[3] = 0;
        }
        assert!(derive_emissive_light_from_rgba(&transparent, 16, 16).is_none());
        // Truncated buffer.
        assert!(derive_emissive_light_from_rgba(&[255u8; 100], 16, 16).is_none());
    }

    #[test]
    fn animated_strip_reads_frame_zero() {
        // 16x64 strip: frame 0 = top 16x16, frames below garbage.
        let mut strip = vec![0u8; 16 * 64 * 4];
        for y in 0..16usize {
            for x in 0..16usize {
                let i = (y * 16 + x) * 4;
                strip[i..i + 4].copy_from_slice(&[0, 200, 255, 255]); // cyan frame 0
            }
        }
        // Fill lower frames with red to prove they're ignored.
        for y in 16..64usize {
            for x in 0..16usize {
                let i = (y * 16 + x) * 4;
                strip[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
            }
        }
        let d = derive_emissive_light_from_rgba(&strip, 16, 64).unwrap();
        assert!(d.rgb[2] > d.rgb[0], "rgb={:?}", d.rgb); // cyan not red
    }

    // -- 2. Zero-hardcoding modded block ---------------------------------------

    #[test]
    fn arbitrary_modded_lamp_resolves_without_mod_table() {
        // Bright emerald pixels -> green light, radius > 12.
        let tex = tex16(|_x, _y| [30, 255, 90, 255]);
        let inp = input(
            "arbitrarymod:xyz_lamp",
            &[],
            14,
            Some((&tex, 16, 16)),
            false,
        );
        let e = resolve_block_light_emitter(&inp).unwrap();
        assert!(e.rgb[1] > e.rgb[0] && e.rgb[1] > e.rgb[2], "rgb={:?}", e.rgb);
        assert!(e.radius > 12.0, "radius={}", e.radius);
        assert_eq!(e.light_level, 14);
    }

    // -- 3. Redstone family ------------------------------------------------------

    #[test]
    fn redstone_lamp_gates_on_lit() {
        let off = input("minecraft:redstone_lamp", LIT_FALSE, 0, None, false);
        assert!(resolve_block_light_emitter(&off).is_none());
        let on = input("minecraft:redstone_lamp", LIT_TRUE, 15, None, false);
        let e = resolve_block_light_emitter(&on).unwrap();
        assert_eq!(e.light_level, 15);
        assert!(e.intensity > 1.5);
        // Warm white lamp.
        assert!(e.rgb[0] > 0.8 && e.rgb[1] > 0.6);
    }

    #[test]
    fn redstone_torch_gates_and_colors_crimson() {
        let off = input("minecraft:redstone_torch", LIT_FALSE, 0, None, false);
        assert!(resolve_block_light_emitter(&off).is_none());
        let on = input("minecraft:redstone_torch", LIT_TRUE, 7, None, false);
        let e = resolve_block_light_emitter(&on).unwrap();
        assert!(e.rgb[0] > 0.9 && e.rgb[1] < 0.45, "rgb={:?}", e.rgb);
        // Open flame modulation.
        assert!(matches!(e.modulation, LightModulation::OpenFlame { .. }));
    }

    #[test]
    fn redstone_wire_power_scales_monotonically() {
        let p1 = input("minecraft:redstone_wire", POWER_1, 0, None, false);
        let p15 = input("minecraft:redstone_wire", POWER_15, 0, None, false);
        let e1 = resolve_block_light_emitter(&p1).unwrap();
        let e15 = resolve_block_light_emitter(&p15).unwrap();
        assert!(e15.light_level > e1.light_level);
        assert!(e15.intensity > e1.intensity);
        // Floor-flat origin.
        assert!(e1.local_origin[1] < 0.15, "origin={:?}", e1.local_origin);
        // power=0 -> none.
        let p0 = input("minecraft:redstone_wire", POWER_0, 0, None, false);
        assert!(resolve_block_light_emitter(&p0).is_none());
    }

    #[test]
    fn powered_components_glow_subtly() {
        for (id, prop) in [
            ("minecraft:repeater", "powered"),
            ("minecraft:comparator", "powered"),
            ("minecraft:observer", "powered"),
        ] {
            let off_props = [(prop, "false")];
            let on_props = [(prop, "true")];
            let off = input(id, &off_props, 0, None, false);
            assert!(resolve_block_light_emitter(&off).is_none(), "{id} off");
            let on = input(id, &on_props, 0, None, false);
            let e = resolve_block_light_emitter(&on).unwrap();
            assert_eq!(e.light_level, 4, "{id}");
            assert!(e.intensity < 0.8, "{id} subtle");
        }
    }

    #[test]
    fn redstone_ore_pulses() {
        let inp = input("minecraft:redstone_ore", LIT_TRUE, 9, None, false);
        let e = resolve_block_light_emitter(&inp).unwrap();
        assert!(matches!(e.modulation, LightModulation::SignalPulse { .. }));
        assert!(e.rgb[0] > e.rgb[1]);
    }

    #[test]
    fn copper_bulb_oxidation_tiers_decrease() {
        // Exposed oxidation property lowers luminance tiers 15/12/8/4.
        let tiers = [15u8, 12, 8, 4];
        let mut prev_radius = f32::MAX;
        let mut prev_intensity = f32::MAX;
        for lum in tiers {
            let bulb_props = [("lit", "true"), ("lit_level", "15")];
            let inp = input(
                "minecraft:exposed_copper_bulb",
                &bulb_props,
                lum,
                None,
                false,
            );
            let e = resolve_block_light_emitter(&inp).unwrap();
            assert!(e.radius < prev_radius);
            assert!(e.intensity < prev_intensity);
            prev_radius = e.radius;
            prev_intensity = e.intensity;
        }
    }

    // -- 4. Underwater quenching -------------------------------------------------

    #[test]
    fn open_flame_quenches_but_enclosed_does_not() {
        // Torch quenches.
        let torch = input("minecraft:torch", &[], 14, None, true);
        let e = resolve_block_light_emitter(&torch).unwrap();
        assert!(e.quenches_underwater);

        // Redstone lamp is Steady -> never quenches.
        let lamp = input("minecraft:redstone_lamp", LIT_TRUE, 15, None, true);
        let e = resolve_block_light_emitter(&lamp).unwrap();
        assert!(!e.quenches_underwater);

        // Sea lantern / soul lantern / modded block stay lit.
        for id in ["minecraft:sea_lantern", "minecraft:soul_lantern", "mymod:void_lantern"] {
            let inp = input(id, &[], 15, None, true);
            let e = resolve_block_light_emitter(&inp).unwrap();
            assert!(!e.quenches_underwater, "{id}");
        }

        // Waterlogged torch also quenches via waterlogged property.
        let wl = input("minecraft:torch", WATERLOGGED_TRUE, 14, None, false);
        let e = resolve_block_light_emitter(&wl).unwrap();
        assert!(e.quenches_underwater);
    }

    // -- 5. Item / entity resolvers ----------------------------------------------

    #[test]
    fn item_resolver_covers_light_bearing_items() {
        assert!(resolve_item_light_emitter("minecraft:stick", None, None, false).is_none());
        let torch = resolve_item_light_emitter("minecraft:torch", Some(14), None, false).unwrap();
        assert_eq!(torch.light_level, 14);
        let mod_gem = resolve_item_light_emitter("somemod:plasma_gem", None, None, false);
        assert!(mod_gem.is_some());
        assert!(mod_gem.unwrap().rgb[1] > 0.5 || mod_gem.unwrap().rgb[2] > 0.5);
    }

    #[test]
    fn entity_resolver_handles_fire_glow_and_luminous() {
        let burning = resolve_entity_light_emitter("minecraft:zombie", true, false, None, None).unwrap();
        assert!(matches!(burning.modulation, LightModulation::OpenFlame { .. }));
        assert!(burning.quenches_underwater);

        let glowing = resolve_entity_light_emitter("minecraft:sheep", false, true, None, None).unwrap();
        assert!(glowing.rgb[2] >= glowing.rgb[0]); // cyan-white spectral

        let plain = resolve_entity_light_emitter("minecraft:pig", false, false, None, None);
        assert!(plain.is_none());

        let blaze = resolve_entity_light_emitter("minecraft:blaze", false, false, Some(12), None).unwrap();
        assert_eq!(blaze.light_level, 12);
    }

    // -- 6. Modulation determinism ------------------------------------------------

    #[test]
    fn modulation_is_deterministic_and_nonnegative() {
        let emitter = ResolvedLightEmitter {
            rgb: [1.0, 0.6, 0.2],
            local_origin: [0.5, 0.6, 0.5],
            light_level: 14,
            radius: 13.0,
            intensity: 1.8,
            modulation: LightModulation::OpenFlame { flicker_amplitude: 0.3 },
            quenches_underwater: false,
        };
        let a = evaluate_modulated_intensity(&emitter, 1.23, 4.5);
        let b = evaluate_modulated_intensity(&emitter, 1.23, 4.5);
        assert_eq!(a, b);
        assert!(a >= 0.0 && a.is_finite());
        // Pulse emitter varies over time.
        let pulse = ResolvedLightEmitter {
            modulation: LightModulation::SignalPulse { pulse_hz: 2.0, amplitude: 0.8 },
            ..emitter
        };
        let v1 = evaluate_modulated_intensity(&pulse, 0.0, 0.0);
        let v2 = evaluate_modulated_intensity(&pulse, 0.125, 0.0);
        assert!((v1 - v2).abs() > 1e-4);
    }
}
