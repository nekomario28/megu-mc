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
    /// `universalAnger`: the last provocation angers at every nearby player
    /// and holds while the rule stays on, instead of ticking down at one
    /// target.
    universal: bool,
    barter: Option<(i32, u8)>,
    conversion: NetherConversion,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PiglinBehavior {
    fn hostile_to(&self, player: i32, armor: &[&str]) -> bool {
        if self.conversion.converted() {
            return false;
        }
        if self.universal {
            // A universal provocation makes every nearby player a target;
            // golden armor still pacifies the piglin itself.
            return self.anger.is_some()
                && !armor.iter().any(|piece| {
                    matches!(
                        mob_behavior_id(piece),
                        "golden_helmet" | "golden_chestplate" | "golden_leggings" | "golden_boots"
                    )
                });
        }
        self.anger.is_some_and(|(target, _)| target == player)
            || !armor.iter().any(|piece| {
                matches!(
                    mob_behavior_id(piece),
                    "golden_helmet" | "golden_chestplate" | "golden_leggings" | "golden_boots"
                )
            })
    }

    fn anger_ticks(&self) -> u16 {
        self.anger.map_or(0, |(_, ticks)| ticks)
    }

    fn provoke(&mut self, player: i32, distance: f64, event: PiglinProvocation<'_>) -> bool {
        if self.conversion.converted() || distance > 16.0 || !event.guards() {
            return false;
        }
        self.universal = universal_anger_enabled();
        self.anger = Some((player, 600));
        true
    }

    /// `forgiveDeadPlayers` arm: the caller reports whether the angered target
    /// died (or left the world); vanilla clears the grudge on that death.
    fn forgive_target(&mut self, target_dead: bool) {
        if forgive_dead_players_enabled() && target_dead && self.anger.is_some() {
            self.anger = None;
            self.universal = false;
        }
    }

    // A successful offer transfers one item from the caller to the piglin.
    // `mobGriefing` gates the item pickup itself, so a piglin that cannot take
    // the ingot never starts a barter.
    fn offer(&mut self, player: i32, item: &str) -> bool {
        if self.conversion.converted()
            || self.barter.is_some()
            || !mob_griefing_enabled()
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
            self.universal = false;
            self.barter = None;
            return Some(PiglinEvent::ConvertToZombifiedPiglin);
        }
        if !self.universal {
            if let Some((player, ticks)) = self.anger {
                self.anger = (ticks > 1).then_some((player, ticks - 1));
            }
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

    /// A sniff noticing an entity adds the same +35 a vibration does.
    fn add_sniff(&mut self, entity: i32) -> u16 {
        self.add_vibration(entity)
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
    /// Reachable victims take the warden's full melee hit.
    Melee {
        target: i32,
        damage: f32,
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
                // `generic.follow_range` (100.0) bounds target acquisition;
                // `GhastAttackGoal` still fires only inside 64 blocks.
                let tracked = delta.filter(|delta| {
                    ghast_acquires_target(mob_behavior_dot(*delta, *delta).sqrt())
                });
                let can_fire = visible_target.is_some()
                    && delta.is_some_and(|delta| mob_behavior_dot(delta, delta) < 64.0 * 64.0);
                match ghast.tick(tracked, can_fire) {
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
            Self::Piglin(piglin) => {
                // `forgiveDeadPlayers`: a grudge target seen dead (or gone) is
                // forgiven before the anger ticks.
                let target_dead = piglin.anger.is_some_and(|(target, _)| {
                    ctx.nearest_target
                        .is_some_and(|snapshot| snapshot.eid == target && snapshot.hp <= 0.0)
                });
                piglin.forgive_target(target_dead);
                match piglin.tick(ctx.dim == DimId::Nether, &mut draw) {
                    Some(PiglinEvent::ConvertToZombifiedPiglin) => {
                        Some(ConvertEntity { target_etype: 127 })
                    }
                    Some(PiglinEvent::Barter { player, loot }) => Some(PiglinBarter {
                        player_eid: player,
                        loot,
                    }),
                    None => None,
                }
            }
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
                    Some(WardenEvent::Melee { target, damage }) => Some(MeleeDamage {
                        target_eid: target,
                        amount: damage,
                        knockback_y: 0.0,
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

// ---------- P2: mob gamerules ----------
//
// Each accessor reads `gamerule_live_value` (world_mechanisms.rs) — the same
// atomic registry and stored map `/gamerule` mutates.

/// `mobGriefing`: whether mobs may modify the world — pick up items (the
/// piglin barter gate above), carry or break blocks, trample crops, and
/// destroy blocks in explosions.
#[cfg_attr(not(test), allow(dead_code))]
fn mob_griefing_enabled() -> bool {
    gamerule_live_value("mobGriefing") != 0
}

/// `forgiveDeadPlayers`: neutral mobs drop a grudge when the player it was
/// held against dies.
#[cfg_attr(not(test), allow(dead_code))]
fn forgive_dead_players_enabled() -> bool {
    gamerule_live_value("forgiveDeadPlayers") != 0
}

/// `universalAnger`: a provoked neutral mob angers at every nearby player,
/// not just the source of the provocation.
#[cfg_attr(not(test), allow(dead_code))]
fn universal_anger_enabled() -> bool {
    gamerule_live_value("universalAnger") != 0
}

/// `maxEntityCramming`: the live push engine reads the same atomic; `0`
/// disables cramming damage entirely.
#[cfg_attr(not(test), allow(dead_code))]
fn max_entity_cramming_limit() -> i32 {
    gamerule_live_value("maxEntityCramming")
}

/// `LivingEntity.pushEntities` cramming arm, mirrored from the live push
/// engine: more than `limit - 1` pushable entities sharing the cell, at least
/// that many not riding anything, and a one-in-four roll — a limit of `0`
/// (or lower) disables the damage.
#[cfg_attr(not(test), allow(dead_code))]
fn eval_entity_cramming_damage(
    limit: i32,
    pushable: usize,
    non_passengers: usize,
    roll_mod_4: u32,
) -> f32 {
    const ENTITY_CRAMMING_HIT: f32 = 6.0;
    if limit > 0
        && pushable > (limit - 1) as usize
        && roll_mod_4 % 4 == 0
        && non_passengers > (limit - 1) as usize
    {
        ENTITY_CRAMMING_HIT
    } else {
        0.0
    }
}

// ---------- P6a / #219: additional hostile-family evaluators ----------
//
// The four families the earlier passes had not covered — phantom, pillager,
// evoker, guardian/elder guardian — plus the named constants and evaluators
// the parity spec pins for families already dispatched (ghast notice range,
// blaze hover height, warden melee/unreachable rules). All are deterministic:
// no wall clock, no hash iteration order, scripted draws only.

/// Vanilla `generic.follow_range` for a ghast is 100.0: it notices targets up
/// to 100 blocks away even though `GhastAttackGoal` only fires within 64.
#[cfg_attr(not(test), allow(dead_code))]
const GHAST_FOLLOW_RANGE: f64 = 100.0;

#[cfg_attr(not(test), allow(dead_code))]
fn ghast_acquires_target(distance: f64) -> bool {
    distance <= GHAST_FOLLOW_RANGE
}

/// `BlazeAttackGoal` hovers the blaze half a block above the target's
/// mid-body (`getY(0.5) + 0.5`).
#[cfg_attr(not(test), allow(dead_code))]
fn blaze_hover_y(target_pos: [f64; 3], target_height: f64) -> f64 {
    target_pos[1] + f64::from(target_height) * 0.5 + 0.5
}

// --- Phantom: circle / swoop / climb state machine ---

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhantomPhase {
    /// Orbiting an anchor 20–30 blocks above the target.
    Circle,
    /// Diving at the target.
    Swoop,
    /// Climbing back to altitude after a hit or an aborted dive.
    Climb,
}

/// One tick of phantom intent: the steering velocity the caller applies and
/// the melee connect the swoop produces.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct PhantomTick {
    velocity: [f64; 3],
    attack: Option<i32>,
}

#[cfg_attr(not(test), allow(dead_code))]
const PHANTOM_ATTACK_DAMAGE: f32 = 2.0;

#[cfg_attr(not(test), allow(dead_code))]
struct PhantomBehavior {
    phase: PhantomPhase,
    anchor: Option<[f64; 3]>,
    clockwise: bool,
    phase_ticks: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PhantomBehavior {
    const CIRCLE_TICKS: u16 = 60;
    const SWOOP_TIMEOUT_TICKS: u16 = 80;
    const CLIMB_TIMEOUT_TICKS: u16 = 100;
    const SWOOP_HIT_DISTANCE: f64 = 2.0;
    const SWOOP_SPEED: f64 = 1.0;
    const CLIMB_SPEED: f64 = 0.7;

    fn new() -> Self {
        Self {
            phase: PhantomPhase::Circle,
            anchor: None,
            clockwise: true,
            phase_ticks: 0,
        }
    }

    fn set_phase(&mut self, phase: PhantomPhase) {
        self.phase = phase;
        self.phase_ticks = 0;
    }

    /// `target` is `(eid, position)` of the tracked victim.
    fn tick(
        &mut self,
        pos: [f64; 3],
        target: Option<(i32, [f64; 3])>,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> PhantomTick {
        let Some((eid, target_pos)) = target else {
            self.anchor = None;
            self.set_phase(PhantomPhase::Circle);
            return PhantomTick {
                velocity: [0.0; 3],
                attack: None,
            };
        };
        self.phase_ticks = self.phase_ticks.saturating_add(1);
        match self.phase {
            PhantomPhase::Circle => {
                let Some(anchor) = self.anchor else {
                    // Vanilla lifts the anchor 20–30 blocks over the target
                    // with a ±10 XZ jitter and a coin-flip orbit direction.
                    self.anchor = Some([
                        target_pos[0] + f64::from(draw(21)) - 10.0,
                        target_pos[1] + 20.0 + f64::from(draw(11)),
                        target_pos[2] + f64::from(draw(21)) - 10.0,
                    ]);
                    self.clockwise = draw(2) == 0;
                    return PhantomTick {
                        velocity: [0.0; 3],
                        attack: None,
                    };
                };
                if self.phase_ticks >= Self::CIRCLE_TICKS {
                    self.set_phase(PhantomPhase::Swoop);
                }
                PhantomTick {
                    velocity: phantom_circle_velocity(pos, anchor, self.clockwise),
                    attack: None,
                }
            }
            PhantomPhase::Swoop => {
                let delta = mob_behavior_delta(pos, target_pos);
                let close = mob_behavior_dot(delta, delta) <= Self::SWOOP_HIT_DISTANCE.powi(2);
                if close || self.phase_ticks >= Self::SWOOP_TIMEOUT_TICKS {
                    self.set_phase(PhantomPhase::Climb);
                    return PhantomTick {
                        velocity: phantom_swoop_velocity(pos, target_pos),
                        attack: close.then_some(eid),
                    };
                }
                PhantomTick {
                    velocity: phantom_swoop_velocity(pos, target_pos)
                        .map(|c| c * Self::SWOOP_SPEED),
                    attack: None,
                }
            }
            PhantomPhase::Climb => {
                let anchor = self.anchor.unwrap_or([
                    target_pos[0],
                    target_pos[1] + 25.0,
                    target_pos[2],
                ]);
                let at_altitude = pos[1] >= anchor[1] - 1.0;
                if at_altitude || self.phase_ticks >= Self::CLIMB_TIMEOUT_TICKS {
                    self.anchor = None;
                    self.set_phase(PhantomPhase::Circle);
                    return PhantomTick {
                        velocity: [0.0; 3],
                        attack: None,
                    };
                }
                PhantomTick {
                    velocity: mob_behavior_unit(mob_behavior_delta(pos, anchor))
                        .map(|c| c * Self::CLIMB_SPEED),
                    attack: None,
                }
            }
        }
    }
}

// --- Pillager: crossbow charge + strafe ---

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
enum PillagerAction {
    Shoot { target: i32 },
}

/// Vanilla `CrossbowItem.getChargeDuration` is 1.25 s = 25 ticks.
#[cfg_attr(not(test), allow(dead_code))]
const PILLAGER_CROSSBOW_CHARGE_TICKS: u8 = 25;

#[cfg_attr(not(test), allow(dead_code))]
struct PillagerBehavior {
    charge_ticks: u8,
    strafe_clockwise: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl PillagerBehavior {
    /// `RangedCrossbowAttackMob` holds roughly a 10 m ring around the target.
    const STRAFE_RANGE: f64 = 12.0;
    const STRAFE_MIN: f64 = 4.0;
    const STRAFE_SPEED: f64 = 0.5;

    fn new() -> Self {
        Self {
            charge_ticks: 0,
            strafe_clockwise: true,
        }
    }

    /// `distance` is the horizontal gap to the visible target; the strafe is
    /// the same shape as the skeleton's, just the pillager's band and speed.
    fn strafe_velocity(&self, distance: f64, forward: bool) -> [f64; 3] {
        if distance < Self::STRAFE_MIN || distance > Self::STRAFE_RANGE {
            return [0.0; 3];
        }
        let direction = if self.strafe_clockwise { 1.0 } else { -1.0 };
        let ahead = if forward { 1.0 } else { -1.0 };
        [
            direction * Self::STRAFE_SPEED * 0.5,
            0.0,
            ahead * Self::STRAFE_SPEED * 0.5,
        ]
    }

    /// `Some(eid)` while a target is visible. A charge started stays latched
    /// while the crossbow loads, then releases the bolt on the 25th tick.
    fn tick(&mut self, target: Option<i32>) -> Option<PillagerAction> {
        let Some(target) = target else {
            self.charge_ticks = 0;
            return None;
        };
        if self.charge_ticks < PILLAGER_CROSSBOW_CHARGE_TICKS {
            self.charge_ticks += 1;
            return None;
        }
        self.charge_ticks = 0;
        Some(PillagerAction::Shoot { target })
    }
}

// --- Evoker: fangs, vex summon, wololo ---

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvokerSpell {
    /// 16 fangs marching toward the victim.
    FangLine,
    /// The close-range defense: an inner ring of 5 and an outer ring of 8.
    FangCircle,
    SummonVex,
    /// The blue-sheep-to-red-sheep easter egg.
    Wololo,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum EvokerEvent {
    FangAttack { positions: Vec<[i32; 3]> },
    SummonVex { count: u8 },
    Wololo { target: i32 },
}

/// Vanilla `SpellcastingIllager` warmup for the fang spells is 40 ticks.
#[cfg_attr(not(test), allow(dead_code))]
const EVOKER_SPELL_WARMUP: u8 = 40;
/// The summoning spell's post-cast cooldown.
#[cfg_attr(not(test), allow(dead_code))]
const EVOKER_VEX_COOLDOWN: u16 = 100;
/// Wololo also rests between casts.
#[cfg_attr(not(test), allow(dead_code))]
const EVOKER_WOLOLO_COOLDOWN: u16 = 100;
/// The wololo spell reaches blue sheep up to 16 blocks away.
#[cfg_attr(not(test), allow(dead_code))]
const EVOKER_WOLOLO_RANGE: f64 = 16.0;
#[cfg_attr(not(test), allow(dead_code))]
const EVOKER_FANG_LINE_COUNT: usize = 16;

#[cfg_attr(not(test), allow(dead_code))]
fn evoker_fang_line(from: [f64; 3], to: [f64; 3]) -> Vec<[i32; 3]> {
    // Vanilla advances each fang 1.25 blocks along the ray to the target.
    let [dx, _, dz] = mob_behavior_delta(from, to);
    let yaw = dz.atan2(dx);
    let ground_y = to[1].floor() as i32;
    (1..=EVOKER_FANG_LINE_COUNT)
        .map(|i| {
            let reach = i as f64 * 1.25;
            [
                (from[0] + reach * yaw.cos()).floor() as i32,
                ground_y,
                (from[2] + reach * yaw.sin()).floor() as i32,
            ]
        })
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
fn evoker_fang_circle(center: [f64; 3]) -> Vec<[i32; 3]> {
    let mut positions = Vec::with_capacity(13);
    let ground_y = center[1].floor() as i32;
    for (count, radius) in [(5usize, 1.5f64), (8, 2.5)] {
        for i in 0..count {
            let angle = 2.0 * std::f64::consts::PI * i as f64 / count as f64;
            positions.push([
                (center[0] + radius * angle.cos()).floor() as i32,
                ground_y,
                (center[2] + radius * angle.sin()).floor() as i32,
            ]);
        }
    }
    positions
}

#[cfg_attr(not(test), allow(dead_code))]
struct EvokerBehavior {
    cast: Option<(EvokerSpell, u8)>,
    /// Fang-line direction source or the wololo victim position, taken at cast.
    cast_anchor: Option<[f64; 3]>,
    cast_eid: Option<i32>,
    vex_cooldown: u16,
    wololo_cooldown: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EvokerBehavior {
    /// `FangCircle` when the victim closes to melee, `FangLine` otherwise.
    const FANG_CIRCLE_RANGE: f64 = 3.0;
    const FANG_LINE_RANGE: f64 = 15.0;

    fn new() -> Self {
        Self {
            cast: None,
            cast_anchor: None,
            cast_eid: None,
            vex_cooldown: 0,
            wololo_cooldown: 0,
        }
    }

    fn casting(&self) -> bool {
        self.cast.is_some()
    }

    /// `target` is `(eid, position)`; `blue_sheep` is `(eid, position)` of
    /// blue-wool sheep the world noticed this tick.
    fn tick(
        &mut self,
        pos: [f64; 3],
        target: Option<(i32, [f64; 3])>,
        blue_sheep: &[(i32, [f64; 3])],
    ) -> Option<EvokerEvent> {
        self.vex_cooldown = self.vex_cooldown.saturating_sub(1);
        self.wololo_cooldown = self.wololo_cooldown.saturating_sub(1);

        if let Some((spell, ticks)) = self.cast {
            if ticks > 1 {
                self.cast = Some((spell, ticks - 1));
                return None;
            }
            self.cast = None;
            return Some(match spell {
                EvokerSpell::FangLine => EvokerEvent::FangAttack {
                    positions: evoker_fang_line(pos, self.cast_anchor.take().unwrap_or(pos)),
                },
                EvokerSpell::FangCircle => EvokerEvent::FangAttack {
                    positions: evoker_fang_circle(pos),
                },
                EvokerSpell::SummonVex => {
                    self.vex_cooldown = EVOKER_VEX_COOLDOWN;
                    EvokerEvent::SummonVex { count: 3 }
                }
                EvokerSpell::Wololo => {
                    self.wololo_cooldown = EVOKER_WOLOLO_COOLDOWN;
                    EvokerEvent::Wololo {
                        target: self.cast_eid.take().unwrap_or(0),
                    }
                }
            });
        }

        // An idle evoker with a blue sheep in reach does the wololo.
        if target.is_none() && self.wololo_cooldown == 0 {
            if let Some(&(eid, _)) = blue_sheep
                .iter()
                .filter(|&(_, sheep_pos)| {
                    let delta = mob_behavior_delta(pos, *sheep_pos);
                    mob_behavior_dot(delta, delta) <= EVOKER_WOLOLO_RANGE * EVOKER_WOLOLO_RANGE
                })
                .min_by(|a, b| {
                    let da = mob_behavior_delta(pos, a.1);
                    let db = mob_behavior_delta(pos, b.1);
                    mob_behavior_dot(da, da).total_cmp(&mob_behavior_dot(db, db))
                })
            {
                self.cast = Some((EvokerSpell::Wololo, EVOKER_SPELL_WARMUP));
                self.cast_eid = Some(eid);
                return None;
            }
        }
        let Some((_, target_pos)) = target else {
            return None;
        };
        let [dx, _, dz] = mob_behavior_delta(pos, target_pos);
        let distance = dx.hypot(dz);
        let spell = if self.vex_cooldown == 0 {
            EvokerSpell::SummonVex
        } else if distance <= Self::FANG_CIRCLE_RANGE {
            EvokerSpell::FangCircle
        } else if distance <= Self::FANG_LINE_RANGE {
            EvokerSpell::FangLine
        } else {
            return None;
        };
        self.cast = Some((spell, EVOKER_SPELL_WARMUP));
        self.cast_anchor = Some(target_pos);
        None
    }
}

// --- Guardian / elder guardian ---

/// `GuardianAttackGoal.getAttackDuration` is 80 ticks (4 s) of beam charge.
#[cfg_attr(not(test), allow(dead_code))]
const GUARDIAN_LASER_TICKS: u16 = 80;
/// The guardian's follow range is 16 blocks.
#[cfg_attr(not(test), allow(dead_code))]
const GUARDIAN_ATTACK_RANGE: f64 = 16.0;
/// Normal-difficulty beam damage; easy deals 5, hard deals 9.
#[cfg_attr(not(test), allow(dead_code))]
const GUARDIAN_LASER_DAMAGE: f32 = 6.0;
/// Elder guardian beam damage on normal; easy 5, hard 12.
#[cfg_attr(not(test), allow(dead_code))]
const ELDER_GUARDIAN_LASER_DAMAGE: f32 = 8.0;
/// The aura reaches every player within 50 blocks every 1200 ticks (60 s).
#[cfg_attr(not(test), allow(dead_code))]
const ELDER_FATIGUE_RADIUS: f64 = 50.0;
#[cfg_attr(not(test), allow(dead_code))]
const ELDER_FATIGUE_PERIOD: u16 = 1200;
/// Mining Fatigue III: amplifier 2, applied for 5 minutes = 6000 ticks.
#[cfg_attr(not(test), allow(dead_code))]
const ELDER_FATIGUE_AMPLIFIER: u8 = 2;
#[cfg_attr(not(test), allow(dead_code))]
const ELDER_FATIGUE_DURATION: i32 = 6000;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum GuardianEvent {
    Laser { target: i32, damage: f32 },
    MiningFatigue { target: i32 },
}

#[cfg_attr(not(test), allow(dead_code))]
struct GuardianBehavior {
    elder: bool,
    attack_ticks: u16,
    aura_ticks: u16,
}

#[cfg_attr(not(test), allow(dead_code))]
impl GuardianBehavior {
    fn new(elder: bool) -> Self {
        Self {
            elder,
            attack_ticks: 0,
            aura_ticks: 0,
        }
    }

    fn damage(&self) -> f32 {
        if self.elder {
            ELDER_GUARDIAN_LASER_DAMAGE
        } else {
            GUARDIAN_LASER_DAMAGE
        }
    }

    /// `target` is `(eid, distance)` of a visible victim. The beam needs line
    /// of sight the whole charge; losing it resets the 4 s timer.
    fn tick(&mut self, target: Option<(i32, f64)>) -> Vec<GuardianEvent> {
        let mut events = Vec::new();
        if self.elder {
            self.aura_ticks += 1;
            if self.aura_ticks >= ELDER_FATIGUE_PERIOD {
                self.aura_ticks = 0;
                if let Some((eid, distance)) = target {
                    if distance <= ELDER_FATIGUE_RADIUS {
                        events.push(GuardianEvent::MiningFatigue { target: eid });
                    }
                }
            }
        }
        let Some((eid, _)) = target.filter(|&(_, d)| d <= GUARDIAN_ATTACK_RANGE) else {
            self.attack_ticks = 0;
            return events;
        };
        self.attack_ticks += 1;
        if self.attack_ticks >= GUARDIAN_LASER_TICKS {
            self.attack_ticks = 0;
            events.push(GuardianEvent::Laser {
                target: eid,
                damage: self.damage(),
            });
        }
        events
    }
}

// --- Warden melee / unreachable-ranged combat plan ---
//
// `WardenBehavior` above owns the vibration-anger loop (0..=150, agitated at
// 80, -1/s decay, +35 per vibration — a sniff contributes the same 35). This
// plan owns the attack choice once a target exists: melee at reach, or the
// sonic boom after 200 ticks without reach — 60-tick charge, then a hit that
// pierces armor and shields and ignores blocks in the way.

#[cfg_attr(not(test), allow(dead_code))]
const WARDEN_MELEE_DAMAGE: f32 = 30.0;
#[cfg_attr(not(test), allow(dead_code))]
const WARDEN_SONIC_TRIGGER_TICKS: u16 = 200;
#[cfg_attr(not(test), allow(dead_code))]
const WARDEN_SONIC_CHARGE_TICKS: u8 = 60;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct WardenCombatPlan {
    unreachable_ticks: u16,
    sonic_charge: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WardenCombatPlan {
    /// `reachable` is the caller's melee-reach verdict for this tick.
    fn tick(&mut self, target: Option<i32>, reachable: bool) -> Option<WardenEvent> {
        let target = target?;
        if reachable {
            self.unreachable_ticks = 0;
            self.sonic_charge = 0;
            return Some(WardenEvent::Melee {
                target,
                damage: WARDEN_MELEE_DAMAGE,
            });
        }
        self.unreachable_ticks = self.unreachable_ticks.saturating_add(1);
        if self.sonic_charge > 0 {
            self.sonic_charge -= 1;
            if self.sonic_charge == 0 {
                self.unreachable_ticks = 0;
                return Some(WardenEvent::SonicBoom {
                    target,
                    damage: 10.0,
                    bypass_armor: true,
                    bypass_shield: true,
                });
            }
            return None;
        }
        if self.unreachable_ticks >= WARDEN_SONIC_TRIGGER_TICKS {
            self.sonic_charge = WARDEN_SONIC_CHARGE_TICKS;
            return Some(WardenEvent::ChargeSonicBoom { target });
        }
        None
    }
}

// ---------- P6a / #220: projectile despawn & ground-life rules ----------
//
// Vanilla keeps no artificial in-flight lifetime on arrows, tridents,
// throwables or fireballs: a projectile lives until it lands, hits, or leaves
// the world. `AbstractArrow` counts `inGroundTime` only while `in_ground` —
// 1200 ticks (60 s) — and a dislodged arrow that starts falling again resets
// the timer. A Loyalty trident returning to its owner never despawns.

/// Out-of-bounds: `Y < min_y - 64` discards regardless of family.
#[cfg_attr(not(test), allow(dead_code))]
const PROJECTILE_VOID_MARGIN: i32 = 64;
/// `AbstractArrow` ground-life budget in ticks (60 s).
#[cfg_attr(not(test), allow(dead_code))]
const ARROW_GROUND_LIFE_TICKS: u32 = 1200;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectileFamily {
    Arrow,
    SpectralArrow,
    Trident,
    /// Snowballs, ender pearls, eggs, thrown potions, experience bottles.
    Throwable,
    /// Ghast/blaze/dragon fireballs, wither skulls, wind charges.
    Fireball,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProjectileFamily {
    /// Only `AbstractArrow` subclasses carry the `in_ground` life timer.
    fn has_ground_life(self) -> bool {
        matches!(
            self,
            Self::Arrow | Self::SpectralArrow | Self::Trident
        )
    }
}

/// One tick of the observations the despawn rule reads.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct ProjectileTick {
    family: ProjectileFamily,
    in_ground: bool,
    /// A Loyalty trident on the way back to its owner.
    returning_to_owner: bool,
    y: f64,
    min_y: i32,
    chunk_loaded: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct ProjectileDespawnPolicy {
    /// `AbstractArrow.inGroundTime`.
    ground_life: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProjectileDespawnPolicy {
    fn new() -> Self {
        Self { ground_life: 0 }
    }

    fn grounded_ticks(&self) -> u32 {
        self.ground_life
    }

    /// Returns true when the projectile must be discarded this tick.
    fn tick(&mut self, tick: ProjectileTick) -> bool {
        // A returning Loyalty trident can cross the void and still lives.
        if tick.returning_to_owner {
            return false;
        }
        // No ticking in unloaded chunks — an entity that unloaded despawns.
        if !tick.chunk_loaded {
            return true;
        }
        if tick.y < f64::from(tick.min_y - PROJECTILE_VOID_MARGIN) {
            return true;
        }
        if tick.in_ground && tick.family.has_ground_life() {
            self.ground_life = self.ground_life.saturating_add(1);
            return self.ground_life >= ARROW_GROUND_LIFE_TICKS;
        }
        if !tick.in_ground {
            // In-flight or dislodged: no lifetime cap, ground timer resets.
            self.ground_life = 0;
        }
        false
    }
}

// ---------- Vanilla equines: temper, mount-bucking taming, gear & jump bar ----------
//
// `AbstractHorse` (horse/donkey/mule), `Llama`, `AbstractChestedHorse` and
// `RunAroundLikeCrazyGoal` mechanics for 1.21.1: the temper-driven tame/buck
// roll while ridden, the `handleEating` food table, the saddle/chest/armor
// equip gates, the `onPlayerJump` charge scale, and `createOffspringAttribute`
// breeding. Llamas override `getMaxTemper` (30) and accept only `LLAMA_FOOD`
// (wheat, hay block); the other kinds share `HORSE_FOOD`.

/// The four tamable `AbstractHorse` descendants this lane models.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EquineKind {
    Horse,
    Donkey,
    Mule,
    Llama,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EquineKind {
    /// `getMaxTemper`: 100 for every `AbstractHorse` descendant except the
    /// llama, which caps temper at 30.
    fn max_temper(self) -> i32 {
        match self {
            Self::Llama => 30,
            _ => 100,
        }
    }

    /// `AbstractChestedHorse`: donkeys, mules and llamas take a chest; horses
    /// take body armor instead.
    fn can_chest(self) -> bool {
        matches!(self, Self::Donkey | Self::Mule | Self::Llama)
    }

    /// `Horse.mobInteract`/`isEquippableInSlot(BODY)`: only horses wear the
    /// four armor tiers.
    fn can_wear_armor(self) -> bool {
        matches!(self, Self::Horse)
    }

    /// `HORSE_FOOD` membership (`ItemTags`): wheat, sugar, hay block, apple,
    /// carrot, golden carrot, golden apple, enchanted golden apple. `carrot`
    /// has no `handleEating` branch, so it is food by tag but does nothing.
    fn is_horse_food(item: &str) -> bool {
        matches!(
            mob_behavior_id(item),
            "wheat"
                | "sugar"
                | "hay_block"
                | "apple"
                | "carrot"
                | "golden_carrot"
                | "golden_apple"
                | "enchanted_golden_apple"
        )
    }

    /// `LLAMA_FOOD` membership: wheat and hay block only.
    fn is_llama_food(item: &str) -> bool {
        matches!(mob_behavior_id(item), "wheat" | "hay_block")
    }

    /// Whether the item enters the `fedFood` path for this kind.
    fn is_food(self, item: &str) -> bool {
        match self {
            Self::Llama => Self::is_llama_food(item),
            _ => Self::is_horse_food(item),
        }
    }

    /// One row of the vanilla `handleEating` table: heal amount, `ageUp`
    /// ticks, temper delta, and whether the item can trigger love on a tamed
    /// adult (`setInLove` for golden foods / llama hay).
    fn food_effect(self, item: &str) -> Option<EquineFoodEffect> {
        let fx = |heal: f32, age_up_ticks: i32, temper: i32, love_food: bool| {
            Some(EquineFoodEffect {
                heal,
                age_up_ticks,
                temper,
                love_food,
            })
        };
        match (self, mob_behavior_id(item)) {
            // `Llama.handleEating` over `LLAMA_FOOD`.
            (Self::Llama, "wheat") => fx(2.0, 10, 3, false),
            (Self::Llama, "hay_block") => fx(10.0, 90, 6, true),
            // Llamas reject every other item before the horse table applies.
            (Self::Llama, _) => None,
            // `AbstractHorse.handleEating` over `HORSE_FOOD`; `carrot` is in
            // the tag but has no branch and falls through to PASS.
            (_, "wheat") => fx(2.0, 20, 3, false),
            (_, "sugar") => fx(1.0, 30, 3, false),
            (_, "hay_block") => fx(20.0, 180, 0, false),
            (_, "apple") => fx(3.0, 60, 3, false),
            (_, "golden_carrot") => fx(4.0, 60, 5, true),
            (_, "golden_apple" | "enchanted_golden_apple") => fx(10.0, 240, 10, true),
            _ => None,
        }
    }
}

/// One `handleEating` row as defined above.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct EquineFoodEffect {
    heal: f32,
    age_up_ticks: i32,
    temper: i32,
    love_food: bool,
}

/// `generateJumpStrength`: `0.4 + 0.2 * (r1 + r2 + r3)` over three unit rolls.
#[cfg_attr(not(test), allow(dead_code))]
fn equine_generate_jump_strength(rolls: [f64; 3]) -> f64 {
    0.4 + 0.2 * rolls.iter().sum::<f64>()
}

/// `generateSpeed`: `(0.45 + 0.3 * (r1 + r2 + r3)) * 0.25`.
#[cfg_attr(not(test), allow(dead_code))]
fn equine_generate_move_speed(rolls: [f64; 3]) -> f64 {
    (0.45 + 0.3 * rolls.iter().sum::<f64>()) * 0.25
}

/// Vanilla spawn attribute ranges the breeding formula clamps into.
#[cfg_attr(not(test), allow(dead_code))]
const EQUINE_JUMP_STRENGTH_RANGE: (f64, f64) = (0.4, 1.0);
#[cfg_attr(not(test), allow(dead_code))]
const EQUINE_MOVE_SPEED_RANGE: (f64, f64) = (0.1125, 0.3375);
/// `generateMaxHealth`: `15 + rand(8) + rand(9)` bounds the bred health roll.
#[cfg_attr(not(test), allow(dead_code))]
const EQUINE_MAX_HEALTH_RANGE: (f64, f64) = (15.0, 30.0);

/// Result of one mount-bucking tame check (`RunAroundLikeCrazyGoal.tick`).
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EquineTameOutcome {
    /// `nextInt(maxTemper) < temper`: the horse tames to its rider.
    Tamed,
    /// The roll failed: temper rises by 5 (clamped) and the rider is thrown.
    Bucked { new_temper: i32 },
}

/// Result of `EquineState::feed`, mirroring `handleEating`/`fedFood`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
enum EquineFeedOutcome {
    /// Not this kind's food, or a full-health tamed adult with nothing to
    /// gain — vanilla returns PASS and the item is not consumed.
    Pass,
    /// Item consumed: heal/age-up/love/temper applied as vanilla does.
    Consumed {
        healed: f32,
        temper_gain: i32,
        fell_in_love: bool,
    },
}

/// Per-entity equine state: taming, temper, equipment and attributes.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
struct EquineState {
    kind: EquineKind,
    tamed: bool,
    owner: Option<[u8; 16]>,
    /// Vanilla `temper`; `modify_temper` clamps to `0..=kind.max_temper()`.
    temper: i32,
    saddled: bool,
    chested: bool,
    /// Horse body armor: 0 none, 1 leather, 2 iron, 3 gold, 4 diamond.
    armor_tier: u8,
    jump_strength: f64,
    move_speed: f64,
    health: f32,
    max_health: f32,
    /// Vanilla age in ticks: negative while a baby (`ageUp` adds toward 0).
    age_ticks: i32,
    in_love: bool,
    /// Set when this record was claimed by the live breed-stat path: the
    /// animal spawned as a foal from two in-love parents rather than from
    /// spawn dice, so `age_ticks` counts the vanilla 24000-tick grow-up.
    bred_foal: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EquineState {
    /// Base-attribute defaults (`JUMP_STRENGTH` 0.7, `MOVEMENT_SPEED` 0.225)
    /// before spawn rolls; `spawned` applies the rolled ranges instead.
    fn new(kind: EquineKind) -> Self {
        Self {
            kind,
            tamed: false,
            owner: None,
            temper: 0,
            saddled: false,
            chested: false,
            armor_tier: 0,
            jump_strength: 0.7,
            move_speed: 0.225,
            health: 30.0,
            max_health: 30.0,
            age_ticks: 0,
            in_love: false,
            bred_foal: false,
        }
    }

    /// A fresh spawn: attributes rolled via `generateJumpStrength`/
    /// `generateSpeed` from six unit rolls, health from the vanilla
    /// `generateMaxHealth` range midpoint.
    fn spawned(
        kind: EquineKind,
        jump_rolls: [f64; 3],
        speed_rolls: [f64; 3],
        health_rolls: [f64; 2],
    ) -> Self {
        let mut state = Self::new(kind);
        state.jump_strength = equine_generate_jump_strength(jump_rolls);
        state.move_speed = equine_generate_move_speed(speed_rolls);
        // `generateMaxHealth`: `15 + nextInt(8) + nextInt(9)` as unit rolls
        // approximates the integer draws over [15, 30).
        state.max_health = (15.0 + 7.0 * health_rolls[0] + 8.0 * health_rolls[1]) as f32;
        state.health = state.max_health;
        state
    }

    fn is_baby(&self) -> bool {
        self.age_ticks < 0
    }

    /// `modifyTemper`: clamp `temper + delta` into `0..=getMaxTemper()`.
    fn modify_temper(&mut self, delta: i32) -> i32 {
        self.temper = (self.temper + delta).clamp(0, self.kind.max_temper());
        self.temper
    }

    /// `AbstractHorse.tameWithName`: ownership and the tame flag together —
    /// name-tag taming and the successful `RunAroundLikeCrazyGoal` roll both
    /// land here. Returns whether the animal was untamed before.
    fn tame_with_name(&mut self, player: [u8; 16]) -> bool {
        let was_tamed = self.tamed;
        self.tamed = true;
        self.owner = Some(player);
        !was_tamed
    }

    /// One `RunAroundLikeCrazyGoal.tick` roll while an untamed equine is
    /// ridden by a player: vanilla draws `nextInt(getMaxTemper())` and tames
    /// when it lands below `temper`; otherwise `modifyTemper(5)` and the
    /// rider is bucked off (`ejectPassengers` + `makeMad` + event 6).
    ///
    /// `roll` is that draw; for the common 100-temper kinds it is a 0..=99
    /// value. Llamas draw `nextInt(30)`, so callers should pass 0..=29 there.
    /// Returns `None` once tamed — the goal only runs while `!isTamed`.
    fn try_equine_tame_tick(&mut self, roll: u32) -> Option<EquineTameOutcome> {
        if self.tamed {
            return None;
        }
        if roll < self.temper.max(0) as u32 {
            self.tamed = true;
            return Some(EquineTameOutcome::Tamed);
        }
        let new_temper = self.modify_temper(5);
        Some(EquineTameOutcome::Bucked { new_temper })
    }

    /// `fedFood`/`handleEating`: apply the kind's food table. Temper only
    /// rises when the item was actually used (heal/age-up/love) or the animal
    /// is still untamed, and never past `getMaxTemper()` — so a full-health
    /// tamed adult refuses plain foods exactly like vanilla. The `is_food`
    /// tag check runs first, mirroring `mobInteract`'s `isFood` gate.
    fn feed(&mut self, item: &str) -> EquineFeedOutcome {
        if !self.kind.is_food(item) {
            return EquineFeedOutcome::Pass;
        }
        let Some(fx) = self.kind.food_effect(item) else {
            return EquineFeedOutcome::Pass;
        };
        let mut used = false;
        let mut healed = 0.0;
        if self.health < self.max_health && fx.heal > 0.0 {
            let before = self.health;
            self.health = (self.health + fx.heal).min(self.max_health);
            healed = self.health - before;
            used = true;
        }
        if self.is_baby() && fx.age_up_ticks > 0 {
            self.age_ticks = self.age_ticks.saturating_add(fx.age_up_ticks);
            used = true;
        }
        let mut fell_in_love = false;
        if fx.love_food && self.tamed && !self.is_baby() && !self.in_love {
            self.in_love = true;
            fell_in_love = true;
            used = true;
        }
        let mut temper_gain = 0;
        if fx.temper > 0 && (used || !self.tamed) && self.temper < self.kind.max_temper() {
            let before = self.temper;
            self.modify_temper(fx.temper);
            temper_gain = self.temper - before;
            used = true;
        }
        if used {
            EquineFeedOutcome::Consumed {
                healed,
                temper_gain,
                fell_in_love,
            }
        } else {
            EquineFeedOutcome::Pass
        }
    }

    /// `isSaddleable` + `equipSaddle`: tamed adult without a saddle already.
    fn equip_saddle(&mut self) -> bool {
        if !(self.tamed && !self.is_baby() && !self.saddled) {
            return false;
        }
        self.saddled = true;
        true
    }

    /// `AbstractChestedHorse.equipChest`: tamed adult of a chested kind only.
    fn equip_chest(&mut self) -> bool {
        if !(self.kind.can_chest() && self.tamed && !self.is_baby() && !self.chested) {
            return false;
        }
        self.chested = true;
        true
    }

    /// Horse body armor (`isEquippableInSlot(BODY)` + `equipBodyArmor`):
    /// tiers 1..=4 on a tamed adult horse; vanilla refuses to swap a worn
    /// piece, so equipping requires `armor_tier == 0`.
    fn equip_armor(&mut self, tier: u8) -> bool {
        if !(self.kind.can_wear_armor()
            && (1..=4).contains(&tier)
            && self.tamed
            && !self.is_baby()
            && self.armor_tier == 0)
        {
            return false;
        }
        self.armor_tier = tier;
        true
    }

    /// `canJump`: only a saddled mount answers the jump bar.
    fn can_jump(&self) -> bool {
        self.saddled
    }
}

/// `AbstractHorse.onPlayerJump` charge → `playerJumpPendingScale`: negatives
/// clamp to 0, >=90 is full power, below that it ramps 0.4..0.8 linearly —
/// `0.4 + 0.4 * charge / 90` — the exact vanilla formula.
#[cfg_attr(not(test), allow(dead_code))]
fn equine_jump_scale(charge_0_to_100: i32) -> f64 {
    let charge = charge_0_to_100.max(0);
    if charge >= 90 {
        1.0
    } else {
        0.4 + 0.4 * charge as f64 / 90.0
    }
}

/// `LivingEntity.getJumpPower(scale)` vertical term on ordinary ground
/// (block jump factor 1.0, no jump-boost effect): `jump_strength * scale`.
/// Callers apply `getBlockJumpFactor`/`getJumpBoostPower` corrections for
/// special blocks or effects — see `wm_horse_jump_velocity`.
#[cfg_attr(not(test), allow(dead_code))]
fn equine_jump_velocity(jump_strength: f64, charge_0_to_100: i32) -> f64 {
    jump_strength * equine_jump_scale(charge_0_to_100)
}

/// `AbstractHorse.createOffspringAttribute`: clamp both parents into the
/// attribute range, average them, then perturb by `spread * draw` where
/// `spread` is the parent gap plus 30% of the range on each side, and `draw`
/// is vanilla's `(r1+r2+r3)/3 - 0.5`. `rand_unit` supplies that draw as one
/// [0,1) value (`draw = rand_unit - 0.5`); out-of-range results reflect once
/// back inside, exactly like vanilla.
#[cfg_attr(not(test), allow(dead_code))]
fn equine_offspring_attribute(
    parent_a: f64,
    parent_b: f64,
    min: f64,
    max: f64,
    rand_unit: f64,
) -> f64 {
    let a = parent_a.clamp(min, max);
    let b = parent_b.clamp(min, max);
    let pad = 0.15 * (max - min);
    let spread = (a - b).abs() + pad * 2.0;
    let average = (a + b) / 2.0;
    let result = average + spread * (rand_unit - 0.5);
    if result > max {
        max - (result - max)
    } else if result < min {
        min + (min - result)
    } else {
        result
    }
}

/// The attributes `setOffspringAttributes` rolls for a foal.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct EquineOffspringStats {
    max_health: f64,
    jump_strength: f64,
    move_speed: f64,
}

/// `breed_equine_stats`: average both parents plus the vanilla random third
/// draw per attribute. One `rand_unit` in [0,1) drives all three rolls —
/// vanilla draws independently, so callers wanting per-attribute draws can
/// call `equine_offspring_attribute` directly.
#[cfg_attr(not(test), allow(dead_code))]
fn breed_equine_stats(a: &EquineState, b: &EquineState, rand_unit: f64) -> EquineOffspringStats {
    EquineOffspringStats {
        max_health: equine_offspring_attribute(
            a.max_health as f64,
            b.max_health as f64,
            EQUINE_MAX_HEALTH_RANGE.0,
            EQUINE_MAX_HEALTH_RANGE.1,
            rand_unit,
        ),
        jump_strength: equine_offspring_attribute(
            a.jump_strength,
            b.jump_strength,
            EQUINE_JUMP_STRENGTH_RANGE.0,
            EQUINE_JUMP_STRENGTH_RANGE.1,
            rand_unit,
        ),
        move_speed: equine_offspring_attribute(
            a.move_speed,
            b.move_speed,
            EQUINE_MOVE_SPEED_RANGE.0,
            EQUINE_MOVE_SPEED_RANGE.1,
            rand_unit,
        ),
    }
}

// ---------- Vanilla parrot: food, cookie poison, sit order & shoulder perch ----------
//
// `Parrot.mobInteract`, `PARROT_FOOD`/`PARROT_POISONOUS_FOOD`,
// `LandOnOwnersShoulderGoal`, `ShoulderRidingEntity` and the player-side
// `setEntityOnShoulder`/`removeEntitiesOnShoulder` rules for 1.21.1.

/// Ticks a freshly-dropped parrot waits before it can perch again —
/// `ShoulderRidingEntity.canSitOnShoulder` requires `rideCooldownCounter > 100`.
#[cfg_attr(not(test), allow(dead_code))]
const PERCH_COOLDOWN_TICKS: u16 = 100;
/// `removeEntitiesOnShoulder` refuses to drop a parrot that has perched for
/// fewer than 20 ticks (`timeEntitySatOnShoulder + 20 < gameTime`).
#[cfg_attr(not(test), allow(dead_code))]
const SHOULDER_MIN_PERCH_TICKS: u32 = 20;

/// Per-entity parrot state.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
struct ParrotState {
    tamed: bool,
    owner: Option<[u8; 16]>,
    /// `Parrot.Variant` id (cosmetic; 0..=4 vanilla variants plus the rare 5th).
    variant: u8,
    /// `isOrderedToSit`: right-click toggles it for the owning player.
    sitting_ordered: bool,
    /// `isFlying`: grounded parrots only attempt the shoulder goal.
    flying: bool,
    leashed: bool,
    /// Currently consumed into the owner's shoulder NBT slot.
    on_shoulder: bool,
    /// `rideCooldownCounter` equivalent: ticks since the last perch ended.
    shoulder_ticks: u16,
    /// Ticks spent on the owner's shoulder since the last perch — the
    /// `timeEntitySatOnShoulder` counter `removeEntitiesOnShoulder` reads.
    perched_ticks: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ParrotState {
    fn new(variant: u8) -> Self {
        Self {
            tamed: false,
            owner: None,
            variant,
            sitting_ordered: false,
            flying: false,
            leashed: false,
            on_shoulder: false,
            shoulder_ticks: 0,
            perched_ticks: 0,
        }
    }

    /// `ShoulderRidingEntity.tick`: the re-perch cooldown only counts while
    /// the parrot exists as a world entity (not while perched as NBT).
    fn tick(&mut self) {
        if !self.on_shoulder {
            self.shoulder_ticks = self.shoulder_ticks.saturating_add(1);
        }
    }

    /// `PARROT_FOOD` membership: the six seed/pod items vanilla parrots eat.
    fn is_parrot_food(item: &str) -> bool {
        matches!(
            mob_behavior_id(item),
            "wheat_seeds"
                | "melon_seeds"
                | "pumpkin_seeds"
                | "beetroot_seeds"
                | "torchflower_seeds"
                | "pitcher_pod"
        )
    }

    /// `PARROT_POISONOUS_FOOD` membership: cookies only.
    fn is_parrot_poisonous_food(item: &str) -> bool {
        mob_behavior_id(item) == "cookie"
    }

    /// `Parrot.mobInteract` in vanilla order: untamed + parrot food eats and
    /// rolls `nextInt(10) == 0` to tame; any non-poisonous item on a tamed,
    /// owned, grounded parrot toggles the sit order; poisonous food (cookie)
    /// is consumed, applies poison and deals `Float.MAX_VALUE` damage —
    /// fatal for the parrot.
    fn mob_interact(
        &mut self,
        player: [u8; 16],
        item: &str,
        tame_roll_0_9: u32,
    ) -> ParrotInteract {
        let id = mob_behavior_id(item);
        if !self.tamed && Self::is_parrot_food(id) {
            let tamed_now = tame_roll_0_9 == 0;
            if tamed_now {
                self.tamed = true;
                self.owner = Some(player);
            }
            return ParrotInteract::Ate { tamed_now };
        }
        if !Self::is_parrot_poisonous_food(id) {
            if !self.flying && self.tamed && self.owner == Some(player) {
                self.sitting_ordered = !self.sitting_ordered;
                return ParrotInteract::Sat {
                    sitting: self.sitting_ordered,
                };
            }
            return ParrotInteract::Pass;
        }
        ParrotInteract::Poisoned
    }

    /// `LandOnOwnersShoulderGoal.tick` + `Player.setEntityOnShoulder`: the
    /// parrot must be tamed by this player, not sit-ordered, grounded,
    /// unleashed, past the 100-tick cooldown, touching the player's box, and
    /// the player must be on the ground, unmounted, dry and out of powder
    /// snow with a free shoulder slot. On success the entity is consumed
    /// into the player's shoulder NBT (`on_shoulder = true`).
    fn try_perch_on_shoulder(&mut self, ctx: &ShoulderPerchContext) -> bool {
        let player_ready = ctx.player_on_ground
            && !ctx.player_mounted
            && !ctx.player_in_water
            && !ctx.player_in_powder_snow;
        if self.on_shoulder
            || !self.tamed
            || self.owner != Some(ctx.player)
            || self.sitting_ordered
            || self.flying
            || self.leashed
            || !ctx.bounding_boxes_intersect
            || self.shoulder_ticks <= PERCH_COOLDOWN_TICKS
            || !player_ready
            || !ctx.shoulder_slot_free
        {
            return false;
        }
        self.on_shoulder = true;
        true
    }

    /// `Player.removeEntitiesOnShoulder` triggers: the owning player's fall
    /// distance over 0.5, water, elytra flight, sleep, powder snow, taking
    /// damage, or a riptide spin attack all drop shoulder entities — but only
    /// after the parrot has perched for more than 20 ticks. On a forced
    /// dismount the entity respawns in the world and the 100-tick re-perch
    /// cooldown restarts.
    fn shoulder_dismount(
        &mut self,
        hazards: &ShoulderDropConditions,
        ticks_on_shoulder: u32,
    ) -> bool {
        if !self.on_shoulder || ticks_on_shoulder <= SHOULDER_MIN_PERCH_TICKS {
            return false;
        }
        if !(hazards.fall_distance > 0.5
            || hazards.in_water
            || hazards.player_flying
            || hazards.player_sleeping
            || hazards.in_powder_snow
            || hazards.player_damaged
            || hazards.spin_attack)
        {
            return false;
        }
        self.on_shoulder = false;
        self.shoulder_ticks = 0;
        true
    }
}

/// Result of `ParrotState::mob_interact`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParrotInteract {
    /// Nothing consumed; the caller may fall through to other interact logic.
    Pass,
    /// Parrot food consumed; `tamed_now` reports the `nextInt(10) == 0` roll.
    Ate { tamed_now: bool },
    /// Cookie consumed: fatal poison + `Float.MAX_VALUE` damage (vanilla).
    Poisoned,
    /// Sit order toggled for the owning player.
    Sat { sitting: bool },
}

/// Player-side facts `try_perch_on_shoulder` reads.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
struct ShoulderPerchContext {
    player: [u8; 16],
    bounding_boxes_intersect: bool,
    player_on_ground: bool,
    player_mounted: bool,
    player_in_water: bool,
    player_in_powder_snow: bool,
    shoulder_slot_free: bool,
}

/// Player-side hazards that force `removeEntitiesOnShoulder`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
struct ShoulderDropConditions {
    fall_distance: f32,
    in_water: bool,
    player_flying: bool,
    player_sleeping: bool,
    in_powder_snow: bool,
    player_damaged: bool,
    spin_attack: bool,
}

// ---------- Generic environmental egg incubator ----------
//
// Data-driven `Block.tick`/`randomTick` hatching for turtle eggs, sniffer
// eggs and any modded fire/water/thunder egg: the spec declares the required
// environment, the per-stage tick cost and the hatch payload; `tick_incubation`
// advances deterministically and reports cracks and the final hatch. Zero
// hardcoded mod names — mods describe themselves through `Custom` tags.

/// Incubation credit per tick while sitting on a moss-family substrate —
/// the vanilla sniffer egg's 2x boost (12000 vs 24000 ticks), generalized.
#[cfg_attr(not(test), allow(dead_code))]
const MOSS_HATCH_CREDIT: u32 = 2;
/// Incubation credit per tick on a sand substrate during the night window —
/// models the turtle egg's always-crack night ticks vs the rare day chance.
#[cfg_attr(not(test), allow(dead_code))]
const NIGHT_HATCH_CREDIT: u32 = 8;
/// Stage boundaries before hatch: vanilla eggs show two crack textures then
/// hatch on the third qualifying tick (`HATCH` 0 -> 1 -> 2 -> hatch).
#[cfg_attr(not(test), allow(dead_code))]
const EGG_CRACK_STAGES: u8 = 3;
/// Vanilla `SnifferEggBlock`: 24000 ticks (20 min) on ordinary blocks,
/// halved to 12000 (10 min) on moss — modeled as 8000/stage at 1x credit.
#[cfg_attr(not(test), allow(dead_code))]
const SNIFFER_EGG_TICKS_PER_STAGE: u32 = 8_000;
/// Turtle-egg day-rate stage cost; the night credit makes the night window
/// dominant, matching vanilla's ~95% night-window hatching.
#[cfg_attr(not(test), allow(dead_code))]
const TURTLE_EGG_TICKS_PER_STAGE: u32 = 9_600;

/// Environment observations an `EggSpec` can require or an egg can sit in.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum IncubationEnv {
    /// Sand-family substrate; incubation accelerates while `is_night` holds
    /// (the turtle egg's night window).
    SandNight,
    /// Moss-family substrate; doubles incubation credit (sniffer egg).
    MossBlock,
    /// Modded: the egg sits in or adjacent to fire/lava.
    OnFire,
    /// Modded: the egg is submerged.
    SubmergedWater,
    /// Modded: a thunderstorm is active.
    Thunderstorm,
    /// Any other mod-defined environment tag, matched by exact string.
    Custom(String),
}

/// Declarative hatch recipe for one egg block.
#[cfg_attr(not(test), allow(dead_code))]
struct EggSpec {
    /// Stable spec id, conventionally the egg block id.
    id: String,
    /// Environment that must hold for any progress; `None` incubates in any
    /// environment (the sniffer egg hatches on every solid block).
    required_env: Option<IncubationEnv>,
    /// Base ticks credited per stage at 1x rate.
    ticks_per_stage: u32,
    /// Stage boundaries crossed before hatching.
    stages: u8,
    /// Entity id emitted on hatch.
    spawns_entity: String,
    /// Whether the hatched entity inherits the planter's owner id.
    inherit_owner: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl EggSpec {
    /// Vanilla `TurtleEggBlock`: sand substrate only, hatches a turtle; the
    /// night window accelerates via `NIGHT_HATCH_CREDIT`.
    fn turtle_egg() -> Self {
        Self {
            id: "minecraft:turtle_egg".into(),
            required_env: Some(IncubationEnv::SandNight),
            ticks_per_stage: TURTLE_EGG_TICKS_PER_STAGE,
            stages: EGG_CRACK_STAGES,
            spawns_entity: "minecraft:turtle".into(),
            inherit_owner: false,
        }
    }

    /// Vanilla `SnifferEggBlock`: hatches on any block — 24000 ticks normally,
    /// halved on moss blocks through `MOSS_HATCH_CREDIT`.
    fn sniffer_egg() -> Self {
        Self {
            id: "minecraft:sniffer_egg".into(),
            required_env: None,
            ticks_per_stage: SNIFFER_EGG_TICKS_PER_STAGE,
            stages: EGG_CRACK_STAGES,
            spawns_entity: "minecraft:sniffer".into(),
            inherit_owner: false,
        }
    }
}

/// Live incubation progress for one placed egg.
#[cfg_attr(not(test), allow(dead_code))]
struct IncubationState {
    spec: EggSpec,
    /// Completed stage boundaries, `0..spec.stages`.
    stage: u8,
    /// Accumulated credit inside the current stage.
    ticks_in_stage: u32,
    /// Planter id, emitted on hatch only when `spec.inherit_owner`.
    owner: Option<[u8; 16]>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl IncubationState {
    fn new(spec: EggSpec, owner: Option<[u8; 16]>) -> Self {
        Self {
            spec,
            stage: 0,
            ticks_in_stage: 0,
            owner,
        }
    }

    fn stage(&self) -> u8 {
        self.stage
    }

    fn hatched(&self) -> bool {
        self.stage >= self.spec.stages
    }
}

/// What one `tick_incubation` call produced.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum HatchResult {
    /// The required environment did not hold, the stage is unfinished, or the
    /// egg already hatched.
    Waiting,
    /// One crack stage completed this tick.
    Cracked { stage: u8 },
    /// All stages done: the egg hatches `spec.spawns_entity`.
    Hatched {
        entity_id: String,
        owner: Option<[u8; 16]>,
    },
}

/// Advance one placed egg by one game tick. Progress accrues only while
/// `current_env` satisfies `spec.required_env` (`None` accepts everything,
/// `Custom` tags compare by string). Credit modifiers are environmental:
/// moss-family substrates double the rate and sand substrates multiply it
/// during the night window — the vanilla sniffer/turtle rules generalized
/// to every spec. Emits `HatchResult::Hatched` exactly once, when `stage`
/// reaches `spec.stages`.
#[cfg_attr(not(test), allow(dead_code))]
fn tick_incubation(
    state: &mut IncubationState,
    current_env: &IncubationEnv,
    is_night: bool,
) -> HatchResult {
    if state.hatched() {
        return HatchResult::Waiting;
    }
    let env_ok = match &state.spec.required_env {
        None => true,
        Some(required) => required == current_env,
    };
    if !env_ok {
        return HatchResult::Waiting;
    }
    let mut credit = 1u32;
    if *current_env == IncubationEnv::MossBlock {
        credit *= MOSS_HATCH_CREDIT;
    }
    if *current_env == IncubationEnv::SandNight && is_night {
        credit *= NIGHT_HATCH_CREDIT;
    }
    state.ticks_in_stage = state.ticks_in_stage.saturating_add(credit);
    let stage_len = state.spec.ticks_per_stage.max(1);
    if state.ticks_in_stage < stage_len {
        return HatchResult::Waiting;
    }
    state.ticks_in_stage -= stage_len;
    state.stage += 1;
    if state.stage >= state.spec.stages {
        let owner = if state.spec.inherit_owner {
            state.owner
        } else {
            None
        };
        return HatchResult::Hatched {
            entity_id: state.spec.spawns_entity.clone(),
            owner,
        };
    }
    HatchResult::Cracked { stage: state.stage }
}

// ---- Live egg-block incubation ---------------------------------------------
//
// `TurtleEggBlock.randomTick`/`SnifferEggBlock.randomTick`: every random tick
// on a placed egg advances its incubation record through `tick_incubation`,
// writes the crack `hatch` stage back to the block state and, on the final
// stage, spawns the hatchling and consumes one `eggs` count (or the whole
// block when the last egg hatches). `p2_plant_random_tick` claims these names,
// so the live `on_random_tick_dim` driver in `main.rs` runs the model for
// every in-world egg.

/// Per-block incubation progress, keyed like the block-entity registry.
#[cfg_attr(not(test), allow(dead_code))]
fn live_incubations() -> &'static Mutex<HashMap<(DimId, BlockPos), IncubationState>> {
    static INCUBATIONS: OnceLock<Mutex<HashMap<(DimId, BlockPos), IncubationState>>> =
        OnceLock::new();
    INCUBATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The substrate observation an egg block sits on: sand-family below maps to
/// `SandNight`, moss to `MossBlock`; anything else is a plain custom tag that
/// satisfies only `required_env: None` specs.
#[cfg_attr(not(test), allow(dead_code))]
fn incubation_env_below(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    pos: BlockPos,
) -> IncubationEnv {
    match reg.name_of(block_at_dim(dim, pos.0, pos.1 - 1, pos.2)) {
        "minecraft:sand" | "minecraft:red_sand" => IncubationEnv::SandNight,
        "minecraft:moss_block" | "minecraft:moss_carpet" => IncubationEnv::MossBlock,
        name => IncubationEnv::Custom(name.to_string()),
    }
}

/// `randomTick` for `turtle_egg` and `sniffer_egg` blocks. Returns `true`
/// once the block name is claimed (even when the egg simply accrues credit).
#[cfg_attr(not(test), allow(dead_code))]
fn egg_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) -> bool {
    let spec = match reg.name_of(st) {
        "minecraft:turtle_egg" => EggSpec::turtle_egg(),
        "minecraft:sniffer_egg" => EggSpec::sniffer_egg(),
        _ => return false,
    };
    let env = incubation_env_below(reg, dim, (x, y, z));
    let day_time = WORLD_TIME.load(Ordering::Relaxed) % 24_000;
    let is_night = day_time >= 13_000;
    let outcome = {
        let mut store = live_incubations().lock().unwrap_or_else(|e| e.into_inner());
        let state = store
            .entry((dim, (x, y, z)))
            .or_insert_with(|| IncubationState::new(spec, None));
        tick_incubation(state, &env, is_night)
    };
    match outcome {
        HatchResult::Waiting => {}
        HatchResult::Cracked { stage } => {
            let cracked = reg.with_prop(st, "hatch", &stage.min(2).to_string());
            if cracked != st {
                set_block_dim(dim, x, y, z, cracked);
                note_block_update_dim(dim, x, y, z, cracked);
            }
        }
        HatchResult::Hatched { entity_id, .. } => {
            egg_incubation_remove(dim, (x, y, z));
            let eggs: u8 = reg
                .get_prop(st, "eggs")
                .and_then(|count| count.parse().ok())
                .unwrap_or(1);
            if eggs > 1 {
                let remaining = reg.with_prop(
                    reg.with_prop(st, "eggs", &(eggs - 1).to_string()),
                    "hatch",
                    "0",
                );
                set_block_dim(dim, x, y, z, remaining);
                note_block_update_dim(dim, x, y, z, remaining);
            } else {
                let air = reg.state_of("minecraft:air", None);
                set_block_dim(dim, x, y, z, air);
                note_block_update_dim(dim, x, y, z, air);
            }
            live_spawn_named(
                dim,
                &entity_id,
                (x as f64 + 0.5, y as f64, z as f64 + 0.5, 0.0, 0.0),
            );
        }
    }
    true
}

/// Drop the incubation record of an egg block that no longer sits at `pos`
/// (broken, replaced, or pushed). Called by the block-entity reconcile pass.
#[cfg_attr(not(test), allow(dead_code))]
fn egg_incubation_remove(dim: DimId, pos: BlockPos) {
    live_incubations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(dim, pos));
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

    #[test]
    fn witch_drinking_prioritizes_water_fire_health_and_distance_at_chance_boundaries() {
        use WitchPotion::*;
        let all_needs = WitchNeeds {
            underwater: true,
            in_fire_or_lava: true,
            hurt: true,
            chase_distance: Some(12.0),
        };
        for (active, expected) in [
            (&[][..], WaterBreathing),
            (&[WaterBreathing][..], FireResistance),
            (&[WaterBreathing, FireResistance][..], Healing),
            (&[WaterBreathing, FireResistance, Healing][..], Swiftness),
        ] {
            let mut witch = WitchBehavior::default();
            assert_eq!(
                witch.tick_drinking(all_needs, active, &mut zero),
                Some(WitchAction::StartDrinking(expected))
            );
        }
        let mut witch = WitchBehavior::default();
        assert_eq!(
            witch.tick_drinking(
                all_needs,
                &[WaterBreathing, FireResistance, Healing, Swiftness],
                &mut no_draw
            ),
            None
        );

        // A failed roll falls through to the next unmet need; a passed roll keeps
        // the higher-priority potion.
        let mut witch = WitchBehavior::default();
        assert_eq!(
            witch.tick_drinking(all_needs, &[], &mut rolls([15, 14])),
            Some(WitchAction::StartDrinking(FireResistance))
        );
        let mut witch = WitchBehavior::default();
        assert_eq!(
            witch.tick_drinking(all_needs, &[], &mut rolls([14, 99, 99, 99])),
            Some(WitchAction::StartDrinking(WaterBreathing))
        );

        // 11.0 is not farther than 11 blocks, 11.01 is.
        let near = WitchNeeds {
            chase_distance: Some(11.0),
            ..WitchNeeds::default()
        };
        let far = WitchNeeds {
            chase_distance: Some(11.01),
            ..WitchNeeds::default()
        };
        assert_eq!(
            WitchBehavior::default().tick_drinking(near, &[], &mut no_draw),
            None
        );
        assert_eq!(
            WitchBehavior::default().tick_drinking(far, &[], &mut zero),
            Some(WitchAction::StartDrinking(Swiftness))
        );

        // Each chance is exclusive at its upper bound.
        let water = WitchNeeds {
            underwater: true,
            ..WitchNeeds::default()
        };
        let hurt = WitchNeeds {
            hurt: true,
            ..WitchNeeds::default()
        };
        assert_eq!(
            WitchBehavior::default().tick_drinking(water, &[], &mut rolls([14])),
            Some(WitchAction::StartDrinking(WaterBreathing))
        );
        assert_eq!(
            WitchBehavior::default().tick_drinking(water, &[], &mut rolls([15])),
            None
        );
        assert_eq!(
            WitchBehavior::default().tick_drinking(hurt, &[], &mut rolls([4])),
            Some(WitchAction::StartDrinking(Healing))
        );
        assert_eq!(
            WitchBehavior::default().tick_drinking(hurt, &[], &mut rolls([5])),
            None
        );
        assert_eq!(
            WitchBehavior::default().tick_drinking(far, &[], &mut rolls([49])),
            Some(WitchAction::StartDrinking(Swiftness))
        );
        assert_eq!(
            WitchBehavior::default().tick_drinking(far, &[], &mut rolls([50])),
            None
        );
    }

    #[test]
    fn witch_dispatch_heals_four_at_half_health_and_only_while_still_hurt() {
        use MobBehaviorEffect::*;
        let world = || MobBehaviorWorldTick::Witch {
            eid: 42,
            active_effects: &[],
            target_effects: &[],
            throw_ready: true,
        };

        let mut state = MobBehaviorState::for_etype(118).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.hp = 5.0; // exactly a quarter of max health, well below the half-health gate
        ctx.rng_u64 = 3; // first draw is 3 % 100 < 5, so Healing is chosen
        assert!(state.step_with_world(&ctx, world()).is_empty());
        for _ in 0..31 {
            assert!(state.step_with_world(&ctx, world()).is_empty());
        }
        assert_eq!(state.step_with_world(&ctx, world()), [HealSelf(4.0)]);

        // Reaching full health while drinking cancels the heal itself.
        let mut state = MobBehaviorState::for_etype(118).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.hp = 5.0;
        ctx.rng_u64 = 3;
        assert!(state.step_with_world(&ctx, world()).is_empty());
        ctx.hp = ctx.max_hp;
        for _ in 0..31 {
            assert!(state.step_with_world(&ctx, world()).is_empty());
        }
        assert!(state.step_with_world(&ctx, world()).is_empty());
    }

    #[test]
    fn witch_splash_falls_back_to_harming_when_every_stronger_option_is_blocked() {
        use WitchPotion::*;
        let witch = WitchBehavior::default();
        let splash = |distance, hp, effects, draw| {
            witch.splash(
                WitchTarget {
                    entity: 1,
                    horizontal_distance: distance,
                    hp,
                    effects,
                },
                &mut rolls(draw),
            )
        };
        assert_eq!(
            splash(7.99, 7.99, &[Slowness, Poison, Weakness], [0]),
            Some(WitchAction::Splash {
                target: 1,
                potion: Harming
            })
        );
        assert_eq!(
            splash(100.0, 100.0, &[Slowness, Poison, Weakness], [3]),
            Some(WitchAction::Splash {
                target: 1,
                potion: Harming
            })
        );
        assert_eq!(
            splash(3.0, 7.0, &[], [0]),
            Some(WitchAction::Splash {
                target: 1,
                potion: Weakness
            })
        );
        assert_eq!(
            splash(3.0, 7.0, &[], [3]),
            Some(WitchAction::Splash {
                target: 1,
                potion: Harming
            })
        );
    }

    #[test]
    fn creeper_swell_holds_only_inside_the_cancel_band_and_charging_doubles_power() {
        let mut creeper = CreeperBehavior::new();
        assert_eq!(creeper.fuse, -1);
        assert_eq!(creeper.tick(Some(3.0)), None);
        assert_eq!(creeper.fuse, 0);
        for _ in 0..3 {
            assert_eq!(creeper.tick(Some(7.0)), None);
        }
        assert_eq!(creeper.fuse, 0);
        assert_eq!(creeper.tick(Some(7.0001)), None);
        assert_eq!(creeper.fuse, -1);
        assert_eq!(creeper.tick(None), None);
        assert_eq!(creeper.fuse, -1);

        creeper.ignite(true);
        assert_eq!(creeper.fuse, 0);
        for _ in 0..10 {
            assert_eq!(creeper.tick(None), None);
        }
        assert_eq!(creeper.fuse, 10);
        creeper.ignite(true);
        assert_eq!(creeper.fuse, 10);
        creeper.ignite(false);
        assert_eq!(creeper.fuse, -1);
        creeper.ignite(true);
        assert_eq!(creeper.fuse, 0);
        for _ in 0..29 {
            assert_eq!(creeper.tick(None), None);
        }
        assert_eq!(creeper.tick(None), Some(3.0));

        for (charged, power) in [(false, 3.0), (true, 6.0)] {
            let mut creeper = CreeperBehavior {
                charged,
                ..CreeperBehavior::new()
            };
            creeper.ignite(true);
            for _ in 0..29 {
                assert_eq!(creeper.tick(None), None);
            }
            assert_eq!(creeper.tick(None), Some(power));
        }
    }

    #[test]
    fn enderman_stare_cone_and_range_are_strict_at_their_boundaries() {
        let player = [0.0, 1.6, 0.0];
        let look = [1.0, 0.0, 0.0];
        assert!(enderman_stared_at(player, look, [64.0, 1.6, 0.0], "air", true));
        assert!(!enderman_stared_at(
            player,
            look,
            [64.0001, 1.6, 0.0],
            "air",
            true
        ));
        // At ten blocks the cone is cos > 1 - 0.025 / 10 = 0.9975.
        let target = [10.0, 1.6, 0.0];
        assert!(enderman_stared_at(
            player,
            [1.0, 0.0, 0.069],
            target,
            "air",
            true
        ));
        assert!(!enderman_stared_at(
            player,
            [1.0, 0.0, 0.071],
            target,
            "air",
            true
        ));
        assert!(!enderman_stared_at(player, [0.0; 3], target, "air", true));
        assert!(!enderman_stared_at(player, look, player, "air", true));
    }

    #[test]
    fn enderman_escape_rejects_world_edge_and_below_minimum_ground() {
        // The chosen y is origin + 63 - 32 = 95; feet at 96 leave no headroom in
        // a world ending at 96, so the destination is rejected.
        let escape = enderman_escape(
            EndermanHazard::Water,
            [0.0, 64.0, 0.0],
            -64..96,
            |[_, y, _]| EndermanTeleportBlock {
                blocks_motion: y == 95,
                liquid: false,
            },
            |_| true,
            &mut |bound| if bound == 64 { 63 } else { 1 << 23 },
        );
        assert_eq!(escape.destination, None);

        // With the chosen y at 32, ground at 30 is below min_ground 31 and never
        // considered.
        let escape = enderman_escape(
            EndermanHazard::Rain,
            [0.0, 64.0, 0.0],
            0..320,
            |[_, y, _]| EndermanTeleportBlock {
                blocks_motion: y == 30,
                liquid: false,
            },
            |_| true,
            &mut |bound| if bound == 64 { 0 } else { 1 << 23 },
        );
        assert_eq!(escape.destination, None);
        assert!(!escape.reject_damage);
    }

    #[test]
    fn pet_taming_skips_draws_when_ineligible_and_assist_follows_an_untamed_owner() {
        let owner = [1; 16];
        let visitor = [2; 16];
        let mut wolf = PetBehavior::new(PetKind::Wolf);
        assert_eq!(
            wolf.try_tame(owner, "bone", &mut highest),
            PetTameResult::Consumed
        );
        assert_eq!(
            wolf.try_tame(owner, "bone", &mut zero),
            PetTameResult::Tamed
        );
        assert_eq!(
            wolf.try_tame(visitor, "bone", &mut no_draw),
            PetTameResult::Pass
        );
        assert!(wolf.sitting);
        assert_eq!(wolf.wolf_assist_target(owner, Some(11), None), None);
        assert_eq!(wolf.toggle_sitting(owner), Some(false));
        assert_eq!(wolf.wolf_assist_target(owner, Some(11), None), Some(11));
        assert!(!wolf.dye_collar(owner, 16));
        assert!(!wolf.dye_collar(owner, 14));
        assert!(wolf.dye_collar(owner, 15));

        let mut cat = PetBehavior::new(PetKind::Cat);
        assert_eq!(
            cat.try_tame(owner, "cooked_salmon", &mut no_draw),
            PetTameResult::Pass
        );
        assert_eq!(
            cat.try_tame(owner, "salmon", &mut zero),
            PetTameResult::Tamed
        );
        assert_eq!(
            cat.try_tame(visitor, "cod", &mut no_draw),
            PetTameResult::Pass
        );
        assert_eq!(cat.toggle_sitting(visitor), None);
        assert_eq!(cat.toggle_sitting(owner), Some(false));
    }

    #[test]
    fn iron_golem_aggregates_aggro_and_knockup_including_coincident_positions() {
        assert!(iron_golem_provoked(true, true));
        assert!(iron_golem_provoked(true, false));
        assert!(iron_golem_provoked(false, true));
        assert!(!iron_golem_provoked(false, false));

        assert_eq!(
            iron_golem_fling_velocity([0.0; 3], [0.0; 3]),
            [0.0, 0.4, 0.0]
        );
        assert_eq!(
            iron_golem_fling_velocity([0.0, 50.0, 0.0], [0.0; 3]),
            [0.0, 0.4, 0.0]
        );
        let fling = iron_golem_fling_velocity([3.0, 10.0, 4.0], [0.0, 10.0, 0.0]);
        assert!((fling[0] - 0.24).abs() < 1e-12);
        assert_eq!(fling[1], 0.4);
        assert!((fling[2] - 0.32).abs() < 1e-12);
    }
}

/// P2 parity tests: neutral-mob gamerules and entity cramming.
#[cfg(test)]
mod p2_parity_mob_tests {
    use super::*;

    /// Restores gamerule atomics a test changed, on every exit path.
    struct Rules(Vec<(&'static str, i32)>);
    impl Rules {
        fn take(names: &[&'static str]) -> Self {
            Rules(
                names
                    .iter()
                    .map(|name| (*name, gamerule_atomic_value(name).unwrap_or(0)))
                    .collect(),
            )
        }
    }
    impl std::ops::Drop for Rules {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                gamerule_atomic_store(name, value);
            }
        }
    }

    /// Restores stored-only gamerule map entries a test changed.
    struct StoredRules(Vec<(&'static str, i32)>);
    impl StoredRules {
        fn take(names: &[&'static str]) -> Self {
            let map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            StoredRules(
                names
                    .iter()
                    .map(|name| (*name, map.get(*name).copied().unwrap_or(0)))
                    .collect(),
            )
        }
        fn set(&self, name: &'static str, value: i32) {
            command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert(name, value);
        }
    }
    impl std::ops::Drop for StoredRules {
        fn drop(&mut self) {
            let mut map = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            for (name, value) in self.0.drain(..) {
                map.insert(name, value);
            }
        }
    }

    #[test]
    fn p2_parity_universal_anger_targets_every_player_when_on() {
        let _stored = StoredRules::take(&["universalAnger"]);
        _stored.set("universalAnger", 0);
        let mut piglin = PiglinBehavior::default();
        assert!(piglin.provoke(11, 1.0, PiglinProvocation::Opened("chest")));
        // The provoker is chased even in golden armor; another player in
        // golden armor stays safe without universalAnger.
        assert!(piglin.hostile_to(11, &["golden_helmet"]));
        assert!(
            !piglin.hostile_to(12, &["golden_helmet"]),
            "without universalAnger golden armor still pacifies non-provokers"
        );
        // And the grudge decays on the vanilla 600-tick anger timer.
        for _ in 0..700 {
            piglin.tick(true, &mut |_| 0);
        }
        assert!(
            !piglin.hostile_to(11, &["golden_helmet"]),
            "a normal grudge expires after 600 ticks"
        );

        _stored.set("universalAnger", 1);
        assert!(universal_anger_enabled());
        let mut universal = PiglinBehavior::default();
        assert!(universal.provoke(11, 1.0, PiglinProvocation::Opened("chest")));
        // The universal flag is sampled live at provoke time.
        assert!(universal.universal);
        // Universal anger holds past the 600-tick timer while the rule stays
        // on, so the piglin never stands down.
        for _ in 0..700 {
            universal.tick(true, &mut |_| 0);
        }
        assert!(
            universal.hostile_to(12, &["iron_helmet"]),
            "universal anger must not decay on the 600-tick timer"
        );
        // Golden armor still pacifies the piglin itself.
        assert!(!universal.hostile_to(12, &["golden_helmet"]));
    }

    #[test]
    fn p2_parity_forgive_dead_players_clears_the_grudge() {
        let _stored = StoredRules::take(&["forgiveDeadPlayers"]);
        let mut piglin = PiglinBehavior::default();
        piglin.provoke(11, 1.0, PiglinProvocation::Opened("chest"));

        _stored.set("forgiveDeadPlayers", 0);
        piglin.forgive_target(true);
        assert!(
            piglin.hostile_to(11, &["golden_helmet"]) || piglin.anger.is_some(),
            "forgiveDeadPlayers=false keeps the grudge even after the target dies"
        );

        _stored.set("forgiveDeadPlayers", 1);
        piglin.forgive_target(true);
        assert!(
            piglin.anger.is_none(),
            "forgiveDeadPlayers=true forgives the dead target"
        );
        // A living target is never forgiven.
        piglin.provoke(12, 1.0, PiglinProvocation::Opened("chest"));
        piglin.forgive_target(false);
        assert!(piglin.anger.is_some());
    }

    #[test]
    fn p2_parity_forgive_dead_players_flows_through_the_live_step() {
        let _stored = StoredRules::take(&["forgiveDeadPlayers"]);
        _stored.set("forgiveDeadPlayers", 1);
        let mut state = MobBehaviorState::for_etype(78).expect("piglin behavior");
        let MobBehaviorState::Piglin(piglin) = &mut state else {
            unreachable!()
        };
        piglin.provoke(11, 1.0, PiglinProvocation::Opened("chest"));
        let ctx = MobBehaviorTickCtx {
            dim: DimId::Nether,
            pos: [0.0, 64.0, 0.0],
            hp: 16.0,
            max_hp: 16.0,
            in_water: false,
            in_rain: false,
            on_fire: false,
            nearest_target: Some(MobTargetSnapshot {
                eid: 11,
                pos: [1.0, 64.0, 0.0],
                hp: 0.0,
                visible: true,
            }),
            rng_u64: 0,
        };
        state.step(&ctx);
        let MobBehaviorState::Piglin(piglin) = &state else {
            unreachable!()
        };
        assert!(
            piglin.anger.is_none(),
            "a dead grudge target must be forgiven inside the live step"
        );
    }

    #[test]
    fn p2_parity_mob_griefing_gates_piglin_item_pickup() {
        let _rules = Rules::take(&["mobGriefing"]);
        let mut piglin = PiglinBehavior::default();
        gamerule_atomic_store("mobGriefing", 0);
        assert!(!mob_griefing_enabled());
        assert!(
            !piglin.offer(7, "gold_ingot"),
            "mobGriefing=false must block the barter pickup"
        );
        gamerule_atomic_store("mobGriefing", 1);
        assert!(mob_griefing_enabled());
        assert!(piglin.offer(7, "gold_ingot"));
    }

    #[test]
    fn p2_parity_max_entity_cramming_reads_atomic_and_zero_disables() {
        let _rules = Rules::take(&["maxEntityCramming"]);
        gamerule_atomic_store("maxEntityCramming", 4);
        assert_eq!(max_entity_cramming_limit(), 4);
        assert_eq!(eval_entity_cramming_damage(4, 4, 4, 0), 6.0);
        assert_eq!(eval_entity_cramming_damage(4, 3, 3, 0), 0.0);
        assert_eq!(
            eval_entity_cramming_damage(4, 4, 3, 0),
            0.0,
            "riders do not count toward the cramming limit"
        );
        assert_eq!(eval_entity_cramming_damage(4, 4, 4, 2), 0.0);
        gamerule_atomic_store("maxEntityCramming", 0);
        assert_eq!(max_entity_cramming_limit(), 0);
        assert_eq!(
            eval_entity_cramming_damage(0, 100, 100, 0),
            0.0,
            "maxEntityCramming=0 disables cramming damage"
        );
        gamerule_atomic_store("maxEntityCramming", 24);
    }
}

#[cfg(test)]
mod p6a_parity_tests {
    use super::*;

    fn zero(_: u32) -> u32 {
        0
    }

    // ---------- #219: hostile families ----------

    #[test]
    fn p6a_parity_enderman_stare_angle_pumpkin_and_teleport_bounds() {
        let eye = [0.0, 1.62, 0.0];
        let enderman = [10.0, 2.55, 0.0];
        // A direct stare inside 64 blocks aggroes; a carved pumpkin blinds it.
        assert!(enderman_stared_at(eye, [1.0, 0.06, 0.0], enderman, "air", true));
        assert!(!enderman_stared_at(
            eye,
            [1.0, 0.06, 0.0],
            enderman,
            "minecraft:carved_pumpkin",
            true
        ));
        // Blocked line of sight never aggroes, whatever the angle.
        assert!(!enderman_stared_at(eye, [1.0, 0.06, 0.0], enderman, "air", false));
        // Perpendicular gaze is far outside the 0.025 cone.
        assert!(!enderman_stared_at(eye, [0.0, 0.0, 1.0], enderman, "air", true));
        // Past 64 blocks there is no stare check at all.
        assert!(!enderman_stared_at(eye, [1.0, 0.0, 0.0], [65.0, 2.55, 0.0], "air", true));

        // A water escape keeps the destination within the vanilla attempt box
        // and a projectile escape additionally rejects the incoming damage.
        let draws = std::cell::Cell::new(0u32);
        let mut draw = |bound: u32| {
            draws.set(draws.get() + 1);
            bound / 2
        };
        let escape = enderman_escape(
            EndermanHazard::Water,
            [0.0, 70.0, 0.0],
            -64..320,
            |[_, y, _]| EndermanTeleportBlock {
                blocks_motion: y == 60,
                liquid: false,
            },
            |_| true,
            &mut draw,
        );
        let [x, y, z] = escape.destination.expect("open world teleports");
        assert!((-32.0..=32.0).contains(&x) && (-32.0..=32.0).contains(&z));
        assert!((38.0..=102.0).contains(&y), "y must stay within +/-32 of 70");
        assert!(!escape.reject_damage);
        assert!(draws.get() <= 3 * 64, "at most 64 attempts of three draws");
        // With no legal destination the 64-attempt budget exhausts at 192 draws.
        draws.set(0);
        let nowhere = enderman_escape(
            EndermanHazard::Projectile,
            [0.0, 70.0, 0.0],
            -64..320,
            |_| EndermanTeleportBlock::default(),
            |_| false,
            &mut draw,
        );
        assert!(nowhere.destination.is_none());
        assert!(nowhere.reject_damage);
        assert_eq!(draws.get(), 192);
    }

    #[test]
    fn p6a_parity_blaze_burst_hazards_and_hover() {
        // 60-tick charge, three shots at 6-tick spacing, 100-tick cooldown.
        let mut blaze = BlazeBehavior::new();
        for _ in 0..60 {
            assert!(blaze.tick(Some((7, 12.0))).is_none());
        }
        assert!(blaze.on_fire());
        for _ in 0..60 {
            assert!(blaze.tick(Some((7, 12.0))).is_none());
        }
        let mut shots = 0;
        for gap in [1, 6, 6] {
            for _ in 0..gap {
                if let Some(BlazeAttack::SmallFireball { target: 7 }) =
                    blaze.tick(Some((7, 12.0)))
                {
                    shots += 1;
                }
            }
        }
        assert_eq!(shots, 3);
        assert_eq!(blaze.phase, BlazePhase::Cooldown(100));
        // Water hurts for 1, a snowball for 3.
        assert_eq!(blaze_hazard_damage(BlazeHazard::Water), 1.0);
        assert_eq!(blaze_hazard_damage(BlazeHazard::Snowball), 3.0);
        // The flight hover target is half a block above the victim's mid-body.
        assert_eq!(blaze_hover_y([0.0, 64.0, 0.0], 1.8), 65.4);
    }

    #[test]
    fn p6a_parity_witch_drinks_and_chooses_splash_by_distance_and_hp() {
        // Drinking priorities: water breathing underwater, fire resistance in
        // fire, healing while hurt, swiftness against a far chase.
        let mut witch = WitchBehavior::default();
        let needs = |underwater, fire, hurt, chase| WitchNeeds {
            underwater,
            in_fire_or_lava: fire,
            hurt,
            chase_distance: chase,
        };
        assert_eq!(
            witch.tick_drinking(needs(true, false, false, None), &[], &mut zero),
            Some(WitchAction::StartDrinking(WitchPotion::WaterBreathing))
        );
        let mut witch = WitchBehavior::default();
        assert_eq!(
            witch.tick_drinking(needs(false, true, false, None), &[], &mut zero),
            Some(WitchAction::StartDrinking(WitchPotion::FireResistance))
        );
        let mut witch = WitchBehavior::default();
        assert_eq!(
            witch.tick_drinking(needs(false, false, true, None), &[], &mut zero),
            Some(WitchAction::StartDrinking(WitchPotion::Healing))
        );
        // Splash selection: Slowness beyond 8m, Poison at target hp >= 8,
        // Weakness inside 3m on a 1-in-4 roll, otherwise Harming.
        let witch = WitchBehavior::default();
        assert_eq!(
            witch.splash(
                WitchTarget {
                    entity: 7,
                    horizontal_distance: 9.0,
                    hp: 20.0,
                    effects: &[],
                },
                &mut zero,
            ),
            Some(WitchAction::Splash {
                target: 7,
                potion: WitchPotion::Slowness
            })
        );
        assert_eq!(
            witch.splash(
                WitchTarget {
                    entity: 7,
                    horizontal_distance: 5.0,
                    hp: 8.0,
                    effects: &[],
                },
                &mut zero,
            ),
            Some(WitchAction::Splash {
                target: 7,
                potion: WitchPotion::Poison
            })
        );
        assert_eq!(
            witch.splash(
                WitchTarget {
                    entity: 7,
                    horizontal_distance: 2.0,
                    hp: 2.0,
                    effects: &[],
                },
                &mut zero,
            ),
            Some(WitchAction::Splash {
                target: 7,
                potion: WitchPotion::Weakness
            })
        );
        assert_eq!(
            witch.splash(
                WitchTarget {
                    entity: 7,
                    horizontal_distance: 2.0,
                    hp: 2.0,
                    effects: &[WitchPotion::Weakness],
                },
                &mut zero,
            ),
            Some(WitchAction::Splash {
                target: 7,
                potion: WitchPotion::Harming
            })
        );
    }

    #[test]
    fn p6a_parity_phantom_circles_swoops_hits_and_climbs() {
        let mut phantom = PhantomBehavior::new();
        let target_pos = [10.0, 64.0, -4.0];
        // First tick latches the orbit anchor 20–30 blocks above the target.
        let tick = phantom.tick([0.0, 90.0, 0.0], Some((7, target_pos)), &mut zero);
        let anchor = phantom.anchor.expect("circling needs an anchor");
        assert!((84.0..=94.0).contains(&anchor[1]));
        assert!((target_pos[0] - 10.0..=target_pos[0] + 10.0).contains(&anchor[0]));
        assert_eq!(tick.velocity, [0.0; 3]);
        assert_eq!(phantom.phase, PhantomPhase::Circle);
        // Orbit until the dive: 60 circle ticks then a swoop.
        for _ in 0..58 {
            let tick = phantom.tick(anchor, Some((7, target_pos)), &mut zero);
            assert_eq!(phantom.phase, PhantomPhase::Circle);
            assert!(tick.attack.is_none());
        }
        let _ = phantom.tick(anchor, Some((7, target_pos)), &mut zero);
        assert_eq!(phantom.phase, PhantomPhase::Swoop);
        // Dive: 3 blocks over the victim closes inside a tick or two.
        let mut pos = [target_pos[0], target_pos[1] + 3.0, target_pos[2]];
        let mut hit = None;
        for _ in 0..PhantomBehavior::SWOOP_TIMEOUT_TICKS {
            let tick = phantom.tick(pos, Some((7, target_pos)), &mut zero);
            pos = std::array::from_fn(|i| pos[i] + tick.velocity[i]);
            if let Some(eid) = tick.attack {
                hit = Some(eid);
                break;
            }
        }
        assert_eq!(hit, Some(7));
        assert_eq!(PHANTOM_ATTACK_DAMAGE, 2.0);
        assert_eq!(phantom.phase, PhantomPhase::Climb);
        // The climb restores altitude and falls back to circling.
        let mut pos = target_pos;
        for _ in 0..PhantomBehavior::CLIMB_TIMEOUT_TICKS + 10 {
            let tick = phantom.tick(pos, Some((7, target_pos)), &mut zero);
            if phantom.phase == PhantomPhase::Circle {
                break;
            }
            pos = std::array::from_fn(|i| pos[i] + tick.velocity[i]);
        }
        assert_eq!(phantom.phase, PhantomPhase::Circle);
        // Daylight burns phantoms in the open sky.
        assert!(phantom_burns_in_sun(true, true, 0.6));
        assert!(!phantom_burns_in_sun(false, true, 0.6));
        assert!(!phantom_burns_in_sun(true, false, 0.6));
    }

    #[test]
    fn p6a_parity_pillager_charges_25_ticks_and_strafes_in_band() {
        let mut pillager = PillagerBehavior::new();
        for _ in 0..PILLAGER_CROSSBOW_CHARGE_TICKS {
            assert!(pillager.tick(Some(7)).is_none());
        }
        assert_eq!(
            pillager.tick(Some(7)),
            Some(PillagerAction::Shoot { target: 7 })
        );
        // A lost target re-arms the charge.
        for _ in 0..10 {
            assert!(pillager.tick(None).is_none());
        }
        for _ in 0..PILLAGER_CROSSBOW_CHARGE_TICKS {
            assert!(pillager.tick(Some(7)).is_none());
        }
        assert_eq!(
            pillager.tick(Some(7)),
            Some(PillagerAction::Shoot { target: 7 })
        );
        // The strafe only applies inside the 4–12 m ring.
        assert_eq!(pillager.strafe_velocity(3.9, true), [0.0; 3]);
        assert_eq!(pillager.strafe_velocity(12.1, true), [0.0; 3]);
        assert_ne!(pillager.strafe_velocity(8.0, true), [0.0; 3]);
    }

    #[test]
    fn p6a_parity_evoker_fangs_vex_cooldown_and_wololo() {
        let pos = [0.0, 64.0, 0.0];
        // Vex summon wins when its cooldown is ready; it casts for 40 ticks.
        let mut evoker = EvokerBehavior::new();
        for _ in 0..EVOKER_SPELL_WARMUP {
            assert!(evoker.tick(pos, Some((7, [10.0, 64.0, 0.0])), &[]).is_none());
            assert!(evoker.casting());
        }
        assert_eq!(
            evoker.tick(pos, Some((7, [10.0, 64.0, 0.0])), &[]),
            Some(EvokerEvent::SummonVex { count: 3 })
        );
        assert_eq!(evoker.vex_cooldown, EVOKER_VEX_COOLDOWN);
        // While the vex cooldown runs the evoker falls back to fangs: a line
        // of 16 marching toward a 10 m victim.
        for _ in 0..EVOKER_SPELL_WARMUP {
            assert!(evoker.tick(pos, Some((7, [10.0, 64.0, 0.0])), &[]).is_none());
        }
        let Some(EvokerEvent::FangAttack { positions }) =
            evoker.tick(pos, Some((7, [10.0, 64.0, 0.0])), &[])
        else {
            panic!("expected a fang line");
        };
        assert_eq!(positions.len(), EVOKER_FANG_LINE_COUNT);
        let xs: Vec<i32> = positions.iter().map(|p| p[0]).collect();
        assert!(xs.windows(2).all(|w| w[1] >= w[0]), "fangs march toward +x");
        assert_eq!(positions[0][0], 1);
        // A victim inside 3 m triggers the circle defense instead.
        let mut evoker = EvokerBehavior::new();
        evoker.vex_cooldown = EVOKER_VEX_COOLDOWN;
        for _ in 0..EVOKER_SPELL_WARMUP {
            assert!(evoker.tick(pos, Some((7, [2.0, 64.0, 0.0])), &[]).is_none());
        }
        let Some(EvokerEvent::FangAttack { positions }) =
            evoker.tick(pos, Some((7, [2.0, 64.0, 0.0])), &[])
        else {
            panic!("expected a fang circle");
        };
        assert_eq!(positions.len(), 13); // 5 inner + 8 outer
        // Idle, a blue sheep inside 16 m gets the wololo; outside it does not.
        let mut evoker = EvokerBehavior::new();
        assert!(evoker.tick(pos, None, &[(9, [14.0, 64.0, 0.0])]).is_none());
        for _ in 0..EVOKER_SPELL_WARMUP - 1 {
            assert!(evoker.tick(pos, None, &[(9, [14.0, 64.0, 0.0])]).is_none());
        }
        assert_eq!(
            evoker.tick(pos, None, &[(9, [14.0, 64.0, 0.0])]),
            Some(EvokerEvent::Wololo { target: 9 })
        );
        let mut evoker = EvokerBehavior::new();
        for _ in 0..200 {
            assert!(evoker.tick(pos, None, &[(9, [20.0, 64.0, 0.0])]).is_none());
        }
    }

    #[test]
    fn p6a_parity_warden_anger_decay_melee_and_unreachable_sonic() {
        // Vibrations add 35 each against the 150 roar ceiling; 80 agitates.
        let mut anger = WardenAngerManagement::default();
        assert_eq!(anger.add_vibration(7), 35);
        assert!(!anger.agitated(7));
        assert_eq!(anger.add_sniff(7), 70);
        assert_eq!(anger.add_vibration(7), 105);
        assert!(anger.agitated(7));
        // Anger decays one point per second (per 20 ticks).
        for _ in 0..20 {
            anger.tick();
        }
        assert_eq!(anger.anger(7), 104);
        // Reachable targets eat the 30-damage melee.
        let mut plan = WardenCombatPlan::default();
        assert_eq!(
            plan.tick(Some(7), true),
            Some(WardenEvent::Melee {
                target: 7,
                damage: WARDEN_MELEE_DAMAGE
            })
        );
        assert_eq!(WARDEN_MELEE_DAMAGE, 30.0);
        // After 200 unreachable ticks the 60-tick sonic charge starts.
        for _ in 0..WARDEN_SONIC_TRIGGER_TICKS - 1 {
            assert!(plan.tick(Some(7), false).is_none());
        }
        assert_eq!(
            plan.tick(Some(7), false),
            Some(WardenEvent::ChargeSonicBoom { target: 7 })
        );
        for _ in 0..WARDEN_SONIC_CHARGE_TICKS - 1 {
            assert!(plan.tick(Some(7), false).is_none());
        }
        assert_eq!(
            plan.tick(Some(7), false),
            Some(WardenEvent::SonicBoom {
                target: 7,
                damage: 10.0,
                bypass_armor: true,
                bypass_shield: true,
            })
        );
        // A returned reach resets the unreachable counter.
        assert_eq!(
            plan.tick(Some(7), true),
            Some(WardenEvent::Melee {
                target: 7,
                damage: WARDEN_MELEE_DAMAGE
            })
        );
        for _ in 0..WARDEN_SONIC_TRIGGER_TICKS - 1 {
            assert!(plan.tick(Some(7), false).is_none());
        }
        assert_eq!(
            plan.tick(Some(7), false),
            Some(WardenEvent::ChargeSonicBoom { target: 7 })
        );
    }

    #[test]
    fn p6a_parity_guardian_80_tick_laser_and_elder_fatigue_aura() {
        // The beam charges for 80 ticks against a visible target in 16 m.
        let mut guardian = GuardianBehavior::new(false);
        for _ in 0..GUARDIAN_LASER_TICKS - 1 {
            assert!(guardian.tick(Some((7, 10.0))).is_empty());
        }
        assert_eq!(
            guardian.tick(Some((7, 10.0))),
            [GuardianEvent::Laser {
                target: 7,
                damage: GUARDIAN_LASER_DAMAGE
            }]
        );
        // Losing the beam target resets the charge.
        assert!(guardian.tick(None).is_empty());
        for _ in 0..GUARDIAN_LASER_TICKS - 1 {
            assert!(guardian.tick(Some((7, 10.0))).is_empty());
        }
        // Outside the 16 m attack range the laser never charges.
        let mut guardian = GuardianBehavior::new(false);
        for _ in 0..200 {
            assert!(guardian.tick(Some((7, 16.5))).is_empty());
        }
        // The elder guardian hits harder and paces the 50 m fatigue aura every
        // 1200 ticks.
        let mut elder = GuardianBehavior::new(true);
        for _ in 0..GUARDIAN_LASER_TICKS - 1 {
            elder.tick(Some((7, 10.0)));
        }
        assert_eq!(
            elder.tick(Some((7, 10.0))),
            [GuardianEvent::Laser {
                target: 7,
                damage: ELDER_GUARDIAN_LASER_DAMAGE
            }]
        );
        let mut elder = GuardianBehavior::new(true);
        for _ in 0..ELDER_FATIGUE_PERIOD - 1 {
            elder.tick(Some((7, 30.0)));
        }
        assert_eq!(
            elder.tick(Some((7, 30.0))),
            [GuardianEvent::MiningFatigue { target: 7 }]
        );
        let mut elder = GuardianBehavior::new(true);
        for _ in 0..ELDER_FATIGUE_PERIOD {
            assert!(elder.tick(Some((7, 55.0))).is_empty());
        }
        assert_eq!(ELDER_FATIGUE_AMPLIFIER, 2);
        assert_eq!(ELDER_FATIGUE_DURATION, 6000);
    }

    #[test]
    fn p6a_parity_ghast_100_block_sight_and_20_tick_volley() {
        assert!(ghast_acquires_target(100.0));
        assert!(!ghast_acquires_target(100.5));
        // 20 ticks of charge -> fireball, then a 60-tick cycle to the next.
        let mut ghast = GhastBehavior::new(4, 80.0);
        let delta = Some([10.0, 0.0, 0.0]);
        let mut warnings = 0;
        for _ in 0..10 {
            if ghast.tick(delta, true) == Some(GhastAttack::Warning) {
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1);
        for _ in 0..9 {
            assert!(ghast.tick(delta, true).is_none());
        }
        assert!(matches!(ghast.tick(delta, true), Some(GhastAttack::Fire(_))));
        for _ in 0..59 {
            assert!(!matches!(
                ghast.tick(delta, true),
                Some(GhastAttack::Fire(_))
            ));
        }
        assert!(matches!(ghast.tick(delta, true), Some(GhastAttack::Fire(_))));
    }

    // ---------- #220: projectile despawn ----------

    fn flying(family: ProjectileFamily) -> ProjectileTick {
        ProjectileTick {
            family,
            in_ground: false,
            returning_to_owner: false,
            y: 64.0,
            min_y: -64,
            chunk_loaded: true,
        }
    }

    #[test]
    fn p6a_parity_in_flight_projectile_never_despawns_artificially() {
        for family in [
            ProjectileFamily::Arrow,
            ProjectileFamily::SpectralArrow,
            ProjectileFamily::Trident,
            ProjectileFamily::Throwable,
            ProjectileFamily::Fireball,
        ] {
            let mut policy = ProjectileDespawnPolicy::new();
            for _ in 0..2200 {
                assert!(!policy.tick(flying(family)), "{family:?} must keep flying");
            }
            assert_eq!(policy.grounded_ticks(), 0);
        }
    }

    #[test]
    fn p6a_parity_grounded_arrow_1200_ticks_and_dislodge_reset() {
        let grounded = |family| ProjectileTick {
            in_ground: true,
            ..flying(family)
        };
        // Exactly 1200 grounded ticks discards arrows and spectral arrows.
        for family in [ProjectileFamily::Arrow, ProjectileFamily::SpectralArrow] {
            let mut policy = ProjectileDespawnPolicy::new();
            for _ in 0..ARROW_GROUND_LIFE_TICKS - 1 {
                assert!(!policy.tick(grounded(family)), "{family:?} early despawn");
            }
            assert!(policy.tick(grounded(family)), "{family:?} lives past 1200");
        }
        // A broken support resets the timer: 600 grounded + dislodged + 1200
        // fresh grounded ticks are needed before despawn.
        let mut policy = ProjectileDespawnPolicy::new();
        for _ in 0..600 {
            policy.tick(grounded(ProjectileFamily::Arrow));
        }
        assert_eq!(policy.grounded_ticks(), 600);
        assert!(!policy.tick(flying(ProjectileFamily::Arrow)));
        assert_eq!(policy.grounded_ticks(), 0, "a dislodged arrow restarts");
        for _ in 0..ARROW_GROUND_LIFE_TICKS - 1 {
            assert!(!policy.tick(grounded(ProjectileFamily::Arrow)));
        }
        assert!(policy.tick(grounded(ProjectileFamily::Arrow)));
        // Throwables and fireballs never take the ground-life rule at all.
        for family in [ProjectileFamily::Throwable, ProjectileFamily::Fireball] {
            let mut policy = ProjectileDespawnPolicy::new();
            for _ in 0..ARROW_GROUND_LIFE_TICKS + 10 {
                assert!(!policy.tick(grounded(family)));
            }
            assert_eq!(policy.grounded_ticks(), 0);
        }
        // Out-of-world and unloaded despawn regardless of the life timer.
        let mut policy = ProjectileDespawnPolicy::new();
        assert!(policy.tick(ProjectileTick {
            y: -129.0,
            ..flying(ProjectileFamily::Throwable)
        }));
        assert!(!policy.tick(ProjectileTick {
            y: -128.0,
            ..flying(ProjectileFamily::Throwable)
        }));
        assert!(policy.tick(ProjectileTick {
            chunk_loaded: false,
            ..flying(ProjectileFamily::Throwable)
        }));
    }

    #[test]
    fn p6a_parity_loyalty_trident_returns_instead_of_despawning() {
        // A returning Loyalty trident crosses the void and lives past any
        // ground timer — it despawns only once actually delivered.
        let mut policy = ProjectileDespawnPolicy::new();
        for _ in 0..3000 {
            assert!(!policy.tick(ProjectileTick {
                family: ProjectileFamily::Trident,
                returning_to_owner: true,
                y: -200.0,
                ..flying(ProjectileFamily::Trident)
            }));
        }
        // A stuck non-loyalty trident still follows the arrow ground rule.
        let mut policy = ProjectileDespawnPolicy::new();
        for _ in 0..ARROW_GROUND_LIFE_TICKS - 1 {
            assert!(!policy.tick(ProjectileTick {
                family: ProjectileFamily::Trident,
                in_ground: true,
                ..flying(ProjectileFamily::Trident)
            }));
        }
        assert!(policy.tick(ProjectileTick {
            family: ProjectileFamily::Trident,
            in_ground: true,
            ..flying(ProjectileFamily::Trident)
        }));
    }
}

#[cfg(test)]
mod vanilla_horse_tests {
    use super::*;

    fn player(seed: u8) -> [u8; 16] {
        let mut id = [0u8; 16];
        id[0] = seed;
        id[15] = seed;
        id
    }

    fn adult_tamed(kind: EquineKind) -> EquineState {
        let mut state = EquineState::new(kind);
        state.tamed = true;
        state.owner = Some(player(1));
        state
    }

    fn run_until_hatch(
        state: &mut IncubationState,
        env: &IncubationEnv,
        is_night: bool,
        max_ticks: u32,
    ) -> (u32, HatchResult) {
        for tick in 1..=max_ticks {
            let result = tick_incubation(state, env, is_night);
            if let HatchResult::Hatched { .. } = result {
                return (tick, result);
            }
        }
        (max_ticks, tick_incubation(state, env, is_night))
    }

    // ---- Taming & temper -------------------------------------------------

    #[test]
    fn vanilla_horse_tame_roll_bucks_until_temper() {
        let mut horse = EquineState::new(EquineKind::Horse);
        assert_eq!(horse.temper, 0);
        // Temper 0 can never roll a tame: every ride bucks and +5 temper.
        // Roll 99 always fails the `roll < temper` check at these tempers.
        for expected in [5, 10, 15] {
            assert_eq!(
                horse.try_equine_tame_tick(99),
                Some(EquineTameOutcome::Bucked {
                    new_temper: expected
                })
            );
            assert_eq!(horse.temper, expected);
            assert!(!horse.tamed);
        }
        // At temper 15 a roll below 15 tames; a roll equal to or above bucks.
        assert_eq!(
            horse.try_equine_tame_tick(14),
            Some(EquineTameOutcome::Tamed)
        );
        assert!(horse.tamed);
        // Once tamed the goal no longer runs.
        assert_eq!(horse.try_equine_tame_tick(0), None);
    }

    #[test]
    fn vanilla_horse_tame_roll_strict_less_than_temper() {
        let mut horse = EquineState::new(EquineKind::Horse);
        horse.temper = 50;
        // roll == temper is NOT a tame (vanilla `nextInt < temper` is strict).
        assert_eq!(
            horse.try_equine_tame_tick(50),
            Some(EquineTameOutcome::Bucked { new_temper: 55 })
        );
        assert!(!horse.tamed);
        // roll == temper - 1 tames.
        horse.temper = 50;
        assert_eq!(
            horse.try_equine_tame_tick(49),
            Some(EquineTameOutcome::Tamed)
        );
    }

    #[test]
    fn vanilla_horse_temper_clamps_at_max_and_llama_caps_at_30() {
        let mut horse = EquineState::new(EquineKind::Horse);
        horse.temper = 98;
        assert_eq!(
            horse.try_equine_tame_tick(200),
            Some(EquineTameOutcome::Bucked { new_temper: 100 })
        );
        // Another buck stays pinned at 100.
        assert_eq!(
            horse.try_equine_tame_tick(200),
            Some(EquineTameOutcome::Bucked { new_temper: 100 })
        );

        let mut llama = EquineState::new(EquineKind::Llama);
        llama.temper = 29;
        // Llama `getMaxTemper` is 30: +5 clamps to 30, not 100.
        assert_eq!(
            llama.try_equine_tame_tick(200),
            Some(EquineTameOutcome::Bucked { new_temper: 30 })
        );
        assert_eq!(llama.temper, 30);
        // And a llama draw is nextInt(30): roll 29 < 30 tames at full temper.
        assert_eq!(
            llama.try_equine_tame_tick(29),
            Some(EquineTameOutcome::Tamed)
        );
    }

    #[test]
    fn vanilla_horse_tame_with_name_sets_owner() {
        let mut horse = EquineState::new(EquineKind::Donkey);
        assert!(horse.tame_with_name(player(7)));
        assert!(horse.tamed);
        assert_eq!(horse.owner, Some(player(7)));
        // Already tamed: tameWithName still applies but reports no change.
        assert!(!horse.tame_with_name(player(9)));
        assert_eq!(horse.owner, Some(player(9)));
    }

    // ---- Feeding / temper gains -------------------------------------------

    fn feq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn vanilla_horse_feeding_untamed_gains_temper_and_heals() {
        let mut horse = EquineState::new(EquineKind::Horse);
        horse.health = 28.0;
        horse.max_health = 30.0;
        // Untamed full-table foods always gain temper even at full health.
        assert_eq!(
            horse.feed("wheat"),
            EquineFeedOutcome::Consumed {
                healed: 2.0,
                temper_gain: 3,
                fell_in_love: false,
            }
        );
        assert_eq!(horse.health, 30.0);
        assert_eq!(horse.temper, 3);
        // hay_block heals 20 but grants no temper.
        horse.health = 5.0;
        assert_eq!(
            horse.feed("hay_block"),
            EquineFeedOutcome::Consumed {
                healed: 20.0,
                temper_gain: 0,
                fell_in_love: false,
            }
        );
        assert_eq!(horse.health, 25.0);
        assert_eq!(horse.temper, 3);
    }

    #[test]
    fn vanilla_horse_feeding_tamed_adult_requires_use_for_temper() {
        let mut horse = adult_tamed(EquineKind::Horse);
        horse.health = 30.0;
        // Full-health tamed adult: wheat has nothing to do -> PASS, no temper.
        assert_eq!(horse.feed("wheat"), EquineFeedOutcome::Pass);
        assert_eq!(horse.temper, 0);
        // Hurt tamed adult: heal applies, temper follows because item was used.
        horse.health = 28.0;
        assert_eq!(
            horse.feed("sugar"),
            EquineFeedOutcome::Consumed {
                healed: 1.0,
                temper_gain: 3,
                fell_in_love: false,
            }
        );
        assert_eq!(horse.temper, 3);
        // Heal is capped at max_health: hay_block on 25/30 restores only 5.
        horse.health = 25.0;
        assert_eq!(
            horse.feed("hay_block"),
            EquineFeedOutcome::Consumed {
                healed: 5.0,
                temper_gain: 0,
                fell_in_love: false,
            }
        );
        assert_eq!(horse.health, 30.0);
    }

    #[test]
    fn vanilla_horse_golden_foods_trigger_love_on_tamed_adult() {
        let mut horse = adult_tamed(EquineKind::Horse);
        horse.health = 30.0;
        // golden_carrot: love food -> setInLove even at full health, +5 temper.
        assert_eq!(
            horse.feed("golden_carrot"),
            EquineFeedOutcome::Consumed {
                healed: 0.0,
                temper_gain: 5,
                fell_in_love: true,
            }
        );
        assert!(horse.in_love);
        assert_eq!(horse.temper, 5);
        // Already in love: no second love, but temper still applies (item used? no —
        // vanilla: used=false when love gate fails and health full -> PASS).
        let mut horse2 = adult_tamed(EquineKind::Horse);
        horse2.in_love = true;
        horse2.health = 30.0;
        assert_eq!(horse2.feed("golden_apple"), EquineFeedOutcome::Pass);
        assert_eq!(horse2.temper, 0);
        // enchanted_golden_apple shares the golden_apple row (heal 10, temper 10).
        let mut horse3 = adult_tamed(EquineKind::Horse);
        horse3.health = 20.0;
        assert_eq!(
            horse3.feed("enchanted_golden_apple"),
            EquineFeedOutcome::Consumed {
                healed: 10.0,
                temper_gain: 10,
                fell_in_love: true,
            }
        );
    }

    #[test]
    fn vanilla_horse_carrot_is_tag_food_but_does_nothing() {
        let mut horse = EquineState::new(EquineKind::Horse);
        assert!(EquineKind::Horse.is_food("carrot"));
        assert_eq!(horse.feed("carrot"), EquineFeedOutcome::Pass);
        assert_eq!(horse.temper, 0);
        // Non-food items pass for every kind.
        assert_eq!(horse.feed("diamond"), EquineFeedOutcome::Pass);
    }

    #[test]
    fn vanilla_horse_baby_age_up_and_llama_food_table() {
        let mut horse = EquineState::new(EquineKind::Horse);
        horse.age_ticks = -100;
        horse.health = 30.0;
        // Baby at full health: ageUp still consumes the item.
        assert_eq!(
            horse.feed("wheat"),
            EquineFeedOutcome::Consumed {
                healed: 0.0,
                temper_gain: 3,
                fell_in_love: false,
            }
        );
        assert_eq!(horse.age_ticks, -80);

        // Llama food table: wheat + hay only.
        let mut llama = EquineState::new(EquineKind::Llama);
        llama.health = 10.0;
        assert_eq!(
            llama.feed("wheat"),
            EquineFeedOutcome::Consumed {
                healed: 2.0,
                temper_gain: 3,
                fell_in_love: false,
            }
        );
        // Apple is HORSE_FOOD but not LLAMA_FOOD.
        assert_eq!(llama.feed("apple"), EquineFeedOutcome::Pass);
        // Llama hay_block: heal 10, temper +6, love food.
        llama.tamed = true;
        llama.owner = Some(player(3));
        llama.health = 10.0;
        assert_eq!(
            llama.feed("hay_block"),
            EquineFeedOutcome::Consumed {
                healed: 10.0,
                temper_gain: 6,
                fell_in_love: true,
            }
        );
        assert!(llama.in_love);
        assert_eq!(llama.temper, 9);
    }

    // ---- Saddle / chest / armor --------------------------------------------

    #[test]
    fn vanilla_horse_saddle_requires_tamed_adult() {
        let mut horse = EquineState::new(EquineKind::Horse);
        assert!(!horse.equip_saddle());
        horse.tame_with_name(player(1));
        horse.age_ticks = -50; // baby cannot wear a saddle
        assert!(!horse.equip_saddle());
        horse.age_ticks = 0;
        assert!(horse.equip_saddle());
        assert!(horse.saddled);
        assert!(horse.can_jump());
        // No double-saddling.
        assert!(!horse.equip_saddle());
    }

    #[test]
    fn vanilla_horse_chest_rules_per_kind() {
        // Horse never chests.
        let mut horse = adult_tamed(EquineKind::Horse);
        assert!(!horse.equip_chest());
        // Donkey/Mule/Llama chest when tamed adult.
        for kind in [EquineKind::Donkey, EquineKind::Mule, EquineKind::Llama] {
            let mut state = adult_tamed(kind);
            assert!(state.equip_chest(), "{kind:?} should accept a chest");
            assert!(state.chested);
            assert!(!state.equip_chest(), "{kind:?} double-chest refused");
        }
        // Untamed mule refuses.
        let mut mule = EquineState::new(EquineKind::Mule);
        assert!(!mule.equip_chest());
    }

    #[test]
    fn vanilla_horse_armor_tiers_horse_only_no_swap() {
        let mut horse = adult_tamed(EquineKind::Horse);
        assert!(horse.equip_armor(1)); // leather
        assert_eq!(horse.armor_tier, 1);
        // Vanilla refuses to swap a worn piece.
        assert!(!horse.equip_armor(4));
        assert_eq!(horse.armor_tier, 1);

        let mut horse2 = adult_tamed(EquineKind::Horse);
        assert!(horse2.equip_armor(4)); // diamond
        assert_eq!(horse2.armor_tier, 4);
        // Tier 0 and out-of-range tiers rejected.
        assert!(!horse2.equip_armor(0));
        assert!(!horse2.equip_armor(5));

        // Donkeys/mules/llamas cannot wear horse armor.
        for kind in [EquineKind::Donkey, EquineKind::Mule, EquineKind::Llama] {
            let mut state = adult_tamed(kind);
            assert!(!state.equip_armor(1), "{kind:?} cannot wear armor");
        }
        // Untamed horse refuses.
        let mut wild = EquineState::new(EquineKind::Horse);
        assert!(!wild.equip_armor(2));
    }

    // ---- Jump bar ------------------------------------------------------------

    #[test]
    fn vanilla_horse_jump_scale_exact_boundaries() {
        // AbstractHorse.onPlayerJump: charge<0 -> 0; >=90 -> 1.0; else
        // 0.4 + 0.4*charge/90. Exact values at the required charges.
        assert_eq!(equine_jump_scale(0), 0.4);
        assert!(feq(equine_jump_scale(45), 0.6));
        assert!(feq(equine_jump_scale(45), 0.4 + 0.4 * 45.0 / 90.0));
        assert_eq!(equine_jump_scale(90), 1.0);
        assert_eq!(equine_jump_scale(100), 1.0);
        assert_eq!(equine_jump_scale(-30), 0.4);
        assert_eq!(equine_jump_scale(89), 0.4 + 0.4 * 89.0 / 90.0);
        assert_eq!(equine_jump_scale(1), 0.4 + 0.4 / 90.0);
    }

    #[test]
    fn vanilla_horse_jump_velocity_applies_strength() {
        // getJumpPower on normal ground: jump_strength * scale.
        assert!(feq(equine_jump_velocity(0.7, 0), 0.28));
        assert!(feq(equine_jump_velocity(0.7, 45), 0.42));
        assert_eq!(equine_jump_velocity(0.7, 90), 0.7);
        assert_eq!(equine_jump_velocity(0.7, 100), 0.7);
        assert!(feq(equine_jump_velocity(1.0, 45), 0.6));
        // Full charge is exactly jump_strength (scale 1.0 is exact).
        assert_eq!(equine_jump_velocity(0.9, 90), 0.9);
        // Spawn-roll attribute ranges stay inside vanilla bounds.
        let fast = EquineState::spawned(
            EquineKind::Horse,
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0],
        );
        assert!(feq(fast.jump_strength, 1.0));
        assert!(feq(fast.move_speed, 0.3375));
        let slow = EquineState::spawned(
            EquineKind::Horse,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            [0.0, 0.0],
        );
        assert!(feq(slow.jump_strength, 0.4));
        assert!(feq(slow.move_speed, 0.1125));
    }

    // ---- Breeding ------------------------------------------------------------

    #[test]
    fn vanilla_horse_breeding_averages_parents_plus_third_draw() {
        // createOffspringAttribute: clamp, average, spread*(draw-0.5), reflect.
        // parents 0.5/0.7 in [0.4,1.0], rand_unit=0.5 -> draw 0 -> pure average.
        assert!(feq(
            equine_offspring_attribute(0.5, 0.7, 0.4, 1.0, 0.5),
            0.6
        ));
        // rand_unit=1.0 -> draw +0.5 -> avg + spread*0.5.
        // spread = |0.5-0.7| + 0.15*0.6*2 = 0.2 + 0.18 = 0.38 -> 0.6+0.19=0.79.
        assert!(feq(
            equine_offspring_attribute(0.5, 0.7, 0.4, 1.0, 1.0),
            0.79
        ));
        // rand_unit=0.0 -> draw -0.5 -> 0.6-0.19=0.41.
        assert!(feq(
            equine_offspring_attribute(0.5, 0.7, 0.4, 1.0, 0.0),
            0.41
        ));
        // Identical parents still get the range pad spread.
        // avg 0.7, spread = 0 + 0.18 = 0.18 -> rand 1.0 -> 0.7+0.09=0.79.
        assert!(feq(
            equine_offspring_attribute(0.7, 0.7, 0.4, 1.0, 1.0),
            0.79
        ));
        // Out-of-range parents clamp first; result reflects back inside.
        // a=2.0 clamps to 1.0, b=0.0 clamps to 0.4, avg=0.7,
        // spread=|1.0-0.4|+0.18=0.78, rand=0 -> 0.7-0.39=0.31 < 0.4 ->
        // reflect: 0.4 + (0.4-0.31) = 0.49.
        assert!(feq(
            equine_offspring_attribute(2.0, 0.0, 0.4, 1.0, 0.0),
            0.49
        ));
    }

    #[test]
    fn vanilla_horse_breed_equine_stats_stays_in_ranges() {
        let a = EquineState::spawned(
            EquineKind::Horse,
            [0.0, 1.0, 0.5],
            [0.5, 0.5, 0.5],
            [0.5, 0.5],
        );
        let b = EquineState::spawned(
            EquineKind::Horse,
            [1.0, 0.0, 0.5],
            [0.0, 1.0, 0.5],
            [0.0, 1.0],
        );
        for draw in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let child = breed_equine_stats(&a, &b, draw);
            assert!(
                (0.4..=1.0).contains(&child.jump_strength),
                "jump {child:?}"
            );
            assert!(
                (0.1125..=0.3375).contains(&child.move_speed),
                "speed {child:?}"
            );
            assert!((15.0..=30.0).contains(&child.max_health), "hp {child:?}");
        }
        // Midpoint draw averages the parents' means.
        let child = breed_equine_stats(&a, &b, 0.5);
        // a.jump = 0.4+0.2*1.5=0.7, b.jump = 0.4+0.2*1.5=0.7 -> child 0.7.
        assert!(feq(child.jump_strength, 0.7));
    }

    // ---- Parrot --------------------------------------------------------------

    #[test]
    fn vanilla_parrot_cookie_is_fatal_poison() {
        let mut wild = ParrotState::new(0);
        assert_eq!(wild.variant, 0);
        // Cookie on an untamed parrot: still the poisonous-food branch.
        assert_eq!(wild.mob_interact(player(1), "cookie", 0), ParrotInteract::Poisoned);
        assert!(!wild.tamed);
        // Cookie on a tamed parrot: same fatal outcome.
        let mut tame = ParrotState::new(0);
        tame.tamed = true;
        tame.owner = Some(player(2));
        assert_eq!(tame.mob_interact(player(2), "cookie", 0), ParrotInteract::Poisoned);
        // Poison applies before any tame/ownership logic — item names map through
        // mob_behavior_id, so a namespaced id works too.
        let mut tame2 = ParrotState::new(0);
        tame2.tamed = true;
        tame2.owner = Some(player(2));
        assert_eq!(
            tame2.mob_interact(player(2), "minecraft:cookie", 0),
            ParrotInteract::Poisoned
        );
    }

    #[test]
    fn vanilla_parrot_seed_taming_roll_and_sit_toggle() {
        let mut parrot = ParrotState::new(0);
        // nextInt(10)==0 tames on the spot.
        assert_eq!(
            parrot.mob_interact(player(4), "wheat_seeds", 0),
            ParrotInteract::Ate { tamed_now: true }
        );
        assert!(parrot.tamed);
        assert_eq!(parrot.owner, Some(player(4)));

        // A non-zero roll eats but stays wild.
        let mut wild = ParrotState::new(1);
        assert_eq!(
            wild.mob_interact(player(4), "melon_seeds", 7),
            ParrotInteract::Ate { tamed_now: false }
        );
        assert!(!wild.tamed);
        assert_eq!(wild.owner, None);

        // Tamed + owned + grounded: any non-poisonous item toggles sit.
        let mut tame = ParrotState::new(2);
        tame.tamed = true;
        tame.owner = Some(player(4));
        assert_eq!(
            tame.mob_interact(player(4), "wheat_seeds", 0),
            ParrotInteract::Sat { sitting: true }
        );
        assert!(tame.sitting_ordered);
        assert_eq!(
            tame.mob_interact(player(4), "stick", 0),
            ParrotInteract::Sat { sitting: false }
        );
        assert!(!tame.sitting_ordered);
        // A different player cannot toggle the sit order.
        assert_eq!(
            tame.mob_interact(player(9), "stick", 0),
            ParrotInteract::Pass
        );
        // Flying parrots ignore the sit toggle.
        let mut flyer = ParrotState::new(2);
        flyer.tamed = true;
        flyer.owner = Some(player(4));
        flyer.flying = true;
        assert_eq!(
            flyer.mob_interact(player(4), "stick", 0),
            ParrotInteract::Pass
        );
        assert!(!flyer.sitting_ordered);
    }

    #[test]
    fn vanilla_parrot_shoulder_perch_gates() {
        let mut parrot = ParrotState::new(0);
        parrot.tamed = true;
        parrot.owner = Some(player(5));
        for _ in 0..PERCH_COOLDOWN_TICKS + 1 {
            parrot.tick();
        }
        let ready = ShoulderPerchContext {
            player: player(5),
            bounding_boxes_intersect: true,
            player_on_ground: true,
            player_mounted: false,
            player_in_water: false,
            player_in_powder_snow: false,
            shoulder_slot_free: true,
        };
        assert!(parrot.try_perch_on_shoulder(&ready));
        assert!(parrot.on_shoulder);
        // Already perched parrots do not re-perch.
        assert!(!parrot.try_perch_on_shoulder(&ready));
    }

    #[test]
    fn vanilla_parrot_shoulder_perch_rejects_each_gate() {
        let base = ShoulderPerchContext {
            player: player(5),
            bounding_boxes_intersect: true,
            player_on_ground: true,
            player_mounted: false,
            player_in_water: false,
            player_in_powder_snow: false,
            shoulder_slot_free: true,
        };
        let fresh = |parrot: &mut ParrotState| {
            parrot.tamed = true;
            parrot.owner = Some(player(5));
            for _ in 0..PERCH_COOLDOWN_TICKS + 1 {
                parrot.tick();
            }
        };
        // Wrong owner.
        let mut p = ParrotState::new(0);
        fresh(&mut p);
        let mut ctx = base;
        ctx.player = player(6);
        assert!(!p.try_perch_on_shoulder(&ctx));
        // Not intersecting.
        let mut p = ParrotState::new(0);
        fresh(&mut p);
        let mut ctx = base;
        ctx.bounding_boxes_intersect = false;
        assert!(!p.try_perch_on_shoulder(&ctx));
        // Cooldown not elapsed.
        let mut p = ParrotState::new(0);
        p.tamed = true;
        p.owner = Some(player(5));
        for _ in 0..PERCH_COOLDOWN_TICKS {
            p.tick();
        }
        assert!(!p.try_perch_on_shoulder(&base));
        // Sit-ordered parrot refuses to perch.
        let mut p = ParrotState::new(0);
        fresh(&mut p);
        p.sitting_ordered = true;
        assert!(!p.try_perch_on_shoulder(&base));
        // Flying / leashed refuse.
        let mut p = ParrotState::new(0);
        fresh(&mut p);
        p.flying = true;
        assert!(!p.try_perch_on_shoulder(&base));
        let mut p = ParrotState::new(0);
        fresh(&mut p);
        p.leashed = true;
        assert!(!p.try_perch_on_shoulder(&base));
        // Player-side gates: airborne, mounted, wet, powder snow, slot taken.
        let gates: [fn(&mut ShoulderPerchContext); 5] = [
            |c| c.player_on_ground = false,
            |c| c.player_mounted = true,
            |c| c.player_in_water = true,
            |c| c.player_in_powder_snow = true,
            |c| c.shoulder_slot_free = false,
        ];
        for mutate in gates {
            let mut p = ParrotState::new(0);
            fresh(&mut p);
            let mut ctx = base;
            mutate(&mut ctx);
            assert!(!p.try_perch_on_shoulder(&ctx));
        }
    }

    #[test]
    fn vanilla_parrot_shoulder_dismount_on_fall_water_damage() {
        let mut parrot = ParrotState::new(0);
        parrot.tamed = true;
        parrot.owner = Some(player(5));
        parrot.on_shoulder = true;
        let safe = ShoulderDropConditions::default();
        // No hazard -> stays.
        assert!(!parrot.shoulder_dismount(&safe, 100));
        // Vanilla 20-tick minimum: even a real hazard cannot drop it sooner.
        assert!(!parrot.shoulder_dismount(
            &ShoulderDropConditions {
                fall_distance: 3.0,
                ..safe
            },
            20
        ));
        // Fall distance > 0.5 after the minimum -> dismount + cooldown reset.
        assert!(parrot.shoulder_dismount(
            &ShoulderDropConditions {
                fall_distance: 0.6,
                ..safe
            },
            21
        ));
        assert!(!parrot.on_shoulder);
        assert_eq!(parrot.shoulder_ticks, 0);

        // Water entry dismounts too.
        parrot.on_shoulder = true;
        assert!(parrot.shoulder_dismount(
            &ShoulderDropConditions {
                in_water: true,
                ..safe
            },
            30
        ));
        // Player damage dismounts.
        parrot.on_shoulder = true;
        assert!(parrot.shoulder_dismount(
            &ShoulderDropConditions {
                player_damaged: true,
                ..safe
            },
            30
        ));
        // Elytra flight, sleep, powder snow, riptide all dismount.
        for hazard in [
            ShoulderDropConditions {
                player_flying: true,
                ..safe
            },
            ShoulderDropConditions {
                player_sleeping: true,
                ..safe
            },
            ShoulderDropConditions {
                in_powder_snow: true,
                ..safe
            },
            ShoulderDropConditions {
                spin_attack: true,
                ..safe
            },
        ] {
            parrot.on_shoulder = true;
            assert!(parrot.shoulder_dismount(&hazard, 30));
            parrot.on_shoulder = true;
        }
    }

    // ---- EnvIncubator ---------------------------------------------------------

    #[test]
    fn vanilla_horse_turtle_egg_sand_night_accelerates() {
        let mut day = IncubationState::new(EggSpec::turtle_egg(), None);
        let (day_ticks, day_result) =
            run_until_hatch(&mut day, &IncubationEnv::SandNight, false, 60_000);
        assert_eq!(
            day_result,
            HatchResult::Hatched {
                entity_id: "minecraft:turtle".into(),
                owner: None,
            }
        );
        assert_eq!(day.stage(), EGG_CRACK_STAGES);
        // Deterministic totals: 3 x 9600 day ticks vs 3 x 1200 night ticks.
        assert_eq!(day_ticks, 28_800);

        let mut night = IncubationState::new(EggSpec::turtle_egg(), None);
        let (night_ticks, night_result) =
            run_until_hatch(&mut night, &IncubationEnv::SandNight, true, 60_000);
        assert!(matches!(night_result, HatchResult::Hatched { .. }));
        assert_eq!(night_ticks, 3_600);
        // Night credit 8x -> strictly fewer ticks than the day rate.
        assert!(night_ticks < day_ticks);

        // Wrong substrate never progresses.
        let mut stuck = IncubationState::new(EggSpec::turtle_egg(), None);
        for _ in 0..10_000 {
            assert_eq!(
                tick_incubation(&mut stuck, &IncubationEnv::OnFire, true),
                HatchResult::Waiting
            );
        }
        assert_eq!(stuck.stage(), 0);
    }

    #[test]
    fn vanilla_horse_sniffer_egg_moss_block_2x_speedup() {
        let mut plain = IncubationState::new(EggSpec::sniffer_egg(), None);
        let (plain_ticks, plain_result) =
            run_until_hatch(&mut plain, &IncubationEnv::Custom("dirt".into()), false, 30_000);
        assert_eq!(
            plain_result,
            HatchResult::Hatched {
                entity_id: "minecraft:sniffer".into(),
                owner: None,
            }
        );
        // Vanilla: 24000 ticks on ordinary blocks.
        assert_eq!(plain_ticks, 24_000);

        let mut moss = IncubationState::new(EggSpec::sniffer_egg(), None);
        let (moss_ticks, moss_result) =
            run_until_hatch(&mut moss, &IncubationEnv::MossBlock, false, 30_000);
        assert!(matches!(moss_result, HatchResult::Hatched { .. }));
        // Vanilla: 12000 ticks on moss — exactly half.
        assert_eq!(moss_ticks, 12_000);
        assert_eq!(plain_ticks, moss_ticks * 2);

        // Cracks land at the right stage boundaries.
        let mut fresh = IncubationState::new(EggSpec::sniffer_egg(), None);
        let mut saw = Vec::new();
        for _ in 0..24_000 {
            match tick_incubation(&mut fresh, &IncubationEnv::Custom("stone".into()), false) {
                HatchResult::Cracked { stage } => saw.push(stage),
                HatchResult::Hatched { .. } => break,
                HatchResult::Waiting => {}
            }
        }
        assert_eq!(saw, vec![1, 2]);
    }

    #[test]
    fn vanilla_horse_generic_fire_water_thunder_eggs() {
        // A modded fire egg: no mod name hardcoded, just the env contract.
        let fire_spec = EggSpec {
            id: "mymod:ember_egg".into(),
            required_env: Some(IncubationEnv::OnFire),
            ticks_per_stage: 100,
            stages: 3,
            spawns_entity: "mymod:ember_imp".into(),
            inherit_owner: true,
        };
        let mut fire = IncubationState::new(fire_spec, Some(player(11)));
        assert_eq!(fire.spec.id, "mymod:ember_egg");
        let (ticks, result) = run_until_hatch(&mut fire, &IncubationEnv::OnFire, false, 1_000);
        assert_eq!(ticks, 300);
        assert_eq!(
            result,
            HatchResult::Hatched {
                entity_id: "mymod:ember_imp".into(),
                owner: Some(player(11)),
            }
        );
        // Owner inheritance off -> owner dropped even when planter known.
        let no_inherit = EggSpec {
            id: "mymod:plain_egg".into(),
            required_env: Some(IncubationEnv::OnFire),
            ticks_per_stage: 1,
            stages: 1,
            spawns_entity: "mymod:plain".into(),
            inherit_owner: false,
        };
        let mut plain = IncubationState::new(no_inherit, Some(player(11)));
        assert_eq!(
            tick_incubation(&mut plain, &IncubationEnv::OnFire, false),
            HatchResult::Hatched {
                entity_id: "mymod:plain".into(),
                owner: None,
            }
        );
        // A fire egg in water does not progress.
        let mut blocked = IncubationState::new(
            EggSpec {
                id: "mymod:ember_egg2".into(),
                required_env: Some(IncubationEnv::OnFire),
                ticks_per_stage: 100,
                stages: 3,
                spawns_entity: "mymod:ember_imp".into(),
                inherit_owner: false,
            },
            None,
        );
        for _ in 0..500 {
            assert_eq!(
                tick_incubation(&mut blocked, &IncubationEnv::SubmergedWater, false),
                HatchResult::Waiting
            );
        }
        assert_eq!(blocked.stage(), 0);

        // Thunderstorm egg.
        let mut storm = IncubationState::new(
            EggSpec {
                id: "mymod:storm_egg".into(),
                required_env: Some(IncubationEnv::Thunderstorm),
                ticks_per_stage: 50,
                stages: 2,
                spawns_entity: "mymod:storm_hawk".into(),
                inherit_owner: false,
            },
            None,
        );
        let (storm_ticks, storm_result) =
            run_until_hatch(&mut storm, &IncubationEnv::Thunderstorm, false, 500);
        assert_eq!(storm_ticks, 100);
        assert!(matches!(storm_result, HatchResult::Hatched { .. }));
        // Same egg in a Custom env does not satisfy the requirement.
        let mut storm2 = IncubationState::new(
            EggSpec {
                id: "mymod:storm_egg".into(),
                required_env: Some(IncubationEnv::Thunderstorm),
                ticks_per_stage: 50,
                stages: 2,
                spawns_entity: "mymod:storm_hawk".into(),
                inherit_owner: false,
            },
            None,
        );
        for _ in 0..200 {
            assert_eq!(
                tick_incubation(&mut storm2, &IncubationEnv::Custom("mymod:storm".into()), false),
                HatchResult::Waiting
            );
        }

        // Custom env egg with owner inheritance.
        let mut custom = IncubationState::new(
            EggSpec {
                id: "mymod:rime_egg".into(),
                required_env: Some(IncubationEnv::Custom("mymod:frost".into())),
                ticks_per_stage: 1,
                stages: 1,
                spawns_entity: "mymod:rime_wisp".into(),
                inherit_owner: true,
            },
            Some(player(12)),
        );
        assert_eq!(
            tick_incubation(&mut custom, &IncubationEnv::Custom("mymod:frost".into()), false),
            HatchResult::Hatched {
                entity_id: "mymod:rime_wisp".into(),
                owner: Some(player(12)),
            }
        );
        // Post-hatch ticks report Waiting forever.
        assert_eq!(
            tick_incubation(&mut custom, &IncubationEnv::Custom("mymod:frost".into()), false),
            HatchResult::Waiting
        );
    }
}
