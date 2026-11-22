// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Optional built-in resource pack: **Classic Unified Ores**.
//!
//! This module is **opt-in and default OFF**: [`ClassicOresConfig::default`]
//! has `enabled == false`, so it is exposed in `Mods & Packs` as a standard
//! built-in pack the user may toggle and Settings gains zero new knobs. When
//! enabled it procedurally synthesizes nostalgic, unified-silhouette ore
//! textures — the same clean chunky vein shape stamped into a host-rock
//! background for every ore — including **Copper** (warm orange primary with
//! subtle teal-green verdigris patina accents), the Deepslate and Nether
//! variants, and *standard* stone-hosted mod ores (Create Zinc, Silver, Tin,
//! Osmium, Lead, …) whose vein colours are extracted directly from the mod's
//! own texture so the classic look inherits the mod's identity.
//!
//! The hard rule of this module: **special, unique-art, animated, or
//! custom-model ores must never be touched.** [`evaluate_ore_adaptation`] is
//! a fail-safe classifier that returns
//! [`OreAdaptationDecision::PreserveOriginal`] unless every check proves the
//! block is an ordinary ore texture: pack enabled, no external resource pack
//! override, not denylisted, static single 16x16 frame, standard cube model,
//! not a known unique block, actually an ore, inside the configured scope,
//! and — for mod ores — pixels that demonstrably look like veins embedded in
//! a standard stone/deepslate/netherrack background. Any doubt preserves the
//! original bytes untouched.
//!
//! All art here is clean-room: no Mojang or third-party texture bytes are
//! referenced anywhere. The vein mask is generated from a small table of blob
//! seeds with deterministic integer hashing, host rocks are procedural
//! integer-noise fields, and all palettes are either hand-picked clean-room
//! colours or extracted from the caller-supplied mod texture. Everything is
//! pure `std`, reads no wall clock, and is fully deterministic: the same
//! inputs always produce byte-identical output.

use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Limits and constants
// ---------------------------------------------------------------------------

/// Edge length of a classic ore sprite, in pixels.
pub const CLASSIC_ORE_DIM: u32 = 16;

/// Byte length of one 16x16 RGBA8 sprite (`16 * 16 * 4`).
pub const CLASSIC_ORE_BYTES: usize = 1024;

/// Alpha below which a source pixel counts as transparent (matching the
/// classic alpha-test threshold used across this crate).
const ALPHA_CUTOFF: u8 = 128;

/// Minimum fraction of the sprite that must match a standard host-rock
/// background for a mod ore to count as "standard stone-hosted".
const MIN_HOST_RATIO: f32 = 0.55;

/// Minimum fraction of the sprite that may be non-host vein pixels. Below
/// this the "veins" are too sparse to be an ore texture — likely noise or a
/// flat gem block.
const MIN_VEIN_RATIO: f32 = 0.06;

/// Maximum fraction of the sprite that may be non-host vein pixels. Above
/// this the texture is mostly coloured art, not veins embedded in rock.
const MAX_VEIN_RATIO: f32 = 0.48;

/// Minimum combined share of the two dominant vein hue clusters. Veins that
/// scatter across more of the colour wheel are stylised multi-colour art, not
/// a coherent single-mineral vein.
const MIN_HUE_CLUSTER_SHARE: f32 = 0.80;

/// Fraction by which the 1px recessed border ring is darkened.
const EDGE_SHADE_NUM: u32 = 225;
const EDGE_SHADE_DEN: u32 = 255;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which ores the pack is allowed to restyle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClassicOresScope {
    /// Only vanilla `minecraft:` ores (coal, iron, gold, diamond, redstone,
    /// lapis, emerald, copper, quartz plus their deepslate and nether
    /// variants). `ancient_debris` and `gilded_blackstone` are never in
    /// scope.
    VanillaOnly,
    /// Vanilla ores plus mod ores that pass the strict standard-ore safety
    /// checks (16x16 static frame, standard cube model, standard
    /// stone/deepslate/netherrack host background with a coherent 1-2 hue
    /// vein cluster, not denylisted). **Default** when the pack is enabled.
    #[default]
    VanillaAndStandardModOres,
}

