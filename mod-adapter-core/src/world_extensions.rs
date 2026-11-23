// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic world & mod extension primitives shared by every modded-world
//! adapter — pure `std`, zero new dependencies, 100% clean-room.
//!
//! Four families live here:
//!
//! 1. [`ProgressionBarrierEngine`]: declarative biome/dimension progression
//!    gates (Twilight-Forest-style chained boss gating, Cataclysm arenas,
//!    RPG progression packs) driven entirely by advancement predicates —
//!    creative/spectator players bypass cleanly.
//! 2. [`BossEncounterState`]: multi-part boss hitboxes with per-part damage
//!    multipliers, invulnerable sub-parts, phase thresholds, bossbar
//!    progress, and arena bounds protection — plus the bounded binary wire
//!    contracts [`ChunkGenDelegateRequest`]/[`ChunkGenDelegateResponse`]
//!    (`CGD1`) and [`EntityAiDelegateSnapshot`]/[`EntityAiMutationBatch`]
//!    (`EAD1`) letting a delegated JVM `ChunkGenerator`/`EntityAi` hand work
//!    to Rust with zero retained JVM heap and a strict per-tick budget.
//! 3. Built-in QoL server protocol support: AppleSkin
//!    exhaustion/saturation sync channels with a 4-byte big-endian `f32`
//!    codec and delta suppression, and a server-side inventory sorter
//!    ([`sort_and_merge_inventory`]) with a proven item-conservation
//!    invariant — identical stacks merge, nothing dupes, nothing vanishes.
//! 4. [`BiomeRegionBlender`]: TerraBlender-style MultiNoise region blending —
//!    multiple mods' biome regions coexist in one dimension via weighted,
//!    deterministic jittered-grid Voronoi assignment, so Biomes O' Plenty,
//!    BWG, and Regions Unexplored compose without overwriting each other.
//!
//! Clean-room note: every preset, wire format, and blending rule here is an
//! original declarative design — no code, constants, or data tables are
//! ported from Twilight Forest, TerraBlender, AppleSkin, or any other mod.
//! Named channels and advancement ids are interop identifiers only.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

// ===========================================================================
// 1. Progression barriers
// ===========================================================================

/// One declarative progression gate: while a player lacks any of
/// `required_advancements`, every biome in `target_biomes` inside `dimension`
/// is locked — block break/place is denied per the protection flags and the
/// listed debuffs/periodic damage apply each tick the player stays inside.
///
/// `enforce_effects` tuples are `(effect_id, duration_ticks, amplifier)`;
/// `periodic_damage` is `(hearts_per_hit, interval_ticks)`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgressionRule {
    /// Stable rule identifier reported back in [`ProgressionVerdict::Locked`].
    pub id: String,
    /// Dimension resource location this rule applies to (e.g.
    /// `"twilightforest:twilight_forest"`).
    pub dimension: String,
    /// Biomes inside `dimension` gated by this rule.
    pub target_biomes: BTreeSet<String>,
    /// Advancement ids the player must hold *all* of to be allowed in.
    pub required_advancements: Vec<String>,
    /// Deny block breaking while locked.
    pub block_break_protected: bool,
    /// Deny block placing while locked.
    pub block_place_protected: bool,
    /// `(effect_id, duration_ticks, amplifier)` applied each tick while locked.
    pub enforce_effects: Vec<(String, u32, u8)>,
    /// `(hearts, interval_ticks)` of periodic damage while locked.
    pub periodic_damage: Option<(f32, u32)>,
}

/// Outcome of [`ProgressionBarrierEngine::check_progression`].
#[derive(Clone, Debug, PartialEq)]
pub enum ProgressionVerdict {
    /// No locked rule matched — the player may act freely.
    Allowed,
    /// A rule's advancement predicate is unmet; the area stays sealed.
    Locked {
        /// Id of the first matching locked rule.
        rule_id: String,
        /// `required_advancements` the player does not yet hold.
        missing_advancements: Vec<String>,
        /// Mirror of the rule's `block_break_protected` flag.
        block_break_blocked: bool,
        /// Mirror of the rule's `block_place_protected` flag.
        block_place_blocked: bool,
        /// Debuffs to apply each tick while locked.
        effects: Vec<(String, u32, u8)>,
        /// Periodic damage to apply while locked.
        periodic_damage: Option<(f32, u32)>,
    },
}

/// Ordered set of [`ProgressionRule`]s evaluated against a player's
/// advancement set and current position.
#[derive(Clone, Debug, Default)]
pub struct ProgressionBarrierEngine {
    rules: Vec<ProgressionRule>,
}

impl ProgressionBarrierEngine {
    /// Engine evaluating `rules` in declaration order; the first locked rule
    /// that matches a position decides the verdict.
    pub fn new(rules: Vec<ProgressionRule>) -> Self {
        Self { rules }
    }

    /// Registered rules, in evaluation order.
    pub fn rules(&self) -> &[ProgressionRule] {
        &self.rules
    }

    /// Decide whether `player` (holding `player_advancements`) may act at
    /// `biome` in `dimension`. Creative and spectator players always
    /// receive [`ProgressionVerdict::Allowed`].
    pub fn check_progression(
        &self,
        dimension: &str,
        biome: &str,
        player_advancements: &BTreeSet<String>,
        is_creative_or_spectator: bool,
    ) -> ProgressionVerdict {
        if is_creative_or_spectator {
            return ProgressionVerdict::Allowed;
        }
        for rule in &self.rules {
            if rule.dimension != dimension || !rule.target_biomes.contains(biome) {
                continue;
            }
            let missing: Vec<String> = rule
                .required_advancements
                .iter()
                .filter(|a| !player_advancements.contains(*a))
                .cloned()
                .collect();
            if missing.is_empty() {
                continue;
            }
            return ProgressionVerdict::Locked {
                rule_id: rule.id.clone(),
                missing_advancements: missing,
                block_break_blocked: rule.block_break_protected,
                block_place_blocked: rule.block_place_protected,
                effects: rule.enforce_effects.clone(),
                periodic_damage: rule.periodic_damage,
            };
        }
        ProgressionVerdict::Allowed
    }
}

