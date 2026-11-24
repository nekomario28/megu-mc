// SERVER-DIRECTIONAL-SHADOW-GAMEPLAY: the opt-in server-side companion to the
// client's GPU sun. `compute_sun_direction` is the exact formula
// `client-port/src/builtin_shadow_wgsl.rs` feeds the shadow map, so the voxel
// ray below lands on the same lit/shadow boundary the client renders — a
// zombie sheltering on the wrong side of a wall's real shadow stops burning
// and hostiles can spawn there.
//
// Everything here is pure and allocation-free: the world reaches in through
// the `opacity_at` callback, so this module compiles identically as
// `megu_server_files::directional_shadow` and inside the `megu` binary
// (spliced in by `include!`; `#[cfg(test)]` below strips away in production
// expansion, keeping the module-layer graph untouched).
//
// The switch defaults to OFF: with `DirectionalShadowConfig::default()` every
// query answers the unmodified vanilla inputs and the opacity callback is
// never invoked.

/// A normalized sun vector, with noon at tick 6000 and midnight at 18000.
/// The moon is visible along the opposite ray. Mirrors
/// `builtin_shadow_wgsl::compute_sun_direction` bit for bit.
pub fn compute_sun_direction(world_time_ticks: u64) -> [f32; 3] {
    let phase = (world_time_ticks % 24_000) as f32 * (std::f32::consts::TAU / 24_000.0);
    [phase.cos(), phase.sin(), 0.0]
}

/// Tuning for the opt-in directional shadow gameplay. The defaults keep every
/// consumer at strict Vanilla 1.21.1 behaviour until `enabled` is set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalShadowConfig {
    /// Master switch. `false` makes `sample_directional_shadow` a zero-cost
    /// pass-through: no ray is cast and the vanilla answer is returned.
    pub enabled: bool,
    /// Amanatides-Woo traversal bound, clamped to `4..=64` at query time so a
    /// single probe stays bounded O(1) microseconds.
    pub max_ray_steps: u32,
    /// Sky-light ceiling inside a fully shadowed cell (a wall or cliff
    /// shadow), clamped to `0..=15`.
    pub shadow_sky_light_cap: u8,
    /// Let shadowed cells feed the spawn-light checks (hostiles spawn in real
    /// shadows, surface creatures stop spawning there).
    pub affect_mob_spawning: bool,
    /// Let the ray decide whether the sun actually reaches an undead mob.
    pub affect_undead_burn: bool,
    /// Let direct sun scale the crop growth roll (torch-lit greenhouses are
    /// exempt through their block light).
    pub affect_crop_growth: bool,
    /// Let dynamic emitters (held luminous items, burning entities, dropped
    /// luminous items, modded emitters) lift the unified gameplay light.
    pub affect_dynamic_lights: bool,
}

impl Default for DirectionalShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_ray_steps: 24,
            shadow_sky_light_cap: 4,
            affect_mob_spawning: true,
            affect_undead_burn: true,
            affect_crop_growth: true,
            affect_dynamic_lights: true,
        }
    }
}

/// One probe's answer: how much direct sun reaches the cell after the voxel
/// ray, and the sky light gameplay should see there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowSample {
    /// `1.0` full direct sun, `0.0` fully shadowed (or night/rain),
    /// intermediate values under a partial canopy such as leaves.
    pub direct_sun_fraction: f32,
    /// Accumulated opacity the ray crossed, `0..=15`.
    pub occluded_opacity: u8,
    /// `direct_sun_fraction < 0.35`: the point where a wall's shadow counts as
    /// real shade for burning and spawning.
    pub in_shadow: bool,
    /// The sky light this cell reports to gameplay, `0..=15`.
    pub effective_sky_light: u8,
}

