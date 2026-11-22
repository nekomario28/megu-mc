// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Client-fork entity interpolation and sky/fog evaluation, pure `std`.
//!
//! Remote entities (mobs, players, boats, minecarts) arrive at 20 TPS while the
//! renderer runs at 60/144/240 FPS. [`EntityInterpolationTable`] keeps the last
//! two server-tick poses per entity and reproduces the vanilla three-tick
//! smooth catch-up (`lerpSteps == 3`), so high-FPS sampling between ticks is a
//! deterministic lerp with no overshoot. Teleports beyond
//! [`TELEPORT_DISTANCE_SQ`] snap instead of streaking.
//!
//! [`evaluate_sky_and_fog`] reproduces the vanilla 1.21.1 overworld sky colour,
//! star brightness, weather darkening and fluid/blindness fog distances, plus
//! smooth biome-fog colour blending across biome borders. All inputs are plain
//! values; the function is total (non-finite inputs are sanitized to zero).

use crate::Error;
use std::collections::HashMap;

/// Upper bound on tracked entities; inserts past it fail with
/// [`Error::OutOfRange`] instead of growing without limit.
pub const MAX_INTERP_ENTITIES: usize = 16_384;
/// Vanilla `lerpSteps`: a fresh position packet catches up over three ticks.
pub const MAX_INTERP_STEPS: i32 = 3;
/// Squared snap distance (16 blocks) above which a move is a teleport.
pub const TELEPORT_DISTANCE_SQ: f64 = 256.0;

/// Update kinds for [`crate::ffi::InterpEntityUpdate`].
pub const INTERP_UPDATE_POSITION: u8 = 0;
pub const INTERP_UPDATE_ROTATION: u8 = 1;
pub const INTERP_UPDATE_SNAP: u8 = 2;
pub const INTERP_UPDATE_REMOVE: u8 = 3;

const TAU: f32 = std::f32::consts::TAU;

/// Wrap degrees into `(-180, 180]`.
pub fn wrap_angle_deg(deg: f32) -> f32 {
    let mut wrapped = deg % 360.0;
    if wrapped <= -180.0 {
        wrapped += 360.0;
    } else if wrapped > 180.0 {
        wrapped -= 360.0;
    }
    wrapped
}

/// Shortest-arc degree interpolation: `350° → 10°` passes through `0°`, never
/// the long way around through `180°`.
pub fn lerp_angle_deg(from: f32, to: f32, t: f32) -> f32 {
    wrap_angle_deg(from + wrap_angle_deg(to - from) * t)
}

/// A fully interpolated entity pose at one render partial tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InterpolatedPose {
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub head_yaw: f32,
}

#[derive(Clone, Copy, Debug)]
struct InterpState {
    prev_pos: [f64; 3],
    curr_pos: [f64; 3],
    prev_yaw: f32,
    curr_yaw: f32,
    prev_pitch: f32,
    curr_pitch: f32,
    prev_head_yaw: f32,
    curr_head_yaw: f32,
    steps: i32,
}

impl InterpState {
    fn snapped(position: [f64; 3], yaw: f32, pitch: f32, head_yaw: f32) -> Self {
        Self {
            prev_pos: position,
            curr_pos: position,
            prev_yaw: yaw,
            curr_yaw: yaw,
            prev_pitch: pitch,
            curr_pitch: pitch,
            prev_head_yaw: head_yaw,
            curr_head_yaw: head_yaw,
            steps: 0,
        }
    }

    fn snap(&mut self, position: [f64; 3], yaw: f32, pitch: f32, head_yaw: f32) {
        *self = Self::snapped(position, yaw, pitch, head_yaw);
    }
}

fn finite_vec3(v: [f64; 3]) -> bool {
    v.iter().all(|c| c.is_finite())
}

fn finite_angles(yaw: f32, pitch: f32, head_yaw: f32) -> bool {
    yaw.is_finite() && pitch.is_finite() && head_yaw.is_finite()
}

/// Per-entity 20 TPS → high-FPS interpolation state.
///
/// Positions arrive once per server tick via [`Self::update_position`]; each
/// call shifts `curr` into `prev` and arms a [`MAX_INTERP_STEPS`]-tick catch-up
/// unless the move is a teleport, which snaps. Call [`Self::advance_tick`]
/// once per client tick and [`Self::sample`] once per rendered frame.
pub struct EntityInterpolationTable {
    states: HashMap<i32, InterpState>,
}