/// Per-user configuration for the Classic Unified Ores pack. `Default` is
/// fully vanilla: `enabled == false`.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassicOresConfig {
    /// Master opt-in switch. **`false` by default** — the pack never changes
    /// the vanilla look unless explicitly enabled in `Mods & Packs`.
    pub enabled: bool,
    /// Which ores may be restyled; see [`ClassicOresScope`].
    pub scope: ClassicOresScope,
    /// When `true` (default), any texture an external resource pack supplies
    /// for the ore always wins — the pack yields rather than fighting user
    /// content.
    pub respect_external_resourcepack: bool,
    /// Mod namespaces that must never be restyled (e.g. `"thaumcraft"`).
    pub denylist_mods: BTreeSet<String>,
    /// Individual block ids that must never be restyled. Matched against the
    /// full `namespace:path` id and the bare `path` for convenience.
    pub denylist_blocks: BTreeSet<String>,
    /// Individual block ids the user explicitly trusts: they skip the
    /// host-rock pixel heuristic (all other safety checks still apply).
    pub allowlist_blocks: BTreeSet<String>,
}

impl Default for ClassicOresConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            scope: ClassicOresScope::VanillaAndStandardModOres,
            respect_external_resourcepack: true,
            denylist_mods: BTreeSet::new(),
            denylist_blocks: BTreeSet::new(),
            allowlist_blocks: BTreeSet::new(),
        }
    }
}

impl ClassicOresConfig {
    /// Whether `block_id` (`"namespace:path"`) is explicitly denylisted.
    fn is_denylisted(&self, block_id: &str) -> bool {
        let (namespace, path) = split_id(block_id);
        self.denylist_mods.contains(namespace)
            || self.denylist_blocks.contains(block_id)
            || self.denylist_blocks.contains(path)
    }

    /// Whether `block_id` is explicitly allowlisted by the user.
    fn is_allowlisted(&self, block_id: &str) -> bool {
        let (_, path) = split_id(block_id);
        self.allowlist_blocks.contains(block_id) || self.allowlist_blocks.contains(path)
    }
}

// ---------------------------------------------------------------------------
// Decision / classification
// ---------------------------------------------------------------------------

/// Why an ore texture was left untouched. Every variant is a deliberate
/// fail-safe: the classifier must be *sure* a texture is an ordinary ore
/// before it agrees to restyle it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreserveReason {
    /// The pack is disabled (the default state).
    PackDisabled,
    /// An external resource pack already provides this texture and
    /// `respect_external_resourcepack` is set.
    ExternalPackPriority,
    /// The block id or its mod namespace is on the user's denylist.
    Denylisted,
    /// The texture is animated (`.mcmeta` animation or a non-square strip).
    AnimatedTexture,
    /// The texture is not 16x16 — HD or custom-resolution art is never
    /// downscaled.
    NonStandardResolution,
    /// The block uses a custom / non-full-cube model.
    CustomModel,
    /// A known unique block: `ancient_debris`, `gilded_blackstone`, budding
    /// amethyst, `*_cluster`, `*_crystal*`, `*_geode*`, and friends.
    SpecialUniqueOre,
    /// Not an ore block at all (no vanilla-ore match, no ore tag, no `*_ore`
    /// name suffix).
    NotAnOre,
    /// A non-`minecraft` block seen while scope is
    /// [`ClassicOresScope::VanillaOnly`].
    ExcludedByScope,
    /// Pixel analysis proved the texture is not veins-in-standard-rock: the
    /// host background ratio was too low, the vein fraction fell outside
    /// `0.06..=0.48`, or the vein colours scatter across too many hue
    /// clusters. Stylised / crystalline / full-frame legacy art keeps its
    /// original look.
    UniqueModArtStyle,
}

impl PreserveReason {
    /// Stable machine-readable name for logs and pack UI badges.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PackDisabled => "pack_disabled",
            Self::ExternalPackPriority => "external_pack_priority",
            Self::Denylisted => "denylisted",
            Self::AnimatedTexture => "animated_texture",
            Self::NonStandardResolution => "non_standard_resolution",
            Self::CustomModel => "custom_model",
            Self::SpecialUniqueOre => "special_unique_ore",
            Self::NotAnOre => "not_an_ore",
            Self::ExcludedByScope => "excluded_by_scope",
            Self::UniqueModArtStyle => "unique_mod_art_style",
        }
    }
}

/// Host rock the unified vein silhouette is stamped into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostRock {
    /// Neutral mid-gray overworld stone.
    #[default]
    Stone,
    /// Dark slate-gray deepslate.
    Deepslate,
    /// Warm dark-red netherrack.
    Netherrack,
}