/// Sample the directional shadow above `origin` at `world_time_ticks`.
///
/// `vanilla_sky_light` is the sky-light nibble the vanilla rules already
/// computed for the cell; `raining` mirrors the weather flag the callers hold.
/// `opacity_at(x, y, z)` answers the block's light opacity (`0` transparent,
/// `15` fully opaque) — leaves and other partial blocks attenuate the ray
/// progressively instead of blocking it outright.
///
/// With `cfg.enabled == false` the callback is never invoked.
pub fn sample_directional_shadow<F>(
    origin: [i32; 3],
    world_time_ticks: u64,
    vanilla_sky_light: u8,
    raining: bool,
    cfg: &DirectionalShadowConfig,
    mut opacity_at: F,
) -> ShadowSample
where
    F: FnMut(i32, i32, i32) -> u8,
{
    let vanilla_sky_light = vanilla_sky_light.min(15);
    let shadowed_sky = vanilla_sky_light.min(cfg.shadow_sky_light_cap.min(15));
    if !cfg.enabled {
        return ShadowSample {
            direct_sun_fraction: if vanilla_sky_light >= 12 && !raining {
                1.0
            } else {
                0.0
            },
            occluded_opacity: 0,
            in_shadow: false,
            effective_sky_light: vanilla_sky_light,
        };
    }
    let sun_dir = compute_sun_direction(world_time_ticks);
    // Sun at or below the horizon, or a cell the vanilla engine already
    // reports as fully dark: no direct sun can arrive.
    if sun_dir[1] <= 0.02 || vanilla_sky_light == 0 {
        return ShadowSample {
            direct_sun_fraction: 0.0,
            occluded_opacity: 0,
            in_shadow: true,
            effective_sky_light: shadowed_sky,
        };
    }
    let max_steps = cfg.max_ray_steps.clamp(4, 64);

    // Amanatides-Woo traversal from the origin cell's centre along the sun
    // vector. The origin cell itself is skipped: a block occupying the sample
    // cell does not shadow itself.
    let start = [
        origin[0] as f32 + 0.5,
        origin[1] as f32 + 0.5,
        origin[2] as f32 + 0.5,
    ];
    let step = [
        if sun_dir[0] > 0.0 {
            1
        } else if sun_dir[0] < 0.0 {
            -1
        } else {
            0
        },
        if sun_dir[1] > 0.0 {
            1
        } else if sun_dir[1] < 0.0 {
            -1
        } else {
            0
        },
        if sun_dir[2] > 0.0 {
            1
        } else if sun_dir[2] < 0.0 {
            -1
        } else {
            0
        },
    ];
    let first_t = |pos: f32, vox: i32, d: f32, step: i32| -> f32 {
        if step > 0 {
            (vox as f32 + 1.0 - pos) / d
        } else if step < 0 {
            (vox as f32 - pos) / d
        } else {
            f32::INFINITY
        }
    };
    let mut t_max = [
        first_t(start[0], origin[0], sun_dir[0], step[0]),
        first_t(start[1], origin[1], sun_dir[1], step[1]),
        first_t(start[2], origin[2], sun_dir[2], step[2]),
    ];
    let t_delta = [
        if step[0] != 0 {
            sun_dir[0].abs().recip()
        } else {
            f32::INFINITY
        },
        if step[1] != 0 {
            sun_dir[1].abs().recip()
        } else {
            f32::INFINITY
        },
        if step[2] != 0 {
            sun_dir[2].abs().recip()
        } else {
            f32::INFINITY
        },
    ];
    let mut vox = origin;
    let mut occluded_opacity = 0u8;
    for _ in 0..max_steps {
        if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
            vox[0] += step[0];
            t_max[0] += t_delta[0];
        } else if t_max[1] <= t_max[2] {
            vox[1] += step[1];
            t_max[1] += t_delta[1];
        } else {
            vox[2] += step[2];
            t_max[2] += t_delta[2];
        }
        // Above the build ceiling nothing occludes; a saturated ray is opaque.
        if vox[1] >= 320 {
            break;
        }
        let op = opacity_at(vox[0], vox[1], vox[2]).min(15);
        occluded_opacity = occluded_opacity.saturating_add(op).min(15);
        if occluded_opacity >= 15 {
            break;
        }
    }

    let direct_sun_fraction = ((1.0 - f32::from(occluded_opacity) / 15.0).clamp(0.0, 1.0))
        * if raining { 0.2 } else { 1.0 };
    // Lerp the shadow cap (no direct sun) up to the vanilla value (full sun).
    let effective = f32::from(shadowed_sky)
        + (f32::from(vanilla_sky_light) - f32::from(shadowed_sky)) * direct_sun_fraction;
    ShadowSample {
        direct_sun_fraction,
        occluded_opacity,
        in_shadow: direct_sun_fraction < 0.35,
        effective_sky_light: effective.round().clamp(0.0, 15.0) as u8,
    }
}