impl Default for EntityInterpolationTable {
    fn default() -> Self {
        Self::new()
    }
}

impl EntityInterpolationTable {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn contains(&self, entity_id: i32) -> bool {
        self.states.contains_key(&entity_id)
    }

    pub fn clear(&mut self) {
        self.states.clear();
    }

    /// Remove an entity on despawn; returns whether it was tracked.
    pub fn remove_entity(&mut self, entity_id: i32) -> bool {
        self.states.remove(&entity_id).is_some()
    }

    fn ensure_capacity(&self) -> Result<(), Error> {
        if self.states.len() >= MAX_INTERP_ENTITIES {
            Err(Error::OutOfRange)
        } else {
            Ok(())
        }
    }

    /// Insert or teleport-snap an entity. Also the spawn path: a missing id is
    /// created already converged so the first frame does not streak.
    pub fn snap_entity(
        &mut self,
        entity_id: i32,
        position: [f64; 3],
        yaw: f32,
        pitch: f32,
        head_yaw: f32,
    ) -> Result<(), Error> {
        if !finite_vec3(position) || !finite_angles(yaw, pitch, head_yaw) {
            return Err(Error::InvalidArgument);
        }
        if let Some(state) = self.states.get_mut(&entity_id) {
            state.snap(position, yaw, pitch, head_yaw);
        } else {
            self.ensure_capacity()?;
            self.states
                .insert(entity_id, InterpState::snapped(position, yaw, pitch, head_yaw));
        }
        Ok(())
    }

    /// One 20 TPS position packet. Moves beyond [`TELEPORT_DISTANCE_SQ`] from
    /// the current target snap; shorter moves arm the three-tick catch-up.
    /// An unknown id is spawned snapped.
    pub fn update_position(&mut self, entity_id: i32, position: [f64; 3]) -> Result<(), Error> {
        if !finite_vec3(position) {
            return Err(Error::InvalidArgument);
        }
        match self.states.get_mut(&entity_id) {
            Some(state) => {
                let d = [
                    position[0] - state.curr_pos[0],
                    position[1] - state.curr_pos[1],
                    position[2] - state.curr_pos[2],
                ];
                if d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > TELEPORT_DISTANCE_SQ {
                    state.curr_pos = position;
                    state.prev_pos = position;
                    state.steps = 0;
                } else {
                    state.prev_pos = state.curr_pos;
                    state.curr_pos = position;
                    state.steps = MAX_INTERP_STEPS;
                }
            }
            None => {
                self.ensure_capacity()?;
                self.states.insert(
                    entity_id,
                    InterpState::snapped(position, 0.0, 0.0, 0.0),
                );
            }
        }
        Ok(())
    }

    /// One 20 TPS rotation packet (yaw, pitch and head yaw together, as the
    /// vanilla move/teleport packets carry them). Fails with
    /// [`Error::NotFound`] for an untracked entity.
    pub fn update_rotation(
        &mut self,
        entity_id: i32,
        yaw: f32,
        pitch: f32,
        head_yaw: f32,
    ) -> Result<(), Error> {
        if !finite_angles(yaw, pitch, head_yaw) {
            return Err(Error::InvalidArgument);
        }
        let state = self.states.get_mut(&entity_id).ok_or(Error::NotFound)?;
        state.prev_yaw = state.curr_yaw;
        state.curr_yaw = yaw;
        state.prev_pitch = state.curr_pitch;
        state.curr_pitch = pitch;
        state.prev_head_yaw = state.curr_head_yaw;
        state.curr_head_yaw = head_yaw;
        state.steps = MAX_INTERP_STEPS;
        Ok(())
    }

    /// Advance the catch-up window once per client tick.
    pub fn advance_tick(&mut self) {
        for state in self.states.values_mut() {
            if state.steps > 0 {
                state.steps -= 1;
            }
        }
    }

