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

pub(super) fn effect_registry_name(id: i32) -> Option<&'static str> {
    usize::try_from(id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .and_then(|index| EFFECT_REGISTRY.get(index).copied())
}

pub(super) fn effect_registry_id(name: &str) -> Option<i32> {
    let name = name.trim_start_matches("minecraft:");
    EFFECT_REGISTRY
        .iter()
        .position(|candidate| *candidate == name)
        .map(|index| index as i32 + 1)
}

pub(super) fn effect_internal_id(registry_id: i32) -> Option<i32> {
    let name = effect_registry_name(registry_id)?;
    Some(if name == "hunger" {
        HUNGER_EFFECT
    } else {
        registry_id - 1
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Mirrors the five vanilla attribute axes; production reads AttackSpeed,
// the rest are exercised by the effect fixture suite.
#[allow(dead_code)]
pub(super) enum EffectAttribute {
    MovementSpeed,
    AttackSpeed,
    AttackDamage,
    MaxHealth,
    MaxAbsorption,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
#[cfg(test)]
pub(super) enum DamageKind {
    Ordinary,
    Fire,
    Lava,
    Fall,
    Void,
    Kill,
}

#[cfg(test)]
pub(super) fn resistance_multiplier(amplifier: i32) -> f32 {
    (1.0 - 0.2 * (amplifier as f32 + 1.0)).max(0.0)
}

#[cfg(test)]
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

pub(super) fn slow_falling_gravity_multiplier(active: bool) -> f32 {
    if active {
        0.125
    } else {
        1.0
    }
}

pub(super) fn periodic_effect_interval(base_ticks: u32, amplifier: i32) -> u32 {
    // Java's int shift masks the shift count to five bits. Clamp the resulting
    // interval to one tick so high amplifiers never create a zero divisor.
    let shifted = base_ticks >> ((amplifier as u32) & 31);
    shifted.max(1)
}

pub(super) fn poison_damage(amplifier: i32) -> f32 {
    let _ = amplifier;
    1.0
}

pub(super) fn wither_damage(amplifier: i32) -> f32 {
    let _ = amplifier;
    1.0
}

pub(super) fn regeneration_heal(amplifier: i32) -> f32 {
    let _ = amplifier;
    1.0
}

#[cfg(test)]
pub(super) fn hunger_exhaustion_per_tick(amplifier: i32) -> f32 {
    0.005 * (amplifier as f32 + 1.0)
}

#[cfg(test)]
pub(super) fn saturation_tick(food: i32, saturation: f32) -> (i32, f32) {
    saturation_tick_for(food, saturation, 0)
}

/// `SaturationEffect.applyEffectTick`: `FoodData.eat(amplifier + 1, 1.0)`
/// restores `amplifier + 1` food and twice that in saturation every tick,
/// saturation capped at the resulting food level.
pub(super) fn saturation_tick_for(food: i32, saturation: f32, amplifier: i32) -> (i32, f32) {
    let food = (food + amplifier + 1).clamp(0, 20);
    (
        food,
        (saturation + 2.0 * (amplifier as f32 + 1.0)).clamp(0.0, food as f32),
    )
}

#[cfg(test)]
pub(super) fn effect_prevents_drowning(registry_ids: &[i32]) -> bool {
    registry_ids.iter().any(|id| {
        matches!(
            effect_registry_name(*id),
            Some("water_breathing" | "conduit_power")
        )
    })
}

#[cfg(test)]
pub(super) fn levitation_velocity_delta(vertical_velocity: f64, amplifier: i32) -> f64 {
    (0.05 * (amplifier as f64 + 1.0) - vertical_velocity) * 0.2
}

#[cfg(test)]
pub(super) fn instant_effect_amount(healing: bool, amplifier: i32) -> f32 {
    let base = if healing { 4i32 } else { 6i32 };
    base.wrapping_shl(amplifier as u32) as f32
}

/// `LivingEntity#isInvertedHealAndHarm` as it applies to the instant pair: a
/// target in the undead family (`#minecraft:undead` — zombies, skeletons,
/// phantoms, the wither, …) swaps Instant Health and Instant Damage.
pub(super) fn instant_effect_harmful(harm: bool, inverted_heal_and_harm: bool) -> bool {
    harm != inverted_heal_and_harm
}

/// `HealOrHarmMobEffect#applyEffectTick` in health points, scaled by the
/// caller's splash/lingering distance factor.
///
/// The two instant effects share one implementation in vanilla: a *healing*
/// application restores `4 << amplifier` and a *harmful* one deals
/// `6 << amplifier` magic damage, and the pair inverts on undead targets, so
/// Instant Damage heals an undead for `4 << amplifier` while Instant Health
/// hurts it for `6 << amplifier`.
pub(super) fn instant_effect_health_delta(
    harm: bool,
    amplifier: i32,
    inverted_heal_and_harm: bool,
    scale: f32,
) -> f32 {
    let magnitude = (if instant_effect_harmful(harm, inverted_heal_and_harm) {
        6i32
    } else {
        4i32
    })
    // Java's `Math.max(4 << amplifier, 0)` keeps a negative magnitude from
    // flipping the sign; the shift masks its count to five bits.
    .wrapping_shl((amplifier as u32) & 31)
    .max(0);
    let amount = if instant_effect_harmful(harm, inverted_heal_and_harm) {
        -magnitude
    } else {
        magnitude
    };
    amount as f32 * scale.max(0.0)
}

/// The entity carries `name` (a registry id from [`EFFECT_REGISTRY`]) at any
/// amplifier. `registry_ids` holds the one-based registry ids the wire and the
/// save format use.
fn effect_named_present(registry_ids: &[i32], name: &str) -> bool {
    registry_ids
        .iter()
        .any(|id| effect_registry_name(*id) == Some(name))
}

/// `MobEffects.SLOW_FALLING` empties `LivingEntity`'s fall-damage arm.
pub(super) fn effect_negates_fall_damage(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "slow_falling")
}

/// The `gravity` attribute slow falling contributes while active: 0.125 of
/// normal, or 1.0 without it.
pub(super) fn effect_gravity_scale(registry_ids: &[i32]) -> f32 {
    slow_falling_gravity_multiplier(effect_negates_fall_damage(registry_ids))
}

/// `MobEffects.FIRE_RESISTANCE` empties the fire and lava damage arms.
#[cfg(test)]
pub(super) fn effect_negates_fire_damage(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "fire_resistance")
}

/// The `max_absorption`/`max_health` attribute total over `effects`, which
/// carries `(registry id, amplifier)` pairs: +4.0 per effect level.
fn effect_attribute_bonus(effects: &[(i32, i32)], name: &str) -> f32 {
    effects
        .iter()
        .filter(|&&(id, _)| effect_registry_name(id) == Some(name))
        .map(|&(_, amplifier)| 4.0 * (amplifier as f32 + 1.0))
        .sum()
}

#[cfg(test)]
pub(super) fn effect_bonus_absorption(effects: &[(i32, i32)]) -> f32 {
    effect_attribute_bonus(effects, "absorption")
}

#[cfg(test)]
pub(super) fn effect_bonus_max_health(effects: &[(i32, i32)]) -> f32 {
    effect_attribute_bonus(effects, "health_boost")
}

/// Night vision brightens the client's view; the server side of the contract
/// is the flag itself.
pub(super) fn effect_grants_night_vision(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "night_vision")
}

/// `Entity.setInvisible`/the invisible flag the client and target selection
/// consume.
pub(super) fn effect_is_invisible(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "invisibility")
}