/// Declarative clean-room preset demonstrating a chained boss-biome
/// progression gate: each arena unlocks only after the previous boss's
/// advancement is held. The chain shape (`Naga courtyard -> Lich tower ->
/// Hydra lair -> Snow Queen`) is expressed as ordinary data — the engine is
/// fully generic and the same builder pattern covers Cataclysm or any RPG
/// progression pack.
pub fn twilight_progression_example_rules() -> Vec<ProgressionRule> {
    let dim = "twilightforest:twilight_forest";
    let gate = |id: &str, biomes: &[&str], requires: &[&str], damage: Option<(f32, u32)>| {
        ProgressionRule {
            id: format!("twilight_gate:{id}"),
            dimension: dim.to_string(),
            target_biomes: biomes.iter().map(|b| format!("twilightforest:{b}")).collect(),
            required_advancements: requires
                .iter()
                .map(|a| format!("twilightforest:{a}"))
                .collect(),
            block_break_protected: true,
            block_place_protected: true,
            enforce_effects: vec![("minecraft:mining_fatigue".to_string(), 100, 4)],
            periodic_damage: damage,
        }
    };
    vec![
        // The courtyard is the entry boss: gated only on having entered the
        // dimension at all, so players cannot shortcut-geared skip it.
        gate(
            "naga_courtyard",
            &["naga_courtyard"],
            &["progress_naga_entry"],
            None,
        ),
        // The lich tower seals until the naga falls; intruders are chilled.
        gate(
            "lich_tower",
            &["lich_tower"],
            &["progress_naga"],
            Some((0.5, 40)),
        ),
        // The fire swamp burns intruders who skipped the lich.
        ProgressionRule {
            id: "twilight_gate:hydra_lair".to_string(),
            dimension: dim.to_string(),
            target_biomes: ["twilightforest:fire_swamp".to_string()].into_iter().collect(),
            required_advancements: vec!["twilightforest:progress_lich".to_string()],
            block_break_protected: true,
            block_place_protected: true,
            enforce_effects: vec![
                ("minecraft:slowness".to_string(), 100, 1),
                ("minecraft:weakness".to_string(), 100, 1),
            ],
            periodic_damage: Some((1.0, 30)),
        },
        // The aurora palace blinds anyone who has not slain the hydra.
        ProgressionRule {
            id: "twilight_gate:snow_queen".to_string(),
            dimension: dim.to_string(),
            target_biomes: [
                "twilightforest:snowy_forest".to_string(),
                "twilightforest:glacier".to_string(),
            ]
            .into_iter()
            .collect(),
            required_advancements: vec!["twilightforest:progress_hydra".to_string()],
            block_break_protected: true,
            block_place_protected: true,
            enforce_effects: vec![
                ("minecraft:blindness".to_string(), 100, 0),
                ("minecraft:mining_fatigue".to_string(), 100, 4),
            ],
            periodic_damage: Some((0.5, 20)),
        },
    ]
}

// ===========================================================================
// 2. Multi-part boss encounters
// ===========================================================================

/// One child hitbox riding on a boss entity. `offset_xyz` is the part's
/// center relative to the boss position; `half_extents` gives its AABB.
/// `damage_multiplier` scales every point of base damage the part forwards
/// to the parent HP pool (heads take `2.0`, armored plates `0.5`, …);
/// `invulnerable` parts report hits but absorb all damage.
#[derive(Clone, Debug, PartialEq)]
pub struct SubPartHitbox {
    /// Stable part identifier (`"head"`, `"tail"`, …).
    pub part_id: String,
    /// Center offset from the boss root position.
    pub offset_xyz: [f64; 3],
    /// AABB half extents on each axis.
    pub half_extents: [f64; 3],
    /// Multiplier applied to base damage before it reaches the boss HP.
    pub damage_multiplier: f32,
    /// Absorb all damage (hit is still detected and reported).
    pub invulnerable: bool,
}

/// Result of resolving one hit against a [`BossEncounterState`].
#[derive(Clone, Debug, PartialEq)]
pub enum PartHitOutcome {
    /// The ray/point hit no sub-part.
    Miss,
    /// The resolved point lies outside the arena bounds — the encounter is
    /// protected from out-of-bounds damage cheese.
    OutsideArena,
    /// `apply_part_hit` was given a part id this boss does not have.
    UnknownPart,
    /// The part exists and was hit, but is invulnerable — zero damage dealt.
    Invulnerable {
        /// Part that absorbed the hit.
        part_id: String,
    },
    /// Damage was forwarded to the boss HP pool.
    Landed {
        /// Part that took the hit.
        part_id: String,
        /// Damage after the part's `damage_multiplier` was applied.
        damage_applied: f32,
        /// Boss health remaining after the hit.
        health_remaining: f32,
        /// 1-based phase after the hit.
        phase: u32,
        /// Whether this hit crossed a phase threshold.
        phase_changed: bool,
    },
}

/// Mutable state of one multi-part boss fight: HP pool, per-part hitboxes,
/// phase thresholds, bossbar progress, and the arena bounds hits must land
/// inside. Damage resolution runs entirely on the parent HP — sub-parts
/// never have their own health, only multipliers.
#[derive(Clone, Debug)]
pub struct BossEncounterState {
    /// Entity id of the boss root.
    pub entity_id: u64,
    /// HP at fight start; the bossbar denominator.
    pub max_health: f32,
    /// Current parent HP shared by all sub-parts.
    pub health: f32,
    /// Descending HP fractions at which the next phase begins (e.g. `0.5`
    /// puts phase 2 at half health). Normalized on construction.
    pub phase_thresholds: Vec<f32>,
    /// Sub-part hitboxes; positions ride on `position_xyz`.
    pub parts: Vec<SubPartHitbox>,
    /// Boss root position all part offsets are relative to.
    pub position_xyz: [f64; 3],
    /// Inclusive arena bounds — damage may only land on hit points inside.
    pub arena_min: [f64; 3],
    /// Inclusive arena bounds — damage may only land on hit points inside.
    pub arena_max: [f64; 3],
}

impl BossEncounterState {
    /// Build an encounter. `phase_thresholds` are kept in `(0, 1)`, sorted
    /// descending, and deduplicated.
    pub fn new(
        entity_id: u64,
        max_health: f32,
        phase_thresholds: Vec<f32>,
        parts: Vec<SubPartHitbox>,
        position_xyz: [f64; 3],
        arena_min: [f64; 3],
        arena_max: [f64; 3],
    ) -> Self {
        let mut thresholds: Vec<f32> = phase_thresholds
            .into_iter()
            .filter(|t| *t > 0.0 && *t < 1.0)
            .collect();
        thresholds.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        thresholds.dedup();
        Self {
            entity_id,
            max_health: max_health.max(f32::MIN_POSITIVE),
            health: max_health.max(f32::MIN_POSITIVE),
            phase_thresholds: thresholds,
            parts,
            position_xyz,
            arena_min,
            arena_max,
        }
    }

    /// Bossbar fill fraction `0.0..=1.0`.
    pub fn bossbar_progress(&self) -> f32 {
        (self.health / self.max_health).clamp(0.0, 1.0)
    }

