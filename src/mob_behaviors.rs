// R7 GP-6 specialized behaviors. The caller owns entity state, supplies one game
// tick at a time, and applies the returned combat, movement, and world actions.
// Random draws accept the world's next_below stream so tests can script rolls.

fn mob_behavior_id(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

fn mob_behavior_item_matches(item: &str, choices: &[&str]) -> bool {
    choices.contains(&mob_behavior_id(item))
}

const WOLF_TAMING_ITEMS: &[&str] = &["bone"];
const CAT_TAMING_ITEMS: &[&str] = &["cod", "salmon"];
#[cfg(test)]
const HORSE_FOOD_ITEMS: &[&str] = &[
    "wheat", "sugar", "hay_block", "apple", "carrot", "golden_carrot", "golden_apple",
    "enchanted_golden_apple",
];
#[cfg(test)]
const LLAMA_FOOD_ITEMS: &[&str] = &["wheat", "hay_block"];
const PARROT_FOOD_ITEMS: &[&str] = &[
    "wheat_seeds", "melon_seeds", "pumpkin_seeds", "beetroot_seeds", "torchflower_seeds",
    "pitcher_pod",
];

fn mob_behavior_delta(from: [f64; 3], to: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| to[axis] - from[axis])
}

fn mob_behavior_dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.into_iter().zip(b).map(|(a, b)| a * b).sum()
}

fn mob_behavior_unit(vector: [f64; 3]) -> [f64; 3] {
    let length = mob_behavior_dot(vector, vector).sqrt();
    if length > 0.0 {
        vector.map(|component| component / length)
    } else {
        [0.0; 3]
    }
}

fn mob_behavior_random_unit(draw: &mut impl FnMut(u32) -> u32) -> f64 {
    const STEPS: u32 = 1 << 24;
    f64::from(draw(STEPS)) / f64::from(STEPS)
}