/// `Monster.isSunBurnTick` after the directional shadow: an undead that
/// vanilla would ignite keeps burning only while the sun ray actually reaches
/// it. With the feature off, or the undead lane off, the vanilla answer
/// passes through unchanged.
pub fn shadow_allows_undead_burn(
    vanilla_burns: bool,
    sample: &ShadowSample,
    cfg: &DirectionalShadowConfig,
) -> bool {
    if !cfg.enabled || !cfg.affect_undead_burn {
        return vanilla_burns;
    }
    vanilla_burns && !sample.in_shadow
}

/// Growth-rate scale for the crop random tick. `1.0` in full sun or wherever
/// block light already feeds the crop (a torch-lit greenhouse); inside a real
/// directional shadow the value floors at `0.45` — shade slows crops, it does
/// not stop them.
pub fn shadow_crop_growth_multiplier(
    sample: &ShadowSample,
    block_light: u8,
    cfg: &DirectionalShadowConfig,
) -> f32 {
    if !cfg.enabled || !cfg.affect_crop_growth || block_light >= 9 {
        return 1.0;
    }
    0.45 + 0.55 * sample.direct_sun_fraction.clamp(0.0, 1.0)
}

/// A light source that lives outside the block grid: a held torch or lantern,
/// a burning entity, a dropped luminous item, or any modded emitter the
/// caller wants gameplay to see. `pos` is the continuous position of the
/// emitter, `light_level` its emission `0..=15`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DynamicGameplayLight {
    pub pos: [f32; 3],
    pub light_level: u8,
}

/// The strongest dynamic light reaching `origin`'s cell centre:
/// `light_level - ceil(distance)` per emitter, maximised over the set,
/// clamped `0..=15`.
pub fn dynamic_light_level_at(origin: [i32; 3], lights: &[DynamicGameplayLight]) -> u8 {
    let center = [
        origin[0] as f32 + 0.5,
        origin[1] as f32 + 0.5,
        origin[2] as f32 + 0.5,
    ];
    let mut best = 0u8;
    for light in lights {
        let level = light.light_level.min(15);
        if level == 0 {
            continue;
        }
        let d = [
            light.pos[0] - center[0],
            light.pos[1] - center[1],
            light.pos[2] - center[2],
        ];
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        best = best.max(level.saturating_sub(dist.ceil() as u8));
    }
    best
}

/// The one gameplay light value every mechanic reads: shadow-modulated sky
/// light, static block light, and — while the feature and its dynamic lane
/// are on — the attenuated dynamic emitters, combined with `max` and clamped
/// `0..=15`.
pub fn effective_gameplay_light(
    sample: &ShadowSample,
    static_block_light: u8,
    dynamic_lights: &[DynamicGameplayLight],
    origin: [i32; 3],
    cfg: &DirectionalShadowConfig,
) -> u8 {
    let mut light = sample.effective_sky_light.max(static_block_light.min(15));
    if cfg.enabled && cfg.affect_dynamic_lights {
        light = light.max(dynamic_light_level_at(origin, dynamic_lights));
    }
    light.min(15)
}

#[cfg(test)]
mod directional_shadow_tests {
    use super::*;