/// `SharedEntityFlags`' glowing outline flag.
pub(super) fn effect_outlines_entity(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "glowing")
}

/// `MobEffects.DARKNESS` dims the fog client-side; the flag is the whole
/// server-side contract.
pub(super) fn effect_darkens_view(registry_ids: &[i32]) -> bool {
    effect_named_present(registry_ids, "darkness")
}

/// `LivingEntity.travel`'s water branch: Dolphin's Grace overrides the
/// slowdown factor to 0.96F. Returns the factor in force.
pub(super) fn effect_swim_slow_down(registry_ids: &[i32]) -> f32 {
    if effect_named_present(registry_ids, "dolphins_grace") {
        super::DOLPHINS_GRACE_SLOW_DOWN
    } else {
        1.0
    }
}

/// `Levitation.applyEffectTick` lerps vertical velocity toward
/// `0.05 * (amplifier + 1)` every tick.
pub(super) fn levitation_upward_velocity(amplifier: i32) -> f64 {
    0.05 * (amplifier as f64 + 1.0)
}

/// `Villager.updateSpecialPrices`: Hero of the Village discounts the base
/// cost by `0.3 + 0.0625 * (level - 1)`, `level` being the 1-based effect
/// level (amplifier + 1).
#[cfg(test)]
pub(super) fn hero_trade_discount(level: i32) -> f32 {
    0.3 + 0.0625 * (level - 1).max(0) as f32
}

/// The inverse of [`effect_internal_id`]: the legacy `Eff.id` is the zero-based
/// registry index for every effect except Hunger, which rides the
/// `HUNGER_EFFECT` sentinel.
fn effect_registry_id_of_internal(internal: i32) -> Option<i32> {
    if internal == HUNGER_EFFECT {
        return effect_registry_id("hunger");
    }
    let registry = internal.checked_add(1)?;
    effect_registry_name(registry)?;
    Some(registry)
}

/// One pass over the active `(registry id, amplifier)` pairs that condenses
/// every status effect the rest of the tick machinery needs into a single
/// view: client-facing flags, movement/gravity scalars, loot bonuses and the
/// 1.21.1 Trial Chambers carriers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct EffectRuntimeSummary {
    /// `MobEffects.NIGHT_VISION` — client brightening; the flag itself is the
    /// server-side contract.
    pub night_vision: bool,
    /// `Entity.setInvisible` — shared-flags bit `0x20`.
    pub invisible: bool,
    /// `MobEffects.GLOWING` — shared-flags outline bit `0x40`.
    pub outlined: bool,
    /// Blindness or Darkness is dimming the fog client-side.
    pub fog_darkened: bool,
    /// `MobEffects.BLINDNESS` — also gates crits and sprint-knockback in
    /// vanilla's attack paths.
    pub blinded: bool,
    /// `LivingEntity.travel`'s water slowdown factor: `0.96` with Dolphin's
    /// Grace, `1.0` otherwise.
    pub swim_slow_down: f32,
    /// `MobEffects.SLOW_FALLING` empties the fall-damage arm.
    pub negates_fall_damage: bool,
    /// Effective gravity multiplier while falling (`0.125` slow falling).
    pub gravity_scale: f32,
    /// `Levitation.applyEffectTick`'s target upward velocity; `0.0` when the
    /// effect is not carried.
    pub levitation_velocity: f64,
    /// `LivingEntity.getJumpBoostPower`: `0.1 * (amplifier + 1)`.
    pub jump_boost_bonus_velocity: f32,
    /// Net `generic.luck` for loot rolls: `+(amp + 1)` luck, `-(amp + 1)` unluck.
    pub luck_bonus: f32,
    /// Cobweb movement factor: Weaving raises vanilla's `0.25` to `0.5`.
    pub cobweb_speed_scale: f32,
    /// `MobEffects.TRIAL_OMEN` arms ominous trial spawners.
    pub trial_omen_active: bool,
    /// `MobEffects.INFESTED` — 1.21.1 on-hurt silverfish carrier.
    pub has_infested: bool,
    /// `MobEffects.OOZING` — 1.21.1 on-death slime carrier.
    pub has_oozing: bool,
    /// `MobEffects.WEAVING` — 1.21.1 on-death cobweb carrier.
    pub has_weaving: bool,
    /// `MobEffects.WIND_CHARGED` — 1.21.1 on-death wind-burst carrier.
    pub has_wind_charged: bool,
}

/// Summarizes the active `(registry id, amplifier)` effect set in one pass,
/// folding the per-effect helpers into a single runtime view.
pub(super) fn summarize_active_effects(effects: &[(i32, i32)]) -> EffectRuntimeSummary {
    let registry_ids: Vec<i32> = effects.iter().map(|&(id, _)| id).collect();
    let levitation_amplifier = effects
        .iter()
        .filter(|&&(id, _)| effect_registry_name(id) == Some("levitation"))
        .map(|&(_, amplifier)| amplifier)
        .max();
    EffectRuntimeSummary {
        night_vision: effect_grants_night_vision(&registry_ids),
        invisible: effect_is_invisible(&registry_ids),
        outlined: effect_outlines_entity(&registry_ids),
        fog_darkened: effect_darkens_view(&registry_ids)
            || effect_named_present(&registry_ids, "blindness"),
        blinded: effect_named_present(&registry_ids, "blindness"),
        swim_slow_down: effect_swim_slow_down(&registry_ids),
        negates_fall_damage: effect_negates_fall_damage(&registry_ids),
        gravity_scale: effect_gravity_scale(&registry_ids),
        levitation_velocity: levitation_amplifier.map_or(0.0, levitation_upward_velocity),
        jump_boost_bonus_velocity: jump_boost_velocity_bonus(effects),
        luck_bonus: luck_loot_bonus(effects),
        cobweb_speed_scale: if effect_named_present(&registry_ids, "weaving") {
            0.5
        } else {
            0.25
        },
        trial_omen_active: effect_named_present(&registry_ids, "trial_omen"),
        has_infested: effect_named_present(&registry_ids, "infested"),
        has_oozing: effect_named_present(&registry_ids, "oozing"),
        has_weaving: effect_named_present(&registry_ids, "weaving"),
        has_wind_charged: effect_named_present(&registry_ids, "wind_charged"),
    }
}