#[derive(Debug, PartialEq)]
enum BlazeAttack {
    Melee { target: i32, damage: f32 },
    SmallFireball { target: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlazePhase {
    Idle(u8),
    Charging(u8),
    Burst { ticks: u8, shots: u8 },
    Cooldown(u8),
}

struct BlazeBehavior {
    phase: BlazePhase,
    melee_cooldown: u8,
}

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

#[derive(Clone, Copy)]
enum BlazeHazard {
    Water,
    #[cfg(test)]
    Snowball,
}

fn blaze_hazard_damage(hazard: BlazeHazard) -> f32 {
    match hazard {
        BlazeHazard::Water => 1.0,
        #[cfg(test)]
        BlazeHazard::Snowball => 3.0,
    }
}

#[derive(Debug, PartialEq)]
struct GhastFireball {
    fired_by: i32,
    owner: i32,
    velocity: [f64; 3],
    explosion_power: u8,
}

#[cfg(test)]
impl GhastFireball {
    fn deflect(&mut self, attacker: i32) {
        self.velocity = self.velocity.map(|component| -component);
        self.owner = attacker;
    }

    fn returns_to_sender(&self, hit: i32) -> bool {
        hit == self.fired_by && self.owner != self.fired_by
    }
}

#[derive(Debug, PartialEq)]
enum GhastAttack {
    Warning,
    Fire(GhastFireball),
}

struct GhastBehavior {
    entity: i32,
    altitude: f64,
    attack_tick: i8,
}

impl GhastBehavior {
    fn new(entity: i32, altitude: f64) -> Self {
        Self {
            entity,
            altitude,
            attack_tick: 0,
        }
    }

    #[cfg(test)]
    fn hover_velocity(&self, y: f64, vertical_velocity: f64) -> f64 {
        vertical_velocity * 0.8 + ((self.altitude - y) * 0.05).clamp(-0.1, 0.1)
    }

    #[cfg(test)]
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

/// `DimensionType.piglinSafe`: vanilla marks only the Nether; datapack
/// dimensions declare their own flag instead of hardcoding dimension identity.
fn dimension_piglin_safe(dim: DimId) -> bool {
    worldgen::dimension_spec(dim).map_or(dim == DimId::Nether, |spec| spec.piglin_safe)
}

#[derive(Default)]
struct NetherConversion {
    unsafe_ticks: u16,
}

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

/// World interactions that can provoke a piglin (`PiglinAi` anger sources).
#[derive(Clone, Copy, Debug)]
pub enum PiglinProvocation<'a> {
    Opened(&'a str),
    Mined(&'a str),
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PiglinBarterMetadata {
    Plain,
    SoulSpeed,
    Potion(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PiglinBarterLoot {
    item: &'static str,
    count: u8,
    potion: Option<&'static str>,
    soul_speed: Option<u8>,
}

// Minecraft 1.21.1's gameplay/piglin_bartering, in loot-pool order.
// Each row is (item, weight, inclusive minimum and maximum count, metadata).
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

#[derive(Debug, PartialEq, Eq)]
enum PiglinEvent {
    Barter { player: i32, loot: PiglinBarterLoot },
    ConvertToZombifiedPiglin,
}

#[derive(Default)]
pub struct PiglinBehavior {
    anger: Option<(i32, u16)>,
    /// `universalAnger`: the last provocation angers at every nearby player
    /// and holds while the rule stays on, instead of ticking down at one
    /// target.
    universal: bool,
    barter: Option<(i32, u8)>,
    conversion: NetherConversion,
}

impl PiglinBehavior {
    /// Whether the piglin treats `player` as hostile. Golden armor on any
    /// piece pacifies it; under `universalAnger` a provoked piglin stays
    /// hostile to every nearby player instead of only the provoker.
    pub fn hostile_to(&self, player: i32, armor: &[&str]) -> bool {
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

    /// Ticks left on the current anger; `0` when the piglin is calm.
    pub fn anger_ticks(&self) -> u16 {
        self.anger.map_or(0, |(_, ticks)| ticks)
    }

    /// `PiglinAi` provocation: opening a container or mining a gold block
    /// within 16 blocks angers the piglin for 600 ticks
    /// (`PiglinAi.java` — `setAngerTarget` / `updateAnger`).
    pub fn provoke(&mut self, player: i32, distance: f64, event: PiglinProvocation<'_>) -> bool {
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
        // `PiglinAi.ADMIRE_GOLD_ITEM_DURATION` is 119 ticks.
        self.barter = Some((player, 119));
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

#[derive(Default)]
struct HoglinBehavior {
    conversion: NetherConversion,
}

#[derive(Debug, PartialEq, Eq)]
enum HoglinEvent {
    ConvertToZoglin,
}

impl HoglinBehavior {
    fn tick(&mut self, dimension_piglin_safe: bool) -> Option<HoglinEvent> {
        self.conversion
            .tick(dimension_piglin_safe)
            .then_some(HoglinEvent::ConvertToZoglin)
    }

    #[cfg(test)]
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

    #[cfg(test)]
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
    #[cfg(test)]
    fn knockback_resistance(&self) -> f64 {
        0.6
    }
}

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

#[derive(Clone, Copy, Default)]
struct EndermanTeleportBlock {
    blocks_motion: bool,
    liquid: bool,
}

#[derive(Clone, Copy)]
enum EndermanHazard {
    Water,
    Rain,
    #[cfg(test)]
    Projectile,
}

#[derive(Debug, PartialEq)]
struct EndermanEscape {
    destination: Option<[f64; 3]>,
    reject_damage: bool,
}

fn enderman_escape(
    hazard: EndermanHazard,
    position: [f64; 3],
    world_y: std::ops::Range<i32>,
    mut block_at: impl FnMut([i32; 3]) -> EndermanTeleportBlock,
    mut can_occupy: impl FnMut([f64; 3]) -> bool,
    draw: &mut impl FnMut(u32) -> u32,
) -> EndermanEscape {
    let reject_damage = match hazard {
        #[cfg(test)]
        EndermanHazard::Projectile => true,
        _ => false,
    };
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

#[derive(Debug, PartialEq)]
struct ShulkerBullet {
    owner: i32,
    target: i32,
    position: [f64; 3],
    velocity: [f64; 3],
}

#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
struct ShulkerBulletHit {
    victim: i32,
    levitation_ticks: u16,
}

#[cfg(test)]
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

struct ShulkerBehavior {
    entity: i32,
    open_ticks: u8,
    shot_ticks: u8,
}

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

    #[cfg(test)]
    fn armor_bonus(&self) -> u8 {
        if self.peek() == 0 {
            20
        } else {
            0
        }
    }

    #[cfg(test)]
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

fn end_crystal_spawn_position(pillar: &feature::EndSpike) -> [f64; 3] {
    [
        f64::from(pillar.center_x) + 0.5,
        f64::from(pillar.height) + 1.0,
        f64::from(pillar.center_z) + 0.5,
    ]
}

#[derive(Clone, Copy)]
struct DragonCrystal {
    entity: i32,
    position: [f64; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragonPhase {
    HoldingPattern,
    StrafePlayer,
    LandingApproach,
    Landing,
    SittingFlaming,
    SittingScanning,
    SittingAttacking,
    ChargingPlayer,
    Takeoff,
    Dying,
    Dead,
}

#[derive(Clone, Copy)]
pub enum BossHit {
    Arrow,
    Trident,
    /// `WindCharge` projectile (`WitherBoss.java:463-467` grants the same
    /// 50%-health armor immunity to `AbstractArrow || WindCharge`).
    WindCharge,
    Other,
}

struct DragonTick<'a> {
    position: [f64; 3],
    portal_y: i32,
    path_complete: bool,
    strafe_target: Option<i32>,
    strafe_aligned: bool,
    target_pos: Option<[f64; 3]>,
    crystals: &'a [DragonCrystal],
}

#[derive(Debug, PartialEq, Eq)]
enum DragonEvent {
    StrafeFireball { target: i32 },
    FlameBreath,
    PerchAttack,
    CompleteFight(DragonFightRewards),
}

#[derive(Debug, PartialEq, Eq)]
struct DragonFightRewards {
    exit_portal_base: [i32; 3],
    dragon_egg: Option<[i32; 3]>,
    xp: u32,
}

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

struct EnderDragonBehavior {
    hp: f32,
    phase: DragonPhase,
    phase_ticks: u16,
    fight_ticks: u64,
    strafe_charge: u8,
    strafe_target: Option<i32>,
    linked_crystal: Option<i32>,
    flame_count: u8,
    charge_target: Option<[f64; 3]>,
    charge_recovery: u8,
    death_landed: bool,
    previously_killed: bool,
}

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
            flame_count: 0,
            charge_target: None,
            charge_recovery: 0,
            death_landed: false,
            previously_killed,
        }
    }

    fn set_phase(&mut self, phase: DragonPhase) {
        self.phase = phase;
        self.phase_ticks = 0;
        self.charge_target = None;
        self.charge_recovery = 0;
        if phase == DragonPhase::SittingFlaming {
            self.flame_count = self.flame_count.saturating_add(1);
        }
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
        if self.hp == 0.0 || damage <= 0.0 || (matches!(hit, BossHit::Arrow | BossHit::Trident) && self.is_sitting()) {
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
                self.flame_count = 0;
                self.set_phase(DragonPhase::SittingFlaming);
            }
            DragonPhase::SittingFlaming if self.phase_ticks == 10 => {
                return Some(DragonEvent::FlameBreath);
            }
            DragonPhase::SittingFlaming if self.phase_ticks == 200 => {
                let takeoff = self.flame_count >= 4;
                if takeoff {
                    self.flame_count = 0;
                }
                self.set_phase(if takeoff {
                    DragonPhase::Takeoff
                } else {
                    DragonPhase::SittingScanning
                });
            }
            DragonPhase::SittingScanning => {
                let distance_sq = input.target_pos.map_or(f64::INFINITY, |target| {
                    let delta = mob_behavior_delta(input.position, target);
                    mob_behavior_dot(delta, delta)
                });
                if distance_sq <= 20.0 * 20.0 {
                    if self.phase_ticks > 25 {
                        self.set_phase(DragonPhase::SittingAttacking);
                        return Some(DragonEvent::PerchAttack);
                    }
                } else if self.phase_ticks >= 100 {
                    self.flame_count = 0;
                    if distance_sq <= 150.0 * 150.0 {
                        self.set_phase(DragonPhase::ChargingPlayer);
                        self.charge_target = input.target_pos;
                    } else {
                        self.set_phase(DragonPhase::Takeoff);
                    }
                }
            }
            DragonPhase::SittingAttacking if self.phase_ticks >= 40 => {
                self.set_phase(DragonPhase::SittingFlaming)
            }
            DragonPhase::ChargingPlayer => {
                if let Some(target) = self.charge_target {
                    let delta = mob_behavior_delta(input.position, target);
                    let distance_sq = mob_behavior_dot(delta, delta);
                    if self.charge_recovery >= 10 {
                        self.set_phase(DragonPhase::HoldingPattern);
                    } else if self.charge_recovery > 0 || !(100.0..=22_500.0).contains(&distance_sq) {
                        self.charge_recovery += 1;
                    }
                } else {
                    self.set_phase(DragonPhase::HoldingPattern);
                }
            }
            DragonPhase::Takeoff if input.path_complete => {
                self.set_phase(DragonPhase::HoldingPattern)
            }
            _ => {}
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlimeKind {
    Slime,
    MagmaCube,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlimeSize {
    Small = 1,
    Medium = 2,
    Large = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlimeBehavior {
    kind: SlimeKind,
    size: SlimeSize,
}

impl SlimeBehavior {
    #[cfg(test)]
    fn max_hp(self) -> f32 {
        let size = self.size as u8;
        f32::from(size * size)
    }

    #[cfg(test)]
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

fn spider_is_hostile(daytime: bool, brightness: u8, provoked: bool) -> bool {
    provoked || !daytime || brightness < 12
}

fn spider_leap_velocity(from: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    let horizontal = mob_behavior_unit([target[0] - from[0], 0.0, target[2] - from[2]]);
    [horizontal[0] * 0.4, 0.4, horizontal[2] * 0.4]
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(test)]
struct CreeperBehavior {
    fuse: i16,
    ignited: bool,
    charged: bool,
}

#[cfg(test)]
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
        } else if target_distance.is_none_or(|distance| distance > 7.0) {
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

fn skeleton_bow_charge_interval(hard_difficulty: bool) -> u8 {
    if hard_difficulty {
        20
    } else {
        40
    }
}

fn skeleton_strafe_velocity(distance: f64, clockwise: bool, forward: bool) -> [f64; 3] {
    if distance < 5.0 || distance > 15.0 {
        return [0.0; 3];
    }
    let direction = if clockwise { 1.0 } else { -1.0 };
    let ahead = if forward { 1.0 } else { -1.0 };
    [direction * 0.25, 0.0, ahead * 0.25]
}

fn skeleton_burns_in_sun(daytime: bool, sky_visible: bool, brightness: f32, helmet: bool) -> bool {
    daytime && sky_visible && brightness > 0.5 && !helmet
}

fn phantom_circle_velocity(pos: [f64; 3], center: [f64; 3], clockwise: bool) -> [f64; 3] {
    let radial = mob_behavior_unit([pos[0] - center[0], 0.0, pos[2] - center[2]]);
    let sign = if clockwise { 1.0 } else { -1.0 };
    [
        radial[0] * -0.1 - radial[2] * sign * 0.2,
        0.0,
        radial[2] * -0.1 + radial[0] * sign * 0.2,
    ]
}

fn phantom_swoop_velocity(pos: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    let delta = mob_behavior_delta(pos, target);
    mob_behavior_unit([delta[0], delta[1] - 1.0, delta[2]])
}

fn phantom_burns_in_sun(daytime: bool, sky_visible: bool, brightness: f32) -> bool {
    daytime && sky_visible && brightness > 0.5
}

fn drowned_should_hide_in_water(daytime: bool, in_water: bool) -> bool {
    daytime && in_water
}

fn drowned_swim_chase_velocity(pos: [f64; 3], target: [f64; 3]) -> [f64; 3] {
    mob_behavior_unit(mob_behavior_delta(pos, target)).map(|component| component * 0.1)
}

fn iron_golem_provoked(village_threat: bool, player_attack: bool) -> bool {
    village_threat || player_attack
}

fn iron_golem_fling_velocity(target_pos: [f64; 3], golem_pos: [f64; 3]) -> [f64; 3] {
    let horizontal = mob_behavior_unit([
        target_pos[0] - golem_pos[0],
        0.0,
        target_pos[2] - golem_pos[2],
    ]);
    [horizontal[0] * 0.4, 0.4, horizontal[2] * 0.4]
}

/// Vanilla `SnowGolem.performRangedAttack` (1.21.1): aims at the target's eye
/// line minus the 1.1 projectile spawn offset, adds the 0.2-per-block arc
/// compensation, and throws at 1.6 speed (the 12.0 inaccuracy stays with the
/// launcher's spread, so this vector is deterministic).
fn snow_golem_snowball_velocity(
    golem_pos: [f64; 3],
    target_pos: [f64; 3],
    target_eye_height: f64,
) -> [f64; 3] {
    // The snowball spawns at `getEyeY() - 0.1` (snow golem eye height 1.7).
    let snowball_y = golem_pos[1] + 1.6;
    let dx = target_pos[0] - golem_pos[0];
    let dy = target_pos[1] + target_eye_height - 1.1 - snowball_y;
    let dz = target_pos[2] - golem_pos[2];
    let arc = (dx * dx + dz * dz).sqrt() * 0.2;
    mob_behavior_unit([dx, dy + arc, dz]).map(|component| component * 1.6)
}

/// Vanilla `AbstractSkeleton.performRangedAttack` (1.21.1): the arrow leaves
/// at `getEyeY() - 0.1`, aims at the target's `getY(1/3)` aim line, adds the
/// 0.2-per-block arc compensation, and shoots at 1.6 speed. Inaccuracy stays
/// with the launcher's spread, so this vector is deterministic.
fn skeleton_arrow_velocity(
    skeleton_pos: [f64; 3],
    target_pos: [f64; 3],
    target_third_height: f64,
    shooter_eye_height: f64,
) -> [f64; 3] {
    let arrow_y = skeleton_pos[1] + shooter_eye_height - 0.1;
    let dx = target_pos[0] - skeleton_pos[0];
    let dy = target_pos[1] + target_third_height - arrow_y;
    let dz = target_pos[2] - skeleton_pos[2];
    let arc = (dx * dx + dz * dz).sqrt() * 0.2;
    mob_behavior_unit([dx, dy + arc, dz]).map(|component| component * 1.6)
}

/// Vanilla `Snowball.onHitEntity`: a thrown snowball deals 3 damage only to
/// Blazes; every other entity takes a 0-damage hurt that still applies the
/// standard projectile knockback.
fn snowball_hit_effect(target: &str) -> (f32, bool) {
    let damage = if mob_behavior_id(target) == "blaze" {
        3.0
    } else {
        0.0
    };
    (damage, true)
}

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

#[derive(Clone, Copy, Default)]
struct WitchNeeds {
    underwater: bool,
    in_fire_or_lava: bool,
    hurt: bool,
    chase_distance: Option<f64>,
}

struct WitchTarget<'a> {
    entity: i32,
    horizontal_distance: f64,
    hp: f32,
    effects: &'a [WitchPotion],
}

#[derive(Debug, PartialEq, Eq)]
enum WitchAction {
    StartDrinking(WitchPotion),
    FinishDrinking(WitchPotion),
    Splash { target: i32, potion: WitchPotion },
}

#[derive(Default)]
struct WitchBehavior {
    drinking: Option<(u8, WitchPotion)>,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PetKind {
    Wolf,
    Cat,
    Parrot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PetTameResult {
    Pass,
    Consumed,
    Tamed,
}

pub struct PetBehavior {
    kind: PetKind,
    owner: Option<[u8; 16]>,
    hp: f32,
    sitting: bool,
    collar_color: u8,
}

impl PetBehavior {
    pub fn new(kind: PetKind) -> Self {
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
            (PetKind::Parrot, _) => 6.0,
        }
    }

    pub fn try_tame(
        &mut self,
        player: [u8; 16],
        item: &str,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> PetTameResult {
        let correct_item = match self.kind {
            PetKind::Wolf => mob_behavior_item_matches(item, WOLF_TAMING_ITEMS),
            PetKind::Cat => mob_behavior_item_matches(item, CAT_TAMING_ITEMS),
            PetKind::Parrot => mob_behavior_item_matches(item, PARROT_FOOD_ITEMS),
        };
        if self.owner.is_some() || !correct_item {
            return PetTameResult::Pass;
        }
        // Vanilla tame rolls: wolf/cat `nextInt(3) == 0`, parrot `nextInt(10) == 0`.
        let bound = if self.kind == PetKind::Parrot { 10 } else { 3 };
        if draw(bound) != 0 {
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

    /// Tamed wolf assist: while ordered to stand it ignores its owner, but a
    /// wolf that is not sitting joins the fight against whoever last hurt or
    /// attacked its owner (vanilla `Wolf.setTarget`/`Wolf.performCommand`
    /// assist rule).
    pub fn wolf_assist_target(
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

    /// Vanilla repulsion radii: untamed cats scare creepers inside 6 blocks
    /// (`CatAvoidEntityGoal`, registered only while the cat is wild) and make
    /// phantoms skip their player attack while one is within 16 blocks
    /// (`Phantom`'s `ckf$i` scan calls `Cat.hiss` on every cat in range).
    pub fn repels(&self, mob: &str, distance: f64) -> bool {
        self.kind == PetKind::Cat
            && match mob_behavior_id(mob) {
                "creeper" => distance <= 6.0,
                "phantom" => distance <= 16.0,
                _ => false,
            }
    }

    /// `Wolf.actuallyHurt` + `Wolf.canArmorAbsorb`: a wolf wearing
    /// `wolf_armor` (the `EquipmentSlot.BODY` stack) absorbs the full amount of
    /// any damage type outside `#minecraft:bypasses_wolf_armor`; the armor
    /// pays `Mth.ceil(amount)` durability (`ai_wolf_armor_durability_cost`)
    /// and the wolf's HP is untouched. `armor_remaining` is the equipped
    /// stack's remaining durability (`None`/`0` = no armor). Returns the HP
    /// damage that reaches the wolf — `0.0` when the armor absorbs the hit.
    pub fn hp_damage_after_armor(
        &self,
        damage_type: &str,
        amount: f32,
        armor_remaining: Option<u8>,
    ) -> f32 {
        let armor_present = armor_remaining.is_some_and(|durability| durability > 0);
        if self.kind == PetKind::Wolf
            && ai_wolf_armor_absorbs(ai_damage_type_bypasses_wolf_armor(damage_type), armor_present)
        {
            0.0
        } else {
            amount
        }
    }
}

#[derive(Debug, PartialEq)]
enum WitherEvent {
    SpawnExplosion { power: f32 },
    #[cfg(test)]
    Drop { item: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WitherSkull {
    Black { target: i32 },
    Blue { target: i32 },
}

struct WitherBehavior {
    hp: f32,
    spawn_ticks: u16,
    cage_clear_ticks: u8,
}

impl WitherBehavior {
    const MAX_HP: f32 = 300.0;
    const DEATH_DROP: &'static str = "nether_star";

    fn new() -> Self {
        Self {
            hp: Self::MAX_HP / 3.0,
            spawn_ticks: 220,
            cage_clear_ticks: 0,
        }
    }

    fn observe_health(&mut self, hp: f32) -> bool {
        if self.spawn_ticks == 0 && self.cage_clear_ticks == 0 && hp < self.hp {
            self.cage_clear_ticks = 20;
        }
        self.hp = hp;
        if self.cage_clear_ticks == 0 {
            return false;
        }
        self.cage_clear_ticks -= 1;
        self.cage_clear_ticks == 0
    }

    fn has_wither_armor(&self) -> bool {
        self.hp > 0.0 && self.hp <= Self::MAX_HP / 2.0
    }

    fn immune_to(&self, hit: BossHit) -> bool {
        self.spawn_ticks > 0
            || (self.has_wither_armor()
                && matches!(
                    hit,
                    BossHit::Arrow | BossHit::Trident | BossHit::WindCharge
                ))
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

    #[cfg(test)]
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

#[derive(Default)]
struct WardenAngerManagement {
    entities: HashMap<i32, u16>,
    decay_ticks: u8,
}

impl WardenAngerManagement {
    const AGITATED: u16 = 80;
    const ROAR: u16 = 150;

    #[cfg(test)]
    fn add_vibration(&mut self, entity: i32) -> u16 {
        let anger = self.entities.entry(entity).or_default();
        *anger = (*anger + 35).min(Self::ROAR);
        *anger
    }

    /// A sniff noticing an entity adds the same +35 a vibration does.
    #[cfg(test)]
    fn add_sniff(&mut self, entity: i32) -> u16 {
        self.add_vibration(entity)
    }

    /// A direct hit angers the warden more than a vibration but far less
    /// than the stored ceiling: vanilla `ON_HURT_ANGER_BOOST` is 20 against
    /// `DEFAULT_ANGER` 35 for vibrations.
    fn add_direct_hit(&mut self, entity: i32) -> u16 {
        let anger = self.entities.entry(entity).or_default();
        *anger = (*anger + 20).min(Self::ROAR);
        *anger
    }

    fn anger(&self, entity: i32) -> u16 {
        self.entities.get(&entity).copied().unwrap_or(0)
    }

    fn agitated(&self, entity: i32) -> bool {
        self.anger(entity) >= Self::AGITATED
    }

    fn roar_target(&self) -> Option<i32> {
        // Vanilla converts anger into a target lock at 80; 150 is only the
        // stored anger ceiling, not the acquisition threshold.
        self.entities
            .iter()
            .filter(|&(_, &anger)| anger >= Self::AGITATED)
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

#[derive(Clone, Copy)]
struct WardenTarget {
    entity: i32,
    offset: [f64; 3],
}

#[derive(Debug, PartialEq)]
enum WardenEvent {
    Roar {
        target: i32,
    },
    /// Reachable victims take the warden's full melee hit.
    #[cfg(test)]
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

struct WardenBehavior {
    hp: f32,
    anger: WardenAngerManagement,
    target: Option<i32>,
    sonic_charge: u8,
    sonic_cooldown: u8,
}

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

    #[cfg(test)]
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

#[derive(Clone, Copy, Debug)]
struct MobTargetSnapshot {
    eid: i32,
    pos: [f64; 3],
    hp: f32,
    visible: bool,
}

/// One game tick of observations and one draw from the caller's world RNG.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MobProjectileKind {
    SmallFireball,
    GhastFireball,
    ShulkerBullet { target_eid: i32 },
    DragonFireball,
    WitchSplash(WitchPotion),
    WitherSkull { blue: bool },
}

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
    WardenRoar,
    WardenSonicCharge,
    WitherCageClear,
    PiglinBarter {
        player_eid: i32,
        loot: PiglinBarterLoot,
    },
    DragonFlameBreath,
    DragonPerchAttack,
}

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
    Parrot(PetBehavior),
    Breeze(BreezeBehavior),
    Wither(WitherBehavior),
    Warden(WardenBehavior),
}

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
            75 => Self::Parrot(PetBehavior::new(PetKind::Parrot)),
            12 => Self::Breeze(BreezeBehavior::new()),
            119 => Self::Wither(WitherBehavior::new()),
            116 => Self::Warden(WardenBehavior::new()),
            _ => return None,
        })
    }

    #[cfg(test)]
    fn step(&mut self, ctx: &MobBehaviorTickCtx) -> Vec<MobBehaviorEffect> {
        self.step_with_world(ctx, MobBehaviorWorldTick::Basic)
    }

    fn step_enderman(
        ctx: &MobBehaviorTickCtx,
        world: MobBehaviorWorldTick<'_>,
        draw: &mut impl FnMut(u32) -> u32,
    ) -> Option<MobBehaviorEffect> {
        let MobBehaviorWorldTick::Enderman {
            block_at,
            can_occupy,
        } = world
        else {
            return None;
        };
        let hazard = match (ctx.in_water, ctx.in_rain) {
            (true, _) => EndermanHazard::Water,
            (false, true) => EndermanHazard::Rain,
            _ => return None,
        };
        let bottom = ctx.dim.min_y();
        enderman_escape(
            hazard,
            ctx.pos,
            bottom..bottom + ctx.dim.height(),
            block_at,
            can_occupy,
            draw,
        )
            .destination
            .map(|to| MobBehaviorEffect::Teleport { to })
    }

    fn step_dragon(
        dragon: &mut EnderDragonBehavior,
        ctx: &MobBehaviorTickCtx,
        target: Option<MobTargetSnapshot>,
        world: MobBehaviorWorldTick<'_>,
        effects: &mut Vec<MobBehaviorEffect>,
        launch: &impl Fn(MobProjectileKind, MobTargetSnapshot, f32) -> MobBehaviorEffect,
    ) -> Option<MobBehaviorEffect> {
        use MobBehaviorEffect::*;
        let MobBehaviorWorldTick::EnderDragon {
            portal_y,
            path_complete,
            strafe_aligned,
            crystals,
        } = world
        else {
            return None;
        };
        if !matches!(dragon.phase, DragonPhase::Dying | DragonPhase::Dead) {
            if ctx.hp <= 0.0 {
                dragon.hurt(BossHit::Other, dragon.hp);
            } else {
                dragon.hp = ctx.hp;
            }
        }
        let hp_before = dragon.hp;
        // Only crystals at a generated spike anchor can feed the dragon.
        let anchors: Vec<[f64; 3]> = end_crystal_pillars(WORLD_SEED.load(Ordering::Relaxed))
            .iter()
            .map(end_crystal_spawn_position)
            .collect();
        let anchored: Vec<DragonCrystal> = crystals
            .iter()
            .copied()
            .filter(|crystal| {
                anchors.iter().any(|anchor| {
                    let delta = mob_behavior_delta(crystal.position, *anchor);
                    mob_behavior_dot(delta, delta) <= 4.0
                })
            })
            .collect();
        let event = dragon.tick(DragonTick {
            position: ctx.pos, portal_y, path_complete,
            strafe_target: target.map(|target| target.eid), strafe_aligned,
            target_pos: target.map(|target| target.pos), crystals: &anchored,
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
                spawn_egg: rewards.dragon_egg.is_some(), xp: rewards.xp,
            }),
            None => None,
        }
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
                    Some(BlazeAttack::Melee { target, damage }) => {
                        // `BlazeAttackGoal` only strikes once the blaze holds
                        // its hover station half a block above the target's
                        // mid-body (`getY(0.5) + 0.5`); players are 1.8 tall.
                        let on_station = visible_target.is_none_or(|target| {
                            (ctx.pos[1] - blaze_hover_y(target.pos, 1.8)).abs() <= 2.0
                        });
                        on_station.then_some(MeleeDamage {
                            target_eid: target,
                            amount: damage,
                            knockback_y: 0.0,
                        })
                    }
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
                match piglin.tick(dimension_piglin_safe(ctx.dim), &mut draw) {
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
                .tick(dimension_piglin_safe(ctx.dim))
                .map(|HoglinEvent::ConvertToZoglin| ConvertEntity { target_etype: 123 }),
            Self::Enderman => Self::step_enderman(ctx, world, &mut draw),
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
            Self::EnderDragon(dragon) => Self::step_dragon(dragon, ctx, target, world, &mut effects, &launch),
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
            Self::Wolf(pet) | Self::Cat(pet) | Self::Parrot(pet) => {
                let _ = pet.kind;
                None // Pet interactions are event-driven.
            }
            Self::Breeze(breeze) => {
                let event = breeze.tick(
                    ctx.pos,
                    visible_target.map(|target| (target.eid, target.pos)),
                    // The live mob tick has no collision bit in its snapshot. Treat
                    // a breeze outside fluid as able to start its ground jump; the
                    // world-side movement solver still clips the resulting step.
                    !ctx.in_water,
                );
                match event {
                    Some(BreezeEvent::LongJump { velocity })
                    | Some(BreezeEvent::Slide { velocity }) => Some(Teleport {
                        to: std::array::from_fn(|axis| ctx.pos[axis] + velocity[axis]),
                    }),
                    Some(BreezeEvent::WindCharge { .. }) => visible_target
                        .map(|target| launch(MobProjectileKind::SmallFireball, target, 0.0)),
                    Some(BreezeEvent::ReflectProjectile { .. }) | None => None,
                }
            }
            Self::Wither(wither) => {
                if wither.observe_health(ctx.hp) {
                    effects.push(WitherCageClear);
                }
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
                    Some(WardenEvent::Roar { .. }) => Some(WardenRoar),
                    Some(WardenEvent::ChargeSonicBoom { .. }) => Some(WardenSonicCharge),
                    Some(WardenEvent::SonicBoom { target, damage, .. }) => Some(SonicBoom {
                        target_eid: target,
                        damage,
                    }),
                    #[cfg(test)]
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


// ---------- Vanilla 1.21.1 boss-fight and trial-mob contracts ----------
//
// These evaluators own the parts of the boss fights that cannot be expressed
// by the single live target snapshot: dragon body parts and fight ceremony,
// wither skull/block rules, warden lifecycle timing, and the 1.21 trial mobs.
// They are deliberately deterministic: the world supplies positions, block
// facts and scripted random draws, while these types own vanilla's arithmetic.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragonCombatPart {
    Head,
    Neck,
    Body,
    Wing,
    Tail,
}

/// Vanilla `EnderDragon.hurt(EnderDragonPart, ...)`: the head pays the hit
/// through unchanged, while every other part first passes through
/// `damage / 4 + 1` (the minimum enormous-body-part hit is therefore one).
pub fn dragon_part_damage(part: DragonCombatPart, damage: f32) -> f32 {
    if part == DragonCombatPart::Head {
        damage.max(0.0)
    } else {
        damage.max(0.0) / 4.0 + 1.0
    }
}

/// A wing sweep pushes entities horizontally away from the dragon and lifts
/// them by the wingbeat's small vertical impulse. `wing_center` lets the
/// caller supply the animated part centre; anything inside the vanilla wing
/// band (three blocks horizontally, two vertically) is launched.
pub fn dragon_wing_knockback(wing_center: [f64; 3], entity: [f64; 3]) -> Option<[f64; 3]> {
    let delta = mob_behavior_delta(wing_center, entity);
    if delta[0].abs() > 3.0 || delta[1].abs() > 2.0 || delta[2].abs() > 3.0 {
        return None;
    }
    let horizontal = mob_behavior_unit([delta[0], 0.0, delta[2]]);
    Some([horizontal[0] * 0.8, 0.35, horizontal[2] * 0.8])
}

/// Dragon-breath area-effect cloud. A sitting flame attack creates a radius-5
/// cloud lasting 200 ticks; glass bottles scoop dragon breath and shrink the
/// cloud by the vanilla half-block per bottle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragonBreathCloud {
    pos: [f64; 3],
    radius: f32,
    left: u16,
}

impl DragonBreathCloud {
    pub fn sitting_flame(pos: [f64; 3]) -> Self {
        Self {
            pos,
            radius: 5.0,
            left: 200,
        }
    }

    pub fn tick(&mut self) -> bool {
        self.left = self.left.saturating_sub(1);
        self.left > 0 && self.radius > 0.0
    }

    /// Returns the filled bottle item when a glass bottle can scoop this cloud.
    pub fn scoop(&mut self, bottle: &str) -> Option<&'static str> {
        if mob_behavior_id(bottle) != "glass_bottle" || self.radius <= 0.0 || self.left == 0 {
            return None;
        }
        self.radius -= 0.5;
        if self.radius <= 0.0 {
            self.left = 0;
        }
        Some("dragon_breath")
    }
}

/// End-gateway positions on vanilla's 96-block ring. One gateway is opened per
/// dragon kill until all twenty exist; the deterministic index order is the
/// ring order, which keeps the cap and spacing testable without exposing an
/// implementation random stream.
pub fn end_gateway_ring_positions(count: usize, surface_y: i32) -> Vec<[i32; 3]> {
    let count = count.min(20);
    (0..count)
        .map(|index| {
            let angle = 2.0 * std::f64::consts::PI * index as f64 / 20.0;
            [
                (96.0 * angle.cos()).round() as i32,
                surface_y,
                (96.0 * angle.sin()).round() as i32,
            ]
        })
        .collect()
}

/// The four crystal positions of the dragon respawn ritual: one on each side
/// of the exit portal's bedrock rim, two blocks out from the portal centre
/// (`EndDragonFight.java:499-506` — `portal.above(1).relative(direction, 2)`).
pub fn dragon_respawn_crystal_positions(portal_y: i32) -> [[i32; 3]; 4] {
    [
        [2, portal_y + 1, 0],
        [0, portal_y + 1, 2],
        [-2, portal_y + 1, 0],
        [0, portal_y + 1, -2],
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragonRespawnEvent {
    CloseExitPortal,
    BeamAt { crystal: [i32; 3] },
    RestoreCrystal { crystal: [i32; 3] },
    SpawnDragon,
}

/// The four-crystal respawn ceremony. The first tick closes the portal, beams
/// then rebuild the four crystals over the next hundred ticks, and the dragon
/// is summoned only after every crystal has been restored.
#[derive(Debug)]
pub struct DragonRespawnRitual {
    portal_y: i32,
    ticks: u16,
    complete: bool,
}

impl DragonRespawnRitual {
    pub fn start(portal_y: i32, placed: &[[i32; 3]]) -> Option<Self> {
        let mut required = dragon_respawn_crystal_positions(portal_y);
        required.sort();
        let mut placed = placed.to_vec();
        placed.sort();
        (placed == required).then_some(Self {
            portal_y,
            ticks: 0,
            complete: false,
        })
    }

    pub fn tick(&mut self) -> Vec<DragonRespawnEvent> {
        if self.complete {
            return Vec::new();
        }
        self.ticks = self.ticks.saturating_add(1);
        let crystals = dragon_respawn_crystal_positions(self.portal_y);
        let mut events = Vec::new();
        if self.ticks == 1 {
            events.push(DragonRespawnEvent::CloseExitPortal);
        }
        if self.ticks <= 100 && self.ticks.is_multiple_of(25) {
            let crystal = crystals[(self.ticks as usize / 25 - 1) % crystals.len()];
            events.push(DragonRespawnEvent::BeamAt { crystal });
        }
        if self.ticks == 100 {
            events.extend(crystals.map(|crystal| DragonRespawnEvent::RestoreCrystal { crystal }));
        }
        if self.ticks == 101 {
            self.complete = true;
            events.push(DragonRespawnEvent::SpawnDragon);
        }
        events
    }
}


/// Portal rewards produced when the End-owned dragon fight is completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndPortalReward {
    pub exit_portal_base: [i32; 3],
    pub dragon_egg: Option<[i32; 3]>,
    pub xp: u32,
}

/// World-facing effects of the End dragon fight controller. Entity creation is
/// deliberately left to the End host; the controller owns the vanilla sequence,
/// crystal positions, healing linkage, and respawn ritual state.
#[derive(Clone, Debug, PartialEq)]
pub enum EndDragonFightEvent {
    SpawnDragon { position: [f64; 3], respawned: bool },
    MaterializeCrystal { position: [f64; 3] },
    CrystalBeam { crystal: [i32; 3] },
    CloseExitPortal,
    BeamAt { crystal: [i32; 3] },
}

/// End-owned dragon fight state. The ten pillar crystals are keyed by their
/// integer block position; the values are deterministic fight-local crystal
/// entity ids used only for healing-link identity inside the controller.
pub struct EndDragonFightController {
    world_seed: u64,
    portal_y: i32,
    dragon: EnderDragonBehavior,
    initialized: bool,
    dragon_alive: bool,
    kills: u32,
    ticks: u64,
    pillar_crystals: std::collections::BTreeMap<[i32; 3], i32>,
    /// Live crystals that are not fight pillars — player placements and any
    /// other `EndCrystal` entities. Vanilla's healing scan sees every live
    /// crystal inside the dragon's inflated bounding box, not just the ten
    /// spike-top pillars (`EnderDragon.checkCrystals`).
    extra_crystals: std::collections::BTreeMap<[i32; 3], i32>,
    ritual_placed: std::collections::BTreeSet<[i32; 3]>,
    respawn_ticks: Option<u16>,
    next_crystal_entity: i32,
    linked_crystal: Option<i32>,
    /// Deterministic per-fight stream for the vanilla
    /// `random.nextInt(10) == 0` crystal re-pick draw.
    rng_state: u64,
}

impl EndDragonFightController {
    pub fn new(world_seed: u64, portal_y: i32) -> Self {
        Self {
            world_seed,
            portal_y,
            dragon: EnderDragonBehavior::new(false),
            initialized: false,
            dragon_alive: false,
            kills: 0,
            ticks: 0,
            pillar_crystals: std::collections::BTreeMap::new(),
            extra_crystals: std::collections::BTreeMap::new(),
            ritual_placed: std::collections::BTreeSet::new(),
            respawn_ticks: None,
            next_crystal_entity: 1,
            linked_crystal: None,
            rng_state: (world_seed ^ 0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// xorshift64* — the fight-local stream behind the vanilla
    /// `random.nextInt(10)` crystal re-pick.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Create the initial fight exactly once: the dragon at the vanilla high
    /// central spawn point plus the ten crystals on this seed's obsidian spikes.
    pub fn ensure_dragon_fight_initialized(&mut self) -> Vec<EndDragonFightEvent> {
        if self.initialized {
            return Vec::new();
        }
        self.initialized = true;
        self.dragon_alive = true;
        self.dragon = EnderDragonBehavior::new(self.kills > 0);
        let mut events = vec![EndDragonFightEvent::SpawnDragon {
            position: [0.0, 128.0, 0.0],
            respawned: false,
        }];
        events.extend(self.materialize_end_pillar_crystals());
        events
    }

    /// Materialize every missing spike-top crystal. Positions come from the
    /// same seed-derived ten-spike layout used by the live dragon behavior.
    pub fn materialize_end_pillar_crystals(&mut self) -> Vec<EndDragonFightEvent> {
        let mut events = Vec::new();
        for pillar in end_crystal_pillars(self.world_seed) {
            let position = end_crystal_spawn_position(&pillar);
            let key = position.map(|coordinate| coordinate.floor() as i32);
            if self.pillar_crystals.contains_key(&key) {
                continue;
            }
            let entity = self.next_crystal_entity;
            self.next_crystal_entity += 1;
            self.pillar_crystals.insert(key, entity);
            events.push(EndDragonFightEvent::MaterializeCrystal { position });
        }
        events
    }

    /// Register a player-placed end crystal. Returns true when the position is
    /// one of the four vanilla respawn-ritual rim positions and the crystal was
    /// accepted into that ritual. Completing the quartet starts the ceremony.
    /// Every other placement — including crystals placed while the dragon
    /// lives — is tracked as a live crystal that can feed the healing beam.
    pub fn on_end_crystal_placed(&mut self, x: i32, y: i32, z: i32) -> bool {
        let pos = [x, y, z];
        if !self.dragon_alive
            && self.respawn_ticks.is_none()
            && dragon_respawn_crystal_positions(self.portal_y).contains(&pos)
        {
            self.ritual_placed.insert(pos);
            if self.ritual_placed.len() == dragon_respawn_crystal_positions(self.portal_y).len() {
                self.ritual_placed.clear();
                self.respawn_ticks = Some(0);
            }
            return true;
        }
        if !self.pillar_crystals.contains_key(&pos) && !self.extra_crystals.contains_key(&pos) {
            let entity = self.next_crystal_entity;
            self.next_crystal_entity += 1;
            self.extra_crystals.insert(pos, entity);
        }
        false
    }

    /// Remove a destroyed crystal. Destroying the crystal currently linked by a
    /// healing beam deals vanilla's 10 damage to the dragon; destroying an
    /// unlinked crystal merely removes it. Returns whether a tracked fight
    /// crystal was consumed.
    pub fn on_end_crystal_destroyed(&mut self, pos: [i32; 3]) -> bool {
        let entity = self
            .pillar_crystals
            .remove(&pos)
            .or_else(|| self.extra_crystals.remove(&pos));
        if let Some(entity) = entity {
            if self.linked_crystal == Some(entity) {
                self.dragon.crystal_destroyed(entity);
                self.linked_crystal = None;
                if self.dragon.hp == 0.0 {
                    self.dragon_alive = false;
                }
            }
            return true;
        }
        self.ritual_placed.remove(&pos)
    }

    /// Advance healing linkage and, while active, the 100-tick respawn ritual.
    pub fn tick(&mut self, dragon_position: [f64; 3]) -> Vec<EndDragonFightEvent> {
        let mut events = Vec::new();

        if let Some(ticks) = self.respawn_ticks {
            let ticks = ticks.saturating_add(1);
            self.respawn_ticks = Some(ticks);
            let ritual_crystals = dragon_respawn_crystal_positions(self.portal_y);
            if ticks == 1 {
                events.push(EndDragonFightEvent::CloseExitPortal);
            }
            if ticks <= 100 && ticks.is_multiple_of(25) {
                events.push(EndDragonFightEvent::BeamAt {
                    crystal: ritual_crystals[(ticks as usize / 25 - 1) % ritual_crystals.len()],
                });
            }
            if ticks == 100 {
                events.extend(self.materialize_end_pillar_crystals());
            }
            if ticks == 101 {
                self.respawn_ticks = None;
                self.dragon = EnderDragonBehavior::new(true);
                self.dragon_alive = true;
                self.linked_crystal = None;
                events.push(EndDragonFightEvent::SpawnDragon {
                    position: [0.0, 128.0, 0.0],
                    respawned: true,
                });
            }
            return events;
        }

        if !self.dragon_alive {
            return events;
        }
        self.ticks = self.ticks.saturating_add(1);
        // `EnderDragon.checkCrystals`: a live linked crystal heals 1 HP every
        // ten ticks; the link itself is re-picked at random.
        if self.ticks.is_multiple_of(10) {
            if let Some(entity) = self.linked_crystal {
                if let Some(crystal) = self.crystal_block(entity) {
                    if self.dragon.hp < 200.0 {
                        self.dragon.hp = (self.dragon.hp + 1.0).min(200.0);
                    }
                    events.push(EndDragonFightEvent::CrystalBeam { crystal });
                }
            }
        }
        if self.next_u64().is_multiple_of(10) {
            self.linked_crystal = self.nearest_crystal(dragon_position);
        }
        self.dragon.linked_crystal = self.linked_crystal;
        events
    }

    /// The block position a tracked crystal entity id belongs to, across both
    /// the fight pillars and player-placed live crystals.
    fn crystal_block(&self, entity: i32) -> Option<[i32; 3]> {
        self.pillar_crystals
            .iter()
            .chain(self.extra_crystals.iter())
            .find(|(_, &candidate)| candidate == entity)
            .map(|(&block, _)| block)
    }

    /// `EnderDragon.checkCrystals` scans every live `EndCrystal` inside the
    /// dragon's bounding box inflated by 32 blocks (`bb.grow(32, 32, 32)`)
    /// and keeps the nearest by entity distance — pillar crystals and
    /// player-placed ones alike. The dragon is 16x8, so the inflated box
    /// reaches 40 blocks horizontally and 32 below / 40 above its position.
    fn nearest_crystal(&self, dragon_position: [f64; 3]) -> Option<i32> {
        const REACH_XZ: f64 = 8.0 + 32.0;
        const REACH_BELOW: f64 = -32.0;
        const REACH_ABOVE: f64 = 8.0 + 32.0;
        let in_reach = |position: [f64; 3]| {
            let delta = mob_behavior_delta(dragon_position, position);
            delta[0].abs() <= REACH_XZ
                && (REACH_BELOW..=REACH_ABOVE).contains(&delta[1])
                && delta[2].abs() <= REACH_XZ
        };
        // Spike-top crystals spawn one block above their obsidian pillar;
        // placed crystals sit on the face they were clicked onto.
        self.pillar_crystals
            .iter()
            .map(|(&block, &entity)| {
                (
                    [
                        block[0] as f64 + 0.5,
                        block[1] as f64 + 1.0,
                        block[2] as f64 + 0.5,
                    ],
                    entity,
                )
            })
            .chain(self.extra_crystals.iter().map(|(&block, &entity)| {
                (
                    [
                        block[0] as f64 + 0.5,
                        block[1] as f64,
                        block[2] as f64 + 0.5,
                    ],
                    entity,
                )
            }))
            .filter(|(position, _)| in_reach(*position))
            .min_by(|a, b| {
                let da = mob_behavior_delta(dragon_position, a.0);
                let db = mob_behavior_delta(dragon_position, b.0);
                mob_behavior_dot(da, da)
                    .total_cmp(&mob_behavior_dot(db, db))
                    .then_with(|| a.1.cmp(&b.1))
            })
            .map(|(_, entity)| entity)
    }

    /// Complete the fight and return the exit-portal reward. The first kill
    /// awards 12,000 XP and the egg; later kills award 500 XP and no egg.
    pub fn on_dragon_killed(&mut self) -> EndPortalReward {
        let first = self.kills == 0;
        self.kills = self.kills.saturating_add(1);
        self.dragon_alive = false;
        self.linked_crystal = None;
        self.dragon.linked_crystal = None;
        EndPortalReward {
            exit_portal_base: [0, self.portal_y, 0],
            dragon_egg: first.then_some([0, self.portal_y + 4, 0]),
            xp: if first { 12_000 } else { 500 },
        }
    }

    /// Restore the durable save record. The record stores the binary fight
    /// state rather than runtime entity ids: `previously_killed` controls the
    /// reduced later-kill reward and `dragon_killed` means no live dragon
    /// should be materialized until the crystal ritual runs.
    pub fn restored(world_seed: u64, portal_y: i32, dragon_killed: bool, previously_killed: bool) -> Self {
        let mut fight = Self::new(world_seed, portal_y);
        fight.initialized = true;
        fight.dragon_alive = !dragon_killed;
        fight.kills = u32::from(previously_killed);
        fight.dragon = EnderDragonBehavior::new(previously_killed);
        fight
    }

    pub fn dragon_alive(&self) -> bool {
        self.dragon_alive
    }

    pub fn active_pillar_crystals(&self) -> Vec<[i32; 3]> {
        self.pillar_crystals.keys().copied().collect()
    }

    pub fn dragon_hp(&self) -> f32 {
        self.dragon.hp
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WitherHead {
    Center,
    Left,
    Right,
}

/// Independent wither-head cadence. The centre head uses its faster normal
/// attack timer while the two side heads carry separate cooldowns; blue skulls
/// remain the rare one-in-a-thousand shot unless the caller forces one.
pub fn wither_head_cadence(head: WitherHead) -> u16 {
    match head {
        WitherHead::Center => 20,
        WitherHead::Left | WitherHead::Right => 40,
    }
}

/// Vanilla blue wither skulls ignore ordinary explosion resistance when they
/// erase blocks, but still cannot remove bedrock or end-portal framework.
pub fn wither_skull_breaks_block(blue: bool, block: &str) -> bool {
    if !blue {
        return false;
    }
    !matches!(
        mob_behavior_id(block),
        "bedrock" | "end_portal" | "end_portal_frame" | "barrier" | "command_block"
            | "chain_command_block" | "repeating_command_block" | "structure_block" | "jigsaw"
    )
}

/// The wither's damage-triggered cage clear is a 3x4x3 box centred on its XZ
/// position and rising from its feet. The predicate supplies blast resistance
/// or unbreakable knowledge; positions are returned in vanilla's deterministic
/// x/y/z scan order.
pub fn wither_cage_clear(
    center: [i32; 3],
    mut breakable: impl FnMut([i32; 3]) -> bool,
) -> Vec<[i32; 3]> {
    let mut cleared = Vec::new();
    for dy in 0..4 {
        for dx in -1..=1 {
            for dz in -1..=1 {
                let pos = [center[0] + dx, center[1] + dy, center[2] + dz];
                if breakable(pos) {
                    cleared.push(pos);
                }
            }
        }
    }
    cleared
}

/// Nether stars survive the explosion that produced them (and later explosions)
/// as blast-resistant item entities.
pub fn nether_star_ignores_explosions(item: &str) -> bool {
    mob_behavior_id(item) == "nether_star"
}

/// Warden melee damage before protection: 16/30/45 by Easy/Normal/Hard.
pub fn warden_melee_damage(difficulty: u8) -> f32 {
    match difficulty {
        1 => 16.0,
        3 => 45.0,
        _ => 30.0,
    }
}

/// Vanilla `DARKNESS_DURATION`: the warden's pulse applies Darkness for 260
/// ticks to every player inside its 20-block sphere.
pub const WARDEN_DARKNESS_DURATION_TICKS: u16 = 260;

/// The darkness pulse repeats every 120 ticks while a player is inside the
/// warden's 20-block-radius sphere (`DARKNESS_RADIUS`), returning the effect
/// duration to apply when the pulse fires.
pub fn warden_darkness_pulse(tick: u64, offset: [f64; 3]) -> Option<u16> {
    (tick.is_multiple_of(120)
        && offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2] <= 400.0)
        .then_some(WARDEN_DARKNESS_DURATION_TICKS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WardenLifecyclePhase {
    Emerging(u16),
    Roaring(u16),
    Sniffing(u16),
    Investigating,
    Attacking,
    Digging(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WardenLifecycleEvent {
    Emerged,
    Roar,
    Sniff,
    Attack,
    Dig,
    Resurface,
}

/// Vanilla's spawn/lifecycle shell: emerge for 134 ticks, roar for 84 when a
/// target locks, sniff for 84, investigate or attack, and dig for 100 ticks
/// after 1200 calm ticks. The behavior owns no wall clock; tests and the live
/// adapter feed one game tick at a time.
#[derive(Debug)]
pub struct WardenLifecycle {
    phase: WardenLifecyclePhase,
    calm_ticks: u16,
}

impl WardenLifecycle {
    pub fn emerging() -> Self {
        Self {
            phase: WardenLifecyclePhase::Emerging(134),
            calm_ticks: 0,
        }
    }

    pub fn tick(&mut self, angry: bool, target_reachable: bool) -> Option<WardenLifecycleEvent> {
        match self.phase {
            WardenLifecyclePhase::Emerging(left) => {
                if left == 1 {
                    self.phase = WardenLifecyclePhase::Sniffing(84);
                    Some(WardenLifecycleEvent::Emerged)
                } else {
                    self.phase = WardenLifecyclePhase::Emerging(left - 1);
                    None
                }
            }
            WardenLifecyclePhase::Roaring(left) => {
                if left == 1 {
                    self.phase = WardenLifecyclePhase::Attacking;
                    Some(WardenLifecycleEvent::Attack)
                } else {
                    self.phase = WardenLifecyclePhase::Roaring(left - 1);
                    None
                }
            }
            WardenLifecyclePhase::Sniffing(left) => {
                if angry {
                    self.phase = WardenLifecyclePhase::Roaring(84);
                    return Some(WardenLifecycleEvent::Roar);
                }
                if left == 1 {
                    self.phase = WardenLifecyclePhase::Investigating;
                    Some(WardenLifecycleEvent::Sniff)
                } else {
                    self.phase = WardenLifecyclePhase::Sniffing(left - 1);
                    None
                }
            }
            WardenLifecyclePhase::Investigating | WardenLifecyclePhase::Attacking => {
                if angry {
                    self.calm_ticks = 0;
                    if !matches!(self.phase, WardenLifecyclePhase::Attacking) {
                        self.phase = WardenLifecyclePhase::Roaring(84);
                        return Some(WardenLifecycleEvent::Roar);
                    }
                    if target_reachable {
                        return Some(WardenLifecycleEvent::Attack);
                    }
                } else {
                    self.calm_ticks = self.calm_ticks.saturating_add(1);
                    if self.calm_ticks >= 1200 {
                        self.calm_ticks = 0;
                        self.phase = WardenLifecyclePhase::Digging(100);
                        return Some(WardenLifecycleEvent::Dig);
                    }
                }
                None
            }
            WardenLifecyclePhase::Digging(left) => {
                if angry {
                    self.phase = WardenLifecyclePhase::Emerging(134);
                    return Some(WardenLifecycleEvent::Resurface);
                }
                if left == 1 {
                    self.phase = WardenLifecyclePhase::Investigating;
                    None
                } else {
                    self.phase = WardenLifecyclePhase::Digging(left - 1);
                    None
                }
            }
        }
    }
}

/// Guardian thorns: a melee attacker takes 2 damage while the guardian's
/// spikes are extended. Elder guardians use the same reflection rule.
pub fn guardian_spikes_reflect_damage(spikes_extended: bool, melee_attack: bool) -> f32 {
    if spikes_extended && melee_attack {
        2.0
    } else {
        0.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BreezeEvent {
    LongJump { velocity: [f64; 3] },
    Slide { velocity: [f64; 3] },
    WindCharge { target: i32 },
    ReflectProjectile { projectile: i32, back_at: i32 },
}

/// Breeze movement and volley state. The long jump is the high arc used to
/// close distance, the slide is the low post-jump strafe, and wind charges are
/// launched only after the vanilla ranged cooldown has elapsed.
#[derive(Debug, Default)]
pub struct BreezeBehavior {
    attack_cooldown: u16,
    jump_ticks: u8,
    sliding: bool,
}

impl BreezeBehavior {
    pub fn new() -> Self {
        Self {
            attack_cooldown: 0,
            jump_ticks: 0,
            sliding: false,
        }
    }

    pub fn tick(&mut self, pos: [f64; 3], target: Option<(i32, [f64; 3])>, grounded: bool) -> Option<BreezeEvent> {
        self.attack_cooldown = self.attack_cooldown.saturating_sub(1);
        let (target_eid, target_pos) = target?;
        let delta = mob_behavior_delta(pos, target_pos);
        let horizontal = mob_behavior_unit([delta[0], 0.0, delta[2]]);
        if self.jump_ticks > 0 {
            self.jump_ticks -= 1;
            self.sliding = self.jump_ticks == 0;
            return Some(BreezeEvent::LongJump {
                velocity: [horizontal[0] * 0.7, 0.8, horizontal[2] * 0.7],
            });
        }
        if self.sliding {
            self.sliding = false;
            return Some(BreezeEvent::Slide {
                velocity: [horizontal[2] * 0.45, 0.0, -horizontal[0] * 0.45],
            });
        }
        let distance_sq = mob_behavior_dot(delta, delta);
        let distance = distance_sq.sqrt();
        // `LongJump`: `tooCloseForJump` suppresses the hop at <= 4 blocks and
        // `outOfAggroRange` beyond 24.
        if grounded && distance > 4.0 && distance <= 24.0 {
            self.jump_ticks = 8;
            return Some(BreezeEvent::LongJump {
                velocity: [horizontal[0] * 0.7, 0.8, horizontal[2] * 0.7],
            });
        }
        // `Shoot.isTargetWithinRange`: distance squared inside (4.0, 256.0) —
        // between 2 and 16 blocks — with `SHOOT_COOLDOWN_TICKS` = 10.
        if self.attack_cooldown == 0 && distance_sq > 4.0 && distance_sq < 256.0 {
            self.attack_cooldown = 10;
            return Some(BreezeEvent::WindCharge { target: target_eid });
        }
        None
    }

    pub fn reflect_projectile(&self, projectile: i32, shooter: i32) -> BreezeEvent {
        BreezeEvent::ReflectProjectile {
            projectile,
            back_at: shooter,
        }
    }
}

/// Wind bursts flip the redstone interactables vanilla marks as wind-sensitive.
pub fn wind_charge_flips_block(block: &str) -> bool {
    let block = mob_behavior_id(block);
    block.ends_with("_door")
        || block.ends_with("_trapdoor")
        || block.ends_with("_button")
        || block == "lever"
}

/// Bogged arrows apply Poison I for the 1.21.1 four-second tipped-arrow hit.
pub fn bogged_arrow_poison() -> (i32, u16) {
    (0, 80)
}

pub fn bogged_attack_interval_ticks() -> u16 {
    70
}

pub fn bogged_max_hp() -> f32 {
    16.0
}

/// Shearing removes both head mushrooms and leaves the bogged alive; vanilla
/// drops the two mushroom items and makes the operation irreversible.
pub fn bogged_shear_mushrooms(sheared: bool) -> Option<(&'static str, u8)> {
    (!sheared).then_some(("brown_mushroom", 2))
}

// ---------- P2: mob gamerules ----------
//
// Each accessor reads `gamerule_live_value` (world_mechanisms.rs) — the same
// atomic registry and stored map `/gamerule` mutates.

/// `mobGriefing`: whether mobs may modify the world — pick up items (the
/// piglin barter gate above), carry or break blocks, trample crops, and
/// destroy blocks in explosions.
fn mob_griefing_enabled() -> bool {
    gamerule_live_value("mobGriefing") != 0
}

/// `forgiveDeadPlayers`: neutral mobs drop a grudge when the player it was
/// held against dies.
fn forgive_dead_players_enabled() -> bool {
    gamerule_live_value("forgiveDeadPlayers") != 0
}

/// `universalAnger`: a provoked neutral mob angers at every nearby player,
/// not just the source of the provocation.
fn universal_anger_enabled() -> bool {
    gamerule_live_value("universalAnger") != 0
}

/// `maxEntityCramming`: the live push engine reads the same atomic; `0`
/// disables cramming damage entirely.
fn max_entity_cramming_limit() -> i32 {
    gamerule_live_value("maxEntityCramming")
}

/// `LivingEntity.pushEntities` cramming arm, mirrored from the live push
/// engine: more than `limit - 1` pushable entities sharing the cell, at least
/// that many not riding anything, and a one-in-four roll — a limit of `0`
/// (or lower) disables the damage.
fn eval_entity_cramming_damage(
    limit: i32,
    pushable: usize,
    non_passengers: usize,
    roll_mod_4: u32,
) -> f32 {
    const ENTITY_CRAMMING_HIT: f32 = 6.0;
    if limit > 0
        && pushable > (limit - 1) as usize
        && roll_mod_4.is_multiple_of(4)
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
const GHAST_FOLLOW_RANGE: f64 = 100.0;

fn ghast_acquires_target(distance: f64) -> bool {
    distance <= GHAST_FOLLOW_RANGE
}

/// `BlazeAttackGoal` hovers the blaze half a block above the target's
/// mid-body (`getY(0.5) + 0.5`).
fn blaze_hover_y(target_pos: [f64; 3], target_height: f64) -> f64 {
    target_pos[1] + target_height * 0.5 + 0.5
}

// --- Phantom: circle / swoop / climb state machine ---

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
#[derive(Debug, PartialEq)]
struct PhantomTick {
    velocity: [f64; 3],
    attack: Option<i32>,
}

const PHANTOM_ATTACK_DAMAGE: f32 = 2.0;

struct PhantomBehavior {
    phase: PhantomPhase,
    anchor: Option<[f64; 3]>,
    clockwise: bool,
    phase_ticks: u16,
}

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

#[derive(Debug, PartialEq, Eq)]
enum PillagerAction {
    Shoot { target: i32 },
}

/// Vanilla `CrossbowItem.getChargeDuration` is 1.25 s = 25 ticks.
const PILLAGER_CROSSBOW_CHARGE_TICKS: u8 = 25;

struct PillagerBehavior {
    charge_ticks: u8,
    #[cfg(test)]
    strafe_clockwise: bool,
}

impl PillagerBehavior {
    /// `RangedCrossbowAttackMob` holds roughly a 10 m ring around the target.
    #[cfg(test)]
    const STRAFE_RANGE: f64 = 12.0;
    #[cfg(test)]
    const STRAFE_MIN: f64 = 4.0;
    #[cfg(test)]
    const STRAFE_SPEED: f64 = 0.5;

    fn new() -> Self {
        Self {
            charge_ticks: 0,
            #[cfg(test)]
            strafe_clockwise: true,
        }
    }

    /// `distance` is the horizontal gap to the visible target; the strafe is
    /// the same shape as the skeleton's, just the pillager's band and speed.
    #[cfg(test)]
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

#[derive(Debug, PartialEq)]
enum EvokerEvent {
    FangAttack { positions: Vec<[i32; 3]> },
    SummonVex { count: u8 },
    Wololo { target: i32 },
}

/// Vanilla `SpellcastingIllager` warmup for the fang spells is 40 ticks.
const EVOKER_SPELL_WARMUP: u8 = 40;
/// The summoning spell's post-cast cooldown.
const EVOKER_VEX_COOLDOWN: u16 = 100;
/// Wololo also rests between casts.
const EVOKER_WOLOLO_COOLDOWN: u16 = 100;
/// The wololo spell reaches blue sheep up to 16 blocks away.
const EVOKER_WOLOLO_RANGE: f64 = 16.0;
const EVOKER_FANG_LINE_COUNT: usize = 16;

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

struct EvokerBehavior {
    cast: Option<(EvokerSpell, u8)>,
    /// Fang-line direction source or the wololo victim position, taken at cast.
    cast_anchor: Option<[f64; 3]>,
    cast_eid: Option<i32>,
    vex_cooldown: u16,
    wololo_cooldown: u16,
}

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

    #[cfg(test)]
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
        let (_, target_pos) = target?;
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

/// `GuardianAttackGoal.getAttackDuration` is 80 ticks (4 s) of beam charge;
/// `ElderGuardian.getAttackDuration` overrides it to 60.
const GUARDIAN_LASER_TICKS: u16 = 80;
const ELDER_GUARDIAN_LASER_TICKS: u16 = 60;
/// The guardian's follow range is 16 blocks.
const GUARDIAN_ATTACK_RANGE: f64 = 16.0;
/// Normal-difficulty beam damage; easy deals 5, hard deals 9.
const GUARDIAN_LASER_DAMAGE: f32 = 6.0;
/// Elder guardian beam damage on normal; easy 5, hard 12.
const ELDER_GUARDIAN_LASER_DAMAGE: f32 = 8.0;
/// The aura reaches every player within 50 blocks every 1200 ticks (60 s).
const ELDER_FATIGUE_RADIUS: f64 = 50.0;
const ELDER_FATIGUE_PERIOD: u16 = 1200;
/// Mining Fatigue III: amplifier 2, applied for 5 minutes = 6000 ticks.
const ELDER_FATIGUE_AMPLIFIER: u8 = 2;
const ELDER_FATIGUE_DURATION: i32 = 6000;

#[derive(Debug, PartialEq)]
pub enum GuardianEvent {
    Laser { target: i32, damage: f32 },
    MiningFatigue { target: i32 },
}

pub struct GuardianBehavior {
    elder: bool,
    attack_ticks: u16,
    elder_effect_pending: bool,
}

impl GuardianBehavior {
    fn new(elder: bool) -> Self {
        Self {
            elder,
            attack_ticks: 0,
            elder_effect_pending: false,
        }
    }

    fn damage(&self) -> f32 {
        if self.elder {
            ELDER_GUARDIAN_LASER_DAMAGE
        } else {
            GUARDIAN_LASER_DAMAGE
        }
    }

    fn laser_ticks(&self) -> u16 {
        if self.elder {
            ELDER_GUARDIAN_LASER_TICKS
        } else {
            GUARDIAN_LASER_TICKS
        }
    }

    /// Beam-charge tail shared by both entry points. `target` is `(eid,
    /// distance)` of a visible victim; the beam needs line of sight the whole
    /// charge, and losing it resets the timer.
    fn laser_events(&mut self, target: Option<(i32, f64)>, events: &mut Vec<GuardianEvent>) {
        let Some((eid, _)) = target.filter(|&(_, d)| d <= GUARDIAN_ATTACK_RANGE) else {
            self.attack_ticks = 0;
            return;
        };
        self.attack_ticks += 1;
        if self.attack_ticks >= self.laser_ticks() {
            self.attack_ticks = 0;
            events.push(GuardianEvent::Laser {
                target: eid,
                damage: self.damage(),
            });
        }
    }

    /// World-clock guardian tick. `entity_id` and `tick_count` reproduce
    /// vanilla's `(tickCount + id) % 1200` aura stagger
    /// (`ElderGuardian.java:66`); `players` lists every player the caller can
    /// see as `(eid, distance, creative_or_spectator)` and the pulse hits all
    /// eligible players within 50 blocks — not just the laser victim.
    pub fn tick_scheduled(
        &mut self,
        entity_id: i32,
        tick_count: u64,
        target: Option<(i32, f64)>,
        players: &[(i32, f64, bool)],
    ) -> Vec<GuardianEvent> {
        let mut events = Vec::new();
        if self.elder
            && (tick_count as i128 + i128::from(entity_id))
                .rem_euclid(i128::from(ELDER_FATIGUE_PERIOD))
                == 0
        {
            self.elder_effect_pending = true;
            for &(eid, distance, exempt) in players {
                if !exempt && distance <= ELDER_FATIGUE_RADIUS {
                    events.push(GuardianEvent::MiningFatigue { target: eid });
                }
            }
        }
        self.laser_events(target, &mut events);
        events
    }

    /// One-shot flag for the vanilla `GUARDIAN_ELDER_EFFECT` game event emitted
    /// on each aura pulse; the packet layer takes it once per pulse.
    pub fn take_elder_effect_event(&mut self) -> bool {
        std::mem::take(&mut self.elder_effect_pending)
    }
}

// --- Warden melee / unreachable-ranged combat plan ---
//
// `WardenBehavior` above owns the vibration-anger loop (0..=150, agitated at
// 80, -1/s decay, +35 per vibration — a sniff contributes the same 35). This
// plan owns the attack choice once a target exists: melee at reach, or the
// sonic boom after 200 ticks without reach — 60-tick charge, then a hit that
// pierces armor and shields and ignores blocks in the way.

#[cfg(test)]
const WARDEN_MELEE_DAMAGE: f32 = 30.0;
#[cfg(test)]
const WARDEN_SONIC_TRIGGER_TICKS: u16 = 200;
#[cfg(test)]
const WARDEN_SONIC_CHARGE_TICKS: u8 = 34;

#[derive(Default)]
#[cfg(test)]
struct WardenCombatPlan {
    unreachable_ticks: u16,
    sonic_charge: u8,
}

#[cfg(test)]
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
#[cfg(test)]
const PROJECTILE_VOID_MARGIN: i32 = 64;
/// `AbstractArrow` ground-life budget in ticks (60 s).
#[cfg(test)]
const ARROW_GROUND_LIFE_TICKS: u32 = 1200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
enum ProjectileFamily {
    Arrow,
    SpectralArrow,
    Trident,
    /// Snowballs, ender pearls, eggs, thrown potions, experience bottles.
    Throwable,
    /// Ghast/blaze/dragon fireballs, wither skulls, wind charges.
    Fireball,
}

#[cfg(test)]
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
#[derive(Clone, Copy, Debug)]
#[cfg(test)]
struct ProjectileTick {
    family: ProjectileFamily,
    in_ground: bool,
    /// A Loyalty trident on the way back to its owner.
    returning_to_owner: bool,
    y: f64,
    min_y: i32,
    chunk_loaded: bool,
}

#[derive(Default)]
#[cfg(test)]
struct ProjectileDespawnPolicy {
    /// `AbstractArrow.inGroundTime`.
    ground_life: u32,
}

#[cfg(test)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EquineKind {
    Horse,
    Donkey,
    Mule,
    Llama,
}

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
    #[cfg(test)]
    fn can_carry_chest(self) -> bool {
        matches!(self, Self::Donkey | Self::Mule | Self::Llama)
    }

    /// `isSaddleable`: horses, donkeys and mules take a saddle. Llamas can
    /// never be saddled in vanilla — their body slot accepts carpets instead.
    #[cfg(test)]
    fn can_wear_saddle(self) -> bool {
        matches!(self, Self::Horse | Self::Donkey | Self::Mule)
    }

    /// `Horse.mobInteract`/`isEquippableInSlot(BODY)`: only horses wear the
    /// four armor tiers.
    #[cfg(test)]
    fn can_wear_armor(self) -> bool {
        matches!(self, Self::Horse)
    }

    /// `HORSE_FOOD` membership (`ItemTags`): wheat, sugar, hay block, apple,
    /// carrot, golden carrot, golden apple, enchanted golden apple. `carrot`
    /// has no `handleEating` branch, so it is food by tag but does nothing.
    #[cfg(test)]
    fn is_horse_food(item: &str) -> bool {
        mob_behavior_item_matches(item, HORSE_FOOD_ITEMS)
    }

    /// `LLAMA_FOOD` membership: wheat and hay block only.
    #[cfg(test)]
    fn is_llama_food(item: &str) -> bool {
        mob_behavior_item_matches(item, LLAMA_FOOD_ITEMS)
    }

    /// Whether the item enters the `fedFood` path for this kind.
    #[cfg(test)]
    fn is_food(self, item: &str) -> bool {
        match self {
            Self::Llama => Self::is_llama_food(item),
            _ => Self::is_horse_food(item),
        }
    }

    /// Vanilla `handleEating` effects, including healing, growth and temper.
    #[cfg(test)]
    fn food_effect(self, item: &str) -> Option<EquineFoodEffect> {
        let item = mob_behavior_id(item);
        EQUINE_FOOD_EFFECTS
            .iter()
            .find(|(kind, names, _)| {
                kind.map_or(self != Self::Llama, |kind| kind == self) && names.contains(&item)
            })
            .map(|(_, _, effect)| *effect)
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg(test)]
struct EquineFoodEffect {
    heal: f32,
    age_up_ticks: i32,
    temper: i32,
    love_food: bool,
}

#[cfg(test)]
impl EquineFoodEffect {
    const fn new(heal: f32, age_up_ticks: i32, temper: i32, love_food: bool) -> Self { Self { heal, age_up_ticks, temper, love_food } }
}

#[cfg(test)]
const EQUINE_FOOD_EFFECTS: &[(Option<EquineKind>, &[&str], EquineFoodEffect)] = &[
    (Some(EquineKind::Llama), &["wheat"], EquineFoodEffect::new(2.0, 10, 3, false)),
    (Some(EquineKind::Llama), &["hay_block"], EquineFoodEffect::new(10.0, 90, 6, true)),
    (None, &["wheat"], EquineFoodEffect::new(2.0, 20, 3, false)),
    (None, &["sugar"], EquineFoodEffect::new(1.0, 30, 3, false)),
    (None, &["hay_block"], EquineFoodEffect::new(20.0, 180, 0, false)),
    (None, &["apple"], EquineFoodEffect::new(3.0, 60, 3, false)),
    (None, &["golden_carrot"], EquineFoodEffect::new(4.0, 60, 5, true)),
    (None, &["golden_apple", "enchanted_golden_apple"], EquineFoodEffect::new(10.0, 240, 10, true)),
];

/// `generateJumpStrength`: `0.4 + 0.2 * (r1 + r2 + r3)` over three unit rolls.
fn equine_generate_jump_strength(rolls: [f64; 3]) -> f64 {
    0.4 + 0.2 * rolls.iter().sum::<f64>()
}

/// `generateSpeed`: `(0.45 + 0.3 * (r1 + r2 + r3)) * 0.25`.
fn equine_generate_move_speed(rolls: [f64; 3]) -> f64 {
    (0.45 + 0.3 * rolls.iter().sum::<f64>()) * 0.25
}

/// Vanilla spawn attribute ranges the breeding formula clamps into.
const EQUINE_JUMP_STRENGTH_RANGE: (f64, f64) = (0.4, 1.0);
const EQUINE_MOVE_SPEED_RANGE: (f64, f64) = (0.1125, 0.3375);
/// `generateMaxHealth`: `15 + rand(8) + rand(9)` bounds the bred health roll.
const EQUINE_MAX_HEALTH_RANGE: (f64, f64) = (15.0, 30.0);

/// Result of one mount-bucking tame check (`RunAroundLikeCrazyGoal.tick`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EquineTameOutcome {
    /// `nextInt(maxTemper) < temper`: the horse tames to its rider.
    Tamed,
    /// The roll failed: temper rises by 5 (clamped) and the rider is thrown.
    Bucked { new_temper: i32 },
}

/// Result of `EquineState::feed`, mirroring `handleEating`/`fedFood`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(test)]
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
#[derive(Clone, Debug)]
struct EquineState {
    kind: EquineKind,
    tamed: bool,
    owner: Option<[u8; 16]>,
    /// Vanilla `temper`; `modify_temper` clamps to `0..=kind.max_temper()`.
    temper: i32,
    #[cfg(test)]
    saddled: bool,
    #[cfg(test)]
    chested: bool,
    /// Horse body armor: 0 none, 1 leather, 2 iron, 3 gold, 4 diamond.
    #[cfg(test)]
    armor_tier: u8,
    /// Vanilla `Llama.strength` clamped to `1..=5` (`Llama.setStrength`); the
    /// chest inventory is `3 * strength` columns. Always `0` on non-llamas.
    #[cfg(test)]
    llama_strength: u8,
    /// Wool carpet on a llama's body slot (`0..=15` dye index). Vanilla has
    /// no carpet slot on Horse/Donkey/Mule, so it is always `None` there.
    #[cfg(test)]
    carpet_color: Option<u8>,
    /// `AbstractChestedHorse.inventory`: the chest contents, allocated only
    /// while `chested` — 15 slots on donkey/mule, `3 * llama_strength` on a
    /// llama, always empty on a horse (which cannot carry a chest).
    #[cfg(test)]
    chest_slots: Vec<Option<(String, u8)>>,
    jump_strength: f64,
    move_speed: f64,
    health: f32,
    max_health: f32,
    /// Vanilla age in ticks: negative while a baby (`ageUp` adds toward 0).
    age_ticks: i32,
    #[cfg(test)]
    in_love: bool,
    /// Set when this record was claimed by the live breed-stat path: the
    /// animal spawned as a foal from two in-love parents rather than from
    /// spawn dice, so `age_ticks` counts the vanilla 24000-tick grow-up.
    bred_foal: bool,
}

impl EquineState {
    /// Base-attribute defaults (`JUMP_STRENGTH` 0.7, `MOVEMENT_SPEED` 0.225)
    /// before spawn rolls; `spawned` applies the rolled ranges instead.
    fn new(kind: EquineKind) -> Self {
        Self {
            kind,
            tamed: false,
            owner: None,
            temper: 0,
            #[cfg(test)]
            saddled: false,
            #[cfg(test)]
            chested: false,
            #[cfg(test)]
            armor_tier: 0,
            #[cfg(test)]
            llama_strength: if kind == EquineKind::Llama { 1 } else { 0 },
            #[cfg(test)]
            carpet_color: None,
            #[cfg(test)]
            chest_slots: Vec::new(),
            jump_strength: 0.7,
            move_speed: 0.225,
            health: 30.0,
            max_health: 30.0,
            age_ticks: 0,
            #[cfg(test)]
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

    #[cfg(test)]
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
    #[cfg(test)]
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

    /// The live `AbstractChestedHorse` chest column count for this kind:
    /// 15 on donkey/mule (`AbstractChestedHorse.getInventoryColumns`), the
    /// llama's `3 * strength` (`Llama.getInventorySize`), 0 on a horse.
    #[cfg(test)]
    fn chest_capacity(&self) -> usize {
        match self.kind {
            EquineKind::Donkey | EquineKind::Mule => 15,
            EquineKind::Llama => 3 * self.llama_strength as usize,
            EquineKind::Horse => 0,
        }
    }

    /// `Llama.setStrength`: `Math.max(1, Math.min(5, value))`, llama only.
    /// Like vanilla `createInventory`, resizing a chested llama re-allocates
    /// the container — slots that no longer fit are dropped from the
    /// inventory, never copied twice.
    #[cfg(test)]
    fn set_llama_strength(&mut self, strength: u8) -> bool {
        if self.kind != EquineKind::Llama {
            return false;
        }
        self.llama_strength = strength.clamp(1, 5);
        if self.chested {
            self.chest_slots.resize(self.chest_capacity(), None);
        }
        true
    }

    /// `isSaddleable` + `equipSaddle`: tamed adult of a saddle-wearing kind
    /// without a saddle already. Llamas have no saddle slot — a tamed llama
    /// still refuses a saddle exactly like vanilla.
    #[cfg(test)]
    fn equip_saddle(&mut self) -> bool {
        if !(self.kind.can_wear_saddle() && self.tamed && !self.is_baby() && !self.saddled) {
            return false;
        }
        self.saddled = true;
        true
    }

    /// `AbstractChestedHorse.equipChest` + `createInventory`: tamed adult of
    /// a chested kind only. Success allocates the exact vanilla slot count —
    /// a second chest is refused before anything is allocated, so the chest
    /// inventory can never be duplicated or re-created over live contents.
    #[cfg(test)]
    fn equip_chest(&mut self) -> bool {
        if !(self.kind.can_carry_chest()
            && self.tamed
            && !self.is_baby()
            && !self.chested)
        {
            return false;
        }
        self.chested = true;
        self.chest_slots = vec![None; self.chest_capacity()];
        true
    }

    /// Horse body armor (`isEquippableInSlot(BODY)` + `equipBodyArmor`):
    /// tiers 1..=4 on a tamed adult horse; vanilla refuses to swap a worn
    /// piece, so equipping requires `armor_tier == 0`.
    #[cfg(test)]
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

    /// `Llama` carpet slot (`Llama.mobInteract` over `ItemTags.WOOL_CARPETS`
    /// plus `setWoolCarpet`): only a tamed adult llama takes a carpet, and
    /// the slot swaps atomically — the previously worn color is returned so
    /// a carpet swap can never lose or duplicate one. `None` removes the
    /// carpet. Every rejection leaves the state untouched.
    #[cfg(test)]
    fn equip_llama_carpet(&mut self, color: Option<u8>) -> Result<Option<u8>, ()> {
        if self.kind != EquineKind::Llama || !self.tamed || self.is_baby() {
            return Err(());
        }
        if color.is_some_and(|c| c >= 16) {
            return Err(());
        }
        Ok(std::mem::replace(&mut self.carpet_color, color))
    }

    /// Chest-inventory write: only into an in-bounds, currently empty slot.
    /// Out-of-range (past `3 * strength` on a llama) or occupied slots fail
    /// closed — never overwriting or losing a stored stack, and never
    /// writing while `!chested` (the vec is empty then anyway).
    #[cfg(test)]
    fn put_chest_slot(&mut self, slot: usize, item: (String, u8)) -> bool {
        let Some(cell) = self.chest_slots.get_mut(slot) else {
            return false;
        };
        if cell.is_some() {
            return false;
        }
        *cell = Some(item);
        true
    }

    /// Removes and returns one stack; out-of-range fails closed to `None`.
    #[cfg(test)]
    fn take_chest_slot(&mut self, slot: usize) -> Option<(String, u8)> {
        self.chest_slots.get_mut(slot)?.take()
    }

    /// `AbstractChestedHorse`/`Llama` death drops as an idempotent drain:
    /// saddle, horse armor, carpet, chest and every stored stack are yielded
    /// exactly once, then the equipment fields are cleared — a second call
    /// returns empty, so the classic donkey/llama death- or portal-dupe
    /// cannot produce the same items twice.
    #[cfg(test)]
    fn collect_death_drops(&mut self) -> Vec<(String, u8)> {
        let mut drops = Vec::new();
        if std::mem::take(&mut self.saddled) {
            drops.push(("saddle".to_string(), 1));
        }
        if let Some(armor) = match self.armor_tier {
            1 => Some("leather_horse_armor"),
            2 => Some("iron_horse_armor"),
            3 => Some("golden_horse_armor"),
            4 => Some("diamond_horse_armor"),
            _ => None,
        } {
            drops.push((armor.to_string(), 1));
        }
        self.armor_tier = 0;
        if let Some(color) = self.carpet_color.take() {
            drops.push((format!("{}_carpet", wool_carpet_color_name(color)), 1));
        }
        if self.chested {
            self.chested = false;
            drops.push(("chest".to_string(), 1));
            for slot in &mut self.chest_slots {
                if let Some(item) = slot.take() {
                    drops.push(item);
                }
            }
            self.chest_slots.clear();
        }
        drops
    }

    /// `canJump`: only a saddled mount answers the jump bar.
    #[cfg(test)]
    fn can_jump(&self) -> bool {
        self.saddled
    }
}

/// `DyeColor` names in vanilla dye-index order — llama carpet items drop as
/// `<color>_carpet`.
#[cfg(test)]
fn wool_carpet_color_name(color: u8) -> &'static str {
    match color {
        0 => "white",
        1 => "orange",
        2 => "magenta",
        3 => "light_blue",
        4 => "yellow",
        5 => "lime",
        6 => "pink",
        7 => "gray",
        8 => "light_gray",
        9 => "cyan",
        10 => "purple",
        11 => "blue",
        12 => "brown",
        13 => "green",
        14 => "red",
        _ => "black",
    }
}

/// `AbstractHorse.onPlayerJump` charge → `playerJumpPendingScale`: negatives
/// clamp to 0, >=90 is full power, below that it ramps 0.4..0.8 linearly —
/// `0.4 + 0.4 * charge / 90` — the exact vanilla formula.
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
fn equine_jump_velocity(jump_strength: f64, charge_0_to_100: i32) -> f64 {
    jump_strength * equine_jump_scale(charge_0_to_100)
}

/// `AbstractHorse.createOffspringAttribute`: clamp both parents into the
/// attribute range, average them, then perturb by `spread * draw` where
/// `spread` is the parent gap plus 30% of the range on each side, and `draw`
/// is vanilla's `(r1+r2+r3)/3 - 0.5`. `rand_unit` supplies that draw as one
/// [0,1) value (`draw = rand_unit - 0.5`); out-of-range results reflect once
/// back inside, exactly like vanilla.
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
const PERCH_COOLDOWN_TICKS: u16 = 100;
/// `removeEntitiesOnShoulder` refuses to drop a parrot that has perched for
/// fewer than 20 ticks (`timeEntitySatOnShoulder + 20 < gameTime`).
const SHOULDER_MIN_PERCH_TICKS: u32 = 20;

/// Per-entity parrot state.
#[derive(Clone, Debug)]
pub struct ParrotState {
    tamed: bool,
    owner: Option<[u8; 16]>,
    /// `Parrot.Variant` id (cosmetic; 0..=4 vanilla variants plus the rare 5th).
    #[cfg(test)]
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

impl ParrotState {
    fn new(_variant: u8) -> Self {
        Self {
            tamed: false,
            owner: None,
            #[cfg(test)]
            variant: _variant,
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
    pub fn is_parrot_food(item: &str) -> bool {
        mob_behavior_item_matches(item, PARROT_FOOD_ITEMS)
    }

    /// `PARROT_POISONOUS_FOOD` membership: cookies only.
    pub fn is_parrot_poisonous_food(item: &str) -> bool {
        mob_behavior_id(item) == "cookie"
    }

    /// `Parrot.mobInteract` in vanilla order: untamed + parrot food eats and
    /// rolls `nextInt(10) == 0` to tame; any non-poisonous item on a tamed,
    /// owned, grounded parrot toggles the sit order; poisonous food (cookie)
    /// is consumed, applies poison and deals `Float.MAX_VALUE` damage —
    /// fatal for the parrot.
    pub fn mob_interact(
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParrotInteract {
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
const MOSS_HATCH_CREDIT: u32 = 2;
/// Incubation credit per tick on a sand substrate during the night window —
/// models the turtle egg's always-crack night ticks vs the rare day chance.
const NIGHT_HATCH_CREDIT: u32 = 8;
/// Stage boundaries before hatch: vanilla eggs show two crack textures then
/// hatch on the third qualifying tick (`HATCH` 0 -> 1 -> 2 -> hatch).
const EGG_CRACK_STAGES: u8 = 3;
/// Vanilla `SnifferEggBlock`: 24000 ticks (20 min) on ordinary blocks,
/// halved to 12000 (10 min) on moss — modeled as 8000/stage at 1x credit.
const SNIFFER_EGG_TICKS_PER_STAGE: u32 = 8_000;
/// Turtle-egg day-rate stage cost; the night credit makes the night window
/// dominant, matching vanilla's ~95% night-window hatching.
const TURTLE_EGG_TICKS_PER_STAGE: u32 = 9_600;

/// Environment observations an `EggSpec` can require or an egg can sit in.
#[derive(Clone, Debug, PartialEq, Eq)]
enum IncubationEnv {
    /// Sand-family substrate; incubation accelerates while `is_night` holds
    /// (the turtle egg's night window).
    SandNight,
    /// Moss-family substrate; doubles incubation credit (sniffer egg).
    MossBlock,
    /// Modded: the egg sits in or adjacent to fire/lava.
    #[cfg(test)]
    OnFire,
    /// Modded: the egg is submerged.
    #[cfg(test)]
    SubmergedWater,
    /// Modded: a thunderstorm is active.
    #[cfg(test)]
    Thunderstorm,
    /// Any other mod-defined environment tag, matched by exact string.
    Custom(String),
}

/// Declarative hatch recipe for one egg block.
struct EggSpec {
    /// Stable spec id, conventionally the egg block id.
    #[cfg(test)]
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

impl EggSpec {
    /// Vanilla `TurtleEggBlock`: sand substrate only, hatches a turtle; the
    /// night window accelerates via `NIGHT_HATCH_CREDIT`.
    fn turtle_egg() -> Self {
        Self {
            #[cfg(test)]
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
            #[cfg(test)]
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
struct IncubationState {
    spec: EggSpec,
    /// Completed stage boundaries, `0..spec.stages`.
    stage: u8,
    /// Accumulated credit inside the current stage.
    ticks_in_stage: u32,
    /// Planter id, emitted on hatch only when `spec.inherit_owner`.
    owner: Option<[u8; 16]>,
}

impl IncubationState {
    fn new(spec: EggSpec, owner: Option<[u8; 16]>) -> Self {
        Self {
            spec,
            stage: 0,
            ticks_in_stage: 0,
            owner,
        }
    }

    #[cfg(test)]
    fn stage(&self) -> u8 {
        self.stage
    }

    fn hatched(&self) -> bool {
        self.stage >= self.spec.stages
    }
}

/// What one `tick_incubation` call produced.
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
fn live_incubations() -> &'static Mutex<HashMap<(DimId, BlockPos), IncubationState>> {
    static INCUBATIONS: OnceLock<Mutex<HashMap<(DimId, BlockPos), IncubationState>>> =
        OnceLock::new();
    INCUBATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The substrate observation an egg block sits on: sand-family below maps to
/// `SandNight`, moss to `MossBlock`; anything else is a plain custom tag that
/// satisfies only `required_env: None` specs.
fn incubation_env_below(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    pos: BlockPos,
) -> IncubationEnv {
    match reg.name_of(block_at_cached_dim(dim, pos.0, pos.1 - 1, pos.2)) {
        "minecraft:sand" | "minecraft:red_sand" => IncubationEnv::SandNight,
        "minecraft:moss_block" | "minecraft:moss_carpet" => IncubationEnv::MossBlock,
        name => IncubationEnv::Custom(name.to_string()),
    }
}

/// `randomTick` for `turtle_egg` and `sniffer_egg` blocks. Returns `true`
/// once the block name is claimed (even when the egg simply accrues credit).
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
fn egg_incubation_remove(dim: DimId, pos: BlockPos) {
    live_incubations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(dim, pos));
}

/// `AgeableMob.setAge(6000)` on both parents after `getBreedOffspring`.
const ANIMAL_BREED_COOLDOWN_TICKS: i32 = 6_000;
/// The baby lands at the pair midpoint; 6 blocks covers every breed-range
/// pairing the live AI allows.
const ANIMAL_BREED_BABY_RADIUS_SQ: f64 = 6.0 * 6.0;

fn live_animal_breed_states() -> &'static Mutex<HashMap<i32, AnimalBreedState>> {
    static STATES: OnceLock<Mutex<HashMap<i32, AnimalBreedState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Default)]
struct AnimalBreedState {
    /// `love` observed on the previous pass; a `>0 -> 0` edge is the
    /// `live_mob_breed` consumption signature.
    last_love: i32,
    /// Ticks left of the vanilla parental breeding cooldown.
    cooldown: i32,
}

/// Parental breeding cooldown for the generic `Animal` family. The live
/// `live_mob_breed` path consumes both parents' `love` and inserts the
/// offspring in one step; this pass detects that edge, confirms it with the
/// newborn appearing beside a consumed parent, and applies the vanilla
/// `setAge(6000)` lockout — while cooling down, love the feed path grants is
/// suppressed exactly like `isReadyToBreed == false` refuses `setInLove`.
pub(crate) fn live_animal_breed_cooldown_tick() {
    let mobs_guard = mobs().lock().unwrap_or_else(|e| e.into_inner());
    let alive: HashSet<i32> = mobs_guard.iter().map(|mob| mob.eid).collect();
    let mut states = live_animal_breed_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    states.retain(|eid, _| alive.contains(eid));
    // On the very first pass every mob looks newborn; breed detection only
    // makes sense once the tracker has seen a previous tick.
    let primed = !states.is_empty();
    let mut consumed: Vec<(i32, i64, DimId, [f64; 3])> = Vec::new();
    let mut newborns: Vec<(i64, DimId, [f64; 3])> = Vec::new();
    for mob in mobs_guard.iter() {
        let is_new = primed && !states.contains_key(&mob.eid);
        let love = *mob.love.lock().unwrap_or_else(|e| e.into_inner());
        let state = states.entry(mob.eid).or_default();
        if state.cooldown > 0 {
            state.cooldown -= 1;
            if love > 0 {
                *mob.love.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            }
        }
        let effective_love = if state.cooldown > 0 { 0 } else { love };
        if state.last_love > 0 && effective_love == 0 {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            consumed.push((mob.eid, mob.etype, mob.dim, [x, y, z]));
        }
        state.last_love = effective_love;
        if is_new {
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            newborns.push((mob.etype, mob.dim, [x, y, z]));
        }
    }
    // Only a consumed parent with a same-species newborn beside it actually
    // bred — a love timer expiring on its own earns no cooldown.
    if !primed {
        return;
    }
    for &(eid, etype, dim, pos) in &consumed {
        let bred = newborns.iter().any(|&(netype, ndim, npos)| {
            netype == etype
                && ndim == dim
                && (npos[0] - pos[0]).powi(2)
                    + (npos[1] - pos[1]).powi(2)
                    + (npos[2] - pos[2]).powi(2)
                    <= ANIMAL_BREED_BABY_RADIUS_SQ
        });
        if bred {
            if let Some(state) = states.get_mut(&eid) {
                state.cooldown = ANIMAL_BREED_COOLDOWN_TICKS;
            }
        }
    }
}

/// Vanilla `Zombie.aiStep` drowning conversion: eyes in water for 600 ticks
/// (30 s) starts `startUnderWaterConversion`, a 300-tick (15 s) shake window,
/// then `doUnderWaterConversion` swaps the zombie for a Drowned. Husk shares
/// the `Zombie` drowning path; zombie villagers and drowned never convert.
const ZOMBIE_DROWN_SUBMERGED_TICKS: u64 = 600;
const ZOMBIE_DROWN_CONVERSION_TICKS: u32 = 300;

struct ZombieDrownState {
    submerged: u64,
    converting: u32,
}

fn live_zombie_drown_states() -> &'static Mutex<HashMap<i32, ZombieDrownState>> {
    static STATES: OnceLock<Mutex<HashMap<i32, ZombieDrownState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-tick driver for the zombie drowning conversion. Conversion runs to
/// completion once started even if the zombie leaves the water, matching
/// vanilla's `isUnderWaterConverting` latch.
pub(crate) fn live_zombie_drown_tick() {
    type MobPos = (f64, f64, f64, f32, f32);
    let mut converted: Vec<(i32, DimId, MobPos)> = Vec::new();
    {
        let mobs_guard = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let alive: HashSet<i32> = mobs_guard.iter().map(|mob| mob.eid).collect();
        let mut states = live_zombie_drown_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        states.retain(|eid, _| alive.contains(eid));
        for mob in mobs_guard.iter() {
            let is_zombie = entity_spec_by_id(mob.etype)
                .is_some_and(|spec| matches!(spec.name, "minecraft:zombie" | "minecraft:husk"));
            if !is_zombie {
                continue;
            }
            let (x, y, z, yaw, pitch) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            let state = states.entry(mob.eid).or_insert(ZombieDrownState {
                submerged: 0,
                converting: 0,
            });
            if state.converting > 0 {
                state.converting -= 1;
                if state.converting == 0 {
                    converted.push((mob.eid, mob.dim, (x, y, z, yaw, pitch)));
                }
                continue;
            }
            if mob_drowning_submerged_dim(mob.dim, mob.etype, x, y, z) {
                state.submerged += 1;
                if state.submerged > ZOMBIE_DROWN_SUBMERGED_TICKS {
                    state.submerged = 0;
                    state.converting = ZOMBIE_DROWN_CONVERSION_TICKS;
                }
            } else {
                state.submerged = 0;
            }
        }
    }
    for (eid, dim, pos) in converted {
        if remove_live_mob(eid).is_some() {
            let _ = live_spawn_named(dim, "drowned", pos);
        }
    }
}

#[cfg(test)]
mod mob_behaviors_tests {
    use super::*;

    #[test]
    fn piglin_safety_follows_the_dimension_type_flag() {
        // Vanilla `DimensionType.piglinSafe` marks only the Nether among the
        // built-in dimensions; custom dimensions declare their own flag.
        assert!(dimension_piglin_safe(DimId::Nether));
        assert!(!dimension_piglin_safe(DimId::Overworld));
        assert!(!dimension_piglin_safe(DimId::End));
    }

    #[test]
    fn dispatch_selects_the_protocol_767_behavior() {
        for etype in [8, 45, 78, 51, 33, 88, 31, 93, 67, 118, 122, 15, 75, 119, 116] {
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
                MobBehaviorState::Parrot(pet) if pet.kind == PetKind::Parrot => "minecraft:parrot",
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
        for _ in 0..118 {
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
        assert_eq!(state.step(&ctx), [WardenRoar]);
        ctx.nearest_target.as_mut().unwrap().eid = 8;
        assert!(state.step(&ctx).is_empty());
        let target = ctx.nearest_target.as_mut().unwrap();
        target.eid = 7;
        target.visible = false; // The sonic attack can pass through blocks.
        assert_eq!(state.step(&ctx), [WardenSonicCharge]);
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
        for _ in 0..39 {
            assert!(state.step(&ctx).is_empty());
        }
        assert_eq!(state.step(&ctx), [WardenSonicCharge]);
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
        let _guard = crate::world_guard();
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
        let _guard = crate::world_guard();
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
    fn piglin_barter_consumes_one_ingot_and_resolves_at_tick_119() {
        let mut piglin = PiglinBehavior::default();
        assert!(!piglin.offer(3, "gold_nugget"));
        assert!(piglin.offer(3, "minecraft:gold_ingot"));
        assert!(!piglin.offer(4, "gold_ingot"));
        for _ in 0..118 {
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
            target_pos: None,
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
    fn v1211_veh_ai_enderman_water_escape_runs_through_live_step() {
        let mut state = MobBehaviorState::for_etype(33).unwrap();
        let mut ctx = dispatch_ctx();
        ctx.in_water = true;
        let mut block_at = |[_, y, _]: [i32; 3]| EndermanTeleportBlock {
            blocks_motion: y == 48,
            liquid: false,
        };
        let mut can_occupy = |[_, y, _]: [f64; 3]| y == 49.0;
        // Water damage forces the enderman escape teleport through the live
        // world-aware step, exactly like the rain path.
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
    fn v1211_veh_ai_blaze_fireball_runs_through_live_step() {
        use MobBehaviorEffect::*;

        let mut state = MobBehaviorState::for_etype(8).unwrap();
        let ctx = dispatch_ctx();
        // The same Idle->Charging->Burst cycle the live mob_tick drives emits
        // a small fireball launch through step_with_world.
        let mut saw_fireball = false;
        for _ in 0..140 {
            for effect in state.step_with_world(&ctx, MobBehaviorWorldTick::Basic) {
                if let LaunchProjectile {
                    kind: MobProjectileKind::SmallFireball,
                    ..
                } = effect
                {
                    saw_fireball = true;
                }
            }
        }
        assert!(saw_fireball, "blaze never fired through the live step");
    }

    #[test]
    fn v1211_veh_ai_dragon_heals_only_from_spike_anchored_crystals() {
        use MobBehaviorEffect::*;

        let old_seed = WORLD_SEED.swap(4242, Ordering::Relaxed);
        let pillars = end_crystal_pillars(4242);
        let anchor = end_crystal_spawn_position(&pillars[0]);
        // A crystal entity on the pillar anchor feeds the dragon's regen link;
        // a stray crystal inside the same 32-block window must not.
        let anchored_xtal = DragonCrystal {
            entity: 1,
            position: anchor,
        };
        let stray_xtal = DragonCrystal {
            entity: 2,
            position: [anchor[0] + 5.0, anchor[1], anchor[2]],
        };
        let mut ctx = dispatch_ctx();
        ctx.dim = DimId::End;
        ctx.pos = anchor;
        ctx.hp = 100.0;
        ctx.max_hp = 200.0;

        // Stray-only crystals: no pillar anchor, so no heal link ever forms.
        let mut stray_only = MobBehaviorState::for_etype(31).unwrap();
        for _ in 0..15 {
            assert!(
                !stray_only
                    .step_with_world(
                        &ctx,
                        MobBehaviorWorldTick::EnderDragon {
                            portal_y: 80,
                            path_complete: false,
                            strafe_aligned: false,
                            crystals: &[stray_xtal],
                        },
                    )
                    .iter()
                    .any(|e| matches!(e, HealSelf(_))),
                "unanchored crystal healed the dragon"
            );
        }

        // Anchored crystal: the link forms and the tenth fight tick heals.
        let both = [stray_xtal, anchored_xtal];
        let mut anchored = MobBehaviorState::for_etype(31).unwrap();
        let mut healed = false;
        for _ in 0..15 {
            healed |= anchored
                .step_with_world(
                    &ctx,
                    MobBehaviorWorldTick::EnderDragon {
                        portal_y: 80,
                        path_complete: false,
                        strafe_aligned: false,
                        crystals: &both,
                    },
                )
                .iter()
                .any(|e| matches!(e, HealSelf(_)));
        }
        assert!(healed, "spike-anchored crystal never healed the dragon");
        WORLD_SEED.store(old_seed, Ordering::Relaxed);
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
        assert_eq!(dragon.tick(dragon_tick(&[])), None);
        assert_eq!(dragon.phase, DragonPhase::SittingFlaming);
        assert!(!dragon.hurt(BossHit::Arrow, 30.0));

        for count in 1..=199 {
            let event = dragon.tick(dragon_tick(&[]));
            assert_eq!(event, (count == 10).then_some(DragonEvent::FlameBreath));
        }
        assert_eq!(dragon.tick(dragon_tick(&[])), None);
        assert_eq!(dragon.phase, DragonPhase::SittingScanning);
        assert!(!dragon.hurt(BossHit::Arrow, 30.0));
        for count in 1..=26 {
            let mut tick = dragon_tick(&[]);
            tick.strafe_target = Some(12);
            tick.target_pos = Some([12.0, 80.0, 0.0]);
            assert_eq!(
                dragon.tick(tick),
                (count == 26).then_some(DragonEvent::PerchAttack)
            );
        }
        assert_eq!(dragon.phase, DragonPhase::SittingAttacking);
        assert!(!dragon.hurt(BossHit::Arrow, 30.0));
        assert!(dragon.hurt(BossHit::Other, 1.0));

        for _ in 0..40 {
            assert_eq!(dragon.tick(dragon_tick(&[])), None);
        }
        assert_eq!(dragon.phase, DragonPhase::SittingFlaming);
        assert_eq!(dragon.flame_count, 2);
        for _ in 0..2 {
            for _ in 0..200 {
                dragon.tick(dragon_tick(&[]));
            }
            for _ in 0..26 {
                let mut tick = dragon_tick(&[]);
                tick.target_pos = Some([12.0, 80.0, 0.0]);
                dragon.tick(tick);
            }
            for _ in 0..40 {
                dragon.tick(dragon_tick(&[]));
            }
        }
        assert_eq!(dragon.flame_count, 4);
        for _ in 0..200 {
            dragon.tick(dragon_tick(&[]));
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

        // `Wolf.actuallyHurt`: armor absorbs ordinary hits for `ceil(amount)`
        // durability while `#minecraft:bypasses_wolf_armor` types hit HP.
        let armored = PetBehavior::new(PetKind::Wolf);
        assert_eq!(armored.hp_damage_after_armor("player_attack", 7.0, Some(64)), 0.0);
        assert_eq!(armored.hp_damage_after_armor("arrow", 3.5, Some(20)), 0.0);
        assert_eq!(armored.hp_damage_after_armor("drown", 2.0, Some(64)), 2.0);
        assert_eq!(armored.hp_damage_after_armor("magic", 4.0, Some(64)), 4.0);
        assert_eq!(armored.hp_damage_after_armor("starve", 1.0, Some(64)), 1.0);
        assert_eq!(armored.hp_damage_after_armor("player_attack", 7.0, Some(0)), 7.0);
        assert_eq!(armored.hp_damage_after_armor("player_attack", 7.0, None), 7.0);
        let cat = PetBehavior::new(PetKind::Cat);
        assert_eq!(cat.hp_damage_after_armor("player_attack", 7.0, Some(64)), 7.0);
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
    fn parrot_pet_has_6_hp_and_tames_seed_food_on_a_tenth_roll() {
        let owner = [1; 16];
        let mut parrot = PetBehavior::new(PetKind::Parrot);
        // `Parrot.createAttributes`: max health 6.0.
        assert_eq!(parrot.max_hp(), 6.0);
        assert_eq!(parrot.hp, 6.0);
        // `PARROT_FOOD` is seeds/pods only; cookies are poisonous, not food.
        for food in [
            "wheat_seeds",
            "melon_seeds",
            "pumpkin_seeds",
            "beetroot_seeds",
            "torchflower_seeds",
            "pitcher_pod",
        ] {
            assert_eq!(
                PetBehavior::new(PetKind::Parrot).try_tame(owner, food, &mut highest),
                PetTameResult::Consumed,
                "{food}"
            );
        }
        assert_eq!(
            parrot.try_tame(owner, "cookie", &mut no_draw),
            PetTameResult::Pass
        );
        assert_eq!(
            parrot.try_tame(owner, "bread", &mut no_draw),
            PetTameResult::Pass
        );
        // `nextInt(10) == 0` tames; every other roll consumes the seed.
        assert_eq!(
            parrot.try_tame(owner, "minecraft:pitcher_pod", &mut highest),
            PetTameResult::Consumed
        );
        assert_eq!(parrot.owner, None);
        assert_eq!(
            parrot.try_tame(owner, "torchflower_seeds", &mut zero),
            PetTameResult::Tamed
        );
        assert_eq!((parrot.max_hp(), parrot.hp), (6.0, 6.0));
        assert_eq!(parrot.toggle_sitting(owner), Some(false));
        // A tamed parrot no longer eats food for taming.
        assert_eq!(
            parrot.try_tame(owner, "wheat_seeds", &mut no_draw),
            PetTameResult::Pass
        );
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
        assert!(!wither.immune_to(BossHit::Trident));
        assert_eq!(wither.hurt(BossHit::Arrow, 1.0), None);
        assert_eq!(wither.hp, 150.0);
        assert!(wither.has_wither_armor());
        assert_eq!(wither.hurt(BossHit::Arrow, 500.0), None);
        assert_eq!(wither.hurt(BossHit::Trident, 500.0), None);
        assert_eq!(wither.hp, 150.0);
        // `WitherBoss.java:463-467`: the 50% wither armor also shrugs off
        // wind-charge projectiles.
        assert!(wither.immune_to(BossHit::WindCharge));
        assert_eq!(wither.hurt(BossHit::WindCharge, 500.0), None);
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
        for anger in [35, 70] {
            assert_eq!(warden.anger.add_vibration(target), anger);
        }
        assert_eq!(warden.anger.add_vibration(8), 35);
        for _ in 0..500 {
            assert_eq!(warden.tick(None), None);
        }
        assert_eq!(warden.anger.anger(target), 45);
        assert_eq!(warden.anger.anger(8), 10);
        assert!(!warden.anger.agitated(target));
        assert!(warden.target.is_none());
        assert_eq!(warden.anger.add_vibration(target), 80);
        assert!(warden.anger.agitated(target));
        // Eighty anger is already the vanilla target-lock threshold.
        assert_eq!(warden.tick(None), Some(WardenEvent::Roar { target }));
        assert_eq!(warden.target, Some(target));
        assert_eq!(warden.anger.add_vibration(target), 115);
        assert_eq!(warden.anger.add_vibration(target), 150);

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
        let _guard = crate::world_guard();
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

    #[test]
    fn snow_golem_snowball_arc_compensates_and_hits_blazes_for_three() {
        // A target 10 blocks east, feet level with the golem: vanilla adds
        // 0.2 * horizontal distance of arc lift to the eye-line delta.
        let velocity = snow_golem_snowball_velocity([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], 1.74);
        let speed = mob_behavior_dot(velocity, velocity).sqrt();
        assert!((speed - 1.6).abs() < 1e-12, "snowball speed {speed}");
        // dy = 1.74 - 1.1 - 1.6 = -0.96 before the +2.0 arc compensation.
        assert!(velocity[1] > 0.0, "arc lift must push the throw up");
        assert!(velocity[0] > 0.0 && velocity[2].abs() < 1e-12);
        // A directly overhead target still yields a unit 1.6-speed throw.
        let up = snow_golem_snowball_velocity([0.0, 0.0, 0.0], [0.0, 5.0, 0.0], 1.62);
        assert!((mob_behavior_dot(up, up).sqrt() - 1.6).abs() < 1e-12);

        // Snowballs deal 3 damage only to Blazes; everything else takes a
        // 0-damage hurt that still carries the standard knockback.
        assert_eq!(snowball_hit_effect("minecraft:blaze"), (3.0, true));
        assert_eq!(snowball_hit_effect("blaze"), (3.0, true));
        assert_eq!(snowball_hit_effect("minecraft:zombie"), (0.0, true));
        assert_eq!(snowball_hit_effect("creeper"), (0.0, true));
    }

    #[test]
    fn skeleton_arrow_arc_uses_third_height_aim_and_sixteenth_speed() {
        // A target 10 blocks east: the arrow leaves the eye line minus 0.1,
        // aims at target.getY(1/3) = 0.6, and lifts 0.2 * horizontal distance.
        let velocity = skeleton_arrow_velocity([0.0, 1.0, 0.0], [10.0, 1.0, 0.0], 0.6, 1.74);
        let speed = mob_behavior_dot(velocity, velocity).sqrt();
        assert!((speed - 1.6).abs() < 1e-12, "arrow speed {speed}");
        // dy = 1.6 - (1 + 1.74 - 0.1) = -1.04, then +2.0 arc compensation.
        assert!(velocity[1] > 0.0, "arc lift must push the arrow up");
        assert!(velocity[0] > 0.0 && velocity[2].abs() < 1e-12);
        // Eye-height and aim offsets shift the trajectory deterministically.
        let high_aim = skeleton_arrow_velocity([0.0, 1.0, 0.0], [10.0, 1.0, 0.0], 1.2, 1.74);
        assert!(high_aim[1] > velocity[1], "a higher aim line lifts the shot");
    }

    #[test]
    fn dragon_v1211_multipart_breath_gateways_and_respawn_ritual() {
        assert_eq!(dragon_part_damage(DragonCombatPart::Head, 20.0), 20.0);
        assert_eq!(dragon_part_damage(DragonCombatPart::Body, 20.0), 6.0);
        assert_eq!(dragon_part_damage(DragonCombatPart::Wing, 0.0), 1.0);
        assert!(dragon_wing_knockback([0.0, 64.0, 0.0], [2.0, 64.5, 0.0]).is_some());
        assert!(dragon_wing_knockback([0.0, 64.0, 0.0], [4.0, 64.5, 0.0]).is_none());

        let mut cloud = DragonBreathCloud::sitting_flame([0.0, 80.0, 0.0]);
        assert_eq!(cloud.radius, 5.0);
        assert_eq!(cloud.left, 200);
        assert_eq!(cloud.scoop("glass_bottle"), Some("dragon_breath"));
        assert_eq!(cloud.radius, 4.5);
        assert_eq!(cloud.scoop("water_bucket"), None);

        let gateways = end_gateway_ring_positions(25, 75);
        assert_eq!(gateways.len(), 20, "the end gateway ring caps at twenty");
        for [x, y, z] in gateways {
            assert_eq!(y, 75);
            let radius = ((x * x + z * z) as f64).sqrt();
            assert!((radius - 96.0).abs() <= 1.0, "gateway radius {radius}");
        }

        let crystals = dragon_respawn_crystal_positions(80);
        assert_eq!(crystals.len(), 4);
        assert!(DragonRespawnRitual::start(80, &crystals[..3]).is_none());
        let mut ritual = DragonRespawnRitual::start(80, &crystals).unwrap();
        assert_eq!(ritual.tick(), vec![DragonRespawnEvent::CloseExitPortal]);
        let mut beams = 0;
        let mut restores = 0;
        let mut spawned = false;
        for _ in 1..101 {
            for event in ritual.tick() {
                match event {
                    DragonRespawnEvent::BeamAt { .. } => beams += 1,
                    DragonRespawnEvent::RestoreCrystal { .. } => restores += 1,
                    DragonRespawnEvent::SpawnDragon => spawned = true,
                    DragonRespawnEvent::CloseExitPortal => unreachable!(),
                }
            }
        }
        assert_eq!((beams, restores, spawned), (4, 4, true));
    }

    #[test]
    fn dragon_scanning_far_target_enters_charging_player_then_holding() {
        let mut dragon = EnderDragonBehavior::new(false);
        dragon.set_phase(DragonPhase::SittingScanning);
        for _ in 0..100 {
            let mut tick = dragon_tick(&[]);
            tick.strafe_target = Some(12);
            tick.target_pos = Some([60.0, 80.0, 0.0]);
            assert_eq!(dragon.tick(tick), None);
        }
        assert_eq!(dragon.phase, DragonPhase::ChargingPlayer);
        assert_eq!(dragon.charge_target, Some([60.0, 80.0, 0.0]));
        for _ in 0..20 {
            let mut tick = dragon_tick(&[]);
            tick.target_pos = Some([100.0, 80.0, 0.0]);
            assert_eq!(dragon.tick(tick), None);
        }
        assert_eq!(dragon.charge_target, Some([60.0, 80.0, 0.0]));
        assert_eq!(dragon.phase, DragonPhase::ChargingPlayer);
        for _ in 0..11 {
            let mut tick = dragon_tick(&[]);
            tick.position = [55.0, 80.0, 0.0];
            assert_eq!(dragon.tick(tick), None);
        }
        assert_eq!(dragon.phase, DragonPhase::HoldingPattern);
    }

    #[test]
    fn wither_v1211_head_cadence_skull_cage_and_star_rules() {
        let mut wither = WitherBehavior::new();
        assert!(!wither.observe_health(99.0));
        wither.spawn_ticks = 0;
        for tick in 1..=20 {
            assert_eq!(wither.observe_health(98.0), tick == 20);
        }
        assert!(!wither.observe_health(98.0));
        assert_eq!(wither_head_cadence(WitherHead::Center), 20);
        assert_eq!(wither_head_cadence(WitherHead::Left), 40);
        assert_eq!(wither_head_cadence(WitherHead::Right), 40);
        assert!(wither_skull_breaks_block(true, "obsidian"));
        assert!(wither_skull_breaks_block(true, "ancient_debris"));
        for protected in ["bedrock", "end_portal", "end_portal_frame"] {
            assert!(!wither_skull_breaks_block(true, protected), "{protected}");
        }
        assert!(!wither_skull_breaks_block(false, "stone"));
        assert!(nether_star_ignores_explosions("minecraft:nether_star"));
        assert!(!nether_star_ignores_explosions("diamond"));

        let cleared = wither_cage_clear([10, 64, 10], |pos| pos != [10, 64, 10]);
        assert_eq!(cleared.len(), 35, "3x4x3 has 36 cells; one unbreakable remains");
        assert!(cleared.contains(&[9, 64, 9]));
        assert!(cleared.contains(&[11, 67, 11]));
        assert!(!cleared.contains(&[10, 64, 10]));
    }

    #[test]
    fn breeze_is_registered_in_live_behavior_factory() {
        assert!(
            matches!(MobBehaviorState::for_etype(12), Some(MobBehaviorState::Breeze(_))),
            "entity type 12 (breeze) must execute wind-charge behavior instead of idling"
        );
    }

    #[test]
    fn warden_v1211_lifecycle_anger_darkness_and_damage_rules() {
        let mut lifecycle = WardenLifecycle::emerging();
        for tick in 1..=134 {
            let event = lifecycle.tick(false, false);
            assert_eq!(event, (tick == 134).then_some(WardenLifecycleEvent::Emerged));
        }
        assert_eq!(lifecycle.phase, WardenLifecyclePhase::Sniffing(84));
        assert_eq!(lifecycle.tick(true, true), Some(WardenLifecycleEvent::Roar));
        for tick in 1..=84 {
            let event = lifecycle.tick(true, true);
            assert_eq!(event, (tick == 84).then_some(WardenLifecycleEvent::Attack));
        }
        assert_eq!(lifecycle.phase, WardenLifecyclePhase::Attacking);

        lifecycle.phase = WardenLifecyclePhase::Investigating;
        lifecycle.calm_ticks = 1199;
        assert_eq!(lifecycle.tick(false, false), Some(WardenLifecycleEvent::Dig));
        assert_eq!(lifecycle.phase, WardenLifecyclePhase::Digging(100));
        assert_eq!(lifecycle.tick(true, false), Some(WardenLifecycleEvent::Resurface));
        assert_eq!(lifecycle.phase, WardenLifecyclePhase::Emerging(134));

        assert_eq!(warden_melee_damage(1), 16.0);
        assert_eq!(warden_melee_damage(2), 30.0);
        assert_eq!(warden_melee_damage(3), 45.0);
        assert_eq!(warden_darkness_pulse(120, [20.0, 20.0, 20.0]), None);
        assert_eq!(warden_darkness_pulse(120, [0.0, 0.0, 0.0]), Some(260));
        assert_eq!(warden_darkness_pulse(121, [0.0, 0.0, 0.0]), None);
        assert_eq!(warden_darkness_pulse(240, [20.1, 0.0, 0.0]), None);
        // Sphere, not cube: a corner inside the old 20³ box is out of range.
        assert_eq!(warden_darkness_pulse(120, [12.0, 12.0, 12.0]), None);
        assert_eq!(warden_darkness_pulse(120, [11.0, 11.0, 11.0]), Some(260));

        let mut anger = WardenAngerManagement::default();
        assert_eq!(anger.add_vibration(7), 35);
        assert_eq!(anger.add_direct_hit(7), 55);
        assert_eq!(anger.roar_target(), None);
        assert_eq!(anger.add_vibration(7), 90);
        assert!(anger.roar_target() == Some(7));
    }

    #[test]
    fn guardian_breeze_and_bogged_v1211_signature_rules() {
        assert_eq!(guardian_spikes_reflect_damage(true, true), 2.0);
        assert_eq!(guardian_spikes_reflect_damage(true, false), 0.0);
        assert_eq!(guardian_spikes_reflect_damage(false, true), 0.0);

        let mut breeze = BreezeBehavior::new();
        let first = breeze.tick([0.0, 64.0, 0.0], Some((5, [12.0, 64.0, 0.0])), true);
        assert!(matches!(first, Some(BreezeEvent::LongJump { .. })), "{first:?}");
        for _ in 0..8 {
            assert!(matches!(
                breeze.tick([0.0, 64.0, 0.0], Some((5, [12.0, 64.0, 0.0])), false),
                Some(BreezeEvent::LongJump { .. })
            ));
        }
        assert!(matches!(
            breeze.tick([0.0, 64.0, 0.0], Some((5, [12.0, 64.0, 0.0])), false),
            Some(BreezeEvent::Slide { .. })
        ));
        assert!(matches!(
            breeze.reflect_projectile(99, 5),
            BreezeEvent::ReflectProjectile {
                projectile: 99,
                back_at: 5
            }
        ));
        for block in ["oak_door", "spruce_trapdoor", "stone_button", "lever"] {
            assert!(wind_charge_flips_block(block), "{block}");
        }
        assert!(!wind_charge_flips_block("iron_block"));

        let mut close = BreezeBehavior::new();
        // Vanilla `Shoot.isTargetWithinRange` wants distance squared inside
        // (4.0, 256.0): three blocks shoots, while one block is too close.
        assert!(matches!(
            close.tick([0.0, 64.0, 0.0], Some((5, [3.0, 64.0, 0.0])), true),
            Some(BreezeEvent::WindCharge { target: 5 })
        ));
        let mut point_blank = BreezeBehavior::new();
        assert!(point_blank
            .tick([0.0, 64.0, 0.0], Some((5, [1.0, 64.0, 0.0])), true)
            .is_none());

        assert_eq!(bogged_arrow_poison(), (0, 80));
        assert_eq!(bogged_attack_interval_ticks(), 70);
        assert_eq!(bogged_max_hp(), 16.0);
        assert_eq!(bogged_shear_mushrooms(false), Some(("brown_mushroom", 2)));
        assert_eq!(bogged_shear_mushrooms(true), None);
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
        for t in 1..u64::from(GUARDIAN_LASER_TICKS) {
            assert!(guardian.tick_scheduled(0, t, Some((7, 10.0)), &[]).is_empty());
        }
        assert_eq!(
            guardian.tick_scheduled(0, u64::from(GUARDIAN_LASER_TICKS), Some((7, 10.0)), &[]),
            [GuardianEvent::Laser {
                target: 7,
                damage: GUARDIAN_LASER_DAMAGE
            }]
        );
        // Losing the beam target resets the charge.
        assert!(guardian.tick_scheduled(0, 81, None, &[]).is_empty());
        for t in 1..u64::from(GUARDIAN_LASER_TICKS) {
            assert!(guardian.tick_scheduled(0, 81 + t, Some((7, 10.0)), &[]).is_empty());
        }
        // Outside the 16 m attack range the laser never charges.
        let mut guardian = GuardianBehavior::new(false);
        for t in 1..=200 {
            assert!(guardian.tick_scheduled(0, t, Some((7, 16.5)), &[]).is_empty());
        }
        // The elder guardian hits harder, charges its beam in 60 ticks
        // (`ElderGuardian.getAttackDuration`), and paces the 50 m fatigue
        // aura every 1200 ticks.
        let mut elder = GuardianBehavior::new(true);
        for t in 1..u64::from(ELDER_GUARDIAN_LASER_TICKS) {
            elder.tick_scheduled(0, t, Some((7, 10.0)), &[(7, 10.0, false)]);
        }
        assert_eq!(
            elder.tick_scheduled(
                0,
                u64::from(ELDER_GUARDIAN_LASER_TICKS),
                Some((7, 10.0)),
                &[(7, 10.0, false)],
            ),
            [GuardianEvent::Laser {
                target: 7,
                damage: ELDER_GUARDIAN_LASER_DAMAGE
            }]
        );
        let mut elder = GuardianBehavior::new(true);
        for t in 1..u64::from(ELDER_FATIGUE_PERIOD) {
            elder.tick_scheduled(0, t, Some((7, 30.0)), &[(7, 30.0, false)]);
        }
        assert_eq!(
            elder.tick_scheduled(
                0,
                u64::from(ELDER_FATIGUE_PERIOD),
                Some((7, 30.0)),
                &[(7, 30.0, false)],
            ),
            [GuardianEvent::MiningFatigue { target: 7 }]
        );
        let mut elder = GuardianBehavior::new(true);
        for t in 1..=u64::from(ELDER_FATIGUE_PERIOD) {
            assert!(elder
                .tick_scheduled(0, t, Some((7, 55.0)), &[(7, 55.0, false)])
                .is_empty());
        }
        assert_eq!(ELDER_FATIGUE_AMPLIFIER, 2);
        assert_eq!(ELDER_FATIGUE_DURATION, 6000);
    }

    #[test]
    fn elder_guardian_scheduled_aura_staggers_and_reaches_every_player() {
        let mut elder = GuardianBehavior::new(true);
        // `(tickCount + entityId) % 1200 == 0` — entity 37 pulses when the
        // world clock lands on 1163, 2363, ...
        let players = [
            (7, 10.0, false),
            (8, 49.9, false),
            (9, 50.1, false),
            (10, 30.0, true),
        ];
        for tick in 0..1_300u64 {
            let events = elder.tick_scheduled(37, tick, None, &players);
            let fatigue: Vec<i32> = events
                .iter()
                .filter_map(|e| match e {
                    GuardianEvent::MiningFatigue { target } => Some(*target),
                    _ => None,
                })
                .collect();
            if (tick as i128 + 37).rem_euclid(1_200) == 0 {
                assert_eq!(fatigue, vec![7, 8], "pulse hits every eligible player in 50 m");
                assert!(elder.take_elder_effect_event(), "GUARDIAN_ELDER_EFFECT flag raised");
                assert!(!elder.take_elder_effect_event(), "flag is one-shot");
            } else {
                assert!(fatigue.is_empty(), "no pulse off the staggered tick");
                assert!(!elder.elder_effect_pending);
            }
        }
        // A regular guardian never runs the aura, even on the stagger tick.
        let mut guardian = GuardianBehavior::new(false);
        let events = guardian.tick_scheduled(37, 1_163, None, &players);
        assert!(events.is_empty());
        assert!(!guardian.elder_effect_pending);
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

    // ---- Llama strength / carpets, chest slots & death-drop conservation -----

    #[test]
    fn vanilla_llama_strength_clamps_into_vanilla_range() {
        let mut llama = adult_tamed(EquineKind::Llama);
        assert_eq!(llama.llama_strength, 1);
        // setStrength clamps to 1..=5 like vanilla.
        for (input, expected) in [(0u8, 1u8), (1, 1), (3, 3), (5, 5), (6, 5), (255, 5)] {
            assert!(llama.set_llama_strength(input));
            assert_eq!(llama.llama_strength, expected, "strength {input}");
        }
        // Non-llamas have no strength slot and refuse the setter.
        for kind in [EquineKind::Horse, EquineKind::Donkey, EquineKind::Mule] {
            let mut state = adult_tamed(kind);
            assert_eq!(state.llama_strength, 0);
            assert!(!state.set_llama_strength(5), "{kind:?} has no strength");
            assert_eq!(state.llama_strength, 0);
        }
    }

    #[test]
    fn vanilla_donkey_mule_chest_is_exactly_15_slots() {
        for kind in [EquineKind::Donkey, EquineKind::Mule] {
            let mut state = adult_tamed(kind);
            assert!(state.equip_chest(), "{kind:?} should chest");
            assert_eq!(state.chest_slots.len(), 15, "{kind:?} slot count");
            assert!(state.chest_slots.iter().all(Option::is_none));
            // Double-chest refused and cannot re-allocate over live contents.
            state.chest_slots[0] = Some(("diamond".to_string(), 3));
            assert!(!state.equip_chest(), "{kind:?} double-chest refused");
            assert_eq!(state.chest_slots[0], Some(("diamond".to_string(), 3)));
        }
        // Baby and wild adults refuse; the horse can never chest at all.
        for kind in [EquineKind::Donkey, EquineKind::Mule] {
            let mut baby = adult_tamed(kind);
            baby.age_ticks = -100;
            assert!(!baby.equip_chest(), "{kind:?} baby chest refused");
            assert!(baby.chest_slots.is_empty());
            let mut wild = EquineState::new(kind);
            assert!(!wild.equip_chest(), "{kind:?} untamed chest refused");
        }
        let mut horse = adult_tamed(EquineKind::Horse);
        assert!(!horse.equip_chest());
        assert!(horse.chest_slots.is_empty());
    }

    #[test]
    fn vanilla_llama_chest_slots_scale_with_strength() {
        for (strength, slots) in [(1u8, 3usize), (2, 6), (3, 9), (4, 12), (5, 15)] {
            let mut llama = adult_tamed(EquineKind::Llama);
            assert!(llama.set_llama_strength(strength));
            assert!(llama.equip_chest());
            assert_eq!(llama.chest_slots.len(), slots, "strength {strength}");
        }
    }

    #[test]
    fn vanilla_horse_never_allocates_chest_slots() {
        let mut horse = adult_tamed(EquineKind::Horse);
        assert!(!horse.equip_chest());
        assert!(!horse.put_chest_slot(0, ("diamond".to_string(), 1)));
        assert_eq!(horse.take_chest_slot(0), None);
        assert!(horse.chest_slots.is_empty());
    }

    #[test]
    fn vanilla_chest_slots_fail_closed_without_chest() {
        let mut donkey = adult_tamed(EquineKind::Donkey);
        // Not chested: every read and write fails closed.
        assert!(!donkey.put_chest_slot(0, ("apple".to_string(), 5)));
        assert_eq!(donkey.take_chest_slot(0), None);
        assert!(donkey.chest_slots.is_empty());
    }

    #[test]
    fn vanilla_chest_put_and_take_conserve_stacks() {
        let mut donkey = adult_tamed(EquineKind::Donkey);
        assert!(donkey.equip_chest());
        // The full 15-slot inventory is addressable; slot 15 is out of range.
        assert!(donkey.put_chest_slot(14, ("wheat".to_string(), 64)));
        assert!(!donkey.put_chest_slot(15, ("wheat".to_string(), 64)));
        // An occupied slot refuses an overwrite instead of losing the stack.
        assert!(!donkey.put_chest_slot(14, ("apple".to_string(), 1)));
        // Take removes the exact stored stack; a second take sees the empty slot.
        assert_eq!(donkey.take_chest_slot(14), Some(("wheat".to_string(), 64)));
        assert_eq!(donkey.take_chest_slot(14), None);
        // And now the slot accepts a new stack again.
        assert!(donkey.put_chest_slot(14, ("apple".to_string(), 1)));
    }

    #[test]
    fn vanilla_llama_out_of_range_slot_write_fails_closed() {
        let mut llama = adult_tamed(EquineKind::Llama);
        assert!(llama.set_llama_strength(2));
        assert!(llama.equip_chest());
        assert_eq!(llama.chest_slots.len(), 6);
        // Slots 0..=5 are writable; slot 6 is past the strength-2 capacity.
        assert!(llama.put_chest_slot(5, ("dirt".to_string(), 1)));
        assert!(!llama.put_chest_slot(6, ("dirt".to_string(), 1)));
        assert_eq!(llama.take_chest_slot(6), None);
        assert_eq!(llama.take_chest_slot(usize::MAX), None);
    }

    #[test]
    fn vanilla_llama_carpet_swaps_without_loss() {
        let mut llama = adult_tamed(EquineKind::Llama);
        assert_eq!(llama.equip_llama_carpet(Some(4)), Ok(None));
        assert_eq!(llama.carpet_color, Some(4));
        // Swapping returns the worn carpet — zero lost, zero fabricated.
        assert_eq!(llama.equip_llama_carpet(Some(15)), Ok(Some(4)));
        assert_eq!(llama.carpet_color, Some(15));
        // `None` removes the carpet and hands it back.
        assert_eq!(llama.equip_llama_carpet(None), Ok(Some(15)));
        assert_eq!(llama.carpet_color, None);
        assert_eq!(llama.equip_llama_carpet(None), Ok(None));
    }

    #[test]
    fn vanilla_llama_accepts_all_sixteen_carpet_colors() {
        let mut llama = adult_tamed(EquineKind::Llama);
        for color in 0..16u8 {
            assert_eq!(
                llama.equip_llama_carpet(Some(color)),
                Ok(color.checked_sub(1)),
                "carpet color {color}"
            );
            assert_eq!(llama.carpet_color, Some(color));
        }
        // Colors outside the 16 wool dyes are rejected and change nothing.
        for bad in [16u8, 100, u8::MAX] {
            assert_eq!(llama.equip_llama_carpet(Some(bad)), Err(()));
        }
        assert_eq!(llama.carpet_color, Some(15));
    }

    #[test]
    fn vanilla_non_llamas_never_wear_carpets() {
        for kind in [EquineKind::Horse, EquineKind::Donkey, EquineKind::Mule] {
            let mut state = adult_tamed(kind);
            assert_eq!(state.equip_llama_carpet(Some(0)), Err(()));
            assert_eq!(state.carpet_color, None, "{kind:?} carpet stayed None");
        }
        // Untamed and baby llamas refuse too, leaving the slot untouched.
        let mut wild = EquineState::new(EquineKind::Llama);
        assert_eq!(wild.equip_llama_carpet(Some(0)), Err(()));
        let mut baby = adult_tamed(EquineKind::Llama);
        baby.age_ticks = -10;
        assert_eq!(baby.equip_llama_carpet(Some(0)), Err(()));
    }

    #[test]
    fn vanilla_llama_rejects_saddle_and_horse_armor() {
        // Llamas have a carpet slot, not a saddle slot: `isSaddleable` is
        // false for them in vanilla.
        let mut llama = adult_tamed(EquineKind::Llama);
        assert!(!llama.equip_saddle());
        assert!(!llama.saddled);
        assert!(!llama.can_jump());
        assert!(!llama.equip_armor(1));
        assert_eq!(llama.armor_tier, 0);
        // Every other kind saddles normally.
        for kind in [EquineKind::Horse, EquineKind::Donkey, EquineKind::Mule] {
            let mut state = adult_tamed(kind);
            assert!(state.equip_saddle(), "{kind:?} should saddle");
        }
    }

    #[test]
    fn vanilla_baby_and_wild_llama_reject_chest_and_carpet() {
        let mut baby = adult_tamed(EquineKind::Llama);
        baby.age_ticks = -50;
        assert!(!baby.equip_chest());
        assert_eq!(baby.equip_llama_carpet(Some(2)), Err(()));
        assert!(baby.chest_slots.is_empty());
        let mut wild = EquineState::new(EquineKind::Llama);
        assert!(!wild.equip_chest());
        assert_eq!(wild.equip_llama_carpet(Some(2)), Err(()));
    }

    #[test]
    fn vanilla_death_drops_drain_saddle_armor_chest_exactly() {
        // A kitted horse: saddle + diamond armor drop, nothing else.
        let mut horse = adult_tamed(EquineKind::Horse);
        assert!(horse.equip_saddle());
        assert!(horse.equip_armor(4));
        assert_eq!(
            horse.collect_death_drops(),
            vec![
                ("saddle".to_string(), 1),
                ("diamond_horse_armor".to_string(), 1)
            ]
        );
        assert!(!horse.saddled);
        assert_eq!(horse.armor_tier, 0);

        // A chested mule with contents: chest + stacks + no phantom items.
        let mut mule = adult_tamed(EquineKind::Mule);
        assert!(mule.equip_chest());
        assert!(mule.put_chest_slot(0, ("bread".to_string(), 6)));
        assert!(mule.put_chest_slot(14, ("emerald".to_string(), 1)));
        assert_eq!(
            mule.collect_death_drops(),
            vec![
                ("chest".to_string(), 1),
                ("bread".to_string(), 6),
                ("emerald".to_string(), 1)
            ]
        );
        assert!(!mule.chested);
        assert!(mule.chest_slots.is_empty());
    }

    #[test]
    fn vanilla_llama_death_drops_drain_carpet_chest_exactly() {
        let mut llama = adult_tamed(EquineKind::Llama);
        assert!(llama.set_llama_strength(5));
        assert_eq!(llama.equip_llama_carpet(Some(9)), Ok(None));
        assert!(llama.equip_chest());
        assert!(llama.put_chest_slot(0, ("hay_block".to_string(), 2)));
        assert!(llama.put_chest_slot(14, ("cactus".to_string(), 7)));
        assert_eq!(
            llama.collect_death_drops(),
            vec![
                ("cyan_carpet".to_string(), 1),
                ("chest".to_string(), 1),
                ("hay_block".to_string(), 2),
                ("cactus".to_string(), 7)
            ]
        );
        assert_eq!(llama.carpet_color, None);
        assert!(!llama.chested);
        assert!(llama.chest_slots.is_empty());
    }

    #[test]
    fn vanilla_death_drops_are_idempotent_no_dupe() {
        // The classic dupe kills: a death event that somehow fires the drop
        // path twice must produce the items exactly once.
        let mut donkey = adult_tamed(EquineKind::Donkey);
        assert!(donkey.equip_saddle());
        assert!(donkey.equip_chest());
        assert!(donkey.put_chest_slot(3, ("iron_ingot".to_string(), 42)));
        let first = donkey.collect_death_drops();
        assert_eq!(
            first,
            vec![
                ("saddle".to_string(), 1),
                ("chest".to_string(), 1),
                ("iron_ingot".to_string(), 42)
            ]
        );
        assert!(donkey.collect_death_drops().is_empty());
        assert!(donkey.collect_death_drops().is_empty());

        let mut llama = adult_tamed(EquineKind::Llama);
        assert_eq!(llama.equip_llama_carpet(Some(14)), Ok(None));
        assert!(llama.equip_chest());
        assert!(llama.put_chest_slot(0, ("stick".to_string(), 3)));
        assert_eq!(
            llama.collect_death_drops(),
            vec![
                ("red_carpet".to_string(), 1),
                ("chest".to_string(), 1),
                ("stick".to_string(), 3)
            ]
        );
        assert!(llama.collect_death_drops().is_empty());
        // A bare animal drops nothing twice either.
        let mut plain = adult_tamed(EquineKind::Horse);
        assert!(plain.collect_death_drops().is_empty());
    }

    #[test]
    fn vanilla_llama_strength_shrink_truncates_slots_no_dupe() {
        // `createInventory` on setStrength keeps what fits; items past the
        // new capacity leave the inventory rather than duplicating later.
        let mut llama = adult_tamed(EquineKind::Llama);
        assert!(llama.set_llama_strength(5));
        assert!(llama.equip_chest());
        assert!(llama.put_chest_slot(2, ("apple".to_string(), 4)));
        assert!(llama.put_chest_slot(14, ("diamond".to_string(), 1)));
        assert!(llama.set_llama_strength(1));
        assert_eq!(llama.chest_slots.len(), 3);
        assert_eq!(llama.chest_slots[2], Some(("apple".to_string(), 4)));
        // The dropped stack is gone from the inventory — not duplicated by
        // a later death-drop drain.
        assert_eq!(
            llama.collect_death_drops(),
            vec![
                ("chest".to_string(), 1),
                ("apple".to_string(), 4)
            ]
        );

        // Growing strength re-allocates with empty new cells, no dupes.
        let mut llama = adult_tamed(EquineKind::Llama);
        assert!(llama.equip_chest());
        assert!(llama.put_chest_slot(2, ("apple".to_string(), 4)));
        assert!(llama.set_llama_strength(5));
        assert_eq!(llama.chest_slots.len(), 15);
        assert_eq!(llama.chest_slots[2], Some(("apple".to_string(), 4)));
        assert_eq!(llama.chest_slots[14], None);
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

    #[test]
    fn end_dragon_fight_initializes_dragon_and_ten_pillar_crystals() {
        let mut fight = EndDragonFightController::new(42, 60);
        let events = fight.ensure_dragon_fight_initialized();
        let spawns = events
            .iter()
            .filter(|e| matches!(e, EndDragonFightEvent::SpawnDragon { respawned: false, .. }))
            .count();
        let crystals = events
            .iter()
            .filter(|e| matches!(e, EndDragonFightEvent::MaterializeCrystal { .. }))
            .count();
        assert_eq!(spawns, 1);
        assert_eq!(crystals, 10, "exactly ten obsidian spike crystals");
        assert_eq!(fight.active_pillar_crystals().len(), 10);
        assert!(
            fight.ensure_dragon_fight_initialized().is_empty(),
            "initialization is idempotent"
        );
        // Initial crystals exactly match the seed-derived spike tops.
        for pillar in end_crystal_pillars(42) {
            let expected = end_crystal_spawn_position(&pillar);
            assert!(events.iter().any(|e| matches!(
                e,
                EndDragonFightEvent::MaterializeCrystal { position } if *position == expected
            )));
        }
    }

    #[test]
    fn end_dragon_fight_crystal_heals_and_destruction_damages_dragon() {
        let mut fight = EndDragonFightController::new(42, 60);
        fight.ensure_dragon_fight_initialized();
        let crystal = fight.active_pillar_crystals()[0];
        fight.dragon.hp = 100.0;
        // Park the dragon directly above the first crystal so it links.
        let dragon_pos = [
            crystal[0] as f64 + 0.5,
            crystal[1] as f64 + 4.0,
            crystal[2] as f64 + 0.5,
        ];
        // The link is re-picked only when the fight's `nextInt(10)` draw lands
        // on a multiple of ten, so allow several windows for it to form.
        let mut beamed = false;
        for _ in 0..2_000 {
            if fight
                .tick(dragon_pos)
                .iter()
                .any(|e| matches!(e, EndDragonFightEvent::CrystalBeam { crystal: c } if *c == crystal))
            {
                beamed = true;
                break;
            }
        }
        assert!(beamed, "dragon linked to a nearby crystal emits a healing beam");
        assert!(fight.dragon_hp() > 100.0, "healing beam restores 1 hp per beam tick");
        let hp_before = fight.dragon_hp();
        assert!(fight.on_end_crystal_destroyed(crystal));
        assert!(
            fight.dragon_hp() <= hp_before - 9.0,
            "destroying the linked crystal deals vanilla's 10 damage"
        );
        assert!(!fight.active_pillar_crystals().contains(&crystal));
        assert!(!fight.on_end_crystal_destroyed(crystal), "unknown crystals are not tracked");
    }

    #[test]
    fn dragon_respawn_crystal_positions_match_vanilla_rim() {
        // `EndDragonFight` places the four respawn crystals two blocks out on
        // each side of the exit portal's bedrock rim.
        assert_eq!(
            dragon_respawn_crystal_positions(60),
            [[2, 61, 0], [0, 61, 2], [-2, 61, 0], [0, 61, -2]]
        );
    }

    #[test]
    fn dragon_heals_from_any_live_crystal_inside_inflated_bbox() {
        let mut fight = EndDragonFightController::new(42, 60);
        fight.ensure_dragon_fight_initialized();
        // Remove the ten pillar crystals so only player placements remain.
        for crystal in fight.active_pillar_crystals() {
            fight.on_end_crystal_destroyed(crystal);
        }
        fight.dragon.hp = 100.0;
        // A crystal placed while the dragon lives is tracked even though it is
        // not a respawn-rim position and not a spike pillar.
        assert!(!fight.on_end_crystal_placed(10, 100, 0));
        let dragon_pos = [0.0, 100.0, 0.0];
        let mut beamed = false;
        for _ in 0..2_000 {
            if fight
                .tick(dragon_pos)
                .iter()
                .any(|e| matches!(e, EndDragonFightEvent::CrystalBeam { crystal } if *crystal == [10, 100, 0]))
            {
                beamed = true;
                break;
            }
        }
        assert!(beamed, "a player-placed crystal feeds the healing beam");
        assert!(fight.dragon_hp() > 100.0);

        // The same placement 45 blocks out sits beyond the 16x8 dragon box
        // inflated by 32 (40 horizontally) and can never link.
        let mut far = EndDragonFightController::new(42, 60);
        far.ensure_dragon_fight_initialized();
        for crystal in far.active_pillar_crystals() {
            far.on_end_crystal_destroyed(crystal);
        }
        far.dragon.hp = 100.0;
        assert!(!far.on_end_crystal_placed(45, 100, 0));
        for _ in 0..2_000 {
            far.tick(dragon_pos);
        }
        assert_eq!(far.dragon_hp(), 100.0, "crystals outside the inflated bbox never heal");
        assert!(far.linked_crystal.is_none());
    }

    #[test]
    fn end_dragon_fight_four_crystal_respawn_regenerates_pillars_and_dragon() {
        let mut fight = EndDragonFightController::new(42, 60);
        fight.ensure_dragon_fight_initialized();
        let reward = fight.on_dragon_killed();
        assert_eq!(reward.xp, 12_000);
        assert!(reward.dragon_egg.is_some());
        // Non-rim positions are rejected.
        assert!(!fight.on_end_crystal_placed(5, 61, 5));
        let rim = dragon_respawn_crystal_positions(60);
        assert_eq!(rim.len(), 4);
        assert!(rim.contains(&[2, 61, 0]) || rim.contains(&[2, 60, 0]));
        for pos in rim {
            assert!(fight.on_end_crystal_placed(pos[0], pos[1], pos[2]));
        }
        // Destroy all ten pillar crystals so respawn must regenerate them.
        for crystal in fight.active_pillar_crystals() {
            fight.on_end_crystal_destroyed(crystal);
        }
        assert!(fight.active_pillar_crystals().is_empty());
        let dragon_pos = [0.0, 100.0, 0.0];
        let mut saw_close = false;
        let mut saw_beams = 0;
        let mut saw_respawn = false;
        for _ in 0..101 {
            for event in fight.tick(dragon_pos) {
                match event {
                    EndDragonFightEvent::CloseExitPortal => saw_close = true,
                    EndDragonFightEvent::BeamAt { .. } => saw_beams += 1,
                    EndDragonFightEvent::SpawnDragon { respawned: true, .. } => saw_respawn = true,
                    _ => {}
                }
            }
        }
        assert!(saw_close, "respawn closes the exit portal");
        assert_eq!(saw_beams, 4, "beams fire at each of the four rim crystals");
        assert_eq!(fight.active_pillar_crystals().len(), 10, "pillar crystals regenerate");
        assert!(saw_respawn, "dragon respawns after the 100-tick ritual");
        // Second kill gives reduced XP and no egg.
        let reward2 = fight.on_dragon_killed();
        assert_eq!(reward2.xp, 500);
        assert!(reward2.dragon_egg.is_none());
    }

}