    fn enabled() -> DirectionalShadowConfig {
        DirectionalShadowConfig {
            enabled: true,
            ..DirectionalShadowConfig::default()
        }
    }

    /// The opacity probe that must never run while the switch is off.
    struct CountingOpacity {
        calls: usize,
        map: std::collections::HashMap<(i32, i32, i32), u8>,
    }

    impl CountingOpacity {
        fn new() -> Self {
            Self {
                calls: 0,
                map: std::collections::HashMap::new(),
            }
        }

        fn at(&mut self, x: i32, y: i32, z: i32) -> u8 {
            self.calls += 1;
            self.map.get(&(x, y, z)).copied().unwrap_or(0)
        }
    }

    #[test]
    fn disabled_config_is_a_zero_raycast_passthrough() {
        let cfg = DirectionalShadowConfig::default();
        let mut probe = CountingOpacity::new();
        probe.map.insert((0, 65, 0), 15);
        let sample = sample_directional_shadow(
            [0, 64, 0],
            6_000,
            15,
            false,
            &cfg,
            |x, y, z| probe.at(x, y, z),
        );
        assert_eq!(probe.calls, 0, "disabled probe still cast a ray");
        assert_eq!(sample.effective_sky_light, 15);
        assert!(!sample.in_shadow);
        assert!(sample.direct_sun_fraction >= 1.0 - f32::EPSILON);

        // The same disabled config never casts at night either.
        let mut probe = CountingOpacity::new();
        let night = sample_directional_shadow(
            [0, 64, 0],
            18_000,
            0,
            false,
            &cfg,
            |x, y, z| probe.at(x, y, z),
        );
        assert_eq!(probe.calls, 0);
        assert_eq!(night.effective_sky_light, 0);
        assert!(night.direct_sun_fraction <= 0.0);
    }

    #[test]
    fn morning_sun_east_wall_shadows_and_noon_sun_burns() {
        let cfg = enabled();
        // A tall solid wall three blocks east of the probe, from y=64 to 80.
        let wall = |x: i32, y: i32, z: i32| -> u8 {
            if x == 3 && z == 0 && (64..=80).contains(&y) {
                15
            } else {
                0
            }
        };
        // tick 1000: the sun is low in the east (+X), so the wall's shadow
        // falls west onto the probe cell.
        let morning = sample_directional_shadow([0, 64, 0], 1_000, 15, false, &cfg, wall);
        assert!(morning.in_shadow, "morning sun: wall at +X must shade the probe: {morning:?}");
        assert!(morning.effective_sky_light <= 4);
        assert!(!shadow_allows_undead_burn(true, &morning, &cfg));

        // tick 6000: noon, the sun is straight up; the same cell is fully lit.
        let noon = sample_directional_shadow([0, 64, 0], 6_000, 15, false, &cfg, wall);
        assert!(!noon.in_shadow, "noon sun overhead must light the probe: {noon:?}");
        assert!(noon.direct_sun_fraction > 0.99);
        assert_eq!(noon.effective_sky_light, 15);
        assert!(shadow_allows_undead_burn(true, &noon, &cfg));
    }

    #[test]
    fn afternoon_sun_west_wall_shadows_east_side_only() {
        let cfg = enabled();
        let west_wall = |x: i32, y: i32, z: i32| -> u8 {
            if x == -3 && z == 0 && (64..=80).contains(&y) {
                15
            } else {
                0
            }
        };
        // tick 11000: the sun is low in the west (-X); the western wall's
        // shadow falls east onto the probe.
        let afternoon = sample_directional_shadow([0, 64, 0], 11_000, 15, false, &cfg, west_wall);
        assert!(afternoon.in_shadow, "afternoon sun: wall at -X must shade the probe: {afternoon:?}");

        // An eastern wall cannot shade anything under a western sun.
        let east_wall = |x: i32, y: i32, z: i32| -> u8 {
            if x == 3 && z == 0 && (64..=80).contains(&y) {
                15
            } else {
                0
            }
        };
        let unshaded = sample_directional_shadow([0, 64, 0], 11_000, 15, false, &cfg, east_wall);
        assert!(!unshaded.in_shadow, "eastern wall cannot shadow under a western sun: {unshaded:?}");
    }