    /// 1-based phase: `1 +` the number of thresholds the current HP fraction
    /// has fallen at or below. `phase_thresholds = [0.5]` therefore yields
    /// phase 1 above half health and phase 2 at or below it.
    pub fn current_phase(&self) -> u32 {
        let frac = self.health / self.max_health;
        1 + self
            .phase_thresholds
            .iter()
            .filter(|t| frac <= **t)
            .count() as u32
    }

    /// Whether the boss still has HP remaining.
    pub fn is_alive(&self) -> bool {
        self.health > 0.0
    }

    /// Whether `point` lies inside the inclusive arena bounds.
    pub fn is_inside_arena(&self, point: [f64; 3]) -> bool {
        (0..3).all(|i| point[i] >= self.arena_min[i] && point[i] <= self.arena_max[i])
    }

    /// `(min, max)` world-space AABB of `part`.
    pub fn part_aabb(&self, part: &SubPartHitbox) -> ([f64; 3], [f64; 3]) {
        let mut min = [0.0; 3];
        let mut max = [0.0; 3];
        for i in 0..3 {
            let c = self.position_xyz[i] + part.offset_xyz[i];
            min[i] = c - part.half_extents[i];
            max[i] = c + part.half_extents[i];
        }
        (min, max)
    }

    /// First sub-part whose AABB contains `point`.
    pub fn part_at(&self, point: [f64; 3]) -> Option<&SubPartHitbox> {
        self.parts.iter().find(|part| {
            let (min, max) = self.part_aabb(part);
            (0..3).all(|i| point[i] >= min[i] && point[i] <= max[i])
        })
    }

    /// Nearest sub-part intersected by the ray `origin + t*dir` with
    /// `t <= max_distance`. Returns `(part_id, hit_point, t)` of the
    /// smallest non-negative `t`; geometry only — use [`Self::resolve_ray_hit`]
    /// for the full damage pipeline.
    pub fn hitscan(
        &self,
        origin: [f64; 3],
        dir: [f64; 3],
        max_distance: f64,
    ) -> Option<(String, [f64; 3], f64)> {
        let mut best: Option<(&SubPartHitbox, f64)> = None;
        for part in &self.parts {
            let (min, max) = self.part_aabb(part);
            if let Some(t) = ray_aabb(origin, dir, min, max, max_distance) {
                if best.is_none_or(|(_, bt)| t < bt) {
                    best = Some((part, t));
                }
            }
        }
        best.map(|(part, t)| {
            (
                part.part_id.clone(),
                [
                    origin[0] + dir[0] * t,
                    origin[1] + dir[1] * t,
                    origin[2] + dir[2] * t,
                ],
                t,
            )
        })
    }

    /// Full ray pipeline: [`Self::hitscan`] then arena/invulnerability checks
    /// and parent-HP forwarding.
    pub fn resolve_ray_hit(
        &mut self,
        origin: [f64; 3],
        dir: [f64; 3],
        max_distance: f64,
        base_damage: f32,
    ) -> PartHitOutcome {
        let Some((part_id, point, _t)) = self.hitscan(origin, dir, max_distance) else {
            return PartHitOutcome::Miss;
        };
        self.apply_part_hit(&part_id, base_damage, point)
    }

    /// Full point pipeline: find the part containing `point`, then
    /// arena/invulnerability checks and parent-HP forwarding.
    pub fn resolve_point_hit(&mut self, point: [f64; 3], base_damage: f32) -> PartHitOutcome {
        let Some(part_id) = self.part_at(point).map(|p| p.part_id.clone()) else {
            return PartHitOutcome::Miss;
        };
        self.apply_part_hit(&part_id, base_damage, point)
    }

    /// Forward `base_damage` through the part's multiplier onto the parent HP
    /// pool. `hit_point` must lie inside the arena bounds.
    pub fn apply_part_hit(
        &mut self,
        part_id: &str,
        base_damage: f32,
        hit_point: [f64; 3],
    ) -> PartHitOutcome {
        if !self.is_inside_arena(hit_point) {
            return PartHitOutcome::OutsideArena;
        }
        let Some(part) = self.parts.iter().find(|p| p.part_id == part_id) else {
            return PartHitOutcome::UnknownPart;
        };
        if part.invulnerable {
            return PartHitOutcome::Invulnerable {
                part_id: part_id.to_string(),
            };
        }
        let phase_before = self.current_phase();
        let damage = (base_damage * part.damage_multiplier).max(0.0);
        self.health = (self.health - damage).max(0.0);
        let phase = self.current_phase();
        PartHitOutcome::Landed {
            part_id: part_id.to_string(),
            damage_applied: damage,
            health_remaining: self.health,
            phase,
            phase_changed: phase != phase_before,
        }
    }
}

/// Slab-method ray/AABB intersection: smallest `t` in `[0, max_t]` or `None`.
fn ray_aabb(origin: [f64; 3], dir: [f64; 3], min: [f64; 3], max: [f64; 3], max_t: f64) -> Option<f64> {
    let mut t_enter = 0.0f64;
    let mut t_exit = max_t;
    for i in 0..3 {
        if dir[i].abs() < f64::EPSILON {
            if origin[i] < min[i] || origin[i] > max[i] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / dir[i];
        let mut t1 = (min[i] - origin[i]) * inv;
        let mut t2 = (max[i] - origin[i]) * inv;
        if t1 > t2 {
            std::mem::swap(&mut t1, &mut t2);
        }
        t_enter = t_enter.max(t1);
        t_exit = t_exit.min(t2);
        if t_enter > t_exit {
            return None;
        }
    }
    Some(t_enter)
}

// ===========================================================================
// Bounded delegate wire contracts (CGD1 / EAD1)
// ===========================================================================

/// IEEE 802 CRC-32 delegated to the shared canonical primitive.
fn crc32(data: &[u8]) -> u32 {
    megu_core::hash::crc32_ieee(data)
}

/// `magic + crc32(body) + body` frame; `None` on any corruption, truncation,
/// or trailing garbage.
fn encode_frame(magic: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&crc32(body).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn decode_frame<'a>(magic: &[u8; 4], bytes: &'a [u8]) -> Option<&'a [u8]> {
    if bytes.len() < 8 || &bytes[..4] != magic {
        return None;
    }
    let declared = u32::from_be_bytes(bytes[4..8].try_into().ok()?);
    let body = &bytes[8..];
    (crc32(body) == declared).then_some(body)
}

fn get_i32(body: &[u8], cursor: &mut usize) -> Option<i32> {
    let v = i32::from_be_bytes(body.get(*cursor..*cursor + 4)?.try_into().ok()?);
    *cursor += 4;
    Some(v)
}

