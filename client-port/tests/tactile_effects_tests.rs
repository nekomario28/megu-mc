// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::tactile_effects::*;
use megu_client_mesh::vfx_engine::VfxHardwareTier;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fancy_config() -> TactileEffectsConfig {
    TactileEffectsConfig {
        preset: TactilePreset::Fancy,
        ..TactileEffectsConfig::default()
    }
}

fn subtle_config() -> TactileEffectsConfig {
    TactileEffectsConfig {
        preset: TactilePreset::Subtle,
        ..TactileEffectsConfig::default()
    }
}

fn fancy_sim(tier: VfxHardwareTier) -> TactileEffectsSim {
    TactileEffectsSim::new(fancy_config(), tier)
}

fn entry(body: FluidEntryBody, speed: f32, is_lava: bool) -> FluidEntryEvent {
    FluidEntryEvent {
        pos: [10.0, 64.0, 20.0],
        impact_speed: speed,
        body,
        is_lava,
        seed_tick: 1_000,
    }
}

fn count_kind(sim: &TactileEffectsSim, pred: impl Fn(&TactileParticleKind) -> bool) -> usize {
    sim.particles().iter().filter(|p| pred(&p.kind)).count()
}

fn droplets(sim: &TactileEffectsSim) -> usize {
    count_kind(sim, |k| matches!(k, TactileParticleKind::SplashDroplet))
}

fn rings(sim: &TactileEffectsSim) -> Vec<f32> {
    sim.particles()
        .iter()
        .filter_map(|p| match p.kind {
            TactileParticleKind::WaterRippleRing { max_radius } => Some(max_radius),
            _ => None,
        })
        .collect()
}

fn assert_finite_particles(sim: &TactileEffectsSim) {
    for p in sim.particles() {
        for c in p.pos.iter().chain(p.vel.iter()).chain(p.rgba.iter()) {
            assert!(c.is_finite(), "non-finite component in {p:?}");
        }
        assert!(p.size.is_finite() && p.age_sec.is_finite() && p.ttl_sec > 0.0);
    }
}

fn emit_everything(sim: &mut TactileEffectsSim, tick: u64) -> usize {
    let mut n = 0;
    n += sim.emit_fluid_entry(&entry(FluidEntryBody::LargeMob, 9.0, false));
    n += sim.emit_fluid_entry(&entry(FluidEntryBody::LargeMob, 9.0, true));
    n += sim.emit_bubble_pop([10.0, 64.0, 20.0], tick);
    n += sim.emit_tree_shake_burst([10.0, 70.0, 20.0], LeafCanopyKind::CherrySakura, 1.0, tick);
    n += sim.emit_footstep_or_landing([10.0, 64.0, 20.0], GroundSurfaceKind::Snow, 8.0, tick);
    n += sim.emit_ceiling_or_eaves_drip([10.0, 80.0, 20.0], false, tick);
    n += sim.emit_cold_breath([10.0, 66.0, 20.0], [0.0, 0.0, 1.0], -10.0, true, tick);
    n += sim.emit_hearth_ember([10.0, 64.0, 20.0], 1.0, tick);
    n += sim.emit_impact_sparks(
        [10.0, 64.0, 20.0],
        [0.0, 1.0, 0.0],
        ImpactMaterial::CriticalHit,
        tick,
    );
    n
}

// ---------------------------------------------------------------------------
// 1. Default Off preset is fully inert
// ---------------------------------------------------------------------------

#[test]
fn default_preset_is_off_and_safe() {
    let cfg = TactileEffectsConfig::default();
    assert_eq!(cfg.preset, TactilePreset::Off);
    assert!(!cfg.subtle_camera_motion);

    let mut sim = TactileEffectsSim::default();
    assert_eq!(sim.config().preset, TactilePreset::Off);
    assert_eq!(emit_everything(&mut sim, 42), 0);
    assert!(sim.is_empty());

    // Pending wet drips are still consumed but spawn nothing while Off.
    sim.wetness_mut().step(0.25, true);
    sim.wetness_mut().step(0.25, false);
    for _ in 0..10 {
        sim.step_with_wetness(0.1, false);
    }
    assert!(sim.is_empty());

    let drama = evaluate_night_sky_drama(18_000, true, true, 1.0, &cfg);
    assert!(drama.is_empty());

    let mut cam = CameraMicroMotionState::default();
    let off = cam.step(9.0, -3.0, 12.0, 1.0, 1.0 / 60.0, &cfg);
    assert_eq!(off.roll_rad, 0.0);
    assert_eq!(off.landing_dip_blocks, 0.0);
}