/// Deterministic 64-bit mixer (splitmix64) for the reactive Trial Chambers
/// rolls: a stable seed always yields the same outcome.
fn fx_mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

/// `InfestedMobEffect`: each accepted hurt has a 10% chance to release one or
/// two silverfish. The `seed` roll is caller-supplied so the check stays pure.
pub(super) fn infested_on_hurt_silverfish_count(seed: u64, has_infested: bool) -> u8 {
    if !has_infested {
        return 0;
    }
    let roll = fx_mix64(seed);
    if !roll.is_multiple_of(10) {
        return 0;
    }
    1 + ((roll >> 8) & 1) as u8
}

/// `OozingMobEffect.onMobRemoved`: up to two medium slimes at the death point,
/// bounded by `maxEntityCramming` headroom (`0` disables the cap, matching the
/// gamerule's disabled state).
pub(super) fn oozing_on_death_slime_count(
    nearby_slimes: usize,
    max_entity_cramming: usize,
    has_oozing: bool,
) -> u8 {
    if !has_oozing {
        return 0;
    }
    let headroom = if max_entity_cramming == 0 {
        2
    } else {
        max_entity_cramming.saturating_sub(nearby_slimes)
    };
    headroom.min(2) as u8
}

/// `WeavingMobEffect.onMobRemoved`: scatters two or three cobweb offsets in the
/// 3x3x3 cell block around the death position. Offsets are unique, ordered by
/// the deterministic `seed` draw.
pub(super) fn weaving_on_death_cobweb_offsets(
    seed: u64,
    has_weaving: bool,
) -> Vec<(i32, i32, i32)> {
    if !has_weaving {
        return Vec::new();
    }
    let mut state = fx_mix64(seed);
    let count = 2 + (state % 2) as usize;
    let mut offsets = Vec::with_capacity(count);
    while offsets.len() < count {
        state = fx_mix64(state ^ 0x9e3779b97f4a7c15);
        let cell = (state % 27) as i32;
        let offset = (cell % 3 - 1, (cell / 3) % 3 - 1, cell / 9 - 1);
        if !offsets.contains(&offset) {
            offsets.push(offset);
        }
    }
    offsets
}

/// `WindChargedMobEffect.onMobRemoved`: the death wind burst knocks entities
/// back across a 3.5-block radius; inactive carriers burst nothing.
pub(super) fn wind_charged_on_death_burst_radius(has_wind_charged: bool) -> f32 {
    if has_wind_charged {
        3.5
    } else {
        0.0
    }
}

/// `generic.luck` over the effect set: Luck adds `amplifier + 1`, Unluck
/// subtracts it. Multiple instances collapse to the strongest of each, like
/// vanilla's single-instance-per-effect map.
pub(super) fn luck_loot_bonus(effects: &[(i32, i32)]) -> f32 {
    let level = |name: &str| {
        effects
            .iter()
            .filter(|&&(id, _)| effect_registry_name(id) == Some(name))
            .map(|&(_, amplifier)| amplifier + 1)
            .max()
            .unwrap_or(0) as f32
    };
    level("luck") - level("unluck")
}

/// `LivingEntity.getJumpBoostPower`: `0.1 * (amplifier + 1)` from the strongest
/// Jump Boost instance, `0.0` without one.
pub(super) fn jump_boost_velocity_bonus(effects: &[(i32, i32)]) -> f32 {
    effects
        .iter()
        .filter(|&&(id, _)| effect_registry_name(id) == Some("jump_boost"))
        .map(|&(_, amplifier)| 0.1 * (amplifier as f32 + 1.0))
        .fold(0.0, f32::max)
}

/// Per-player effect-tick state: the last broadcast shared-flags byte, the
/// hurt-window snapshot feeding `infested`, and the death latch for the
/// `oozing`/`weaving`/`wind_charged` on-death behaviours.
#[derive(Default)]
struct EffectTickState {
    flags: u8,
    last_hurt: f32,
    invulnerable: i32,
    dead: bool,
}

fn effect_runtime_states() -> &'static Mutex<HashMap<i32, EffectTickState>> {
    static STATES: OnceLock<Mutex<HashMap<i32, EffectTickState>>> = OnceLock::new();
    STATES.get_or_init(Mutex::default)
}

/// The `Entity` shared-flags byte (metadata index 0): invisibility `0x20` and
/// the glowing outline `0x40` ride the same byte the entity-action writer
/// composes from sneak `0x02`, sprint `0x08` and fall-flying `0x80`, so this
/// pass rebroadcasts the full composition rather than its two bits alone.
fn effect_shared_flags(c: &Client, summary: &EffectRuntimeSummary) -> u8 {
    let mut flags = 0u8;
    if *c.sneaking.lock().unwrap_or_else(|e| e.into_inner()) {
        flags |= 0x02;
    }
    if *c.sprinting.lock().unwrap_or_else(|e| e.into_inner()) {
        flags |= 0x08;
    }
    if summary.invisible {
        flags |= 0x20;
    }
    if summary.outlined {
        flags |= 0x40;
    }
    if *c.fall_fly.lock().unwrap_or_else(|e| e.into_inner()) {
        flags |= 0x80;
    }
    flags
}

const SILVERFISH_ETYPE: i64 = 90;
const SLIME_ETYPE: i64 = 93;

/// `Explosion.hurtEntities`' knockback arm with the reach and multiplier
/// supplied by the caller: `(1 - dist/reach)` falloff along the normalized
/// direction to the target's eye. Entities at the exact burst centre are
/// skipped, like vanilla.
fn wind_burst_knockback(
    impact: [f64; 3],
    target: [f64; 3],
    target_eye_y: f64,
    reach: f64,
    multiplier: f64,
    knockback_resist: f64,
) -> Option<[f64; 3]> {
    if !impact.iter().all(|v| v.is_finite())
        || !target.iter().all(|v| v.is_finite())
        || !target_eye_y.is_finite()
        || !knockback_resist.is_finite()
    {
        return None;
    }
    let d = [
        target[0] - impact[0],
        target_eye_y - impact[1],
        target[2] - impact[2],
    ];
    let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if dist <= 1e-9 || dist > reach {
        return None;
    }
    let strength =
        (1.0 - dist / reach) * multiplier * (1.0 - knockback_resist.clamp(0.0, 1.0));
    Some([
        d[0] / dist * strength,
        d[1] / dist * strength,
        d[2] / dist * strength,
    ])
}