fn get_u32(body: &[u8], cursor: &mut usize) -> Option<u32> {
    let v = u32::from_be_bytes(body.get(*cursor..*cursor + 4)?.try_into().ok()?);
    *cursor += 4;
    Some(v)
}

fn get_i64(body: &[u8], cursor: &mut usize) -> Option<i64> {
    let v = i64::from_be_bytes(body.get(*cursor..*cursor + 8)?.try_into().ok()?);
    *cursor += 8;
    Some(v)
}

fn get_u64(body: &[u8], cursor: &mut usize) -> Option<u64> {
    let v = u64::from_be_bytes(body.get(*cursor..*cursor + 8)?.try_into().ok()?);
    *cursor += 8;
    Some(v)
}

fn get_f32(body: &[u8], cursor: &mut usize) -> Option<f32> {
    Some(f32::from_bits(get_u32(body, cursor)?))
}

fn get_f64(body: &[u8], cursor: &mut usize) -> Option<f64> {
    Some(f64::from_bits(get_u64(body, cursor)?))
}

fn put_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_bits().to_be_bytes());
}

/// Length-prefixed UTF-8 string (`u16 BE` length). `None` when the declared
/// length overruns `body` or the bytes are not UTF-8.
fn get_string(body: &[u8], cursor: &mut usize, max_len: usize) -> Option<String> {
    let len = u16::from_be_bytes(body.get(*cursor..*cursor + 2)?.try_into().ok()?) as usize;
    *cursor += 2;
    if len > max_len {
        return None;
    }
    let slice = body.get(*cursor..*cursor + len)?;
    *cursor += len;
    std::str::from_utf8(slice).ok().map(str::to_string)
}

fn put_string(out: &mut Vec<u8>, text: &str) -> bool {
    if text.len() > u16::MAX as usize {
        return false;
    }
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    true
}

// ---------------------------------------------------------------------------
// CGD1 — chunk-generator delegate
// ---------------------------------------------------------------------------

const CGD1_MAGIC: &[u8; 4] = b"CGD1";

/// States in one chunk section (16x16x16 palette indices).
pub const SECTION_STATE_COUNT: usize = 4096;
/// Maximum sections a delegate response may carry (256 * 16 = 4096 blocks of
/// build height — far past every vanilla and modded dimension).
pub const MAX_DELEGATE_SECTIONS: u32 = 256;
/// Largest dimension id accepted on the wire.
pub const MAX_DELEGATE_DIM_LEN: usize = 1024;

/// One bounded hand-off request asking a delegated JVM `ChunkGenerator` to
/// produce raw section states for chunk `(cx, cz)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkGenDelegateRequest {
    /// Dimension resource location.
    pub dim: String,
    /// World seed.
    pub seed: i64,
    /// Chunk X coordinate.
    pub cx: i32,
    /// Chunk Z coordinate.
    pub cz: i32,
    /// Minimum build Y of the chunk column.
    pub min_y: i32,
    /// Column height in blocks; must be a positive multiple of 16 and at
    /// most [`MAX_DELEGATE_SECTIONS`] sections.
    pub height: u32,
}

/// Response carrying the requested chunk's sections — each a
/// [`SECTION_STATE_COUNT`]-state palette-index array — so Rust owns the data
/// and the JVM retains zero chunk heap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkGenDelegateResponse {
    /// Chunk X coordinate (must echo the request).
    pub cx: i32,
    /// Chunk Z coordinate (must echo the request).
    pub cz: i32,
    /// Bottom-up sections of exactly [`SECTION_STATE_COUNT`] states.
    pub sections: Vec<Vec<u32>>,
}

/// Encode `req` as a `CGD1` frame. Returns `Err` when the dimension id or
/// height is out of contract instead of emitting a frame the peer must
/// reject.
pub fn encode_chunk_gen_request(req: &ChunkGenDelegateRequest) -> Result<Vec<u8>, &'static str> {
    if req.dim.len() > MAX_DELEGATE_DIM_LEN {
        return Err("oversized delegate dimension id");
    }
    if req.height == 0 || !req.height.is_multiple_of(16) || req.height / 16 > MAX_DELEGATE_SECTIONS {
        return Err("delegate height out of section bounds");
    }
    let mut body = Vec::with_capacity(24 + req.dim.len());
    if !put_string(&mut body, &req.dim) {
        return Err("oversized delegate dimension id");
    }
    body.extend_from_slice(&req.seed.to_be_bytes());
    body.extend_from_slice(&req.cx.to_be_bytes());
    body.extend_from_slice(&req.cz.to_be_bytes());
    body.extend_from_slice(&req.min_y.to_be_bytes());
    body.extend_from_slice(&req.height.to_be_bytes());
    Ok(encode_frame(CGD1_MAGIC, &body))
}

/// Decode a `CGD1` request frame, failing closed on corruption or
/// out-of-contract fields.
pub fn decode_chunk_gen_request(bytes: &[u8]) -> Option<ChunkGenDelegateRequest> {
    let body = decode_frame(CGD1_MAGIC, bytes)?;
    let mut cursor = 0usize;
    let dim = get_string(body, &mut cursor, MAX_DELEGATE_DIM_LEN)?;
    let seed = get_i64(body, &mut cursor)?;
    let cx = get_i32(body, &mut cursor)?;
    let cz = get_i32(body, &mut cursor)?;
    let min_y = get_i32(body, &mut cursor)?;
    let height = get_u32(body, &mut cursor)?;
    if cursor != body.len()
        || height == 0
        || height % 16 != 0
        || height / 16 > MAX_DELEGATE_SECTIONS
    {
        return None;
    }
    Some(ChunkGenDelegateRequest {
        dim,
        seed,
        cx,
        cz,
        min_y,
        height,
    })
}

/// Encode `resp` as a `CGD1` frame. Every section must contain exactly
/// [`SECTION_STATE_COUNT`] states and the section count must stay within
/// [`MAX_DELEGATE_SECTIONS`].
pub fn encode_chunk_gen_response(resp: &ChunkGenDelegateResponse) -> Result<Vec<u8>, &'static str> {
    if resp.sections.len() > MAX_DELEGATE_SECTIONS as usize {
        return Err("too many delegate sections");
    }
    if resp
        .sections
        .iter()
        .any(|s| s.len() != SECTION_STATE_COUNT)
    {
        return Err("delegate section does not hold 4096 states");
    }
    let mut body = Vec::with_capacity(12 + resp.sections.len() * (4 + SECTION_STATE_COUNT * 4));
    body.extend_from_slice(&resp.cx.to_be_bytes());
    body.extend_from_slice(&resp.cz.to_be_bytes());
    body.extend_from_slice(&(resp.sections.len() as u32).to_be_bytes());
    for section in &resp.sections {
        body.extend_from_slice(&(section.len() as u32).to_be_bytes());
        for state in section {
            body.extend_from_slice(&state.to_be_bytes());
        }
    }
    Ok(encode_frame(CGD1_MAGIC, &body))
}