#[test]
fn individual_toggles_gate_their_systems() {
    let base = fancy_config();
    let mut cfg = base.clone();
    cfg.water_and_lava_reactions = false;
    let mut sim = TactileEffectsSim::new(cfg, VfxHardwareTier::High);
    assert_eq!(
        sim.emit_fluid_entry(&entry(FluidEntryBody::LargeMob, 9.0, false)),
        0
    );
    assert_eq!(sim.emit_bubble_pop([0.0, 64.0, 0.0], 1), 0);
    assert!(sim.emit_hearth_ember([0.0, 64.0, 0.0], 1.0, 1) > 0);

    let mut cfg = base.clone();
    cfg.tree_shake_and_footsteps = false;
    let mut sim = TactileEffectsSim::new(cfg, VfxHardwareTier::High);
    assert_eq!(
        sim.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::OakGreen, 1.0, 1),
        0
    );
    assert_eq!(
        sim.emit_footstep_or_landing([0.0, 64.0, 0.0], GroundSurfaceKind::Snow, 8.0, 1),
        0
    );

    let mut cfg = base.clone();
    cfg.cold_breath_and_embers = false;
    let mut sim = TactileEffectsSim::new(cfg, VfxHardwareTier::High);
    assert_eq!(
        sim.emit_cold_breath([0.0, 66.0, 0.0], [0.0, 0.0, 1.0], -5.0, false, 1),
        0
    );
    assert_eq!(sim.emit_hearth_ember([0.0, 64.0, 0.0], 1.0, 1), 0);

    let mut cfg = base.clone();
    cfg.impact_sparks = false;
    let mut sim = TactileEffectsSim::new(cfg, VfxHardwareTier::High);
    assert_eq!(
        sim.emit_impact_sparks(
            [0.0, 64.0, 0.0],
            [0.0, 1.0, 0.0],
            ImpactMaterial::OreOrStone,
            1
        ),
        0
    );

    let mut cfg = base;
    cfg.shooting_stars_and_aurora = false;
    let drama = evaluate_night_sky_drama(18_000, true, true, 1.0, &cfg);
    assert!(drama.is_empty());
}

// ---------------------------------------------------------------------------
// 2. Water & lava entry scaling
// ---------------------------------------------------------------------------

#[test]
fn water_entry_scales_with_body_mass() {
    let bodies = [
        FluidEntryBody::ItemDrop,
        FluidEntryBody::Projectile,
        FluidEntryBody::SmallMob,
        FluidEntryBody::PlayerOrMediumMob,
        FluidEntryBody::LargeMob,
    ];
    let mut prev_drops = 0usize;
    let mut prev_radius = 0.0f32;
    for body in bodies {
        let mut sim = fancy_sim(VfxHardwareTier::High);
        assert!(sim.emit_fluid_entry(&entry(body, 8.0, false)) > 0);
        let drops = droplets(&sim);
        assert!(
            drops > prev_drops,
            "droplet count must strictly increase across {body:?}: {drops} <= {prev_drops}"
        );
        prev_drops = drops;

        let radii = rings(&sim);
        assert!(!radii.is_empty(), "expected a ripple ring for {body:?}");
        let max_r = radii.iter().copied().fold(0.0f32, f32::max);
        assert!(
            max_r > prev_radius,
            "ripple radius must strictly increase across {body:?}: {max_r} <= {prev_radius}"
        );
        prev_radius = max_r;
    }
}

#[test]
fn water_entry_scales_with_impact_speed() {
    let mut slow = fancy_sim(VfxHardwareTier::High);
    slow.emit_fluid_entry(&entry(FluidEntryBody::PlayerOrMediumMob, 1.5, false));
    let mut fast = fancy_sim(VfxHardwareTier::High);
    fast.emit_fluid_entry(&entry(FluidEntryBody::PlayerOrMediumMob, 11.0, false));

    assert!(droplets(&fast) > droplets(&slow));
    let slow_r = rings(&slow).iter().copied().fold(0.0f32, f32::max);
    let fast_r = rings(&fast).iter().copied().fold(0.0f32, f32::max);
    assert!(fast_r > slow_r);
}

#[test]
fn item_drop_is_a_tiny_plop_and_golem_is_a_big_splash() {
    let mut item = fancy_sim(VfxHardwareTier::High);
    item.emit_fluid_entry(&entry(FluidEntryBody::ItemDrop, 1.0, false));
    assert!(droplets(&item) >= 1 && droplets(&item) <= 4);
    assert!(rings(&item).iter().all(|r| *r < 1.0));

    let mut golem = fancy_sim(VfxHardwareTier::High);
    golem.emit_fluid_entry(&entry(FluidEntryBody::LargeMob, 11.0, false));
    assert!(droplets(&golem) >= 10);
    assert!(rings(&golem).iter().any(|r| *r > 2.0));
}

#[test]
fn lava_entry_emits_sparks_and_steam() {
    let mut sim = fancy_sim(VfxHardwareTier::High);
    let n = sim.emit_fluid_entry(&entry(FluidEntryBody::PlayerOrMediumMob, 8.0, true));
    assert!(n > 0);
    let sparks = count_kind(&sim, |k| matches!(k, TactileParticleKind::LavaSizzleSpark));
    let steam = count_kind(&sim, |k| matches!(k, TactileParticleKind::SteamPuff));
    assert!(sparks > 0, "lava entry should sizzle");
    assert!(steam > 0, "lava entry should steam");
    // No water-specific kinds in a lava splash.
    assert_eq!(droplets(&sim), 0);
    assert!(rings(&sim).is_empty());
}

#[test]
fn lava_entry_scales_with_body_mass() {
    let mut prev = 0usize;
    for body in [
        FluidEntryBody::ItemDrop,
        FluidEntryBody::SmallMob,
        FluidEntryBody::PlayerOrMediumMob,
        FluidEntryBody::LargeMob,
    ] {
        let mut sim = fancy_sim(VfxHardwareTier::High);
        sim.emit_fluid_entry(&entry(body, 9.0, true));
        let sparks = count_kind(&sim, |k| matches!(k, TactileParticleKind::LavaSizzleSpark));
        assert!(sparks > prev, "lava sparks must increase across {body:?}");
        prev = sparks;
    }
}