/// The Wind Charged death burst: `hurtEntities`-style knockback at the thrown
/// wind charge's `1.22` multiplier over a `radius * 2` reach, then the TRIGGER
/// block pass and the explosion broadcast. Mirrors `wind_charge_burst` with
/// the death burst's own radius.
fn wind_charged_death_burst(dim: DimId, impact: [f64; 3], radius: f32) {
    const KNOCKBACK_MULTIPLIER: f64 = 1.22;
    let reach = f64::from(radius) * 2.0;
    for c in client_snapshot() {
        if *c.dim.lock().unwrap_or_else(|e| e.into_inner()) != dim
            || *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 3
        {
            continue;
        }
        let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        let eye = y
            + f64::from(player_eye_height(
                *c.sneaking.lock().unwrap_or_else(|e| e.into_inner()),
                *c.fall_fly.lock().unwrap_or_else(|e| e.into_inner()),
            ));
        if let Some(dv) = wind_burst_knockback(
            impact,
            [x, y, z],
            eye,
            reach,
            KNOCKBACK_MULTIPLIER,
            knockback_resist_of(&c),
        ) {
            let pkt = velocity_pkt(
                c.eid,
                (dv[0] * 8000.0) as i16,
                (dv[1] * 8000.0) as i16,
                (dv[2] * 8000.0) as i16,
            );
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x5a, pkt.clone());
            }
            broadcast_seen_ex(c.eid, c.eid, |_c| (0x5a, pkt.clone()));
        }
    }
    for m in mobs().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if m.dim != dim {
            continue;
        }
        let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
        let eye = my + f64::from(entity_eye_height_1211(m.etype));
        if let Some(dv) =
            wind_burst_knockback(impact, [mx, my, mz], eye, reach, KNOCKBACK_MULTIPLIER, 0.0)
        {
            let mut p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
            p.0 += dv[0];
            p.1 += dv[1];
            p.2 += dv[2];
            let (nx, ny, nz, yaw, pitch) = *p;
            drop(p);
            broadcast_seen(m.eid, |_c| {
                (0x70, entity_teleport(m.eid, nx, ny, nz, yaw, pitch, true))
            });
        }
    }
    wind_charge_trigger_blocks(dim, impact, f64::from(radius));
    broadcast(-1, |_c| {
        (
            0x20,
            explosion_pkt_with_power(impact[0], impact[1], impact[2], radius, &[]),
        )
    });
    ac_note_explosion(dim, impact, f64::from(radius));
}

/// Per-call entropy for the reactive rolls: a splitmix64 sequence keyed by
/// tick and entity id, so repeated deaths and hurts never replay one outcome.
fn effect_fx_seed(eid: i32) -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0x9e3779b97f4a7c15);
    SEQ.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed)
        ^ TICK.load(Ordering::Relaxed).wrapping_mul(0x100000001b3)
        ^ (eid as u64).wrapping_mul(0x9e3779b185ebca87)
}

/// The `LivingEntity.die` arm of the 1.21.1 Trial Chambers effects: Oozing
/// releases medium slimes under the cramming cap, Weaving scatters cobwebs
/// into open cells, Wind Charged detonates its burst.
fn apply_effect_death(c: &Client, summary: &EffectRuntimeSummary) {
    let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    let dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
    let seed = effect_fx_seed(c.eid);
    if summary.has_oozing {
        let nearby = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|m| {
                m.etype == SLIME_ETYPE
                    && m.dim == dim
                    && {
                        let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
                        (mx - x).abs() < 1.0 && (my - y).abs() < 1.0 && (mz - z).abs() < 1.0
                    }
            })
            .count();
        let cramming = max_entity_cramming_limit().max(0) as usize;
        let count = oozing_on_death_slime_count(nearby, cramming, true);
        for i in 0..count {
            let jx = ((seed >> (u64::from(i) * 13)) % 40) as f64 / 40.0 - 0.5;
            let jz = ((seed >> (u64::from(i) * 13 + 7)) % 40) as f64 / 40.0 - 0.5;
            // Medium slime: size 2, `size * size` health.
            insert_live_mob(
                SLIME_ETYPE,
                dim,
                (x + jx, y, z + jz, 0.0, 0.0),
                4.0,
                Some(SlimeSize::Medium),
            );
        }
    }
    // Cobwebs only take over open cells; the air check is the conservative
    // core of vanilla's `canBeReplaced` and never overwrites another block.
    if let Some(web) = name_to_state("cobweb") {
        for (dx, dy, dz) in weaving_on_death_cobweb_offsets(seed, summary.has_weaving) {
            let (bx, by, bz) = (
                x.floor() as i32 + dx,
                y.floor() as i32 + dy,
                z.floor() as i32 + dz,
            );
            if block_at_dim(dim, bx, by, bz) == 0 {
                set_block_dim(dim, bx, by, bz, web);
            }
        }
    }
    let radius = wind_charged_on_death_burst_radius(summary.has_wind_charged);
    if radius > 0.0 {
        wind_charged_death_burst(dim, [x, y + 0.5, z], radius);
    }
}

/// `InfestedMobEffect` releases one or two silverfish next to the hurt entity.
fn spawn_infested_silverfish(c: &Client, count: u8) {
    if count == 0 {
        return;
    }
    let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
    let dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
    let seed = effect_fx_seed(c.eid);
    for i in 0..count {
        let jx = ((seed >> (u64::from(i) * 11)) % 40) as f64 / 40.0 - 0.5;
        let jz = ((seed >> (u64::from(i) * 11 + 5)) % 40) as f64 / 40.0 - 0.5;
        insert_live_mob(
            SILVERFISH_ETYPE,
            dim,
            (x + jx, y, z + jz, 0.0, 0.0),
            8.0,
            None,
        );
    }
}