/// Decode a `CGD1` response frame. The section count is bounds-checked
/// against [`MAX_DELEGATE_SECTIONS`] and each section's state count against
/// [`SECTION_STATE_COUNT`] **before** any allocation, so a corrupt or hostile
/// frame cannot force a huge `Vec::with_capacity`.
pub fn decode_chunk_gen_response(bytes: &[u8]) -> Option<ChunkGenDelegateResponse> {
    let body = decode_frame(CGD1_MAGIC, bytes)?;
    let mut cursor = 0usize;
    let cx = get_i32(body, &mut cursor)?;
    let cz = get_i32(body, &mut cursor)?;
    let section_count = get_u32(body, &mut cursor)?;
    if section_count > MAX_DELEGATE_SECTIONS {
        return None;
    }
    // Each section needs at least its 4-byte count; a count beyond what the
    // remaining bytes can describe is provably corrupt before we allocate.
    if section_count as usize > (body.len() - cursor) / 4 {
        return None;
    }
    let mut sections = Vec::with_capacity(section_count as usize);
    for _ in 0..section_count {
        let state_count = get_u32(body, &mut cursor)? as usize;
        if state_count > SECTION_STATE_COUNT || state_count > (body.len() - cursor) / 4 {
            return None;
        }
        let mut states = Vec::with_capacity(state_count);
        for _ in 0..state_count {
            states.push(get_u32(body, &mut cursor)?);
        }
        sections.push(states);
    }
    if cursor != body.len() {
        return None;
    }
    Some(ChunkGenDelegateResponse { cx, cz, sections })
}

/// Post-decode contract check: a response only pairs with the request that
/// produced it when coordinates match and it carries exactly
/// `request.height / 16` full sections.
pub fn validate_chunk_gen_response(
    req: &ChunkGenDelegateRequest,
    resp: &ChunkGenDelegateResponse,
) -> bool {
    resp.cx == req.cx
        && resp.cz == req.cz
        && resp.sections.len() as u64 == u64::from(req.height) / 16
        && resp
            .sections
            .iter()
            .all(|s| s.len() == SECTION_STATE_COUNT)
}

// ---------------------------------------------------------------------------
// EAD1 — entity-AI delegate
// ---------------------------------------------------------------------------

const EAD1_MAGIC: &[u8; 4] = b"EAD1";
const EAD1_KIND_SNAPSHOT: u8 = 0x01;
const EAD1_KIND_MUTATIONS: u8 = 0x02;

/// Hard per-tick cap on mutations a delegated AI may commit — a buggy or
/// hostile AI can never hang or grief the server past this budget.
pub const MAX_AI_MUTATIONS_PER_TICK: u32 = 256;
/// Largest particle count one `SpawnParticle` mutation may request.
pub const MAX_PARTICLE_SPAWN_COUNT: u32 = 10_000;
/// Largest sound id accepted on the wire.
pub const MAX_SOUND_ID_LEN: usize = 1024;

/// State snapshot handed to a delegated `EntityAi` each tick.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityAiDelegateSnapshot {
    /// Entity being driven.
    pub entity_id: u64,
    /// World position.
    pub pos_xyz: [f64; 3],
    /// Head yaw in degrees.
    pub yaw_deg: f32,
    /// Head pitch in degrees.
    pub pitch_deg: f32,
    /// Current health.
    pub health: f32,
    /// Maximum health.
    pub max_health: f32,
    /// Current attack target, if any.
    pub target_entity_id: Option<u64>,
}

/// One bounded world mutation a delegated `EntityAi` may request.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityAiMutation {
    /// Path toward `xyz` at `speed` blocks/tick.
    MoveTo {
        /// Destination.
        xyz: [f64; 3],
        /// Travel speed.
        speed: f64,
    },
    /// Overwrite the entity's velocity vector.
    SetVelocity {
        /// New velocity.
        xyz: [f64; 3],
    },
    /// Melee-attack `target_entity_id` for `damage` hearts.
    AttackEntity {
        /// Victim entity id.
        target_entity_id: u64,
        /// Damage to deal.
        damage: f32,
    },
    /// Set the block at `pos` to palette `state_id`.
    SetBlock {
        /// Block position.
        pos: [i32; 3],
        /// Target block state id.
        state_id: u32,
    },
    /// Emit `count` of `particle_id` at `xyz`.
    SpawnParticle {
        /// Particle id.
        particle_id: u32,
        /// Origin.
        xyz: [f64; 3],
        /// Particle count (bounded by [`MAX_PARTICLE_SPAWN_COUNT`]).
        count: u32,
    },
    /// Play `sound_id` at `volume`/`pitch` from the entity's position.
    PlaySound {
        /// Sound resource location.
        sound_id: String,
        /// Volume.
        volume: f32,
        /// Pitch.
        pitch: f32,
    },
}

/// One tick's worth of mutations a delegated AI wants committed for
/// `entity_id`. Batches beyond [`MAX_AI_MUTATIONS_PER_TICK`] fail on both
/// encode and decode.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityAiMutationBatch {
    /// Entity the mutations apply to.
    pub entity_id: u64,
    /// Ordered mutations; length must stay within the per-tick budget.
    pub mutations: Vec<EntityAiMutation>,
}

/// Encode `snapshot` as an `EAD1` frame (kind `0x01`).
pub fn encode_ai_snapshot(snapshot: &EntityAiDelegateSnapshot) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    body.push(EAD1_KIND_SNAPSHOT);
    body.extend_from_slice(&snapshot.entity_id.to_be_bytes());
    for v in snapshot.pos_xyz {
        put_f64(&mut body, v);
    }
    body.extend_from_slice(&snapshot.yaw_deg.to_bits().to_be_bytes());
    body.extend_from_slice(&snapshot.pitch_deg.to_bits().to_be_bytes());
    body.extend_from_slice(&snapshot.health.to_bits().to_be_bytes());
    body.extend_from_slice(&snapshot.max_health.to_bits().to_be_bytes());
    match snapshot.target_entity_id {
        Some(id) => {
            body.push(1);
            body.extend_from_slice(&id.to_be_bytes());
        }
        None => body.push(0),
    }
    encode_frame(EAD1_MAGIC, &body)
}

