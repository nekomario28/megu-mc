// R7 GP-7 evaluators. Included by main.rs alongside BlockPos and the wire helpers.

#[cfg_attr(not(test), allow(dead_code))]
const WORLD_BORDER_MAX_SIZE: f64 = 59_999_968.0;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
struct WorldBorderState {
    center_x: f64,
    center_z: f64,
    old_size: f64,
    new_size: f64,
    lerp_start_tick: u64,
    lerp_duration_ticks: u64,
    damage_per_block: f64,
    damage_safe_zone: f64,
    warning_blocks: i32,
    warning_time_secs: i32,
    absolute_max_size: i32,
}

impl Default for WorldBorderState {
    fn default() -> Self {
        Self {
            center_x: 0.0,
            center_z: 0.0,
            old_size: WORLD_BORDER_MAX_SIZE,
            new_size: WORLD_BORDER_MAX_SIZE,
            lerp_start_tick: 0,
            lerp_duration_ticks: 0,
            damage_per_block: 0.2,
            damage_safe_zone: 5.0,
            warning_blocks: 5,
            warning_time_secs: 15,
            absolute_max_size: 29_999_984,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorldBorderState {
    fn lerp_to(&mut self, new_size: f64, now_tick: u64, duration_ticks: u64) {
        if new_size.is_nan() {
            return;
        }
        self.old_size = self.current_size(now_tick);
        self.new_size = new_size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        self.lerp_start_tick = now_tick;
        self.lerp_duration_ticks = duration_ticks;
    }

    fn current_size(&self, now_tick: u64) -> f64 {
        let elapsed = now_tick.saturating_sub(self.lerp_start_tick);
        let target = self.new_size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        if elapsed >= self.lerp_duration_ticks {
            return target;
        }
        let start = self.old_size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        let fraction = elapsed as f64 / self.lerp_duration_ticks as f64;
        start + (target - start) * fraction
    }

    fn bounds(&self, now_tick: u64) -> (f64, f64, f64, f64) {
        let radius = self.current_size(now_tick) * 0.5;
        let limit = f64::from(self.absolute_max_size.max(0));
        (
            (self.center_x - radius).clamp(-limit, limit),
            (self.center_z - radius).clamp(-limit, limit),
            (self.center_x + radius).clamp(-limit, limit),
            (self.center_z + radius).clamp(-limit, limit),
        )
    }

    fn distance_to_border(&self, x: f64, z: f64, now_tick: u64) -> f64 {
        let (min_x, min_z, max_x, max_z) = self.bounds(now_tick);
        (x - min_x).min(max_x - x).min(z - min_z).min(max_z - z)
    }

    fn border_damage_for_pos(&self, x: f64, z: f64, now_tick: u64) -> f32 {
        let distance = self.distance_to_border(x, z, now_tick);
        if distance < -self.damage_safe_zone && self.damage_per_block > 0.0 {
            let damage = (-distance - self.damage_safe_zone) * self.damage_per_block;
            (damage.floor() as i32).max(1) as f32
        } else {
            0.0
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
struct AdvancementDefinition {
    id: &'static str,
    parent: Option<&'static str>,
    // All groups must pass. Any one criterion satisfies its group.
    requirements: &'static [&'static [&'static str]],
}

#[cfg_attr(not(test), allow(dead_code))]
const CORE_ADVANCEMENTS: &[AdvancementDefinition] = &[
    AdvancementDefinition {
        id: "minecraft:story/root",
        parent: None,
        requirements: &[&["crafting_table"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/mine_stone",
        parent: Some("minecraft:story/root"),
        requirements: &[&["get_stone"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/upgrade_tools",
        parent: Some("minecraft:story/mine_stone"),
        requirements: &[&["stone_pickaxe"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/smelt_iron",
        parent: Some("minecraft:story/upgrade_tools"),
        requirements: &[&["iron"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/obtain_armor",
        parent: Some("minecraft:story/smelt_iron"),
        requirements: &[&[
            "iron_helmet",
            "iron_chestplate",
            "iron_leggings",
            "iron_boots",
        ]],
    },
    AdvancementDefinition {
        id: "minecraft:story/lava_bucket",
        parent: Some("minecraft:story/smelt_iron"),
        requirements: &[&["lava_bucket"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/iron_tools",
        parent: Some("minecraft:story/smelt_iron"),
        requirements: &[&["iron_pickaxe"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/deflect_arrow",
        parent: Some("minecraft:story/obtain_armor"),
        requirements: &[&["deflected_projectile"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/form_obsidian",
        parent: Some("minecraft:story/lava_bucket"),
        requirements: &[&["obsidian"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/mine_diamond",
        parent: Some("minecraft:story/iron_tools"),
        requirements: &[&["diamond"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/enter_the_nether",
        parent: Some("minecraft:story/form_obsidian"),
        requirements: &[&["entered_nether"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/shiny_gear",
        parent: Some("minecraft:story/mine_diamond"),
        requirements: &[&[
            "diamond_helmet",
            "diamond_chestplate",
            "diamond_leggings",
            "diamond_boots",
        ]],
    },
    AdvancementDefinition {
        id: "minecraft:story/enchant_item",
        parent: Some("minecraft:story/mine_diamond"),
        requirements: &[&["enchanted_item"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/cure_zombie_villager",
        parent: Some("minecraft:story/enter_the_nether"),
        requirements: &[&["cured_zombie"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/follow_ender_eye",
        parent: Some("minecraft:story/enter_the_nether"),
        requirements: &[&["in_stronghold"]],
    },
    AdvancementDefinition {
        id: "minecraft:story/enter_the_end",
        parent: Some("minecraft:story/follow_ender_eye"),
        requirements: &[&["entered_end"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/root",
        parent: None,
        requirements: &[&["entered_nether"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/root",
        parent: None,
        requirements: &[&["entered_end"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/kill_dragon",
        parent: Some("minecraft:end/root"),
        requirements: &[&["killed_dragon"]],
    },
];

#[cfg_attr(not(test), allow(dead_code))]
type AdvancementCriteriaProgress = std::collections::BTreeMap<&'static str, i64>;
#[cfg_attr(not(test), allow(dead_code))]
type PlayerAdvancementProgress = HashMap<&'static str, AdvancementCriteriaProgress>;

#[cfg_attr(not(test), allow(dead_code))]
struct AdvancementTracker {
    // Definitions are ordered parent before child.
    definitions: &'static [AdvancementDefinition],
    progress: HashMap<String, PlayerAdvancementProgress>,
    sent_progress: HashMap<String, PlayerAdvancementProgress>,
}

impl Default for AdvancementTracker {
    fn default() -> Self {
        Self {
            definitions: CORE_ADVANCEMENTS,
            progress: HashMap::new(),
            sent_progress: HashMap::new(),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl AdvancementTracker {
    fn definition(&self, id: &str) -> Option<&'static AdvancementDefinition> {
        self.definitions
            .iter()
            .find(|definition| definition.id == id)
    }

    fn grant_criterion(&mut self, player: &str, id: &str, criterion: &str, time_ms: i64) -> bool {
        let Some(definition) = self.definition(id) else {
            return false;
        };
        let Some(&name) = definition
            .requirements
            .iter()
            .copied()
            .flatten()
            .find(|&&name| name == criterion)
        else {
            return false;
        };
        let criteria = self
            .progress
            .entry(player.to_owned())
            .or_default()
            .entry(definition.id)
            .or_default();
        if let std::collections::btree_map::Entry::Vacant(entry) = criteria.entry(name) {
            entry.insert(time_ms);
            return true;
        }
        false
    }

    fn revoke_criterion(&mut self, player: &str, id: &str, criterion: &str) -> bool {
        let Some(progress) = self.progress.get_mut(player) else {
            return false;
        };
        let Some(criteria) = progress.get_mut(id) else {
            return false;
        };
        let revoked = criteria.remove(criterion).is_some();
        if criteria.is_empty() {
            progress.remove(id);
        }
        if progress.is_empty() {
            self.progress.remove(player);
        }
        revoked
    }

    fn grant_only(&mut self, player: &str, id: &str, time_ms: i64) -> usize {
        self.definition(id).map_or(0, |definition| {
            self.grant_definition(player, definition, time_ms)
        })
    }

    fn grant_from(&mut self, player: &str, id: &str, time_ms: i64) -> usize {
        let mut granted = 0;
        for definition in self.definitions {
            if self.has_ancestor(definition.id, id) {
                granted += self.grant_definition(player, definition, time_ms);
            }
        }
        granted
    }

    fn grant_through(&mut self, player: &str, id: &str, time_ms: i64) -> usize {
        let mut granted = self.grant_from(player, id, time_ms);
        let mut parent = self.definition(id).and_then(|definition| definition.parent);
        while let Some(definition) = parent.and_then(|id| self.definition(id)) {
            granted += self.grant_definition(player, definition, time_ms);
            parent = definition.parent;
        }
        granted
    }

    fn grant_everything(&mut self, player: &str, time_ms: i64) -> usize {
        self.definitions
            .iter()
            .map(|definition| self.grant_definition(player, definition, time_ms))
            .sum()
    }

    fn grant_definition(
        &mut self,
        player: &str,
        definition: &AdvancementDefinition,
        time_ms: i64,
    ) -> usize {
        definition
            .requirements
            .iter()
            .copied()
            .flatten()
            .filter(|name| self.grant_criterion(player, definition.id, name, time_ms))
            .count()
    }

    fn has_ancestor(&self, mut id: &str, ancestor: &str) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            let Some(parent) = self.definition(id).and_then(|definition| definition.parent) else {
                return false;
            };
            id = parent;
        }
    }

    fn completed(&self, player: &str, id: &str) -> bool {
        let Some(progress) = self.progress.get(player) else {
            return false;
        };
        let Some(mut definition) = self.definition(id) else {
            return false;
        };
        loop {
            if !progress.get(definition.id).is_some_and(|criteria| {
                definition
                    .requirements
                    .iter()
                    .all(|group| group.iter().any(|name| criteria.contains_key(name)))
            }) {
                return false;
            }
            let Some(parent) = definition.parent else {
                return true;
            };
            let Some(parent_definition) = self.definition(parent) else {
                return false;
            };
            definition = parent_definition;
        }
    }

    // Send reset on join or after a failed delivery. A revoked parent hides its descendants'
    // progress in the delta while retaining the criteria they already earned on the server.
    fn take_delta(&mut self, player: &str, reset: bool) -> Option<AdvancementDelta> {
        let mut delta = AdvancementDelta {
            reset,
            added: if reset { self.definitions } else { &[] },
            progress: Vec::new(),
        };
        let mut visible = PlayerAdvancementProgress::new();
        let before = if reset {
            None
        } else {
            self.sent_progress.get(player)
        };
        for definition in self.definitions {
            let current = self
                .progress
                .get(player)
                .and_then(|progress| progress.get(definition.id))
                .filter(|_| {
                    definition
                        .parent
                        .is_none_or(|parent| self.completed(player, parent))
                });
            let previous = before.and_then(|progress| progress.get(definition.id));
            if current != previous {
                delta
                    .progress
                    .push((definition.id, current.cloned().unwrap_or_default()));
            }
            if let Some(criteria) = current {
                visible.insert(definition.id, criteria.clone());
            }
        }
        if visible.is_empty() {
            self.sent_progress.remove(player);
        } else {
            self.sent_progress.insert(player.to_owned(), visible);
        }
        (reset || !delta.progress.is_empty()).then_some(delta)
    }
}

// Protocol 767 clientbound Update Advancements.
#[cfg_attr(not(test), allow(dead_code))]
const UPDATE_ADVANCEMENTS_PACKET_ID: i64 = 0x74;

#[cfg_attr(not(test), allow(dead_code))]
struct AdvancementDelta {
    reset: bool,
    added: &'static [AdvancementDefinition],
    progress: Vec<(&'static str, AdvancementCriteriaProgress)>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl AdvancementDelta {
    fn into_packet(self) -> (i64, Vec<u8>) {
        let mut out = vec![u8::from(self.reset)];
        write_varint(&mut out, self.added.len() as i64);
        for definition in self.added {
            write_str(&mut out, definition.id);
            out.push(u8::from(definition.parent.is_some()));
            if let Some(parent) = definition.parent {
                write_str(&mut out, parent);
            }
            out.push(0); // Optional display data is absent.
            write_varint(&mut out, definition.requirements.len() as i64);
            for group in definition.requirements {
                write_varint(&mut out, group.len() as i64);
                for criterion in *group {
                    write_str(&mut out, criterion);
                }
            }
            out.push(1); // Core definitions send telemetry in 1.21.1.
        }
        write_varint(&mut out, 0); // The built-in definition set is fixed.
        write_varint(&mut out, self.progress.len() as i64);
        for (id, criteria) in self.progress {
            write_str(&mut out, id);
            write_varint(&mut out, criteria.len() as i64);
            for (criterion, time_ms) in criteria {
                write_str(&mut out, criterion);
                out.push(1);
                out.extend_from_slice(&time_ms.to_be_bytes());
            }
        }
        (UPDATE_ADVANCEMENTS_PACKET_ID, out)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardCriteria {
    Dummy,
    Trigger,
    DeathCount,
    PlayerKillCount,
    TotalKillCount,
    Health,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ScoreboardCriteria {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "dummy" => Self::Dummy,
            "trigger" => Self::Trigger,
            "deathCount" => Self::DeathCount,
            "playerKillCount" => Self::PlayerKillCount,
            "totalKillCount" => Self::TotalKillCount,
            "health" => Self::Health,
            _ => return None,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardRenderType {
    Integer,
    Hearts,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScoreboardObjective {
    name: String,
    criteria: ScoreboardCriteria,
    display_name: String,
    render_type: ScoreboardRenderType,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardDisplaySlot {
    List,
    Sidebar,
    BelowName,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardOperation {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Assign,
    Min,
    Max,
    Swap,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ScoreboardOperation {
    fn parse(symbol: &str) -> Option<Self> {
        Some(match symbol {
            "+=" => Self::Add,
            "-=" => Self::Subtract,
            "*=" => Self::Multiply,
            "/=" => Self::Divide,
            "%=" => Self::Modulo,
            "=" => Self::Assign,
            "<" => Self::Min,
            ">" => Self::Max,
            "><" => Self::Swap,
            _ => return None,
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardError {
    DuplicateObjective,
    UnknownObjective,
    ReadOnlyObjective,
    NotTrigger,
    TriggerLocked,
    DivisionByZero,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct ScoreboardScore {
    value: i32,
    trigger_enabled: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Default)]
struct ScoreboardEngine {
    objectives: HashMap<String, ScoreboardObjective>,
    display_slots: [Option<String>; 3],
    scores: HashMap<String, HashMap<String, ScoreboardScore>>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ScoreboardEngine {
    fn add_objective(&mut self, objective: ScoreboardObjective) -> Result<(), ScoreboardError> {
        if self.objectives.contains_key(&objective.name) {
            return Err(ScoreboardError::DuplicateObjective);
        }
        self.objectives.insert(objective.name.clone(), objective);
        Ok(())
    }

    fn objective(&self, name: &str) -> Option<&ScoreboardObjective> {
        self.objectives.get(name)
    }

    fn remove_objective(&mut self, name: &str) -> bool {
        if self.objectives.remove(name).is_none() {
            return false;
        }
        self.scores.remove(name);
        for slot in &mut self.display_slots {
            if slot.as_deref() == Some(name) {
                *slot = None;
            }
        }
        true
    }

    fn set_display(
        &mut self,
        slot: ScoreboardDisplaySlot,
        name: Option<&str>,
    ) -> Result<(), ScoreboardError> {
        if let Some(name) = name {
            self.criteria(name)?;
        }
        self.display_slots[slot as usize] = name.map(str::to_owned);
        Ok(())
    }

    fn display(&self, slot: ScoreboardDisplaySlot) -> Option<&str> {
        self.display_slots[slot as usize].as_deref()
    }

    fn set(&mut self, player: &str, objective: &str, value: i32) -> Result<(), ScoreboardError> {
        self.require_writable(objective)?;
        self.score_mut(player, objective).value = value;
        Ok(())
    }

    fn add(&mut self, player: &str, objective: &str, value: i32) -> Result<i32, ScoreboardError> {
        self.require_writable(objective)?;
        let score = self.score_mut(player, objective);
        score.value = score.value.wrapping_add(value);
        Ok(score.value)
    }

    fn remove(
        &mut self,
        player: &str,
        objective: &str,
        value: i32,
    ) -> Result<i32, ScoreboardError> {
        self.add(player, objective, value.wrapping_neg())
    }

    fn reset(&mut self, player: &str, objective: Option<&str>) -> Result<usize, ScoreboardError> {
        if let Some(objective) = objective {
            self.criteria(objective)?;
            let removed = self
                .scores
                .get_mut(objective)
                .and_then(|scores| scores.remove(player));
            return Ok(usize::from(removed.is_some()));
        }
        Ok(self
            .scores
            .values_mut()
            .filter_map(|scores| scores.remove(player))
            .count())
    }

    fn get(&self, player: &str, objective: &str) -> Option<i32> {
        self.scores
            .get(objective)?
            .get(player)
            .map(|score| score.value)
    }

    fn update_health(&mut self, player: &str, health: i32) {
        for (name, objective) in &self.objectives {
            if objective.criteria == ScoreboardCriteria::Health {
                let score = self
                    .scores
                    .entry(name.clone())
                    .or_default()
                    .entry(player.to_owned())
                    .or_default();
                score.value = health;
            }
        }
    }

    fn enable_trigger(&mut self, player: &str, objective: &str) -> Result<(), ScoreboardError> {
        self.require_trigger(objective)?;
        self.score_mut(player, objective).trigger_enabled = true;
        Ok(())
    }

    fn trigger_add(
        &mut self,
        player: &str,
        objective: &str,
        value: i32,
    ) -> Result<i32, ScoreboardError> {
        self.require_trigger(objective)?;
        let score = self
            .scores
            .get_mut(objective)
            .and_then(|scores| scores.get_mut(player))
            .filter(|score| score.trigger_enabled)
            .ok_or(ScoreboardError::TriggerLocked)?;
        score.trigger_enabled = false;
        score.value = score.value.wrapping_add(value);
        Ok(score.value)
    }

    fn operation(
        &mut self,
        target: (&str, &str),
        operation: ScoreboardOperation,
        source: (&str, &str),
    ) -> Result<i32, ScoreboardError> {
        self.require_writable(target.1)?;
        self.criteria(source.1)?;
        if operation == ScoreboardOperation::Swap {
            self.require_writable(source.1)?;
        }
        let lhs = self.get(target.0, target.1).unwrap_or(0);
        let rhs = self.get(source.0, source.1).unwrap_or(0);
        let value = match operation {
            ScoreboardOperation::Add => lhs.wrapping_add(rhs),
            ScoreboardOperation::Subtract => lhs.wrapping_sub(rhs),
            ScoreboardOperation::Multiply => lhs.wrapping_mul(rhs),
            ScoreboardOperation::Divide | ScoreboardOperation::Modulo => {
                if rhs == 0 {
                    return Err(ScoreboardError::DivisionByZero);
                }
                // i64 permits Java's MIN_VALUE / -1 result to wrap on conversion back.
                let (lhs, rhs) = (i64::from(lhs), i64::from(rhs));
                let quotient = if rhs < 0 {
                    (-lhs).div_euclid(-rhs)
                } else {
                    lhs.div_euclid(rhs)
                };
                match operation {
                    ScoreboardOperation::Divide => quotient as i32,
                    _ => (lhs - quotient * rhs) as i32,
                }
            }
            ScoreboardOperation::Assign => rhs,
            ScoreboardOperation::Min => lhs.min(rhs),
            ScoreboardOperation::Max => lhs.max(rhs),
            ScoreboardOperation::Swap => rhs,
        };
        // Vanilla creates a zero-valued score for a previously unscored source.
        let source_score = self.score_mut(source.0, source.1);
        if operation == ScoreboardOperation::Swap {
            source_score.value = lhs;
        }
        self.score_mut(target.0, target.1).value = value;
        Ok(value)
    }

    fn score_mut(&mut self, player: &str, objective: &str) -> &mut ScoreboardScore {
        self.scores
            .entry(objective.to_owned())
            .or_default()
            .entry(player.to_owned())
            .or_default()
    }

    fn criteria(&self, objective: &str) -> Result<ScoreboardCriteria, ScoreboardError> {
        self.objective(objective)
            .map(|objective| objective.criteria)
            .ok_or(ScoreboardError::UnknownObjective)
    }

    fn require_writable(&self, objective: &str) -> Result<(), ScoreboardError> {
        if self.criteria(objective)? == ScoreboardCriteria::Health {
            return Err(ScoreboardError::ReadOnlyObjective);
        }
        Ok(())
    }

    fn require_trigger(&self, objective: &str) -> Result<(), ScoreboardError> {
        if self.criteria(objective)? != ScoreboardCriteria::Trigger {
            return Err(ScoreboardError::NotTrigger);
        }
        Ok(())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
const MODIFICATION_BLOCK_LIMIT: usize = 32_768;

#[cfg_attr(not(test), allow(dead_code))]
const COMMAND_AIR_STATE: u32 = 0;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillMode {
    Replace { filter: Option<u32> },
    Outline,
    Hollow,
    Keep,
    Destroy,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneFilter {
    Replace,
    Masked,
    Filtered(u32),
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneMode {
    Normal,
    Force,
    Move,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockModificationError {
    VolumeLimit { volume: u128, limit: usize },
    DestinationOverflow,
    OverlappingClone,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockModification {
    pos: BlockPos,
    state: u32,
    destroyed: Option<u32>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
struct ModificationBox {
    min: BlockPos,
    max: BlockPos,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ModificationBox {
    fn between(a: BlockPos, b: BlockPos) -> Self {
        Self {
            min: (a.0.min(b.0), a.1.min(b.1), a.2.min(b.2)),
            max: (a.0.max(b.0), a.1.max(b.1), a.2.max(b.2)),
        }
    }

    fn check_volume(self, limit: usize) -> Result<(), BlockModificationError> {
        let (x, y, z) = self.offset(self.max);
        let volume = (x as u128 + 1) * (y as u128 + 1) * (z as u128 + 1);
        if volume > limit as u128 {
            return Err(BlockModificationError::VolumeLimit { volume, limit });
        }
        Ok(())
    }

    fn offset(self, (x, y, z): BlockPos) -> (i64, i64, i64) {
        (
            i64::from(x) - i64::from(self.min.0),
            i64::from(y) - i64::from(self.min.1),
            i64::from(z) - i64::from(self.min.2),
        )
    }

    fn at_offset(self, (x, y, z): (i64, i64, i64)) -> Option<BlockPos> {
        Some((
            i32::try_from(i64::from(self.min.0) + x).ok()?,
            i32::try_from(i64::from(self.min.1) + y).ok()?,
            i32::try_from(i64::from(self.min.2) + z).ok()?,
        ))
    }

    fn intersects(self, other: Self) -> bool {
        self.min.0 <= other.max.0
            && self.max.0 >= other.min.0
            && self.min.1 <= other.max.1
            && self.max.1 >= other.min.1
            && self.min.2 <= other.max.2
            && self.max.2 >= other.min.2
    }

    fn boundary(self, (x, y, z): BlockPos) -> bool {
        x == self.min.0
            || x == self.max.0
            || y == self.min.1
            || y == self.max.1
            || z == self.min.2
            || z == self.max.2
    }

    fn positions(self) -> impl Iterator<Item = BlockPos> {
        (self.min.2..=self.max.2).flat_map(move |z| {
            (self.min.1..=self.max.1)
                .flat_map(move |y| (self.min.0..=self.max.0).map(move |x| (x, y, z)))
        })
    }
}

// The caller applies the returned changes and handles block drops and world updates.
#[cfg_attr(not(test), allow(dead_code))]
fn eval_fill(
    from: BlockPos,
    to: BlockPos,
    state: u32,
    mode: FillMode,
    block_limit: usize,
    mut read: impl FnMut(BlockPos) -> u32,
) -> Result<Vec<BlockModification>, BlockModificationError> {
    let region = ModificationBox::between(from, to);
    region.check_volume(block_limit)?;
    let mut changes = Vec::new();
    for pos in region.positions() {
        let state = match mode {
            FillMode::Outline if !region.boundary(pos) => continue,
            FillMode::Hollow if !region.boundary(pos) => COMMAND_AIR_STATE,
            _ => state,
        };
        let previous = read(pos);
        match mode {
            FillMode::Replace {
                filter: Some(filter),
            } if previous != filter => continue,
            FillMode::Keep if previous != COMMAND_AIR_STATE => continue,
            _ => {}
        }
        let destroyed =
            (mode == FillMode::Destroy && previous != COMMAND_AIR_STATE).then_some(previous);
        if state != previous || destroyed.is_some() {
            changes.push(BlockModification {
                pos,
                state,
                destroyed,
            });
        }
    }
    Ok(changes)
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_clone(
    from: BlockPos,
    to: BlockPos,
    destination: BlockPos,
    filter: CloneFilter,
    mode: CloneMode,
    block_limit: usize,
    mut read: impl FnMut(BlockPos) -> u32,
) -> Result<Vec<BlockModification>, BlockModificationError> {
    let source = ModificationBox::between(from, to);
    source.check_volume(block_limit)?;
    let mut target = ModificationBox::between(destination, destination);
    target.max = target
        .at_offset(source.offset(source.max))
        .ok_or(BlockModificationError::DestinationOverflow)?;
    let overlap = source.intersects(target);
    if overlap && mode == CloneMode::Normal {
        return Err(BlockModificationError::OverlappingClone);
    }

    // Capture the entire selection before returning any write to the caller.
    let snapshot: Vec<_> = source
        .positions()
        .zip(target.positions())
        .filter_map(|(source, destination)| {
            let state = read(source);
            let selected = match filter {
                CloneFilter::Replace => true,
                CloneFilter::Masked => state != COMMAND_AIR_STATE,
                CloneFilter::Filtered(filter_state) => state == filter_state,
            };
            selected.then_some((source, destination, state))
        })
        .collect();
    let mut changes = Vec::new();
    if mode == CloneMode::Move {
        let written: HashSet<_> = if overlap {
            snapshot
                .iter()
                .map(|&(_, destination, _)| destination)
                .collect()
        } else {
            HashSet::new()
        };
        for &(source, _, state) in &snapshot {
            if state != COMMAND_AIR_STATE && !written.contains(&source) {
                changes.push(BlockModification {
                    pos: source,
                    state: COMMAND_AIR_STATE,
                    destroyed: None,
                });
            }
        }
    }
    changes.extend(
        snapshot
            .into_iter()
            .map(|(_, pos, state)| BlockModification {
                pos,
                state,
                destroyed: None,
            }),
    );
    Ok(changes)
}

#[cfg(test)]
mod world_mechanisms_tests {
    use super::*;

    const ROOT: &str = "minecraft:story/root";
    const STONE: &str = "minecraft:story/mine_stone";
    const TOOLS: &str = "minecraft:story/upgrade_tools";
    const IRON: &str = "minecraft:story/smelt_iron";
    const ARMOR: &str = "minecraft:story/obtain_armor";
    const NETHER: &str = "minecraft:story/enter_the_nether";
    const END: &str = "minecraft:story/enter_the_end";
    const DRAGON: &str = "minecraft:end/kill_dragon";

    #[test]
    fn world_border_shrinks_expands_and_restarts_from_current_size() {
        let mut border = WorldBorderState::default();
        assert_eq!(border.current_size(0), 59_999_968.0);
        assert_eq!(border.warning_blocks, 5);
        assert_eq!(border.warning_time_secs, 15);
        border.lerp_to(100.0, 0, 0);
        border.lerp_to(20.0, 10, 20);
        for (tick, expected) in [(0, 100.0), (10, 100.0), (20, 60.0), (30, 20.0), (40, 20.0)] {
            assert_eq!(border.current_size(tick), expected);
        }
        border.lerp_to(100.0, 30, 20);
        for (tick, expected) in [(30, 20.0), (40, 60.0), (50, 100.0)] {
            assert_eq!(border.current_size(tick), expected);
        }
        border.lerp_to(0.0, 40, 10);
        assert_eq!(border.current_size(40), 60.0);
        assert_eq!(border.current_size(50), 1.0);
        border.lerp_to(f64::INFINITY, 50, 0);
        assert_eq!(border.current_size(50), WORLD_BORDER_MAX_SIZE);
    }

    #[test]
    fn world_border_clips_bounds_and_applies_safe_zone_and_minimum_damage() {
        let mut border = WorldBorderState {
            center_x: 10.0,
            center_z: -10.0,
            ..WorldBorderState::default()
        };
        border.lerp_to(20.0, 0, 0);
        assert_eq!(border.bounds(0), (0.0, -20.0, 20.0, 0.0));
        assert_eq!(border.distance_to_border(10.0, -10.0, 0), 10.0);
        assert_eq!(border.distance_to_border(0.0, -10.0, 0), 0.0);
        assert_eq!(border.distance_to_border(23.0, 4.0, 0), -4.0);
        assert_eq!(border.border_damage_for_pos(10.0, -10.0, 0), 0.0);
        assert_eq!(border.border_damage_for_pos(25.0, -10.0, 0), 0.0);
        assert_eq!(border.border_damage_for_pos(26.0, -10.0, 0), 1.0);
        assert_eq!(border.border_damage_for_pos(35.0, -10.0, 0), 2.0);
        border.damage_per_block = 0.1;
        assert_eq!(border.border_damage_for_pos(35.0, -10.0, 0), 1.0);
        assert_eq!(border.border_damage_for_pos(40.0, -10.0, 0), 1.0);
        assert_eq!(border.border_damage_for_pos(45.0, -10.0, 0), 2.0);
        border.damage_per_block = 0.0;
        assert_eq!(border.border_damage_for_pos(45.0, -10.0, 0), 0.0);
        border.absolute_max_size = 12;
        assert_eq!(border.bounds(0), (0.0, -12.0, 12.0, 0.0));
        border.center_x = 100.0;
        assert_eq!(border.bounds(0), (12.0, -12.0, 12.0, 0.0));
    }

    #[test]
    fn advancements_require_every_group_and_accept_any_member_of_a_group() {
        const MULTI_GROUP: &[AdvancementDefinition] = &[AdvancementDefinition {
            id: "example:both",
            parent: None,
            requirements: &[&["a", "b"], &["c", "d"]],
        }];
        let mut tracker = AdvancementTracker {
            definitions: MULTI_GROUP,
            ..AdvancementTracker::default()
        };
        let id = MULTI_GROUP[0].id;
        assert!(!tracker.grant_criterion("Alex", "example:missing", "b", 10));
        assert!(!tracker.grant_criterion("Alex", id, "unlisted", 10));
        assert!(tracker.grant_criterion("Alex", id, "b", 10));
        assert!(!tracker.grant_criterion("Alex", id, "b", 11));
        assert!(!tracker.completed("Alex", id));
        assert!(tracker.grant_criterion("Alex", id, "d", 12));
        assert!(tracker.completed("Alex", id));
        assert!(!tracker.completed("Steve", id));
        assert!(tracker.revoke_criterion("Alex", id, "b"));
        assert!(!tracker.revoke_criterion("Alex", id, "b"));
        assert!(!tracker.completed("Alex", id));
        assert!(tracker.grant_criterion("Alex", id, "a", 13));
        assert!(tracker.completed("Alex", id));
    }

    #[test]
    fn advancements_story_progression_follows_each_vanilla_parent() {
        let mut tracker = AdvancementTracker::default();
        let path = [
            (ROOT, "crafting_table"),
            (STONE, "get_stone"),
            (TOOLS, "stone_pickaxe"),
            (IRON, "iron"),
            ("minecraft:story/lava_bucket", "lava_bucket"),
            ("minecraft:story/form_obsidian", "obsidian"),
            (NETHER, "entered_nether"),
            ("minecraft:story/follow_ender_eye", "in_stronghold"),
            (END, "entered_end"),
            ("minecraft:end/root", "entered_end"),
            (DRAGON, "killed_dragon"),
        ];
        // An earned child criterion becomes effective once its parent completes.
        assert!(tracker.grant_criterion("Alex", STONE, "get_stone", 0));
        assert!(!tracker.completed("Alex", STONE));
        for (i, (id, criterion)) in path.into_iter().enumerate() {
            assert!(!tracker.completed("Alex", id) || id == STONE);
            assert_eq!(
                tracker.grant_criterion("Alex", id, criterion, i as i64),
                id != STONE
            );
            assert!(tracker.completed("Alex", id), "{id}");
        }
        assert!(tracker.grant_criterion("Alex", ARMOR, "iron_boots", 20));
        assert!(tracker.completed("Alex", ARMOR));
        assert_eq!(
            tracker.definition(DRAGON).unwrap().parent,
            Some("minecraft:end/root")
        );
        assert!(!tracker.completed("Alex", "minecraft:nether/root"));
    }

    #[test]
    fn advancements_grant_scopes_and_revocation_cascade() {
        let mut tracker = AdvancementTracker::default();
        assert!(tracker.grant_from("Alex", NETHER, 10) > 0);
        assert!(!tracker.completed("Alex", NETHER));
        assert!(tracker.grant_through("Alex", NETHER, 11) > 0);
        assert!(tracker.completed("Alex", END));
        assert!(tracker.completed("Alex", "minecraft:story/cure_zombie_villager"));
        assert!(!tracker.completed("Alex", ARMOR));
        assert!(tracker.grant_only("Alex", ARMOR, 12) > 0);
        assert!(tracker.completed("Alex", ARMOR));
        assert!(!tracker.completed("Alex", DRAGON));

        let initial = tracker.take_delta("Alex", true).unwrap();
        assert_eq!(initial.added.len(), 19);
        assert!(tracker.take_delta("Alex", false).is_none());
        assert!(tracker.revoke_criterion("Alex", IRON, "iron"));
        assert!(!tracker.completed("Alex", END));
        assert!(tracker.completed("Alex", TOOLS));

        let revoked = tracker.take_delta("Alex", false).unwrap();
        assert!(revoked.added.is_empty());
        assert!(revoked
            .progress
            .iter()
            .any(|(id, c)| *id == END && c.is_empty()));
        assert!(tracker.take_delta("Steve", false).is_none());

        assert!(tracker.grant_criterion("Alex", IRON, "iron", 15));
        assert!(tracker.completed("Alex", END));
        let restored = tracker.take_delta("Alex", false).unwrap();
        assert!(restored
            .progress
            .iter()
            .any(|(id, c)| *id == END && c.get("entered_end") == Some(&10)));
        assert!(tracker.grant_everything("Steve", 16) > 0);
        for definition in CORE_ADVANCEMENTS {
            assert!(
                tracker.completed("Steve", definition.id),
                "{}",
                definition.id
            );
        }
        assert_eq!(tracker.grant_everything("Steve", 17), 0);
    }

    fn read_advancement_string<'a>(packet: &mut &'a [u8]) -> &'a str {
        let len = read_varint(packet).unwrap() as usize;
        let (string, remaining) = packet.split_at(len);
        *packet = remaining;
        std::str::from_utf8(string).unwrap()
    }

    #[test]
    fn advancements_delta_uses_protocol_767_reset_progress_and_revoke_layout() {
        let mut tracker = AdvancementTracker::default();
        let (id, reset) = tracker.take_delta("Alex", true).unwrap().into_packet();
        assert_eq!(id, 0x74);
        assert_eq!(reset[0], 1);
        let mut packet = &reset[1..];
        assert_eq!(read_varint(&mut packet), Some(19));
        for definition in CORE_ADVANCEMENTS {
            assert_eq!(read_advancement_string(&mut packet), definition.id);
            let (has_parent, remaining) = packet.split_at(1);
            packet = remaining;
            assert_eq!(has_parent, &[u8::from(definition.parent.is_some())]);
            if let Some(parent) = definition.parent {
                assert_eq!(read_advancement_string(&mut packet), parent);
            }
            assert_eq!(packet[0], 0);
            packet = &packet[1..];
            assert_eq!(
                read_varint(&mut packet),
                Some(definition.requirements.len() as i64)
            );
            for group in definition.requirements {
                assert_eq!(read_varint(&mut packet), Some(group.len() as i64));
                for criterion in *group {
                    assert_eq!(read_advancement_string(&mut packet), *criterion);
                }
            }
            assert_eq!(packet[0], 1);
            packet = &packet[1..];
        }
        assert_eq!(packet, &[0, 0]);

        tracker.grant_only("Alex", ROOT, 1_234_567);
        let (id, progress) = tracker.take_delta("Alex", false).unwrap().into_packet();
        assert_eq!(id, 0x74);
        assert_eq!(&progress[..4], &[0, 0, 0, 1]);
        let mut packet = &progress[4..];
        assert_eq!(read_advancement_string(&mut packet), ROOT);
        assert_eq!(read_varint(&mut packet), Some(1));
        assert_eq!(read_advancement_string(&mut packet), "crafting_table");
        assert_eq!(packet[0], 1);
        assert_eq!(&packet[1..], &1_234_567i64.to_be_bytes());

        tracker.revoke_criterion("Alex", ROOT, "crafting_table");
        let (_, revoke) = tracker.take_delta("Alex", false).unwrap().into_packet();
        assert_eq!(&revoke[..4], &[0, 0, 0, 1]);
        let mut packet = &revoke[4..];
        assert_eq!(read_advancement_string(&mut packet), ROOT);
        assert_eq!(packet, &[0]);
    }

    fn objective(name: &str, criteria: ScoreboardCriteria) -> ScoreboardObjective {
        ScoreboardObjective {
            name: name.to_owned(),
            criteria,
            display_name: format!("{name} display"),
            render_type: if criteria == ScoreboardCriteria::Health {
                ScoreboardRenderType::Hearts
            } else {
                ScoreboardRenderType::Integer
            },
        }
    }

    #[test]
    fn scoreboard_operations_wrap_and_use_signed_floor_division() {
        let mut board = ScoreboardEngine::default();
        board
            .add_objective(objective("a", ScoreboardCriteria::Dummy))
            .unwrap();
        board
            .add_objective(objective("b", ScoreboardCriteria::Dummy))
            .unwrap();
        let target = ("Alex", "a");
        let source = ("Steve", "b");
        for (symbol, lhs, rhs, expected) in [
            ("+=", i32::MAX, 1, i32::MIN),
            ("-=", i32::MIN, 1, i32::MAX),
            ("*=", i32::MAX, 2, -2),
            ("/=", -7, 3, -3),
            ("/=", 7, -3, -3),
            ("/=", i32::MIN, -1, i32::MIN),
            ("%=", -7, 3, 2),
            ("%=", 7, -3, -2),
            ("=", 3, 7, 7),
            ("<", 7, 3, 3),
            (">", 3, 7, 7),
            ("><", 8, 4, 4),
        ] {
            board.set(target.0, target.1, lhs).unwrap();
            board.set(source.0, source.1, rhs).unwrap();
            let op = ScoreboardOperation::parse(symbol).unwrap();
            assert_eq!(
                board.operation(target, op, source),
                Ok(expected),
                "{symbol}"
            );
            assert_eq!(board.get(target.0, target.1), Some(expected));
            let expected_source = if op == ScoreboardOperation::Swap {
                lhs
            } else {
                rhs
            };
            assert_eq!(board.get(source.0, source.1), Some(expected_source));
        }
        board.set(source.0, source.1, 0).unwrap();
        for op in [ScoreboardOperation::Divide, ScoreboardOperation::Modulo] {
            assert_eq!(
                board.operation(target, op, source),
                Err(ScoreboardError::DivisionByZero)
            );
            assert_eq!(board.get(target.0, target.1), Some(4));
        }
        let unscored = ("Sam", "b");
        assert_eq!(board.get(unscored.0, unscored.1), None);
        assert_eq!(
            board.operation(target, ScoreboardOperation::Add, unscored),
            Ok(4)
        );
        assert_eq!(board.get(unscored.0, unscored.1), Some(0));
    }

    #[test]
    fn scoreboard_objectives_display_and_scores_have_consistent_lifecycle() {
        let mut board = ScoreboardEngine::default();
        for (name, criteria) in [
            ("dummy", ScoreboardCriteria::Dummy),
            ("trigger", ScoreboardCriteria::Trigger),
            ("deathCount", ScoreboardCriteria::DeathCount),
            ("playerKillCount", ScoreboardCriteria::PlayerKillCount),
            ("totalKillCount", ScoreboardCriteria::TotalKillCount),
            ("health", ScoreboardCriteria::Health),
        ] {
            assert_eq!(ScoreboardCriteria::parse(name), Some(criteria));
            board.add_objective(objective(name, criteria)).unwrap();
        }
        assert_eq!(ScoreboardCriteria::parse("other"), None);
        assert_eq!(
            board.add_objective(objective("dummy", ScoreboardCriteria::Dummy)),
            Err(ScoreboardError::DuplicateObjective)
        );
        assert_eq!(
            board.objective("dummy").unwrap().display_name,
            "dummy display"
        );
        assert_eq!(
            board.objective("health").unwrap().render_type,
            ScoreboardRenderType::Hearts
        );
        for slot in [
            ScoreboardDisplaySlot::List,
            ScoreboardDisplaySlot::Sidebar,
            ScoreboardDisplaySlot::BelowName,
        ] {
            assert_eq!(
                board.set_display(slot, Some("missing")),
                Err(ScoreboardError::UnknownObjective)
            );
            board.set_display(slot, Some("dummy")).unwrap();
            assert_eq!(board.display(slot), Some("dummy"));
        }
        assert_eq!(board.add("Alex", "dummy", 7), Ok(7));
        assert_eq!(board.remove("Alex", "dummy", 2), Ok(5));
        assert_eq!(board.reset("Steve", Some("dummy")), Ok(0));
        assert_eq!(
            board.set("Alex", "health", 5),
            Err(ScoreboardError::ReadOnlyObjective)
        );
        board.update_health("Alex", 18);
        assert_eq!(board.get("Alex", "health"), Some(18));
        assert_eq!(board.reset("Alex", Some("health")), Ok(1));
        assert!(board.remove_objective("dummy"));
        assert!(!board.remove_objective("dummy"));
        assert_eq!(board.get("Alex", "dummy"), None);
        assert_eq!(board.display(ScoreboardDisplaySlot::List), None);
        assert_eq!(board.display(ScoreboardDisplaySlot::Sidebar), None);
        assert_eq!(board.display(ScoreboardDisplaySlot::BelowName), None);
    }

    #[test]
    fn scoreboard_triggers_relock_after_one_add_and_on_reset() {
        let mut board = ScoreboardEngine::default();
        board
            .add_objective(objective("quest", ScoreboardCriteria::Trigger))
            .unwrap();
        board
            .add_objective(objective("other", ScoreboardCriteria::Dummy))
            .unwrap();
        assert_eq!(
            board.enable_trigger("Alex", "other"),
            Err(ScoreboardError::NotTrigger)
        );
        assert_eq!(
            board.trigger_add("Alex", "quest", 4),
            Err(ScoreboardError::TriggerLocked)
        );
        board.enable_trigger("Alex", "quest").unwrap();
        assert_eq!(board.trigger_add("Alex", "quest", 4), Ok(4));
        assert_eq!(
            board.trigger_add("Alex", "quest", 4),
            Err(ScoreboardError::TriggerLocked)
        );
        assert_eq!(board.get("Alex", "quest"), Some(4));
        board.enable_trigger("Alex", "quest").unwrap();
        assert_eq!(board.trigger_add("Alex", "quest", 3), Ok(7));
        board.enable_trigger("Alex", "quest").unwrap();
        assert_eq!(board.reset("Alex", None), Ok(1));
        assert_eq!(board.get("Alex", "quest"), None);
        assert_eq!(
            board.trigger_add("Alex", "quest", 1),
            Err(ScoreboardError::TriggerLocked)
        );
    }

    #[test]
    fn fill_replace_keep_and_destroy_use_original_states() {
        let existing = [0, 1, 2];
        let fill = |mode, state| {
            eval_fill((2, 0, 0), (0, 0, 0), state, mode, 3, |(x, _, _)| {
                existing[x as usize]
            })
            .unwrap()
        };
        assert_eq!(fill(FillMode::Replace { filter: None }, 9).len(), 3);
        assert_eq!(
            fill(FillMode::Replace { filter: Some(2) }, 9),
            vec![BlockModification {
                pos: (2, 0, 0),
                state: 9,
                destroyed: None
            }]
        );
        assert_eq!(
            fill(FillMode::Keep, 9),
            vec![BlockModification {
                pos: (0, 0, 0),
                state: 9,
                destroyed: None
            }]
        );
        let destroyed = fill(FillMode::Destroy, 1);
        assert_eq!(
            destroyed
                .iter()
                .map(|change| change.destroyed)
                .collect::<Vec<_>>(),
            [None, Some(1), Some(2)]
        );
        assert!(destroyed.iter().all(|change| change.state == 1));
    }

    #[test]
    fn fill_outline_skips_interior_and_hollow_sets_it_to_air() {
        let fill = |mode| eval_fill((0, 0, 0), (2, 2, 2), 9, mode, 27, |_| 4).unwrap();
        let outline = fill(FillMode::Outline);
        assert_eq!(outline.len(), 26);
        assert!(outline
            .iter()
            .all(|change| change.pos != (1, 1, 1) && change.state == 9));
        let hollow = fill(FillMode::Hollow);
        assert_eq!(hollow.len(), 27);
        for change in hollow {
            assert_eq!(change.state, if change.pos == (1, 1, 1) { 0 } else { 9 });
        }
        let thin = eval_fill((0, 0, 0), (2, 2, 0), 9, FillMode::Hollow, 9, |_| 4).unwrap();
        assert_eq!(thin.len(), 9);
        assert!(thin.iter().all(|change| change.state == 9));
    }

    #[test]
    fn volume_limits_are_inclusive_configurable_and_checked_before_any_reads() {
        let mode = FillMode::Replace { filter: None };
        let limit = MODIFICATION_BLOCK_LIMIT;
        let from = (0, 0, 0);
        let to = (32_768, 0, 0);
        let volume_error = Err(BlockModificationError::VolumeLimit {
            volume: 32_769,
            limit,
        });
        assert_eq!(
            eval_fill(from, to, 1, mode, limit, |_| panic!(
                "read on rejected fill"
            )),
            volume_error
        );
        assert_eq!(
            eval_clone(
                from,
                to,
                (0, 0, 1),
                CloneFilter::Replace,
                CloneMode::Normal,
                limit,
                |_| panic!("read on rejected clone")
            ),
            volume_error
        );
        assert_eq!(
            eval_fill(from, (32_767, 0, 0), 1, mode, limit, |_| 0)
                .unwrap()
                .len(),
            limit
        );
        assert_eq!(
            eval_fill(from, to, 1, mode, limit + 1, |_| 1).unwrap(),
            vec![]
        );
        assert!(matches!(
            eval_fill((i32::MIN, i32::MIN, i32::MIN), (i32::MAX, i32::MAX, i32::MAX), 1, mode, usize::MAX, |_| panic!("read on overflow")),
            Err(BlockModificationError::VolumeLimit { volume, .. }) if volume == (1u128 << 96)
        ));
        assert_eq!(
            eval_clone(
                from,
                (1, 0, 0),
                (i32::MAX, 0, 0),
                CloneFilter::Replace,
                CloneMode::Force,
                2,
                |_| panic!("read on overflow")
            ),
            Err(BlockModificationError::DestinationOverflow)
        );
    }

    #[test]
    fn clone_normal_rejects_overlap_and_force_and_move_use_snapshot() {
        let original = [10, 20, 30, 40];
        let from = (2, 0, 0);
        let to = (0, 0, 0);
        let destination = (1, 0, 0);
        assert_eq!(
            eval_clone(
                from,
                to,
                destination,
                CloneFilter::Replace,
                CloneMode::Normal,
                3,
                |_| panic!("read on overlap")
            ),
            Err(BlockModificationError::OverlappingClone)
        );
        for (mode, expected) in [
            (CloneMode::Force, [10, 10, 20, 30]),
            (CloneMode::Move, [0, 10, 20, 30]),
        ] {
            let mut world = original;
            let changes = eval_clone(
                from,
                to,
                destination,
                CloneFilter::Replace,
                mode,
                3,
                |(x, _, _)| world[x as usize],
            )
            .unwrap();
            assert_eq!(changes.len(), if mode == CloneMode::Move { 4 } else { 3 });
            for change in changes {
                world[change.pos.0 as usize] = change.state;
                assert_eq!(change.destroyed, None);
            }
            assert_eq!(world, expected);
        }
    }

    #[test]
    fn clone_filters_leave_destination_holes_and_move_only_clears_selected_sources() {
        let original = [0, 20, 30, 40, 50, 60];
        for (filter, expected) in [
            (CloneFilter::Replace, [0, 20, 30, 0, 20, 30]),
            (CloneFilter::Masked, [0, 20, 30, 40, 20, 30]),
            (CloneFilter::Filtered(30), [0, 20, 30, 40, 50, 30]),
        ] {
            let mut world = original;
            let changes = eval_clone(
                (0, 0, 0),
                (2, 0, 0),
                (3, 0, 0),
                filter,
                CloneMode::Normal,
                3,
                |(x, _, _)| world[x as usize],
            )
            .unwrap();
            for change in changes {
                world[change.pos.0 as usize] = change.state;
            }
            assert_eq!(world, expected);
        }

        let mut world = original;
        let changes = eval_clone(
            (0, 0, 0),
            (2, 0, 0),
            (1, 0, 0),
            CloneFilter::Masked,
            CloneMode::Move,
            3,
            |(x, _, _)| world[x as usize],
        )
        .unwrap();
        for change in changes {
            world[change.pos.0 as usize] = change.state;
        }
        assert_eq!(world, [0, 0, 20, 30, 50, 60]);
    }
}