impl HostRock {
    /// Stable machine-readable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stone => "stone",
            Self::Deepslate => "deepslate",
            Self::Netherrack => "netherrack",
        }
    }
}

/// Three-tone vein palette: a bright top-left `highlight`, the dominant
/// `primary` mineral colour, and a bottom-right `shadow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OreVeinPalette {
    /// Brightest vein tone (lit edge).
    pub highlight: [u8; 3],
    /// Dominant vein tone (mineral body).
    pub primary: [u8; 3],
    /// Darkest vein tone (recessed edge).
    pub shadow: [u8; 3],
}

/// Everything needed to synthesize one classic ore sprite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SynthesisPlan {
    /// Host rock the vein is embedded in.
    pub host: HostRock,
    /// Three-tone vein palette.
    pub palette: OreVeinPalette,
}

/// What the texture pipeline should do with one ore texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OreAdaptationDecision {
    /// Keep the original texture bytes untouched.
    PreserveOriginal(PreserveReason),
    /// Replace the texture with a procedurally synthesized classic unified
    /// ore built from this plan.
    SynthesizeClassic(SynthesisPlan),
}

/// Caller-supplied facts about one candidate ore texture.
#[derive(Clone, Debug)]
pub struct OreTextureInput<'a> {
    /// Namespaced block id, e.g. `"minecraft:iron_ore"` or `"create:zinc_ore"`.
    pub block_id: &'a str,
    /// Block/item tags the registry reports for the block, e.g.
    /// `"c:ores"`, `"forge:ores"`, `"minecraft:coal_ores"`.
    pub tags: Vec<String>,
    /// Texture width in pixels.
    pub width: u32,
    /// Texture height in pixels (taller than wide implies a vertical
    /// animation strip).
    pub height: u32,
    /// An `.mcmeta` animation descriptor accompanies the texture.
    pub has_mcmeta_animation: bool,
    /// The block model is not the standard full cube (custom JSON/3D model).
    pub has_custom_model: bool,
    /// An enabled external resource pack already supplies this texture.
    pub has_external_pack_override: bool,
    /// Raw RGBA8 pixels (`width * height * 4` bytes). May be empty or short
    /// when the caller has no pixel data — that simply makes the mod-ore
    /// pixel heuristic fail closed into preservation.
    pub rgba: &'a [u8],
}

/// Splits `"namespace:path"` into its parts; an un-namespaced id is treated
/// as `minecraft:`.
fn split_id(block_id: &str) -> (&str, &str) {
    match block_id.split_once(':') {
        Some((ns, path)) => (ns, path),
        None => ("minecraft", block_id),
    }
}

/// Known unique-art block paths that are always preserved, regardless of
/// whether they otherwise look like an ore. These are the blocks whose
/// vanilla identity *is* their special texture.
fn is_special_unique_path(path: &str) -> bool {
    path == "ancient_debris"
        || path == "gilded_blackstone"
        || path.contains("amethyst")
        || path.contains("crystal")
        || path.contains("geode")
        || path.ends_with("_cluster")
}

/// Whether `tag` is an ore-family tag: `c:ores`, `forge:ores`,
/// `minecraft:*_ores`, and equivalents.
fn is_ore_tag(tag: &str) -> bool {
    let (_, path) = split_id(tag);
    path == "ores" || path.ends_with("_ores")
}

/// Curated clean-room palette for a vanilla ore id, or `None` when the id is
/// not a standard vanilla ore. Deepslate and nether variants share the
/// mineral palette of their base ore — only the host rock changes.
pub fn curated_vanilla_palette(block_id: &str) -> Option<OreVeinPalette> {
    let pal = |h: [u8; 3], p: [u8; 3], s: [u8; 3]| OreVeinPalette {
        highlight: h,
        primary: p,
        shadow: s,
    };
    let (_, path) = split_id(block_id);
    Some(match path {
        "coal_ore" | "deepslate_coal_ore" => {
            pal([104, 104, 108], [58, 58, 62], [26, 26, 30])
        }
        "iron_ore" | "deepslate_iron_ore" => {
            pal([236, 208, 176], [206, 152, 112], [138, 92, 60])
        }
        "gold_ore" | "deepslate_gold_ore" | "nether_gold_ore" => {
            pal([255, 242, 148], [246, 208, 62], [166, 128, 22])
        }
        "diamond_ore" | "deepslate_diamond_ore" => {
            pal([196, 250, 246], [93, 219, 213], [31, 132, 148])
        }
        "redstone_ore" | "deepslate_redstone_ore" => {
            pal([255, 148, 140], [226, 38, 38], [128, 12, 12])
        }
        "lapis_ore" | "deepslate_lapis_ore" => {
            pal([134, 162, 255], [56, 92, 220], [22, 40, 138])
        }
        "emerald_ore" | "deepslate_emerald_ore" => {
            pal([148, 255, 176], [38, 199, 92], [12, 118, 44])
        }
        // Copper: warm copper-orange body with a teal-green verdigris patina
        // as the lit accent — the oxidation fleck is part of the metal's
        // identity, so the highlight layer carries it.
        "copper_ore" | "deepslate_copper_ore" => {
            pal([96, 178, 150], [196, 108, 56], [112, 56, 30])
        }
        "nether_quartz_ore" => pal([255, 250, 244], [233, 219, 208], [168, 148, 136]),
        _ => return None,
    })
}

