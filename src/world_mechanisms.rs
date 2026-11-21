// R7 GP-7 evaluators. Included by main.rs alongside BlockPos and the wire helpers.

/// Resolve a fluid reaction to the block-registry state used by the world writer.
#[cfg_attr(not(test), allow(dead_code))]
fn fluid_solidification_state(reaction: flowing_fluid::Solidification) -> Option<u32> {
    let name = match reaction {
        flowing_fluid::Solidification::Obsidian => "minecraft:obsidian",
        flowing_fluid::Solidification::Cobblestone => "minecraft:cobblestone",
        flowing_fluid::Solidification::Stone => "minecraft:stone",
    };
    crate::block_registry::breg().map(|registry| registry.state_of(name, None))
}

/// Empty-collision hosts that water replaces instead of waterlogging. Rails and other
/// SimpleWaterloggedBlocks retain their host and therefore stay out of this set.
#[cfg_attr(not(test), allow(dead_code))]
fn fluid_washes_away(reg: &crate::block_registry::BReg, state: u32) -> bool {
    if reg.is_air(state) || reg.get_prop(state, "waterlogged").is_some() {
        return false;
    }
    if matches!(
        reg.name_of(state),
        "minecraft:ladder"
            | "minecraft:sugar_cane"
            | "minecraft:bubble_column"
            | "minecraft:nether_portal"
            | "minecraft:end_portal"
            | "minecraft:end_gateway"
            | "minecraft:structure_void"
    ) || reg.in_tag(state, "minecraft:signs")
    {
        return false;
    }
    reg.is_collision_empty(state) && !reg.blocks_motion(state)
}

#[cfg_attr(not(test), allow(dead_code))]
const WORLD_BORDER_MAX_SIZE: f64 = 59_999_968.0;