/// The per-tick consumer of [`summarize_active_effects`]: broadcasts the
/// composed shared-flags byte when the effect bits move (and keeps
/// rebroadcasting while an effect flag is up, because the entity-action writer
/// resends the same byte without them), keeps `fall_from` honest under Slow
/// Falling and Levitation, and fires the hurt/death edges for the Trial
/// Chambers carriers.
fn apply_effect_runtime(c: &Client, summary: &EffectRuntimeSummary) {
    let flags = effect_shared_flags(c, summary);
    let last_hurt = *c.last_hurt.lock().unwrap_or_else(|e| e.into_inner());
    let invulnerable = *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner());
    let dead = *c.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0;
    let (flag_change, hurt_edge, death_edge) = {
        let mut states = effect_runtime_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tick = TICK.load(Ordering::Relaxed);
        if tick != 0 && tick.is_multiple_of(256) {
            let live: HashSet<i32> = client_snapshot().iter().map(|cl| cl.eid).collect();
            states.retain(|eid, _| live.contains(eid));
        }
        let state = states.entry(c.eid).or_insert(EffectTickState {
            flags,
            last_hurt,
            invulnerable,
            dead,
        });
        let edges = (
            state.flags != flags,
            last_hurt != state.last_hurt || invulnerable > state.invulnerable,
            dead && !state.dead,
        );
        state.flags = flags;
        state.last_hurt = last_hurt;
        state.invulnerable = invulnerable;
        state.dead = dead;
        edges
    };
    if flag_change || flags & 0x60 != 0 {
        broadcast_seen(c.eid, move |_c| {
            (0x58, live_mob_metadata(c.eid, 0, 0, flags))
        });
    }
    // Slow Falling and Levitation stop fall-distance accumulation while they
    // carry: tracking the live height keeps `fall_from` at the point a landing
    // after expiry is measured from.
    if summary.negates_fall_damage || summary.levitation_velocity != 0.0 {
        let (_, y, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        *c.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = y;
    }
    if hurt_edge {
        let count = infested_on_hurt_silverfish_count(
            effect_fx_seed(c.eid),
            summary.has_infested,
        );
        spawn_infested_silverfish(c, count);
    }
    if death_edge {
        apply_effect_death(c, summary);
    }
}

/// Republish the `generic.max_health`/`generic.max_absorption` attribute
/// bonuses the active set carries. Health Boost feeds `max_hp_of` (food
/// healing, respawn, attribute reads); when the bonus drops below current
/// health vanilla clamps `getHealth` to the new maximum. Absorption tops the
/// damage pool back up to `4.0 * (amplifier + 1)` whenever it has been fully
/// spent, like `AbsorptionMobEffect#applyEffectTick`, and the summed bonus
/// caps what other sources can hold.
fn sync_effect_attribute_bonuses(c: &Client, ef: &[Eff]) {
    let pairs: Vec<(i32, i32)> = ef
        .iter()
        .filter_map(|e| effect_registry_id_of_internal(e.id).map(|id| (id, e.amp)))
        .collect();
    let health = effect_attribute_bonus(&pairs, "health_boost");
    let previous = set_effect_max_health_bonus(&c.name, health);
    let mut changed = false;
    // Only a shrinking bonus can push current health over the new maximum.
    if health < previous {
        let mh = max_hp_of(&c.name);
        let mut h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
        if *h > mh {
            *h = mh;
            changed = true;
        }
    }
    let absorption = effect_attribute_bonus(&pairs, "absorption");
    if absorption > 0.0 {
        let mut a = c.absorb.lock().unwrap_or_else(|e| e.into_inner());
        let next = if *a <= 0.0 {
            absorption
        } else {
            (*a).min(absorption)
        };
        if next != *a {
            *a = next;
            changed = true;
        }
    }
    if changed {
        let hp = *c.hp.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x5d, health_pkt_c(hp, c));
        }
    }
}