/// Integer luminance proxy (`~0.3/0.59/0.11` in 1/256 units) for ordering.
fn lum(p: [u8; 3]) -> u32 {
    p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29
}

/// Coarse hue bucket for a pixel: `0..=11` are 30-degree hue bins, `12` is
/// the achromatic "neutral" bin (near-gray, near-black, near-white veins like
/// coal or quartz all land here and count as one coherent cluster).
fn hue_bin(r: i32, g: i32, b: i32) -> usize {
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    if d < 24 {
        return 12;
    }
    // Integer hue in degrees, 0..360 (standard hexcone, x60 scaled).
    let h60 = if mx == r {
        60 * (g - b) / d
    } else if mx == g {
        60 * (b - r) / d + 120
    } else {
        60 * (r - g) / d + 240
    };
    let h = h60.rem_euclid(360) as usize;
    (h / 30).min(11)
}

/// Whether one 16x16 pixel is part of `host` rock. Matching is deliberately
/// fuzzy: it recognises the general family of a standard rock colour, not one
/// exact shade, so modded stone variants still classify.
fn is_host_pixel(host: HostRock, r: u8, g: u8, b: u8) -> bool {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let spread = mx - mn;
    match host {
        // Stone: neutral mid gray.
        HostRock::Stone => spread <= 20 && (88..=175).contains(&mx),
        // Deepslate: near-neutral dark gray (a faint cool bias is allowed).
        HostRock::Deepslate => spread <= 26 && (30..=95).contains(&mx),
        // Netherrack: clearly warm dark red — red channel dominates.
        HostRock::Netherrack => r > g + 22 && r > b + 22 && (55..=165).contains(&r) && g < 115,
    }
}

/// Counts how many of the 256 pixels of `rgba` match each host family and
/// returns the dominant one (ties and no-data default to `Stone`).
pub fn detect_host_rock(rgba: &[u8]) -> HostRock {
    let mut counts = [0u32; 3];
    if rgba.len() >= CLASSIC_ORE_BYTES {
        for px in rgba[..CLASSIC_ORE_BYTES].as_chunks::<4>().0 {
            if px[3] < ALPHA_CUTOFF {
                continue;
            }
            for (i, host) in [HostRock::Netherrack, HostRock::Deepslate, HostRock::Stone]
                .iter()
                .enumerate()
            {
                if is_host_pixel(*host, px[0], px[1], px[2]) {
                    counts[i] += 1;
                    break;
                }
            }
        }
    }
    // counts[] order: Netherrack, Deepslate, Stone.
    if counts[0] >= counts[1] && counts[0] >= counts[2] && counts[0] > 0 {
        HostRock::Netherrack
    } else if counts[1] >= counts[2] && counts[1] > 0 {
        HostRock::Deepslate
    } else {
        HostRock::Stone
    }
}

