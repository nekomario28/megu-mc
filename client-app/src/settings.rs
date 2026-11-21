// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Player-facing graphics settings for the native mc-spine client.
//!
//! Design rule (low cognitive load): optimizations with zero player-visible
//! downside are ALWAYS ON under the hood and deliberately have no toggle —
//! see [`BuiltInOptimizations`]. This module only exposes settings that
//! genuinely change what the player sees or that trade something off:
//! preset, view distance, distant horizon LOD, clouds, particles, smooth
//! lighting, entity shadows, FOV, VSync, and an FPS cap.
//!
//! Persistence is human-readable TOML (`mc-spine-client.toml`) plus a
//! [`ClientSettings::import_vanilla_options_txt`] helper so players migrating
//! from the official launcher keep their FOV, render distance, clouds,
//! particles, and VSync automatically. Pure std Rust; zero dependencies.

use crate::lod::LodConfig;

/// Minimum render distance selectable in the UI (chunks).
pub const MIN_VIEW_DISTANCE: u8 = 6;
/// Maximum render distance selectable in the UI (chunks).
pub const MAX_VIEW_DISTANCE: u8 = 32;
/// Default render distance for a fresh profile (chunks).
pub const DEFAULT_VIEW_DISTANCE: u8 = 12;
/// Minimum field of view (degrees).
pub const MIN_FOV: u8 = 30;
/// Maximum field of view (degrees).
pub const MAX_FOV: u8 = 110;
/// Default field of view (degrees).
pub const DEFAULT_FOV: u8 = 70;
/// Minimum FPS cap when one is set (frames/second).
pub const MIN_MAX_FPS: u32 = 30;
/// Maximum FPS cap when one is set (frames/second).
pub const MAX_MAX_FPS: u32 = 260;
/// Distant-horizon radius in chunks for [`DistantHorizon::Medium`].
pub const DISTANT_HORIZON_MEDIUM_CHUNKS: i32 = 64;
/// Distant-horizon radius in chunks for [`DistantHorizon::Far`].
pub const DISTANT_HORIZON_FAR_CHUNKS: i32 = 128;
/// LOD summary memory budget shared by every distant-horizon level.
const LOD_MEMORY_BUDGET_BYTES: usize = 64 * 1024 * 1024;
/// Default filename for [`ClientSettings::to_toml`] output.
pub const SETTINGS_FILE_NAME: &str = "mc-spine-client.toml";

/// Named bundle of player-visible settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityPreset {
    Fast,
    Balanced,
    High,
    /// The player tuned at least one field away from a preset.
    Custom,
}

/// Distance-tiered LOD horizon beyond the full-detail view distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistantHorizon {
    /// No LOD drawing; the world ends at `view_distance`.
    Off,
    /// LOD columns out to 64 chunks.
    Medium,
    /// LOD columns out to 128 chunks.
    Far,
}

/// Cloud drawing mode, matching Vanilla semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudMode {
    Off,
    Fast,
    Fancy,
}

/// Particle emission level, matching Vanilla semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticleLevel {
    Minimal,
    Decreased,
    All,
}

macro_rules! named_option {
    ($ty:ty, $default:expr, $($variant:ident => $name:literal),+ $(,)?) => {
        impl $ty {
            /// Lowercase TOML/options.txt token for this value.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(<$ty>::$variant => $name,)+
                }
            }

            /// Parse a lowercase-or-mixed-case token; `None` when unknown.
            pub fn parse(value: &str) -> Option<Self> {
                match value.to_ascii_lowercase().as_str() {
                    $($name => Some(<$ty>::$variant),)+
                    _ => None,
                }
            }
        }

        impl Default for $ty {
            fn default() -> Self {
                $default
            }
        }
    };
}

named_option!(QualityPreset, QualityPreset::Balanced,
    Fast => "fast", Balanced => "balanced", High => "high", Custom => "custom");
named_option!(DistantHorizon, DistantHorizon::Off,
    Off => "off", Medium => "medium", Far => "far");
named_option!(CloudMode, CloudMode::Fancy,
    Off => "off", Fast => "fast", Fancy => "fancy");