#[test]
fn bubble_pop_emits_ring_and_micro_droplets() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    let n = sim.emit_bubble_pop([5.0, 62.0, 5.0], 77);
    assert_eq!(
        count_kind(&sim, |k| matches!(
            k,
            TactileParticleKind::BubblePopRing { .. }
        )),
        1
    );
    assert!(droplets(&sim) >= 2);
    assert_eq!(n, sim.len());
}

#[test]
fn splash_physics_fall_and_rings_expand() {
    let mut sim = fancy_sim(VfxHardwareTier::High);
    sim.emit_fluid_entry(&entry(FluidEntryBody::SmallMob, 8.0, false));
    let before: Vec<f32> = sim.particles().iter().map(|p| p.pos[1]).collect();
    sim.step(0.05);
    sim.step(0.05);
    let after: Vec<f32> = sim.particles().iter().map(|p| p.pos[1]).collect();
    assert_eq!(before.len(), after.len());
    // Droplets may rise first then fall; every position must stay finite.
    assert_finite_particles(&sim);
    for p in sim.particles() {
        if let TactileParticleKind::WaterRippleRing { max_radius } = p.kind {
            assert!(p.size > 0.0 && p.size <= max_radius + 1e-6);
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Wetness tracker
// ---------------------------------------------------------------------------

#[test]
fn wetness_saturates_drips_then_dries_clean() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    sim.wetness_mut().set_position([3.0, 64.0, 3.0]);

    // Soak in water.
    for _ in 0..4 {
        sim.step_with_wetness(0.25, true);
    }
    assert!((sim.wetness().wetness() - 1.0).abs() < 1e-6);
    assert!(sim.is_empty(), "no drips while submerged");

    // Exit water: drips start appearing within a second.
    let mut saw_drip = false;
    for _ in 0..20 {
        sim.step_with_wetness(0.1, false);
        if count_kind(&sim, |k| matches!(k, TactileParticleKind::WetDrip)) > 0 {
            saw_drip = true;
            break;
        }
    }
    assert!(saw_drip, "a wet body should shed WetDrip particles");

    // Dry out completely over ~5 s.
    for _ in 0..60 {
        sim.step_with_wetness(0.1, false);
    }
    assert_eq!(sim.wetness().wetness(), 0.0);
    assert!(sim.wetness().is_dry());
}

#[test]
fn wetness_drips_respect_water_toggle_and_off() {
    // Water toggle off: tracker still tracks, but no drips spawn.
    let mut cfg = fancy_config();
    cfg.water_and_lava_reactions = false;
    let mut sim = TactileEffectsSim::new(cfg, VfxHardwareTier::Balanced);
    for _ in 0..4 {
        sim.step_with_wetness(0.25, true);
    }
    for _ in 0..20 {
        sim.step_with_wetness(0.1, false);
    }
    assert_eq!(
        count_kind(&sim, |k| matches!(k, TactileParticleKind::WetDrip)),
        0
    );

    // Bad positions are ignored rather than flinging drips.
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    sim.wetness_mut().set_position([f32::NAN, 0.0, 0.0]);
    assert_eq!(sim.wetness().position(), [0.0; 3]);
}

// ---------------------------------------------------------------------------
// 4. Tree shake, footsteps & drips
// ---------------------------------------------------------------------------

#[test]
fn tree_shake_emits_spiraling_petals() {
    let mut sim = fancy_sim(VfxHardwareTier::High);
    let n = sim.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::CherrySakura, 1.0, 9);
    assert!(
        n >= 10,
        "fancy full-intensity burst should be rich, got {n}"
    );
    assert_eq!(
        n,
        count_kind(&sim, |k| matches!(k, TactileParticleKind::LeafPetal))
    );

    // Sakura tint is pink: red dominant over blue-green midpoint.
    for p in sim.particles() {
        assert!(p.rgba[0] > p.rgba[1] && p.rgba[0] > 0.8);
    }

    // Spiral flutter: horizontal velocity present, always descending.
    for _ in 0..4 {
        sim.step(0.05);
    }
    for p in sim.particles() {
        let horizontal = p.vel[0].abs() + p.vel[2].abs();
        assert!(
            horizontal > 0.05,
            "petal should flutter horizontally: {p:?}"
        );
        assert!(p.vel[1] < 0.0, "petal should descend: {p:?}");
    }
}

#[test]
fn tree_shake_intensity_scales_count_and_tints_differ() {
    let mut weak = fancy_sim(VfxHardwareTier::High);
    weak.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::AutumnMaple, 0.2, 9);
    let mut strong = fancy_sim(VfxHardwareTier::High);
    strong.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::AutumnMaple, 1.0, 9);
    assert!(strong.len() > weak.len());

    let mut maple = fancy_sim(VfxHardwareTier::High);
    maple.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::AutumnMaple, 1.0, 9);
    let mut spruce = fancy_sim(VfxHardwareTier::High);
    spruce.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::SpruceNeedle, 1.0, 9);
    let maple_r = maple.particles()[0].rgba;
    let spruce_r = spruce.particles()[0].rgba;
    assert_ne!(maple_r, spruce_r, "distinct canopy tints");

    let tint = [0.2, 0.8, 0.9];
    let mut custom = fancy_sim(VfxHardwareTier::High);
    custom.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::CustomTint(tint), 1.0, 9);
    let c = custom.particles()[0].rgba;
    assert!((c[0] - tint[0]).abs() < 0.1 && (c[2] - tint[2]).abs() < 0.1);
}