#[cfg_attr(not(test), allow(dead_code))]
const WORLD_BORDER_MAX_CENTER: i32 = 29_999_984;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
struct WorldBorderState {
    center_x: f64,
    center_z: f64,
    old_size: f64,
    new_size: f64,
    lerp_start_ms: u64,
    lerp_duration_ms: i64,
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
            lerp_start_ms: 0,
            lerp_duration_ms: 0,
            damage_per_block: 0.2,
            damage_safe_zone: 5.0,
            warning_blocks: 5,
            warning_time_secs: 15,
            absolute_max_size: WORLD_BORDER_MAX_CENTER,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorldBorderState {
    // The caller supplies monotonic milliseconds so interpolation is independent of tick rate.
    fn lerp_to(&mut self, new_size: f64, now_ms: u64, duration_ms: i64) {
        if new_size.is_nan() {
            return;
        }
        self.old_size = self.current_size(now_ms);
        self.new_size = new_size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        self.lerp_start_ms = now_ms;
        self.lerp_duration_ms = if self.old_size == self.new_size {
            0
        } else {
            duration_ms.max(0)
        };
    }

    fn elapsed_ms(&self, now_ms: u64) -> i64 {
        now_ms
            .saturating_sub(self.lerp_start_ms)
            .min(i64::MAX as u64) as i64
    }

    fn lerp_remaining_ms(&self, now_ms: u64) -> i64 {
        if self.lerp_duration_ms == 0 {
            return 0;
        }
        self.lerp_duration_ms - self.elapsed_ms(now_ms)
    }

    // Vanilla keeps the moving extent until the next border tick, even after its deadline.
    fn finish_lerp(&mut self, now_ms: u64) {
        if self.lerp_remaining_ms(now_ms) <= 0 {
            self.lerp_duration_ms = 0;
        }
    }

    fn current_size(&self, now_ms: u64) -> f64 {
        let elapsed = self.elapsed_ms(now_ms);
        let target = self.new_size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        if elapsed >= self.lerp_duration_ms {
            return target;
        }
        let start = self.old_size.clamp(1.0, WORLD_BORDER_MAX_SIZE);
        let fraction = elapsed as f64 / self.lerp_duration_ms as f64;
        start + (target - start) * fraction
    }

    fn bounds(&self, now_ms: u64) -> (f64, f64, f64, f64) {
        let radius = self.current_size(now_ms) * 0.5;
        let limit = f64::from(self.absolute_max_size.max(0));
        (
            (self.center_x - radius).clamp(-limit, limit),
            (self.center_z - radius).clamp(-limit, limit),
            (self.center_x + radius).clamp(-limit, limit),
            (self.center_z + radius).clamp(-limit, limit),
        )
    }

    fn distance_to_border(&self, x: f64, z: f64, now_ms: u64) -> f64 {
        let (min_x, min_z, max_x, max_z) = self.bounds(now_ms);
        (x - min_x).min(max_x - x).min(z - min_z).min(max_z - z)
    }

    fn border_damage_for_pos(&self, x: f64, z: f64, now_ms: u64) -> f32 {
        let distance = self.distance_to_border(x, z, now_ms);
        if distance < -self.damage_safe_zone && self.damage_per_block > 0.0 {
            let damage = (-distance - self.damage_safe_zone) * self.damage_per_block;
            (damage.floor() as i32).max(1) as f32
        } else {
            0.0
        }
    }
}

// Protocol 767 clientbound IDs follow the vanilla 1.21.1 GameProtocols registration.

#[cfg_attr(not(test), allow(dead_code))]
type WorldMechanismsPacket = (i64, Vec<u8>);

#[cfg_attr(not(test), allow(dead_code))]
impl WorldBorderState {
    fn initialize_packet(&self, now_ms: u64) -> WorldMechanismsPacket {
        let mut out = Vec::new();
        write_doubles(&mut out, [self.center_x, self.center_z]);
        write_doubles(&mut out, [self.current_size(now_ms), self.new_size]);
        write_border_duration(&mut out, self.lerp_remaining_ms(now_ms));
        write_varint(&mut out, i64::from(self.absolute_max_size));
        write_varint(&mut out, i64::from(self.warning_blocks));
        write_varint(&mut out, i64::from(self.warning_time_secs));
        (0x25, out)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
enum WorldBorderUpdate {
    Center(f64, f64),
    LerpSize {
        from: f64,
        to: f64,
        duration_ms: i64,
    },
    Size(f64),
    WarningDelay(i32),
    WarningDistance(i32),
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorldBorderUpdate {
    fn into_packet(self) -> WorldMechanismsPacket {
        let mut out = Vec::new();
        let id = match self {
            Self::Center(x, z) => {
                write_doubles(&mut out, [x, z]);
                0x4d
            }
            Self::LerpSize {
                from,
                to,
                duration_ms,
            } => {
                write_doubles(&mut out, [from, to]);
                write_border_duration(&mut out, duration_ms);
                0x4e
            }
            Self::Size(size) => {
                out.extend_from_slice(&size.to_be_bytes());
                0x4f
            }
            Self::WarningDelay(seconds) => {
                write_varint(&mut out, i64::from(seconds));
                0x50
            }
            Self::WarningDistance(blocks) => {
                write_varint(&mut out, i64::from(blocks));
                0x51
            }
        };
        (id, out)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_border_duration(out: &mut Vec<u8>, ms: i64) {
    if ms < 0 {
        // write_varint uses five bytes for i32-range negatives. VarLong retains all 64 bits.
        out.push((ms as u8 & 0x7f) | 0x80);
        write_varint(out, ((ms as u64) >> 7) as i64);
    } else {
        write_varint(out, ms);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_doubles(out: &mut Vec<u8>, values: [f64; 2]) {
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct WorldBorderFeedback {
    key: &'static str,
    arguments: Vec<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorldBorderFeedback {
    fn new(key: &'static str, arguments: impl IntoIterator<Item = String>) -> Self {
        Self {
            key,
            arguments: arguments.into_iter().collect(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum WorldBorderCommandError {
    Syntax,
    MixedCoordinates,
    ExpectedNumber(&'static str),
    InvalidNumber(&'static str, String),
    NumberTooLow {
        kind: &'static str,
        minimum: String,
        found: String,
    },
    NumberTooHigh {
        kind: &'static str,
        maximum: String,
        found: String,
    },
    Rejected(WorldBorderFeedback),
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct WorldBorderCommandResult {
    result: i32,
    feedback: WorldBorderFeedback,
    broadcast_feedback: bool,
    packet: Option<WorldMechanismsPacket>,
}

#[derive(Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
enum WorldBorderCommand {
    Set(f64, i32),
    Add(f64, Option<i32>),
    Center(f64, f64),
    DamageAmount(f32),
    DamageBuffer(f32),
    WarningDistance(i32),
    WarningTime(i32),
    Get,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorldBorderCommand {
    /// Parse the arguments following the worldborder literal, using the command source's X/Z.
    fn parse(args: &[&str], source_x: f64, source_z: f64) -> Result<Self, WorldBorderCommandError> {
        let distance_range = -WORLD_BORDER_MAX_SIZE..=WORLD_BORDER_MAX_SIZE;
        let nonnegative_int = |arg| border_number(arg, "integer", Some(0..=i32::MAX));
        let nonnegative_float = |arg| border_number(arg, "float", Some(0.0..=f32::MAX));
        Ok(match args {
            ["set", size] => Self::Set(border_number(size, "double", Some(distance_range))?, 0),
            ["set", size, time] => Self::Set(
                border_number(size, "double", Some(distance_range))?,
                nonnegative_int(time)?,
            ),
            ["add", distance] => Self::Add(
                border_number(distance, "double", Some(distance_range))?,
                None,
            ),
            ["add", distance, time] => Self::Add(
                border_number(distance, "double", Some(distance_range))?,
                Some(nonnegative_int(time)?),
            ),
            ["center", x, z] => Self::Center(
                border_coordinate(x, source_x)?,
                border_coordinate(z, source_z)?,
            ),
            ["damage", "amount", amount] => Self::DamageAmount(nonnegative_float(amount)?),
            ["damage", "buffer", buffer] => Self::DamageBuffer(nonnegative_float(buffer)?),
            ["warning", "distance", distance] => Self::WarningDistance(nonnegative_int(distance)?),
            ["warning", "time", time] => Self::WarningTime(nonnegative_int(time)?),
            ["get"] => Self::Get,
            _ => return Err(WorldBorderCommandError::Syntax),
        })
    }

    /// Apply to the overworld border. Supply a source border for add in another dimension.
    fn execute(
        self,
        border: &mut WorldBorderState,
        source_border: Option<&WorldBorderState>,
        now_ms: u64,
    ) -> Result<WorldBorderCommandResult, WorldBorderCommandError> {
        let (result, feedback, update) = match self {
            Self::Set(size, seconds) => {
                return set_border_size(border, size, now_ms, i64::from(seconds) * 1_000)
            }
            Self::Add(distance, seconds) => {
                let source = source_border.unwrap_or(border);
                let size = source.current_size(now_ms) + distance;
                let duration = seconds.map_or(0, |seconds| {
                    source
                        .lerp_remaining_ms(now_ms)
                        .wrapping_add(i64::from(seconds) * 1_000)
                });
                return set_border_size(border, size, now_ms, duration);
            }
            Self::Center(x, z) => {
                if (x, z) == (border.center_x, border.center_z) {
                    return Err(border_rejected("commands.worldborder.center.failed", []));
                }
                let limit = f64::from(WORLD_BORDER_MAX_CENTER);
                if x.abs() > limit || z.abs() > limit {
                    return Err(border_rejected(
                        "commands.worldborder.set.failed.far",
                        [format!("{limit:E}")],
                    ));
                }
                border.center_x = x;
                border.center_z = z;
                (
                    0,
                    WorldBorderFeedback::new(
                        "commands.worldborder.center.success",
                        [border_decimal(x, 2), border_decimal(z, 2)],
                    ),
                    Some(WorldBorderUpdate::Center(x, z)),
                )
            }
            Self::DamageAmount(value) => {
                if border.damage_per_block == f64::from(value) {
                    return Err(border_rejected(
                        "commands.worldborder.damage.amount.failed",
                        [],
                    ));
                }
                border.damage_per_block = f64::from(value);
                (
                    value as i32,
                    WorldBorderFeedback::new(
                        "commands.worldborder.damage.amount.success",
                        [border_decimal(f64::from(value), 2)],
                    ),
                    None,
                )
            }
            Self::DamageBuffer(value) => {
                if border.damage_safe_zone == f64::from(value) {
                    return Err(border_rejected(
                        "commands.worldborder.damage.buffer.failed",
                        [],
                    ));
                }
                border.damage_safe_zone = f64::from(value);
                (
                    value as i32,
                    WorldBorderFeedback::new(
                        "commands.worldborder.damage.buffer.success",
                        [border_decimal(f64::from(value), 2)],
                    ),
                    None,
                )
            }
            Self::WarningDistance(value) => {
                if border.warning_blocks == value {
                    return Err(border_rejected(
                        "commands.worldborder.warning.distance.failed",
                        [],
                    ));
                }
                border.warning_blocks = value;
                (
                    value,
                    WorldBorderFeedback::new(
                        "commands.worldborder.warning.distance.success",
                        [value.to_string()],
                    ),
                    Some(WorldBorderUpdate::WarningDistance(value)),
                )
            }
            Self::WarningTime(value) => {
                if border.warning_time_secs == value {
                    return Err(border_rejected(
                        "commands.worldborder.warning.time.failed",
                        [],
                    ));
                }
                border.warning_time_secs = value;
                (
                    value,
                    WorldBorderFeedback::new(
                        "commands.worldborder.warning.time.success",
                        [value.to_string()],
                    ),
                    Some(WorldBorderUpdate::WarningDelay(value)),
                )
            }
            Self::Get => {
                let size = border.current_size(now_ms);
                return Ok(WorldBorderCommandResult {
                    result: (size + 0.5).floor() as i32,
                    feedback: WorldBorderFeedback::new(
                        "commands.worldborder.get",
                        [border_decimal(size, 0)],
                    ),
                    broadcast_feedback: false,
                    packet: None,
                });
            }
        };
        Ok(WorldBorderCommandResult {
            result,
            feedback,
            broadcast_feedback: true,
            packet: update.map(WorldBorderUpdate::into_packet),
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn border_rejected(
    key: &'static str,
    arguments: impl IntoIterator<Item = String>,
) -> WorldBorderCommandError {
    WorldBorderCommandError::Rejected(WorldBorderFeedback::new(key, arguments))
}

#[cfg_attr(not(test), allow(dead_code))]
fn set_border_size(
    border: &mut WorldBorderState,
    size: f64,
    now_ms: u64,
    duration_ms: i64,
) -> Result<WorldBorderCommandResult, WorldBorderCommandError> {
    let previous = border.current_size(now_ms);
    if size == previous {
        return Err(border_rejected(
            "commands.worldborder.set.failed.nochange",
            [],
        ));
    }
    if size < 1.0 {
        return Err(border_rejected("commands.worldborder.set.failed.small", []));
    }
    if size > WORLD_BORDER_MAX_SIZE {
        return Err(border_rejected(
            "commands.worldborder.set.failed.big",
            [format!("{WORLD_BORDER_MAX_SIZE:E}")],
        ));
    }

    border.lerp_to(size, now_ms, duration_ms);
    let mut arguments = vec![border_decimal(size, 1)];
    let (key, update) = if duration_ms > 0 {
        arguments.push((duration_ms / 1_000).to_string());
        let key = if size > previous {
            "commands.worldborder.set.grow"
        } else {
            "commands.worldborder.set.shrink"
        };
        (
            key,
            WorldBorderUpdate::LerpSize {
                from: previous,
                to: size,
                duration_ms,
            },
        )
    } else {
        (
            "commands.worldborder.set.immediate",
            WorldBorderUpdate::Size(size),
        )
    };
    Ok(WorldBorderCommandResult {
        result: (size - previous) as i32,
        feedback: WorldBorderFeedback::new(key, arguments),
        broadcast_feedback: true,
        packet: Some(update.into_packet()),
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn border_number<T: std::str::FromStr + PartialOrd + std::fmt::Display>(
    token: &str,
    kind: &'static str,
    range: Option<std::ops::RangeInclusive<T>>,
) -> Result<T, WorldBorderCommandError> {
    // Brigadier's numeric reader consumes only decimal digits, dots and minus signs.
    let numeric_len = token
        .bytes()
        .take_while(|&byte| matches!(byte, b'0'..=b'9' | b'.' | b'-'))
        .count();
    if numeric_len == 0 {
        return Err(WorldBorderCommandError::ExpectedNumber(kind));
    }
    let found = token[..numeric_len]
        .parse::<T>()
        .map_err(|_| WorldBorderCommandError::InvalidNumber(kind, token[..numeric_len].into()))?;
    if let Some(range) = range {
        if &found < range.start() {
            return Err(WorldBorderCommandError::NumberTooLow {
                kind,
                minimum: range.start().to_string(),
                found: found.to_string(),
            });
        }
        if &found > range.end() {
            return Err(WorldBorderCommandError::NumberTooHigh {
                kind,
                maximum: range.end().to_string(),
                found: found.to_string(),
            });
        }
    }
    if numeric_len != token.len() {
        return Err(WorldBorderCommandError::Syntax);
    }
    Ok(found)
}

#[cfg_attr(not(test), allow(dead_code))]
fn border_coordinate(token: &str, origin: f64) -> Result<f64, WorldBorderCommandError> {
    if token.starts_with('^') {
        return Err(WorldBorderCommandError::MixedCoordinates);
    }
    let coord = match token.strip_prefix('~') {
        Some("") => 0.0 + origin,
        Some(offset) => border_number::<f64>(offset, "double", None)? + origin,
        None => {
            let absolute = border_number::<f64>(token, "double", None)?;
            if token.contains('.') {
                absolute
            } else {
                absolute + 0.5
            }
        }
    };
    // The 2D command argument resolves world coordinates as a Vec2 of Java floats.
    Ok(f64::from(coord as f32))
}

#[cfg_attr(not(test), allow(dead_code))]
fn border_decimal(value: f64, places: usize) -> String {
    // Vanilla uses Java Formatter with a fixed precision and decimal half-up rounding.
    let decimal = value.abs().to_string();
    let (whole, fraction) = decimal.split_once('.').unwrap_or((&decimal, ""));
    let mut digits = whole.as_bytes().to_vec();
    digits.extend(fraction.bytes().chain(std::iter::repeat(b'0')).take(places));
    if fraction
        .as_bytes()
        .get(places)
        .is_some_and(|&digit| digit >= b'5')
    {
        if let Some(index) = digits.iter().rposition(|&digit| digit != b'9') {
            digits[index] += 1;
            digits[index + 1..].fill(b'0');
        } else {
            digits.fill(b'0');
            digits.insert(0, b'1');
        }
    }
    if places != 0 {
        digits.insert(digits.len() - places, b'.');
    }
    if value.is_sign_negative() {
        digits.insert(0, b'-');
    }
    String::from_utf8(digits).expect("ASCII decimal")
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
                    .push((definition, current.cloned().unwrap_or_default()));
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
    progress: Vec<(&'static AdvancementDefinition, AdvancementCriteriaProgress)>,
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
        for (definition, earned) in self.progress {
            write_str(&mut out, definition.id);
            // The progress codec writes every criterion, with an optional Instant for each.
            let criteria: std::collections::BTreeSet<_> = definition
                .requirements
                .iter()
                .copied()
                .flatten()
                .copied()
                .collect();
            write_varint(&mut out, criteria.len() as i64);
            for criterion in criteria {
                write_str(&mut out, criterion);
                let time_ms = earned.get(criterion);
                out.push(u8::from(time_ms.is_some()));
                if let Some(time_ms) = time_ms {
                    out.extend_from_slice(&time_ms.to_be_bytes());
                }
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

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
enum ScoreboardNumberFormat<'a> {
    Blank,
    Styled(&'a [u8]), // Anonymous NBT for a Style.
    Fixed(&'a [u8]),  // Anonymous NBT for a text component.
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_number_format(out: &mut Vec<u8>, format: Option<ScoreboardNumberFormat<'_>>) {
    out.push(u8::from(format.is_some()));
    // Number format type IDs are from the vanilla 1.21.1 registry report.
    match format {
        None => {}
        Some(ScoreboardNumberFormat::Blank) => write_varint(out, 0),
        Some(ScoreboardNumberFormat::Styled(style)) => {
            write_varint(out, 1);
            out.extend_from_slice(style);
        }
        Some(ScoreboardNumberFormat::Fixed(component)) => {
            write_varint(out, 2);
            out.extend_from_slice(component);
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
enum ScoreboardObjectivePacket<'a> {
    Create(&'a ScoreboardObjective, Option<ScoreboardNumberFormat<'a>>),
    Remove(&'a str),
    Update(&'a ScoreboardObjective, Option<ScoreboardNumberFormat<'a>>),
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_objective(packet: ScoreboardObjectivePacket<'_>) -> WorldMechanismsPacket {
    let mut out = Vec::new();
    let (objective, format, mode) = match packet {
        ScoreboardObjectivePacket::Create(objective, format) => (objective, format, 0),
        ScoreboardObjectivePacket::Update(objective, format) => (objective, format, 2),
        ScoreboardObjectivePacket::Remove(name) => {
            write_str(&mut out, name);
            out.push(1);
            return (0x5e, out);
        }
    };
    write_str(&mut out, &objective.name);
    out.push(mode);
    out.extend_from_slice(&nbt_string(&objective.display_name));
    write_varint(&mut out, objective.render_type as i64);
    write_number_format(&mut out, format);
    (0x5e, out)
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_display(
    slot: ScoreboardDisplaySlot,
    objective: Option<&str>,
) -> WorldMechanismsPacket {
    (
        0x57,
        display_pkt(slot as i64, objective.unwrap_or_default()),
    )
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_score(
    owner: &str,
    objective: &str,
    score: i32,
    display: Option<&[u8]>, // Anonymous NBT for a text component.
    format: Option<ScoreboardNumberFormat<'_>>,
) -> WorldMechanismsPacket {
    let mut out = Vec::new();
    write_str(&mut out, owner);
    write_str(&mut out, objective);
    write_varint(&mut out, i64::from(score));
    out.push(u8::from(display.is_some()));
    if let Some(component) = display {
        out.extend_from_slice(component);
    }
    write_number_format(&mut out, format);
    (0x61, out)
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_scoreboard_reset(owner: &str, objective: Option<&str>) -> WorldMechanismsPacket {
    let mut out = Vec::new();
    write_str(&mut out, owner);
    out.push(u8::from(objective.is_some()));
    if let Some(name) = objective {
        write_str(&mut out, name);
    }
    (0x44, out)
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

// Pure 1.21.1 random-tick evaluators. These are kept independent of the server's
// world storage and RNG so callers can supply snapshots and apply returned changes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum CropKind {
    Wheat,
    Carrots,
    Potatoes,
    Beetroots,
    NetherWart,
    MelonStem,
    PumpkinStem,
    SweetBerryBush,
    Cocoa,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct CropTickInput {
    kind: CropKind,
    age: u8,
    light: u8,
    farmland_below: bool,
    /// Moisture values for farmland, `None` for any other block; row-major by z then x.
    farmland_3x3: [[Option<u8>; 3]; 3],
    same_crop_north_south: bool,
    same_crop_east_west: bool,
    /// Raw caller-provided random value; modulo the species-specific bound chooses growth.
    random_value: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
fn crop_growth_speed(
    farmland_3x3: [[Option<u8>; 3]; 3],
    same_crop_north_south: bool,
    same_crop_east_west: bool,
) -> f32 {
    let mut speed = 1.0_f32;
    for (z, row) in farmland_3x3.iter().enumerate() {
        for (x, moisture) in row.iter().enumerate() {
            let mut contribution = match moisture {
                Some(moisture) if *moisture > 0 => 3.0,
                Some(_) => 1.0,
                None => 0.0,
            };
            if x != 1 || z != 1 {
                contribution /= 4.0;
            }
            speed += contribution;
        }
    }
    // CropBlock halves growth speed only when the same crop borders on both axes.
    if same_crop_north_south && same_crop_east_west {
        speed /= 2.0;
    }
    speed
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_crop_random_tick(input: CropTickInput) -> Option<u8> {
    let max_age = match input.kind {
        CropKind::Beetroots | CropKind::NetherWart | CropKind::SweetBerryBush => 3,
        CropKind::Cocoa => 2,
        _ => 7,
    };
    if input.age >= max_age {
        return None;
    }
    let grows = match input.kind {
        CropKind::NetherWart => input.random_value % 10 == 0,
        CropKind::Cocoa | CropKind::SweetBerryBush => input.random_value % 5 == 0,
        CropKind::Wheat
        | CropKind::Carrots
        | CropKind::Potatoes
        | CropKind::Beetroots
        | CropKind::MelonStem
        | CropKind::PumpkinStem => {
            if input.light < 9 || !input.farmland_below {
                return None;
            }
            let speed = crop_growth_speed(
                input.farmland_3x3,
                input.same_crop_north_south,
                input.same_crop_east_west,
            );
            let bound = (25.0_f32 / speed) as u32 + 1;
            input.random_value % bound == 0
        }
    };
    grows.then_some(input.age + 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct RandomTickSection {
    chunk_x: i32,
    section_y: i32,
    chunk_z: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum RandomTickDispatch {
    Crop,
    Fire,
    Leaves,
    Snow,
    Ice,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct RandomTickSample {
    pos: BlockPos,
    dispatch: RandomTickDispatch,
}

/// Java's 48-bit LCG, locally seeded for reproducible section sampling.
#[cfg_attr(not(test), allow(dead_code))]
struct WorldMechanismLcg(u64);

#[cfg_attr(not(test), allow(dead_code))]
impl WorldMechanismLcg {
    fn new(seed: u64) -> Self {
        Self((seed ^ 0x5deece66d) & ((1_u64 << 48) - 1))
    }
    fn next_bits(&mut self, bits: u32) -> u32 {
        self.0 = self.0.wrapping_mul(0x5deece66d).wrapping_add(0xb) & ((1_u64 << 48) - 1);
        (self.0 >> (48 - bits)) as u32
    }
    fn next_int(&mut self, bound: u32) -> u32 {
        assert!(bound > 0);
        if bound.is_power_of_two() {
            return ((u64::from(bound) * u64::from(self.next_bits(31))) >> 31) as u32;
        }
        loop {
            let bits = self.next_bits(31);
            let value = bits % bound;
            if bits.wrapping_sub(value).wrapping_add(bound - 1) < (1 << 31) {
                return value;
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_random_ticks(
    sections: &[RandomTickSection],
    random_tick_speed: u32,
    seed: u64,
    mut block_name: impl FnMut(BlockPos) -> String,
) -> Vec<RandomTickSample> {
    let mut rng = WorldMechanismLcg::new(seed);
    let mut samples = Vec::new();
    for section in sections {
        for _ in 0..random_tick_speed {
            let pos = (
                section.chunk_x * 16 + rng.next_int(16) as i32,
                section.section_y * 16 + rng.next_int(16) as i32,
                section.chunk_z * 16 + rng.next_int(16) as i32,
            );
            let name = block_name(pos);
            let dispatch = if matches!(
                name.as_str(),
                "minecraft:wheat"
                    | "minecraft:carrots"
                    | "minecraft:potatoes"
                    | "minecraft:beetroots"
                    | "minecraft:nether_wart"
                    | "minecraft:melon_stem"
                    | "minecraft:pumpkin_stem"
                    | "minecraft:sweet_berry_bush"
                    | "minecraft:cocoa"
            ) {
                RandomTickDispatch::Crop
            } else if name == "minecraft:fire" || name == "minecraft:soul_fire" {
                RandomTickDispatch::Fire
            } else if name.ends_with("_leaves") {
                RandomTickDispatch::Leaves
            } else if name == "minecraft:snow" {
                RandomTickDispatch::Snow
            } else if name == "minecraft:ice" {
                RandomTickDispatch::Ice
            } else {
                RandomTickDispatch::Other
            };
            samples.push(RandomTickSample { pos, dispatch });
        }
    }
    samples
}

#[cfg_attr(not(test), allow(dead_code))]
const DEFAULT_RANDOM_TICK_SPEED: u32 = 3;

#[cfg_attr(not(test), allow(dead_code))]
fn eval_random_ticks_default(
    sections: &[RandomTickSection],
    seed: u64,
    block_name: impl FnMut(BlockPos) -> String,
) -> Vec<RandomTickSample> {
    eval_random_ticks(sections, DEFAULT_RANDOM_TICK_SPEED, seed, block_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct FireTickInput {
    age: u8,
    rain_exposed: bool,
    soul_fire: bool,
    /// Adjacent cells in a fixed caller-defined order; true means flammable.
    flammable_neighbors: [bool; 6],
    /// A 0..15 roll for each adjacent cell, where 0 means a spread attempt.
    spread_rolls: [u8; 6],
    /// The 0..2 random value used for vanilla's one-in-three age increment.
    age_roll: u8,
    /// A 0..15 roll for the fire's random burnout check.
    burnout_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct FireTickResult {
    extinguished: bool,
    age: Option<u8>,
    spread: [bool; 6],
    burned_out: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_fire_tick(input: FireTickInput) -> FireTickResult {
    let mut result = FireTickResult {
        extinguished: false,
        age: Some(input.age.min(15)),
        spread: [false; 6],
        burned_out: false,
    };
    if input.rain_exposed && !input.soul_fire {
        result.extinguished = true;
        result.age = None;
        return result;
    }
    if input.soul_fire {
        return result;
    }
    let age = input.age.min(15);
    result.age = Some(if input.age_roll % 3 == 2 {
        age.saturating_add(1).min(15)
    } else {
        age
    });
    for index in 0..6 {
        result.spread[index] = input.flammable_neighbors[index] && input.spread_rolls[index] == 0;
    }
    result.burned_out =
        input.burnout_roll % 4 == 0 && !input.flammable_neighbors.iter().any(|v| *v);
    if result.burned_out {
        result.age = None;
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct LeafDecayInput {
    persistent: bool,
    distance: u8,
    random_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct LeafDecayResult {
    decay: bool,
    drop_item: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_leaf_decay(input: LeafDecayInput) -> LeafDecayResult {
    let decay = !input.persistent && input.distance >= 7;
    LeafDecayResult {
        decay,
        drop_item: decay && input.random_roll % 20 == 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct SnowIceTickInput {
    cold_biome: bool,
    sky_exposed: bool,
    light: u8,
    support_is_full_solid: bool,
    water_source: bool,
    random_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum SnowIceTickResult {
    NoChange,
    AccumulateSnow,
    FreezeWater,
    MeltIce,
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_snow_accumulation(input: SnowIceTickInput) -> SnowIceTickResult {
    if input.cold_biome
        && input.sky_exposed
        && !input.support_is_full_solid
        && input.light < 10
        && input.random_roll % 16 == 0
    {
        SnowIceTickResult::AccumulateSnow
    } else {
        SnowIceTickResult::NoChange
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn eval_ice_freeze_melt(input: SnowIceTickInput, is_ice: bool) -> SnowIceTickResult {
    if is_ice && input.light >= 11 {
        return SnowIceTickResult::MeltIce;
    }
    if !is_ice && input.water_source && input.cold_biome && input.sky_exposed {
        return SnowIceTickResult::FreezeWater;
    }
    SnowIceTickResult::NoChange
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
    fn crop_growth_uses_vanilla_farmland_moisture_and_canopy_penalties() {
        let dry = [[Some(0); 3]; 3];
        let wet = [[Some(7); 3]; 3];
        let empty = [[None; 3]; 3];
        assert_eq!(crop_growth_speed(dry, false, false), 4.0);
        assert_eq!(crop_growth_speed(wet, false, false), 10.0);
        assert_eq!(crop_growth_speed(wet, true, false), 10.0);
        assert_eq!(crop_growth_speed(wet, true, true), 5.0);
        assert_eq!(crop_growth_speed(empty, false, false), 1.0);

        let input = CropTickInput {
            kind: CropKind::Wheat,
            age: 2,
            light: 9,
            farmland_below: true,
            farmland_3x3: wet,
            same_crop_north_south: false,
            same_crop_east_west: false,
            random_value: 0,
        };
        assert_eq!(eval_crop_random_tick(input), Some(3));
        assert_eq!(
            eval_crop_random_tick(CropTickInput { light: 8, ..input }),
            None
        );
        assert_eq!(
            eval_crop_random_tick(CropTickInput {
                farmland_below: false,
                ..input
            }),
            None
        );
        assert_eq!(
            eval_crop_random_tick(CropTickInput { age: 7, ..input }),
            None
        );

        for (kind, age, roll, expected) in [
            (CropKind::NetherWart, 0, 9, None),
            (CropKind::NetherWart, 0, 10, Some(1)),
            (CropKind::Cocoa, 1, 5, Some(2)),
            (CropKind::SweetBerryBush, 2, 10, Some(3)),
            (CropKind::Beetroots, 2, 0, Some(3)),
            (CropKind::Carrots, 2, 0, Some(3)),
            (CropKind::Potatoes, 2, 0, Some(3)),
            (CropKind::MelonStem, 2, 0, Some(3)),
            (CropKind::PumpkinStem, 2, 0, Some(3)),
        ] {
            let input = CropTickInput {
                kind,
                age,
                light: 15,
                farmland_below: true,
                farmland_3x3: wet,
                same_crop_north_south: false,
                same_crop_east_west: false,
                random_value: roll,
            };
            assert_eq!(eval_crop_random_tick(input), expected);
        }
    }

    #[test]
    fn random_tick_samples_are_seeded_section_local_and_dispatch_by_name() {
        let sections = [
            RandomTickSection {
                chunk_x: -2,
                section_y: 4,
                chunk_z: 3,
            },
            RandomTickSection {
                chunk_x: 1,
                section_y: -1,
                chunk_z: -3,
            },
        ];
        let run = |seed| {
            eval_random_ticks_default(&sections, seed, |pos| match pos.0.rem_euclid(6) {
                0 => "minecraft:wheat".to_owned(),
                1 => "minecraft:oak_leaves".to_owned(),
                2 => "minecraft:fire".to_owned(),
                3 => "minecraft:snow".to_owned(),
                4 => "minecraft:ice".to_owned(),
                _ => "minecraft:stone".to_owned(),
            })
        };
        let first = run(1234);
        assert_eq!(first, run(1234));
        assert_ne!(first, run(1235));
        assert_eq!(first.len(), 6);
        assert!(first.iter().all(|tick| {
            let (cx, sy, cz) = (
                tick.pos.0.div_euclid(16),
                tick.pos.1.div_euclid(16),
                tick.pos.2.div_euclid(16),
            );
            sections.contains(&RandomTickSection {
                chunk_x: cx,
                section_y: sy,
                chunk_z: cz,
            })
        }));
        let crops = eval_random_ticks_default(&sections, 1234, |_| "minecraft:wheat".to_owned());
        assert!(crops
            .iter()
            .all(|tick| tick.dispatch == RandomTickDispatch::Crop));
    }

    #[test]
    fn fire_age_spread_rain_burnout_and_soul_fire_are_evaluated() {
        let mut input = FireTickInput {
            age: 14,
            rain_exposed: false,
            soul_fire: false,
            flammable_neighbors: [true, false, true, false, false, false],
            spread_rolls: [0, 0, 1, 0, 0, 0],
            age_roll: 2,
            burnout_roll: 2,
        };
        let result = eval_fire_tick(input);
        assert_eq!(result.age, Some(15));
        assert_eq!(result.spread, [true, false, false, false, false, false]);
        assert!(!result.extinguished && !result.burned_out);
        input.burnout_roll = 0;
        input.flammable_neighbors = [false; 6];
        let result = eval_fire_tick(input);
        assert_eq!(result.age, None);
        assert!(result.burned_out);
        input.rain_exposed = true;
        assert!(eval_fire_tick(input).extinguished);
        input.soul_fire = true;
        let soul = eval_fire_tick(input);
        assert!(!soul.extinguished);
        assert_eq!(soul.age, Some(14));
    }

    #[test]
    fn only_nonpersistent_distance_seven_leaves_decay_and_drop_at_one_in_twenty() {
        for persistent in [true, false] {
            for distance in 1..=7 {
                let result = eval_leaf_decay(LeafDecayInput {
                    persistent,
                    distance,
                    random_roll: 0,
                });
                assert_eq!(result.decay, !persistent && distance == 7);
                assert_eq!(result.drop_item, !persistent && distance == 7);
            }
        }
        assert!(
            !eval_leaf_decay(LeafDecayInput {
                persistent: false,
                distance: 7,
                random_roll: 1
            })
            .drop_item
        );
    }

    #[test]
    fn snow_ice_freezing_and_melting_obey_climate_exposure_and_light() {
        let base = SnowIceTickInput {
            cold_biome: true,
            sky_exposed: true,
            light: 9,
            support_is_full_solid: false,
            water_source: true,
            random_roll: 0,
        };
        assert_eq!(
            eval_snow_accumulation(base),
            SnowIceTickResult::AccumulateSnow
        );
        assert_eq!(
            eval_ice_freeze_melt(base, false),
            SnowIceTickResult::FreezeWater
        );
        assert_eq!(
            eval_ice_freeze_melt(SnowIceTickInput { light: 10, ..base }, true),
            SnowIceTickResult::NoChange
        );
        assert_eq!(
            eval_ice_freeze_melt(SnowIceTickInput { light: 11, ..base }, true),
            SnowIceTickResult::MeltIce
        );
        assert_eq!(
            eval_snow_accumulation(SnowIceTickInput { light: 10, ..base }),
            SnowIceTickResult::NoChange
        );
        assert_eq!(
            eval_snow_accumulation(SnowIceTickInput {
                sky_exposed: false,
                ..base
            }),
            SnowIceTickResult::NoChange
        );
        assert_eq!(
            eval_snow_accumulation(SnowIceTickInput {
                cold_biome: false,
                ..base
            }),
            SnowIceTickResult::NoChange
        );
        assert_eq!(
            eval_snow_accumulation(SnowIceTickInput {
                support_is_full_solid: true,
                ..base
            }),
            SnowIceTickResult::NoChange
        );
        assert_eq!(
            eval_ice_freeze_melt(
                SnowIceTickInput {
                    water_source: false,
                    ..base
                },
                false
            ),
            SnowIceTickResult::NoChange
        );
    }

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

    fn run_border_command(
        border: &mut WorldBorderState,
        args: &[&str],
        now_ms: u64,
    ) -> Result<WorldBorderCommandResult, WorldBorderCommandError> {
        WorldBorderCommand::parse(args, 0.0, 0.0)?.execute(border, None, now_ms)
    }

    #[test]
    fn border_commands_encode_all_six_packets() {
        let mut border = WorldBorderState {
            absolute_max_size: 300,
            ..WorldBorderState::default()
        };
        assert_eq!(
            run_border_command(&mut border, &["center", "1", "-2.0"], 0)
                .unwrap()
                .packet,
            Some((
                0x4d,
                vec![
                    0x3f, 0xf8, 0, 0, 0, 0, 0, 0, // X = 1.5, centered absolute integer.
                    0xc0, 0x00, 0, 0, 0, 0, 0, 0, // Z = -2, explicit decimal.
                ]
            )),
        );
        assert_eq!(
            run_border_command(&mut border, &["set", "8"], 0)
                .unwrap()
                .packet,
            Some((0x4f, vec![0x40, 0x20, 0, 0, 0, 0, 0, 0])), // Diameter 8.
        );
        assert_eq!(
            run_border_command(&mut border, &["set", "4", "1"], 10)
                .unwrap()
                .packet,
            Some((
                0x4e,
                vec![
                    0x40, 0x20, 0, 0, 0, 0, 0, 0, // Old diameter 8.
                    0x40, 0x10, 0, 0, 0, 0, 0, 0, // New diameter 4.
                    0xe8, 0x07, // VarLong 1000 ms.
                ]
            )),
        );
        assert_eq!(
            run_border_command(&mut border, &["warning", "time", "200"], 510)
                .unwrap()
                .packet,
            Some((0x50, vec![0xc8, 0x01])), // VarInt 200 seconds.
        );
        assert_eq!(
            run_border_command(&mut border, &["warning", "distance", "150"], 510)
                .unwrap()
                .packet,
            Some((0x51, vec![0x96, 0x01])), // VarInt 150 blocks.
        );
        assert_eq!(
            border.initialize_packet(510),
            (
                0x25,
                vec![
                    0x3f, 0xf8, 0, 0, 0, 0, 0, 0, // X = 1.5.
                    0xc0, 0x00, 0, 0, 0, 0, 0, 0, // Z = -2.
                    0x40, 0x18, 0, 0, 0, 0, 0, 0, // Current diameter 6.
                    0x40, 0x10, 0, 0, 0, 0, 0, 0, // Target diameter 4.
                    0xf4, 0x03, // Remaining time 500 ms.
                    0xac, 0x02, // Absolute coordinate limit 300.
                    0x96, 0x01, // Warning blocks 150.
                    0xc8, 0x01, // Warning time 200 seconds.
                ]
            )
        );

        let expired = border.initialize_packet(1_011);
        assert_eq!(
            &expired.1[32..42],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], // VarLong -1 ms.
        );
        border.finish_lerp(1_011);
        assert_eq!(
            border.initialize_packet(1_011),
            (
                0x25,
                vec![
                    0x3f, 0xf8, 0, 0, 0, 0, 0, 0, // X.
                    0xc0, 0x00, 0, 0, 0, 0, 0, 0, // Z.
                    0x40, 0x10, 0, 0, 0, 0, 0, 0, // Diameter 4.
                    0x40, 0x10, 0, 0, 0, 0, 0, 0,    // Static target is also 4.
                    0x00, // Border tick replaces the elapsed lerp with a static extent.
                    0xac, 0x02, 0x96, 0x01, 0xc8, 0x01, // Unchanged limit and warnings.
                ]
            )
        );
    }

    #[test]
    fn border_commands_enforce_ranges_before_mutation() {
        use WorldBorderCommandError::*;
        let mut border = WorldBorderState::default();
        let original = border;
        for (args, error) in [
            (
                &["set", "59999969"][..],
                NumberTooHigh {
                    kind: "double",
                    maximum: "59999968".into(),
                    found: "59999969".into(),
                },
            ),
            (
                &["set", "-59999969"],
                NumberTooLow {
                    kind: "double",
                    minimum: "-59999968".into(),
                    found: "-59999969".into(),
                },
            ),
            (
                &["set", "0.999"],
                border_rejected("commands.worldborder.set.failed.small", []),
            ),
            (
                &["add", "1"],
                border_rejected(
                    "commands.worldborder.set.failed.big",
                    ["5.9999968E7".into()],
                ),
            ),
            (
                &["set", "59999968"],
                border_rejected("commands.worldborder.set.failed.nochange", []),
            ),
            (
                &["set", "10", "-1"],
                NumberTooLow {
                    kind: "integer",
                    minimum: "0".into(),
                    found: "-1".into(),
                },
            ),
            (
                &["add", "0", "2147483648"],
                InvalidNumber("integer", "2147483648".into()),
            ),
            (
                &["warning", "distance", "-1"],
                NumberTooLow {
                    kind: "integer",
                    minimum: "0".into(),
                    found: "-1".into(),
                },
            ),
            (
                &["warning", "time", "15"],
                border_rejected("commands.worldborder.warning.time.failed", []),
            ),
            (
                &["damage", "buffer", "-0.5"],
                NumberTooLow {
                    kind: "float",
                    minimum: "0".into(),
                    found: "-0.5".into(),
                },
            ),
            (&["damage", "amount", "NaN"], ExpectedNumber("float")),
            (&["set", "1e3"], Syntax),
            (
                &["center", "29999986", "0"],
                border_rejected(
                    "commands.worldborder.set.failed.far",
                    ["2.9999984E7".into()],
                ),
            ),
            (&["center", "^1", "~"], MixedCoordinates),
            (
                &["center", "~", "~"],
                border_rejected("commands.worldborder.center.failed", []),
            ),
            (&["get", "extra"], Syntax),
        ] {
            assert_eq!(
                run_border_command(&mut border, args, 0),
                Err(error),
                "{args:?}"
            );
            assert_eq!(border, original, "{args:?}");
        }
        assert_eq!(
            run_border_command(&mut border, &["set", "1"], 0)
                .unwrap()
                .result,
            -59_999_967,
        );
        assert_eq!(border.current_size(0), 1.0);
        assert_eq!(
            run_border_command(&mut border, &["set", "59999968", "2147483647"], 0)
                .unwrap()
                .packet
                .unwrap()
                .1[16..],
            [0x98, 0xf8, 0xff, 0xff, 0xbf, 0x3e], // 2,147,483,647,000 ms as a six-byte VarLong.
        );
        assert_eq!(border.lerp_remaining_ms(0), 2_147_483_647_000);
    }

    #[test]
    fn border_commands_restart_lerp_and_extend_time_only_for_timed_add() {
        let mut border = WorldBorderState::default();
        border.lerp_to(100.0, 0, 0);
        let shrink = run_border_command(&mut border, &["set", "20", "2"], 1_000).unwrap();
        assert_eq!(shrink.result, -80);
        assert_eq!(
            shrink.feedback,
            WorldBorderFeedback::new(
                "commands.worldborder.set.shrink",
                ["20.0".into(), "2".into()]
            )
        );
        assert!(shrink.broadcast_feedback);
        assert_eq!(border.current_size(1_500), 80.0);

        let grow = run_border_command(&mut border, &["add", "20", "3"], 1_500).unwrap();
        assert_eq!(grow.result, 20);
        assert_eq!(
            grow.feedback,
            WorldBorderFeedback::new(
                "commands.worldborder.set.grow",
                ["100.0".into(), "4".into()]
            )
        );
        assert_eq!(border.lerp_remaining_ms(1_500), 4_500);
        assert_eq!(border.current_size(3_750), 90.0);
        let immediate = run_border_command(&mut border, &["add", "10"], 3_750).unwrap();
        assert_eq!(immediate.feedback.key, "commands.worldborder.set.immediate");
        assert_eq!(immediate.result, 10);
        assert_eq!(border.lerp_remaining_ms(3_750), 0);
        assert_eq!(border.current_size(3_750), 100.0);

        // The size and remaining duration for add come from the source dimension.
        let mut source = WorldBorderState::default();
        source.lerp_to(50.0, 0, 0);
        source.lerp_to(30.0, 4_000, 2_000);
        let add = WorldBorderCommand::parse(&["add", "5", "0"], 0.0, 0.0).unwrap();
        assert_eq!(
            add.execute(&mut border, Some(&source), 5_000)
                .unwrap()
                .result,
            -55
        );
        assert_eq!(border.lerp_remaining_ms(5_000), 1_000);
        assert_eq!(border.current_size(5_500), 72.5);
        assert_eq!(border.current_size(6_000), 45.0);
    }

    #[test]
    fn border_commands_return_vanilla_feedback_and_server_only_damage_changes() {
        let mut border = WorldBorderState::default();
        let center = WorldBorderCommand::parse(&["center", "~0.125", "~-0.125"], 0.0, 0.0).unwrap();
        let centered = center.execute(&mut border, None, 0).unwrap();
        assert_eq!(centered.result, 0);
        assert_eq!(
            centered.feedback,
            WorldBorderFeedback::new(
                "commands.worldborder.center.success",
                ["0.13".into(), "-0.13".into()]
            )
        );

        let set = run_border_command(&mut border, &["set", "2.25"], 0).unwrap();
        assert_eq!(set.feedback.arguments, ["2.3"]);
        assert_eq!(set.result, -59_999_965);
        run_border_command(&mut border, &["set", "2.5"], 0).unwrap();
        let get = run_border_command(&mut border, &["get"], 0).unwrap();
        assert_eq!(
            get,
            WorldBorderCommandResult {
                result: 3,
                feedback: WorldBorderFeedback::new("commands.worldborder.get", ["3".into()]),
                broadcast_feedback: false,
                packet: None,
            }
        );

        let amount = run_border_command(&mut border, &["damage", "amount", "0.2"], 0).unwrap();
        // The default is double 0.2, while commands widen float 0.2 to double.
        assert_eq!(border.damage_per_block, f64::from(0.2f32));
        assert_eq!(
            amount.feedback,
            WorldBorderFeedback::new(
                "commands.worldborder.damage.amount.success",
                ["0.20".into()]
            )
        );
        assert!(amount.broadcast_feedback);
        assert_eq!(amount.packet, None);
        assert_eq!(
            run_border_command(&mut border, &["damage", "amount", "0.2"], 0),
            Err(border_rejected(
                "commands.worldborder.damage.amount.failed",
                []
            )),
        );
        let buffer = run_border_command(&mut border, &["damage", "buffer", "2.125"], 0).unwrap();
        assert_eq!(border.damage_safe_zone, 2.125);
        assert_eq!(buffer.result, 2);
        assert_eq!(
            buffer.feedback,
            WorldBorderFeedback::new(
                "commands.worldborder.damage.buffer.success",
                ["2.13".into()]
            )
        );
        assert_eq!(buffer.packet, None);
        let time = run_border_command(&mut border, &["warning", "time", "0"], 0).unwrap();
        assert_eq!(
            time.feedback,
            WorldBorderFeedback::new("commands.worldborder.warning.time.success", ["0".into()])
        );
        let distance = run_border_command(&mut border, &["warning", "distance", "0"], 0).unwrap();
        assert_eq!(
            distance.feedback,
            WorldBorderFeedback::new(
                "commands.worldborder.warning.distance.success",
                ["0".into()]
            )
        );
    }

    #[test]
    fn worldborder_command_updates_size_center_damage_and_warnings() {
        let mut border = WorldBorderState::default();
        run_border_command(&mut border, &["set", "100"], 0).unwrap();
        assert_eq!(border.current_size(0), 100.0);
        run_border_command(&mut border, &["set", "60", "2"], 500).unwrap();
        assert_eq!(
            run_border_command(&mut border, &["get"], 1_500)
                .unwrap()
                .result,
            80
        );
        run_border_command(&mut border, &["add", "-20", "3"], 1_500).unwrap();
        assert_eq!(border.lerp_duration_ms, 4_000);
        assert_eq!(border.current_size(3_500), 70.0);
        run_border_command(&mut border, &["add", "4.5"], 3_500).unwrap();
        assert_eq!(border.lerp_duration_ms, 0);
        assert_eq!(border.current_size(3_500), 74.5);
        assert_eq!(
            run_border_command(&mut border, &["get"], 3_500)
                .unwrap()
                .result,
            75
        );
        for args in [
            &["center", "10", "-4.25"][..],
            &["damage", "amount", "1.5"][..],
            &["damage", "buffer", "0"][..],
            &["warning", "distance", "12"][..],
            &["warning", "time", "25"][..],
        ] {
            run_border_command(&mut border, args, 3_500).unwrap();
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
                run_border_command(&mut border, args, 0).is_err(),
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
        for (now_ms, expected) in [(0, 100.0), (10, 100.0), (20, 60.0), (30, 20.0), (40, 20.0)] {
            assert_eq!(border.current_size(now_ms), expected);
        }
        border.lerp_to(100.0, 30, 20);
        for (now_ms, expected) in [(30, 20.0), (40, 60.0), (50, 100.0)] {
            assert_eq!(border.current_size(now_ms), expected);
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
            .any(|(definition, c)| definition.id == END && c.is_empty()));
        assert!(tracker.take_delta("Steve", false).is_none());

        assert!(tracker.grant_criterion("Alex", IRON, "iron", 15));
        assert!(tracker.completed("Alex", END));
        let restored = tracker.take_delta("Alex", false).unwrap();
        assert!(restored
            .progress
            .iter()
            .any(|(definition, c)| definition.id == END && c.get("entered_end") == Some(&10)));
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
        border.lerp_to(180.0, 500, 1_000);
        for (now_ms, expected) in [(1_000, (140.0, 180.0, 500)), (1_500, (180.0, 180.0, 0))] {
            let (id, payload) = border.initialize_packet(now_ms);
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

        let (id, payload) = WorldBorderUpdate::Center(-15.5, 33.25).into_packet();
        assert_eq!(id, 0x4d);
        let mut packet = payload.as_slice();
        assert_eq!(read_packet_double(&mut packet), -15.5);
        assert_eq!(read_packet_double(&mut packet), 33.25);
        assert!(packet.is_empty());

        let (id, payload) = WorldBorderUpdate::Size(1.5).into_packet();
        assert_eq!(id, 0x4f);
        let mut packet = payload.as_slice();
        assert_eq!(read_packet_double(&mut packet), 1.5);
        assert!(packet.is_empty());

        let (id, payload) = WorldBorderUpdate::LerpSize {
            from: 100.0,
            to: 29.5,
            duration_ms: 50_000_000_000,
        }
        .into_packet();
        assert_eq!(id, 0x4e);
        let mut packet = payload.as_slice();
        assert_eq!(read_packet_lerp(&mut packet), (100.0, 29.5, 50_000_000_000));
        assert_eq!(payload.len(), 16 + 6); // VarLong needs more than five bytes.
        assert!(packet.is_empty());
    }

    #[test]
    fn scoreboard_objective_packets_encode_display_modes_and_number_formats() {
        let objective = ScoreboardObjective {
            name: "o".into(),
            criteria: ScoreboardCriteria::Health,
            display_name: "HP".into(),
            render_type: ScoreboardRenderType::Hearts,
        };
        assert_eq!(
            encode_scoreboard_display(ScoreboardDisplaySlot::Sidebar, Some("o")),
            (0x57, b"\x01\x01o".to_vec()), // Sidebar enum, objective name.
        );
        assert_eq!(
            encode_scoreboard_display(ScoreboardDisplaySlot::BelowName, None),
            (0x57, vec![2, 0]), // Below-name enum, empty string clears the slot.
        );
        assert_eq!(
            encode_scoreboard_objective(ScoreboardObjectivePacket::Create(&objective, None)),
            (0x5e, b"\x01o\x00\x08\x00\x02HP\x01\x00".to_vec()), // Name, create, TAG_String, hearts, absent format.
        );
        assert_eq!(
            encode_scoreboard_objective(ScoreboardObjectivePacket::Update(
                &objective,
                Some(ScoreboardNumberFormat::Blank)
            )),
            (0x5e, b"\x01o\x02\x08\x00\x02HP\x01\x01\x00".to_vec()), // Change, hearts, present blank format ID 0.
        );
        let bold_style = b"\x0a\x01\x00\x04bold\x01\x00";
        assert_eq!(
            encode_scoreboard_objective(ScoreboardObjectivePacket::Update(
                &objective,
                Some(ScoreboardNumberFormat::Styled(bold_style))
            )),
            (
                0x5e,
                b"\x01o\x02\x08\x00\x02HP\x01\x01\x01\x0a\x01\x00\x04bold\x01\x00".to_vec()
            ), // Present styled ID 1, compound {bold:1b}.
        );
        assert_eq!(
            encode_scoreboard_objective(ScoreboardObjectivePacket::Remove("o")),
            (0x5e, b"\x01o\x01".to_vec()), // Removal has only name and mode.
        );
    }

    #[test]
    fn scoreboard_score_packets_encode_optional_components_and_objectives() {
        assert_eq!(
            encode_scoreboard_score("P", "o", -1, None, None),
            (0x61, b"\x01P\x01o\xff\xff\xff\xff\x0f\x00\x00".to_vec()), // Owner, objective, signed VarInt -1, both options absent.
        );
        let display = nbt_string("P2");
        let fixed = nbt_string("X");
        assert_eq!(
            encode_scoreboard_score(
                "P",
                "o",
                130,
                Some(&display),
                Some(ScoreboardNumberFormat::Fixed(&fixed))
            ),
            (
                0x61,
                [
                    &b"\x01P\x01o\x82\x01"[..], // Owner, objective, VarInt 130.
                    b"\x01\x08\x00\x02P2",      // Present display name, TAG_String "P2".
                    b"\x01\x02\x08\x00\x01X",   // Present fixed format ID 2, TAG_String "X".
                ]
                .concat()
            ),
        );
        assert_eq!(
            encode_scoreboard_reset("P", Some("o")),
            (0x44, b"\x01P\x01\x01o".to_vec()), // Owner, present objective name.
        );
        assert_eq!(
            encode_scoreboard_reset("P", None),
            (0x44, b"\x01P\x00".to_vec()), // Owner, absent objective resets every score.
        );
    }

    #[test]
    fn scoreboard_objective_and_display_packets_round_trip_protocol_767_payloads() {
        let mut objective = objective("life", ScoreboardCriteria::Health);
        objective.display_name = "Vie ❤️".to_owned();
        for (mode, packet) in [
            (0, ScoreboardObjectivePacket::Create(&objective, None)),
            (1, ScoreboardObjectivePacket::Remove(&objective.name)),
            (2, ScoreboardObjectivePacket::Update(&objective, None)),
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
            let display = display_name.map(nbt_string);
            let (id, payload) =
                encode_scoreboard_score("Alex", "life", value, display.as_deref(), None);
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
        assert_eq!(read_varint(&mut packet), Some(1));
        assert_eq!(read_packet_string(&mut packet), "crafting_table");
        assert_eq!(packet, &[0]); // Criterion present, timestamp absent.
    }

    #[test]
    fn advancements_encode_partial_progress_and_revocation_byte_for_byte() {
        const DEFINITIONS: &[AdvancementDefinition] = &[AdvancementDefinition {
            id: "test:root",
            parent: None,
            requirements: &[&["a", "b"], &["b", "c"]],
        }];
        let mut tracker = AdvancementTracker {
            definitions: DEFINITIONS,
            ..AdvancementTracker::default()
        };
        assert!(tracker.grant_criterion("Alex", "test:root", "a", 0x01_02_03_04_05_06_07_08));
        assert_eq!(
            tracker.take_delta("Alex", true).unwrap().into_packet(),
            (
                0x74,
                [
                    &b"\x01\x01"[..],                             // Reset, one added advancement.
                    b"\x09test:root\x00\x00",                     // ID, no parent or display.
                    b"\x02\x02\x01a\x01b\x02\x01b\x01c\x01",      // Requirement groups, telemetry.
                    b"\x00\x01\x09test:root\x03", // No removals, one progress entry, three distinct criteria.
                    b"\x01a\x01\x01\x02\x03\x04\x05\x06\x07\x08", // Earned, epoch milliseconds as a Long.
                    b"\x01b\x00\x01c\x00",                        // Unobtained criteria.
                ]
                .concat()
            ),
        );

        assert!(tracker.revoke_criterion("Alex", "test:root", "a"));
        assert_eq!(
            tracker.take_delta("Alex", false).unwrap().into_packet(),
            (
                0x74,
                [
                    &b"\x00\x00\x00\x01\x09test:root\x03"[..], // Delta, no definition changes, three criteria.
                    b"\x01a\x00\x01b\x00\x01c\x00",            // All unobtained.
                ]
                .concat()
            ),
        );
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