    /// Interpolated pose at render `partial_tick` (`0.0..=1.0` within the tick).
    /// Returns `None` for an untracked or removed entity.
    pub fn sample(&self, entity_id: i32, partial_tick: f32) -> Option<InterpolatedPose> {
        let state = self.states.get(&entity_id)?;
        let t = if partial_tick.is_finite() {
            partial_tick.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Catch-up progress: `steps == 3` just after a packet, `0` once
        // converged. `(3 - steps + t) / 3` is continuous across tick
        // boundaries and lands exactly on `curr` when `steps` reaches 0.
        let elapsed = (MAX_INTERP_STEPS - state.steps).max(0) as f32;
        let f = if state.steps <= 0 {
            1.0
        } else {
            ((elapsed + t) / MAX_INTERP_STEPS as f32).min(1.0)
        };
        let mut position = [0.0; 3];
        for (i, coordinate) in position.iter_mut().enumerate() {
            *coordinate =
                state.prev_pos[i] + (state.curr_pos[i] - state.prev_pos[i]) * f64::from(f);
        }
        Some(InterpolatedPose {
            position,
            yaw: lerp_angle_deg(state.prev_yaw, state.curr_yaw, f),
            pitch: lerp_angle_deg(state.prev_pitch, state.curr_pitch, f),
            head_yaw: lerp_angle_deg(state.prev_head_yaw, state.curr_head_yaw, f),
        })
    }
}

/// Inputs for [`evaluate_sky_and_fog`]. All scalar fields are sanitized:
/// non-finite values read as `0.0` and levels clamp to `0.0..=1.0`.
#[derive(Clone, Copy, Debug)]
pub struct SkyFogInput {
    /// World daytime in ticks; reduced modulo 24,000 (0 = sunrise, 6,000 =
    /// noon, 12,000 = sunset, 18,000 = midnight).
    pub day_time: i64,
    /// Rain level `0.0..=1.0`.
    pub rain_level: f32,
    /// Thunder level `0.0..=1.0`.
    pub thunder_level: f32,
    /// Render distance in blocks; sets the clear-air fog distances.
    pub view_distance: f32,
    /// Biome fog colour on this side of a biome border.
    pub biome_fog_color: [f32; 3],
    /// Biome fog colour on the far side of the border.
    pub biome_fog_color_next: [f32; 3],
    /// Border blend `0.0..=1.0`: `0` = `biome_fog_color`, `1` = `*_next`.
    pub biome_blend: f32,
    pub underwater: bool,
    pub in_lava: bool,
    pub powder_snow: bool,
    /// Blindness effect strength `0.0..=1.0`.
    pub blindness: f32,
    /// Darkness effect strength `0.0..=1.0`.
    pub darkness: f32,
}

/// Result of [`evaluate_sky_and_fog`]. Colours are linear-ish `0.0..=1.0` RGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyFogOutput {
    /// `day_time mod 24000 / 24000`, in `0.0..1.0`.
    pub celestial_angle: f32,
    /// `0.0` by day, ramps up through dusk to `1.0` at midnight.
    pub star_brightness: f32,
    pub sky_rgb: [f32; 3],
    /// Horizon/fog colour after biome blending and weather.
    pub fog_rgb: [f32; 3],
    pub fog_start: f32,
    pub fog_end: f32,
}