    #[test]
    fn leaf_canopy_attenuates_and_solid_blocks_occlude() {
        let cfg = enabled();
        // Two leaf layers (opacity 1 each) above the probe under the noon sun.
        let leaves = |x: i32, y: i32, z: i32| -> u8 {
            if x == 0 && z == 0 && (66..=68).contains(&y) {
                1
            } else {
                0
            }
        };
        let dappled = sample_directional_shadow([0, 64, 0], 6_000, 15, false, &cfg, leaves);
        assert!(dappled.direct_sun_fraction > 0.0 && dappled.direct_sun_fraction < 1.0);
        assert!(!dappled.in_shadow, "two leaf layers shade but do not hide the sun");

        // One solid stone block ends the ray outright.
        let stone = |x: i32, y: i32, z: i32| -> u8 {
            if x == 0 && y == 66 && z == 0 {
                15
            } else {
                0
            }
        };
        let roofed = sample_directional_shadow([0, 64, 0], 6_000, 15, false, &cfg, stone);
        assert_eq!(roofed.occluded_opacity, 15);
        assert!(roofed.direct_sun_fraction <= 0.0);
        assert!(roofed.in_shadow);
    }

    #[test]
    fn crop_growth_multiplier_tracks_sun_and_torchlight() {
        let cfg = enabled();
        let sunny = ShadowSample {
            direct_sun_fraction: 1.0,
            occluded_opacity: 0,
            in_shadow: false,
            effective_sky_light: 15,
        };
        assert!(shadow_crop_growth_multiplier(&sunny, 0, &cfg) >= 1.0 - f32::EPSILON);

        let shaded = ShadowSample {
            direct_sun_fraction: 0.0,
            occluded_opacity: 15,
            in_shadow: true,
            effective_sky_light: 4,
        };
        let mult = shadow_crop_growth_multiplier(&shaded, 0, &cfg);
        assert!(
            (0.45 - f32::EPSILON..1.0).contains(&mult),
            "shadow multiplier {mult}"
        );

        // Torch-lit greenhouses ignore the shadow entirely.
        assert!(shadow_crop_growth_multiplier(&shaded, 9, &cfg) >= 1.0 - f32::EPSILON);

        // Off means off.
        assert!(shadow_crop_growth_multiplier(&shaded, 0, &DirectionalShadowConfig::default()) >= 1.0 - f32::EPSILON);
    }

    #[test]
    fn dynamic_held_torch_lifts_gameplay_light_inside_a_shadow() {
        let cfg = enabled();
        // Deep inside a cliff shadow the sun contribution is gone entirely.
        let sample = ShadowSample {
            direct_sun_fraction: 0.0,
            occluded_opacity: 15,
            in_shadow: true,
            effective_sky_light: 0,
        };
        let lights = [DynamicGameplayLight {
            pos: [2.5, 64.5, 0.5],
            light_level: 14,
        }];
        // Distance 2 from the cell centre: 14 - ceil(2) = 12, enough to keep
        // hostile spawns out of a torch-carrier's ring even in true shade.
        let light = effective_gameplay_light(&sample, 0, &lights, [0, 64, 0], &cfg);
        assert_eq!(light, 12);
        assert_eq!(dynamic_light_level_at([0, 64, 0], &lights), 12);

        // The dynamic lane off, or the master switch off, drops back to the
        // shadowed vanilla answer.
        let no_dynamic = DirectionalShadowConfig {
            affect_dynamic_lights: false,
            ..cfg
        };
        assert_eq!(effective_gameplay_light(&sample, 0, &lights, [0, 64, 0], &no_dynamic), 0);
        assert_eq!(
            effective_gameplay_light(&sample, 0, &lights, [0, 64, 0], &DirectionalShadowConfig::default()),
            0
        );
    }
}