/// Decode an `EAD1` snapshot frame; `None` on corruption or wrong record kind.
pub fn decode_ai_snapshot(bytes: &[u8]) -> Option<EntityAiDelegateSnapshot> {
    let body = decode_frame(EAD1_MAGIC, bytes)?;
    if body.first().copied() != Some(EAD1_KIND_SNAPSHOT) {
        return None;
    }
    let mut cursor = 1usize;
    let entity_id = get_u64(body, &mut cursor)?;
    let pos_xyz = [
        get_f64(body, &mut cursor)?,
        get_f64(body, &mut cursor)?,
        get_f64(body, &mut cursor)?,
    ];
    let yaw_deg = get_f32(body, &mut cursor)?;
    let pitch_deg = get_f32(body, &mut cursor)?;
    let health = get_f32(body, &mut cursor)?;
    let max_health = get_f32(body, &mut cursor)?;
    let target_entity_id = match body.get(cursor) {
        Some(0) => {
            cursor += 1;
            None
        }
        Some(1) => {
            cursor += 1;
            Some(get_u64(body, &mut cursor)?)
        }
        _ => return None,
    };
    if cursor != body.len() {
        return None;
    }
    Some(EntityAiDelegateSnapshot {
        entity_id,
        pos_xyz,
        yaw_deg,
        pitch_deg,
        health,
        max_health,
        target_entity_id,
    })
}

/// Encode `batch` as an `EAD1` frame (kind `0x02`), failing closed when the
/// batch exceeds [`MAX_AI_MUTATIONS_PER_TICK`], a particle count exceeds
/// [`MAX_PARTICLE_SPAWN_COUNT`], or a sound id exceeds [`MAX_SOUND_ID_LEN`].
pub fn encode_ai_mutation_batch(batch: &EntityAiMutationBatch) -> Result<Vec<u8>, &'static str> {
    if batch.mutations.len() > MAX_AI_MUTATIONS_PER_TICK as usize {
        return Err("AI mutation batch exceeds per-tick budget");
    }
    let mut body = Vec::with_capacity(32 + batch.mutations.len() * 32);
    body.push(EAD1_KIND_MUTATIONS);
    body.extend_from_slice(&batch.entity_id.to_be_bytes());
    body.extend_from_slice(&(batch.mutations.len() as u32).to_be_bytes());
    for mutation in &batch.mutations {
        match mutation {
            EntityAiMutation::MoveTo { xyz, speed } => {
                body.push(1);
                for v in xyz {
                    put_f64(&mut body, *v);
                }
                put_f64(&mut body, *speed);
            }
            EntityAiMutation::SetVelocity { xyz } => {
                body.push(2);
                for v in xyz {
                    put_f64(&mut body, *v);
                }
            }
            EntityAiMutation::AttackEntity {
                target_entity_id,
                damage,
            } => {
                body.push(3);
                body.extend_from_slice(&target_entity_id.to_be_bytes());
                body.extend_from_slice(&damage.to_bits().to_be_bytes());
            }
            EntityAiMutation::SetBlock { pos, state_id } => {
                body.push(4);
                for v in pos {
                    body.extend_from_slice(&v.to_be_bytes());
                }
                body.extend_from_slice(&state_id.to_be_bytes());
            }
            EntityAiMutation::SpawnParticle {
                particle_id,
                xyz,
                count,
            } => {
                if *count > MAX_PARTICLE_SPAWN_COUNT {
                    return Err("particle spawn count out of bounds");
                }
                body.push(5);
                body.extend_from_slice(&particle_id.to_be_bytes());
                for v in xyz {
                    put_f64(&mut body, *v);
                }
                body.extend_from_slice(&count.to_be_bytes());
            }
            EntityAiMutation::PlaySound {
                sound_id,
                volume,
                pitch,
            } => {
                if sound_id.len() > MAX_SOUND_ID_LEN {
                    return Err("oversized sound id");
                }
                body.push(6);
                if !put_string(&mut body, sound_id) {
                    return Err("oversized sound id");
                }
                body.extend_from_slice(&volume.to_bits().to_be_bytes());
                body.extend_from_slice(&pitch.to_bits().to_be_bytes());
            }
        }
    }
    Ok(encode_frame(EAD1_MAGIC, &body))
}

/// Decode an `EAD1` mutation-batch frame. The mutation count is checked
/// against [`MAX_AI_MUTATIONS_PER_TICK`] **before** the batch vector is
/// allocated, and every payload field is bounds-checked as it is read.
pub fn decode_ai_mutation_batch(bytes: &[u8]) -> Option<EntityAiMutationBatch> {
    let body = decode_frame(EAD1_MAGIC, bytes)?;
    if body.first().copied() != Some(EAD1_KIND_MUTATIONS) {
        return None;
    }
    let mut cursor = 1usize;
    let entity_id = get_u64(body, &mut cursor)?;
    let count = get_u32(body, &mut cursor)?;
    if count > MAX_AI_MUTATIONS_PER_TICK || count as usize > body.len() - cursor {
        return None;
    }
    let mut mutations = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let tag = *body.get(cursor)?;
        cursor += 1;
        let mutation = match tag {
            1 => EntityAiMutation::MoveTo {
                xyz: [
                    get_f64(body, &mut cursor)?,
                    get_f64(body, &mut cursor)?,
                    get_f64(body, &mut cursor)?,
                ],
                speed: get_f64(body, &mut cursor)?,
            },
            2 => EntityAiMutation::SetVelocity {
                xyz: [
                    get_f64(body, &mut cursor)?,
                    get_f64(body, &mut cursor)?,
                    get_f64(body, &mut cursor)?,
                ],
            },
            3 => EntityAiMutation::AttackEntity {
                target_entity_id: get_u64(body, &mut cursor)?,
                damage: get_f32(body, &mut cursor)?,
            },
            4 => EntityAiMutation::SetBlock {
                pos: [
                    get_i32(body, &mut cursor)?,
                    get_i32(body, &mut cursor)?,
                    get_i32(body, &mut cursor)?,
                ],
                state_id: get_u32(body, &mut cursor)?,
            },
            5 => {
                let particle_id = get_u32(body, &mut cursor)?;
                let xyz = [
                    get_f64(body, &mut cursor)?,
                    get_f64(body, &mut cursor)?,
                    get_f64(body, &mut cursor)?,
                ];
                let count = get_u32(body, &mut cursor)?;
                if count > MAX_PARTICLE_SPAWN_COUNT {
                    return None;
                }
                EntityAiMutation::SpawnParticle {
                    particle_id,
                    xyz,
                    count,
                }
            }
            6 => EntityAiMutation::PlaySound {
                sound_id: get_string(body, &mut cursor, MAX_SOUND_ID_LEN)?,
                volume: get_f32(body, &mut cursor)?,
                pitch: get_f32(body, &mut cursor)?,
            },
            _ => return None,
        };
        mutations.push(mutation);
    }
    if cursor != body.len() {
        return None;
    }
    Some(EntityAiMutationBatch {
        entity_id,
        mutations,
    })
}