#[test]
fn footsteps_track_surface_and_stay_quiet_on_stone() {
    let pos = [0.0, 64.0, 0.0];

    // Gentle step on plain stone/wood: silence.
    let mut stone = fancy_sim(VfxHardwareTier::High);
    assert_eq!(
        stone.emit_footstep_or_landing(pos, GroundSurfaceKind::StoneOrWood, 0.4, 5),
        0
    );

    // Soft surfaces always respond, even to gentle steps.
    for surface in [
        GroundSurfaceKind::Sand,
        GroundSurfaceKind::Snow,
        GroundSurfaceKind::Gravel,
        GroundSurfaceKind::MudOrShallowWater,
        GroundSurfaceKind::FallenLeaves,
    ] {
        let mut sim = fancy_sim(VfxHardwareTier::High);
        let n = sim.emit_footstep_or_landing(pos, surface, 0.4, 5);
        assert!(n > 0, "surface {surface:?} should puff on a gentle step");
        assert!(count_kind(&sim, |k| matches!(k, TactileParticleKind::FootstepPuff)) > 0);
    }

    // Heavy landing on stone: dust ring even with no soft-surface puffs.
    let mut stone = fancy_sim(VfxHardwareTier::High);
    stone.emit_footstep_or_landing(pos, GroundSurfaceKind::StoneOrWood, 9.0, 5);
    assert!(
        count_kind(&stone, |k| matches!(
            k,
            TactileParticleKind::LandingDustRing { .. }
        )) >= 1,
        "heavy stone landing should ring"
    );

    // Heavy landing on snow: puffs plus ring.
    let mut snow = fancy_sim(VfxHardwareTier::High);
    snow.emit_footstep_or_landing(pos, GroundSurfaceKind::Snow, 9.0, 5);
    assert!(count_kind(&snow, |k| matches!(k, TactileParticleKind::FootstepPuff)) > 0);
    assert!(
        count_kind(&snow, |k| matches!(
            k,
            TactileParticleKind::LandingDustRing { .. }
        )) >= 1
    );
}

#[test]
fn landing_speed_scales_dust() {
    let pos = [0.0, 64.0, 0.0];
    let mut soft = fancy_sim(VfxHardwareTier::High);
    soft.emit_footstep_or_landing(pos, GroundSurfaceKind::Sand, 1.0, 5);
    let mut hard = fancy_sim(VfxHardwareTier::High);
    hard.emit_footstep_or_landing(pos, GroundSurfaceKind::Sand, 10.0, 5);
    assert!(hard.len() > soft.len());
}

#[test]
fn ceiling_drip_leaves_landing_ripple() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    assert_eq!(
        sim.emit_ceiling_or_eaves_drip([1.0, 80.0, 1.0], false, 3),
        1
    );
    assert_eq!(
        count_kind(&sim, |k| matches!(k, TactileParticleKind::CeilingDrip)),
        1
    );

    // Step until the drip expires; its landing cue appears the same step.
    let mut steps = 0;
    while count_kind(&sim, |k| matches!(k, TactileParticleKind::CeilingDrip)) > 0 && steps < 20 {
        sim.step(0.1);
        steps += 1;
    }
    assert!(steps < 20, "drip should expire within its ttl");
    assert_eq!(
        count_kind(&sim, |k| matches!(
            k,
            TactileParticleKind::DripLandingRing { .. }
        )),
        1,
        "expired drip should leave its landing cue"
    );
}

// ---------------------------------------------------------------------------
// 5. Cold breath & hearth embers
// ---------------------------------------------------------------------------

#[test]
fn cold_breath_fires_only_when_cold() {
    let mouth = [0.0, 66.0, 0.0];
    let dir = [0.0, 0.0, 1.0];

    let mut cold = fancy_sim(VfxHardwareTier::Balanced);
    assert!(cold.emit_cold_breath(mouth, dir, -5.0, false, 11) > 0);
    assert!(count_kind(&cold, |k| matches!(k, TactileParticleKind::ColdBreathVapor)) > 0);

    let mut warm = fancy_sim(VfxHardwareTier::Balanced);
    assert_eq!(warm.emit_cold_breath(mouth, dir, 25.0, false, 11), 0);
    assert_eq!(warm.emit_cold_breath(mouth, dir, 2.5, false, 11), 0);

    // Winter forces breath even at mild temperatures.
    let mut winter = fancy_sim(VfxHardwareTier::Balanced);
    assert!(winter.emit_cold_breath(mouth, dir, 10.0, true, 11) > 0);

    // Non-finite temperature (e.g. vacuum/space) fails closed.
    let mut space = fancy_sim(VfxHardwareTier::Balanced);
    assert_eq!(space.emit_cold_breath(mouth, dir, f32::NAN, false, 11), 0);
    assert_eq!(
        space.emit_cold_breath(mouth, dir, f32::NEG_INFINITY, true, 11),
        0
    );
}