/// Extracts a three-tone `(highlight, primary, shadow)` vein palette from the
/// non-host pixels of a 16x16 mod-ore texture. Veins are sorted by luminance;
/// the darkest quartile becomes `shadow`, the middle half `primary`, and the
/// brightest quartile `highlight`, so a single flat vein colour yields that
/// exact colour as `primary`. Returns `None` when the buffer is not a full
/// 16x16 frame or contains no vein pixels.
pub fn extract_ore_vein_palette(rgba: &[u8], host: HostRock) -> Option<OreVeinPalette> {
    if rgba.len() < CLASSIC_ORE_BYTES {
        return None;
    }
    let mut veins: Vec<[u8; 3]> = Vec::with_capacity(256);
    for px in rgba[..CLASSIC_ORE_BYTES].as_chunks::<4>().0 {
        if px[3] < ALPHA_CUTOFF || is_host_pixel(host, px[0], px[1], px[2]) {
            continue;
        }
        veins.push([px[0], px[1], px[2]]);
    }
    if veins.len() < 4 {
        return None;
    }
    veins.sort_by_key(|p| lum(*p));

    let avg = |slice: &[[u8; 3]]| -> [u8; 3] {
        let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
        for p in slice {
            r += p[0] as u32;
            g += p[1] as u32;
            b += p[2] as u32;
        }
        let n = slice.len() as u32;
        [(r / n) as u8, (g / n) as u8, (b / n) as u8]
    };

    let n = veins.len();
    let q = (n / 4).max(1);
    let shadow = avg(&veins[..q]);
    let primary = avg(&veins[q..n - q]);
    let highlight = avg(&veins[n - q..]);

    // A flat single-colour vein extracts to three identical tones; derive
    // accent tones procedurally so the classic 3-layer shading still reads.
    if highlight == primary && primary == shadow {
        let lighten = |c: u8| c.saturating_add((255 - c) / 3);
        let darken = |c: u8| (c as u32 * 3 / 5) as u8;
        return Some(OreVeinPalette {
            highlight: primary.map(lighten),
            primary,
            shadow: primary.map(darken),
        });
    }
    Some(OreVeinPalette {
        highlight,
        primary,
        shadow,
    })
}

/// Pixel-level "standard ore" check for non-vanilla blocks. Returns the
/// detected host rock on success, or the reason the texture must be
/// preserved.
///
/// A standard ore texture is veins embedded in ordinary rock: at least 55%
/// of the sprite matches the dominant stone/deepslate/netherrack background,
/// the coloured vein fraction sits in `0.06..=0.48`, and those vein pixels
/// concentrate into one or two coherent hue clusters. Anything else —
/// crystalline full-frame art, glowing rune blocks, flat gem tiles, rainbow
/// legacy sprites — fails closed into [`PreserveReason::UniqueModArtStyle`].
fn classify_mod_ore_pixels(rgba: &[u8]) -> Result<HostRock, PreserveReason> {
    if rgba.len() < CLASSIC_ORE_BYTES {
        return Err(PreserveReason::UniqueModArtStyle);
    }
    let host = detect_host_rock(rgba);
    let mut host_count = 0u32;
    let mut hue_bins = [0u32; 13];
    for px in rgba[..CLASSIC_ORE_BYTES].as_chunks::<4>().0 {
        if px[3] < ALPHA_CUTOFF {
            continue;
        }
        if is_host_pixel(host, px[0], px[1], px[2]) {
            host_count += 1;
        } else {
            hue_bins[hue_bin(px[0] as i32, px[1] as i32, px[2] as i32)] += 1;
        }
    }
    let host_ratio = host_count as f32 / 256.0;
    let vein_count = hue_bins.iter().sum::<u32>();
    let vein_ratio = vein_count as f32 / 256.0;
    if host_ratio < MIN_HOST_RATIO || vein_ratio < MIN_VEIN_RATIO || vein_ratio > MAX_VEIN_RATIO {
        return Err(PreserveReason::UniqueModArtStyle);
    }
    // The two dominant vein colour clusters must carry >= 80% of the vein
    // pixels; scattered rainbow art fails here.
    let mut top = [0u32; 2];
    for c in hue_bins {
        if c > top[0] {
            top = [c, top[0]];
        } else if c > top[1] {
            top[1] = c;
        }
    }
    if vein_count > 0 && (top[0] + top[1]) as f32 / (vein_count as f32) < MIN_HUE_CLUSTER_SHARE {
        return Err(PreserveReason::UniqueModArtStyle);
    }
    Ok(host)
}

/// Infers the host rock from a block id when no pixel evidence exists:
/// `deepslate_*` ores live in deepslate, `nether_*` ores in netherrack.
fn host_from_id(block_id: &str) -> HostRock {
    let (_, path) = split_id(block_id);
    if path.starts_with("deepslate_") {
        HostRock::Deepslate
    } else if path.starts_with("nether_") || path.contains("netherrack") {
        HostRock::Netherrack
    } else {
        HostRock::Stone
    }
}