named_option!(ParticleLevel, ParticleLevel::All,
    Minimal => "minimal", Decreased => "decreased", All => "all");

/// Engine optimizations that are always compiled in and always enabled.
///
/// Every flag here is a zero-downside win, so none of them is exposed to the
/// player: the settings UI stays small and there is no confusing way to make
/// the game slower. This is also the coverage matrix that makes the
/// `BUILTIN_SUPERSEDED_PERF_MODS` launcher catalog possible — each named mod
/// is superseded by an original clean-room Rust implementation inside
/// `client-port` / `client-app` / `mc-spine`; no third-party optimization mod
/// code, shaders, assets, or logic is ever copied, linked, or ported (see
/// `specs/client-fork/ux-and-builtin-optimizations.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltInOptimizations {
    /// S1/S2: section-graph occlusion culling (BFS visibility graph x
    /// frustum) — supersedes the Sodium/Embeddium culling pipeline.
    pub graph_occlusion_culling: bool,
    /// S3: per-section direction-bucket backface culling.
    pub backface_bucket_culling: bool,
    /// S4: one multidraw submission per region x facing bucket.
    pub multidraw_region_batching: bool,
    /// S5: frustum-ordered dirty-section rebuilds + incremental translucent
    /// re-sort gated on camera displacement.
    pub frustum_prioritized_rebuilds: bool,
    /// Compact `PalettedSection` storage (supersedes FerriteCore palettes).
    pub compact_palettes: bool,
    /// Deduplicated blockstate/model resources (supersedes memory-dedup mods).
    pub deduplicated_resources: bool,
    /// Batched HUD text/icon submission (supersedes ImmediatelyFast).
    pub fast_hud_batching: bool,
    /// Frustum + occlusion entity culling (supersedes EntityCulling).
    pub entity_culling: bool,
}

impl BuiltInOptimizations {
    /// The only configuration this struct ever takes at runtime.
    pub const ALL_ON: Self = Self {
        graph_occlusion_culling: true,
        backface_bucket_culling: true,
        multidraw_region_batching: true,
        frustum_prioritized_rebuilds: true,
        compact_palettes: true,
        deduplicated_resources: true,
        fast_hud_batching: true,
        entity_culling: true,
    };
}

/// Everything a player can tune. Fields are self-documenting; the TOML file
/// mirrors them 1:1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientSettings {
    pub preset: QualityPreset,
    /// `6..=32` chunks; also the LOD `full_detail_radius` boundary.
    pub view_distance: u8,
    pub distant_horizon: DistantHorizon,
    pub clouds: CloudMode,
    pub particles: ParticleLevel,
    pub smooth_lighting: bool,
    pub entity_shadows: bool,
    /// `30..=110` degrees.
    pub fov: u8,
    pub vsync: bool,
    /// `None` = unlimited; `Some(30..=260)` otherwise.
    pub max_fps: Option<u32>,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self::from_preset(QualityPreset::Balanced)
    }
}

impl ClientSettings {
    /// One named bundle. `Custom` seeds the same defaults as `Balanced`;
    /// the preset merely records that the player then tuned fields.
    pub fn from_preset(preset: QualityPreset) -> Self {
        let mut settings = Self {
            preset,
            view_distance: DEFAULT_VIEW_DISTANCE,
            distant_horizon: DistantHorizon::Off,
            clouds: CloudMode::Fancy,
            particles: ParticleLevel::All,
            smooth_lighting: true,
            entity_shadows: true,
            fov: DEFAULT_FOV,
            vsync: true,
            max_fps: None,
        };
        match preset {
            QualityPreset::Fast => {
                settings.view_distance = 8;
                settings.clouds = CloudMode::Fast;
                settings.particles = ParticleLevel::Decreased;
                settings.smooth_lighting = false;
                settings.entity_shadows = false;
            }
            QualityPreset::High => {
                settings.view_distance = 16;
                settings.distant_horizon = DistantHorizon::Medium;
            }
            QualityPreset::Balanced | QualityPreset::Custom => {}
        }
        settings
    }