fn clean(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn level(v: f32) -> f32 {
    clean(v).clamp(0.0, 1.0)
}

fn clean_rgb(rgb: [f32; 3]) -> [f32; 3] {
    [clean(rgb[0]), clean(rgb[1]), clean(rgb[2])]
}

fn lerp_rgb(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale_rgb(rgb: [f32; 3], k: f32) -> [f32; 3] {
    [rgb[0] * k, rgb[1] * k, rgb[2] * k]
}

/// Evaluate the vanilla 1.21.1 overworld sky and fog state for one frame.
///
/// Celestial convention: `celestial_angle = (day_time mod 24000) / 24000` with
/// sun elevation `sin(2π · celestial_angle)` — `+1` at noon (6,000), `-1` at
/// midnight (18,000), `0` at sunrise/sunset. Fog distances follow the vanilla
/// client ordering lava > powder snow > water > air, then blindness/darkness
/// scale the end distance.
pub fn evaluate_sky_and_fog(input: &SkyFogInput) -> SkyFogOutput {
    let celestial_angle = input.day_time.rem_euclid(24_000) as f32 / 24_000.0;
    let sun = (celestial_angle * TAU).sin();
    // Full daylight once the sun is well up; a soft dawn/dusk ramp below.
    let daylight = ((sun + 0.15) / 0.55).clamp(0.0, 1.0);

    let rain = level(input.rain_level);
    let thunder = level(input.thunder_level);
    let overcast = (rain * 0.8 + thunder * 0.4).min(1.0);
    let dim = (1.0 - 0.45 * rain - 0.30 * thunder).max(0.15);

    let day_sky = [0.47, 0.65, 1.0];
    let night_sky = [0.012, 0.018, 0.055];
    let overcast_sky = [0.30, 0.33, 0.38];
    let mut sky_rgb = lerp_rgb(night_sky, day_sky, daylight);
    sky_rgb = lerp_rgb(sky_rgb, overcast_sky, overcast * 0.75);
    sky_rgb = scale_rgb(sky_rgb, dim);

    // Stars only when the sun is genuinely down.
    let star_brightness = ((-sun - 0.2) / 0.5).clamp(0.0, 1.0);

    // Smooth biome-fog blend, then daylight/weather modulation.
    let biome = lerp_rgb(
        clean_rgb(input.biome_fog_color),
        clean_rgb(input.biome_fog_color_next),
        level(input.biome_blend),
    );
    let mut fog_rgb = scale_rgb(biome, 0.30 + 0.70 * daylight);
    fog_rgb = lerp_rgb(fog_rgb, [0.42, 0.45, 0.50], overcast * 0.6);
    fog_rgb = scale_rgb(fog_rgb, dim);

    let view = clean(input.view_distance).clamp(1.0, 2048.0);
    let (mut fog_start, mut fog_end) = if input.in_lava {
        (-8.0, 64.0)
    } else if input.powder_snow {
        (-8.0, 32.0)
    } else if input.underwater {
        (-8.0, 96.0)
    } else {
        (view * 0.75, view)
    };

    let blind = level(input.blindness);
    let dark = level(input.darkness);
    let fog_scale = ((1.0 - 0.8 * blind) * (1.0 - 0.9 * dark)).max(0.05);
    fog_end *= fog_scale;
    fog_start = fog_start.min(fog_end * 0.9);

    SkyFogOutput {
        celestial_angle,
        star_brightness,
        sky_rgb,
        fog_rgb,
        fog_start,
        fog_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angle_lerp_takes_the_short_arc() {
        assert_eq!(wrap_angle_deg(350.0), -10.0);
        assert_eq!(wrap_angle_deg(-190.0), 170.0);
        assert_eq!(lerp_angle_deg(350.0, 10.0, 0.5), 0.0);
        assert_eq!(lerp_angle_deg(10.0, 350.0, 0.5), 0.0);
        assert_eq!(lerp_angle_deg(0.0, 90.0, 0.25), 22.5);
    }

    #[test]
    fn catchup_converges_in_three_ticks_without_overshoot() {
        let mut table = EntityInterpolationTable::new();
        table.update_position(1, [0.0, 64.0, 0.0]).unwrap();
        table
            .update_rotation(1, 10.0, 0.0, 350.0)
            .unwrap();
        table.advance_tick();
        table.update_position(1, [3.0, 64.0, 0.0]).unwrap();
        table.update_rotation(1, 20.0, 0.0, 10.0).unwrap();

        // Fresh packet: steps == 3, so the first third of the tick barely
        // leaves `prev`; the pose reaches `curr` exactly as steps hits 0.
        let first = table.sample(1, 0.0).unwrap();
        assert_eq!(first.position[0], 0.0);
        let mut last_x = first.position[0];
        for _ in 0..3 {
            for step in 0..=4 {
                let t = step as f32 / 4.0;
                let pose = table.sample(1, t).unwrap();
                assert!(pose.position[0] >= last_x - 1e-9);
                assert!(pose.position[0] <= 3.0 + 1e-9);
                last_x = pose.position[0];
            }
            table.advance_tick();
        }
        let done = table.sample(1, 0.5).unwrap();
        assert_eq!(done.position, [3.0, 64.0, 0.0]);
        assert_eq!(done.yaw, 20.0);
        // Head yaw wrapped the short way: 350 -> 10 through 0.
        assert_eq!(done.head_yaw, 10.0);
    }

    #[test]
    fn teleport_and_snap_jump_immediately() {
        let mut table = EntityInterpolationTable::new();
        table.snap_entity(7, [0.0; 3], 0.0, 0.0, 0.0).unwrap();
        // 17 blocks > 16: snaps, no three-tick streak.
        table.update_position(7, [17.0, 0.0, 0.0]).unwrap();
        let pose = table.sample(7, 0.0).unwrap();
        assert_eq!(pose.position, [17.0, 0.0, 0.0]);
        table.snap_entity(7, [100.0, 0.0, 0.0], 90.0, 0.0, 0.0).unwrap();
        assert_eq!(table.sample(7, 0.5).unwrap().position[0], 100.0);
        assert!(table.remove_entity(7));
        assert!(table.sample(7, 0.0).is_none());
    }

    #[test]
    fn sky_noon_midnight_and_weather() {
        let base = SkyFogInput {
            day_time: 6_000,
            rain_level: 0.0,
            thunder_level: 0.0,
            view_distance: 128.0,
            biome_fog_color: [0.6, 0.7, 0.8],
            biome_fog_color_next: [0.6, 0.7, 0.8],
            biome_blend: 0.0,
            underwater: false,
            in_lava: false,
            powder_snow: false,
            blindness: 0.0,
            darkness: 0.0,
        };
        let noon = evaluate_sky_and_fog(&base);
        assert_eq!(noon.celestial_angle, 0.25);
        assert_eq!(noon.star_brightness, 0.0);
        assert!(noon.sky_rgb[2] > noon.sky_rgb[0]);
        assert!(noon.sky_rgb.iter().sum::<f32>() > 1.5);

        let midnight = evaluate_sky_and_fog(&SkyFogInput {
            day_time: 18_000,
            ..base
        });
        assert_eq!(midnight.celestial_angle, 0.75);
        assert!(midnight.star_brightness > 0.0);
        assert!(midnight.sky_rgb.iter().sum::<f32>() < 0.2);

        let storm = evaluate_sky_and_fog(&SkyFogInput {
            day_time: 6_000,
            rain_level: 0.0,
            thunder_level: 1.0,
            ..base
        });
        assert!(storm.sky_rgb.iter().sum::<f32>() < noon.sky_rgb.iter().sum::<f32>());

        // Negative day_time wraps into the same day.
        assert_eq!(
            evaluate_sky_and_fog(&SkyFogInput {
                day_time: -6_000,
                ..base
            })
            .celestial_angle,
            0.75
        );
    }

    #[test]
    fn fog_distances_and_biome_blend() {
        let base = SkyFogInput {
            day_time: 6_000,
            rain_level: 0.0,
            thunder_level: 0.0,
            view_distance: 128.0,
            biome_fog_color: [0.2, 0.4, 0.6],
            biome_fog_color_next: [0.8, 0.6, 0.4],
            biome_blend: 0.0,
            underwater: false,
            in_lava: false,
            powder_snow: false,
            blindness: 0.0,
            darkness: 0.0,
        };
        let air = evaluate_sky_and_fog(&base);
        assert_eq!(air.fog_end, 128.0);

        let water = evaluate_sky_and_fog(&SkyFogInput {
            underwater: true,
            ..base
        });
        assert!(water.fog_end < air.fog_end && water.fog_end > 0.0);
        let lava = evaluate_sky_and_fog(&SkyFogInput {
            in_lava: true,
            ..base
        });
        assert!(lava.fog_end < water.fog_end);
        let snow = evaluate_sky_and_fog(&SkyFogInput {
            powder_snow: true,
            ..base
        });
        assert!(snow.fog_end < water.fog_end);

        let blind = evaluate_sky_and_fog(&SkyFogInput {
            blindness: 1.0,
            ..base
        });
        assert!(blind.fog_end < air.fog_end);
        let dark = evaluate_sky_and_fog(&SkyFogInput {
            darkness: 1.0,
            ..base
        });
        assert!(dark.fog_end < blind.fog_end);

        // Biome border blend is linear in the fog colour.
        let near = evaluate_sky_and_fog(&base).fog_rgb;
        let far = evaluate_sky_and_fog(&SkyFogInput {
            biome_blend: 1.0,
            ..base
        })
        .fog_rgb;
        let mid = evaluate_sky_and_fog(&SkyFogInput {
            biome_blend: 0.5,
            ..base
        })
        .fog_rgb;
        for i in 0..3 {
            assert!((mid[i] - (near[i] + far[i]) * 0.5).abs() < 1e-6);
        }
    }
}