pub(super) fn tick_effects(c: &Client) {
    let __t = crate::tickprof::enabled().then(std::time::Instant::now);
    adv_event(&c.name, "tick", None);
    if let Some(t) = __t {
        crate::tickprof::add("fx/adv_call", t.elapsed());
    }
    let mut ef = c.effs.lock().unwrap_or_else(|e| e.into_inner());
    if ef.is_empty() {
        drop(ef);
        // `/effect clear` and respawn empty the set out-of-band; the bonus
        // tables still need the all-clear pass.
        sync_effect_attribute_bonuses(c, &[]);
        apply_effect_runtime(c, &summarize_active_effects(&[]));
        return;
    }
    let mut dirty2 = false;
    let mut expired: Vec<i32> = Vec::new();
    let mut grant_totem = false;
    let creative = *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
    for e in ef.iter_mut() {
        // `MobEffectInstance#tick` samples `shouldApplyEffectTickThisTick`
        // with the *remaining* duration and only then calls
        // `tickDownDuration`, so a 25-tick poison first bites on the tick that
        // starts at 25. Reading the post-decrement value instead fires every
        // periodic effect one tick early and loses the final tick.
        let duration = e.left;
        e.left -= 1;
        if creative {
            if e.left <= 0 {
                expired.push(e.id);
            }
            continue;
        }
        let mut h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
        match e.id {
            // Instant Health (5) and Instant Damage (6). Vanilla
            // `InstantenousMobEffect` applies on grant and stores no instance,
            // so a stored one (mod- or save-supplied) acts exactly once on
            // its first tick: `HealOrHarmMobEffect#applyEffectTick` heals
            // `4 << amplifier` or hurts `6 << amplifier`, and an undead
            // target swaps the pair. Players are never undead, so the local
            // view is the non-inverted half; the mob grant paths own theirs.
            // Death protection stays with `totem_check` (the effect guard is
            // held here), which is where the instant grant paths already call
            // it.
            5 | 6 => {
                let delta = instant_effect_health_delta(e.id == 6, e.amp, false, 1.0);
                let mh = max_hp_of(&c.name);
                *h = (*h + delta).clamp(0.0, mh);
                dirty2 = true;
                e.left = 0;
            }
            9 => {
                let iv = server_ticks(periodic_effect_interval(50, e.amp) as u64) as i32;
                if duration % iv == 0 {
                    let mh = max_hp_of(&c.name);
                    let healed = (*h + regeneration_heal(e.amp)).min(mh);
                    dirty2 |= healed != *h;
                    *h = healed;
                }
            }
            18 => {
                let iv = server_ticks(periodic_effect_interval(25, e.amp) as u64) as i32;
                if duration % iv == 0 && *h > 1.0 {
                    *h = (*h - poison_damage(e.amp)).max(1.0);
                    dirty2 = true;
                }
            }
            19 => {
                let iv = server_ticks(periodic_effect_interval(40, e.amp) as u64) as i32;
                if duration % iv == 0 {
                    *h = (*h - wither_damage(e.amp)).max(0.0);
                    dirty2 = true;
                }
                if *h <= 0.0 {
                    // The effect guard is held here; do not re-lock it through totem_check.
                    let mut inv2 = c.inv.lock().unwrap_or_else(|e| e.into_inner());
                    let hk = 36i16 + (*c.held.lock().unwrap_or_else(|e| e.into_inner()));
                    let mut saved = false;
                    let mut saved_k = -1i16;
                    let mut saved_now = (0i64, 0u8, Vec::new());
                    for k in [45i16, hk] {
                        let Some(st) = inv2.get(&k).cloned().filter(|v| v.0 == 1163) else {
                            continue;
                        };
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
                    drop(inv2);
                    if saved {
                        *h = 1.0;
                        grant_totem = true;
                        resync_slot(c, 0, saved_k, &saved_now);
                        let mut st_pkt = Vec::new();
                        st_pkt.extend_from_slice(&c.eid.to_be_bytes());
                        st_pkt.push(35);
                        let _ = send_packet(&mut c.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x1f, st_pkt.clone());
                        broadcast_seen(c.eid, |_c| (0x1f, st_pkt.clone()));
                        let (px, py, pz, ..) = {
                            let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut f = c.food.lock().unwrap_or_else(|e| e.into_inner());
                let mut sat = c.sat.lock().unwrap_or_else(|e| e.into_inner());
                let (next_food, next_sat) = saturation_tick_for(*f, *sat, e.amp);
                dirty2 |= next_food != *f || next_sat != *sat;
                *f = next_food;
                *sat = next_sat;
                drop(sat);
                drop(f);
                h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
            }
            12 | 28 => {
                // Water Breathing and Conduit Power prevent the ordinary air
                // supply from reaching the drowning threshold.
                drop(h);
                *c.air.lock().unwrap_or_else(|e| e.into_inner()) = server_ticks(AIR_SUPPLY_GT) as i32;
                h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
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
            let _ = send_packet(&mut w, 0x5d, health_pkt_c(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), c));
        }
    }
    for id in &expired {
        ef.retain(|e| e.id != *id);
        if *id == 21 {
            *c.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
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
    sync_effect_attribute_bonuses(c, &ef);
    let summary = summarize_active_effects(
        &ef.iter()
            .filter_map(|e| effect_registry_id_of_internal(e.id).map(|id| (id, e.amp)))
            .collect::<Vec<_>>(),
    );
    drop(ef);
    apply_effect_runtime(c, &summary);
}

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
        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
        client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
            id: 18,
            amp: 0,
            left: 26,
        });
        tick_effects(&client);
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 1.0);

        client.effs.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
        // `MobEffectInstance#tick` samples the wither's 40-tick period with the
        // remaining duration *before* the decrement, so the damage tick is the
        // one that starts at 40.
        client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
            id: 19,
            amp: 0,
            left: 40,
        });
        tick_effects(&client);
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
    }

    #[test]
    fn v1211_tick_effects_sample_the_periodic_tick_before_the_decrement() {
        // MobEffectInstance#tick: `shouldApplyEffectTickThisTick(duration, amp)`
        // runs with the *remaining* duration and `tickDownDuration` follows, so
        // a 25-tick poison first bites on the tick that starts at 25 and a
        // 1-tick wither never bites at all.
        let (client, _receiver) = crate::effect_tests::client_pair();
        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 10.0;
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 18, amp: 0, left: 25 }];
        tick_effects(&client);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            9.0,
            "the 25th remaining tick is the first poison tick"
        );

        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 10.0;
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 18, amp: 0, left: 26 }];
        tick_effects(&client);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            10.0,
            "one tick early is not a poison tick"
        );
        assert_eq!(
            client.effs.lock().unwrap_or_else(|e| e.into_inner())[0].left,
            25,
            "the instance only counts down"
        );

        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 10.0;
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 19, amp: 0, left: 1 }];
        tick_effects(&client);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            10.0,
            "a one-tick wither expires without dealing damage"
        );
        assert!(client.effs.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn v1211_instant_effects_apply_heal_harm_and_the_undead_inversion() {
        // `HealOrHarmMobEffect#applyEffectTick`: Instant Health heals
        // `4 << amplifier`, Instant Damage hurts `6 << amplifier`, and the pair
        // swaps on a target whose `isInvertedHealAndHarm` is true (undead).
        assert!(!instant_effect_harmful(false, false));
        assert!(instant_effect_harmful(true, false));
        assert!(instant_effect_harmful(false, true), "undead invert instant health");
        assert!(!instant_effect_harmful(true, true), "undead invert instant damage");
        assert_eq!(instant_effect_health_delta(false, 0, false, 1.0), 4.0);
        assert_eq!(instant_effect_health_delta(false, 1, false, 1.0), 8.0);
        assert_eq!(instant_effect_health_delta(true, 0, false, 1.0), -6.0);
        assert_eq!(instant_effect_health_delta(true, 1, false, 1.0), -12.0);
        assert_eq!(instant_effect_health_delta(false, 0, true, 1.0), -6.0);
        assert_eq!(instant_effect_health_delta(true, 0, true, 1.0), 4.0);
        // Splash and cloud distance scales multiply the amount.
        assert_eq!(instant_effect_health_delta(false, 0, false, 0.25), 1.0);
        assert_eq!(instant_effect_health_delta(true, 1, false, 0.5), -6.0);

        // A stored instant instance acts once on its first tick and is dropped:
        // vanilla never keeps one, so this is the only observable path here.
        let (client, _receiver) = crate::effect_tests::client_pair();
        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 10.0;
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 5, amp: 1, left: 600 }];
        tick_effects(&client);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            18.0,
            "instant health II heals 8 and expires"
        );
        assert!(client.effs.lock().unwrap_or_else(|e| e.into_inner()).is_empty());

        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 20.0;
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 5, amp: 3, left: 600 }];
        tick_effects(&client);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            20.0,
            "instant health never heals past the max health"
        );

        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 6, amp: 0, left: 600 }];
        tick_effects(&client);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            14.0,
            "instant damage deals 6 and expires"
        );
        assert!(client.effs.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
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
        *client.food.lock().unwrap_or_else(|e| e.into_inner()) = 10;
        *client.sat.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        client.effs.lock().unwrap_or_else(|e| e.into_inner()).push(Eff {
            id: 22,
            amp: 1,
            left: 20,
        });
        tick_effects(&client);
        assert_eq!(*client.food.lock().unwrap_or_else(|e| e.into_inner()), 12);
        assert_eq!(*client.sat.lock().unwrap_or_else(|e| e.into_inner()), 4.0);
    }

    #[test]
    fn v1211_runtime_summary_maps_every_remaining_status_effect() {
        // One-based registry ids: blindness 15, darkness 33, dolphins_grace 30,
        // glowing 24, infested 39, invisibility 14, jump_boost 8, levitation
        // 25, luck 26, night_vision 16, oozing 38, slow_falling 28, trial_omen
        // 34, unluck 27, weaving 37, wind_charged 36.
        let effects: Vec<(i32, i32)> = vec![
            (15, 0), (33, 0), (30, 0), (24, 0), (39, 0), (14, 0), (8, 2), (25, 1),
            (26, 1), (16, 0), (38, 0), (28, 0), (34, 0), (27, 0), (37, 0), (36, 0),
        ];
        let s = summarize_active_effects(&effects);
        assert!(s.night_vision);
        assert!(s.invisible);
        assert!(s.outlined);
        assert!(s.fog_darkened);
        assert!(s.blinded);
        assert_eq!(s.swim_slow_down, 0.96);
        assert!(s.negates_fall_damage);
        assert_eq!(s.gravity_scale, 0.125);
        assert_eq!(s.levitation_velocity, 0.10, "amplifier 1 -> 0.05 * 2");
        assert!((s.jump_boost_bonus_velocity - 0.3).abs() < 1e-6);
        assert_eq!(s.luck_bonus, 1.0, "luck 2 levels minus unluck 1 level");
        assert_eq!(s.cobweb_speed_scale, 0.5);
        assert!(s.trial_omen_active);
        assert!(s.has_infested);
        assert!(s.has_oozing);
        assert!(s.has_weaving);
        assert!(s.has_wind_charged);

        let empty = summarize_active_effects(&[]);
        assert_eq!(empty, EffectRuntimeSummary {
            swim_slow_down: 1.0,
            gravity_scale: 1.0,
            cobweb_speed_scale: 0.25,
            ..EffectRuntimeSummary::default()
        });
    }

    #[test]
    fn v1211_blindness_darkness_and_night_vision_summarize_independently() {
        let blind = summarize_active_effects(&[(15, 0)]);
        assert!(blind.fog_darkened && blind.blinded && !blind.night_vision);
        let dark = summarize_active_effects(&[(33, 0)]);
        assert!(dark.fog_darkened && !dark.blinded);
        let night = summarize_active_effects(&[(16, 0)]);
        assert!(night.night_vision && !night.fog_darkened && !night.blinded);
    }

    #[test]
    fn v1211_luck_unluck_and_jump_boost_follow_vanilla_formulas() {
        assert_eq!(luck_loot_bonus(&[]), 0.0);
        assert_eq!(luck_loot_bonus(&[(26, 0)]), 1.0);
        assert_eq!(luck_loot_bonus(&[(26, 2)]), 3.0);
        assert_eq!(luck_loot_bonus(&[(27, 0)]), -1.0);
        assert_eq!(luck_loot_bonus(&[(27, 1)]), -2.0);
        assert_eq!(luck_loot_bonus(&[(26, 1), (27, 0)]), 1.0);
        // Strongest instance wins, like vanilla's one-instance-per-effect map.
        assert_eq!(luck_loot_bonus(&[(26, 0), (26, 4)]), 5.0);
        assert_eq!(jump_boost_velocity_bonus(&[]), 0.0);
        assert_eq!(jump_boost_velocity_bonus(&[(8, 0)]), 0.1);
        assert!((jump_boost_velocity_bonus(&[(8, 1)]) - 0.2).abs() < 1e-6);
        assert_eq!(jump_boost_velocity_bonus(&[(8, 0), (8, 3)]), 0.4);
    }

    #[test]
    fn v1211_infested_oozing_weaving_wind_charged_rolls_are_deterministic() {
        // Infested: exactly 10% of the deterministic seeds release 1..=2 fish.
        assert_eq!(infested_on_hurt_silverfish_count(0, false), 0);
        let mut releases = 0usize;
        for seed in 0..10_000u64 {
            let n = infested_on_hurt_silverfish_count(seed, true);
            assert!(n <= 2);
            releases += usize::from(n > 0);
        }
        assert!(
            (900..=1100).contains(&releases),
            "10% hurt roll must trigger for about a tenth of seeds, got {releases}"
        );
        let seeded: Vec<u8> = (0..32).map(|s| infested_on_hurt_silverfish_count(s, true)).collect();
        assert_eq!(
            seeded,
            (0..32).map(|s| infested_on_hurt_silverfish_count(s, true)).collect::<Vec<_>>()
        );

        // Oozing: up to two medium slimes, capped by the cramming headroom.
        assert_eq!(oozing_on_death_slime_count(0, 24, true), 2);
        assert_eq!(oozing_on_death_slime_count(23, 24, true), 1);
        assert_eq!(oozing_on_death_slime_count(24, 24, true), 0);
        assert_eq!(oozing_on_death_slime_count(0, 1, true), 1);
        assert_eq!(oozing_on_death_slime_count(0, 0, true), 2, "cramming disabled");
        assert_eq!(oozing_on_death_slime_count(0, 24, false), 0);

        // Weaving: 2..=3 unique offsets inside the 3x3x3 death cell.
        for seed in 0..64u64 {
            let offsets = weaving_on_death_cobweb_offsets(seed, true);
            assert!((2..=3).contains(&offsets.len()), "seed {seed}");
            let mut unique = offsets.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), offsets.len(), "seed {seed} duplicated a cell");
            for &(dx, dy, dz) in &offsets {
                assert!((-1..=1).contains(&dx) && (-1..=1).contains(&dy) && (-1..=1).contains(&dz));
            }
            assert_eq!(offsets, weaving_on_death_cobweb_offsets(seed, true));
        }
        assert!(weaving_on_death_cobweb_offsets(7, false).is_empty());

        // Wind Charged: 3.5-block burst radius, nothing without the carrier.
        assert_eq!(wind_charged_on_death_burst_radius(true), 3.5);
        assert_eq!(wind_charged_on_death_burst_radius(false), 0.0);
    }

    #[test]
    fn v1211_tick_effects_composes_invisible_glowing_flag_byte_live() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        effect_runtime_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *client.sneaking.lock().unwrap_or_else(|e| e.into_inner()) = true;
        let summary = summarize_active_effects(&[(14, 0), (24, 0)]);
        assert_eq!(effect_shared_flags(&client, &summary), 0x02 | 0x20 | 0x40);
        *client.sprinting.lock().unwrap_or_else(|e| e.into_inner()) = true;
        *client.fall_fly.lock().unwrap_or_else(|e| e.into_inner()) = true;
        assert_eq!(effect_shared_flags(&client, &summary), 0x02 | 0x08 | 0x20 | 0x40 | 0x80);
        assert_eq!(effect_shared_flags(&client, &summarize_active_effects(&[])), 0x0a | 0x80);
    }

    #[test]
    fn v1211_tick_effects_death_edge_runs_oozing_and_weaving_live() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        effect_runtime_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) =
            (47_000.5, 200.0, 47_000.5, 0.0, 0.0, false);
        // First tick alive latches `dead=false`; the second observes the edge.
        client
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Eff { id: 37, amp: 0, left: 600 });
        client
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Eff { id: 36, amp: 0, left: 600 });
        tick_effects(&client);
        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        tick_effects(&client);

        let spawned: Vec<i32> = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|m| m.etype == SLIME_ETYPE && {
                let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
                (mx - 47_000.5).abs() < 2.0 && (my - 200.0).abs() < 2.0 && (mz - 47_000.5).abs() < 2.0
            })
            .map(|m| m.eid)
            .collect();
        assert!(
            (1..=2).contains(&spawned.len()),
            "oozing releases up to two medium slimes, got {spawned:?}"
        );
        for eid in &spawned {
            let states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
            let state = states.get(eid).expect("slime state");
            assert!(
                matches!(
                    state.behavior,
                    Some(MobBehaviorState::Slime { behavior, .. })
                        if behavior.size == SlimeSize::Medium
                ),
                "oozing releases medium slimes"
            );
        }
        for eid in spawned {
            remove_live_mob(eid);
        }

        let web = name_to_state("cobweb").unwrap();
        let (bx, by, bz) = (47_000, 200, 47_000);
        let mut placed = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if block_at_dim(DimId::Overworld, bx + dx, by + dy, bz + dz) == web {
                        placed.push((bx + dx, by + dy, bz + dz));
                    }
                }
            }
        }
        assert!(
            (2..=3).contains(&placed.len()),
            "weaving scatters 2..=3 cobwebs, got {placed:?}"
        );
        for (x, y, z) in placed {
            set_block_dim(DimId::Overworld, x, y, z, 0);
        }
    }

    #[test]
    fn v1211_tick_effects_fall_from_tracks_slow_falling_and_levitation() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        effect_runtime_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) =
            (0.0, 64.0, 0.0, 0.0, 0.0, false);
        *client.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = 120.0;
        client
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Eff { id: 27, amp: 0, left: 600 });
        tick_effects(&client);
        assert_eq!(
            *client.fall_from.lock().unwrap_or_else(|e| e.into_inner()),
            64.0,
            "slow falling keeps fall distance at the live height"
        );
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 24, amp: 0, left: 600 }];
        *client.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = 120.0;
        tick_effects(&client);
        assert_eq!(
            *client.fall_from.lock().unwrap_or_else(|e| e.into_inner()),
            64.0,
            "levitation re-arms the fall base while rising"
        );
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) = vec![Eff { id: 1, amp: 0, left: 600 }];
        *client.fall_from.lock().unwrap_or_else(|e| e.into_inner()) = 120.0;
        tick_effects(&client);
        assert_eq!(
            *client.fall_from.lock().unwrap_or_else(|e| e.into_inner()),
            120.0,
            "unrelated effects leave fall_from alone"
        );
    }

    #[test]
    fn v1211_infested_spawn_releases_the_rolled_silverfish_count() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) =
            (48_000.5, 210.0, 48_000.5, 0.0, 0.0, false);
        let before: HashSet<i32> = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|m| m.eid)
            .collect();
        spawn_infested_silverfish(&client, 2);
        let spawned: Vec<i32> = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|m| m.etype == SILVERFISH_ETYPE && !before.contains(&m.eid))
            .map(|m| m.eid)
            .collect();
        assert_eq!(spawned.len(), 2);
        for eid in spawned {
            remove_live_mob(eid);
        }
        spawn_infested_silverfish(&client, 0);
        let after: HashSet<i32> = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|m| m.eid)
            .collect();
        assert_eq!(after, before, "a zero roll spawns nothing");
    }

    #[test]
    fn v1211_wind_charged_death_burst_knocks_back_live_mobs() {
        let mob = new_live_mob(
            SLIME_ETYPE,
            DimId::Overworld,
            (49_001.5, 200.0, 49_000.5, 0.0, 0.0),
            4.0,
            Some(SlimeSize::Medium),
        );
        let eid = mob.eid;
        mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
        wind_charged_death_burst(DimId::Overworld, [49_000.5, 200.5, 49_000.5], 3.5);
        let (mx, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert!(mx > 49_001.5, "the burst pushes the mob away, got {mx}");
        remove_live_mob(eid);
    }

    #[test]
    fn v1211_name_to_effect_covers_all_39_vanilla_effects() {
        // The internal id is the zero-based `minecraft:mob_effect` registry id;
        // Hunger rides the HUNGER_EFFECT sentinel that maps back to wire 16.
        for (index, name) in EFFECT_REGISTRY.iter().enumerate() {
            let expected = if *name == "hunger" {
                HUNGER_EFFECT
            } else {
                index as i32
            };
            assert_eq!(name_to_effect(name), Some(expected), "{name}");
            assert_eq!(
                name_to_effect(&format!("minecraft:{name}")),
                Some(expected),
                "namespaced {name}"
            );
            // `/effect give` stores the internal id; the registry helpers must
            // resolve it back to the same effect for packets and summaries.
            let registry = effect_registry_id_of_internal(expected).unwrap();
            assert_eq!(effect_registry_name(registry), Some(*name));
        }
        for name in [
            "trial_omen",
            "raid_omen",
            "wind_charged",
            "weaving",
            "oozing",
            "infested",
        ] {
            assert!(name_to_effect(name).is_some(), "{name} must be giveable");
        }
        assert_eq!(name_to_effect("missing"), None);
    }

    #[test]
    fn v1211_health_boost_and_absorption_drive_live_attributes() {
        let (client, _receiver) = crate::effect_tests::client_pair();
        lk(equip_attrs()).remove(&client.name);
        set_effect_max_health_bonus(&client.name, 0.0);
        *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        let base = max_hp_of(&client.name);

        // Health Boost II is +8.0 max health (4.0 per level).
        client
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Eff { id: 20, amp: 1, left: 600 });
        tick_effects(&client);
        assert_eq!(max_hp_of(&client.name), base + 8.0);

        // Health above the new maximum clamps when the bonus shrinks.
        *client.hp.lock().unwrap_or_else(|e| e.into_inner()) = base + 10.0;
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 20, amp: 0, left: 600 }];
        tick_effects(&client);
        assert_eq!(max_hp_of(&client.name), base + 4.0);
        assert_eq!(
            *client.hp.lock().unwrap_or_else(|e| e.into_inner()),
            base + 4.0
        );

        // Full expiry drops the bonus and clamps back to the base maximum.
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) = vec![];
        tick_effects(&client);
        assert_eq!(max_hp_of(&client.name), base);
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), base);

        // Absorption I fills an empty pool with 4.0 and tops it back up only
        // once fully spent; a partial pool is left alone.
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 21, amp: 0, left: 600 }];
        tick_effects(&client);
        assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 4.0);
        *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 2.0;
        tick_effects(&client);
        assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 2.0);
        *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        tick_effects(&client);
        assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 4.0);

        // Amplifier I doubles the pool, and natural expiry clears it.
        *client.effs.lock().unwrap_or_else(|e| e.into_inner()) =
            vec![Eff { id: 21, amp: 1, left: 2 }];
        *client.absorb.lock().unwrap_or_else(|e| e.into_inner()) = 0.0;
        tick_effects(&client);
        assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 8.0);
        tick_effects(&client);
        assert!(client.effs.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert_eq!(*client.absorb.lock().unwrap_or_else(|e| e.into_inner()), 0.0);
        set_effect_max_health_bonus(&client.name, 0.0);
    }
}