    /// Derive the [`LodConfig`] for the chunk-LOD subsystem, or `None` when
    /// `distant_horizon` is `Off`. `view_distance` becomes the full-detail
    /// radius and the horizon maps straight onto the coarse-LOD radius;
    /// fine LOD fills the midpoint so the tier ordering invariant
    /// `full <= fine <= coarse` always holds.
    pub fn to_lod_config(&self) -> Option<LodConfig> {
        let coarse = match self.distant_horizon {
            DistantHorizon::Off => return None,
            DistantHorizon::Medium => DISTANT_HORIZON_MEDIUM_CHUNKS,
            DistantHorizon::Far => DISTANT_HORIZON_FAR_CHUNKS,
        };
        let full = i32::from(self.view_distance);
        Some(LodConfig {
            full_detail_radius: full,
            fine_lod_radius: (full + coarse) / 2,
            coarse_lod_radius: coarse,
            max_memory_bytes: LOD_MEMORY_BUDGET_BYTES,
        })
    }

    /// Engine optimizations active for this session. Always
    /// [`BuiltInOptimizations::ALL_ON`]: these flags are intentionally not
    /// loadable from the user config so players never face a technical
    /// toggle that only makes the game slower when flipped.
    pub fn engine_optimizations(&self) -> BuiltInOptimizations {
        let _ = self;
        BuiltInOptimizations::ALL_ON
    }

    /// Serialize to the human-readable `mc-spine-client.toml` format.
    pub fn to_toml(&self) -> String {
        let max_fps = self
            .max_fps
            .map_or_else(|| "\"unlimited\"".to_string(), |v| v.to_string());
        format!(
            "# mc-spine client settings\n\
             preset = \"{}\"\n\
             view_distance = {}\n\
             distant_horizon = \"{}\"\n\
             clouds = \"{}\"\n\
             particles = \"{}\"\n\
             smooth_lighting = {}\n\
             entity_shadows = {}\n\
             fov = {}\n\
             vsync = {}\n\
             max_fps = {}\n",
            self.preset.as_str(),
            self.view_distance,
            self.distant_horizon.as_str(),
            self.clouds.as_str(),
            self.particles.as_str(),
            self.smooth_lighting,
            self.entity_shadows,
            self.fov,
            self.vsync,
            max_fps,
        )
    }

