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

    fn remaining_lerp_ticks(&self, now_tick: u64) -> u64 {
        self.lerp_duration_ticks
            .saturating_sub(now_tick.saturating_sub(self.lerp_start_tick))
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
fn parse_mechanism_number<T: std::str::FromStr>(token: &str, field: &str) -> Result<T, String> {
    if token
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'.'))
    {
        if let Ok(value) = token.parse() {
            return Ok(value);
        }
    }
    Err(format!("Invalid {field}: {token}"))
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_nonnegative_mechanism_int(token: &str, field: &str) -> Result<i32, String> {
    let value = parse_mechanism_number(token, field)?;
    if value < 0 {
        return Err(format!("{field} must be nonnegative"));
    }
    Ok(value)
}

#[cfg_attr(not(test), allow(dead_code))]
fn execute_worldborder_command(
    state: &mut WorldBorderState,
    args: &[&str],
    now_tick: u64,
) -> Result<String, String> {
    match args {
        ["get"] => Ok(format!(
            "World border is {} blocks wide",
            state.current_size(now_tick).round()
        )),
        [command @ ("set" | "add"), distance, time @ ..] if time.len() <= 1 => {
            let distance = parse_mechanism_number::<f64>(distance, "distance")?;
            let current = state.current_size(now_tick);
            let (target, remaining) = if *command == "add" {
                (current + distance, state.remaining_lerp_ticks(now_tick))
            } else {
                (distance, 0)
            };
            if !(1.0..=WORLD_BORDER_MAX_SIZE).contains(&target) {
                return Err(format!(
                    "Border size must be between 1 and {WORLD_BORDER_MAX_SIZE}"
                ));
            }
            if target == current {
                return Err("World border size is already at the requested distance".into());
            }
            // In 1.21.1 an explicit /worldborder add time extends the current transition.
            let duration = if let [seconds] = time {
                let seconds = parse_nonnegative_mechanism_int(seconds, "time")? as u64;
                remaining.saturating_add(seconds * u64::from(TICKS_PER_SECOND))
            } else {
                0
            };
            state.lerp_to(target, now_tick, duration);
            Ok(format!("Set world border size to {target:.1} blocks"))
        }
        ["center", x, z] => {
            let center_coordinate = |token: &str| {
                let value = parse_mechanism_number::<f64>(token, "center coordinate")?;
                // Bare integer coordinates refer to the center of the block.
                let coordinate = if token.contains('.') {
                    value
                } else {
                    value + 0.5
                };
                // Vanilla's two-dimensional command position is stored as a float pair.
                let coordinate = f64::from(coordinate as f32);
                let limit = WORLD_BORDER_MAX_SIZE / 2.0;
                if !(-limit..=limit).contains(&coordinate) {
                    return Err(format!("Center coordinate must be within ±{limit}"));
                }
                Ok(coordinate)
            };
            let (x, z) = (center_coordinate(x)?, center_coordinate(z)?);
            if (x, z) == (state.center_x, state.center_z) {
                return Err("World border center is already at the requested position".into());
            }
            (state.center_x, state.center_z) = (x, z);
            Ok(format!("Set world border center to {x:.2}, {z:.2}"))
        }
        ["damage", kind @ ("amount" | "buffer"), value] => {
            let value = parse_mechanism_number::<f32>(value, kind)?;
            if !(0.0..=f32::MAX).contains(&value) {
                return Err(format!(
                    "World border damage {kind} must be a finite nonnegative value"
                ));
            }
            let value = f64::from(value);
            let setting = if *kind == "amount" {
                &mut state.damage_per_block
            } else {
                &mut state.damage_safe_zone
            };
            if *setting == value {
                return Err(format!("World border damage {kind} is already {value}"));
            }
            *setting = value;
            Ok(format!("Set world border damage {kind} to {value:.2}"))
        }
        ["warning", kind @ ("distance" | "time"), value] => {
            let value = parse_nonnegative_mechanism_int(value, kind)?;
            let setting = if *kind == "distance" {
                &mut state.warning_blocks
            } else {
                &mut state.warning_time_secs
            };
            if *setting == value {
                return Err(format!("World border warning {kind} is already {value}"));
            }
            *setting = value;
            Ok(format!("Set world border warning {kind} to {value}"))
        }
        _ => Err("Usage: /worldborder get|set|add|center|damage|warning".into()),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_worldborder_initialize(state: &WorldBorderState, now_tick: u64) -> (i64, Vec<u8>) {
    let mut payload = Vec::new();
    payload.extend_from_slice(&state.center_x.to_be_bytes());
    payload.extend_from_slice(&state.center_z.to_be_bytes());
    write_worldborder_lerp(
        &mut payload,
        state.current_size(now_tick),
        state.new_size,
        state.remaining_lerp_ticks(now_tick),
    );
    write_varint(&mut payload, i64::from(state.absolute_max_size));
    write_varint(&mut payload, i64::from(state.warning_blocks));
    write_varint(&mut payload, i64::from(state.warning_time_secs));
    (0x25, payload)
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_worldborder_center(x: f64, z: f64) -> (i64, Vec<u8>) {
    let mut payload = x.to_be_bytes().to_vec();
    payload.extend_from_slice(&z.to_be_bytes());
    (0x4d, payload)
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_worldborder_lerp_size(
    old_size: f64,
    new_size: f64,
    duration_ticks: u64,
) -> (i64, Vec<u8>) {
    let mut payload = Vec::new();
    write_worldborder_lerp(&mut payload, old_size, new_size, duration_ticks);
    (0x4e, payload)
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_worldborder_lerp(
    payload: &mut Vec<u8>,
    old_size: f64,
    new_size: f64,
    duration_ticks: u64,
) {
    payload.extend_from_slice(&old_size.to_be_bytes());
    payload.extend_from_slice(&new_size.to_be_bytes());
    // Nonnegative VarLong is the same encoding as the shared writer's unsigned path.
    let millis = duration_ticks.saturating_mul(TICK_MS).min(i64::MAX as u64);
    write_varint(payload, millis as i64);
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_worldborder_size(size: f64) -> (i64, Vec<u8>) {
    (0x4f, size.to_be_bytes().to_vec())
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

    fn revoke_matching(
        &mut self,
        player: &str,
        mut matches: impl FnMut(&Self, &str) -> bool,
    ) -> usize {
        let Some(progress) = self.progress.get(player) else {
            return 0;
        };
        let ids: Vec<_> = progress
            .keys()
            .copied()
            .filter(|id| matches(self, id))
            .collect();
        let progress = self.progress.get_mut(player).unwrap();
        let revoked = ids
            .into_iter()
            .filter_map(|id| progress.remove(id))
            .map(|criteria| criteria.len())
            .sum();
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
        granted += self.grant_parents(player, id, time_ms);
        granted
    }

    fn grant_until(&mut self, player: &str, id: &str, time_ms: i64) -> usize {
        let Some(definition) = self.definition(id) else {
            return 0;
        };
        let mut granted = self.grant_parents(player, id, time_ms);
        granted += self.grant_definition(player, definition, time_ms);
        granted
    }

    fn grant_parents(&mut self, player: &str, id: &str, time_ms: i64) -> usize {
        let mut granted = 0;
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

#[cfg_attr(not(test), allow(dead_code))]
fn resolve_command_advancement(
    tracker: &AdvancementTracker,
    id: &str,
) -> Result<&'static AdvancementDefinition, String> {
    let default_id = (!id.contains(':')).then(|| format!("minecraft:{id}"));
    tracker
        .definition(default_id.as_deref().unwrap_or(id))
        .ok_or_else(|| format!("Unknown advancement: {id}"))
}

#[cfg_attr(not(test), allow(dead_code))]
fn execute_advancement_command(
    tracker: &mut AdvancementTracker,
    args: &[&str],
    now_ms: i64,
) -> Result<String, String> {
    let [action @ ("grant" | "revoke"), target, scope @ ..] = args else {
        return Err("Usage: /advancement grant|revoke <target> <scope>".into());
    };
    let grant = *action == "grant";
    let changed = match scope {
        ["everything"] if grant => tracker.grant_everything(target, now_ms),
        ["everything"] => tracker.revoke_matching(target, |_, _| true),
        ["only", id, criterion] => {
            let definition = resolve_command_advancement(tracker, id)?;
            if !definition
                .requirements
                .iter()
                .any(|group| group.contains(criterion))
            {
                return Err(format!(
                    "Unknown criterion {criterion} for {}",
                    definition.id
                ));
            }
            let changed = if grant {
                tracker.grant_criterion(target, definition.id, criterion, now_ms)
            } else {
                tracker.revoke_criterion(target, definition.id, criterion)
            };
            usize::from(changed)
        }
        [mode @ ("only" | "from" | "through" | "until"), id] => {
            let id = resolve_command_advancement(tracker, id)?.id;
            if grant {
                match *mode {
                    "from" => tracker.grant_from(target, id, now_ms),
                    "through" => tracker.grant_through(target, id, now_ms),
                    "until" => tracker.grant_until(target, id, now_ms),
                    _ => tracker.grant_only(target, id, now_ms),
                }
            } else {
                tracker.revoke_matching(target, |tracker, candidate| match *mode {
                    "from" => tracker.has_ancestor(candidate, id),
                    "through" => {
                        tracker.has_ancestor(candidate, id) || tracker.has_ancestor(id, candidate)
                    }
                    "until" => tracker.has_ancestor(id, candidate),
                    _ => candidate == id,
                })
            }
        }
        _ => {
            return Err(
                "Usage: everything | only <id> [criterion] | from|through|until <id>".into(),
            )
        }
    };
    if changed == 0 {
        return Err(format!("No advancement criteria changed for {target}"));
    }
    let verb = if grant { "Granted" } else { "Revoked" };
    Ok(format!(
        "{verb} {changed} advancement criteria for {target}"
    ))
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
    // Declaration order is the protocol 767 display slot ID.
    List,
    Sidebar,
    BelowName,
    TeamBlack,
    TeamDarkBlue,
    TeamDarkGreen,
    TeamDarkAqua,
    TeamDarkRed,
    TeamDarkPurple,
    TeamGold,
    TeamGray,
    TeamDarkGray,
    TeamBlue,
    TeamGreen,
    TeamAqua,
    TeamRed,
    TeamLightPurple,
    TeamYellow,
    TeamWhite,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ScoreboardDisplaySlot {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "list" => Self::List,
            "sidebar" => Self::Sidebar,
            "below_name" => Self::BelowName,
            "sidebar.team.black" => Self::TeamBlack,
            "sidebar.team.dark_blue" => Self::TeamDarkBlue,
            "sidebar.team.dark_green" => Self::TeamDarkGreen,
            "sidebar.team.dark_aqua" => Self::TeamDarkAqua,
            "sidebar.team.dark_red" => Self::TeamDarkRed,
            "sidebar.team.dark_purple" => Self::TeamDarkPurple,
            "sidebar.team.gold" => Self::TeamGold,
            "sidebar.team.gray" => Self::TeamGray,
            "sidebar.team.dark_gray" => Self::TeamDarkGray,
            "sidebar.team.blue" => Self::TeamBlue,
            "sidebar.team.green" => Self::TeamGreen,
            "sidebar.team.aqua" => Self::TeamAqua,
            "sidebar.team.red" => Self::TeamRed,
            "sidebar.team.light_purple" => Self::TeamLightPurple,
            "sidebar.team.yellow" => Self::TeamYellow,
            "sidebar.team.white" => Self::TeamWhite,
            _ => return None,
        })
    }
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
    display_slots: [Option<String>; ScoreboardDisplaySlot::TeamWhite as usize + 1],
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
fn scoreboard_command_error(error: ScoreboardError) -> String {
    match error {
        ScoreboardError::DuplicateObjective => "Objective already exists",
        ScoreboardError::UnknownObjective => "Unknown objective",
        ScoreboardError::ReadOnlyObjective => "Objective is read-only",
        ScoreboardError::NotTrigger => "Objective is not a trigger",
        ScoreboardError::TriggerLocked => "Trigger is locked",
        ScoreboardError::DivisionByZero => "Cannot divide by zero",
    }
    .into()
}

#[cfg_attr(not(test), allow(dead_code))]
fn execute_scoreboard_command(
    engine: &mut ScoreboardEngine,
    args: &[&str],
) -> Result<String, String> {
    match args {
        ["objectives", "add", name, criteria, display @ ..] => {
            let criteria = ScoreboardCriteria::parse(criteria)
                .ok_or_else(|| format!("Unknown scoreboard criterion: {criteria}"))?;
            let display_name = if display.is_empty() {
                (*name).to_owned()
            } else {
                display.join(" ")
            };
            let render_type = if criteria == ScoreboardCriteria::Health {
                ScoreboardRenderType::Hearts
            } else {
                ScoreboardRenderType::Integer
            };
            engine
                .add_objective(ScoreboardObjective {
                    name: (*name).into(),
                    criteria,
                    display_name,
                    render_type,
                })
                .map_err(scoreboard_command_error)?;
            Ok(format!("Added objective {name}"))
        }
        ["objectives", "remove", name] => {
            if !engine.remove_objective(name) {
                return Err(format!("Unknown objective: {name}"));
            }
            Ok(format!("Removed objective {name}"))
        }
        ["objectives", "list"] => {
            let mut names: Vec<_> = engine.objectives.keys().map(String::as_str).collect();
            names.sort_unstable();
            Ok(format!("{} objectives: {}", names.len(), names.join(", ")))
        }
        ["objectives", "setdisplay", slot, objective @ ..] if objective.len() <= 1 => {
            let display_slot = ScoreboardDisplaySlot::parse(slot)
                .ok_or_else(|| format!("Unknown scoreboard display slot: {slot}"))?;
            let objective = objective.first().copied();
            engine
                .set_display(display_slot, objective)
                .map_err(scoreboard_command_error)?;
            Ok(match objective {
                Some(name) => format!("Displaying {name} in {slot}"),
                None => format!("Cleared display slot {slot}"),
            })
        }
        ["players", command @ ("set" | "add" | "remove"), player, objective, value] => {
            let score = match *command {
                "set" => {
                    let value = parse_mechanism_number(value, "score")?;
                    engine
                        .set(player, objective, value)
                        .map_err(scoreboard_command_error)?;
                    value
                }
                "add" => engine
                    .add(
                        player,
                        objective,
                        parse_nonnegative_mechanism_int(value, "amount")?,
                    )
                    .map_err(scoreboard_command_error)?,
                _ => engine
                    .remove(
                        player,
                        objective,
                        parse_nonnegative_mechanism_int(value, "amount")?,
                    )
                    .map_err(scoreboard_command_error)?,
            };
            Ok(format!("{player} has {score} in {objective}"))
        }
        ["players", "reset", player, objective @ ..] if objective.len() <= 1 => {
            let count = engine
                .reset(player, objective.first().copied())
                .map_err(scoreboard_command_error)?;
            Ok(format!("Reset {count} scores for {player}"))
        }
        ["players", "get", player, objective] => {
            engine
                .criteria(objective)
                .map_err(scoreboard_command_error)?;
            let score = engine
                .get(player, objective)
                .ok_or_else(|| format!("{player} has no score in {objective}"))?;
            Ok(format!("{player} has {score} in {objective}"))
        }
        ["players", "enable", player, objective] => {
            engine
                .enable_trigger(player, objective)
                .map_err(scoreboard_command_error)?;
            Ok(format!("Enabled {objective} for {player}"))
        }
        ["players", "operation", target, target_obj, symbol, source, source_obj] => {
            let operation = ScoreboardOperation::parse(symbol)
                .ok_or_else(|| format!("Unknown scoreboard operation: {symbol}"))?;
            let score = engine
                .operation((target, target_obj), operation, (source, source_obj))
                .map_err(scoreboard_command_error)?;
            Ok(format!("{target} has {score} in {target_obj}"))
        }
        _ => Err("Usage: /scoreboard objectives|players <subcommand>".into()),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
enum ScoreboardObjectivePacket<'a> {
    Create(&'a ScoreboardObjective),
    Remove(&'a str),
    Update(&'a ScoreboardObjective),
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_objective(packet: ScoreboardObjectivePacket<'_>) -> (i64, Vec<u8>) {
    let mut payload = Vec::new();
    match packet {
        ScoreboardObjectivePacket::Remove(name) => {
            write_str(&mut payload, name);
            payload.push(1);
        }
        ScoreboardObjectivePacket::Create(objective)
        | ScoreboardObjectivePacket::Update(objective) => {
            write_str(&mut payload, &objective.name);
            payload.push(if matches!(packet, ScoreboardObjectivePacket::Create(_)) {
                0
            } else {
                2
            });
            payload.extend(nbt_string(&objective.display_name));
            write_varint(&mut payload, objective.render_type as i64);
            payload.push(0); // Optional number format is absent.
        }
    }
    (0x5e, payload)
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_display(
    slot: ScoreboardDisplaySlot,
    objective: Option<&str>,
) -> (i64, Vec<u8>) {
    (
        0x57,
        display_pkt(slot as i64, objective.unwrap_or_default()),
    )
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_score(
    holder: &str,
    objective: &str,
    value: i32,
    display_name: Option<&str>,
) -> (i64, Vec<u8>) {
    let mut payload = Vec::new();
    write_str(&mut payload, holder);
    write_str(&mut payload, objective);
    write_varint(&mut payload, i64::from(value));
    payload.push(u8::from(display_name.is_some()));
    if let Some(name) = display_name {
        payload.extend(nbt_string(name));
    }
    payload.push(0); // Optional number format is absent.
    (0x61, payload)
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_reset(holder: &str, objective: Option<&str>) -> (i64, Vec<u8>) {
    let mut payload = Vec::new();
    write_str(&mut payload, holder);
    payload.push(u8::from(objective.is_some()));
    if let Some(objective) = objective {
        write_str(&mut payload, objective);
    }
    (0x44, payload)
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
struct ParsedFillCommand {
    from: BlockPos,
    to: BlockPos,
    state: u32,
    mode: FillMode,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedCloneCommand {
    from: BlockPos,
    to: BlockPos,
    destination: BlockPos,
    filter: CloneFilter,
    mode: CloneMode,
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_mechanism_block_pos([x, y, z]: [&str; 3], origin: BlockPos) -> Result<BlockPos, String> {
    let coordinate = |token: &str, base: i32| {
        let Some(offset) = token.strip_prefix('~') else {
            return parse_mechanism_number(token, "block coordinate");
        };
        if offset.is_empty() {
            return Ok(base);
        }
        // A relative block coordinate is floored after adding the executor's origin.
        let resolved = (f64::from(base) + parse_mechanism_number::<f64>(offset, "offset")?).floor();
        if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&resolved) {
            return Err(format!("Block coordinate out of range: {token}"));
        }
        Ok(resolved as i32)
    };
    Ok((
        coordinate(x, origin.0)?,
        coordinate(y, origin.1)?,
        coordinate(z, origin.2)?,
    ))
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_fill_command(
    args: &[&str],
    origin: (i32, i32, i32),
    resolve_block: impl Fn(&str) -> Option<u32>,
) -> Result<ParsedFillCommand, String> {
    let [x1, y1, z1, x2, y2, z2, block, mode @ ..] = args else {
        return Err("Usage: /fill <from> <to> <block> [mode]".into());
    };
    let from = parse_mechanism_block_pos([x1, y1, z1], origin)?;
    let to = parse_mechanism_block_pos([x2, y2, z2], origin)?;
    let state = resolve_block(block).ok_or_else(|| format!("Unknown block: {block}"))?;
    let mode = match mode {
        [] | ["replace"] => FillMode::Replace { filter: None },
        ["replace", block] => FillMode::Replace {
            filter: Some(resolve_block(block).ok_or_else(|| format!("Unknown block: {block}"))?),
        },
        ["outline"] => FillMode::Outline,
        ["hollow"] => FillMode::Hollow,
        ["keep"] => FillMode::Keep,
        ["destroy"] => FillMode::Destroy,
        _ => return Err("Expected replace [filter], outline, hollow, keep or destroy".into()),
    };
    Ok(ParsedFillCommand {
        from,
        to,
        state,
        mode,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_clone_command(
    args: &[&str],
    origin: (i32, i32, i32),
    resolve_block: impl Fn(&str) -> Option<u32>,
) -> Result<ParsedCloneCommand, String> {
    let [x1, y1, z1, x2, y2, z2, dx, dy, dz, options @ ..] = args else {
        return Err("Usage: /clone <from> <to> <destination> [filter [mode]]".into());
    };
    let from = parse_mechanism_block_pos([x1, y1, z1], origin)?;
    let to = parse_mechanism_block_pos([x2, y2, z2], origin)?;
    let destination = parse_mechanism_block_pos([dx, dy, dz], origin)?;
    let (filter, mode) = match options {
        [] => (CloneFilter::Replace, &[][..]),
        ["replace", mode @ ..] => (CloneFilter::Replace, mode),
        ["masked", mode @ ..] => (CloneFilter::Masked, mode),
        ["filtered", block, mode @ ..] => {
            let state = resolve_block(block).ok_or_else(|| format!("Unknown block: {block}"))?;
            (CloneFilter::Filtered(state), mode)
        }
        _ => return Err("Expected replace, masked or filtered <block>".into()),
    };
    let mode = match mode {
        [] | ["normal"] => CloneMode::Normal,
        ["force"] => CloneMode::Force,
        ["move"] => CloneMode::Move,
        _ => return Err("Expected clone mode normal, force or move".into()),
    };
    Ok(ParsedCloneCommand {
        from,
        to,
        destination,
        filter,
        mode,
    })
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
            FillMode::Keep
                if !breg().map_or(previous == COMMAND_AIR_STATE, |reg| reg.is_air(previous)) =>
            {
                continue;
            }
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

    fn command_block(name: &str) -> Option<u32> {
        match name {
            "minecraft:stone" => Some(1),
            "glass" => Some(2),
            _ => None,
        }
    }

    #[test]
    fn fill_command_parses_relative_coordinates_and_every_mode() {
        let origin = (10, -20, 30);
        let coords = ["~", "~-2.5", "42", "~-3", "~4", "~-0.1", "glass"];
        for (suffix, mode) in [
            (&[][..], FillMode::Replace { filter: None }),
            (&["replace"][..], FillMode::Replace { filter: None }),
            (
                &["replace", "minecraft:stone"][..],
                FillMode::Replace { filter: Some(1) },
            ),
            (&["outline"][..], FillMode::Outline),
            (&["hollow"][..], FillMode::Hollow),
            (&["keep"][..], FillMode::Keep),
            (&["destroy"][..], FillMode::Destroy),
        ] {
            let args = [coords.as_slice(), suffix].concat();
            assert_eq!(
                parse_fill_command(&args, origin, command_block).unwrap(),
                ParsedFillCommand {
                    from: (10, -23, 42),
                    to: (7, -16, 29),
                    state: 2,
                    mode,
                }
            );
        }
        for suffix in [
            &["outline", "glass"][..],
            &["replace", "missing"][..],
            &["destroy", "extra"][..],
        ] {
            assert!(parse_fill_command(
                &[coords.as_slice(), suffix].concat(),
                origin,
                command_block
            )
            .is_err());
        }
        for token in ["2147483648", "~2147483647", "~NaN", "^1", "1.5"] {
            let mut args = coords;
            args[0] = token;
            assert!(
                parse_fill_command(&args, origin, command_block).is_err(),
                "{token}"
            );
        }
        assert!(parse_fill_command(&coords[..6], origin, command_block).is_err());
        let mut args = coords;
        args[6] = "missing";
        assert!(parse_fill_command(&args, origin, command_block).is_err());
    }

    #[test]
    fn clone_command_parses_relative_coordinates_filters_and_modes() {
        let origin = (-10, 20, -30);
        let coords = ["~", "~-2", "0", "~-3", "~4", "~-1", "~0.5", "64", "~9"];
        for (suffix, filter, mode) in [
            (&[][..], CloneFilter::Replace, CloneMode::Normal),
            (
                &["replace", "move"][..],
                CloneFilter::Replace,
                CloneMode::Move,
            ),
            (&["masked"][..], CloneFilter::Masked, CloneMode::Normal),
            (
                &["masked", "force"][..],
                CloneFilter::Masked,
                CloneMode::Force,
            ),
            (
                &["filtered", "minecraft:stone", "normal"][..],
                CloneFilter::Filtered(1),
                CloneMode::Normal,
            ),
        ] {
            let args = [coords.as_slice(), suffix].concat();
            assert_eq!(
                parse_clone_command(&args, origin, command_block).unwrap(),
                ParsedCloneCommand {
                    from: (-10, 18, 0),
                    to: (-13, 24, -31),
                    destination: (-10, 64, -21),
                    filter,
                    mode,
                }
            );
        }
        for suffix in [
            &["filtered"][..],
            &["filtered", "missing"][..],
            &["force"][..],
            &["replace", "normal", "extra"][..],
            &["masked", "unknown"][..],
        ] {
            assert!(parse_clone_command(
                &[coords.as_slice(), suffix].concat(),
                origin,
                command_block
            )
            .is_err());
        }
        assert!(parse_clone_command(&coords[..8], origin, command_block).is_err());
        let mut args = coords;
        args[8] = "~-2147483648";
        assert!(parse_clone_command(&args, origin, command_block).is_err());
    }

    #[test]
    fn worldborder_command_updates_size_center_damage_and_warnings() {
        let mut border = WorldBorderState::default();
        execute_worldborder_command(&mut border, &["set", "100"], 0).unwrap();
        assert_eq!(border.current_size(0), 100.0);
        execute_worldborder_command(&mut border, &["set", "60", "2"], 10).unwrap();
        assert_eq!(
            execute_worldborder_command(&mut border, &["get"], 30).unwrap(),
            "World border is 80 blocks wide"
        );
        execute_worldborder_command(&mut border, &["add", "-20", "3"], 30).unwrap();
        assert_eq!(border.lerp_duration_ticks, 80);
        assert_eq!(border.current_size(70), 70.0);
        execute_worldborder_command(&mut border, &["add", "4.5"], 70).unwrap();
        assert_eq!(border.lerp_duration_ticks, 0);
        assert_eq!(border.current_size(70), 74.5);
        assert_eq!(
            execute_worldborder_command(&mut border, &["get"], 70).unwrap(),
            "World border is 75 blocks wide"
        );
        for args in [
            &["center", "10", "-4.25"][..],
            &["damage", "amount", "1.5"][..],
            &["damage", "buffer", "0"][..],
            &["warning", "distance", "12"][..],
            &["warning", "time", "25"][..],
        ] {
            execute_worldborder_command(&mut border, args, 70).unwrap();
        }
        assert_eq!((border.center_x, border.center_z), (10.5, -4.25));
        assert_eq!(border.damage_per_block, 1.5);
        assert_eq!(border.damage_safe_zone, 0.0);
        assert_eq!(border.warning_blocks, 12);
        assert_eq!(border.warning_time_secs, 25);
    }

    #[test]
    fn worldborder_command_rejects_invalid_or_redundant_changes() {
        let mut border = WorldBorderState::default();
        let original = border;
        for args in [
            &["set", "0"][..],
            &["set", "60000000"][..],
            &["set", "50", "-1"][..],
            &["set", "50", "0.5"][..],
            &["add", "1"][..],
            &["add", "0"][..],
            &["center", "30000000", "0"][..],
            &["center", "0.0", "0.0"][..],
            &["damage", "amount", "NaN"][..],
            &["damage", "buffer", "-1"][..],
            &["warning", "distance", "-1"][..],
            &["warning", "time", "15"][..],
            &["get", "extra"][..],
        ] {
            assert!(
                execute_worldborder_command(&mut border, args, 0).is_err(),
                "{args:?}"
            );
            assert_eq!(border, original);
        }
    }

    #[test]
    fn scoreboard_command_updates_objectives_display_and_player_scores() {
        let mut board = ScoreboardEngine::default();
        for args in [
            &["objectives", "add", "points", "dummy", "Match", "points"][..],
            &["objectives", "add", "ready", "trigger"][..],
            &["objectives", "add", "life", "health"][..],
            &["objectives", "setdisplay", "sidebar", "points"][..],
            &["objectives", "setdisplay", "sidebar.team.red", "life"][..],
            &["players", "set", "Alex", "points", "-9"][..],
            &["players", "add", "Alex", "points", "6"][..],
            &["players", "remove", "Alex", "points", "4"][..],
            &["players", "set", "Pat", "points", "3"][..],
            &[
                "players",
                "operation",
                "Alex",
                "points",
                "/=",
                "Pat",
                "points",
            ][..],
            &["players", "enable", "Alex", "ready"][..],
        ] {
            execute_scoreboard_command(&mut board, args).unwrap();
        }
        assert_eq!(
            board.objective("points").unwrap().display_name,
            "Match points"
        );
        assert_eq!(board.objective("ready").unwrap().display_name, "ready");
        assert_eq!(
            board.objective("life").unwrap().render_type,
            ScoreboardRenderType::Hearts
        );
        assert_eq!(
            board.display(ScoreboardDisplaySlot::Sidebar),
            Some("points")
        );
        assert_eq!(board.display(ScoreboardDisplaySlot::TeamRed), Some("life"));
        assert_eq!(
            execute_scoreboard_command(&mut board, &["objectives", "list"]).unwrap(),
            "3 objectives: life, points, ready"
        );
        assert_eq!(
            execute_scoreboard_command(&mut board, &["players", "get", "Alex", "points"]).unwrap(),
            "Alex has -3 in points"
        );
        assert_eq!(board.trigger_add("Alex", "ready", 1), Ok(1));
        execute_scoreboard_command(&mut board, &["players", "reset", "Alex", "points"]).unwrap();
        assert_eq!(board.get("Alex", "points"), None);
        assert_eq!(board.get("Alex", "ready"), Some(1));
        execute_scoreboard_command(&mut board, &["players", "reset", "Alex"]).unwrap();
        assert_eq!(board.get("Alex", "ready"), None);
        assert_eq!(board.get("Pat", "points"), Some(3));
        execute_scoreboard_command(&mut board, &["objectives", "setdisplay", "sidebar"]).unwrap();
        assert_eq!(board.display(ScoreboardDisplaySlot::Sidebar), None);
        execute_scoreboard_command(&mut board, &["objectives", "remove", "life"]).unwrap();
        assert_eq!(board.display(ScoreboardDisplaySlot::TeamRed), None);
        assert!(board.objective("life").is_none());
    }

    #[test]
    fn scoreboard_command_reports_invalid_arguments_and_engine_errors() {
        let mut board = ScoreboardEngine::default();
        execute_scoreboard_command(&mut board, &["objectives", "add", "points", "dummy"]).unwrap();
        execute_scoreboard_command(&mut board, &["objectives", "add", "life", "health"]).unwrap();
        execute_scoreboard_command(&mut board, &["players", "set", "Alex", "points", "9"]).unwrap();
        for args in [
            &["objectives", "add", "points", "trigger"][..],
            &["objectives", "add", "extra", "unknown"][..],
            &["objectives", "setdisplay", "invalid", "points"][..],
            &["objectives", "setdisplay", "sidebar", "unknown"][..],
            &["objectives", "remove", "unknown"][..],
            &["players", "get", "Pat", "points"][..],
            &["players", "set", "Alex", "life", "1"][..],
            &["players", "set", "Alex", "points", "2147483648"][..],
            &["players", "add", "Alex", "points", "-1"][..],
            &["players", "remove", "Alex", "points", "-1"][..],
            &["players", "enable", "Alex", "points"][..],
            &[
                "players",
                "operation",
                "Alex",
                "points",
                "/=",
                "Pat",
                "points",
            ][..],
            &[
                "players",
                "operation",
                "Alex",
                "points",
                "?",
                "Pat",
                "points",
            ][..],
            &["players", "reset", "Alex", "points", "extra"][..],
        ] {
            assert!(
                execute_scoreboard_command(&mut board, args).is_err(),
                "{args:?}"
            );
            assert_eq!(board.get("Alex", "points"), Some(9));
            assert_eq!(board.get("Alex", "life"), None);
            assert_eq!(board.display(ScoreboardDisplaySlot::Sidebar), None);
        }
    }

    #[test]
    fn advancement_command_grants_and_revokes_each_scope() {
        let shield = "minecraft:story/deflect_arrow";
        let scopes = [
            ("only", &[ARMOR][..]),
            ("from", &[ARMOR, shield][..]),
            ("until", &[ROOT, STONE, TOOLS, IRON, ARMOR][..]),
            ("through", &[ROOT, STONE, TOOLS, IRON, ARMOR, shield][..]),
        ];
        for (scope, selected) in scopes {
            let mut tracker = AdvancementTracker::default();
            execute_advancement_command(
                &mut tracker,
                &["grant", "Alex", scope, "story/obtain_armor"],
                777,
            )
            .unwrap();
            let progress = &tracker.progress["Alex"];
            assert_eq!(progress.len(), selected.len(), "{scope}");
            for id in selected {
                assert!(progress[id].values().all(|&time| time == 777));
            }
            execute_advancement_command(&mut tracker, &["grant", "Alex", "everything"], 888)
                .unwrap();
            execute_advancement_command(&mut tracker, &["grant", "Pat", "everything"], 888)
                .unwrap();
            execute_advancement_command(&mut tracker, &["revoke", "Alex", scope, ARMOR], 999)
                .unwrap();
            for definition in CORE_ADVANCEMENTS {
                assert_eq!(
                    tracker.progress["Alex"].contains_key(definition.id),
                    !selected.contains(&definition.id),
                    "{scope} {}",
                    definition.id,
                );
            }
            execute_advancement_command(&mut tracker, &["revoke", "Alex", "everything"], 999)
                .unwrap();
            assert!(!tracker.progress.contains_key("Alex"));
            assert_eq!(tracker.progress["Pat"].len(), CORE_ADVANCEMENTS.len());
        }
    }

    #[test]
    fn advancement_command_changes_a_single_criterion_and_validates_arguments() {
        let mut tracker = AdvancementTracker::default();
        for (criterion, time) in [("iron_helmet", 10), ("iron_boots", 20)] {
            execute_advancement_command(
                &mut tracker,
                &["grant", "Alex", "only", ARMOR, criterion],
                time,
            )
            .unwrap();
        }
        execute_advancement_command(
            &mut tracker,
            &["revoke", "Alex", "only", ARMOR, "iron_helmet"],
            30,
        )
        .unwrap();
        for args in [
            &["grant", "Alex", "only", "unknown"][..],
            &["grant", "Alex", "only", ARMOR, "unknown"][..],
            &["grant", "Alex", "only", ARMOR, "iron_boots"][..],
            &["grant", "Alex", "from", ARMOR, "iron_helmet"][..],
            &["revoke", "Alex", "only", ARMOR, "iron_helmet"][..],
            &["revoke", "Alex", "everything", ARMOR][..],
        ] {
            assert!(
                execute_advancement_command(&mut tracker, args, 40).is_err(),
                "{args:?}"
            );
            let criteria = &tracker.progress["Alex"][ARMOR];
            assert_eq!(criteria.len(), 1);
            assert_eq!(criteria.get("iron_boots"), Some(&20));
        }
    }

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
    fn advancements_grant_until_awards_only_the_selected_ancestor_chain() {
        let mut tracker = AdvancementTracker::default();
        let selected = "minecraft:story/lava_bucket";
        assert_eq!(tracker.grant_until("Alex", "minecraft:unknown", 1), 0);
        assert!(!tracker.progress.contains_key("Alex"));
        assert_eq!(tracker.grant_until("Alex", selected, 2), 5);
        for id in [ROOT, STONE, TOOLS, IRON, selected] {
            assert!(tracker.completed("Alex", id), "{id}");
        }
        assert!(!tracker.completed("Alex", ARMOR));
        assert!(!tracker.completed("Alex", "minecraft:story/form_obsidian"));
        assert_eq!(tracker.grant_until("Alex", selected, 3), 0);
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

    fn take_packet_bytes<'a>(packet: &mut &'a [u8], len: usize) -> &'a [u8] {
        let (bytes, remaining) = packet.split_at(len);
        *packet = remaining;
        bytes
    }

    fn read_packet_string<'a>(packet: &mut &'a [u8]) -> &'a str {
        let len = read_protocol_i32(packet).unwrap().try_into().unwrap();
        std::str::from_utf8(take_packet_bytes(packet, len)).unwrap()
    }

    fn read_packet_bool(packet: &mut &[u8]) -> bool {
        match take_packet_bytes(packet, 1)[0] {
            0 => false,
            1 => true,
            byte => panic!("Invalid wire boolean: {byte}"),
        }
    }

    fn read_packet_double(packet: &mut &[u8]) -> f64 {
        f64::from_be_bytes(take_packet_bytes(packet, 8).try_into().unwrap())
    }

    fn read_packet_lerp(packet: &mut &[u8]) -> (f64, f64, i64) {
        let old_size = read_packet_double(packet);
        let new_size = read_packet_double(packet);
        let duration_ms = read_varint(packet).unwrap();
        (old_size, new_size, duration_ms)
    }

    fn read_packet_literal_component(packet: &mut &[u8]) -> String {
        assert_eq!(take_packet_bytes(packet, 1), &[8]); // Anonymous TAG_String.
        let length = take_packet_bytes(packet, 2);
        let size = u16::from_be_bytes(length.try_into().unwrap()) as usize;
        // Give the anonymous tag an empty root name for the named NBT decoder.
        let mut named = vec![8, 0, 0];
        named.extend_from_slice(length);
        named.extend_from_slice(take_packet_bytes(packet, size));
        let (root, tag) = crate::nbt::read_named(&named).unwrap();
        assert_eq!(root.to_utf8().unwrap(), "");
        tag.string().unwrap().to_utf8().unwrap()
    }

    #[test]
    fn border_packets_round_trip_protocol_767_payloads() {
        let mut border = WorldBorderState {
            center_x: -15.5,
            center_z: 33.25,
            warning_blocks: 130,
            warning_time_secs: 10,
            ..WorldBorderState::default()
        };
        border.lerp_to(100.0, 0, 0);
        border.lerp_to(180.0, 10, 20);
        for (tick, expected) in [(20, (140.0, 180.0, 500)), (30, (180.0, 180.0, 0))] {
            let (id, payload) = encode_worldborder_initialize(&border, tick);
            assert_eq!(id, 0x25);
            let mut packet = payload.as_slice();
            assert_eq!(read_packet_double(&mut packet), -15.5);
            assert_eq!(read_packet_double(&mut packet), 33.25);
            assert_eq!(read_packet_lerp(&mut packet), expected);
            assert_eq!(read_protocol_i32(&mut packet), Some(29_999_984));
            assert_eq!(read_protocol_i32(&mut packet), Some(130));
            assert_eq!(read_protocol_i32(&mut packet), Some(10));
            assert!(packet.is_empty());
        }

        let (id, payload) = encode_worldborder_center(-15.5, 33.25);
        assert_eq!(id, 0x4d);
        let mut packet = payload.as_slice();
        assert_eq!(read_packet_double(&mut packet), -15.5);
        assert_eq!(read_packet_double(&mut packet), 33.25);
        assert!(packet.is_empty());

        let (id, payload) = encode_worldborder_size(1.5);
        assert_eq!(id, 0x4f);
        let mut packet = payload.as_slice();
        assert_eq!(read_packet_double(&mut packet), 1.5);
        assert!(packet.is_empty());

        let (id, payload) = encode_worldborder_lerp_size(100.0, 29.5, 1_000_000_000);
        assert_eq!(id, 0x4e);
        let mut packet = payload.as_slice();
        assert_eq!(read_packet_lerp(&mut packet), (100.0, 29.5, 50_000_000_000));
        assert_eq!(payload.len(), 16 + 6); // VarLong needs more than five bytes.
        assert!(packet.is_empty());
    }

    #[test]
    fn scoreboard_objective_and_display_packets_round_trip_protocol_767_payloads() {
        let mut objective = objective("life", ScoreboardCriteria::Health);
        objective.display_name = "Vie ❤️".to_owned();
        for (mode, packet) in [
            (0, ScoreboardObjectivePacket::Create(&objective)),
            (1, ScoreboardObjectivePacket::Remove(&objective.name)),
            (2, ScoreboardObjectivePacket::Update(&objective)),
        ] {
            let (id, payload) = encode_scoreboard_objective(packet);
            assert_eq!(id, 0x5e);
            let mut packet = payload.as_slice();
            assert_eq!(read_packet_string(&mut packet), "life");
            assert_eq!(take_packet_bytes(&mut packet, 1), &[mode]);
            if mode != 1 {
                assert_eq!(read_packet_literal_component(&mut packet), "Vie ❤️");
                assert_eq!(read_protocol_i32(&mut packet), Some(1)); // Hearts.
                assert!(!read_packet_bool(&mut packet)); // No number format.
            }
            assert!(packet.is_empty());
        }

        for (slot, index, name) in [
            (ScoreboardDisplaySlot::List, 0, Some("life")),
            (ScoreboardDisplaySlot::Sidebar, 1, None),
            (ScoreboardDisplaySlot::BelowName, 2, Some("life")),
            (ScoreboardDisplaySlot::TeamRed, 15, Some("life")),
            (ScoreboardDisplaySlot::TeamWhite, 18, None),
        ] {
            let (id, payload) = encode_scoreboard_display(slot, name);
            assert_eq!(id, 0x57);
            let mut packet = payload.as_slice();
            assert_eq!(read_protocol_i32(&mut packet), Some(index));
            assert_eq!(read_packet_string(&mut packet), name.unwrap_or_default());
            assert!(packet.is_empty());
        }
    }

    #[test]
    fn scoreboard_score_and_reset_packets_round_trip_protocol_767_payloads() {
        for (value, display_name) in [(-1, None), (i32::MIN, Some("🔔")), (130, Some(""))] {
            let (id, payload) = encode_scoreboard_score("Alex", "life", value, display_name);
            assert_eq!(id, 0x61);
            let mut packet = payload.as_slice();
            assert_eq!(read_packet_string(&mut packet), "Alex");
            assert_eq!(read_packet_string(&mut packet), "life");
            if value == -1 {
                assert_eq!(&packet[..5], &[0xff, 0xff, 0xff, 0xff, 0x0f]);
            }
            assert_eq!(read_protocol_i32(&mut packet), Some(value));
            assert_eq!(read_packet_bool(&mut packet), display_name.is_some());
            if let Some(display_name) = display_name {
                assert_eq!(read_packet_literal_component(&mut packet), display_name);
            }
            assert!(!read_packet_bool(&mut packet)); // No number format.
            assert!(packet.is_empty());
        }

        for name in [None, Some("life")] {
            let (id, payload) = encode_scoreboard_reset("Alex", name);
            assert_eq!(id, 0x44);
            let mut packet = payload.as_slice();
            assert_eq!(read_packet_string(&mut packet), "Alex");
            assert_eq!(read_packet_bool(&mut packet), name.is_some());
            if let Some(name) = name {
                assert_eq!(read_packet_string(&mut packet), name);
            }
            assert!(packet.is_empty());
        }
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
            assert_eq!(read_packet_string(&mut packet), definition.id);
            let (has_parent, remaining) = packet.split_at(1);
            packet = remaining;
            assert_eq!(has_parent, &[u8::from(definition.parent.is_some())]);
            if let Some(parent) = definition.parent {
                assert_eq!(read_packet_string(&mut packet), parent);
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
                    assert_eq!(read_packet_string(&mut packet), *criterion);
                }
            }
            assert_eq!(packet[0], 1);
            packet = &packet[1..];
        }
        assert_eq!(packet, &[0, 0]);

        execute_advancement_command(&mut tracker, &["grant", "Alex", "only", ROOT], 1_234_567)
            .unwrap();
        let (id, progress) = tracker.take_delta("Alex", false).unwrap().into_packet();
        assert_eq!(id, 0x74);
        assert_eq!(&progress[..4], &[0, 0, 0, 1]);
        let mut packet = &progress[4..];
        assert_eq!(read_packet_string(&mut packet), ROOT);
        assert_eq!(read_varint(&mut packet), Some(1));
        assert_eq!(read_packet_string(&mut packet), "crafting_table");
        assert_eq!(packet[0], 1);
        assert_eq!(&packet[1..], &1_234_567i64.to_be_bytes());

        execute_advancement_command(
            &mut tracker,
            &["revoke", "Alex", "only", ROOT, "crafting_table"],
            0,
        )
        .unwrap();
        let (_, revoke) = tracker.take_delta("Alex", false).unwrap().into_packet();
        assert_eq!(&revoke[..4], &[0, 0, 0, 1]);
        let mut packet = &revoke[4..];
        assert_eq!(read_packet_string(&mut packet), ROOT);
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
    fn fill_keep_accepts_all_registered_air_states() {
        let reg = breg().expect("block registry");
        let air = reg.state_of("minecraft:air", None);
        let cave_air = reg.state_of("minecraft:cave_air", None);
        let void_air = reg.state_of("minecraft:void_air", None);
        assert_ne!(cave_air, air);
        assert_ne!(void_air, air);
        assert_ne!(cave_air, void_air);
        let stone = reg.state_of("minecraft:stone", None);
        let dirt = reg.state_of("minecraft:dirt", None);
        let existing = [air, cave_air, void_air, stone];
        let changes = eval_fill((0, 0, 0), (3, 0, 0), dirt, FillMode::Keep, 4, |(x, _, _)| {
            existing[x as usize]
        })
        .unwrap();
        assert_eq!(
            changes,
            [0, 1, 2].map(|x| BlockModification {
                pos: (x, 0, 0),
                state: dirt,
                destroyed: None,
            })
        );
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
