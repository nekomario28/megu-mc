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
    let food = (food + 1).clamp(0, 20);
    (food, (saturation + 2.0).clamp(0.0, food as f32))
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
                let (next_food, next_sat) = saturation_tick(*f, *sat);
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