    /// Parse a `mc-spine-client.toml` document. Returns the resolved
    /// settings plus one warning string per unknown key, invalid value, or
    /// clamped out-of-range value — malformed files degrade gracefully to
    /// defaults instead of failing.
    pub fn from_toml(text: &str) -> (ClientSettings, Vec<String>) {
        let mut settings = ClientSettings::default();
        let mut warnings = Vec::new();
        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() || (line.starts_with('[') && line.ends_with(']')) {
                continue;
            }
            let Some((key, raw_value)) = line.split_once('=') else {
                warnings.push(format!("ignored malformed line '{line}'"));
                continue;
            };
            let key = key.trim();
            let value = unquote(raw_value.trim());
            let invalid = |warnings: &mut Vec<String>| {
                warnings.push(format!("invalid value for '{key}': '{value}'"));
            };
            match key {
                "preset" => match QualityPreset::parse(value) {
                    Some(p) => settings.preset = p,
                    None => invalid(&mut warnings),
                },
                "view_distance" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.view_distance = clamp_u8(
                            v,
                            i64::from(MIN_VIEW_DISTANCE),
                            i64::from(MAX_VIEW_DISTANCE),
                            key,
                            &mut warnings,
                        );
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "distant_horizon" => match DistantHorizon::parse(value) {
                    Some(d) => settings.distant_horizon = d,
                    None => invalid(&mut warnings),
                },
                "clouds" => match CloudMode::parse(value) {
                    Some(c) => settings.clouds = c,
                    None => invalid(&mut warnings),
                },
                "particles" => match ParticleLevel::parse(value) {
                    Some(p) => settings.particles = p,
                    None => invalid(&mut warnings),
                },
                "smooth_lighting" => match parse_bool(value) {
                    Some(b) => settings.smooth_lighting = b,
                    None => invalid(&mut warnings),
                },
                "entity_shadows" => match parse_bool(value) {
                    Some(b) => settings.entity_shadows = b,
                    None => invalid(&mut warnings),
                },
                "fov" => match value.parse::<i64>() {
                    Ok(v) => {
                        settings.fov = clamp_u8(
                            v,
                            i64::from(MIN_FOV),
                            i64::from(MAX_FOV),
                            key,
                            &mut warnings,
                        );
                    }
                    Err(_) => invalid(&mut warnings),
                },
                "vsync" => match parse_bool(value) {
                    Some(b) => settings.vsync = b,
                    None => invalid(&mut warnings),
                },
                "max_fps" => {
                    if value.eq_ignore_ascii_case("unlimited")
                        || value.eq_ignore_ascii_case("none")
                    {
                        settings.max_fps = None;
                    } else {
                        match value.parse::<i64>() {
                            Ok(v) => {
                                settings.max_fps = Some(clamp_u32(
                                    v,
                                    i64::from(MIN_MAX_FPS),
                                    i64::from(MAX_MAX_FPS),
                                    key,
                                    &mut warnings,
                                ));
                            }
                            Err(_) => invalid(&mut warnings),
                        }
                    }
                }
                other => warnings.push(format!("unknown key '{other}' ignored")),
            }
        }
        (settings, warnings)
    }

    /// Import a Vanilla `options.txt` (`key:value` lines) so a migrating
    /// player keeps FOV, render distance, clouds, particles, VSync, smooth
    /// lighting, entity shadows, and the FPS cap. The result is preset
    /// `Custom`; every value is clamped into the supported range.
    ///
    /// Recognized keys: `renderDistance`, `fov` (Vanilla 0..1 slider or
    /// degrees), `particles` (0 all / 1 decreased / 2 minimal),
    /// `renderClouds` (true / fast / false), `enableVsync`, `maxFps`
    /// (260 = unlimited), `ao` (0 off / 1 min / 2 max), `entityShadows`,
    /// `graphicsMode` (0 fast / 1 fancy / 2 fabulous — Fast implies
    /// `smooth_lighting`/`entity_shadows` off unless `ao`/`entityShadows`
    /// override them).
    pub fn import_vanilla_options_txt(text: &str) -> ClientSettings {
        let mut pairs = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once(':') {
                pairs.push((key.trim(), value.trim()));
            }
        }
        let get = |key: &str| {
            pairs
                .iter()
                .rev()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| unquote(v))
        };
        let mut settings = ClientSettings {
            preset: QualityPreset::Custom,
            ..ClientSettings::default()
        };
        // graphicsMode applies first: Fast graphics turns AO and entity
        // shadows off; explicit ao/entityShadows keys then override.
        if let Some(mode) = get("graphicsMode").and_then(parse_i64) {
            if mode == 0 {
                settings.smooth_lighting = false;
                settings.entity_shadows = false;
            }
        }
        if let Some(v) = get("renderDistance").and_then(parse_i64) {
            settings.view_distance = v.clamp(
                i64::from(MIN_VIEW_DISTANCE),
                i64::from(MAX_VIEW_DISTANCE),
            ) as u8;
        }
        if let Some(raw) = get("fov") {
            // Vanilla stores fov as a 0..1 slider fraction (0.5 = 70 deg);
            // accept plain degrees too so hand-edited files still work.
            if let Ok(v) = raw.parse::<f64>() {
                let degrees = if v <= 1.5 { 30.0 + 80.0 * v } else { v };
                settings.fov = (degrees.round() as i64)
                    .clamp(i64::from(MIN_FOV), i64::from(MAX_FOV))
                    as u8;
            }
        }
        if let Some(v) = get("particles").and_then(parse_i64) {
            settings.particles = match v {
                0 => ParticleLevel::All,
                1 => ParticleLevel::Decreased,
                _ => ParticleLevel::Minimal,
            };
        }
        if let Some(v) = get("renderClouds") {
            settings.clouds = if v.eq_ignore_ascii_case("fast") {
                CloudMode::Fast
            } else if v.eq_ignore_ascii_case("true") {
                CloudMode::Fancy
            } else {
                CloudMode::Off
            };
        }
        if let Some(v) = get("enableVsync") {
            settings.vsync = v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = get("maxFps").and_then(parse_i64) {
            // Vanilla's slider tops out at 260, which its UI shows as
            // "Unlimited".
            settings.max_fps = if v >= i64::from(MAX_MAX_FPS) {
                None
            } else {
                Some(v.clamp(i64::from(MIN_MAX_FPS), i64::from(MAX_MAX_FPS)) as u32)
            };
        }
        if let Some(v) = get("ao").and_then(parse_i64) {
            settings.smooth_lighting = v != 0;
        }
        if let Some(v) = get("entityShadows") {
            settings.entity_shadows = v.eq_ignore_ascii_case("true");
        }
        settings
    }
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