/// Combines config and texture facts into a restyle-or-preserve decision.
///
/// Check order is the fail-safe ladder documented on [`PreserveReason`]:
/// cheapest global gates first, pixel analysis last. Every early return keeps
/// the original texture, so an odd input can only ever *under*-apply the
/// pack, never damage a special texture.
pub fn evaluate_ore_adaptation(
    config: &ClassicOresConfig,
    input: &OreTextureInput,
) -> OreAdaptationDecision {
    let preserve = |r: PreserveReason| OreAdaptationDecision::PreserveOriginal(r);

    if !config.enabled {
        return preserve(PreserveReason::PackDisabled);
    }
    if config.respect_external_resourcepack && input.has_external_pack_override {
        return preserve(PreserveReason::ExternalPackPriority);
    }
    if config.is_denylisted(input.block_id) {
        return preserve(PreserveReason::Denylisted);
    }
    if input.has_mcmeta_animation || input.height != input.width {
        return preserve(PreserveReason::AnimatedTexture);
    }
    if input.width != CLASSIC_ORE_DIM || input.height != CLASSIC_ORE_DIM {
        return preserve(PreserveReason::NonStandardResolution);
    }
    if input.has_custom_model {
        return preserve(PreserveReason::CustomModel);
    }

    let (namespace, path) = split_id(input.block_id);
    if is_special_unique_path(path) {
        return preserve(PreserveReason::SpecialUniqueOre);
    }

    let vanilla_palette = curated_vanilla_palette(input.block_id);
    let is_ore = vanilla_palette.is_some()
        || path.ends_with("_ore")
        || input.tags.iter().any(|t| is_ore_tag(t));
    if !is_ore {
        return preserve(PreserveReason::NotAnOre);
    }

    if namespace != "minecraft" {
        if config.scope == ClassicOresScope::VanillaOnly {
            return preserve(PreserveReason::ExcludedByScope);
        }
        // Mod ore: either the user allowlisted it (skip the pixel heuristic)
        // or the pixels must prove it is a standard veins-in-rock sprite.
        let host = if config.is_allowlisted(input.block_id) {
            host_from_id(input.block_id)
        } else {
            match classify_mod_ore_pixels(input.rgba) {
                Ok(h) => h,
                Err(r) => return preserve(r),
            }
        };
        let Some(palette) = extract_ore_vein_palette(input.rgba, host) else {
            return preserve(PreserveReason::UniqueModArtStyle);
        };
        return OreAdaptationDecision::SynthesizeClassic(SynthesisPlan { host, palette });
    }

    // Vanilla ore (or a `minecraft:`-namespaced `*_ore` with no curated
    // palette — extract its colours from the supplied texture if possible).
    let host = host_from_id(input.block_id);
    let palette = match vanilla_palette.or_else(|| extract_ore_vein_palette(input.rgba, host)) {
        Some(p) => p,
        None => return preserve(PreserveReason::UniqueModArtStyle),
    };
    OreAdaptationDecision::SynthesizeClassic(SynthesisPlan { host, palette })
}

// ---------------------------------------------------------------------------
// Procedural synthesis
// ---------------------------------------------------------------------------

/// Deterministic integer pixel hash — no RNG, no clock, stable across runs.
fn px_hash(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x
        .wrapping_mul(0x8da6_b343)
        ^ y.wrapping_mul(0xd816_3841)
        ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 15;
    h = h.wrapping_mul(0x9e37_79b9);
    h ^= h >> 13;
    h
}

/// Synthesizes a clean procedural 16x16 host-rock background. Returns `None`
/// when `host_rgba` is a usable 16x16 frame — in that case the caller's own
/// stone pixels are reused so the ore blends seamlessly with whatever base
/// stone texture is loaded.
fn procedural_host_pixel(host: HostRock, x: u32, y: u32) -> [u8; 3] {
    let n = px_hash(x, y, 0x5eed_cafe);
    let v = (n & 15) as i32 - 7; // -7..=8
    match host {
        HostRock::Stone => {
            let mut g = 126 + v * 2;
            if (n >> 4) & 15 == 0 {
                g -= 16;
            } else if (n >> 8) & 31 == 0 {
                g += 10;
            }
            let g = g.clamp(90, 170) as u8;
            [g, g, g]
        }
        HostRock::Deepslate => {
            let mut g = 72 + v;
            if (n >> 4) & 15 == 0 {
                g -= 12;
            }
            let g = g.clamp(34, 92) as u8;
            // Faint cool bias keeps deepslate visually distinct from stone.
            [g, g, g.saturating_add(4).min(96)]
        }
        HostRock::Netherrack => {
            let mut r = 94 + v * 2;
            if (n >> 4) & 15 == 0 {
                r -= 14;
            }
            let r = r.clamp(60, 160) as u8;
            let gb = (40 + v / 2).clamp(28, 56) as u8;
            [r, gb, gb]
        }
    }
}