// ===========================================================================
// 3a. AppleSkin exhaustion/saturation sync
// ===========================================================================

/// AppleSkin exhaustion-sync custom-payload channel.
pub const CHANNEL_APPLESKIN_EXHAUSTION: &str = "appleskin:exhaustion_sync";
/// AppleSkin saturation-sync custom-payload channel.
pub const CHANNEL_APPLESKIN_SATURATION: &str = "appleskin:saturation_sync";
/// Minimum absolute change before a new value is sent — sub-epsilon drift
/// stays silent so the channels carry no per-tick noise.
pub const APPLESKIN_DELTA_EPSILON: f32 = 0.01;

/// AppleSkin's exact wire payload: one 4-byte big-endian IEEE-754 `f32`.
pub fn encode_appleskin_f32(value: f32) -> [u8; 4] {
    value.to_bits().to_be_bytes()
}

/// Decode an AppleSkin payload; `None` unless exactly 4 bytes are given.
pub fn decode_appleskin_f32(bytes: &[u8]) -> Option<f32> {
    let arr: [u8; 4] = bytes.try_into().ok()?;
    Some(f32::from_bits(u32::from_be_bytes(arr)))
}

/// Per-player delta suppressor for the two AppleSkin channels — a payload is
/// produced only on the first observation or when
/// `|new - last| >= APPLESKIN_DELTA_EPSILON`.
#[derive(Clone, Debug, Default)]
pub struct AppleSkinDeltaTracker {
    last_exhaustion: Option<f32>,
    last_saturation: Option<f32>,
}

impl AppleSkinDeltaTracker {
    /// Fresh tracker with nothing sent yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Some(payload)` when `value` should go out on
    /// [`CHANNEL_APPLESKIN_EXHAUSTION`], else `None`.
    pub fn poll_exhaustion(&mut self, value: f32) -> Option<[u8; 4]> {
        poll_delta(&mut self.last_exhaustion, value)
    }

    /// `Some(payload)` when `value` should go out on
    /// [`CHANNEL_APPLESKIN_SATURATION`], else `None`.
    pub fn poll_saturation(&mut self, value: f32) -> Option<[u8; 4]> {
        poll_delta(&mut self.last_saturation, value)
    }
}

fn poll_delta(last: &mut Option<f32>, value: f32) -> Option<[u8; 4]> {
    let send = match *last {
        Some(prev) => (value - prev).abs() >= APPLESKIN_DELTA_EPSILON,
        None => true,
    };
    if send {
        *last = Some(value);
        Some(encode_appleskin_f32(value))
    } else {
        None
    }
}

// ===========================================================================
// 3b. Server-side inventory sorter
// ===========================================================================

/// One inventory slot's contents, comparable for deterministic sorting.
/// `component_hash` fingerprints the stack's components/NBT so stacks merge
/// only when *fully* identical — a `u64` is a hash, never item data itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortableStack {
    /// Item resource location (`"minecraft:stone"`).
    pub item_id: String,
    /// Hash of the stack's component data.
    pub component_hash: u64,
    /// Items in this stack (1..=`max_stack_size`).
    pub count: u32,
    /// Vanilla item stack cap (1, 16, or 64); `0` is clamped to 1.
    pub max_stack_size: u32,
}

/// Merge identical `(item_id, component_hash)` stacks up to each item's
/// `max_stack_size`, then order the inventory deterministically by
/// `(item_id, component_hash, Reverse(count))` with empty slots trailing.
///
/// Item conservation is an invariant: the total `count` per
/// `(item_id, component_hash)` key is identical before and after — merging
/// only repacks existing items into fewer, fuller stacks, so nothing can
/// dupe or vanish. The invariant is checked in debug builds.
pub fn sort_and_merge_inventory(slots: &mut [Option<SortableStack>]) {
    let totals_before = inventory_totals(slots);

    // Group stacks by identity key; BTreeMap iteration is the deterministic
    // (item_id, component_hash) sort order.
    let mut groups: BTreeMap<(String, u64), Vec<SortableStack>> = BTreeMap::new();
    for slot in slots.iter().filter_map(|s| s.as_ref()) {
        groups
            .entry((slot.item_id.clone(), slot.component_hash))
            .or_default()
            .push(slot.clone());
    }

    let mut merged: Vec<SortableStack> = Vec::new();
    for ((_item, _hash), mut stacks) in groups {
        let max = stacks[0].max_stack_size.max(1);
        // Descending-count fill keeps the fullest stacks first, matching the
        // global Reverse(count) tie-break.
        stacks.sort_by_key(|s| Reverse(s.count));
        let mut total: u64 = stacks.iter().map(|s| u64::from(s.count)).sum();
        let mut template = stacks[0].clone();
        while total > 0 {
            let take = total.min(u64::from(max)) as u32;
            template.count = take;
            merged.push(template.clone());
            total -= u64::from(take);
        }
    }
    // Cross-group order is already (item_id, component_hash); the
    // descending-count fill above finishes the (…, Reverse(count)) key.
    merged.sort_by(|a, b| {
        a.item_id
            .cmp(&b.item_id)
            .then(a.component_hash.cmp(&b.component_hash))
            .then(b.count.cmp(&a.count))
    });

    for (i, slot) in slots.iter_mut().enumerate() {
        *slot = merged.get(i).cloned();
    }

    debug_assert_eq!(
        totals_before,
        inventory_totals(slots),
        "inventory sorter must conserve every item"
    );
}

fn inventory_totals(slots: &[Option<SortableStack>]) -> BTreeMap<(String, u64), u64> {
    let mut totals: BTreeMap<(String, u64), u64> = BTreeMap::new();
    for stack in slots.iter().flatten() {
        *totals
            .entry((stack.item_id.clone(), stack.component_hash))
            .or_default() += u64::from(stack.count);
    }
    totals
}

// ===========================================================================
// 4. TerraBlender-style MultiNoise region blending
// ===========================================================================

/// Mod id reported for the vanilla baseline region.
pub const VANILLA_REGION_MOD_ID: &str = "minecraft:vanilla";
/// Default region cell edge in chunks — 32 chunks = 512 blocks.
pub const DEFAULT_REGION_SIZE_CHUNKS: i32 = 32;