fn parse_i64(value: &str) -> Option<i64> {
    value.parse::<i64>().ok()
}

fn clamp_u8(value: i64, min: i64, max: i64, key: &str, warnings: &mut Vec<String>) -> u8 {
    clamp_i64(value, min, max, key, warnings) as u8
}

fn clamp_u32(value: i64, min: i64, max: i64, key: &str, warnings: &mut Vec<String>) -> u32 {
    clamp_i64(value, min, max, key, warnings) as u32
}

fn clamp_i64(value: i64, min: i64, max: i64, key: &str, warnings: &mut Vec<String>) -> i64 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!("'{key}' value {value} clamped to {clamped}"));
    }
    clamped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_presets_fill_every_field_with_valid_lod() {
        for preset in [
            QualityPreset::Fast,
            QualityPreset::Balanced,
            QualityPreset::High,
            QualityPreset::Custom,
        ] {
            let s = ClientSettings::from_preset(preset);
            assert_eq!(s.preset, preset);
            assert!((MIN_VIEW_DISTANCE..=MAX_VIEW_DISTANCE).contains(&s.view_distance));
            assert!((MIN_FOV..=MAX_FOV).contains(&s.fov));
            if let Some(fps) = s.max_fps {
                assert!((MIN_MAX_FPS..=MAX_MAX_FPS).contains(&fps));
            }
            if let Some(lod) = s.to_lod_config() {
                assert!(0 <= lod.full_detail_radius
                    && lod.full_detail_radius <= lod.fine_lod_radius
                    && lod.fine_lod_radius <= lod.coarse_lod_radius);
            }
            assert_eq!(s.engine_optimizations(), BuiltInOptimizations::ALL_ON);
        }
        // Spot-check the intended flavor differences.
        let fast = ClientSettings::from_preset(QualityPreset::Fast);
        assert_eq!(fast.view_distance, 8);
        assert_eq!(fast.clouds, CloudMode::Fast);
        assert!(!fast.smooth_lighting && !fast.entity_shadows);
        let high = ClientSettings::from_preset(QualityPreset::High);
        assert_eq!(high.view_distance, 16);
        assert_eq!(high.distant_horizon, DistantHorizon::Medium);
    }

    #[test]
    fn lod_config_maps_distant_horizon_onto_radii() {
        let mut s = ClientSettings::default();
        assert_eq!(s.to_lod_config(), None);

        s.distant_horizon = DistantHorizon::Medium;
        let lod = s.to_lod_config().unwrap();
        assert_eq!(lod.full_detail_radius, 12);
        assert_eq!(lod.coarse_lod_radius, 64);
        assert_eq!(lod.fine_lod_radius, (12 + 64) / 2);

        s.view_distance = 32;
        s.distant_horizon = DistantHorizon::Far;
        let lod = s.to_lod_config().unwrap();
        assert_eq!(lod.full_detail_radius, 32);
        assert_eq!(lod.coarse_lod_radius, 128);
        // Ordering invariant survives the widest view distance.
        assert!(lod.full_detail_radius <= lod.fine_lod_radius);
        assert!(lod.fine_lod_radius <= lod.coarse_lod_radius);
    }

    #[test]
    fn toml_round_trip_is_lossless() {
        let mut s = ClientSettings {
            preset: QualityPreset::Custom,
            view_distance: 20,
            distant_horizon: DistantHorizon::Far,
            clouds: CloudMode::Off,
            particles: ParticleLevel::Minimal,
            smooth_lighting: false,
            entity_shadows: false,
            fov: 90,
            vsync: false,
            max_fps: Some(144),
        };
        let text = s.to_toml();
        let (back, warnings) = ClientSettings::from_toml(&text);
        assert_eq!(back, s);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        // Unlimited max_fps round-trips through the "unlimited" string.
        s.max_fps = None;
        let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
        assert_eq!(back.max_fps, None);
        assert!(warnings.is_empty());
    }

    #[test]
    fn from_toml_clamps_and_warns_gracefully() {
        let text = "\
            preset = \"ludicrous\"\n\
            view_distance = 99\n\
            fov = 0\n\
            max_fps = 12\n\
            bogus_key = 1\n\
            vsync = maybe\n\
            smooth_lighting = false\n";
        let (s, warnings) = ClientSettings::from_toml(text);
        assert_eq!(s.view_distance, MAX_VIEW_DISTANCE);
        assert_eq!(s.fov, MIN_FOV);
        assert_eq!(s.max_fps, Some(MIN_MAX_FPS));
        assert!(!s.smooth_lighting);
        // preset/vsync stay default on invalid input.
        assert_eq!(s.preset, QualityPreset::Balanced);
        assert!(s.vsync);
        assert!(warnings.iter().any(|w| w.contains("bogus_key")));
        assert!(warnings.iter().any(|w| w.contains("view_distance")));
        assert!(warnings.iter().any(|w| w.contains("'fov'")));
        assert!(warnings.iter().any(|w| w.contains("preset")));
        assert!(warnings.iter().any(|w| w.contains("vsync")));
    }

    #[test]
    fn vanilla_options_txt_import_maps_all_keys() {
        let options = "\
            version:3465\n\
            ao:2\n\
            fov:0.5\n\
            enableVsync:false\n\
            maxFps:144\n\
            renderClouds:\"fast\"\n\
            graphicsMode:1\n\
            particles:1\n\
            entityShadows:false\n\
            renderDistance:16\n";
        let s = ClientSettings::import_vanilla_options_txt(options);
        assert_eq!(s.preset, QualityPreset::Custom);
        assert_eq!(s.view_distance, 16);
        assert_eq!(s.fov, 70);
        assert!(!s.vsync);
        assert_eq!(s.max_fps, Some(144));
        assert_eq!(s.clouds, CloudMode::Fast);
        assert_eq!(s.particles, ParticleLevel::Decreased);
        assert!(s.smooth_lighting); // ao:2 => max smooth lighting
        assert!(!s.entity_shadows);
    }

    #[test]
    fn vanilla_import_clamps_and_handles_unlimited() {
        let s = ClientSettings::import_vanilla_options_txt(
            "renderDistance:64\nfov:1.0\nmaxFps:260\nparticles:2\n\
             renderClouds:false\ngraphicsMode:0\n",
        );
        assert_eq!(s.view_distance, MAX_VIEW_DISTANCE);
        assert_eq!(s.fov, MAX_FOV);
        assert_eq!(s.max_fps, None);
        assert_eq!(s.particles, ParticleLevel::Minimal);
        assert_eq!(s.clouds, CloudMode::Off);
        // Fast graphics disables AO and shadows.
        assert!(!s.smooth_lighting && !s.entity_shadows);
    }

    #[test]
    fn vanilla_import_missing_keys_keep_defaults() {
        let s = ClientSettings::import_vanilla_options_txt("fov:90\n");
        // Values above 1.5 are treated as degrees directly.
        assert_eq!(s.fov, 90);
        assert_eq!(s.view_distance, DEFAULT_VIEW_DISTANCE);
        assert!(s.vsync);
        assert_eq!(s.clouds, CloudMode::Fancy);
    }
}