/// The unified classic vein silhouette: `(cx, cy, r)` blob seeds producing
/// one cohesive blocky vein shape shared by every ore. Original layout —
/// generated from first principles, not derived from any shipped texture.
const VEIN_BLOBS: [(i32, i32, i32); 6] = [
    (4, 4, 2),
    (10, 5, 2),
    (6, 9, 2),
    (12, 11, 2),
    (3, 13, 1),
    (13, 7, 1),
];

/// Vein layer for a pixel inside a blob: a lit top-left crescent
/// (`dx + dy <= -2`), a recessed bottom-right crescent (`dx + dy >= 2`), and
/// the primary mineral body between them — matching the classic top-left
/// light direction.
const LAYER_ROCK: u8 = 0;
const LAYER_SHADOW: u8 = 1;
const LAYER_PRIMARY: u8 = 2;
const LAYER_HIGHLIGHT: u8 = 3;

/// Returns the vein layer for pixel `(x, y)`, or `LAYER_ROCK`. Rim pixels
/// are occasionally chipped away by the pixel hash so veins stay blocky and
/// irregular instead of perfectly circular.
fn vein_layer(x: u32, y: u32) -> u8 {
    let (x, y) = (x as i32, y as i32);
    for &(cx, cy, r) in &VEIN_BLOBS {
        let (dx, dy) = (x - cx, y - cy);
        let d2 = dx * dx + dy * dy;
        if d2 > r * r {
            continue;
        }
        // Chip ~1/4 of rim pixels for a chunky hand-drawn edge.
        if d2 > (r - 1) * (r - 1) && px_hash(x as u32, y as u32, 0xbead_5eed) & 3 == 0 {
            continue;
        }
        return if dx + dy <= -2 {
            LAYER_HIGHLIGHT
        } else if dx + dy >= 2 {
            LAYER_SHADOW
        } else {
            LAYER_PRIMARY
        };
    }
    LAYER_ROCK
}

/// Synthesizes one deterministic 16x16 RGBA8 classic ore sprite.
///
/// `host_rgba` may supply the caller's actual host-rock texture (16x16
/// RGBA8); when present and full-length it is copied verbatim as the
/// background so the vein blends with the loaded base-pack stone. Otherwise
/// a clean procedural background for `plan.host` is generated. A subtle 1px
/// recessed border ring is applied to rock pixels only — vein pixels keep
/// their palette colour verbatim (the `primary` tone is written exactly as
/// given). All output pixels are fully opaque; output is exactly
/// [`CLASSIC_ORE_BYTES`] bytes.
pub fn synthesize_classic_ore_16x16(
    plan: &SynthesisPlan,
    host_rgba: Option<&[u8]>,
) -> [u8; CLASSIC_ORE_BYTES] {
    let mut out = [0u8; CLASSIC_ORE_BYTES];
    for y in 0..16u32 {
        for x in 0..16u32 {
            let i = (y * 16 + x) as usize * 4;
            let layer = vein_layer(x, y);
            if layer == LAYER_ROCK {
                let mut px = match host_rgba.filter(|h| h.len() >= CLASSIC_ORE_BYTES) {
                    Some(h) => [h[i], h[i + 1], h[i + 2]],
                    None => procedural_host_pixel(plan.host, x, y),
                };
                // Subtle recessed 1px rock edge.
                if x == 0 || y == 0 || x == 15 || y == 15 {
                    for c in &mut px {
                        *c = (*c as u32 * EDGE_SHADE_NUM / EDGE_SHADE_DEN) as u8;
                    }
                }
                out[i..i + 4].copy_from_slice(&[px[0], px[1], px[2], 255]);
            } else {
                let c = match layer {
                    LAYER_HIGHLIGHT => plan.palette.highlight,
                    LAYER_SHADOW => plan.palette.shadow,
                    _ => plan.palette.primary,
                };
                out[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
    }
    out
}