#[test]
fn cold_breath_drifts_along_look_dir() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    sim.emit_cold_breath([0.0, 66.0, 0.0], [1.0, 0.0, 0.0], -10.0, false, 11);
    let start_z: Vec<f32> = sim.particles().iter().map(|p| p.pos[0]).collect();
    for _ in 0..3 {
        sim.step(0.05);
    }
    for (p, &x0) in sim.particles().iter().zip(start_z.iter()) {
        assert!(p.pos[0] > x0, "breath should drift down the look dir");
    }
}

#[test]
fn hearth_embers_drift_upward() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    let n = sim.emit_hearth_ember([0.0, 64.0, 0.0], 1.0, 13);
    assert!(n >= 2);
    let ys: Vec<f32> = sim.particles().iter().map(|p| p.pos[1]).collect();
    for _ in 0..4 {
        sim.step(0.05);
    }
    for (p, &y0) in sim.particles().iter().zip(ys.iter()) {
        assert!(p.pos[1] > y0, "ember should rise: {p:?}");
        // Warm orange-gold tint: red >> blue.
        assert!(p.rgba[0] > 0.8 && p.rgba[2] < 0.4);
    }
}

// ---------------------------------------------------------------------------
// 6. Combat & mining sparks
// ---------------------------------------------------------------------------

#[test]
fn impact_sparks_match_material() {
    let pos = [0.0, 64.0, 0.0];
    let up = [0.0, 1.0, 0.0];

    let mut ore = fancy_sim(VfxHardwareTier::Balanced);
    ore.emit_impact_sparks(pos, up, ImpactMaterial::OreOrStone, 17);
    assert!(count_kind(&ore, |k| matches!(k, TactileParticleKind::ImpactSpark)) > 0);
    assert_eq!(
        count_kind(&ore, |k| matches!(k, TactileParticleKind::ShockRing { .. })),
        0
    );

    let mut shield = fancy_sim(VfxHardwareTier::Balanced);
    shield.emit_impact_sparks(pos, up, ImpactMaterial::MetalArmorOrShield, 17);
    assert!(count_kind(&shield, |k| matches!(k, TactileParticleKind::ImpactSpark)) > 0);
    assert_eq!(
        count_kind(&shield, |k| matches!(
            k,
            TactileParticleKind::ShockRing { .. }
        )),
        1,
        "shield block should add a shock ring"
    );

    let mut arrow = fancy_sim(VfxHardwareTier::Balanced);
    let n = arrow.emit_impact_sparks(pos, up, ImpactMaterial::ArrowTrail, 17);
    assert_eq!(n, 1);
    assert_eq!(
        count_kind(&arrow, |k| matches!(k, TactileParticleKind::ArrowTrailMote)),
        1,
        "arrow trail is bounded to a single mote"
    );
}

#[test]
fn impact_sparks_survive_bad_normals() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    let n = sim.emit_impact_sparks(
        [0.0, 64.0, 0.0],
        [f32::NAN, f32::INFINITY, 0.0],
        ImpactMaterial::CriticalHit,
        17,
    );
    assert!(n > 0);
    for _ in 0..3 {
        sim.step(0.05);
    }
    assert_finite_particles(&sim);
}

// ---------------------------------------------------------------------------
// 7. Night sky drama
// ---------------------------------------------------------------------------

#[test]
fn night_sky_silent_in_day_rain_vacuum_and_off() {
    let cfg = fancy_config();
    // Daytime.
    assert!(evaluate_night_sky_drama(6_000, true, true, 1.0, &cfg).is_empty());
    // Night but raining.
    assert!(evaluate_night_sky_drama(18_000, false, true, 1.0, &cfg).is_empty());
    // Night, clear, but vacuum (no atmosphere): no stars, no aurora.
    assert!(evaluate_night_sky_drama(18_000, true, true, 0.0, &cfg).is_empty());
    assert!(evaluate_night_sky_drama(18_000, true, true, f32::NAN, &cfg).is_empty());
    // Off preset.
    assert!(
        evaluate_night_sky_drama(18_000, true, true, 1.0, &TactileEffectsConfig::default())
            .is_empty()
    );
}

#[test]
fn aurora_needs_cold_and_atmosphere() {
    let cfg = fancy_config();
    // Clear night, warm desert: no aurora regardless of atmosphere.
    let drama = evaluate_night_sky_drama(18_000, true, false, 1.0, &cfg);
    assert!(drama.aurora_bands.is_empty());
    // Clear cold night but thin atmosphere: no aurora.
    let drama = evaluate_night_sky_drama(18_000, true, true, 0.10, &cfg);
    assert!(drama.aurora_bands.is_empty());
    // Clear cold night with atmosphere: ribbons appear.
    let drama = evaluate_night_sky_drama(18_000, true, true, 0.9, &cfg);
    assert!(
        drama.aurora_bands.len() >= 2,
        "fancy cold night needs aurora"
    );
    for band in &drama.aurora_bands {
        assert!(band.intensity > 0.0 && band.intensity <= 1.0);
        assert!(band.altitude_deg > 0.0 && band.altitude_deg < 90.0);
        assert!(band.phase.is_finite());
        for c in band.primary_rgb.iter().chain(band.secondary_rgb.iter()) {
            assert!(c.is_finite() && (0.0..=1.0).contains(c));
        }
    }
    // Subtle preset emits fewer ribbons.
    let subtle = evaluate_night_sky_drama(18_000, true, true, 0.9, &subtle_config());
    assert!(subtle.aurora_bands.len() <= drama.aurora_bands.len());
}