/// One mod's biome region: when a coordinate's cell picks this region, its
/// `biome_overrides` map vanilla biome ids onto the mod's biomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiomeRegion {
    /// Mod identifier (also the region's sort key).
    pub mod_id: String,
    /// Relative weight in the per-cell pick; `0` never wins.
    pub weight: u32,
    /// Dimension this region applies to.
    pub dimension: String,
    /// `vanilla_biome -> mod_biome` substitutions.
    pub biome_overrides: BTreeMap<String, String>,
}

/// Deterministic multi-region biome blender: `(seed, dimension)` chooses one
/// winning [`BiomeRegion`] per jittered-grid Voronoi cell (default
/// [`DEFAULT_REGION_SIZE_CHUNKS`] = 32 chunks on a side), weighted by
/// `weight`, so several biome mods share a dimension without clobbering each
/// other. Vanilla keeps `vanilla_weight` as the baseline region.
#[derive(Clone, Debug)]
pub struct BiomeRegionBlender {
    vanilla_weight: u32,
    region_size_chunks: i32,
    /// Regions keyed by `mod_id` — BTreeMap order *is* the deterministic
    /// sorted order the weighted pick walks.
    regions: BTreeMap<String, BiomeRegion>,
}

impl BiomeRegionBlender {
    /// Blender with `vanilla_weight` baseline and the default region size.
    pub fn new(vanilla_weight: u32) -> Self {
        Self {
            vanilla_weight,
            region_size_chunks: DEFAULT_REGION_SIZE_CHUNKS,
            regions: BTreeMap::new(),
        }
    }

    /// Override the region cell edge in chunks (clamped to >= 1).
    pub fn with_region_size_chunks(mut self, region_size_chunks: i32) -> Self {
        self.region_size_chunks = region_size_chunks.max(1);
        self
    }

    /// The configured region cell edge in chunks.
    pub fn region_size_chunks(&self) -> i32 {
        self.region_size_chunks
    }

    /// Register (or replace) a region, keyed by `mod_id`.
    pub fn add_region(&mut self, region: BiomeRegion) {
        self.regions.insert(region.mod_id.clone(), region);
    }

    /// Registered regions in deterministic sorted order.
    pub fn regions(&self) -> &BTreeMap<String, BiomeRegion> {
        &self.regions
    }

    /// Winning region mod id at `(quart_x, quart_z)` — biome quart
    /// coordinates, four blocks per unit — in `dimension` under `seed`.
    /// Returns [`VANILLA_REGION_MOD_ID`] when the baseline bucket wins or no
    /// region matches the dimension.
    pub fn winning_region_id(
        &self,
        seed: i64,
        dimension: &str,
        quart_x: i32,
        quart_z: i32,
    ) -> &str {
        let (cell_x, cell_z) = self.owning_cell(seed, dimension, quart_x, quart_z);
        let hash = cell_hash(seed as u64, fnv1a64(dimension), cell_x, cell_z);
        let total: u64 = u64::from(self.vanilla_weight)
            + self
                .regions
                .values()
                .filter(|r| r.dimension == dimension)
                .map(|r| u64::from(r.weight))
                .sum::<u64>();
        if total == 0 {
            return VANILLA_REGION_MOD_ID;
        }
        let mut roll = hash % total;
        for region in self.regions.values() {
            if region.dimension != dimension {
                continue;
            }
            let w = u64::from(region.weight);
            if roll < w {
                return &region.mod_id;
            }
            roll -= w;
        }
        VANILLA_REGION_MOD_ID
    }

    /// The winning [`BiomeRegion`] at the coordinate, or `None` for vanilla.
    pub fn winning_region<'a>(
        &'a self,
        seed: i64,
        dimension: &str,
        quart_x: i32,
        quart_z: i32,
    ) -> Option<&'a BiomeRegion> {
        let id = self.winning_region_id(seed, dimension, quart_x, quart_z);
        self.regions.get(id)
    }

    /// Resolve the effective biome: the winning region's
    /// `biome_overrides[vanilla_biome]`, or `vanilla_biome` unchanged when
    /// vanilla wins or the region does not override that biome.
    pub fn resolve_biome<'a>(
        &'a self,
        seed: i64,
        dimension: &str,
        quart_x: i32,
        quart_z: i32,
        vanilla_biome: &'a str,
    ) -> &'a str {
        match self.winning_region(seed, dimension, quart_x, quart_z) {
            Some(region) => region
                .biome_overrides
                .get(vanilla_biome)
                .map(String::as_str)
                .unwrap_or(vanilla_biome),
            None => vanilla_biome,
        }
    }

    /// Jittered-grid Voronoi: find the cell whose deterministic interior site
    /// is nearest the (quart-center) point. Sites always stay inside their
    /// own cell, so the 3x3 neighborhood of the point's cell is exhaustive.
    fn owning_cell(&self, seed: i64, dimension: &str, quart_x: i32, quart_z: i32) -> (i64, i64) {
        let cell_blocks = i64::from(self.region_size_chunks) * 16;
        // Quart-center block coordinates keep the point inside its quart.
        let bx = i64::from(quart_x) * 4 + 2;
        let bz = i64::from(quart_z) * 4 + 2;
        let home_x = bx.div_euclid(cell_blocks);
        let home_z = bz.div_euclid(cell_blocks);
        let dim_hash = fnv1a64(dimension);
        let mut best = (home_x, home_z);
        let mut best_dist2 = i64::MAX;
        for dx in -1..=1i64 {
            for dz in -1..=1i64 {
                let cx = home_x + dx;
                let cz = home_z + dz;
                let h = cell_hash(seed as u64, dim_hash, cx, cz);
                let jx = site_jitter(h, cell_blocks);
                let jz = site_jitter(h.rotate_left(21), cell_blocks);
                let sx = cx * cell_blocks + jx;
                let sz = cz * cell_blocks + jz;
                let ddx = bx - sx;
                let ddz = bz - sz;
                let dist2 = ddx * ddx + ddz * ddz;
                // Deterministic tie-break on (cx, cz) keeps the winner total.
                if dist2 < best_dist2
                    || (dist2 == best_dist2 && (cx, cz) < best)
                {
                    best_dist2 = dist2;
                    best = (cx, cz);
                }
            }
        }
        best
    }
}

fn site_jitter(hash: u64, cell_blocks: i64) -> i64 {
    // Upper 32 bits -> uniform jitter inside [0, cell_blocks).
    ((hash >> 32) as i64).rem_euclid(cell_blocks)
}

fn cell_hash(seed: u64, dim_hash: u64, cell_x: i64, cell_z: i64) -> u64 {
    let mut h = seed ^ mix64(dim_hash);
    h ^= mix64((cell_x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    h = mix64(h);
    h ^= mix64((cell_z as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f));
    mix64(h)
}

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn fnv1a64(text: &str) -> u64 {
    megu_core::hash::fnv1a64_str(text)
}
