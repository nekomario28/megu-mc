use super::*;

/// The 1.21.1 `MobEffects` registry in its declaration order. IDs here are
/// one-based for callers that consume registry tables; the legacy client code
/// stores zero-based IDs (and stores Hunger as `HUNGER_EFFECT`).
const EFFECT_REGISTRY: [&str; 39] = [
    "speed",
    "slowness",
    "haste",
    "mining_fatigue",
    "strength",
    "instant_health",
    "instant_damage",
    "jump_boost",
    "nausea",
    "regeneration",
    "resistance",
    "fire_resistance",
    "water_breathing",
    "invisibility",
    "blindness",
    "night_vision",
    "hunger",
    "weakness",
    "poison",
    "wither",
    "health_boost",
    "absorption",
    "saturation",
    "glowing",
    "levitation",
    "luck",
    "unluck",
    "slow_falling",
    "conduit_power",
    "dolphins_grace",
    "bad_omen",
    "hero_of_the_village",
    "darkness",
    "trial_omen",
    "raid_omen",
    "wind_charged",
    "weaving",
    "oozing",
    "infested",
];

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_registry_name(id: i32) -> Option<&'static str> {
    usize::try_from(id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .and_then(|index| EFFECT_REGISTRY.get(index).copied())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_registry_id(name: &str) -> Option<i32> {
    let name = name.trim_start_matches("minecraft:");
    EFFECT_REGISTRY
        .iter()
        .position(|candidate| *candidate == name)
        .map(|index| index as i32 + 1)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_internal_id(registry_id: i32) -> Option<i32> {
    let name = effect_registry_name(registry_id)?;
    Some(if name == "hunger" {
        HUNGER_EFFECT
    } else {
        registry_id - 1
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum EffectAttribute {
    MovementSpeed,
    AttackSpeed,
    AttackDamage,
    MaxHealth,
    MaxAbsorption,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct EffectAttributeModifiers {
    /// Factor applied to the base value; `1.0` means unchanged.
    pub movement_speed: f32,
    /// Factor applied to the base value; `1.0` means unchanged.
    pub attack_speed: f32,
    /// Flat damage added to the base attack damage.
    pub attack_damage: f32,
    pub max_health: f32,
    pub max_absorption: f32,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_attribute_modifiers(
    registry_id: i32,
    amplifier: i32,
) -> EffectAttributeModifiers {
    let levels = (amplifier as f32) + 1.0;
    let mut modifiers = EffectAttributeModifiers::default();
    match effect_registry_name(registry_id) {
        Some("speed") => modifiers.movement_speed = 1.0 + 0.2 * levels,
        Some("slowness") => modifiers.movement_speed = (1.0 - 0.15 * levels).max(0.0),
        Some("haste") => modifiers.attack_speed = 1.0 + 0.1 * levels,
        Some("mining_fatigue") => modifiers.attack_speed = (1.0 - 0.1 * levels).max(0.0),
        Some("strength") => modifiers.attack_damage = 3.0 * levels,
        Some("weakness") => modifiers.attack_damage = -4.0 * levels,
        Some("health_boost") => modifiers.max_health = 4.0 * levels,
        Some("absorption") => modifiers.max_absorption = 4.0 * levels,
        _ => {}
    }
    modifiers
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_attribute_modifier(
    registry_id: i32,
    amplifier: i32,
    attribute: EffectAttribute,
) -> f32 {
    let modifiers = effect_attribute_modifiers(registry_id, amplifier);
    match attribute {
        EffectAttribute::MovementSpeed => modifiers.movement_speed,
        EffectAttribute::AttackSpeed => modifiers.attack_speed,
        EffectAttribute::AttackDamage => modifiers.attack_damage,
        EffectAttribute::MaxHealth => modifiers.max_health,
        EffectAttribute::MaxAbsorption => modifiers.max_absorption,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum DamageKind {
    Ordinary,
    Fire,
    Lava,
    Fall,
    Void,
    Kill,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn resistance_multiplier(amplifier: i32) -> f32 {
    (1.0 - 0.2 * (amplifier as f32 + 1.0)).max(0.0)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn incoming_damage_after_effects(
    damage: f32,
    kind: DamageKind,
    resistance_amplifier: Option<i32>,
    fire_resistance: bool,
    slow_falling: bool,
) -> f32 {
    if damage <= 0.0 || matches!(kind, DamageKind::Void | DamageKind::Kill) {
        return damage.max(0.0);
    }
    if fire_resistance && matches!(kind, DamageKind::Fire | DamageKind::Lava) {
        return 0.0;
    }
    if slow_falling && kind == DamageKind::Fall {
        return 0.0;
    }
    if let Some(amplifier) = resistance_amplifier {
        return damage * resistance_multiplier(amplifier);
    }
    damage
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn slow_falling_gravity_multiplier(active: bool) -> f32 {
    if active {
        0.125
    } else {
        1.0
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn periodic_effect_interval(base_ticks: u32, amplifier: i32) -> u32 {
    // Java's int shift masks the shift count to five bits. Clamp the resulting
    // interval to one tick so high amplifiers never create a zero divisor.
    let shifted = base_ticks >> ((amplifier as u32) & 31);
    shifted.max(1)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn poison_damage(amplifier: i32) -> f32 {
    let _ = amplifier;
    1.0
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn wither_damage(amplifier: i32) -> f32 {
    let _ = amplifier;
    1.0
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn regeneration_heal(amplifier: i32) -> f32 {
    let _ = amplifier;
    1.0
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn hunger_exhaustion_per_tick(amplifier: i32) -> f32 {
    0.005 * (amplifier as f32 + 1.0)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn saturation_tick(food: i32, saturation: f32) -> (i32, f32) {
    saturation_tick_for(food, saturation, 0)
}

/// `SaturationEffect.applyEffectTick`: `FoodData.eat(amplifier + 1, 1.0)`
/// restores `amplifier + 1` food and twice that in saturation every tick,
/// saturation capped at the resulting food level.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn saturation_tick_for(food: i32, saturation: f32, amplifier: i32) -> (i32, f32) {
    let food = (food + amplifier + 1).clamp(0, 20);
    (
        food,
        (saturation + 2.0 * (amplifier as f32 + 1.0)).clamp(0.0, food as f32),
    )
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_prevents_drowning(registry_ids: &[i32]) -> bool {
    registry_ids.iter().any(|id| {
        matches!(
            effect_registry_name(*id),
            Some("water_breathing" | "conduit_power")
        )
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn levitation_velocity_delta(vertical_velocity: f64, amplifier: i32) -> f64 {
    (0.05 * (amplifier as f64 + 1.0) - vertical_velocity) * 0.2
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn instant_effect_amount(healing: bool, amplifier: i32) -> f32 {
    let base = if healing { 4i32 } else { 6i32 };
    base.wrapping_shl(amplifier as u32) as f32
}

/// The entity carries `name` (a registry id from [`EFFECT_REGISTRY`]) at any
/// amplifier. `registry_ids` holds the one-based registry ids the wire and the
/// save format use.
#[cfg_attr(not(test), allow(dead_code))]
fn effect_named_present(registry_ids: &[i32], name: &str) -> bool {
    registry_ids
        .iter()
        .any(|id| effect_registry_name(*id) == Some(name))
}

/// `MobEffects.SLOW_FALLING` empties `LivingEntity`'s fall-damage arm.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_negates_fall_damage(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "slow_falling")
}

/// The `gravity` attribute slow falling contributes while active: 0.125 of
/// normal, or 1.0 without it.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_gravity_scale(registry_ids: &[i32]) -> f32 {
    slow_falling_gravity_multiplier(effect_negates_fall_damage(registry_ids))
}

/// `MobEffects.FIRE_RESISTANCE` empties the fire and lava damage arms.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_negates_fire_damage(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "fire_resistance")
}

/// The `max_absorption`/`max_health` attribute total over `effects`, which
/// carries `(registry id, amplifier)` pairs: +4.0 per effect level.
#[cfg_attr(not(test), allow(dead_code))]
fn effect_attribute_bonus(effects: &[(i32, i32)], name: &str) -> f32 {
    effects
        .iter()
        .filter(|&&(id, _)| effect_registry_name(id) == Some(name))
        .map(|&(_, amplifier)| 4.0 * (amplifier as f32 + 1.0))
        .sum()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_bonus_absorption(effects: &[(i32, i32)]) -> f32 {
    effect_attribute_bonus(effects, "absorption")
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_bonus_max_health(effects: &[(i32, i32)]) -> f32 {
    effect_attribute_bonus(effects, "health_boost")
}

/// Night vision brightens the client's view; the server side of the contract
/// is the flag itself.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_grants_night_vision(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "night_vision")
}

/// `Entity.setInvisible`/the invisible flag the client and target selection
/// consume.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_is_invisible(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "invisibility")
}

/// `SharedEntityFlags`' glowing outline flag.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_outlines_entity(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "glowing")
}

/// `MobEffects.DARKNESS` dims the fog client-side; the flag is the whole
/// server-side contract.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_darkens_view(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "darkness")
}

/// `LivingEntity.travel`'s water branch: Dolphin's Grace overrides the
/// slowdown factor to 0.96F. Returns the factor in force.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn effect_swim_slow_down(registry_ids: &[i32]) -> f32 {
    if effect_named_present(registry_ids, "dolphins_grace") {
        super::DOLPHINS_GRACE_SLOW_DOWN
    } else {
        1.0
    }
}

/// `Levitation.applyEffectTick` lerps vertical velocity toward
/// `0.05 * (amplifier + 1)` every tick.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn levitation_upward_velocity(amplifier: i32) -> f64 {
    0.05 * (amplifier as f64 + 1.0)
}

/// `Villager.updateSpecialPrices`: Hero of the Village discounts the base
/// cost by `0.3 + 0.0625 * (level - 1)`, `level` being the 1-based effect
/// level (amplifier + 1).
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn hero_trade_discount(level: i32) -> f32 {
    0.3 + 0.0625 * (level - 1).max(0) as f32
}

pub(super) fn tick_effects(c: &Client) {
    let __t = crate::tickprof::enabled().then(std::time::Instant::now);
    adv_event(&c.name, "tick", None);
    if let Some(t) = __t {
        crate::tickprof::add("fx/adv_call", t.elapsed());
    }
    let mut ef = c.effs.lock().unwrap();
    if ef.is_empty() {
        return;
    }
    let mut dirty2 = false;
    let mut expired: Vec<i32> = Vec::new();
    let mut grant_totem = false;
    for e in ef.iter_mut() {
        e.left -= 1;
        if *c.gm.lock().unwrap() == 1 {
            if e.left <= 0 {
                expired.push(e.id);
            }
            continue;
        }
        let mut h = c.hp.lock().unwrap();
        match e.id {
            9 => {
                let iv = server_ticks(periodic_effect_interval(50, e.amp) as u64) as i32;
                if e.left % iv == 0 {
                    let mh = max_hp_of(&c.name);
                    let healed = (*h + regeneration_heal(e.amp)).min(mh);
                    dirty2 |= healed != *h;
                    *h = healed;
                }
            }
            18 => {
                let iv = server_ticks(periodic_effect_interval(25, e.amp) as u64) as i32;
                if e.left % iv == 0 && *h > 1.0 {
                    *h = (*h - poison_damage(e.amp)).max(1.0);
                    dirty2 = true;
                }
            }
            19 => {
                let iv = server_ticks(periodic_effect_interval(40, e.amp) as u64) as i32;
                if e.left % iv == 0 {
                    *h = (*h - wither_damage(e.amp)).max(0.0);
                    dirty2 = true;
                }
                if *h <= 0.0 {
                    // The effect guard is held here; do not re-lock it through totem_check.
                    let mut inv2 = c.inv.lock().unwrap();
                    let hk = 36i16 + (*c.held.lock().unwrap());
                    let mut saved = false;
                    let mut saved_k = -1i16;
                    let mut saved_now = (0i64, 0u8, Vec::new());
                    for k in [45i16, hk] {
                        if inv2.get(&k).map(|v| v.0) == Some(1163) {
                            let st = inv2.get(&k).unwrap().clone();
                            if st.1 <= 1 {
                                inv2.remove(&k);
                            } else {
                                inv2.insert(k, (st.0, st.1 - 1, st.2));
                            }
                            saved = true;
                            saved_k = k;
                            saved_now = inv2.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
                            break;
                        }
                    }
                    drop(inv2);
                    if saved {
                        *h = 1.0;
                        grant_totem = true;
                        resync_slot(c, 0, saved_k, &saved_now);
                        let mut st_pkt = Vec::new();
                        st_pkt.extend_from_slice(&c.eid.to_be_bytes());
                        st_pkt.push(35);
                        let _ = send_packet(&mut c.sock.lock().unwrap(), 0x1f, st_pkt.clone());
                        broadcast_seen(c.eid, |_c| (0x1f, st_pkt.clone()));
                        let (px, py, pz, ..) = {
                            let p = c.pos.lock().unwrap();
                            *p
                        };
                        let snd = sound_pkt_named("item.totem.use", px, py, pz);
                        broadcast_seen(c.eid, |_c| (0x68, snd.clone()));
                    }
                }
            }
            // Hunger exhaustion is applied by the existing player-food path.
            // Saturation is a per-tick effect and restores food before adding
            // two saturation points, capped at the resulting food level.
            22 => {
                drop(h);
                let mut f = c.food.lock().unwrap();
                let mut sat = c.sat.lock().unwrap();
                let (next_food, next_sat) = saturation_tick_for(*f, *sat, e.amp);
                dirty2 |= next_food != *f || next_sat != *sat;
                *f = next_food;
                *sat = next_sat;
                drop(sat);
                drop(f);
                h = c.hp.lock().unwrap();
            }
            12 | 28 => {
                // Water Breathing and Conduit Power prevent the ordinary air
                // supply from reaching the drowning threshold.
                drop(h);
                *c.air.lock().unwrap() = server_ticks(AIR_SUPPLY_GT) as i32;
                h = c.hp.lock().unwrap();
            }
            _ => {}
        }
        drop(h);
        if e.left <= 0 {
            expired.push(e.id);
        }
        // Death protection removes the entire old set; later entries must not execute.
        if grant_totem {
            break;
        }
    }
    if dirty2 {
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x5d, health_pkt_c(*c.hp.lock().unwrap(), c));
        }
    }
    for id in &expired {
        ef.retain(|e| e.id != *id);
        if *id == 21 {
            *c.absorb.lock().unwrap() = 0.0;
        }
        let pk = remove_effect_pkt(c.eid, *id);
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x43, pk.clone());
        }
        broadcast_seen(c.eid, |_c| (0x43, pk.clone()));
    }
    // Expired effects belong to the old set, not to the death-protection grant.
    if grant_totem {
        grant_totem_effects(c, &mut ef);
    }
}

// The anti-xray unit shares this module: root `mod` declarations belong to
// `main.rs` alone, so the lane's new file is included the same way the
// main-adjacent units are.
include!("antixray.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Eff;

    #[test]
    fn p5b_parity_status_effects_cover_vanilla_1211_hooks() {
        // Registry ids are one-based; the registry test pins the full table.
        assert!(effect_prevents_drowning(&[13]) && effect_prevents_drowning(&[29]));
        assert!(!effect_prevents_drowning(&[1, 12]));
        assert!(effect_negates_fall_damage(&[28]));
        assert_eq!(effect_gravity_scale(&[28]), 0.125);
        assert_eq!(effect_gravity_scale(&[]), 1.0);
        assert!(effect_negates_fire_damage(&[12]));
        assert!(!effect_negates_fire_damage(&[28]));
        assert!(effect_grants_night_vision(&[16]));
        assert!(effect_is_invisible(&[14]));
        assert!(effect_outlines_entity(&[24]));
        assert!(effect_darkens_view(&[33]));
        assert_eq!(effect_swim_slow_down(&[30]), 0.96);
        assert_eq!(effect_swim_slow_down(&[]), 1.0);
    }

    #[test]
    fn p5b_parity_periodic_rates_and_lethal_bounds_match_vanilla() {
        // Poison every max(1, 25 >> amp) ticks, never below 1 HP of damage
        // application; wither every max(1, 40 >> amp) and lethal; regen every
        // max(1, 50 >> amp).
        assert_eq!(periodic_effect_interval(25, 0), 25);
        assert_eq!(periodic_effect_interval(25, 1), 12);
        assert_eq!(periodic_effect_interval(40, 0), 40);
        assert_eq!(periodic_effect_interval(40, 2), 10);
        assert_eq!(periodic_effect_interval(50, 0), 50);
        assert_eq!(periodic_effect_interval(25, 31), 1);
        assert_eq!(periodic_effect_interval(40, 255), 1);
        assert_eq!(poison_damage(4), 1.0);
        assert_eq!(wither_damage(4), 1.0);
        assert_eq!(regeneration_heal(4), 1.0);
    }

    #[test]
    fn p5b_parity_poison_stops_at_one_hp_and_wither_is_lethal_live() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        *client.hp.lock().unwrap() = 1.0;
        client.effs.lock().unwrap().push(Eff {
            id: 18,
            amp: 0,
            left: 26,
        });
        tick_effects(&client);
        assert_eq!(*client.hp.lock().unwrap(), 1.0);

        client.effs.lock().unwrap().clear();
        *client.hp.lock().unwrap() = 1.0;
        client.effs.lock().unwrap().push(Eff {
            id: 19,
            amp: 0,
            left: 41,
        });
        tick_effects(&client);
        assert_eq!(*client.hp.lock().unwrap(), 0.0);
    }

    #[test]
    fn p5b_parity_health_absorption_levitation_and_hero_values() {
        assert_eq!(effect_bonus_absorption(&[(22, 0)]), 4.0);
        assert_eq!(effect_bonus_absorption(&[(22, 3)]), 16.0);
        assert_eq!(effect_bonus_max_health(&[(21, 0)]), 4.0);
        assert_eq!(effect_bonus_max_health(&[(21, 4), (1, 0)]), 20.0);
        assert_eq!(levitation_upward_velocity(0), 0.05);
        assert_eq!(levitation_upward_velocity(1), 0.10);
        assert!((hero_trade_discount(1) - 0.30).abs() < 1e-6);
        assert!((hero_trade_discount(2) - 0.3625).abs() < 1e-6);
        assert!((hero_trade_discount(5) - 0.55).abs() < 1e-6);
    }

    #[test]
    fn p5b_parity_hunger_and_saturation_follow_vanilla_food_math() {
        assert_eq!(hunger_exhaustion_per_tick(0), 0.005);
        assert_eq!(hunger_exhaustion_per_tick(2), 0.015);
        // `eat(amplifier + 1, 1.0)`: +amp+1 food, twice that saturation.
        assert_eq!(saturation_tick_for(10, 3.0, 0), (11, 5.0));
        assert_eq!(saturation_tick_for(10, 0.0, 1), (12, 4.0));
        assert_eq!(saturation_tick_for(19, 19.0, 4), (20, 20.0));
    }

    #[test]
    fn p5b_parity_tick_effects_apply_amplified_saturation_live() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        *client.food.lock().unwrap() = 10;
        *client.sat.lock().unwrap() = 0.0;
        client.effs.lock().unwrap().push(Eff {
            id: 22,
            amp: 1,
            left: 20,
        });
        tick_effects(&client);
        assert_eq!(*client.food.lock().unwrap(), 12);
        assert_eq!(*client.sat.lock().unwrap(), 4.0);
    }
}