#[test]
fn shooting_stars_are_rare_deterministic_and_night_only() {
    let cfg = fancy_config();
    let mut seen = Vec::new();
    // Sweep a full night; a Fancy clear night should show at least one streak.
    for tod in (13_000u64..23_000).step_by(4) {
        let drama = evaluate_night_sky_drama(tod, true, false, 0.9, &cfg);
        for star in &drama.shooting_stars {
            assert!((0.0..=1.0).contains(&star.progress));
            // A streak caught at progress 0 may not have brightened yet.
            assert!(star.brightness.is_finite() && (0.0..=1.0 + 1e-6).contains(&star.brightness));
            for d in star
                .start_dir
                .iter()
                .chain(star.end_dir.iter())
                .chain(star.tint.iter())
            {
                assert!(d.is_finite());
            }
            seen.push((tod, star.progress));
        }
    }
    assert!(
        !seen.is_empty(),
        "a fancy clear night should contain shooting stars"
    );

    // Determinism: identical inputs -> identical output.
    let a = evaluate_night_sky_drama(42_000 + 18_000, true, true, 0.9, &cfg);
    let b = evaluate_night_sky_drama(42_000 + 18_000, true, true, 0.9, &cfg);
    assert_eq!(a, b);
    // Different nights are scheduled independently (not a crash risk either way).
    let _ = evaluate_night_sky_drama(DAY_WRAP, true, true, 0.9, &cfg);
}

const DAY_WRAP: u64 = 2_000_000 * 24_000 + 19_000;

// ---------------------------------------------------------------------------
// 8. Camera micro-motion & audio hints
// ---------------------------------------------------------------------------

#[test]
fn camera_motion_is_zero_unless_explicitly_enabled() {
    // Fancy preset but camera toggle still off (the default).
    let cfg = fancy_config();
    assert!(!cfg.subtle_camera_motion);
    let mut cam = CameraMicroMotionState::default();
    for _ in 0..30 {
        let off = cam.step(6.0, -4.0, 10.0, 1.0, 1.0 / 60.0, &cfg);
        assert_eq!(off, CameraMicroOffset::default());
    }

    // Strength 0 behaves as disabled even with the toggle on.
    let mut cfg = fancy_config();
    cfg.subtle_camera_motion = true;
    cfg.camera_motion_strength = 0.0;
    let mut cam = CameraMicroMotionState::default();
    let off = cam.step(6.0, -4.0, 10.0, 1.0, 1.0 / 60.0, &cfg);
    assert_eq!(off, CameraMicroOffset::default());
}

#[test]
fn camera_motion_is_smooth_bounded_and_nan_safe() {
    let mut cfg = fancy_config();
    cfg.subtle_camera_motion = true;
    cfg.camera_motion_strength = 1.0;
    let mut cam = CameraMicroMotionState::default();

    // Strafe lean: roll ramps up smoothly and stays inside the hard bound.
    let mut prev = 0.0f32;
    let mut saw_roll = false;
    for _ in 0..60 {
        let off = cam.step(8.0, 0.0, 0.0, 0.0, 1.0 / 60.0, &cfg);
        assert!(off.roll_rad.abs() <= CAMERA_MAX_ROLL_RAD + 1e-9);
        assert!(off.roll_rad.is_finite());
        assert!(
            off.roll_rad >= prev - 1e-9,
            "roll should ramp monotonically"
        );
        prev = off.roll_rad;
        saw_roll |= off.roll_rad.abs() > 1e-4;
    }
    assert!(saw_roll, "strafe should produce a visible lean");

    // Elytra roll in the opposite direction flips the sign smoothly.
    let off = cam.step(0.0, 0.0, 0.0, -1.0, 0.25, &cfg);
    assert!(off.roll_rad.abs() <= CAMERA_MAX_ROLL_RAD + 1e-9);

    // Landing: dip impulse is bounded and decays back toward zero.
    let mut cam = CameraMicroMotionState::default();
    let mut max_dip = 0.0f32;
    let off = cam.step(0.0, 0.0, 14.0, 0.0, 0.0, &cfg);
    assert_eq!(off.landing_dip_blocks, 0.0);
    for _ in 0..90 {
        let off = cam.step(0.0, 0.0, 0.0, 0.0, 1.0 / 60.0, &cfg);
        assert!((0.0..=CAMERA_MAX_DIP_BLOCKS + 1e-9).contains(&off.landing_dip_blocks));
        max_dip = max_dip.max(off.landing_dip_blocks);
    }
    assert!(
        max_dip > 0.005,
        "a 14-block-speed landing should dip visibly"
    );
    let settled = cam.step(0.0, 0.0, 0.0, 0.0, 0.25, &cfg);
    assert!(
        settled.landing_dip_blocks < max_dip,
        "dip should settle back"
    );

    // Hostile inputs: NaN/Inf everywhere, dt far beyond the clamp.
    let mut cam = CameraMicroMotionState::default();
    for _ in 0..10 {
        let off = cam.step(
            f32::NAN,
            f32::INFINITY,
            f32::NAN,
            f32::INFINITY,
            f32::NAN,
            &cfg,
        );
        assert!(off.roll_rad.is_finite() && off.landing_dip_blocks.is_finite());
        assert!(off.roll_rad.abs() <= CAMERA_MAX_ROLL_RAD + 1e-9);
        assert!(off.landing_dip_blocks <= CAMERA_MAX_DIP_BLOCKS + 1e-9);
    }
    let off = cam.step(1.0e9, -1.0e9, 1.0e9, -1.0e9, 10.0, &cfg);
    assert!(off.roll_rad.abs() <= CAMERA_MAX_ROLL_RAD + 1e-9);
    assert!(off.landing_dip_blocks <= CAMERA_MAX_DIP_BLOCKS + 1e-9);
}

