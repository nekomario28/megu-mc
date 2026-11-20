// R7 GP-6 specialized behaviors. The caller owns entity state, supplies one game
// tick at a time, and applies the returned combat, movement, and world actions.
// Random draws accept the world's next_below stream so tests can script rolls.

#[cfg_attr(not(test), allow(dead_code))]
fn mob_behavior_id(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

#[cfg_attr(not(test), allow(dead_code))]
fn mob_behavior_delta(from: [f64; 3], to: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| to[axis] - from[axis])
}

#[cfg_attr(not(test), allow(dead_code))]
fn mob_behavior_dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.into_iter().zip(b).map(|(a, b)| a * b).sum()
}

#[cfg_attr(not(test), allow(dead_code))]
fn mob_behavior_unit(vector: [f64; 3]) -> [f64; 3] {
    let length = mob_behavior_dot(vector, vector).sqrt();
    if length > 0.0 {
        vector.map(|component| component / length)
    } else {
        [0.0; 3]
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn mob_behavior_random_unit(draw: &mut impl FnMut(u32) -> u32) -> f64 {
    const STEPS: u32 = 1 << 24;
    f64::from(draw(STEPS)) / f64::from(STEPS)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum BlazeAttack {
    Melee { target: i32, damage: f32 },
    SmallFireball { target: i32 },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlazePhase {
    Idle(u8),
    Charging(u8),
    Burst { ticks: u8, shots: u8 },
    Cooldown(u8),
}

#[cfg_attr(not(test), allow(dead_code))]
struct BlazeBehavior {
    phase: BlazePhase,
    melee_cooldown: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl BlazeBehavior {
    fn new() -> Self {
        Self {
            phase: BlazePhase::Idle(60),
            melee_cooldown: 0,
        }
    }

    fn on_fire(&self) -> bool {
        matches!(
            self.phase,
            BlazePhase::Charging(_) | BlazePhase::Burst { .. }
        )
    }

    fn tick(&mut self, target: Option<(i32, f64)>) -> Option<BlazeAttack> {
        self.melee_cooldown = self.melee_cooldown.saturating_sub(1);
        let cooling_down = if let BlazePhase::Cooldown(ticks) = self.phase {
            self.phase = if ticks == 1 {
                BlazePhase::Idle(60)
            } else {
                BlazePhase::Cooldown(ticks - 1)
            };
            true
        } else {
            false
        };

        let Some((target, distance)) = target else {
            if !matches!(self.phase, BlazePhase::Cooldown(_)) {
                self.phase = BlazePhase::Idle(60);
            }
            return None;
        };
        if distance <= 2.0 {
            if self.melee_cooldown == 0 {
                self.melee_cooldown = 20;
                return Some(BlazeAttack::Melee {
                    target,
                    damage: 6.0,
                });
            }
            return None;
        }

        if cooling_down {
            return None;
        }
        self.phase = match self.phase {
            BlazePhase::Idle(1) => BlazePhase::Charging(60),
            BlazePhase::Idle(ticks) => BlazePhase::Idle(ticks - 1),
            BlazePhase::Charging(1) => BlazePhase::Burst { ticks: 1, shots: 3 },
            BlazePhase::Charging(ticks) => BlazePhase::Charging(ticks - 1),
            BlazePhase::Burst { ticks: 1, shots } => {
                self.phase = if shots == 1 {
                    BlazePhase::Cooldown(100)
                } else {
                    BlazePhase::Burst {
                        ticks: 6,
                        shots: shots - 1,
                    }
                };
                return Some(BlazeAttack::SmallFireball { target });
            }
            BlazePhase::Burst { ticks, shots } => BlazePhase::Burst {
                ticks: ticks - 1,
                shots,
            },
            BlazePhase::Cooldown(_) => self.phase,
        };
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
enum BlazeHazard {
    Water,
    Snowball,
}

#[cfg_attr(not(test), allow(dead_code))]
fn blaze_hazard_damage(hazard: BlazeHazard) -> f32 {
    match hazard {
        BlazeHazard::Water => 1.0,
        BlazeHazard::Snowball => 3.0,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct GhastFireball {
    fired_by: i32,
    owner: i32,
    velocity: [f64; 3],
    explosion_power: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl GhastFireball {
    fn deflect(&mut self, attacker: i32) {
        self.velocity = self.velocity.map(|component| -component);
        self.owner = attacker;
    }

    fn returns_to_sender(&self, hit: i32) -> bool {
        hit == self.fired_by && self.owner != self.fired_by
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum GhastAttack {
    Warning,
    Fire(GhastFireball),
}

#[cfg_attr(not(test), allow(dead_code))]
struct GhastBehavior {
    entity: i32,
    altitude: f64,
    attack_tick: i8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl GhastBehavior {
    fn new(entity: i32, altitude: f64) -> Self {
        Self {
            entity,
            altitude,
            attack_tick: 0,
        }
    }

    fn hover_velocity(&self, y: f64, vertical_velocity: f64) -> f64 {
        vertical_velocity * 0.8 + ((self.altitude - y) * 0.05).clamp(-0.1, 0.1)
    }

    fn charging(&self) -> bool {
        self.attack_tick > 10
    }

    fn tick(
        &mut self,
        target_delta: Option<[f64; 3]>,
        target_in_range_and_visible: bool,
    ) -> Option<GhastAttack> {
        let Some(delta) = target_delta else {
            self.attack_tick = 0;
            return None;
        };
        if !target_in_range_and_visible {
            if self.attack_tick > 0 {
                self.attack_tick -= 1;
            }
            return None;
        }
        self.attack_tick += 1;
        match self.attack_tick {
            10 => Some(GhastAttack::Warning),
            20 => {
                self.attack_tick = -40;
                Some(GhastAttack::Fire(GhastFireball {
                    fired_by: self.entity,
                    owner: self.entity,
                    velocity: mob_behavior_unit(delta),
                    explosion_power: 1,
                }))
            }
            _ => None,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct NetherConversion {
    unsafe_ticks: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl NetherConversion {
    fn tick(&mut self, dimension_piglin_safe: bool) -> bool {
        if self.converted() {
            return false;
        }
        if dimension_piglin_safe {
            self.unsafe_ticks = 0;
            return false;
        }
        self.unsafe_ticks += 1;
        self.converted()
    }

    fn converted(&self) -> bool {
        self.unsafe_ticks > 300
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
enum PiglinProvocation<'a> {
    Opened(&'a str),
    Mined(&'a str),
}

#[cfg_attr(not(test), allow(dead_code))]
impl PiglinProvocation<'_> {
    fn guards(self) -> bool {
        match self {
            Self::Opened(block) => {
                let block = mob_behavior_id(block);
                matches!(
                    block,
                    "chest" | "trapped_chest" | "barrel" | "ender_chest" | "shulker_box"
                ) || block.ends_with("_shulker_box")
            }
            Self::Mined(block) => matches!(
                mob_behavior_id(block),
                "gold_block"
                    | "raw_gold_block"
                    | "gold_ore"
                    | "deepslate_gold_ore"
                    | "nether_gold_ore"
                    | "gilded_blackstone"
            ),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PiglinBarterMetadata {
    Plain,
    SoulSpeed,
    Potion(&'static str),
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PiglinBarterLoot {
    item: &'static str,
    count: u8,
    potion: Option<&'static str>,
    soul_speed: Option<u8>,
}

// Minecraft 1.21.1's gameplay/piglin_bartering, in loot-pool order.
// Each row is (item, weight, inclusive minimum and maximum count, metadata).
#[cfg_attr(not(test), allow(dead_code))]
const PIGLIN_BARTERING: &[(&str, u32, u8, u8, PiglinBarterMetadata)] = &{
    use PiglinBarterMetadata::{Plain, Potion, SoulSpeed};
    [
        ("enchanted_book", 5, 1, 1, SoulSpeed),
        ("iron_boots", 8, 1, 1, SoulSpeed),
        ("potion", 8, 1, 1, Potion("fire_resistance")),
        ("splash_potion", 8, 1, 1, Potion("fire_resistance")),
        ("potion", 10, 1, 1, Potion("water")),
        ("iron_nugget", 10, 10, 36, Plain),
        ("ender_pearl", 10, 2, 4, Plain),
        ("string", 20, 3, 9, Plain),
        ("quartz", 20, 5, 12, Plain),
        ("obsidian", 40, 1, 1, Plain),
        ("crying_obsidian", 40, 1, 3, Plain),
        ("fire_charge", 40, 1, 1, Plain),
        ("leather", 40, 2, 4, Plain),
        ("soul_sand", 40, 2, 8, Plain),
        ("nether_brick", 40, 2, 8, Plain),
        ("spectral_arrow", 40, 6, 12, Plain),
        ("gravel", 40, 8, 16, Plain),
        ("blackstone", 40, 8, 16, Plain),
    ]
};

#[cfg_attr(not(test), allow(dead_code))]
fn roll_piglin_barter(draw: &mut impl FnMut(u32) -> u32) -> PiglinBarterLoot {
    let mut roll = draw(PIGLIN_BARTERING.iter().map(|entry| entry.1).sum());
    for &(item, weight, min, max, metadata) in PIGLIN_BARTERING {
        if roll >= weight {
            roll -= weight;
            continue;
        }
        return PiglinBarterLoot {
            item,
            count: if min == max {
                min
            } else {
                min + draw(u32::from(max - min) + 1) as u8
            },
            potion: match metadata {
                PiglinBarterMetadata::Potion(potion) => Some(potion),
                _ => None,
            },
            soul_speed: (metadata == PiglinBarterMetadata::SoulSpeed).then(|| 1 + draw(3) as u8),
        };
    }
    unreachable!("barter draw must be below the total pool weight")
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum PiglinEvent {
    Barter { player: i32, loot: PiglinBarterLoot },
    ConvertToZombifiedPiglin,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct PiglinBehavior {
    anger: Option<(i32, u16)>,
    barter: Option<(i32, u8)>,
    conversion: NetherConversion,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PiglinBehavior {
    fn hostile_to(&self, player: i32, armor: &[&str]) -> bool {
        !self.conversion.converted()
            && (self.anger.is_some_and(|(target, _)| target == player)
                || !armor.iter().any(|piece| {
                    matches!(
                        mob_behavior_id(piece),
                        "golden_helmet" | "golden_chestplate" | "golden_leggings" | "golden_boots"
                    )
                }))
    }

    fn anger_ticks(&self) -> u16 {
        self.anger.map_or(0, |(_, ticks)| ticks)
    }

    fn provoke(&mut self, player: i32, distance: f64, event: PiglinProvocation<'_>) -> bool {
        if self.conversion.converted() || distance > 16.0 || !event.guards() {
            return false;
        }
        self.anger = Some((player, 600));
        true
    }

    // A successful offer transfers one item from the caller to the piglin.
    fn offer(&mut self, player: i32, item: &str) -> bool {
        if self.conversion.converted()
            || self.barter.is_some()
            || mob_behavior_id(item) != "gold_ingot"
        {
            return false;
        }
        self.barter = Some((player, 120));
        true
    }

    fn tick(
        &mut self,
        dimension_piglin_safe: bool,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> Option<PiglinEvent> {
        if self.conversion.tick(dimension_piglin_safe) {
            self.anger = None;
            self.barter = None;
            return Some(PiglinEvent::ConvertToZombifiedPiglin);
        }
        if let Some((player, ticks)) = self.anger {
            self.anger = (ticks > 1).then_some((player, ticks - 1));
        }
        let (player, ticks) = self.barter?;
        self.barter = (ticks > 1).then_some((player, ticks - 1));
        (ticks == 1).then(|| PiglinEvent::Barter {
            player,
            loot: roll_piglin_barter(draw),
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct HoglinBehavior {
    conversion: NetherConversion,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum HoglinEvent {
    ConvertToZoglin,
}

#[cfg_attr(not(test), allow(dead_code))]
impl HoglinBehavior {
    fn tick(&mut self, dimension_piglin_safe: bool) -> Option<HoglinEvent> {
        self.conversion
            .tick(dimension_piglin_safe)
            .then_some(HoglinEvent::ConvertToZoglin)
    }

    fn flee_from(&self, position: [f64; 3], repellents: &[([f64; 3], &str)]) -> Option<[f64; 3]> {
        if self.conversion.converted() {
            return None;
        }
        repellents
            .iter()
            .filter_map(|&(block, kind)| {
                if !matches!(
                    mob_behavior_id(kind),
                    "warped_fungus" | "potted_warped_fungus" | "nether_portal" | "respawn_anchor"
                ) {
                    return None;
                }
                let away = mob_behavior_delta(block, position);
                let squared = mob_behavior_dot(away, away);
                (squared <= 49.0).then_some((squared, away))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, away)| mob_behavior_unit(away))
    }

    fn fling(
        &self,
        direction: [f64; 3],
        attack_knockback: f64,
        victim_resistance: f64,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> [f64; 3] {
        let strength = (attack_knockback - victim_resistance).max(0.0);
        if self.conversion.converted() || strength == 0.0 {
            return [0.0; 3];
        }
        let [x, _, z] = mob_behavior_unit([direction[0], 0.0, direction[2]]);
        let (sin, cos) = (f64::from(draw(21)) - 10.0).to_radians().sin_cos();
        let horizontal = strength * (0.2 + 0.5 * mob_behavior_random_unit(draw));
        let up = 0.5 * strength * mob_behavior_random_unit(draw);
        [
            (x * cos + z * sin) * horizontal,
            up,
            (z * cos - x * sin) * horizontal,
        ]
    }

    // Hoglin's own knockback resistance, separate from the victim's resistance.
    fn knockback_resistance(&self) -> f64 {
        0.6
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn enderman_stared_at(
    player_eye: [f64; 3],
    player_look: [f64; 3],
    enderman_eye: [f64; 3],
    head_item: &str,
    clear_line_of_sight: bool,
) -> bool {
    if mob_behavior_id(head_item) == "carved_pumpkin" || !clear_line_of_sight {
        return false;
    }
    let to_enderman = mob_behavior_delta(player_eye, enderman_eye);
    let distance = mob_behavior_dot(to_enderman, to_enderman).sqrt();
    let look = mob_behavior_unit(player_look);
    distance > 0.0
        && distance <= 64.0
        && look != [0.0; 3]
        && mob_behavior_dot(look, mob_behavior_unit(to_enderman)) > 1.0 - 0.025 / distance
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Default)]
struct EndermanTeleportBlock {
    blocks_motion: bool,
    liquid: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
enum EndermanHazard {
    Water,
    Rain,
    Projectile,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct EndermanEscape {
    destination: Option<[f64; 3]>,
    reject_damage: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
fn enderman_escape(
    hazard: EndermanHazard,
    position: [f64; 3],
    world_y: std::ops::Range<i32>,
    mut block_at: impl FnMut([i32; 3]) -> EndermanTeleportBlock,
    mut can_occupy: impl FnMut([f64; 3]) -> bool,
    draw: &mut impl FnMut(u32) -> u32,
) -> EndermanEscape {
    let reject_damage = matches!(hazard, EndermanHazard::Projectile);
    let origin_y = position[1].floor() as i32;
    let min_ground = world_y.start.max(origin_y.saturating_sub(33));
    for _ in 0..64 {
        let x = position[0] + 64.0 * mob_behavior_random_unit(draw) - 32.0;
        let y = origin_y.saturating_add(draw(64) as i32 - 32);
        let z = position[2] + 64.0 * mob_behavior_random_unit(draw) - 32.0;
        let bx = x.floor() as i32;
        let bz = z.floor() as i32;
        if !world_y.contains(&y) {
            continue;
        }
        for ground_y in (min_ground..=y).rev() {
            let ground = block_at([bx, ground_y, bz]);
            if !ground.blocks_motion {
                continue;
            }
            let feet_y = ground_y + 1;
            if ground.liquid
                || feet_y > origin_y.saturating_add(32)
                || feet_y.saturating_add(2) >= world_y.end
            {
                break;
            }
            let dry = (feet_y..=feet_y + 2).all(|by| !block_at([bx, by, bz]).liquid);
            let destination = [x, f64::from(feet_y), z];
            if dry && can_occupy(destination) {
                return EndermanEscape {
                    destination: Some(destination),
                    reject_damage,
                };
            }
            break;
        }
    }
    EndermanEscape {
        destination: None,
        reject_damage,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct ShulkerBullet {
    owner: i32,
    target: i32,
    position: [f64; 3],
    velocity: [f64; 3],
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct ShulkerBulletHit {
    victim: i32,
    levitation_ticks: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ShulkerBullet {
    fn tick(&mut self, target_position: Option<[f64; 3]>) {
        if let Some(target_position) = target_position {
            let direction = mob_behavior_unit(mob_behavior_delta(self.position, target_position));
            for (velocity, direction) in self.velocity.iter_mut().zip(direction) {
                *velocity += (0.2 * direction - *velocity) * 0.2;
            }
        } else {
            self.velocity[1] -= 0.04;
        }
        for (position, velocity) in self.position.iter_mut().zip(self.velocity) {
            *position += velocity;
        }
    }

    fn hit(self, victim: i32) -> ShulkerBulletHit {
        ShulkerBulletHit {
            victim,
            levitation_ticks: 200,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
struct ShulkerBehavior {
    entity: i32,
    open_ticks: u8,
    shot_ticks: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ShulkerBehavior {
    fn new(entity: i32) -> Self {
        Self {
            entity,
            open_ticks: 0,
            shot_ticks: 0,
        }
    }

    fn peek(&self) -> u8 {
        if self.open_ticks > 0 {
            100
        } else {
            0
        }
    }

    fn armor_bonus(&self) -> u8 {
        if self.peek() == 0 {
            20
        } else {
            0
        }
    }

    fn rejects_projectile(&self) -> bool {
        self.peek() == 0
    }

    fn open(&mut self, draw: &mut impl FnMut(u32) -> u32) {
        if self.peek() == 0 {
            self.open_ticks = 20 + draw(41) as u8;
            self.shot_ticks = 20;
        }
    }

    fn tick(
        &mut self,
        position: [f64; 3],
        target: Option<i32>,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> Option<ShulkerBullet> {
        if self.open_ticks == 0 {
            return None;
        }
        self.open_ticks -= 1;
        self.shot_ticks = self.shot_ticks.saturating_sub(1);
        let target = target.filter(|_| self.shot_ticks == 0)?;
        self.shot_ticks = 20 + 10 * draw(10) as u8;
        Some(ShulkerBullet {
            owner: self.entity,
            target,
            position,
            velocity: [0.0; 3],
        })
    }
}

// Match feature::end_spikes_for_seed, whose layout function is private to worldgen.
#[cfg_attr(not(test), allow(dead_code))]
fn end_crystal_pillars(world_seed: u64) -> [feature::EndSpike; 10] {
    let layout_seed = generation_math::Lcg::new(world_seed).next_i64() as u64 & 0xffff;
    let mut layout = generation_math::Lcg::new(layout_seed);
    let mut ranks = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    for end in (2..=ranks.len()).rev() {
        ranks.swap(end - 1, layout.next_bound(end as i32) as usize);
    }
    std::array::from_fn(|index| {
        let angle = 2.0 * (-std::f64::consts::PI + std::f64::consts::PI / 10.0 * index as f64);
        let rank = ranks[index];
        feature::EndSpike {
            center_x: (42.0 * angle.cos()).floor() as i32,
            center_z: (42.0 * angle.sin()).floor() as i32,
            height: 76 + 3 * rank,
            radius: 2 + rank / 3,
            guarded: matches!(rank, 1 | 2),
        }
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn end_crystal_spawn_position(pillar: &feature::EndSpike) -> [f64; 3] {
    [
        f64::from(pillar.center_x) + 0.5,
        f64::from(pillar.height) + 1.0,
        f64::from(pillar.center_z) + 0.5,
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
struct DragonCrystal {
    entity: i32,
    position: [f64; 3],
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragonPhase {
    HoldingPattern,
    StrafePlayer,
    LandingApproach,
    Landing,
    SittingFlaming,
    SittingScanning,
    SittingAttacking,
    Takeoff,
    Dying,
    Dead,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
enum BossHit {
    Arrow,
    Other,
}

#[cfg_attr(not(test), allow(dead_code))]
struct DragonTick<'a> {
    position: [f64; 3],
    portal_y: i32,
    path_complete: bool,
    strafe_target: Option<i32>,
    strafe_aligned: bool,
    crystals: &'a [DragonCrystal],
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum DragonEvent {
    StrafeFireball { target: i32 },
    FlameBreath,
    PerchAttack,
    CompleteFight(DragonFightRewards),
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
struct DragonFightRewards {
    exit_portal_base: [i32; 3],
    dragon_egg: Option<[i32; 3]>,
    xp: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DragonFightRewards {
    // The active podium uses the same foundation and central post as the closed one.
    fn place_bedrock_exit_portal(&self, mut place: impl FnMut([i32; 3], &'static str)) {
        let [x, y, z] = self.exit_portal_base;
        for dz in -3..=3 {
            for dx in -3..=3 {
                let squared = dx * dx + dz * dz;
                if squared > 12 {
                    continue;
                }
                let inner = squared <= 6;
                place(
                    [x + dx, y - 1, z + dz],
                    if inner { "bedrock" } else { "end_stone" },
                );
                place(
                    [x + dx, y, z + dz],
                    if inner { "end_portal" } else { "bedrock" },
                );
                for dy in 1..=32 {
                    place([x + dx, y + dy, z + dz], "air");
                }
            }
        }
        for dy in 0..4 {
            place([x, y + dy, z], "bedrock");
        }
        for [dx, dz] in [[1, 0], [0, 1], [-1, 0], [0, -1]] {
            place([x + dx, y + 2, z + dz], "wall_torch");
        }
        if let Some(egg) = self.dragon_egg {
            place(egg, "dragon_egg");
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
struct EnderDragonBehavior {
    hp: f32,
    phase: DragonPhase,
    phase_ticks: u16,
    fight_ticks: u64,
    strafe_charge: u8,
    strafe_target: Option<i32>,
    linked_crystal: Option<i32>,
    death_landed: bool,
    previously_killed: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EnderDragonBehavior {
    fn new(previously_killed: bool) -> Self {
        Self {
            hp: 200.0,
            phase: DragonPhase::HoldingPattern,
            phase_ticks: 0,
            fight_ticks: 0,
            strafe_charge: 0,
            strafe_target: None,
            linked_crystal: None,
            death_landed: false,
            previously_killed,
        }
    }

    fn set_phase(&mut self, phase: DragonPhase) {
        self.phase = phase;
        self.phase_ticks = 0;
    }

    fn is_sitting(&self) -> bool {
        matches!(
            self.phase,
            DragonPhase::SittingFlaming
                | DragonPhase::SittingScanning
                | DragonPhase::SittingAttacking
        )
    }

    fn hurt(&mut self, hit: BossHit, damage: f32) -> bool {
        if self.hp == 0.0 || damage <= 0.0 || (matches!(hit, BossHit::Arrow) && self.is_sitting()) {
            return false;
        }
        self.hp = (self.hp - damage).max(0.0);
        if self.hp == 0.0 {
            self.linked_crystal = None;
            self.set_phase(DragonPhase::Dying);
        }
        true
    }

    fn crystal_destroyed(&mut self, entity: i32) -> bool {
        if self.linked_crystal != Some(entity) {
            return false;
        }
        self.linked_crystal = None;
        self.hurt(BossHit::Other, 10.0)
    }

    fn perch(portal_y: i32) -> [f64; 3] {
        [0.0, f64::from(portal_y), 0.0]
    }

    fn death_rewards(&self) -> (bool, u32) {
        if self.previously_killed {
            (false, 500)
        } else {
            (true, 12_000)
        }
    }

    fn tick(&mut self, input: DragonTick<'_>) -> Option<DragonEvent> {
        if self.phase == DragonPhase::Dead {
            return None;
        }
        let from_perch = mob_behavior_delta(Self::perch(input.portal_y), input.position);
        let near_perch = mob_behavior_dot(from_perch, from_perch) <= 1.0;
        if self.phase == DragonPhase::Dying {
            self.death_landed |= near_perch;
            if self.death_landed {
                self.phase_ticks += 1;
                if self.phase_ticks == 200 {
                    self.set_phase(DragonPhase::Dead);
                    let (spawn_egg, xp) = self.death_rewards();
                    return Some(DragonEvent::CompleteFight(DragonFightRewards {
                        exit_portal_base: [0, input.portal_y, 0],
                        dragon_egg: spawn_egg.then_some([0, input.portal_y + 4, 0]),
                        xp,
                    }));
                }
            }
            return None;
        }

        self.fight_ticks += 1;
        self.linked_crystal = input
            .crystals
            .iter()
            .filter_map(|crystal| {
                let delta = mob_behavior_delta(input.position, crystal.position);
                delta
                    .into_iter()
                    .all(|d| d.abs() <= 32.0)
                    .then_some((mob_behavior_dot(delta, delta), crystal.entity))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(_, entity)| entity);
        if self.linked_crystal.is_some() && self.fight_ticks.is_multiple_of(10) {
            self.hp = (self.hp + 1.0).min(200.0);
        }

        self.phase_ticks = self.phase_ticks.saturating_add(1);
        match self.phase {
            DragonPhase::HoldingPattern if input.path_complete => {
                self.strafe_charge = 0;
                self.strafe_target = input.strafe_target;
                self.set_phase(if self.strafe_target.is_some() {
                    DragonPhase::StrafePlayer
                } else {
                    DragonPhase::LandingApproach
                });
            }
            DragonPhase::StrafePlayer => {
                if input.strafe_target != self.strafe_target || input.path_complete {
                    self.set_phase(DragonPhase::LandingApproach);
                } else if let Some(target) = self.strafe_target {
                    if input.strafe_aligned {
                        self.strafe_charge += 1;
                        if self.strafe_charge == 5 {
                            self.set_phase(DragonPhase::LandingApproach);
                            return Some(DragonEvent::StrafeFireball { target });
                        }
                    } else {
                        self.strafe_charge = self.strafe_charge.saturating_sub(1);
                    }
                }
            }
            DragonPhase::LandingApproach if input.path_complete => {
                self.set_phase(DragonPhase::Landing)
            }
            DragonPhase::Landing if near_perch => {
                self.set_phase(DragonPhase::SittingFlaming);
                return Some(DragonEvent::FlameBreath);
            }
            DragonPhase::SittingFlaming if self.phase_ticks == 200 => {
                self.set_phase(DragonPhase::SittingScanning)
            }
            DragonPhase::SittingScanning
                if self.phase_ticks >= 25 && input.strafe_target.is_some() =>
            {
                self.set_phase(DragonPhase::SittingAttacking);
                return Some(DragonEvent::PerchAttack);
            }
            DragonPhase::SittingScanning if self.phase_ticks >= 100 => {
                self.set_phase(DragonPhase::Takeoff)
            }
            DragonPhase::SittingAttacking if self.phase_ticks >= 40 => {
                self.set_phase(DragonPhase::Takeoff)
            }
            DragonPhase::Takeoff if input.path_complete => {
                self.set_phase(DragonPhase::HoldingPattern)
            }
            _ => {}
        }
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlimeKind {
    Slime,
    MagmaCube,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlimeSize {
    Small = 1,
    Medium = 2,
    Large = 4,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlimeBehavior {
    kind: SlimeKind,
    size: SlimeSize,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SlimeBehavior {
    fn max_hp(self) -> f32 {
        let size = self.size as u8;
        f32::from(size * size)
    }

    fn attack_damage(self) -> f32 {
        f32::from(self.size as u8)
    }

    fn split_on_death(self, draw: &mut impl FnMut(u32) -> u32) -> Vec<Self> {
        let size = match self.size {
            SlimeSize::Large => SlimeSize::Medium,
            SlimeSize::Medium => SlimeSize::Small,
            SlimeSize::Small => return Vec::new(),
        };
        vec![
            Self {
                kind: self.kind,
                size
            };
            2 + draw(3) as usize
        ]
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn spider_is_hostile(daytime: bool, brightness: u8, provoked: bool) -> bool {
    provoked || !daytime || brightness < 12
}

#[cfg_attr(not(test), allow(dead_code))]
fn spider_leap_velocity(from: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    let horizontal = mob_behavior_unit([target[0] - from[0], 0.0, target[2] - from[2]]);
    [horizontal[0] * 0.4, 0.4, horizontal[2] * 0.4]
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CreeperBehavior {
    fuse: i16,
    ignited: bool,
    charged: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl CreeperBehavior {
    fn new() -> Self {
        Self {
            fuse: -1,
            ignited: false,
            charged: false,
        }
    }

    fn tick(&mut self, target_distance: Option<f64>) -> Option<f32> {
        if self.ignited || target_distance.is_some_and(|distance| distance <= 3.0) {
            self.fuse = (self.fuse + 1).max(0);
            if self.fuse >= 30 {
                return Some(self.explosion_power());
            }
        } else if target_distance.map_or(true, |distance| distance > 7.0) {
            self.fuse = (self.fuse - 1).max(-1);
        }
        None
    }

    fn ignite(&mut self, ignited: bool) {
        self.ignited = ignited;
        if ignited && self.fuse < 0 {
            self.fuse = 0;
        } else if !ignited {
            self.fuse = -1;
        }
    }

    fn explosion_power(self) -> f32 {
        if self.charged {
            6.0
        } else {
            3.0
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn skeleton_bow_charge_interval(hard_difficulty: bool) -> u8 {
    if hard_difficulty {
        20
    } else {
        40
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn skeleton_strafe_velocity(distance: f64, clockwise: bool, forward: bool) -> [f64; 3] {
    if distance < 5.0 || distance > 15.0 {
        return [0.0; 3];
    }
    let direction = if clockwise { 1.0 } else { -1.0 };
    let ahead = if forward { 1.0 } else { -1.0 };
    [direction * 0.25, 0.0, ahead * 0.25]
}

#[cfg_attr(not(test), allow(dead_code))]
fn skeleton_burns_in_sun(daytime: bool, sky_visible: bool, brightness: f32, helmet: bool) -> bool {
    daytime && sky_visible && brightness > 0.5 && !helmet
}

#[cfg_attr(not(test), allow(dead_code))]
fn phantom_circle_velocity(pos: [f64; 3], center: [f64; 3], clockwise: bool) -> [f64; 3] {
    let radial = mob_behavior_unit([pos[0] - center[0], 0.0, pos[2] - center[2]]);
    let sign = if clockwise { 1.0 } else { -1.0 };
    [
        radial[0] * -0.1 - radial[2] * sign * 0.2,
        0.0,
        radial[2] * -0.1 + radial[0] * sign * 0.2,
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
fn phantom_swoop_velocity(pos: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    let delta = mob_behavior_delta(pos, target);
    mob_behavior_unit([delta[0], delta[1] - 1.0, delta[2]])
}

#[cfg_attr(not(test), allow(dead_code))]
fn phantom_burns_in_sun(daytime: bool, sky_visible: bool, brightness: f32) -> bool {
    daytime && sky_visible && brightness > 0.5
}

#[cfg_attr(not(test), allow(dead_code))]
fn drowned_should_hide_in_water(daytime: bool, in_water: bool) -> bool {
    daytime && in_water
}

#[cfg_attr(not(test), allow(dead_code))]
fn drowned_swim_chase_velocity(pos: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    mob_behavior_unit(mob_behavior_delta(pos, target)).map(|component| component * 0.1)
}

#[cfg_attr(not(test), allow(dead_code))]
fn iron_golem_provoked(village_threat: bool, player_attack: bool) -> bool {
    village_threat || player_attack
}

#[cfg_attr(not(test), allow(dead_code))]
fn iron_golem_fling_velocity(target_pos: [f64; 3], golem_pos: [f64; 3]) -> [f64; 3] {
    let horizontal = mob_behavior_unit([
        target_pos[0] - golem_pos[0],
        0.0,
        target_pos[2] - golem_pos[2],
    ]);
    [horizontal[0] * 0.4, 0.4, horizontal[2] * 0.4]
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WitchPotion {
    WaterBreathing,
    FireResistance,
    Healing,
    Swiftness,
    Slowness,
    Poison,
    Weakness,
    Harming,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Default)]
struct WitchNeeds {
    underwater: bool,
    in_fire_or_lava: bool,
    hurt: bool,
    chase_distance: Option<f64>,
}

#[cfg_attr(not(test), allow(dead_code))]
struct WitchTarget<'a> {
    entity: i32,
    horizontal_distance: f64,
    hp: f32,
    effects: &'a [WitchPotion],
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum WitchAction {
    StartDrinking(WitchPotion),
    FinishDrinking(WitchPotion),
    Splash { target: i32, potion: WitchPotion },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct WitchBehavior {
    drinking: Option<(u8, WitchPotion)>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WitchBehavior {
    fn tick_drinking(
        &mut self,
        needs: WitchNeeds,
        active_effects: &[WitchPotion],
        draw: &mut impl FnMut(u32) -> u32,
    ) -> Option<WitchAction> {
        if let Some((ticks, potion)) = self.drinking {
            self.drinking = (ticks > 1).then_some((ticks - 1, potion));
            return (ticks == 1).then_some(WitchAction::FinishDrinking(potion));
        }
        let choices = [
            (WitchPotion::WaterBreathing, needs.underwater, 15),
            (WitchPotion::FireResistance, needs.in_fire_or_lava, 15),
            (WitchPotion::Healing, needs.hurt, 5),
            (
                WitchPotion::Swiftness,
                needs.chase_distance.is_some_and(|d| d > 11.0),
                50,
            ),
        ];
        let (potion, _, _) = choices.into_iter().find(|(potion, needed, chance)| {
            *needed && !active_effects.contains(potion) && draw(100) < *chance
        })?;
        self.drinking = Some((32, potion));
        Some(WitchAction::StartDrinking(potion))
    }

    fn splash(
        &self,
        target: WitchTarget<'_>,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> Option<WitchAction> {
        if self.drinking.is_some() {
            return None;
        }
        let effects = target.effects;
        let distance = target.horizontal_distance;
        let potion = if distance >= 8.0 && !effects.contains(&WitchPotion::Slowness) {
            WitchPotion::Slowness
        } else if target.hp >= 8.0 && !effects.contains(&WitchPotion::Poison) {
            WitchPotion::Poison
        } else if distance <= 3.0 && !effects.contains(&WitchPotion::Weakness) && draw(4) == 0 {
            WitchPotion::Weakness
        } else {
            WitchPotion::Harming
        };
        Some(WitchAction::Splash {
            target: target.entity,
            potion,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PetKind {
    Wolf,
    Cat,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PetTameResult {
    Pass,
    Consumed,
    Tamed,
}

#[cfg_attr(not(test), allow(dead_code))]
struct PetBehavior {
    kind: PetKind,
    owner: Option<[u8; 16]>,
    hp: f32,
    sitting: bool,
    collar_color: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PetBehavior {
    fn new(kind: PetKind) -> Self {
        let mut pet = Self {
            kind,
            owner: None,
            hp: 0.0,
            sitting: false,
            collar_color: 14,
        };
        pet.hp = pet.max_hp();
        pet
    }

    fn max_hp(&self) -> f32 {
        match (self.kind, self.owner) {
            (PetKind::Wolf, None) => 8.0,
            (PetKind::Wolf, Some(_)) => 40.0,
            (PetKind::Cat, _) => 10.0,
        }
    }

    fn try_tame(
        &mut self,
        player: [u8; 16],
        item: &str,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> PetTameResult {
        let correct_item = match self.kind {
            PetKind::Wolf => mob_behavior_id(item) == "bone",
            PetKind::Cat => matches!(mob_behavior_id(item), "cod" | "salmon"),
        };
        if self.owner.is_some() || !correct_item {
            return PetTameResult::Pass;
        }
        if draw(3) != 0 {
            return PetTameResult::Consumed;
        }
        self.owner = Some(player);
        self.hp = self.max_hp();
        self.sitting = true;
        PetTameResult::Tamed
    }

    fn toggle_sitting(&mut self, player: [u8; 16]) -> Option<bool> {
        if self.owner != Some(player) {
            return None;
        }
        self.sitting = !self.sitting;
        Some(self.sitting)
    }

    fn dye_collar(&mut self, player: [u8; 16], color: u8) -> bool {
        if self.owner != Some(player) || color > 15 || color == self.collar_color {
            return false;
        }
        self.collar_color = color;
        true
    }

    fn wolf_assist_target(
        &self,
        owner: [u8; 16],
        owner_hurt_by: Option<i32>,
        owner_attacked: Option<i32>,
    ) -> Option<i32> {
        if self.kind != PetKind::Wolf || self.owner != Some(owner) || self.sitting {
            return None;
        }
        owner_hurt_by.or(owner_attacked)
    }

    fn repels(&self, mob: &str, distance: f64) -> bool {
        self.kind == PetKind::Cat
            && match mob_behavior_id(mob) {
                "creeper" => distance <= 6.0,
                "phantom" => distance <= 16.0,
                _ => false,
            }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum WitherEvent {
    SpawnExplosion { power: f32 },
    Drop { item: &'static str },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WitherSkull {
    Black { target: i32 },
    Blue { target: i32 },
}

#[cfg_attr(not(test), allow(dead_code))]
struct WitherBehavior {
    hp: f32,
    spawn_ticks: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WitherBehavior {
    const MAX_HP: f32 = 300.0;
    const DEATH_DROP: &'static str = "nether_star";

    fn new() -> Self {
        Self {
            hp: Self::MAX_HP / 3.0,
            spawn_ticks: 220,
        }
    }

    fn has_wither_armor(&self) -> bool {
        self.hp > 0.0 && self.hp <= Self::MAX_HP / 2.0
    }

    fn immune_to(&self, hit: BossHit) -> bool {
        self.spawn_ticks > 0 || (self.has_wither_armor() && matches!(hit, BossHit::Arrow))
    }

    fn tick(&mut self) -> Option<WitherEvent> {
        if self.spawn_ticks == 0 {
            return None;
        }
        self.spawn_ticks -= 1;
        if self.spawn_ticks.is_multiple_of(10) {
            self.hp = (self.hp + 10.0).min(Self::MAX_HP);
        }
        (self.spawn_ticks == 0).then_some(WitherEvent::SpawnExplosion { power: 7.0 })
    }

    fn shoot(
        &self,
        target: i32,
        force_blue: bool,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> Option<WitherSkull> {
        if self.spawn_ticks > 0 || self.hp == 0.0 {
            return None;
        }
        Some(if force_blue || draw(1000) == 0 {
            WitherSkull::Blue { target }
        } else {
            WitherSkull::Black { target }
        })
    }

    fn hurt(&mut self, hit: BossHit, damage: f32) -> Option<WitherEvent> {
        if self.hp == 0.0 || damage <= 0.0 || self.immune_to(hit) {
            return None;
        }
        self.hp = (self.hp - damage).max(0.0);
        (self.hp == 0.0).then_some(WitherEvent::Drop {
            item: Self::DEATH_DROP,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct WardenAngerManagement {
    entities: HashMap<i32, u16>,
    decay_ticks: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WardenAngerManagement {
    const AGITATED: u16 = 80;
    const ROAR: u16 = 150;

    fn add_vibration(&mut self, entity: i32) -> u16 {
        let anger = self.entities.entry(entity).or_default();
        *anger = (*anger + 35).min(Self::ROAR);
        *anger
    }

    fn anger(&self, entity: i32) -> u16 {
        self.entities.get(&entity).copied().unwrap_or(0)
    }

    fn agitated(&self, entity: i32) -> bool {
        self.anger(entity) >= Self::AGITATED
    }

    fn roar_target(&self) -> Option<i32> {
        self.entities
            .iter()
            .filter(|&(_, &anger)| anger >= Self::ROAR)
            .map(|(&entity, _)| entity)
            .min()
    }

    fn tick(&mut self) {
        self.decay_ticks += 1;
        if self.decay_ticks == 20 {
            self.decay_ticks = 0;
            self.entities.retain(|_, anger| {
                *anger -= 1;
                *anger > 0
            });
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
struct WardenTarget {
    entity: i32,
    offset: [f64; 3],
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum WardenEvent {
    Roar {
        target: i32,
    },
    ChargeSonicBoom {
        target: i32,
    },
    SonicBoom {
        target: i32,
        damage: f32,
        bypass_armor: bool,
        bypass_shield: bool,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
struct WardenBehavior {
    hp: f32,
    anger: WardenAngerManagement,
    target: Option<i32>,
    sonic_charge: u8,
    sonic_cooldown: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WardenBehavior {
    fn new() -> Self {
        Self {
            hp: 500.0,
            anger: WardenAngerManagement::default(),
            target: None,
            sonic_charge: 0,
            sonic_cooldown: 0,
        }
    }

    fn hurt(&mut self, damage: f32) {
        self.hp = (self.hp - damage.max(0.0)).max(0.0);
    }

    fn forget(&mut self, entity: i32) {
        self.anger.entities.remove(&entity);
        if self.target == Some(entity) {
            self.target = None;
            self.sonic_charge = 0;
        }
    }

    fn tick(&mut self, observed_target: Option<WardenTarget>) -> Option<WardenEvent> {
        if self.hp == 0.0 {
            return None;
        }
        if let Some(target) = self.target.filter(|&target| !self.anger.agitated(target)) {
            self.forget(target);
        }
        let roar = self
            .target
            .is_none()
            .then(|| self.anger.roar_target())
            .flatten();
        self.anger.tick();
        self.sonic_cooldown = self.sonic_cooldown.saturating_sub(1);
        if let Some(target) = roar {
            self.target = Some(target);
            return Some(WardenEvent::Roar { target });
        }

        let target = self.target?;
        let in_range = observed_target.is_some_and(|observed| {
            let [x, y, z] = observed.offset;
            observed.entity == target && x * x + z * z <= 15.0 * 15.0 && y.abs() <= 20.0
        });
        if self.sonic_charge > 0 {
            self.sonic_charge -= 1;
            if self.sonic_charge == 0 {
                self.sonic_cooldown = 40;
                return in_range.then_some(WardenEvent::SonicBoom {
                    target,
                    damage: 10.0,
                    bypass_armor: true,
                    bypass_shield: true,
                });
            }
        } else if self.sonic_cooldown == 0 && in_range {
            self.sonic_charge = 34;
            return Some(WardenEvent::ChargeSonicBoom { target });
        }
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct MobTargetSnapshot {
    eid: i32,
    pos: [f64; 3],
    hp: f32,
    visible: bool,
}

/// One game tick of observations and one draw from the caller's world RNG.
#[cfg_attr(not(test), allow(dead_code))]
struct MobBehaviorTickCtx {
    dim: DimId,
    pos: [f64; 3],
    hp: f32,
    max_hp: f32,
    in_water: bool,
    in_rain: bool,
    // For Blazes this is the currently applied attack-charge flag.
    on_fire: bool,
    nearest_target: Option<MobTargetSnapshot>,
    rng_u64: u64,
}

/// World observations needed by behaviors that cannot decide from the basic snapshot.
#[cfg_attr(not(test), allow(dead_code))]
enum MobBehaviorWorldTick<'a> {
    Basic,
    Enderman {
        block_at: &'a mut dyn FnMut([i32; 3]) -> EndermanTeleportBlock,
        can_occupy: &'a mut dyn FnMut([f64; 3]) -> bool,
    },
    EnderDragon {
        portal_y: i32,
        path_complete: bool,
        strafe_aligned: bool,
        crystals: &'a [DragonCrystal],
    },
    Witch {
        eid: i32,
        active_effects: &'a [WitchPotion],
        target_effects: &'a [WitchPotion],
        throw_ready: bool,
    },
    WitherShot {
        force_blue: bool,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MobProjectileKind {
    SmallFireball,
    GhastFireball,
    ShulkerBullet { target_eid: i32 },
    DragonFireball,
    WitchSplash(WitchPotion),
    WitherSkull { blue: bool },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum MobBehaviorEffect {
    SetOnFire(bool),
    /// Direction is a unit vector (or zero at coincident positions). Power is explosive strength.
    LaunchProjectile {
        kind: MobProjectileKind,
        origin: [f64; 3],
        dir: [f64; 3],
        power: f32,
    },
    MeleeDamage {
        target_eid: i32,
        amount: f32,
        knockback_y: f64,
    },
    /// The existing sonic attack bypasses armor and shields.
    SonicBoom {
        target_eid: i32,
        damage: f32,
    },
    Explosion {
        pos: [f64; 3],
        power: f32,
    },
    Teleport {
        to: [f64; 3],
    },
    ConvertEntity {
        target_etype: i64,
    },
    DropItem {
        item: &'static str,
        count: u8,
    },
    SpawnSplitChildren {
        etype: i64,
        size: SlimeSize,
        count: u8,
    },
    ApplyPotion {
        target_eid: i32,
        effect: WitchPotion,
        duration_ticks: u16,
    },
    HealSelf(f32),
    OpenEndExitPortal {
        spawn_egg: bool,
        xp: u32,
    },
    WarningSignal,
    PiglinBarter {
        player_eid: i32,
        loot: PiglinBarterLoot,
    },
    DragonFlameBreath,
    DragonPerchAttack,
}

#[cfg_attr(not(test), allow(dead_code))]
enum MobBehaviorState {
    Blaze(BlazeBehavior),
    Ghast(GhastBehavior),
    Piglin(PiglinBehavior),
    Hoglin(HoglinBehavior),
    Enderman,
    Shulker(ShulkerBehavior),
    EnderDragon(EnderDragonBehavior),
    Slime {
        behavior: SlimeBehavior,
        split_roll: u8,
    },
    Witch(WitchBehavior),
    Wolf(PetBehavior),
    Cat(PetBehavior),
    Wither(WitherBehavior),
    Warden(WardenBehavior),
}

#[cfg_attr(not(test), allow(dead_code))]
impl MobBehaviorState {
    fn for_etype(etype: i64) -> Option<Self> {
        Some(match etype {
            8 => Self::Blaze(BlazeBehavior::new()),
            // The effect consumer supplies projectile ownership from the ticking entity.
            45 => Self::Ghast(GhastBehavior::new(0, 0.0)),
            78 => Self::Piglin(PiglinBehavior::default()),
            51 => Self::Hoglin(HoglinBehavior::default()),
            33 => Self::Enderman,
            88 => Self::Shulker(ShulkerBehavior::new(0)),
            31 => Self::EnderDragon(EnderDragonBehavior::new(false)),
            67 | 93 => Self::Slime {
                behavior: SlimeBehavior {
                    kind: if etype == 67 {
                        SlimeKind::MagmaCube
                    } else {
                        SlimeKind::Slime
                    },
                    // The generic spawner gives these mobs at least the large size's health.
                    // A size-aware caller can set this from the actual entity size.
                    size: SlimeSize::Large,
                },
                split_roll: 0,
            },
            118 => Self::Witch(WitchBehavior::default()),
            122 => Self::Wolf(PetBehavior::new(PetKind::Wolf)),
            15 => Self::Cat(PetBehavior::new(PetKind::Cat)),
            119 => Self::Wither(WitherBehavior::new()),
            116 => Self::Warden(WardenBehavior::new()),
            _ => return None,
        })
    }

    fn step(&mut self, ctx: &MobBehaviorTickCtx) -> Vec<MobBehaviorEffect> {
        self.step_with_world(ctx, MobBehaviorWorldTick::Basic)
    }

    fn step_with_world(
        &mut self,
        ctx: &MobBehaviorTickCtx,
        world: MobBehaviorWorldTick<'_>,
    ) -> Vec<MobBehaviorEffect> {
        use MobBehaviorEffect::*;

        if ctx.hp <= 0.0 && !matches!(self, Self::EnderDragon(_)) {
            return Vec::new();
        }
        let target = ctx.nearest_target;
        let visible_target = target.filter(|target| target.visible);
        let launch = |kind, target: MobTargetSnapshot, power| LaunchProjectile {
            kind,
            origin: ctx.pos,
            dir: mob_behavior_unit(mob_behavior_delta(ctx.pos, target.pos)),
            power,
        };
        let mut rng = ctx.rng_u64;
        let mut draw = |bound| {
            let value = (rng % u64::from(bound)) as u32;
            // Advance a local xorshift stream for behaviors that need multiple draws.
            if rng == 0 {
                rng = 0x9E37_79B9_7F4A_7C15;
            }
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            value
        };
        let mut effects = Vec::new();
        let effect = match self {
            Self::Blaze(blaze) => {
                let attack = blaze.tick(visible_target.map(|target| {
                    let delta = mob_behavior_delta(ctx.pos, target.pos);
                    (target.eid, mob_behavior_dot(delta, delta).sqrt())
                }));
                if blaze.on_fire() != ctx.on_fire {
                    effects.push(SetOnFire(blaze.on_fire()));
                }
                match attack {
                    Some(BlazeAttack::Melee { target, damage }) => Some(MeleeDamage {
                        target_eid: target,
                        amount: damage,
                        knockback_y: 0.0,
                    }),
                    Some(BlazeAttack::SmallFireball { .. }) => visible_target
                        .map(|target| launch(MobProjectileKind::SmallFireball, target, 0.0)),
                    None => None,
                }
            }
            Self::Ghast(ghast) => {
                let delta = target.map(|target| mob_behavior_delta(ctx.pos, target.pos));
                let can_fire = visible_target.is_some()
                    && delta.is_some_and(|delta| mob_behavior_dot(delta, delta) < 64.0 * 64.0);
                match ghast.tick(delta, can_fire) {
                    Some(GhastAttack::Warning) => Some(WarningSignal),
                    Some(GhastAttack::Fire(fireball)) => Some(LaunchProjectile {
                        kind: MobProjectileKind::GhastFireball,
                        origin: ctx.pos,
                        dir: fireball.velocity,
                        power: f32::from(fireball.explosion_power),
                    }),
                    None => None,
                }
            }
            Self::Piglin(piglin) => match piglin.tick(ctx.dim == DimId::Nether, &mut draw) {
                Some(PiglinEvent::ConvertToZombifiedPiglin) => {
                    Some(ConvertEntity { target_etype: 127 })
                }
                Some(PiglinEvent::Barter { player, loot }) => Some(PiglinBarter {
                    player_eid: player,
                    loot,
                }),
                None => None,
            },
            Self::Hoglin(hoglin) => hoglin
                .tick(ctx.dim == DimId::Nether)
                .map(|HoglinEvent::ConvertToZoglin| ConvertEntity { target_etype: 123 }),
            Self::Enderman => {
                let MobBehaviorWorldTick::Enderman {
                    block_at,
                    can_occupy,
                } = world
                else {
                    return effects;
                };
                let hazard = if ctx.in_water {
                    EndermanHazard::Water
                } else if ctx.in_rain {
                    EndermanHazard::Rain
                } else {
                    return effects;
                };
                let bottom = ctx.dim.min_y();
                enderman_escape(
                    hazard,
                    ctx.pos,
                    bottom..bottom + ctx.dim.height(),
                    block_at,
                    can_occupy,
                    &mut draw,
                )
                .destination
                .map(|to| Teleport { to })
            }
            Self::Shulker(shulker) => {
                if visible_target.is_some() {
                    shulker.open(&mut draw);
                }
                shulker
                    .tick(ctx.pos, visible_target.map(|target| target.eid), &mut draw)
                    .and_then(|bullet| {
                        visible_target.map(|target| {
                            launch(
                                MobProjectileKind::ShulkerBullet {
                                    target_eid: bullet.target,
                                },
                                target,
                                0.0,
                            )
                        })
                    })
            }
            Self::EnderDragon(dragon) => {
                let MobBehaviorWorldTick::EnderDragon {
                    portal_y,
                    path_complete,
                    strafe_aligned,
                    crystals,
                } = world
                else {
                    return effects;
                };
                if !matches!(dragon.phase, DragonPhase::Dying | DragonPhase::Dead) {
                    if ctx.hp <= 0.0 {
                        dragon.hurt(BossHit::Other, dragon.hp);
                    } else {
                        dragon.hp = ctx.hp;
                    }
                }
                let hp_before = dragon.hp;
                let event = dragon.tick(DragonTick {
                    position: ctx.pos,
                    portal_y,
                    path_complete,
                    strafe_target: target.map(|target| target.eid),
                    strafe_aligned,
                    crystals,
                });
                let healed = dragon.hp.min(ctx.max_hp) - hp_before;
                if healed > 0.0 {
                    effects.push(HealSelf(healed));
                }
                match event {
                    Some(DragonEvent::StrafeFireball { target: eid }) => target
                        .filter(|target| target.eid == eid)
                        .map(|target| launch(MobProjectileKind::DragonFireball, target, 0.0)),
                    Some(DragonEvent::FlameBreath) => Some(DragonFlameBreath),
                    Some(DragonEvent::PerchAttack) => Some(DragonPerchAttack),
                    Some(DragonEvent::CompleteFight(rewards)) => Some(OpenEndExitPortal {
                        spawn_egg: rewards.dragon_egg.is_some(),
                        xp: rewards.xp,
                    }),
                    None => None,
                }
            }
            Self::Slime { split_roll, .. } => {
                *split_roll = draw(3) as u8;
                None
            }
            Self::Witch(witch) => {
                let MobBehaviorWorldTick::Witch {
                    eid,
                    active_effects,
                    target_effects,
                    throw_ready,
                } = world
                else {
                    return effects;
                };
                let needs = WitchNeeds {
                    underwater: ctx.in_water,
                    in_fire_or_lava: ctx.on_fire,
                    hurt: ctx.hp <= ctx.max_hp * 0.5,
                    chase_distance: target.map(|target| {
                        let delta = mob_behavior_delta(ctx.pos, target.pos);
                        mob_behavior_dot(delta, delta).sqrt()
                    }),
                };
                match witch.tick_drinking(needs, active_effects, &mut draw) {
                    Some(WitchAction::FinishDrinking(WitchPotion::Healing)) => {
                        let amount = (ctx.max_hp - ctx.hp).min(4.0);
                        (amount > 0.0).then_some(HealSelf(amount))
                    }
                    Some(WitchAction::FinishDrinking(effect)) => Some(ApplyPotion {
                        target_eid: eid,
                        effect,
                        duration_ticks: 3600,
                    }),
                    None if throw_ready => visible_target.and_then(|target| {
                        let [x, _, z] = mob_behavior_delta(ctx.pos, target.pos);
                        let action = witch.splash(
                            WitchTarget {
                                entity: target.eid,
                                horizontal_distance: x.hypot(z),
                                hp: target.hp,
                                effects: target_effects,
                            },
                            &mut draw,
                        );
                        match action {
                            Some(WitchAction::Splash { potion, .. }) => {
                                Some(launch(MobProjectileKind::WitchSplash(potion), target, 0.0))
                            }
                            _ => None,
                        }
                    }),
                    _ => None,
                }
            }
            Self::Wolf(_) | Self::Cat(_) => None, // Pet interactions are event-driven.
            Self::Wither(wither) => {
                wither.hp = ctx.hp;
                let event = wither.tick();
                let healed = wither.hp.min(ctx.max_hp) - ctx.hp;
                if healed > 0.0 {
                    effects.push(HealSelf(healed));
                }
                match event {
                    Some(WitherEvent::SpawnExplosion { power }) => Some(Explosion {
                        pos: ctx.pos,
                        power,
                    }),
                    _ => {
                        let (Some(target), MobBehaviorWorldTick::WitherShot { force_blue }) =
                            (visible_target, world)
                        else {
                            return effects;
                        };
                        wither
                            .shoot(target.eid, force_blue, &mut draw)
                            .map(|skull| {
                                let blue = matches!(skull, WitherSkull::Blue { .. });
                                launch(MobProjectileKind::WitherSkull { blue }, target, 1.0)
                            })
                    }
                }
            }
            Self::Warden(warden) => {
                warden.hp = ctx.hp;
                let observed = target.map(|target| WardenTarget {
                    entity: target.eid,
                    offset: mob_behavior_delta(ctx.pos, target.pos),
                });
                match warden.tick(observed) {
                    Some(WardenEvent::Roar { .. } | WardenEvent::ChargeSonicBoom { .. }) => {
                        Some(WarningSignal)
                    }
                    Some(WardenEvent::SonicBoom { target, damage, .. }) => Some(SonicBoom {
                        target_eid: target,
                        damage,
                    }),
                    None => None,
                }
            }
        };
        effects.extend(effect);
        effects
    }

    /// Call once when removing the dead entity. A Dragon fight completed by the
    /// world-aware tick already emitted its portal and rewards.
    fn on_death(&self) -> Vec<MobBehaviorEffect> {
        use MobBehaviorEffect::*;

        let effect = match self {
            Self::Slime {
                behavior,
                split_roll,
            } => {
                let children = behavior.split_on_death(&mut |_| u32::from(*split_roll));
                children.first().map(|child| SpawnSplitChildren {
                    etype: match child.kind {
                        SlimeKind::Slime => 93,
                        SlimeKind::MagmaCube => 67,
                    },
                    size: child.size,
                    count: children.len() as u8,
                })
            }
            Self::Wither(_) => Some(DropItem {
                item: WitherBehavior::DEATH_DROP,
                count: 1,
            }),
            Self::EnderDragon(dragon) if dragon.phase != DragonPhase::Dead => {
                let (spawn_egg, xp) = dragon.death_rewards();
                Some(OpenEndExitPortal { spawn_egg, xp })
            }
            _ => None,
        };
        effect.into_iter().collect()
    }
}

#[cfg(test)]
mod mob_behaviors_tests {
    use super::*;

    #[test]
    fn dispatch_selects_the_protocol_767_behavior() {
        for etype in [8, 45, 78, 51, 33, 88, 31, 93, 67, 118, 122, 15, 119, 116] {
            let name = match MobBehaviorState::for_etype(etype).unwrap() {
                MobBehaviorState::Blaze(_) => "minecraft:blaze",
                MobBehaviorState::Ghast(_) => "minecraft:ghast",
                MobBehaviorState::Piglin(_) => "minecraft:piglin",
                MobBehaviorState::Hoglin(_) => "minecraft:hoglin",
                MobBehaviorState::Enderman => "minecraft:enderman",
                MobBehaviorState::Shulker(_) => "minecraft:shulker",
                MobBehaviorState::EnderDragon(_) => "minecraft:ender_dragon",
                MobBehaviorState::Slime { behavior, .. } => match behavior.kind {
                    SlimeKind::Slime => "minecraft:slime",
                    SlimeKind::MagmaCube => "minecraft:magma_cube",
                },
                MobBehaviorState::Witch(_) => "minecraft:witch",
                MobBehaviorState::Wolf(pet) if pet.kind == PetKind::Wolf => "minecraft:wolf",
                MobBehaviorState::Cat(pet) if pet.kind == PetKind::Cat => "minecraft:cat",
                MobBehaviorState::Wither(_) => "minecraft:wither",
                MobBehaviorState::Warden(_) => "minecraft:warden",
                _ => panic!("incorrect pet behavior for entity type {etype}"),
            };
            assert_eq!(entity_spec_by_id(etype).unwrap().name, name);
        }
        for etype in [-1, 79, 123, 124, 127, 130] {
            assert!(MobBehaviorState::for_etype(etype).is_none(), "type {etype}");
        }
    }

    fn dispatch_ctx() -> MobBehaviorTickCtx {
        MobBehaviorTickCtx {
            dim: DimId::Overworld,
            pos: [10.0, 80.0, -5.0],
            hp: 20.0,
            max_hp: 20.0,
            in_water: false,
            in_rain: false,
            on_fire: false,
            nearest_target: Some(MobTargetSnapshot {
                eid: 7,
                pos: [10.0, 83.0, -1.0],
                hp: 20.0,
                visible: true,
            }),
            rng_u64: 0,
        }
    }

    #[test]
    fn dispatch_blaze_emits_charge_flag_and_three_fireballs() {
        use MobBehaviorEffect::*;

        let mut state = MobBehaviorState::for_etype(8).unwrap();
        let mut ctx = dispatch_ctx();
        let shot = || LaunchProjectile {
            kind: MobProjectileKind::SmallFireball,
            origin: [10.0, 80.0, -5.0],
            dir: [0.0, 0.6, 0.8],
            power: 0.0,
        };
        for tick in 1..=233 {
            let expected = match tick {
                60 => vec![SetOnFire(true)],
                121 | 127 => vec![shot()],
                133 => vec![SetOnFire(false), shot()],
                _ => vec![],
            };
            assert_eq!(state.step(&ctx), expected, "tick {tick}");
            if tick == 60 || tick == 133 {
                ctx.on_fire = tick == 60;
            }
        }
        ctx.nearest_target.as_mut().unwrap().pos = [10.0, 80.0, -4.0];
        assert_eq!(
            state.step(&ctx),
            [MeleeDamage {
                target_eid: 7,
                amount: 6.0,
                knockback_y: 0.0,
            }]
        );
    }

    #[test]
    fn dispatch_ghast_warns_and_fires_only_at_a_visible_target_in_range() {
        use MobBehaviorEffect::*;

        let mut state = MobBehaviorState::for_etype(45).unwrap();
        let mut ctx = dispatch_ctx();
        let target = ctx.nearest_target.unwrap();
        for unavailable in [
            MobTargetSnapshot {
                visible: false,
                ..target
            },
            MobTargetSnapshot {
                pos: [74.0, 80.0, -5.0],
                ..target
            },
        ] {
            ctx.nearest_target = Some(unavailable);
            for _ in 0..20 {
                assert!(state.step(&ctx).is_empty());
            }
        }
        ctx.nearest_target = Some(target);
        for tick in 1..=20 {
            let expected = match tick {
                10 => vec![WarningSignal],
                20 => vec![LaunchProjectile {
                    kind: MobProjectileKind::GhastFireball,
                    origin: [10.0, 80.0, -5.0],
                    dir: [0.0, 0.6, 0.8],
                    power: 1.0,
                }],
                _ => vec![],
            };
            assert_eq!(state.step(&ctx), expected, "tick {tick}");
        }
        assert!(state.step(&ctx).is_empty());
    }

    #[test]
    fn dispatch_nether_mob_conversions_require_301_consecutive_unsafe_ticks() {
        for (etype, converted_etype, converted_name) in [
            (78, 127, "minecraft:zombified_piglin"),
            (51, 123, "minecraft:zoglin"),
        ] {
            assert_eq!(
                entity_spec_by_id(converted_etype).unwrap().name,
                converted_name
            );
            let mut state = MobBehaviorState::for_etype(etype).unwrap();
            let mut ctx = dispatch_ctx();
            ctx.nearest_target = None;
            for (dim, ticks) in [
                (DimId::Overworld, 300),
                (DimId::Nether, 400),
                (DimId::Overworld, 300),
            ] {
                ctx.dim = dim;
                for _ in 0..ticks {
                    assert!(state.step(&ctx).is_empty(), "type {etype} in {dim:?}");
                }
            }
            assert_eq!(
                state.step(&ctx),
                [MobBehaviorEffect::ConvertEntity {
                    target_etype: converted_etype
                }]
            );
            assert!(state.step(&ctx).is_empty());
        }
    }

    #[test]
    fn dispatch_piglin_barter_preserves_the_potion_in_the_loot() {
        let mut state = MobBehaviorState::for_etype(78).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.dim = DimId::Nether;
        ctx.rng_u64 = 13;
        let MobBehaviorState::Piglin(piglin) = &mut state else {
            panic!("expected Piglin behavior");
        };
        assert!(piglin.offer(7, "gold_ingot"));
        for _ in 0..119 {
            assert!(state.step(&ctx).is_empty());
        }
        assert_eq!(
            state.step(&ctx),
            [MobBehaviorEffect::PiglinBarter {
                player_eid: 7,
                loot: PiglinBarterLoot {
                    item: "potion",
                    count: 1,
                    potion: Some("fire_resistance"),
                    soul_speed: None,
                },
            }]
        );
        assert!(state.step(&ctx).is_empty());
    }

    #[test]
    fn dispatch_shulker_launches_a_bullet_bound_to_its_target() {
        let mut state = MobBehaviorState::for_etype(88).unwrap();
        let ctx = dispatch_ctx();
        for _ in 0..19 {
            assert!(state.step(&ctx).is_empty());
        }
        assert_eq!(
            state.step(&ctx),
            [MobBehaviorEffect::LaunchProjectile {
                kind: MobProjectileKind::ShulkerBullet { target_eid: 7 },
                origin: [10.0, 80.0, -5.0],
                dir: [0.0, 0.6, 0.8],
                power: 0.0,
            }]
        );
    }

    #[test]
    fn dispatch_wither_heals_during_invulnerability_then_explodes_at_tick_220() {
        use MobBehaviorEffect::*;

        let mut state = MobBehaviorState::for_etype(119).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.hp = 100.0;
        ctx.max_hp = 300.0;
        for tick in 1..220 {
            if tick % 10 == 0 && ctx.hp < ctx.max_hp {
                assert_eq!(state.step(&ctx), [HealSelf(10.0)], "tick {tick}");
                ctx.hp += 10.0;
            } else {
                assert!(state.step(&ctx).is_empty(), "tick {tick}");
            }
        }
        assert_eq!(ctx.hp, 300.0);
        assert_eq!(
            state.step(&ctx),
            [Explosion {
                pos: [10.0, 80.0, -5.0],
                power: 7.0
            }]
        );
        assert!(state.step(&ctx).is_empty());

        let world = MobBehaviorWorldTick::WitherShot { force_blue: true };
        assert_eq!(
            state.step_with_world(&ctx, world),
            [LaunchProjectile {
                kind: MobProjectileKind::WitherSkull { blue: true },
                origin: [10.0, 80.0, -5.0],
                dir: [0.0, 0.6, 0.8],
                power: 1.0,
            }]
        );
    }

    #[test]
    fn dispatch_warden_sonic_boom_targets_the_mob_that_triggered_its_anger() {
        use MobBehaviorEffect::*;

        let mut state = MobBehaviorState::for_etype(116).unwrap();
        let mut ctx = dispatch_ctx();
        let MobBehaviorState::Warden(warden) = &mut state else {
            panic!("expected Warden behavior");
        };
        for _ in 0..5 {
            warden.anger.add_vibration(7);
        }
        assert_eq!(state.step(&ctx), [WarningSignal]);
        ctx.nearest_target.as_mut().unwrap().eid = 8;
        assert!(state.step(&ctx).is_empty());
        let target = ctx.nearest_target.as_mut().unwrap();
        target.eid = 7;
        target.visible = false; // The sonic attack can pass through blocks.
        assert_eq!(state.step(&ctx), [WarningSignal]);
        for _ in 0..33 {
            assert!(state.step(&ctx).is_empty());
        }
        assert_eq!(
            state.step(&ctx),
            [SonicBoom {
                target_eid: 7,
                damage: 10.0
            }]
        );
        assert!(state.step(&ctx).is_empty());
    }

    #[test]
    fn dispatch_death_effects_preserve_slime_kind_and_boss_rewards() {
        use MobBehaviorEffect::*;

        for etype in [93, 67] {
            let mut slime = MobBehaviorState::for_etype(etype).unwrap();
            let mut ctx = dispatch_ctx();
            ctx.rng_u64 = 2;
            assert!(slime.step(&ctx).is_empty());
            assert_eq!(
                slime.on_death(),
                [SpawnSplitChildren {
                    etype,
                    size: SlimeSize::Medium,
                    count: 4
                }]
            );
            let MobBehaviorState::Slime { behavior, .. } = &mut slime else {
                panic!("expected Slime behavior");
            };
            behavior.size = SlimeSize::Small;
            assert!(slime.on_death().is_empty());
        }
        assert_eq!(
            MobBehaviorState::for_etype(119).unwrap().on_death(),
            [DropItem {
                item: "nether_star",
                count: 1
            }]
        );
        assert_eq!(
            MobBehaviorState::for_etype(31).unwrap().on_death(),
            [OpenEndExitPortal {
                spawn_egg: true,
                xp: 12_000
            }]
        );
        let repeat_kill = MobBehaviorState::EnderDragon(EnderDragonBehavior::new(true));
        assert_eq!(
            repeat_kill.on_death(),
            [OpenEndExitPortal {
                spawn_egg: false,
                xp: 500
            }]
        );
        assert!(MobBehaviorState::for_etype(8)
            .unwrap()
            .on_death()
            .is_empty());
    }

    #[test]
    fn dispatch_enderman_emits_only_a_checked_teleport_destination() {
        let mut state = MobBehaviorState::for_etype(33).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.in_rain = true;
        let mut block_at = |[_, y, _]: [i32; 3]| EndermanTeleportBlock {
            blocks_motion: y == 48,
            liquid: false,
        };
        let mut can_occupy = |_| false;
        assert!(state.step(&ctx).is_empty());
        assert!(state
            .step_with_world(
                &ctx,
                MobBehaviorWorldTick::Enderman {
                    block_at: &mut block_at,
                    can_occupy: &mut can_occupy,
                }
            )
            .is_empty());
        let mut can_occupy = |[_, y, _]: [f64; 3]| y == 49.0;
        assert!(matches!(
            state
                .step_with_world(
                    &ctx,
                    MobBehaviorWorldTick::Enderman {
                        block_at: &mut block_at,
                        can_occupy: &mut can_occupy,
                    }
                )
                .as_slice(),
            [MobBehaviorEffect::Teleport { to: [_, 49.0, _] }]
        ));
    }

    #[test]
    fn dispatch_dragon_animation_awards_the_portal_only_once() {
        let mut state = MobBehaviorState::for_etype(31).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.dim = DimId::End;
        ctx.pos = [0.0, 80.0, 0.0];
        ctx.hp = 0.0;
        ctx.max_hp = 200.0;
        let world = || MobBehaviorWorldTick::EnderDragon {
            portal_y: 80,
            path_complete: false,
            strafe_aligned: false,
            crystals: &[],
        };
        for _ in 0..199 {
            assert!(state.step_with_world(&ctx, world()).is_empty());
        }
        assert_eq!(
            state.step_with_world(&ctx, world()),
            [MobBehaviorEffect::OpenEndExitPortal {
                spawn_egg: true,
                xp: 12_000
            }]
        );
        assert!(state.step_with_world(&ctx, world()).is_empty());
        assert!(state.on_death().is_empty());
    }

    #[test]
    fn dispatch_witch_finishes_drinking_before_applying_a_potion() {
        let mut state = MobBehaviorState::for_etype(118).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.in_water = true;
        let world = || MobBehaviorWorldTick::Witch {
            eid: 42,
            active_effects: &[],
            target_effects: &[],
            throw_ready: true,
        };
        for _ in 0..32 {
            assert!(state.step_with_world(&ctx, world()).is_empty());
        }
        assert_eq!(
            state.step_with_world(&ctx, world()),
            [MobBehaviorEffect::ApplyPotion {
                target_eid: 42,
                effect: WitchPotion::WaterBreathing,
                duration_ticks: 3600,
            }]
        );
    }

    fn zero(_: u32) -> u32 {
        0
    }

    fn highest(bound: u32) -> u32 {
        bound - 1
    }

    fn no_draw(bound: u32) -> u32 {
        panic!("unexpected random draw with bound {bound}")
    }

    fn rolls<const N: usize>(values: [u32; N]) -> impl FnMut(u32) -> u32 {
        let mut values = values.into_iter();
        move |bound| {
            let value = values.next().expect("scripted roll");
            assert!(value < bound, "roll {value} outside 0..{bound}");
            value
        }
    }

    #[test]
    fn blaze_charges_fires_three_times_and_cools_down() {
        let mut blaze = BlazeBehavior::new();
        let target = Some((7, 12.0));
        let mut shots = vec![];
        for tick in 1..=293 {
            if let Some(attack) = blaze.tick(target) {
                assert_eq!(attack, BlazeAttack::SmallFireball { target: 7 });
                shots.push(tick);
            }
            assert_eq!(
                blaze.on_fire(),
                (60..133).contains(&tick) || tick == 293,
                "charge flag at tick {tick}"
            );
        }
        assert_eq!(shots, [121, 127, 133]);
        for _ in 294..=353 {
            assert_eq!(blaze.tick(target), None);
        }
        assert_eq!(
            blaze.tick(target),
            Some(BlazeAttack::SmallFireball { target: 7 })
        );
    }

    #[test]
    fn blaze_melee_and_vulnerabilities() {
        let mut blaze = BlazeBehavior::new();
        assert_eq!(
            blaze.tick(Some((7, 2.0))),
            Some(BlazeAttack::Melee {
                target: 7,
                damage: 6.0
            })
        );
        assert_eq!(blaze.tick(Some((7, 1.0))), None);
        assert_eq!(blaze.tick(None), None);
        assert_eq!(blaze_hazard_damage(BlazeHazard::Water), 1.0);
        assert_eq!(blaze_hazard_damage(BlazeHazard::Snowball), 3.0);
    }

    #[test]
    fn ghast_hovers_warns_and_fires_a_deflectable_fireball() {
        let mut ghast = GhastBehavior::new(10, 80.0);
        assert!(ghast.hover_velocity(79.0, 0.0) > 0.0);
        assert!(ghast.hover_velocity(81.0, 0.0) < 0.0);
        assert_eq!(ghast.hover_velocity(80.0, 0.0), 0.0);
        let target = Some([0.0, 3.0, 4.0]);
        for tick in 1..20 {
            assert_eq!(
                ghast.tick(target, true),
                (tick == 10).then_some(GhastAttack::Warning)
            );
            assert_eq!(ghast.charging(), tick > 10);
        }
        let Some(GhastAttack::Fire(mut fireball)) = ghast.tick(target, true) else {
            panic!("expected a shot at +20");
        };
        assert!(!ghast.charging());
        assert_eq!(fireball.explosion_power, 1);
        assert_eq!(fireball.owner, 10);
        assert!(!fireball.returns_to_sender(10));
        fireball.deflect(23);
        assert_eq!(fireball.velocity, [0.0, -0.6, -0.8]);
        assert_eq!(fireball.owner, 23);
        assert!(fireball.returns_to_sender(10));
        assert!(!fireball.returns_to_sender(11));
        assert_eq!(ghast.attack_tick, -40);
        for _ in -39..10 {
            assert_eq!(ghast.tick(target, true), None);
        }
        assert_eq!(ghast.tick(target, true), Some(GhastAttack::Warning));
        assert!(!ghast.charging());
        assert_eq!(ghast.tick(target, false), None);
        assert_eq!(ghast.attack_tick, 9);
        assert!(!ghast.charging());
        assert_eq!(ghast.tick(target, true), Some(GhastAttack::Warning));
        for _ in 0..9 {
            assert_eq!(ghast.tick(target, true), None);
        }
        let Some(GhastAttack::Fire(_)) = ghast.tick(target, true) else {
            panic!("expected second shot at +20");
        };
        assert_eq!(ghast.attack_tick, -40);
        assert!(!ghast.charging());
        assert_eq!(ghast.tick(target, false), None);
        assert_eq!(ghast.attack_tick, -40);

        for _ in -39..=19 {
            ghast.tick(target, true);
        }
        assert!(ghast.charging());
        assert_eq!(ghast.tick(None, false), None);
        assert!(!ghast.charging());
        assert_eq!(ghast.tick(target, true), None);
        assert_eq!(ghast.attack_tick, 1);
    }

    #[test]
    fn piglin_gold_armor_pacifies_until_a_guarded_interaction() {
        let mut piglin = PiglinBehavior::default();
        let armor = ["diamond_helmet", "minecraft:golden_boots"];
        assert!(!piglin.hostile_to(11, &armor));
        assert!(piglin.hostile_to(11, &["diamond_helmet"]));
        assert!(!piglin.provoke(11, 16.01, PiglinProvocation::Opened("chest")));
        assert!(!piglin.provoke(11, 1.0, PiglinProvocation::Mined("stone")));
        assert!(piglin.provoke(11, 16.0, PiglinProvocation::Opened("chest")));
        assert_eq!(piglin.anger_ticks(), 600);
        assert!(piglin.hostile_to(11, &armor));
        assert!(!piglin.hostile_to(12, &armor));
        for _ in 0..599 {
            assert_eq!(piglin.tick(true, &mut no_draw), None);
        }
        assert_eq!(piglin.anger_ticks(), 1);
        assert!(piglin.hostile_to(11, &armor));
        assert_eq!(piglin.tick(true, &mut no_draw), None);
        assert!(!piglin.hostile_to(11, &armor));
        assert!(piglin.provoke(
            12,
            15.0,
            PiglinProvocation::Mined("minecraft:gilded_blackstone")
        ));
        assert!(piglin.hostile_to(12, &armor));
        for block in [
            "barrel",
            "ender_chest",
            "shulker_box",
            "red_shulker_box",
            "trapped_chest",
        ] {
            assert!(
                piglin.provoke(11, 1.0, PiglinProvocation::Opened(block)),
                "{block}"
            );
        }
    }

    #[test]
    fn piglin_barter_consumes_one_ingot_and_resolves_at_tick_120() {
        let mut piglin = PiglinBehavior::default();
        assert!(!piglin.offer(3, "gold_nugget"));
        assert!(piglin.offer(3, "minecraft:gold_ingot"));
        assert!(!piglin.offer(4, "gold_ingot"));
        for _ in 0..119 {
            assert_eq!(piglin.tick(true, &mut no_draw), None);
        }
        assert_eq!(
            piglin.tick(true, &mut rolls([0, 2])),
            Some(PiglinEvent::Barter {
                player: 3,
                loot: PiglinBarterLoot {
                    item: "enchanted_book",
                    count: 1,
                    potion: None,
                    soul_speed: Some(3)
                },
            })
        );
        assert_eq!(piglin.tick(true, &mut no_draw), None);
        assert!(piglin.offer(4, "gold_ingot"));
    }

    #[test]
    fn piglin_barter_pool_keeps_weights_ranges_and_item_components() {
        let total: u32 = PIGLIN_BARTERING.iter().map(|entry| entry.1).sum();
        assert_eq!(total, 459);
        let mut start = 0;
        for &(item, weight, min, max, metadata) in PIGLIN_BARTERING {
            for (index, count_roll, count) in [
                (start, 0, min),
                (start + weight - 1, u32::from(max - min), max),
            ] {
                let loot = roll_piglin_barter(&mut rolls([index, count_roll]));
                assert_eq!((loot.item, loot.count), (item, count));
                match metadata {
                    PiglinBarterMetadata::Plain => {
                        assert_eq!((loot.potion, loot.soul_speed), (None, None))
                    }
                    PiglinBarterMetadata::SoulSpeed => assert_eq!(loot.soul_speed, Some(1)),
                    PiglinBarterMetadata::Potion(potion) => assert_eq!(loot.potion, Some(potion)),
                }
            }
            start += weight;
        }
        assert_eq!(roll_piglin_barter(&mut highest).count, 16);
    }

    #[test]
    fn piglin_zombification_happens_on_unsafe_tick_301() {
        let mut piglin = PiglinBehavior::default();
        for _ in 0..299 {
            assert_eq!(piglin.tick(false, &mut no_draw), None);
        }
        assert_eq!(piglin.tick(true, &mut no_draw), None);
        for _ in 0..300 {
            assert_eq!(piglin.tick(false, &mut no_draw), None);
        }
        assert_eq!(
            piglin.tick(false, &mut no_draw),
            Some(PiglinEvent::ConvertToZombifiedPiglin)
        );
        assert_eq!(piglin.tick(false, &mut no_draw), None);
        assert_eq!(piglin.tick(true, &mut no_draw), None);
        assert!(!piglin.offer(4, "gold_ingot"));
    }

    #[test]
    fn hoglin_flees_warped_fungus_flings_and_converts() {
        let mut hoglin = HoglinBehavior::default();
        assert_eq!(hoglin.knockback_resistance(), 0.6);
        assert_eq!(
            hoglin.flee_from([0.0; 3], &[([7.01, 0.0, 0.0], "warped_fungus")]),
            None
        );
        assert_eq!(
            hoglin.flee_from([0.0; 3], &[([7.0, 0.0, 0.0], "minecraft:warped_fungus")]),
            Some([-1.0, 0.0, 0.0])
        );
        let impulse = hoglin.fling(
            [1.0, 2.0, 0.0],
            1.0,
            0.2,
            &mut rolls([10, 1 << 23, 1 << 23]),
        );
        assert!((impulse[0] - 0.36).abs() < 1e-12);
        assert_eq!(impulse[1], 0.2);
        assert_eq!(impulse[2], 0.0);
        assert_eq!(hoglin.fling([1.0; 3], 1.0, 1.0, &mut no_draw), [0.0; 3]);
        for _ in 0..300 {
            assert_eq!(hoglin.tick(false), None);
        }
        assert_eq!(hoglin.tick(false), Some(HoglinEvent::ConvertToZoglin));
        assert_eq!(hoglin.tick(false), None);
    }

    #[test]
    fn enderman_stare_uses_eye_cone_range_pumpkin_and_line_of_sight() {
        let player = [0.0, 1.6, 0.0];
        let enderman = [32.0, 2.6, 0.0];
        let look = [32.0, 1.0, 0.0];
        assert!(enderman_stared_at(player, look, enderman, "air", true));
        assert!(!enderman_stared_at(
            player,
            look,
            enderman,
            "minecraft:carved_pumpkin",
            true
        ));
        assert!(!enderman_stared_at(player, look, enderman, "air", false));
        assert!(!enderman_stared_at(
            player,
            [32.0, 1.0, 2.0],
            enderman,
            "air",
            true
        ));
        assert!(!enderman_stared_at(
            player,
            [1.0, 0.0, 0.0],
            [64.01, 1.6, 0.0],
            "air",
            true
        ));
        assert!(!enderman_stared_at(
            player,
            [0.0; 3],
            [0.01, 1.6, 0.0],
            "air",
            true
        ));
    }

    #[test]
    fn enderman_rejects_projectiles_and_retries_wet_teleport_destinations() {
        let escape = enderman_escape(
            EndermanHazard::Projectile,
            [0.0, 64.0, 0.0],
            -64..320,
            |[x, y, _]| EndermanTeleportBlock {
                blocks_motion: y == 63,
                liquid: x == -32 && y == 64,
            },
            |dst| dst == [0.0, 64.0, -32.0],
            &mut rolls([0, 32, 0, 1 << 23, 32, 0]),
        );
        assert_eq!(
            escape,
            EndermanEscape {
                destination: Some([0.0, 64.0, -32.0]),
                reject_damage: true
            }
        );

        let mut draws = 0;
        let blocked = enderman_escape(
            EndermanHazard::Projectile,
            [0.0, 64.0, 0.0],
            -64..320,
            |_| EndermanTeleportBlock {
                blocks_motion: true,
                liquid: true,
            },
            |_| panic!("wet floor bypassed the teleport check"),
            &mut |bound| {
                draws += 1;
                highest(bound)
            },
        );
        assert_eq!(draws, 64 * 3);
        assert_eq!(
            blocked,
            EndermanEscape {
                destination: None,
                reject_damage: true
            }
        );

        for hazard in [EndermanHazard::Water, EndermanHazard::Rain] {
            let escape = enderman_escape(
                hazard,
                [0.0, 64.0, 0.0],
                -64..320,
                |[_, y, _]| EndermanTeleportBlock {
                    blocks_motion: y == 41,
                    liquid: false,
                },
                |_| true,
                &mut rolls([1 << 23, 32, 1 << 23]),
            );
            assert_eq!(escape.destination, Some([0.0, 42.0, 0.0]));
            assert!(!escape.reject_damage);
        }
    }

    #[test]
    fn shulker_closed_shell_blocks_projectiles_and_open_shell_fires_levitation_bullets() {
        let mut shulker = ShulkerBehavior::new(4);
        assert_eq!(shulker.peek(), 0);
        assert_eq!(shulker.armor_bonus(), 20);
        assert!(shulker.rejects_projectile());
        assert_eq!(shulker.tick([0.0; 3], Some(8), &mut no_draw), None);
        shulker.open(&mut zero);
        assert!(shulker.peek() > 0);
        assert_eq!(shulker.armor_bonus(), 0);
        assert!(!shulker.rejects_projectile());
        for _ in 0..19 {
            assert_eq!(shulker.tick([0.0; 3], Some(8), &mut no_draw), None);
        }
        let mut bullet = shulker.tick([0.0; 3], Some(8), &mut zero).unwrap();
        assert!(shulker.rejects_projectile());
        assert_eq!((bullet.owner, bullet.target), (4, 8));
        bullet.tick(Some([8.0, 0.0, 0.0]));
        assert!(bullet.position[0] > 0.0);
        bullet.tick(Some([8.0, 0.0, 8.0]));
        assert!(bullet.position[2] > 0.0);
        assert_eq!(
            bullet.hit(8),
            ShulkerBulletHit {
                victim: 8,
                levitation_ticks: 200
            }
        );
        shulker.open(&mut highest);
        for _ in 0..59 {
            assert_eq!(shulker.tick([0.0; 3], None, &mut no_draw), None);
            assert!(!shulker.rejects_projectile());
        }
        assert_eq!(shulker.tick([0.0; 3], None, &mut no_draw), None);
        assert!(shulker.rejects_projectile());
    }

    fn dragon_tick(crystals: &[DragonCrystal]) -> DragonTick<'_> {
        DragonTick {
            position: [0.0, 80.0, 0.0],
            portal_y: 80,
            path_complete: false,
            strafe_target: None,
            strafe_aligned: false,
            crystals,
        }
    }

    #[test]
    fn dragon_ten_pillars_and_nearest_crystal_healing_and_break_damage() {
        // Mojang 1.21.1 SpikeCacheLoader with the level seed 4242.
        assert_eq!(
            end_crystal_pillars(4242).map(|pillar| (
                end_crystal_spawn_position(&pillar),
                pillar.radius,
                pillar.guarded
            )),
            [
                ([42.5, 92.0, 0.5], 3, false),
                ([33.5, 95.0, 24.5], 4, false),
                ([12.5, 89.0, 39.5], 3, false),
                ([-12.5, 104.0, 39.5], 5, false),
                ([-33.5, 77.0, 24.5], 2, false),
                ([-41.5, 101.0, -0.5], 4, false),
                ([-33.5, 86.0, -24.5], 3, false),
                ([-12.5, 83.0, -39.5], 2, true),
                ([12.5, 80.0, -39.5], 2, true),
                ([33.5, 98.0, -24.5], 4, false),
            ]
        );

        let mut dragon = EnderDragonBehavior::new(false);
        assert!(dragon.hurt(BossHit::Arrow, 20.0));
        let nearest = DragonCrystal {
            entity: 4,
            position: [1.0, 78.0, 1.0],
        };
        let farther = DragonCrystal {
            entity: 5,
            position: [11.0, 80.0, 5.0],
        };
        for _ in 0..9 {
            assert_eq!(dragon.tick(dragon_tick(&[farther, nearest])), None);
        }
        assert_eq!(dragon.hp, 180.0);
        assert_eq!(dragon.tick(dragon_tick(&[farther, nearest])), None);
        assert_eq!(dragon.hp, 181.0);
        assert!(!dragon.crystal_destroyed(farther.entity));
        assert!(dragon.crystal_destroyed(nearest.entity));
        assert_eq!(dragon.hp, 171.0);
        assert!(!dragon.crystal_destroyed(nearest.entity));
        assert_eq!(dragon.tick(dragon_tick(&[farther])), None);
        assert_eq!(dragon.linked_crystal, Some(farther.entity));
    }

    #[test]
    fn dragon_strafes_lands_and_cycles_sitting_phases_with_arrow_immunity() {
        let mut dragon = EnderDragonBehavior::new(false);
        assert_eq!(dragon.phase, DragonPhase::HoldingPattern);
        let mut tick = dragon_tick(&[]);
        tick.path_complete = true;
        tick.strafe_target = Some(12);
        assert_eq!(dragon.tick(tick), None);
        assert_eq!(dragon.phase, DragonPhase::StrafePlayer);
        for count in 1..=5 {
            let mut tick = dragon_tick(&[]);
            tick.strafe_target = Some(12);
            tick.strafe_aligned = true;
            assert_eq!(
                dragon.tick(tick),
                (count == 5).then_some(DragonEvent::StrafeFireball { target: 12 })
            );
        }
        assert_eq!(dragon.phase, DragonPhase::LandingApproach);
        let mut tick = dragon_tick(&[]);
        tick.path_complete = true;
        assert_eq!(dragon.tick(tick), None);
        assert_eq!(dragon.phase, DragonPhase::Landing);
        assert_eq!(
            dragon.tick(dragon_tick(&[])),
            Some(DragonEvent::FlameBreath)
        );
        assert_eq!(dragon.phase, DragonPhase::SittingFlaming);
        assert!(!dragon.hurt(BossHit::Arrow, 30.0));

        for _ in 0..200 {
            assert_eq!(dragon.tick(dragon_tick(&[])), None);
        }
        assert_eq!(dragon.phase, DragonPhase::SittingScanning);
        assert!(!dragon.hurt(BossHit::Arrow, 30.0));
        for count in 1..=25 {
            let mut tick = dragon_tick(&[]);
            tick.strafe_target = Some(12);
            assert_eq!(
                dragon.tick(tick),
                (count == 25).then_some(DragonEvent::PerchAttack)
            );
        }
        assert_eq!(dragon.phase, DragonPhase::SittingAttacking);
        assert!(!dragon.hurt(BossHit::Arrow, 30.0));
        assert!(dragon.hurt(BossHit::Other, 1.0));

        for _ in 0..40 {
            assert_eq!(dragon.tick(dragon_tick(&[])), None);
        }
        assert_eq!(dragon.phase, DragonPhase::Takeoff);
        assert!(dragon.hurt(BossHit::Arrow, 2.0));
        let mut tick = dragon_tick(&[]);
        tick.path_complete = true;
        assert_eq!(dragon.tick(tick), None);
        assert_eq!(dragon.phase, DragonPhase::HoldingPattern);
    }

    #[test]
    fn dragon_death_opens_bedrock_portal_and_awards_first_kill_egg_and_xp_once() {
        for (previously_killed, egg, xp) in [(false, Some([0, 84, 0]), 12_000), (true, None, 500)] {
            let mut dragon = EnderDragonBehavior::new(previously_killed);
            assert!(dragon.hurt(BossHit::Other, 200.0));
            assert_eq!(dragon.phase, DragonPhase::Dying);
            for _ in 0..200 {
                let mut tick = dragon_tick(&[]);
                tick.position = [42.0, 90.0, 0.0];
                assert_eq!(dragon.tick(tick), None);
            }
            for _ in 0..199 {
                assert_eq!(dragon.tick(dragon_tick(&[])), None);
            }
            let Some(DragonEvent::CompleteFight(rewards)) = dragon.tick(dragon_tick(&[])) else {
                panic!("expected the exit portal and rewards after the death animation");
            };
            assert_eq!(
                rewards,
                DragonFightRewards {
                    exit_portal_base: [0, 80, 0],
                    dragon_egg: egg,
                    xp
                }
            );
            let mut blocks = HashMap::new();
            rewards.place_bedrock_exit_portal(|pos, block| {
                blocks.insert(pos, block);
            });
            assert_eq!(blocks[&[0, 83, 0]], "bedrock");
            assert_eq!(blocks[&[3, 80, 0]], "bedrock");
            assert_eq!(blocks[&[2, 80, 0]], "end_portal");
            assert_eq!(
                blocks[&[0, 84, 0]],
                if egg.is_some() { "dragon_egg" } else { "air" }
            );
            assert_eq!(dragon.phase, DragonPhase::Dead);
            assert_eq!(dragon.tick(dragon_tick(&[])), None);
        }
    }

    #[test]
    fn slime_and_magma_cube_split_4_to_2_to_1_and_keep_their_kind() {
        for kind in [SlimeKind::Slime, SlimeKind::MagmaCube] {
            let large = SlimeBehavior {
                kind,
                size: SlimeSize::Large,
            };
            assert_eq!((large.max_hp(), large.attack_damage()), (16.0, 4.0));
            let children = large.split_on_death(&mut highest);
            assert_eq!(children.len(), 4);
            for medium in children {
                assert_eq!((medium.kind, medium.size), (kind, SlimeSize::Medium));
                assert_eq!((medium.max_hp(), medium.attack_damage()), (4.0, 2.0));
                let children = medium.split_on_death(&mut zero);
                assert_eq!(children.len(), 2);
                for small in children {
                    assert_eq!((small.kind, small.size), (kind, SlimeSize::Small));
                    assert_eq!((small.max_hp(), small.attack_damage()), (1.0, 1.0));
                    assert!(small.split_on_death(&mut no_draw).is_empty());
                }
            }
        }
    }

    #[test]
    fn witch_self_potions_respect_conditions_and_drinking_duration() {
        use WitchPotion::*;
        let mut witch = WitchBehavior::default();
        let needs = WitchNeeds {
            underwater: true,
            in_fire_or_lava: true,
            hurt: true,
            chase_distance: Some(12.0),
        };
        for (effects, potion) in [
            (&[][..], WaterBreathing),
            (&[WaterBreathing][..], FireResistance),
            (&[WaterBreathing, FireResistance][..], Healing),
            (&[WaterBreathing, FireResistance, Healing][..], Swiftness),
        ] {
            assert_eq!(
                witch.tick_drinking(needs, effects, &mut zero),
                Some(WitchAction::StartDrinking(potion))
            );
            let target = WitchTarget {
                entity: 1,
                horizontal_distance: 8.0,
                hp: 20.0,
                effects: &[],
            };
            assert_eq!(witch.splash(target, &mut no_draw), None);
            for _ in 0..31 {
                assert_eq!(witch.tick_drinking(needs, effects, &mut no_draw), None);
            }
            assert_eq!(
                witch.tick_drinking(needs, effects, &mut no_draw),
                Some(WitchAction::FinishDrinking(potion))
            );
        }
        assert_eq!(witch.tick_drinking(needs, &[], &mut highest), None);
    }

    #[test]
    fn spider_hostility_and_leap_cover_daylight_threshold_and_provocation() {
        assert!(!spider_is_hostile(true, 12, false));
        assert!(!spider_is_hostile(true, 15, false));
        assert!(spider_is_hostile(true, 11, false));
        assert!(spider_is_hostile(true, 15, true));
        assert!(spider_is_hostile(false, 15, false));
        assert_eq!(
            spider_leap_velocity([0.0; 3], [0.0, 20.0, 4.0]),
            [0.0, 0.4, 0.4]
        );
        assert_eq!(spider_leap_velocity([1.0; 3], [1.0; 3]), [0.0, 0.4, 0.0]);
    }

    #[test]
    fn creeper_swell_decays_outside_cancel_range_and_charged_power_doubles() {
        let mut creeper = CreeperBehavior::new();
        for _ in 0..10 {
            assert_eq!(creeper.tick(Some(2.0)), None);
        }
        assert_eq!(creeper.fuse, 9);
        assert_eq!(creeper.tick(Some(5.0)), None); // Inside swell range, outside cancel distance.
        assert_eq!(creeper.fuse, 9);
        assert_eq!(creeper.tick(Some(8.0)), None);
        assert_eq!(creeper.fuse, 8);
        creeper.ignite(true);
        for _ in 0..21 {
            assert_eq!(creeper.tick(None), None);
        }
        assert_eq!(creeper.tick(None), Some(3.0));

        let mut charged = CreeperBehavior {
            charged: true,
            ..CreeperBehavior::new()
        };
        charged.ignite(true);
        for _ in 0..29 {
            assert_eq!(charged.tick(None), None);
        }
        assert_eq!(charged.tick(None), Some(6.0));
        charged.ignite(false);
        assert_eq!(charged.fuse, -1);
    }

    #[test]
    fn skeleton_variants_share_bow_strafe_and_sunlight_rules() {
        for _variant in ["skeleton", "stray", "bogged"] {
            assert_eq!(skeleton_bow_charge_interval(false), 40);
            assert_eq!(skeleton_bow_charge_interval(true), 20);
        }
        assert_eq!(
            skeleton_strafe_velocity(10.0, true, true),
            [0.25, 0.0, 0.25]
        );
        assert_eq!(skeleton_strafe_velocity(4.99, true, true), [0.0; 3]);
        assert_eq!(skeleton_strafe_velocity(15.01, false, true), [0.0; 3]);
        assert!(skeleton_burns_in_sun(true, true, 0.51, false));
        assert!(!skeleton_burns_in_sun(true, true, 0.5, false));
        assert!(!skeleton_burns_in_sun(true, false, 1.0, false));
        assert!(!skeleton_burns_in_sun(true, true, 1.0, true));
    }

    #[test]
    fn phantom_dive_and_circle_are_directional_and_sunlight_sensitive() {
        let circle = phantom_circle_velocity([4.0, 0.0, 0.0], [0.0; 3], true);
        assert!(circle[2] > 0.0);
        let dive = phantom_swoop_velocity([0.0; 3], [0.0, 0.0, 0.0]);
        assert!(dive[1] < 0.0);
        assert!((mob_behavior_dot(dive, dive) - 1.0).abs() < 1e-9);
        assert!(phantom_burns_in_sun(true, true, 0.6));
        assert!(!phantom_burns_in_sun(false, true, 1.0));
        assert!(!phantom_burns_in_sun(true, false, 1.0));
    }

    #[test]
    fn drowned_chases_through_water_hides_by_day_and_golem_provokes_and_flings_upward() {
        let swim = drowned_swim_chase_velocity([0.0; 3], [0.0, 3.0, 4.0]);
        for (actual, expected) in swim.into_iter().zip([0.0, 0.06, 0.08]) {
            assert!((actual - expected).abs() < 1e-12);
        }
        assert!(drowned_should_hide_in_water(true, true));
        assert!(!drowned_should_hide_in_water(true, false));
        assert!(!drowned_should_hide_in_water(false, true));
        assert!(iron_golem_provoked(true, false));
        assert!(iron_golem_provoked(false, true));
        assert!(!iron_golem_provoked(false, false));
        assert_eq!(
            iron_golem_fling_velocity([0.0, 10.0, 4.0], [0.0; 3]),
            [0.0, 0.4, 0.4]
        );
    }

    #[test]
    fn witch_splash_selection_uses_distance_hp_and_existing_effects() {
        use WitchPotion::*;
        let witch = WitchBehavior::default();
        for (distance, hp, effects, potion) in [
            (8.0, 20.0, &[][..], Slowness),
            (8.0, 8.0, &[Slowness][..], Poison),
            (3.0, 7.9, &[][..], Weakness),
            (7.0, 8.0, &[Poison][..], Harming),
            (3.0, 7.0, &[Weakness][..], Harming),
        ] {
            let target = WitchTarget {
                entity: 1,
                horizontal_distance: distance,
                hp,
                effects,
            };
            assert_eq!(
                witch.splash(target, &mut zero),
                Some(WitchAction::Splash { target: 1, potion })
            );
        }
    }

    #[test]
    fn wolf_taming_sets_40_hp_and_owner_controls_sitting_collar_and_assist() {
        let owner = [1; 16];
        let visitor = [2; 16];
        let mut wolf = PetBehavior::new(PetKind::Wolf);
        assert_eq!(wolf.max_hp(), 8.0);
        assert_eq!(
            wolf.try_tame(owner, "cod", &mut no_draw),
            PetTameResult::Pass
        );
        assert_eq!(
            wolf.try_tame(owner, "bone", &mut highest),
            PetTameResult::Consumed
        );
        assert_eq!(
            wolf.try_tame(owner, "minecraft:bone", &mut zero),
            PetTameResult::Tamed
        );
        assert_eq!((wolf.max_hp(), wolf.hp), (40.0, 40.0));
        assert!(!wolf.dye_collar(visitor, 11));
        assert!(wolf.dye_collar(owner, 11));
        assert_eq!(wolf.collar_color, 11);
        assert_eq!(wolf.toggle_sitting(visitor), None);
        assert_eq!(wolf.wolf_assist_target(owner, Some(11), Some(12)), None);
        assert_eq!(wolf.toggle_sitting(owner), Some(false));
        assert_eq!(wolf.wolf_assist_target(owner, Some(11), Some(12)), Some(11));
        assert_eq!(wolf.wolf_assist_target(owner, None, Some(12)), Some(12));
        assert_eq!(wolf.wolf_assist_target(visitor, Some(11), None), None);
        assert!(!wolf.repels("creeper", 1.0));
    }

    #[test]
    fn cat_tames_only_with_raw_fish_and_repels_creepers_and_phantoms() {
        let mut cat = PetBehavior::new(PetKind::Cat);
        let owner = [1; 16];
        assert!(cat.repels("creeper", 6.0));
        assert!(!cat.repels("creeper", 6.01));
        assert_eq!(
            cat.try_tame(owner, "cooked_cod", &mut no_draw),
            PetTameResult::Pass
        );
        assert_eq!(
            cat.try_tame(owner, "cod", &mut highest),
            PetTameResult::Consumed
        );
        assert_eq!(
            cat.try_tame(owner, "salmon", &mut zero),
            PetTameResult::Tamed
        );
        assert_eq!(cat.hp, 10.0);
        assert!(cat.repels("minecraft:phantom", 16.0));
        assert!(!cat.repels("phantom", 16.01));
        assert!(!cat.repels("zombie", 1.0));
    }

    #[test]
    fn wither_spawn_explodes_at_tick_220_and_armor_blocks_arrows_at_half_hp() {
        let mut wither = WitherBehavior::new();
        for _ in 0..219 {
            assert_eq!(wither.tick(), None);
            assert!(wither.immune_to(BossHit::Other));
        }
        let hp = wither.hp;
        assert_eq!(wither.hurt(BossHit::Other, 99.0), None);
        assert_eq!(wither.hp, hp);
        assert_eq!(wither.shoot(4, false, &mut no_draw), None);
        assert_eq!(
            wither.tick(),
            Some(WitherEvent::SpawnExplosion { power: 7.0 })
        );
        assert_eq!(wither.tick(), None);
        assert_eq!(wither.hp, 300.0);
        assert!(!wither.has_wither_armor());
        assert_eq!(wither.hurt(BossHit::Arrow, 149.0), None);
        assert_eq!(wither.hp, 151.0);
        assert!(!wither.immune_to(BossHit::Arrow));
        assert_eq!(wither.hurt(BossHit::Arrow, 1.0), None);
        assert_eq!(wither.hp, 150.0);
        assert!(wither.has_wither_armor());
        assert_eq!(wither.hurt(BossHit::Arrow, 500.0), None);
        assert_eq!(wither.hp, 150.0);
        assert_eq!(
            wither.shoot(4, false, &mut highest),
            Some(WitherSkull::Black { target: 4 })
        );
        assert_eq!(
            wither.shoot(4, false, &mut zero),
            Some(WitherSkull::Blue { target: 4 })
        );
        assert_eq!(
            wither.shoot(4, true, &mut no_draw),
            Some(WitherSkull::Blue { target: 4 })
        );
        assert_eq!(
            wither.hurt(BossHit::Other, 150.0),
            Some(WitherEvent::Drop {
                item: "nether_star"
            })
        );
        assert_eq!(wither.hurt(BossHit::Other, 1.0), None);
    }

    #[test]
    fn warden_anger_decays_per_entity_then_locks_and_fires_sonic_boom_after_34_ticks() {
        let mut warden = WardenBehavior::new();
        let target = 7;
        assert_eq!(warden.hp, 500.0);
        assert_eq!(warden.anger.anger(target), 0);
        assert!(!warden.anger.agitated(target));
        for anger in [35, 70, 105] {
            assert_eq!(warden.anger.add_vibration(target), anger);
        }
        assert_eq!(warden.anger.add_vibration(8), 35);
        for _ in 0..500 {
            assert_eq!(warden.tick(None), None);
        }
        assert_eq!(warden.anger.anger(target), 80);
        assert_eq!(warden.anger.anger(8), 10);
        assert!(warden.anger.agitated(target));
        assert!(warden.target.is_none());
        assert_eq!(warden.anger.add_vibration(target), 115);
        assert_eq!(warden.anger.add_vibration(target), 150);
        assert_eq!(warden.tick(None), Some(WardenEvent::Roar { target }));
        assert_eq!(warden.target, Some(target));

        assert_eq!(
            warden.tick(Some(WardenTarget {
                entity: 8,
                offset: [1.0; 3]
            })),
            None
        );
        assert_eq!(
            warden.tick(Some(WardenTarget {
                entity: target,
                offset: [15.01, 20.0, 0.0]
            })),
            None
        );
        let observed = Some(WardenTarget {
            entity: target,
            offset: [9.0, 20.0, 12.0],
        });
        assert_eq!(
            warden.tick(observed),
            Some(WardenEvent::ChargeSonicBoom { target })
        );
        for _ in 0..33 {
            assert_eq!(warden.tick(observed), None);
        }
        assert_eq!(
            warden.tick(observed),
            Some(WardenEvent::SonicBoom {
                target,
                damage: 10.0,
                bypass_armor: true,
                bypass_shield: true
            })
        );
        assert_eq!(warden.tick(observed), None);
        warden.forget(target);
        assert_eq!(warden.target, None);
        assert_eq!(warden.anger.anger(target), 0);
        warden.hurt(500.0);
        assert_eq!(warden.hp, 0.0);
        assert_eq!(warden.tick(observed), None);
    }
}