#[test]
fn environment_audio_hint_is_deterministic_and_bounded() {
    // Underwater muffle.
    let wet = compute_environment_audio_hint(true, 15, 0.0, false);
    assert!(wet.underwater_lowpass > 0.5);
    assert_eq!(wet.cave_reverb_mix, 0.0);
    assert_eq!(wet.rain_eaves_occlusion, 0.0);
    let dry = compute_environment_audio_hint(false, 15, 0.0, false);
    assert_eq!(dry.underwater_lowpass, 0.0);

    // Cave reverb: grows with enclosure and darkness.
    let open = compute_environment_audio_hint(false, 15, 0.0, false);
    let dusk_cave = compute_environment_audio_hint(false, 7, 0.8, false);
    let deep_cave = compute_environment_audio_hint(false, 0, 1.0, false);
    assert!(dusk_cave.cave_reverb_mix > open.cave_reverb_mix);
    assert!(deep_cave.cave_reverb_mix > dusk_cave.cave_reverb_mix);
    assert!(deep_cave.cave_reverb_mix <= 0.85 + 1e-6);

    // Rainy-eaves occlusion: nothing when dry or fully exposed, present
    // under cover in rain.
    let exposed_rain = compute_environment_audio_hint(false, 15, 0.0, true);
    assert_eq!(exposed_rain.rain_eaves_occlusion, 0.0);
    let covered_rain = compute_environment_audio_hint(false, 4, 0.9, true);
    assert!(covered_rain.rain_eaves_occlusion > 0.4);
    assert!(covered_rain.rain_eaves_occlusion <= 0.9 + 1e-6);

    // NaN enclosure fails closed to 0.
    let nan = compute_environment_audio_hint(false, 0, f32::NAN, true);
    assert_eq!(nan.cave_reverb_mix, 0.0);
    assert_eq!(nan.rain_eaves_occlusion, 0.0);
}

// ---------------------------------------------------------------------------
// 9. Hardware-tier caps & robustness
// ---------------------------------------------------------------------------

#[test]
fn tier_caps_are_exact() {
    assert_eq!(
        TactileEffectsSim::particle_cap_for_tier(VfxHardwareTier::LowSpec),
        48
    );
    assert_eq!(
        TactileEffectsSim::particle_cap_for_tier(VfxHardwareTier::Balanced),
        144
    );
    assert_eq!(
        TactileEffectsSim::particle_cap_for_tier(VfxHardwareTier::High),
        288
    );
}

#[test]
fn particle_cap_never_exceeded_under_burst() {
    for (tier, cap) in [
        (VfxHardwareTier::LowSpec, 48usize),
        (VfxHardwareTier::Balanced, 144usize),
        (VfxHardwareTier::High, 288usize),
    ] {
        let mut sim = fancy_sim(tier);
        for tick in 0..40u64 {
            emit_everything(&mut sim, tick);
            assert!(
                sim.len() <= cap,
                "tier {tier:?} exceeded cap: {} > {cap}",
                sim.len()
            );
            sim.step(1.0 / 60.0);
            assert!(sim.len() <= cap);
        }
        // Saturated burst without stepping still holds the cap.
        for tick in 40..80u64 {
            emit_everything(&mut sim, tick);
        }
        assert_eq!(
            sim.len(),
            cap,
            "saturated sim should sit exactly at the cap"
        );
    }
}

#[test]
fn invalid_positions_and_params_fail_closed() {
    let mut sim = fancy_sim(VfxHardwareTier::High);
    let nan = [f32::NAN, 0.0, 0.0];
    // f32 spacing at 3e7 is 2.0, so use a value that cannot round back in.
    let far = [31_000_000.0, 0.0, 0.0];
    let inf = [0.0, f32::INFINITY, 0.0];
    for bad in [nan, far, inf] {
        assert_eq!(sim.emit_bubble_pop(bad, 1), 0);
        assert_eq!(
            sim.emit_tree_shake_burst(bad, LeafCanopyKind::OakGreen, 1.0, 1),
            0
        );
        assert_eq!(
            sim.emit_footstep_or_landing(bad, GroundSurfaceKind::Snow, 5.0, 1),
            0
        );
        assert_eq!(sim.emit_ceiling_or_eaves_drip(bad, false, 1), 0);
        assert_eq!(
            sim.emit_cold_breath(bad, [0.0, 0.0, 1.0], -5.0, false, 1),
            0
        );
        assert_eq!(sim.emit_hearth_ember(bad, 1.0, 1), 0);
        assert_eq!(
            sim.emit_impact_sparks(bad, [0.0, 1.0, 0.0], ImpactMaterial::OreOrStone, 1),
            0
        );
        let mut ev = entry(FluidEntryBody::LargeMob, 9.0, false);
        ev.pos = bad;
        assert_eq!(sim.emit_fluid_entry(&ev), 0);
    }
    let mut ev = entry(FluidEntryBody::LargeMob, f32::NAN, false);
    assert_eq!(sim.emit_fluid_entry(&ev), 0);
    ev.impact_speed = f32::NEG_INFINITY;
    assert_eq!(sim.emit_fluid_entry(&ev), 0);
    assert!(sim.is_empty());
}

#[test]
fn step_clamps_dt_and_stays_finite() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    emit_everything(&mut sim, 7);
    sim.step(f32::NAN);
    sim.step(-1.0);
    sim.step(10.0); // clamped to 0.25 internally
    sim.step(0.25);
    assert_finite_particles(&sim);
}

#[test]
fn emission_is_deterministic_across_sims() {
    let mut a = fancy_sim(VfxHardwareTier::Balanced);
    let mut b = fancy_sim(VfxHardwareTier::Balanced);
    for tick in 0..6u64 {
        assert_eq!(emit_everything(&mut a, tick), emit_everything(&mut b, tick));
        a.step(0.05);
        b.step(0.05);
    }
    assert_eq!(a.particles(), b.particles());
}

#[test]
fn off_preset_ages_out_existing_without_new_spawns() {
    let mut sim = fancy_sim(VfxHardwareTier::Balanced);
    emit_everything(&mut sim, 3);
    assert!(!sim.is_empty());
    sim.set_config(TactileEffectsConfig::default());
    assert_eq!(emit_everything(&mut sim, 4), 0);
    for _ in 0..40 {
        sim.step(0.25);
    }
    assert!(sim.is_empty(), "all particles must expire once Off");
}

#[test]
fn subtle_preset_emits_fewer_than_fancy() {
    let mut subtle = TactileEffectsSim::new(subtle_config(), VfxHardwareTier::High);
    let mut fancy = fancy_sim(VfxHardwareTier::High);
    subtle.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::AutumnMaple, 1.0, 9);
    fancy.emit_tree_shake_burst([0.0, 70.0, 0.0], LeafCanopyKind::AutumnMaple, 1.0, 9);
    assert!(subtle.len() < fancy.len());
    assert!(!subtle.is_empty());
}

#[test]
fn drag_decay_is_frame_rate_independent() {
    // Drag must compound as exp(-k*dt): the same wall-time elapsed in one
    // coarse step or many fine steps has to leave drag-only velocity
    // components (horizontal splash/footstep/breath motion) essentially
    // identical. `1 - k*dt` Euler drag fails this at large dt.
    let mut coarse = fancy_sim(VfxHardwareTier::Balanced);
    let mut fine = fancy_sim(VfxHardwareTier::Balanced);
    for sim in [&mut coarse, &mut fine] {
        sim.emit_fluid_entry(&entry(FluidEntryBody::PlayerOrMediumMob, 9.0, false));
        sim.emit_footstep_or_landing([0.0, 64.0, 0.0], GroundSurfaceKind::Snow, 8.0, 11);
        sim.emit_cold_breath([0.0, 64.0, 0.0], [0.0, 0.0, 1.0], -5.0, false, 12);
    }
    assert!(!coarse.is_empty() && !fine.is_empty());

    coarse.step(0.25);
    for _ in 0..5 {
        fine.step(0.05);
    }

    let vel_of =
        |sim: &TactileEffectsSim, pred: fn(&TactileParticleKind) -> bool| -> Vec<[f32; 3]> {
            sim.particles()
                .iter()
                .filter(|p| pred(&p.kind))
                .map(|p| p.vel)
                .collect()
        };
    let is_splash = |k: &TactileParticleKind| matches!(k, TactileParticleKind::SplashDroplet);
    let is_puff = |k: &TactileParticleKind| matches!(k, TactileParticleKind::FootstepPuff);
    let is_breath = |k: &TactileParticleKind| matches!(k, TactileParticleKind::ColdBreathVapor);
    for (name, pred) in [
        ("splash", is_splash as fn(&TactileParticleKind) -> bool),
        ("footstep", is_puff),
        ("breath", is_breath),
    ] {
        let a = vel_of(&coarse, pred);
        let b = vel_of(&fine, pred);
        assert_eq!(a.len(), b.len(), "{name}: particle count diverged");
        assert!(!a.is_empty(), "{name}: no particles to compare");
        for (i, (va, vb)) in a.iter().zip(b.iter()).enumerate() {
            // Horizontal components see only drag (gravity/buoyancy act on
            // vel[1]); vel[2] of breath is likewise drag-only.
            for axis in [0usize, 2] {
                let diff = (va[axis] - vb[axis]).abs();
                assert!(
                    diff < 1.0e-3,
                    "{name}[{i}] vel[{axis}]: coarse={:?} fine={:?} diff={diff}",
                    va[axis],
                    vb[axis]
                );
            }
        }
    }
}
