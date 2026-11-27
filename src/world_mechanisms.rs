// R7 GP-7 evaluators. Included by main.rs alongside BlockPos and the wire helpers.

/// Resolve a fluid reaction to the block-registry state used by the world writer.
fn fluid_solidification_state(reaction: flowing_fluid::Solidification) -> Option<u32> {
    let name = match reaction {
        flowing_fluid::Solidification::Obsidian => "minecraft:obsidian",
        flowing_fluid::Solidification::Cobblestone => "minecraft:cobblestone",
        flowing_fluid::Solidification::Stone => "minecraft:stone",
        flowing_fluid::Solidification::Basalt => "minecraft:basalt",
    };
    crate::block_registry::breg().map(|registry| registry.state_of(name, None))
}

/// Empty-collision hosts that water replaces instead of waterlogging. Rails and other
/// SimpleWaterloggedBlocks retain their host and therefore stay out of this set.
fn fluid_washes_away(reg: &crate::block_registry::BReg, state: u32) -> bool {
    if reg.is_air(state) || reg.get_prop(state, "waterlogged").is_some() {
        return false;
    }
    let name = reg.name_of(state);
    if matches!(
        name,
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
    // The non-waterloggable 1.21.1 blocks a fluid spread destroys outright —
    // cave plants, vines, carpets, webs and redstone parts. Named explicitly
    // so a state whose collision metadata does not report an empty shape
    // still classifies `Passable` instead of stalling the spring tick.
    if matches!(
        name,
        "minecraft:vine"
            | "minecraft:cave_vines"
            | "minecraft:cave_vines_plant"
            | "minecraft:weeping_vines"
            | "minecraft:weeping_vines_plant"
            | "minecraft:twisting_vines"
            | "minecraft:twisting_vines_plant"
            | "minecraft:spore_blossom"
            | "minecraft:moss_carpet"
            | "minecraft:pink_petals"
            | "minecraft:cobweb"
            | "minecraft:redstone_wire"
            | "minecraft:lever"
            | "minecraft:tripwire"
            | "minecraft:tripwire_hook"
            | "minecraft:dead_bush"
            | "minecraft:fern"
            | "minecraft:large_fern"
            | "minecraft:short_grass"
            | "minecraft:tall_grass"
    ) {
        return true;
    }
    reg.is_collision_empty(state) && !reg.blocks_motion(state)
}

/// Per-tick scratch for [`tick_world_block_entities`]: the mob snapshot and
/// the crafter re-poll list are rebuilt every tick but never escape the
/// pass, so they ride one bounded thread-local instead of two fresh `Vec`s
/// per tick. The whole struct is taken out for the pass so re-entrant use
/// still sees empty buffers. The sorted position list moved to the
/// fingerprint-keyed [`BE_TICK_POSITIONS`] cache — it persists across ticks
/// instead of being rebuilt.
#[derive(Default)]
struct BeTickScratch {
    live_mobs: Vec<(i64, [f64; 3])>,
    crafter_power: Vec<(BlockPos, bool)>,
}

thread_local! {
    static BE_TICK_SCRATCH: std::cell::RefCell<BeTickScratch> =
        std::cell::RefCell::new(BeTickScratch::default());
}

/// Per-dimension dispatch observability for [`tick_world_block_entities`].
/// Counts live behind their own map (not inside the position entry) so a
/// cache rebuild cannot reset them, and per-dim so concurrent ticks of
/// other dimensions — including other tests — cannot pollute a fixture's
/// numbers. `dispatched` counts entities that reached a machine arm;
/// `skipped` counts registered ticking entities idled out before dispatch;
/// `rebuilds`/`hits` count sorted-position cache misses and matches.
#[derive(Default)]
struct BeDimCounts {
    dispatched: AtomicU64,
    skipped: AtomicU64,
    rebuilds: AtomicU64,
    hits: AtomicU64,
}

fn be_dim_counts() -> &'static Mutex<HashMap<DimId, Arc<BeDimCounts>>> {
    static COUNTS: OnceLock<Mutex<HashMap<DimId, Arc<BeDimCounts>>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn be_counts_for(dim: DimId) -> Arc<BeDimCounts> {
    be_dim_counts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(dim)
        .or_insert_with(|| Arc::new(BeDimCounts::default()))
        .clone()
}

/// One dimension's cached sorted position list. `fingerprint` covers every
/// `(pos, kind)` pair of the ticking-capable entities in the dimension, so a
/// match proves the sorted list is still exact.
struct BeTickPositions {
    fingerprint: u128,
    /// Sorted `(x, y, z)` of ticking-capable entities only — lecterns,
    /// chiseled bookshelves and decorated pots match the tick's `_` arm and
    /// are excluded, which also keeps them out of the fingerprint so their
    /// churn cannot invalidate the list.
    positions: Arc<Vec<(i32, i32, i32)>>,
}

fn be_tick_positions() -> &'static Mutex<HashMap<DimId, BeTickPositions>> {
    static CACHE: OnceLock<Mutex<HashMap<DimId, BeTickPositions>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The tick loop's machine arms, as a discriminant for the position
/// fingerprint. `0` marks the families that fall through to `_`.
fn be_tick_kind(entity: &VanillaBlockEntity) -> u8 {
    match entity {
        VanillaBlockEntity::Campfire(_) => 1,
        VanillaBlockEntity::BrewingStand(_) => 2,
        VanillaBlockEntity::MobSpawner(_) => 3,
        VanillaBlockEntity::Jukebox(_) => 4,
        VanillaBlockEntity::Crafter(_) => 5,
        VanillaBlockEntity::Beacon(_) => 6,
        _ => 0,
    }
}

/// splitmix64-style lane mixer for the position fingerprint — order-free
/// (per-key lanes are summed), deterministic, and far cheaper than a
/// cryptographic hash over what is only a cache key, not a security check.
fn be_fp_mix(a: u64, b: u64) -> (u64, u64) {
    let finish = |mut x: u64| {
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    };
    (
        finish(a ^ 0x9E37_79B9_7F4A_7C15),
        finish(b ^ 0xC2B2_AE3D_27D4_EB4F),
    )
}

/// Order-independent 128-bit fingerprint of the dimension's ticking
/// `(pos, kind)` set. The registry is a plain `Mutex<HashMap>` that
/// `save_v2`'s footer restore rewrites directly, so no mutation hook exists
/// to hang a version counter on — folding the keys each tick catches every
/// writer, including ones outside `register_world_block_entity`, while a
/// steady world pays one hash pass and no `Vec` copy or sort. The same pass
/// detects any armed spawner whose delay has expired.
fn be_dim_tick_state(
    store: &HashMap<DimBlockPos, VanillaBlockEntity>,
    dim: DimId,
    players: &[[f64; 3]],
) -> (u128, bool) {
    let mut lo = 0u64;
    let mut hi = 0u64;
    let mut n = 0u64;
    let mut needs_mobs = false;
    for (&(d, x, y, z), entity) in store {
        if d != dim {
            continue;
        }
        let kind = be_tick_kind(entity);
        if kind == 0 {
            continue;
        }
        if !needs_mobs && !players.is_empty() {
            if let VanillaBlockEntity::MobSpawner(spawner) = entity {
                needs_mobs = spawner.delay == 0 && spawner_has_player(spawner, (x, y, z), players);
            }
        }
        n += 1;
        let a = (x as u32 as u64) | ((y as u32 as u64) << 32);
        let b = (z as u32 as u64) | ((u64::from(kind)) << 32);
        let (ma, mb) = be_fp_mix(a, b);
        lo = lo.wrapping_add(ma);
        hi = hi.wrapping_add(mb);
    }
    lo ^= n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hi ^= n.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    ((u128::from(hi) << 64) | u128::from(lo), needs_mobs)
}

fn spawner_has_player(
    spawner: &MobSpawnerBlockEntity,
    (x, y, z): BlockPos,
    players: &[[f64; 3]],
) -> bool {
    players.iter().any(|p| {
        spawner.player_in_range(
            p[0] - (f64::from(x) + 0.5),
            p[1] - (f64::from(y) + 0.5),
            p[2] - (f64::from(z) + 0.5),
        )
    })
}

const WORLD_BORDER_MAX_SIZE: f64 = 59_999_968.0;

const WORLD_BORDER_MAX_CENTER: i32 = 29_999_984;

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

    #[cfg(test)]
    fn distance_to_border(&self, x: f64, z: f64, now_ms: u64) -> f64 {
        let (min_x, min_z, max_x, max_z) = self.bounds(now_ms);
        (x - min_x).min(max_x - x).min(z - min_z).min(max_z - z)
    }

    #[cfg(test)]
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

type WorldMechanismsPacket = (i64, Vec<u8>);

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

fn write_border_duration(out: &mut Vec<u8>, ms: i64) {
    if ms < 0 {
        // write_varint uses five bytes for i32-range negatives. VarLong retains all 64 bits.
        out.push((ms as u8 & 0x7f) | 0x80);
        write_varint(out, ((ms as u64) >> 7) as i64);
    } else {
        write_varint(out, ms);
    }
}

fn write_doubles(out: &mut Vec<u8>, values: [f64; 2]) {
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WorldBorderFeedback {
    key: &'static str,
    arguments: Vec<String>,
}

impl WorldBorderFeedback {
    fn new(key: &'static str, arguments: impl IntoIterator<Item = String>) -> Self {
        Self {
            key,
            arguments: arguments.into_iter().collect(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
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
struct WorldBorderCommandResult {
    result: i32,
    feedback: WorldBorderFeedback,
    broadcast_feedback: bool,
    packet: Option<WorldMechanismsPacket>,
}

#[derive(Debug, PartialEq)]
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

fn border_rejected(
    key: &'static str,
    arguments: impl IntoIterator<Item = String>,
) -> WorldBorderCommandError {
    WorldBorderCommandError::Rejected(WorldBorderFeedback::new(key, arguments))
}

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
    digits.into_iter().map(char::from).collect()
}

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

fn parse_nonnegative_mechanism_int(token: &str, field: &str) -> Result<i32, String> {
    let value = parse_mechanism_number(token, field)?;
    if value < 0 {
        return Err(format!("{field} must be nonnegative"));
    }
    Ok(value)
}

#[derive(Debug)]
struct AdvancementDefinition {
    id: &'static str,
    parent: Option<&'static str>,
    // All groups must pass. Any one criterion satisfies its group.
    requirements: &'static [&'static [&'static str]],
}

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
    AdvancementDefinition {
        id: "minecraft:adventure/root",
        parent: None,
        requirements: &[&["killed_something", "killed_by_something"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/salvage_sherd",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["desert_pyramid", "desert_well", "ocean_ruin_cold", "ocean_ruin_warm", "trail_ruins_rare", "trail_ruins_common"], &["has_sherd"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/sleep_in_bed",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["slept_in_bed"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/spyglass_at_parrot",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["spyglass_at_parrot"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/trade",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["traded"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/trade_at_world_height",
        parent: Some("minecraft:adventure/trade"),
        requirements: &[&["trade_at_world_height"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/trim_with_any_armor_pattern",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["armor_trimmed_minecraft:bolt_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:coast_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:dune_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:eye_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:flow_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:host_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:raiser_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:rib_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:sentry_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:shaper_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:silence_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:snout_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:spire_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:tide_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:vex_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:ward_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:wayfinder_armor_trim_smithing_template_smithing_trim", "armor_trimmed_minecraft:wild_armor_trim_smithing_template_smithing_trim"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/voluntary_exile",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["voluntary_exile"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/walk_on_powder_snow_with_leather_boots",
        parent: Some("minecraft:adventure/sleep_in_bed"),
        requirements: &[&["walk_on_powder_snow_with_leather_boots"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/dragon_breath",
        parent: Some("minecraft:end/kill_dragon"),
        requirements: &[&["dragon_breath"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/dragon_egg",
        parent: Some("minecraft:end/kill_dragon"),
        requirements: &[&["dragon_egg"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/enter_end_gateway",
        parent: Some("minecraft:end/kill_dragon"),
        requirements: &[&["entered_end_gateway"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/find_end_city",
        parent: Some("minecraft:end/enter_end_gateway"),
        requirements: &[&["in_city"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/levitate",
        parent: Some("minecraft:end/find_end_city"),
        requirements: &[&["levitated"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/respawn_dragon",
        parent: Some("minecraft:end/kill_dragon"),
        requirements: &[&["summoned_dragon"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/root",
        parent: None,
        requirements: &[&["consumed_item"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/safely_harvest_honey",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["safely_harvest_honey"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/silk_touch_nest",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["silk_touch_nest"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/tadpole_in_a_bucket",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["tadpole_bucket"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/tame_an_animal",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["tamed_animal"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/wax_on",
        parent: Some("minecraft:husbandry/safely_harvest_honey"),
        requirements: &[&["wax_on"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/whole_pack",
        parent: Some("minecraft:husbandry/tame_an_animal"),
        requirements: &[&["minecraft:ashen"], &["minecraft:black"], &["minecraft:chestnut"], &["minecraft:pale"], &["minecraft:rusty"], &["minecraft:snowy"], &["minecraft:spotted"], &["minecraft:striped"], &["minecraft:woods"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/distract_piglin",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["distract_piglin", "distract_piglin_directly"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/fast_travel",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["travelled"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/find_bastion",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["bastion"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/find_fortress",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["fortress"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/get_wither_skull",
        parent: Some("minecraft:nether/find_fortress"),
        requirements: &[&["wither_skull"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/loot_bastion",
        parent: Some("minecraft:nether/find_bastion"),
        requirements: &[&["loot_bastion_other", "loot_bastion_treasure", "loot_bastion_hoglin_stable", "loot_bastion_bridge"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/obtain_ancient_debris",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["ancient_debris"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/obtain_blaze_rod",
        parent: Some("minecraft:nether/find_fortress"),
        requirements: &[&["blaze_rod"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/obtain_crying_obsidian",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["crying_obsidian"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/return_to_sender",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["killed_ghast"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/ride_strider",
        parent: Some("minecraft:nether/root"),
        requirements: &[&["used_warped_fungus_on_a_stick"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/ride_strider_in_overworld_lava",
        parent: Some("minecraft:nether/ride_strider"),
        requirements: &[&["ride_entity_distance"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/summon_wither",
        parent: Some("minecraft:nether/get_wither_skull"),
        requirements: &[&["summoned"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/uneasy_alliance",
        parent: Some("minecraft:nether/return_to_sender"),
        requirements: &[&["killed_ghast"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/use_lodestone",
        parent: Some("minecraft:nether/obtain_ancient_debris"),
        requirements: &[&["use_lodestone"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/adventuring_time",
        parent: Some("minecraft:adventure/sleep_in_bed"),
        requirements: &[&["minecraft:mushroom_fields"], &["minecraft:deep_frozen_ocean"], &["minecraft:frozen_ocean"], &["minecraft:deep_cold_ocean"], &["minecraft:cold_ocean"], &["minecraft:deep_ocean"], &["minecraft:ocean"], &["minecraft:deep_lukewarm_ocean"], &["minecraft:lukewarm_ocean"], &["minecraft:warm_ocean"], &["minecraft:stony_shore"], &["minecraft:swamp"], &["minecraft:mangrove_swamp"], &["minecraft:snowy_slopes"], &["minecraft:snowy_plains"], &["minecraft:snowy_beach"], &["minecraft:windswept_gravelly_hills"], &["minecraft:grove"], &["minecraft:windswept_hills"], &["minecraft:snowy_taiga"], &["minecraft:windswept_forest"], &["minecraft:taiga"], &["minecraft:plains"], &["minecraft:meadow"], &["minecraft:beach"], &["minecraft:forest"], &["minecraft:old_growth_spruce_taiga"], &["minecraft:flower_forest"], &["minecraft:birch_forest"], &["minecraft:dark_forest"], &["minecraft:savanna_plateau"], &["minecraft:savanna"], &["minecraft:jungle"], &["minecraft:badlands"], &["minecraft:desert"], &["minecraft:wooded_badlands"], &["minecraft:jagged_peaks"], &["minecraft:stony_peaks"], &["minecraft:frozen_river"], &["minecraft:river"], &["minecraft:ice_spikes"], &["minecraft:old_growth_pine_taiga"], &["minecraft:sunflower_plains"], &["minecraft:old_growth_birch_forest"], &["minecraft:sparse_jungle"], &["minecraft:bamboo_jungle"], &["minecraft:eroded_badlands"], &["minecraft:windswept_savanna"], &["minecraft:cherry_grove"], &["minecraft:frozen_peaks"], &["minecraft:dripstone_caves"], &["minecraft:lush_caves"], &["minecraft:deep_dark"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/avoid_vibration",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["avoid_vibration"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/brush_armadillo",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["brush_armadillo"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/craft_decorated_pot_using_only_sherds",
        parent: Some("minecraft:adventure/salvage_sherd"),
        requirements: &[&["pot_crafted_using_only_sherds"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/crafters_crafting_crafters",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["crafter_crafted_crafter"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/fall_from_world_height",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["fall_from_world_height"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/hero_of_the_village",
        parent: Some("minecraft:adventure/voluntary_exile"),
        requirements: &[&["hero_of_the_village"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/honey_block_slide",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["honey_block_slide"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/kill_a_mob",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["minecraft:blaze", "minecraft:bogged", "minecraft:breeze", "minecraft:cave_spider", "minecraft:creeper", "minecraft:drowned", "minecraft:elder_guardian", "minecraft:ender_dragon", "minecraft:enderman", "minecraft:endermite", "minecraft:evoker", "minecraft:ghast", "minecraft:guardian", "minecraft:hoglin", "minecraft:husk", "minecraft:magma_cube", "minecraft:phantom", "minecraft:piglin", "minecraft:piglin_brute", "minecraft:pillager", "minecraft:ravager", "minecraft:shulker", "minecraft:silverfish", "minecraft:skeleton", "minecraft:slime", "minecraft:spider", "minecraft:stray", "minecraft:vex", "minecraft:vindicator", "minecraft:witch", "minecraft:wither_skeleton", "minecraft:wither", "minecraft:zoglin", "minecraft:zombie_villager", "minecraft:zombie", "minecraft:zombified_piglin"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/kill_all_mobs",
        parent: Some("minecraft:adventure/kill_a_mob"),
        requirements: &[&["minecraft:blaze"], &["minecraft:bogged"], &["minecraft:breeze"], &["minecraft:cave_spider"], &["minecraft:creeper"], &["minecraft:drowned"], &["minecraft:elder_guardian"], &["minecraft:ender_dragon"], &["minecraft:enderman"], &["minecraft:endermite"], &["minecraft:evoker"], &["minecraft:ghast"], &["minecraft:guardian"], &["minecraft:hoglin"], &["minecraft:husk"], &["minecraft:magma_cube"], &["minecraft:phantom"], &["minecraft:piglin"], &["minecraft:piglin_brute"], &["minecraft:pillager"], &["minecraft:ravager"], &["minecraft:shulker"], &["minecraft:silverfish"], &["minecraft:skeleton"], &["minecraft:slime"], &["minecraft:spider"], &["minecraft:stray"], &["minecraft:vex"], &["minecraft:vindicator"], &["minecraft:witch"], &["minecraft:wither_skeleton"], &["minecraft:wither"], &["minecraft:zoglin"], &["minecraft:zombie_villager"], &["minecraft:zombie"], &["minecraft:zombified_piglin"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/kill_mob_near_sculk_catalyst",
        parent: Some("minecraft:adventure/kill_a_mob"),
        requirements: &[&["kill_mob_near_sculk_catalyst"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/lightning_rod_with_villager_no_fire",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["lightning_rod_with_villager_no_fire"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/minecraft_trials_edition",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["minecraft_trials_edition"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/ol_betsy",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["shot_crossbow"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/overoverkill",
        parent: Some("minecraft:adventure/minecraft_trials_edition"),
        requirements: &[&["overoverkill"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/play_jukebox_in_meadows",
        parent: Some("minecraft:adventure/sleep_in_bed"),
        requirements: &[&["play_jukebox_in_meadows"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/read_power_of_chiseled_bookshelf",
        parent: Some("minecraft:adventure/root"),
        requirements: &[&["chiseled_bookshelf", "comparator"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/shoot_arrow",
        parent: Some("minecraft:adventure/kill_a_mob"),
        requirements: &[&["shot_arrow"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/sniper_duel",
        parent: Some("minecraft:adventure/shoot_arrow"),
        requirements: &[&["killed_skeleton"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/spyglass_at_ghast",
        parent: Some("minecraft:adventure/spyglass_at_parrot"),
        requirements: &[&["spyglass_at_ghast"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/summon_iron_golem",
        parent: Some("minecraft:adventure/trade"),
        requirements: &[&["summoned_golem"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/throw_trident",
        parent: Some("minecraft:adventure/kill_a_mob"),
        requirements: &[&["shot_trident"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/totem_of_undying",
        parent: Some("minecraft:adventure/kill_a_mob"),
        requirements: &[&["used_totem"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/trim_with_all_exclusive_armor_patterns",
        parent: Some("minecraft:adventure/trim_with_any_armor_pattern"),
        requirements: &[&["armor_trimmed_minecraft:rib_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:silence_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:snout_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:spire_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:tide_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:vex_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:ward_armor_trim_smithing_template_smithing_trim"], &["armor_trimmed_minecraft:wayfinder_armor_trim_smithing_template_smithing_trim"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/two_birds_one_arrow",
        parent: Some("minecraft:adventure/ol_betsy"),
        requirements: &[&["two_birds"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/under_lock_and_key",
        parent: Some("minecraft:adventure/minecraft_trials_edition"),
        requirements: &[&["under_lock_and_key"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/very_very_frightening",
        parent: Some("minecraft:adventure/throw_trident"),
        requirements: &[&["struck_villager"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/who_needs_rockets",
        parent: Some("minecraft:adventure/minecraft_trials_edition"),
        requirements: &[&["who_needs_rockets"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/whos_the_pillager_now",
        parent: Some("minecraft:adventure/ol_betsy"),
        requirements: &[&["kill_pillager"]],
    },
    AdvancementDefinition {
        id: "minecraft:end/elytra",
        parent: Some("minecraft:end/find_end_city"),
        requirements: &[&["elytra"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/allay_deliver_item_to_player",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["allay_deliver_item_to_player"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/breed_an_animal",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["bred"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/complete_catalogue",
        parent: Some("minecraft:husbandry/tame_an_animal"),
        requirements: &[&["minecraft:all_black"], &["minecraft:black"], &["minecraft:british_shorthair"], &["minecraft:calico"], &["minecraft:jellie"], &["minecraft:persian"], &["minecraft:ragdoll"], &["minecraft:red"], &["minecraft:siamese"], &["minecraft:tabby"], &["minecraft:white"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/fishy_business",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["cod", "tropical_fish", "pufferfish", "salmon"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/leash_all_frog_variants",
        parent: Some("minecraft:husbandry/tadpole_in_a_bucket"),
        requirements: &[&["minecraft:temperate"], &["minecraft:warm"], &["minecraft:cold"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/make_a_sign_glow",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["make_a_sign_glow"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/obtain_sniffer_egg",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["obtain_sniffer_egg"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/plant_seed",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["wheat", "pumpkin_stem", "melon_stem", "beetroots", "nether_wart", "torchflower", "pitcher_pod"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/remove_wolf_armor",
        parent: Some("minecraft:husbandry/tame_an_animal"),
        requirements: &[&["remove_wolf_armor"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/repair_wolf_armor",
        parent: Some("minecraft:husbandry/tame_an_animal"),
        requirements: &[&["repair_wolf_armor"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/ride_a_boat_with_a_goat",
        parent: Some("minecraft:husbandry/root"),
        requirements: &[&["ride_a_boat_with_a_goat"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/tactical_fishing",
        parent: Some("minecraft:husbandry/fishy_business"),
        requirements: &[&["cod_bucket", "tropical_fish_bucket", "pufferfish_bucket", "salmon_bucket"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/wax_off",
        parent: Some("minecraft:husbandry/wax_on"),
        requirements: &[&["wax_off"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/brew_potion",
        parent: Some("minecraft:nether/obtain_blaze_rod"),
        requirements: &[&["potion"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/charge_respawn_anchor",
        parent: Some("minecraft:nether/obtain_crying_obsidian"),
        requirements: &[&["charge_respawn_anchor"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/create_beacon",
        parent: Some("minecraft:nether/summon_wither"),
        requirements: &[&["beacon"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/create_full_beacon",
        parent: Some("minecraft:nether/create_beacon"),
        requirements: &[&["beacon"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/explore_nether",
        parent: Some("minecraft:nether/ride_strider"),
        requirements: &[&["minecraft:nether_wastes"], &["minecraft:soul_sand_valley"], &["minecraft:crimson_forest"], &["minecraft:warped_forest"], &["minecraft:basalt_deltas"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/netherite_armor",
        parent: Some("minecraft:nether/obtain_ancient_debris"),
        requirements: &[&["netherite_armor"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/arbalistic",
        parent: Some("minecraft:adventure/ol_betsy"),
        requirements: &[&["arbalistic"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/blowback",
        parent: Some("minecraft:adventure/minecraft_trials_edition"),
        requirements: &[&["blowback"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/bullseye",
        parent: Some("minecraft:adventure/shoot_arrow"),
        requirements: &[&["bullseye"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/lighten_up",
        parent: Some("minecraft:adventure/minecraft_trials_edition"),
        requirements: &[&["lighten_up"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/revaulting",
        parent: Some("minecraft:adventure/under_lock_and_key"),
        requirements: &[&["revaulting"]],
    },
    AdvancementDefinition {
        id: "minecraft:adventure/spyglass_at_dragon",
        parent: Some("minecraft:adventure/spyglass_at_ghast"),
        requirements: &[&["spyglass_at_dragon"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/allay_deliver_cake_to_note_block",
        parent: Some("minecraft:husbandry/allay_deliver_item_to_player"),
        requirements: &[&["allay_deliver_cake_to_note_block"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/axolotl_in_a_bucket",
        parent: Some("minecraft:husbandry/tactical_fishing"),
        requirements: &[&["axolotl_bucket"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/balanced_diet",
        parent: Some("minecraft:husbandry/plant_seed"),
        requirements: &[&["apple"], &["mushroom_stew"], &["bread"], &["porkchop"], &["cooked_porkchop"], &["golden_apple"], &["enchanted_golden_apple"], &["cod"], &["salmon"], &["tropical_fish"], &["pufferfish"], &["cooked_cod"], &["cooked_salmon"], &["cookie"], &["melon_slice"], &["beef"], &["cooked_beef"], &["chicken"], &["cooked_chicken"], &["rotten_flesh"], &["spider_eye"], &["carrot"], &["potato"], &["baked_potato"], &["poisonous_potato"], &["golden_carrot"], &["pumpkin_pie"], &["rabbit"], &["cooked_rabbit"], &["rabbit_stew"], &["mutton"], &["cooked_mutton"], &["chorus_fruit"], &["beetroot"], &["beetroot_soup"], &["dried_kelp"], &["suspicious_stew"], &["sweet_berries"], &["honey_bottle"], &["glow_berries"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/bred_all_animals",
        parent: Some("minecraft:husbandry/breed_an_animal"),
        requirements: &[&["minecraft:horse"], &["minecraft:donkey"], &["minecraft:mule"], &["minecraft:sheep"], &["minecraft:cow"], &["minecraft:mooshroom"], &["minecraft:pig"], &["minecraft:chicken"], &["minecraft:wolf"], &["minecraft:ocelot"], &["minecraft:rabbit"], &["minecraft:llama"], &["minecraft:cat"], &["minecraft:panda"], &["minecraft:fox"], &["minecraft:bee"], &["minecraft:hoglin"], &["minecraft:strider"], &["minecraft:goat"], &["minecraft:axolotl"], &["minecraft:camel"], &["minecraft:armadillo"], &["minecraft:turtle"], &["minecraft:frog"], &["minecraft:sniffer"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/feed_snifflet",
        parent: Some("minecraft:husbandry/obtain_sniffer_egg"),
        requirements: &[&["feed_snifflet"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/froglights",
        parent: Some("minecraft:husbandry/leash_all_frog_variants"),
        requirements: &[&["froglights"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/kill_axolotl_target",
        parent: Some("minecraft:husbandry/axolotl_in_a_bucket"),
        requirements: &[&["kill_axolotl_target"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/obtain_netherite_hoe",
        parent: Some("minecraft:husbandry/plant_seed"),
        requirements: &[&["netherite_hoe"]],
    },
    AdvancementDefinition {
        id: "minecraft:husbandry/plant_any_sniffer_seed",
        parent: Some("minecraft:husbandry/feed_snifflet"),
        requirements: &[&["torchflower", "pitcher_pod"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/all_potions",
        parent: Some("minecraft:nether/brew_potion"),
        requirements: &[&["all_effects"]],
    },
    AdvancementDefinition {
        id: "minecraft:nether/all_effects",
        parent: Some("minecraft:nether/all_potions"),
        requirements: &[&["all_effects"]],
    },
];

type AdvancementCriteriaProgress = std::collections::BTreeMap<&'static str, i64>;
type PlayerAdvancementProgress = HashMap<&'static str, AdvancementCriteriaProgress>;

struct AdvancementTracker {
    // Definitions are the whole built-in advancement tree, ordered parent
    // before child. Every entry is live: the join-time delta ships all of them,
    // and `/advancement` resolves them all.
    definitions: &'static [AdvancementDefinition],
    progress: HashMap<String, PlayerAdvancementProgress>,
    sent_progress: HashMap<String, PlayerAdvancementProgress>,
}

impl Default for AdvancementTracker {
    /// The built-in `data/minecraft/advancement` tree is the base layer vanilla
    /// itself mounts, so it is the definition set every tracker starts from —
    /// all 122 gameplay advancements (16 story + 24 nether + 9 end + 44
    /// adventure + 29 husbandry). A world that mounts extra datapack
    /// advancements loads them through `commands_core`'s datapack map; the
    /// built-in table stays the fallback the unit tests and a bare server use.
    fn default() -> Self {
        Self {
            definitions: CORE_ADVANCEMENTS,
            progress: HashMap::new(),
            sent_progress: HashMap::new(),
        }
    }
}

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
        let Some(progress) = self.progress.get_mut(player) else {
            return 0;
        };
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

    /// Every requirement group satisfied by the recorded criteria — the
    /// completion test rewards key on, without the parent-visibility walk of
    /// `completed`.
    fn criteria_complete(&self, player: &str, definition: &AdvancementDefinition) -> bool {
        self.progress
            .get(player)
            .and_then(|progress| progress.get(definition.id))
            .is_some_and(|criteria| {
                definition
                    .requirements
                    .iter()
                    .all(|group| group.iter().any(|name| criteria.contains_key(name)))
            })
    }

    fn grant_definition(
        &mut self,
        player: &str,
        definition: &AdvancementDefinition,
        time_ms: i64,
    ) -> usize {
        let was_complete = self.criteria_complete(player, definition);
        let granted = definition
            .requirements
            .iter()
            .copied()
            .flatten()
            .filter(|name| self.grant_criterion(player, definition.id, name, time_ms))
            .count();
        // `Advancement.performRewards`: the false->true completion edge is the
        // one moment the rewards execute, for command and event grants alike.
        if !was_complete && self.criteria_complete(player, definition) {
            live_grant_advancement_rewards(player, definition.id);
        }
        granted
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

fn resolve_command_advancement(
    tracker: &AdvancementTracker,
    id: &str,
) -> Result<&'static AdvancementDefinition, String> {
    let default_id = (!id.contains(':')).then(|| format!("minecraft:{id}"));
    tracker
        .definition(default_id.as_deref().unwrap_or(id))
        .ok_or_else(|| format!("Unknown advancement: {id}"))
}

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
const UPDATE_ADVANCEMENTS_PACKET_ID: i64 = 0x74;

struct AdvancementDelta {
    reset: bool,
    added: &'static [AdvancementDefinition],
    progress: Vec<(&'static AdvancementDefinition, AdvancementCriteriaProgress)>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardCriteria {
    Dummy,
    Trigger,
    DeathCount,
    PlayerKillCount,
    TotalKillCount,
    Health,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardRenderType {
    Integer,
    Hearts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScoreboardObjective {
    name: String,
    criteria: ScoreboardCriteria,
    display_name: String,
    render_type: ScoreboardRenderType,
}

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

/// Hard bound on score holders per objective — a command flood must not
/// grow the owner map without limit. Shared with the command precheck in
/// `commands_core` (`scoreboard_entry_available`).
const SCOREBOARD_ENTRY_LIMIT: usize = 4096;

/// Hard bound on a score-holder (owner) name length in bytes. Player
/// names cap at 16 and entity-UUID holders at 36; 64 leaves headroom for
/// modded selectors while capping wire-string retention per entry.
const SCOREBOARD_OWNER_NAME_MAX: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreboardError {
    DuplicateObjective,
    UnknownObjective,
    ReadOnlyObjective,
    NotTrigger,
    TriggerLocked,
    DivisionByZero,
    TooManyScores,
    NameTooLong,
}

#[derive(Default)]
struct ScoreboardScore {
    value: i32,
    trigger_enabled: bool,
}

#[derive(Default)]
struct ScoreboardEngine {
    objectives: HashMap<String, ScoreboardObjective>,
    display_slots: [Option<String>; ScoreboardDisplaySlot::TeamWhite as usize + 1],
    scores: HashMap<String, HashMap<String, ScoreboardScore>>,
}

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
        self.score_mut(player, objective)?.value = value;
        Ok(())
    }

    fn add(&mut self, player: &str, objective: &str, value: i32) -> Result<i32, ScoreboardError> {
        self.require_writable(objective)?;
        let score = self.score_mut(player, objective)?;
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

    #[cfg(test)]
    fn update_health(&mut self, player: &str, health: i32) {
        for (name, objective) in &self.objectives {
            if objective.criteria == ScoreboardCriteria::Health {
                let scores = self.scores.entry(name.clone()).or_default();
                if player.len() > SCOREBOARD_OWNER_NAME_MAX
                    || (scores.len() >= SCOREBOARD_ENTRY_LIMIT
                        && !scores.contains_key(player))
                {
                    continue;
                }
                scores.entry(player.to_owned()).or_default().value = health;
            }
        }
    }

    fn enable_trigger(&mut self, player: &str, objective: &str) -> Result<(), ScoreboardError> {
        self.require_trigger(objective)?;
        self.score_mut(player, objective)?.trigger_enabled = true;
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
        let source_score = self.score_mut(source.0, source.1)?;
        if operation == ScoreboardOperation::Swap {
            source_score.value = lhs;
        }
        self.score_mut(target.0, target.1)?.value = value;
        Ok(value)
    }

    /// The one score-entry insertion point: every write path — `set`,
    /// `add`, `enable_trigger`, `operation` — enforces the owner-name and
    /// per-objective entry bounds here so a flood of unique holders or an
    /// oversized name cannot grow `scores` past the explicit caps.
    fn score_mut(
        &mut self,
        player: &str,
        objective: &str,
    ) -> Result<&mut ScoreboardScore, ScoreboardError> {
        if player.len() > SCOREBOARD_OWNER_NAME_MAX {
            return Err(ScoreboardError::NameTooLong);
        }
        let scores = self.scores.entry(objective.to_owned()).or_default();
        if scores.len() >= SCOREBOARD_ENTRY_LIMIT && !scores.contains_key(player) {
            return Err(ScoreboardError::TooManyScores);
        }
        Ok(scores.entry(player.to_owned()).or_default())
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

fn scoreboard_command_error(error: ScoreboardError) -> String {
    match error {
        ScoreboardError::DuplicateObjective => "Objective already exists",
        ScoreboardError::UnknownObjective => "Unknown objective",
        ScoreboardError::ReadOnlyObjective => "Objective is read-only",
        ScoreboardError::NotTrigger => "Objective is not a trigger",
        ScoreboardError::TriggerLocked => "Trigger is locked",
        ScoreboardError::DivisionByZero => "Cannot divide by zero",
        ScoreboardError::TooManyScores => "Too many scores for objective",
        ScoreboardError::NameTooLong => "Score holder name is too long",
    }
    .into()
}

#[cfg(test)]
mod scoreboard_cap_tests {
    use super::*;

    fn objective(name: &str, criteria: ScoreboardCriteria) -> ScoreboardObjective {
        ScoreboardObjective {
            name: name.to_owned(),
            criteria,
            display_name: name.to_owned(),
            render_type: ScoreboardRenderType::Integer,
        }
    }

    fn board_with_objective() -> ScoreboardEngine {
        let mut board = ScoreboardEngine::default();
        board
            .add_objective(objective("obj", ScoreboardCriteria::Dummy))
            .unwrap();
        board
    }

    #[test]
    fn owner_name_over_limit_is_rejected_on_every_write_path() {
        let mut board = board_with_objective();
        let long = "o".repeat(SCOREBOARD_OWNER_NAME_MAX + 1);
        assert_eq!(
            board.set(&long, "obj", 1),
            Err(ScoreboardError::NameTooLong)
        );
        assert_eq!(
            board.add(&long, "obj", 1),
            Err(ScoreboardError::NameTooLong)
        );
        board.update_health(&long, 20);
        assert!(
            !board.scores.contains_key("obj")
                || board.scores["obj"].is_empty(),
            "no score entry may be created for an oversized owner name"
        );
        let at_limit = "o".repeat(SCOREBOARD_OWNER_NAME_MAX);
        assert!(board.set(&at_limit, "obj", 1).is_ok());
    }

    #[test]
    fn objective_entry_count_is_capped() {
        let mut board = board_with_objective();
        for i in 0..SCOREBOARD_ENTRY_LIMIT {
            board.set(&format!("p{i}"), "obj", i as i32).unwrap();
        }
        assert_eq!(board.scores["obj"].len(), SCOREBOARD_ENTRY_LIMIT);
        assert_eq!(
            board.set("overflow", "obj", 1),
            Err(ScoreboardError::TooManyScores)
        );
        // Existing owners still update at the cap.
        assert!(board.set("p0", "obj", 7).is_ok());
        // The health-criteria fan-out must not grow the map either.
        board
            .add_objective(objective("life", ScoreboardCriteria::Health))
            .unwrap();
        let life = board.scores.entry("life".to_owned()).or_default();
        for i in 0..SCOREBOARD_ENTRY_LIMIT {
            life.entry(format!("h{i}")).or_default().value = 20;
        }
        board.update_health("health_overflow", 20);
        assert_eq!(board.scores["life"].len(), SCOREBOARD_ENTRY_LIMIT);
        board.update_health("h0", 5);
        assert_eq!(board.scores["life"]["h0"].value, 5);
    }
}

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
#[cfg(test)]
enum ScoreboardNumberFormat<'a> {
    Blank,
    Styled(&'a [u8]), // Anonymous NBT for a Style.
    Fixed(&'a [u8]),  // Anonymous NBT for a text component.
}

/// Production builds never parse a scoreboard number format, so the variant
/// payloads exist only for the test suite; `Infallible` keeps `Option<..>`
/// signatures total while staying uninhabited.
#[cfg(not(test))]
type ScoreboardNumberFormat<'a> = std::convert::Infallible;

fn write_number_format(out: &mut Vec<u8>, format: Option<ScoreboardNumberFormat<'_>>) {
    out.push(u8::from(format.is_some()));
    // Number format type IDs are from the vanilla 1.21.1 registry report.
    match format {
        None => {}
        #[cfg(test)]
        Some(ScoreboardNumberFormat::Blank) => write_varint(out, 0),
        #[cfg(test)]
        Some(ScoreboardNumberFormat::Styled(style)) => {
            write_varint(out, 1);
            out.extend_from_slice(style);
        }
        #[cfg(test)]
        Some(ScoreboardNumberFormat::Fixed(component)) => {
            write_varint(out, 2);
            out.extend_from_slice(component);
        }
    }
}

enum ScoreboardObjectivePacket<'a> {
    Create(&'a ScoreboardObjective, Option<ScoreboardNumberFormat<'a>>),
    Remove(&'a str),
    Update(&'a ScoreboardObjective, Option<ScoreboardNumberFormat<'a>>),
}

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

fn encode_scoreboard_display(
    slot: ScoreboardDisplaySlot,
    objective: Option<&str>,
) -> WorldMechanismsPacket {
    (
        0x57,
        display_pkt(slot as i64, objective.unwrap_or_default()),
    )
}

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

fn encode_scoreboard_reset(owner: &str, objective: Option<&str>) -> WorldMechanismsPacket {
    let mut out = Vec::new();
    write_str(&mut out, owner);
    out.push(u8::from(objective.is_some()));
    if let Some(name) = objective {
        write_str(&mut out, name);
    }
    (0x44, out)
}

#[cfg(test)]
const MODIFICATION_BLOCK_LIMIT: usize = 32_768;

const COMMAND_AIR_STATE: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillMode {
    Replace { filter: Option<u32> },
    Outline,
    Hollow,
    Keep,
    Destroy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneFilter {
    Replace,
    Masked,
    Filtered(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneMode {
    Normal,
    Force,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedFillCommand {
    from: BlockPos,
    to: BlockPos,
    state: u32,
    mode: FillMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedCloneCommand {
    from: BlockPos,
    to: BlockPos,
    destination: BlockPos,
    filter: CloneFilter,
    mode: CloneMode,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockModificationError {
    VolumeLimit { volume: u128, limit: usize },
    DestinationOverflow,
    OverlappingClone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockModification {
    pos: BlockPos,
    state: u32,
    destroyed: Option<u32>,
}

#[derive(Clone, Copy)]
struct ModificationBox {
    min: BlockPos,
    max: BlockPos,
}

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
    /// `#minecraft:crops` members and non-vanilla `age` blocks the name table
    /// does not know (Farmer's Delight, Biomes O' Plenty, `.mmod` crops).
    /// Growth follows the generic `CropBlock` rule — light and the farmland
    /// speed bound, no species cap — and the caller clamps the write to the
    /// state's real `age` domain.
    Modded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg(test)]
fn crop_growth_speed(
    farmland_3x3: [[Option<u8>; 3]; 3],
    same_crop_north_south: bool,
    same_crop_east_west: bool,
) -> f32 {
    crop_growth_speed_with_diagonal(farmland_3x3, same_crop_north_south, same_crop_east_west, false)
}

/// `CropBlock.getGrowthSpeed` (`dht.a`): the 3x3 farmland moisture sum plus the
/// two neighbour penalties. Vanilla halves the speed when the same crop borders
/// on both axes, and otherwise halves it once when any of the four diagonals
/// carries the same crop (`flag2`); the diagonal samples are the only input the
/// tick caller has to supply, so the name-only entry point above assumes none.
fn crop_growth_speed_with_diagonal(
    farmland_3x3: [[Option<u8>; 3]; 3],
    same_crop_north_south: bool,
    same_crop_east_west: bool,
    diagonal_same_crop: bool,
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
    // Vanilla halves at most once: both axes, or else a single diagonal.
    if (same_crop_north_south && same_crop_east_west) || diagonal_same_crop {
        speed /= 2.0;
    }
    speed
}

fn eval_crop_random_tick(input: CropTickInput) -> Option<u8> {
    eval_crop_random_tick_with_diagonal(input, false)
}

fn eval_crop_random_tick_with_diagonal(
    input: CropTickInput,
    diagonal_same_crop: bool,
) -> Option<u8> {
    let max_age = match input.kind {
        CropKind::Beetroots | CropKind::NetherWart | CropKind::SweetBerryBush => 3,
        CropKind::Cocoa => 2,
        // The caller enforces the state's own `age` domain for modded crops.
        CropKind::Modded => u8::MAX,
        _ => 7,
    };
    if input.age >= max_age {
        return None;
    }
    let grows = match input.kind {
        CropKind::NetherWart => input.random_value.is_multiple_of(10),
        CropKind::Cocoa | CropKind::SweetBerryBush => input.random_value.is_multiple_of(5),
        // `CropBlock.randomTick` gates on light and the growth-speed roll only;
        // the farmland underneath is read through the speed sum, never as a
        // hard requirement.
        CropKind::Wheat
        | CropKind::Carrots
        | CropKind::Potatoes
        | CropKind::Beetroots
        | CropKind::MelonStem
        | CropKind::PumpkinStem
        | CropKind::Modded => {
            if input.light < 9 {
                return None;
            }
            let speed = crop_growth_speed_with_diagonal(
                input.farmland_3x3,
                input.same_crop_north_south,
                input.same_crop_east_west,
                diagonal_same_crop,
            );
            let bound = (25.0_f32 / speed) as u32 + 1;
            input.random_value.is_multiple_of(bound)
        }
    };
    grows.then_some(input.age + 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
struct RandomTickSection {
    chunk_x: i32,
    section_y: i32,
    chunk_z: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum RandomTickDispatch {
    Crop,
    Fire,
    Leaves,
    Snow,
    Ice,
    Cactus,
    Bamboo,
    Kelp,
    Vine,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
struct RandomTickSample {
    pos: BlockPos,
    dispatch: RandomTickDispatch,
}

/// Java's 48-bit LCG, locally seeded for reproducible section sampling.
struct WorldMechanismLcg(u64);

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

// The nine P5e block-entity families use one dimension-scoped live store.
// Events are returned to the caller so item/entity creation and status effects
// can be applied after the registry mutex has been released.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorldBlockEntityEvent {
    Cooked {
        pos: BlockPos,
        slot: usize,
        item: String,
    },
    Brewed {
        pos: BlockPos,
        ejected_glass_bottle: bool,
    },
    Spawn {
        pos: BlockPos,
        entity_id: String,
        location: [f64; 3],
    },
    BeaconEffect {
        pos: BlockPos,
        player: usize,
        effect: String,
        amplifier: u8,
        duration_ticks: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorldBlockEntityInteraction {
    Changed,
    ReturnedItem(String),
    CrafterPulse,
}

fn lock_world_block_entities(
) -> std::sync::MutexGuard<'static, HashMap<DimBlockPos, VanillaBlockEntity>> {
    world_block_entity_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn vanilla_id_path(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

fn pot_stack_max(item: &str) -> u8 {
    name_to_item(item).map_or(64, |id| item_stack_max(id, &Vec::new()).clamp(1, 64) as u8)
}

/// Insert/replace a live payload at the corresponding dimension and block.
#[cfg(test)]
pub(crate) fn register_world_block_entity(
    dim: DimId,
    pos: BlockPos,
    entity: VanillaBlockEntity,
) -> Option<VanillaBlockEntity> {
    lock_world_block_entities().insert((dim, pos.0, pos.1, pos.2), entity)
}

#[cfg(test)]
pub(crate) fn remove_world_block_entity(
    dim: DimId,
    pos: BlockPos,
) -> Option<VanillaBlockEntity> {
    lock_world_block_entities().remove(&(dim, pos.0, pos.1, pos.2))
}

/// Read the vanilla comparator output of a stored block entity. Families that
/// have no analog output report 0, while absent positions return None.
pub(crate) fn comparator_signal_at_be(dim: DimId, pos: BlockPos) -> Option<u8> {
    let store = lock_world_block_entities();
    let Some(be) = store.get(&(dim, pos.0, pos.1, pos.2)) else {
        // Analog-output blocks without a block-entity record read their level
        // or charge straight from the block state (vanilla
        // `getAnalogOutputSignal` on Composter/RespawnAnchor/Cauldron).
        drop(store);
        let reg = breg()?;
        let st = block_at_dim(dim, pos.0, pos.1, pos.2);
        return match reg.name_of(st) {
            "minecraft:composter" => reg
                .get_prop(st, "level")
                .and_then(|value| value.parse().ok())
                .map(composter_comparator),
            "minecraft:respawn_anchor" => reg
                .get_prop(st, "charges")
                .and_then(|value| value.parse().ok())
                .map(respawn_anchor_comparator),
            // `CommandBlock` reads its `successCount` (clamped 0..15) through
            // the live command-block records, like vanilla's
            // `getAnalogOutputSignal`.
            name if command_block_name(name) => {
                Some(command_block_comparator_signal(dim, pos))
            }
            _ => cauldron_state_of(reg, st).map(|state| state.comparator_signal()),
        };
    };
    Some(match be {
        VanillaBlockEntity::BrewingStand(stand) => {
            let slots: [Option<BeSlotItem>; 5] = std::array::from_fn(|index| match index {
                0..=2 => stand.bottles[index].map(|potion| {
                    let name = match potion.form {
                        PotionForm::Drinkable => "minecraft:potion",
                        PotionForm::Splash => "minecraft:splash_potion",
                        PotionForm::Lingering => "minecraft:lingering_potion",
                    };
                    (name.to_owned(), 1)
                }),
                3 => stand.ingredient.map(|(ingredient, count)| (format!("{ingredient:?}"), count)),
                _ => (stand.blaze_powder > 0)
                    .then(|| ("minecraft:blaze_powder".to_owned(), stand.blaze_powder)),
            });
            be_container_signal(&slots, |id| match id {
                "minecraft:potion" | "minecraft:splash_potion" | "minecraft:lingering_potion" => 1,
                _ => 64,
            })
        }
        VanillaBlockEntity::Crafter(be) => be.comparator_signal(),
        VanillaBlockEntity::Campfire(_) => 0,
        VanillaBlockEntity::Jukebox(be) => be.comparator_signal(),
        VanillaBlockEntity::MobSpawner(_) => 0,
        VanillaBlockEntity::Lectern(be) => be.comparator_signal(),
        VanillaBlockEntity::ChiseledBookshelf(be) => be.comparator_signal(),
        VanillaBlockEntity::DecoratedPot(be) => {
            be.comparator_signal(be.item_id.as_deref().map_or(64, pot_stack_max))
        }
        VanillaBlockEntity::Beacon(_) => 0,
    })
}

/// Interact with one of the non-ticking container families. For a lectern,
/// Some(book) places a book, None + Some(page) turns to that zero-based page,
/// and None + None removes the book. Bookshelves use slots 0..=5. A pot
/// inserts or extracts one item per call. A crafter with None + Some(slot)
/// toggles that slot, while Some("powered")/Some("unpowered") updates its
/// redstone state and reports the rising-edge pulse exactly once.
pub(crate) fn interact_world_block_entity(
    dim: DimId,
    pos: BlockPos,
    actor_item: Option<&str>,
    slot_hint: Option<usize>,
) -> Option<WorldBlockEntityInteraction> {
    let mut store = lock_world_block_entities();
    let be = store.get_mut(&(dim, pos.0, pos.1, pos.2))?;
    match be {
        VanillaBlockEntity::Lectern(lectern) => match actor_item {
            Some(book) if lectern.book_item.is_none() && matches!(
                vanilla_id_path(book),
                "writable_book" | "written_book"
            ) => {
                lectern.book_item = Some(book.to_string());
                lectern.page = 0;
                // Page count must come from the item's content when available.
                lectern.total_pages = lectern.total_pages.max(1);
                Some(WorldBlockEntityInteraction::Changed)
            }
            None if lectern.book_item.is_some() => {
                if let Some(page) = slot_hint {
                    lectern.set_page(i32::try_from(page).unwrap_or(i32::MAX))
                        .then_some(WorldBlockEntityInteraction::Changed)
                } else {
                    lectern.page = 0;
                    lectern.total_pages = 0;
                    lectern.book_item.take().map(WorldBlockEntityInteraction::ReturnedItem)
                }
            }
            _ => None,
        },
        VanillaBlockEntity::ChiseledBookshelf(shelf) => {
            let slot = slot_hint?;
            match actor_item {
                Some(book) if matches!(
                    vanilla_id_path(book),
                    "book" | "writable_book" | "written_book" | "enchanted_book"
                ) => shelf.insert_book(slot, book)
                    .then_some(WorldBlockEntityInteraction::Changed),
                None => shelf.remove_book(slot).map(WorldBlockEntityInteraction::ReturnedItem),
                _ => None,
            }
        }
        VanillaBlockEntity::DecoratedPot(pot) => match actor_item {
            Some(item) if !item.is_empty() => {
                let stack_max = pot_stack_max(item);
                if pot.item_id.as_deref().is_some_and(|stored| stored != item)
                    || pot.item_count >= stack_max
                {
                    None
                } else {
                    pot.item_id.get_or_insert_with(|| item.to_string());
                    pot.item_count += 1;
                    Some(WorldBlockEntityInteraction::Changed)
                }
            }
            None if pot.item_count > 0 => {
                pot.item_count -= 1;
                let returned = pot.item_id.clone()?;
                if pot.item_count == 0 {
                    pot.item_id = None;
                }
                Some(WorldBlockEntityInteraction::ReturnedItem(returned))
            }
            _ => None,
        },
        VanillaBlockEntity::Crafter(crafter) => match actor_item {
            None => {
                let slot = slot_hint?;
                crafter.set_slot_disabled(slot, !crafter.slot_disabled(slot))
                    .then_some(WorldBlockEntityInteraction::Changed)
            }
            Some("powered" | "minecraft:powered") => crafter
                .set_powered(true)
                .then_some(WorldBlockEntityInteraction::CrafterPulse),
            Some("unpowered" | "minecraft:unpowered") => {
                let was_powered = crafter.triggered;
                crafter.set_powered(false);
                was_powered.then_some(WorldBlockEntityInteraction::Changed)
            }
            _ => None,
        },
        _ => None,
    }
}

fn cached_be_tick_positions(
    store: &HashMap<DimBlockPos, VanillaBlockEntity>,
    dim: DimId,
    fingerprint: u128,
    counts: &BeDimCounts,
) -> Arc<Vec<BlockPos>> {
    let mut cache = be_tick_positions().lock().unwrap_or_else(|e| e.into_inner());
    match cache.get(&dim) {
        Some(entry) if entry.fingerprint == fingerprint => {
            counts.hits.fetch_add(1, Ordering::Relaxed);
            entry.positions.clone()
        }
        _ => {
            counts.rebuilds.fetch_add(1, Ordering::Relaxed);
            let mut positions: Vec<(i32, i32, i32)> = store
                .iter()
                .filter(|(&(d, _, _, _), entity)| d == dim && be_tick_kind(entity) != 0)
                .map(|(&(_, x, y, z), _)| (x, y, z))
                .collect();
            // HashMap iteration order is randomized. Sort positions for
            // deterministic spawner randomness and event order across
            // runs and insertion histories — only on rebuild now.
            positions.sort_unstable();
            let positions = Arc::new(positions);
            cache.insert(
                dim,
                BeTickPositions {
                    fingerprint,
                    positions: positions.clone(),
                },
            );
            positions
        }
    }
}

fn tick_campfire(
    dim: DimId,
    fire: &mut CampfireBlockEntity,
    lit: bool,
    pos: BlockPos,
    events: &mut Vec<WorldBlockEntityEvent>,
) {
    if !lit && fire.slots.iter().flatten().all(|(_, elapsed, _)| *elapsed == 0) {
        return;
    }
    for (slot, input) in fire.tick(lit) {
        let cooked = BlockEntityItem::new(&input, 1).and_then(|item| live_campfire_result(&item));
        if let Some(item) = cooked {
            events.push(WorldBlockEntityEvent::Cooked {
                pos,
                slot,
                item: item.id.to_string(),
            });
            continue;
        }
        // `CampfireBlockEntity.cookTick`: the finished stack is the recipe
        // result when a campfire recipe exists and the raw input otherwise, and
        // either way it is dropped as an item entity — it is never put back into
        // the cooking slot.
        if let Some(item) = name_to_item(&input) {
            drop_live_stack_dim(
                dim,
                (
                    f64::from(pos.0) + 0.5,
                    f64::from(pos.1) + 0.5,
                    f64::from(pos.2) + 0.5,
                ),
                (item, 1, Vec::new()),
            );
        }
    }
}

/// Advance every registered ticking entity for one world tick and return the
/// effects to be committed by the simulation owner after releasing the mutex.
/// `nearby_players` contains positions from `dim` in stable caller order.
pub(crate) fn tick_world_block_entities(
    dim: DimId,
    world_tick: u64,
    nearby_players: &[[f64; 3]],
    rand_seed: u64,
) -> Vec<WorldBlockEntityEvent> {
    let mut store = lock_world_block_entities();
    if store.is_empty() {
        return Vec::new();
    }
    let (fingerprint, needs_mobs) = be_dim_tick_state(&store, dim, nearby_players);
    let counts = be_counts_for(dim);
    let mut positions = cached_be_tick_positions(&store, dim, fingerprint, &counts);
    if positions.is_empty() {
        return Vec::new();
    }

    let mut scratch = BE_TICK_SCRATCH.with(|cell| cell.take());
    if needs_mobs {
        // Take the mob snapshot without holding the block-entity registry.
        drop(store);
        scratch.live_mobs.extend(
            mobs().lock().unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|mob| mob.dim == dim)
                .map(|mob| {
                    let pos = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
                    (mob.etype, [pos.0, pos.1, pos.2])
                }),
        );
        store = lock_world_block_entities();
        // Save restoration or a direct write can change the registry while unlocked.
        let current = be_dim_tick_state(&store, dim, &[]).0;
        if current != fingerprint {
            positions = cached_be_tick_positions(&store, dim, current, &counts);
        }
    }
    let reg = breg();
    let mut rng = WorldMechanismLcg::new(rand_seed ^ world_tick);
    let mut events = Vec::new();
    // Crafters whose last applied powered state the redstone pass may since
    // have changed are re-polled after the store lock is released.
    for &(x, y, z) in positions.iter() {
        let pos = (x, y, z);
        let Some(entity) = store.get_mut(&(dim, x, y, z)) else { continue };
        // Idle sleep: a machine whose tick is provably a no-op is skipped
        // before dispatch. The predicate reads live state every tick, so any
        // register/interact/item/redstone/load mutation wakes the machine on
        // the very next pass — no wake bookkeeping to invalidate.
        let idle = match &*entity {
            // `tick` on four empty slots returns nothing and mutates
            // nothing, lit or not; `place` wakes it.
            VanillaBlockEntity::Campfire(fire) => fire.slots.iter().all(Option::is_none),
            // `brewing_inert` is exact: `tick` would return `Idle` without
            // touching a field.
            VanillaBlockEntity::BrewingStand(stand) => brewing_inert(stand),
            VanillaBlockEntity::MobSpawner(_) => nearby_players.is_empty(),
            VanillaBlockEntity::Jukebox(jukebox) => {
                jukebox.record_item.is_none() || jukebox.ticks_since_song_started == u64::MAX
            }
            // The beacon only acts on its 80-tick pulse.
            VanillaBlockEntity::Beacon(_) => !world_tick.is_multiple_of(80),
            _ => false,
        };
        if idle {
            counts.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        counts.dispatched.fetch_add(1, Ordering::Relaxed);
        match entity {
            VanillaBlockEntity::Campfire(fire) => {
                let lit = reg.is_some_and(|reg| {
                    reg.get_prop(block_at_dim(dim, x, y, z), "lit").as_deref() == Some("true")
                });
                tick_campfire(dim, fire, lit, pos, &mut events);
            }
            VanillaBlockEntity::BrewingStand(stand) => {
                if let BrewingTick::Completed { ejected_glass_bottle } = stand.tick() {
                    events.push(WorldBlockEntityEvent::Brewed { pos, ejected_glass_bottle });
                }
            }
            VanillaBlockEntity::MobSpawner(spawner) => {
                if !spawner_has_player(spawner, pos, nearby_players) || !spawner.delay_tick() {
                    continue;
                }
                let center = [f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5];
                let range = f64::from(spawner.spawn_range);
                let kind = entity_spec_by_name(&spawner.entity_id)
                    .filter(|spec| spec.max_health > 0.0)
                    .map(|spec| spec.id);
                let existing = scratch.live_mobs.iter().filter(|(id, at)| {
                    Some(*id) == kind &&
                        (0..3).all(|i| (at[i] - center[i]).abs() <= range)
                }).count();
                let spawn_count = if kind.is_some() { spawner.spawn_count } else { 0 };
                for admitted in (existing..).take(usize::from(spawn_count)) {
                    if admitted >= usize::from(spawner.max_nearby_entities) {
                        break;
                    }
                    let offset = |rng: &mut WorldMechanismLcg| {
                        let a = f64::from(rng.next_bits(24)) / f64::from(1 << 24);
                        let b = f64::from(rng.next_bits(24)) / f64::from(1 << 24);
                        (a - b) * range
                    };
                    let location = [
                        center[0] + offset(&mut rng),
                        f64::from(y) + f64::from(rng.next_int(3)) - 1.0,
                        center[2] + offset(&mut rng),
                    ];
                    events.push(WorldBlockEntityEvent::Spawn {
                        pos,
                        entity_id: spawner.entity_id.clone(),
                        location,
                    });
                }
                let random = rng.next_bits(24) as f32 / (1 << 24) as f32;
                spawner.reset_delay(random);
            }
            VanillaBlockEntity::Jukebox(jukebox) => {
                jukebox.ticks_since_song_started += 1;
            }
            VanillaBlockEntity::Crafter(crafter) => {
                scratch.crafter_power.push((pos, crafter.triggered));
            }
            VanillaBlockEntity::Beacon(beacon) => {
                let Some(reg) = reg else { continue };
                // `BeaconBlockEntity.updateBase`: the pyramid depth is independent
                // of the beam, so a roofed beacon keeps its buff tier here. Only
                // the 80-tick effect application below is gated on the beam.
                beacon.levels = beacon_pyramid_levels(|dx, dy, dz| {
                    let state = block_at_dim(dim, x + dx, y + dy, z + dz);
                    matches!(reg.name_of(state),
                        "minecraft:iron_block" | "minecraft:gold_block" |
                        "minecraft:diamond_block" | "minecraft:emerald_block" |
                        "minecraft:netherite_block")
                });
                if beacon.levels == 0 || beacon.primary_effect.is_none() || nearby_players.is_empty() {
                    continue;
                }
                // `BeaconBlockEntity.tick` applies the powers only while
                // `!beamSections.isEmpty()`: the beam scan starts at the beacon
                // block itself (a white `BeaconBeamBlock`), climbs through cells
                // whose light block is below 15 or that are bedrock, and clears
                // the section list on the first other opaque block. A blocked
                // beam therefore suppresses the effect pass without touching
                // `levels`.
                let beam_unobstructed = (y + 1..dim.min_y() + dim.height()).all(|above| {
                    let state = block_at_dim(dim, x, above, z);
                    reg.opacity_of(state) < 15 || reg.name_of(state) == "minecraft:bedrock"
                });
                if !beam_unobstructed {
                    continue;
                }
                let (primary, secondary) = beacon.active_effects();
                let radius_sq = f64::from(beacon.effect_range()).powi(2);
                for (player, at) in nearby_players.iter().enumerate() {
                    let distance_sq = (at[0] - f64::from(x) - 0.5).powi(2)
                        + (at[1] - f64::from(y) - 0.5).powi(2)
                        + (at[2] - f64::from(z) - 0.5).powi(2);
                    if distance_sq > radius_sq { continue; }
                    if let Some(effect) = &primary {
                        events.push(WorldBlockEntityEvent::BeaconEffect {
                            pos, player, effect: effect.clone(),
                            amplifier: u8::from(secondary.as_ref() == Some(effect)),
                            duration_ticks: beacon.effect_duration_ticks(),
                        });
                    }
                    if let Some(effect) = &secondary {
                        if primary.as_ref() != Some(effect) {
                            events.push(WorldBlockEntityEvent::BeaconEffect {
                                pos, player, effect: effect.clone(), amplifier: 0,
                                duration_ticks: beacon.effect_duration_ticks(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    drop(store);
    // Crafter powered-state poll: the redstone pass computes the neighbor
    // signal lazily, so the stored `triggered` flag tracks the last applied
    // value. A changed signal goes through `interact_world_block_entity`,
    // which reports the rising-edge `CrafterPulse` exactly once — the vanilla
    // `neighborChanged` semantics for a crafter.
    if let (Some(reg), false) = (reg, scratch.crafter_power.is_empty()) {
        let now = live_block_entity_time();
        for (pos, was_powered) in scratch.crafter_power.drain(..) {
            let powered = live_crafter_powered(reg, (dim, pos.0, pos.1, pos.2), now);
            if powered == was_powered {
                continue;
            }
            let action = if powered { "powered" } else { "unpowered" };
            interact_world_block_entity(dim, pos, Some(action), None);
        }
    }
    scratch.live_mobs.clear();
    scratch.crafter_power.clear();
    bound_vec_scratch(&mut scratch.live_mobs);
    bound_vec_scratch(&mut scratch.crafter_power);
    BE_TICK_SCRATCH.with(|cell| *cell.borrow_mut() = scratch);
    events
}

#[cfg(test)]
mod w_be_integration_tests {
    use super::*;

    fn place(dim: DimId, pos: BlockPos, entity: VanillaBlockEntity) {
        assert!(register_world_block_entity(dim, pos, entity).is_none());
    }

    fn remove(dim: DimId, pos: BlockPos) -> VanillaBlockEntity {
        remove_world_block_entity(dim, pos).expect("block entity registered")
    }

    #[test]
    fn w_be_registry_dimensions_and_all_nine_comparator_families() {
        let _guard = world_guard();
        let pos = (23401, 220, 23401);
        place(DimId::Overworld, pos, VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
            record_item: Some("minecraft:music_disc_cat".into()),
            ..Default::default()
        }));
        place(DimId::Nether, pos, VanillaBlockEntity::MobSpawner(MobSpawnerBlockEntity::default()));
        assert_eq!(comparator_signal_at_be(DimId::Overworld, pos), Some(2));
        assert_eq!(comparator_signal_at_be(DimId::Nether, pos), Some(0));
        assert_eq!(comparator_signal_at_be(DimId::End, pos), None);
        assert!(matches!(remove(DimId::Overworld, pos), VanillaBlockEntity::Jukebox(_)));
        assert!(matches!(remove(DimId::Nether, pos), VanillaBlockEntity::MobSpawner(_)));

        let dim = DimId::End;
        let cases = [
            VanillaBlockEntity::BrewingStand(BrewingStand::default()),
            VanillaBlockEntity::Crafter(CrafterBlockEntity::default()),
            VanillaBlockEntity::Campfire(CampfireBlockEntity::default()),
            VanillaBlockEntity::Lectern(LecternBlockEntity::default()),
            VanillaBlockEntity::ChiseledBookshelf(ChiseledBookshelfBlockEntity::default()),
            VanillaBlockEntity::DecoratedPot(DecoratedPotBlockEntity::default()),
            VanillaBlockEntity::Beacon(BeaconBlockEntity::default()),
        ];
        for (i, entity) in cases.into_iter().enumerate() {
            let at = (pos.0 + i as i32 + 1, pos.1, pos.2);
            place(dim, at, entity);
            assert_eq!(comparator_signal_at_be(dim, at), Some(0));
            remove(dim, at);
            assert_eq!(comparator_signal_at_be(dim, at), None);
        }
    }

    #[test]
    fn w_be_campfire_finishes_recipes_and_drops_an_item_with_no_recipe() {
        let _guard = world_guard();
        let dim = DimId::Overworld;
        // `minecraft:diamond` has no campfire recipe, so its finished slot
        // drops the raw input instead of being put back.
        let no_recipe = ("minecraft:diamond".into(), 0, 1);
        let mut fire = CampfireBlockEntity {
            slots: [Some(("minecraft:beef".into(), 0, 1)), Some(no_recipe), None, None],
        };
        let pos = (30040, 200, 30040);
        let diamond = name_to_item("minecraft:diamond").expect("diamond item");
        let before = drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|drop| drop.item == diamond)
            .count();
        let mut events = Vec::new();
        tick_campfire(dim, &mut fire, true, pos, &mut events);
        assert_eq!(
            events,
            [WorldBlockEntityEvent::Cooked {
                pos,
                slot: 0,
                item: "minecraft:cooked_beef".into(),
            }]
        );
        // `CampfireBlockEntity.cookTick`: a finished slot is always emptied, and
        // with no recipe the raw input is the stack that gets dropped.
        assert_eq!(fire.slots, [None, None, None, None]);
        let dropped = drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|drop| drop.item == diamond)
            .count();
        assert_eq!(dropped, before + 1, "the raw input becomes an item entity");
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn w_be_campfire_cooks_four_slots_at_600_ticks_and_clears() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (23420, 220, 23420);
        let reg = breg().expect("block registry");
        let campfire = reg.state_of("minecraft:campfire", None);
        set_block_dim(dim, pos.0, pos.1, pos.2, reg.with_prop(campfire, "lit", "true"));
        let mut fire = CampfireBlockEntity::default();
        for item in ["beef", "chicken", "potato", "salmon"] {
            assert!(fire.place(item, 600).is_some());
        }
        place(dim, pos, VanillaBlockEntity::Campfire(fire));
        for tick in 0..599 {
            assert!(tick_world_block_entities(dim, tick, &[], 17).is_empty());
        }
        let events = tick_world_block_entities(dim, 599, &[], 17);
        assert_eq!(events, [
            WorldBlockEntityEvent::Cooked { pos, slot: 0, item: "minecraft:cooked_beef".into() },
            WorldBlockEntityEvent::Cooked { pos, slot: 1, item: "minecraft:cooked_chicken".into() },
            WorldBlockEntityEvent::Cooked { pos, slot: 2, item: "minecraft:baked_potato".into() },
            WorldBlockEntityEvent::Cooked { pos, slot: 3, item: "minecraft:cooked_salmon".into() },
        ]);
        let VanillaBlockEntity::Campfire(fire) = remove(dim, pos) else { panic!("campfire") };
        assert!(fire.slots.iter().all(Option::is_none));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn w_be_campfire_keeps_unsupported_saved_input() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (23425, 220, 23425);
        let reg = breg().expect("block registry");
        let campfire = reg.state_of("minecraft:campfire", None);
        set_block_dim(dim, pos.0, pos.1, pos.2, reg.with_prop(campfire, "lit", "true"));
        let mut fire = CampfireBlockEntity::default();
        fire.place("minecraft:diamond", 1).expect("free slot");
        place(dim, pos, VanillaBlockEntity::Campfire(fire));
        assert!(tick_world_block_entities(dim, 0, &[], 17).is_empty());
        let VanillaBlockEntity::Campfire(fire) = remove(dim, pos) else { panic!("campfire") };
        assert_eq!(fire.slots[0], Some(("minecraft:diamond".into(), 0, 1)));
    }

    #[test]
    fn w_be_brewing_consumes_blaze_fuel_and_finishes_400_ticks() {
        let _guard = world_guard();
        let dim = DimId::Nether;
        let pos = (23430, 220, 23430);
        let stand = BrewingStand {
            bottles: [Some(BrewingPotion { kind: PotionKind::Water, form: PotionForm::Drinkable }); 3],
            ingredient: Some((BrewingIngredient::NetherWart, 2)),
            blaze_powder: 1,
            ..Default::default()
        };
        place(dim, pos, VanillaBlockEntity::BrewingStand(stand));
        assert!(tick_world_block_entities(dim, 0, &[], 123).is_empty());
        assert!(comparator_signal_at_be(dim, pos).is_some_and(|s| s > 0));
        for tick in 1..400 {
            assert!(tick_world_block_entities(dim, tick, &[], 123).is_empty());
        }
        assert_eq!(tick_world_block_entities(dim, 400, &[], 123), [
            WorldBlockEntityEvent::Brewed { pos, ejected_glass_bottle: false }
        ]);
        let VanillaBlockEntity::BrewingStand(stand) = remove(dim, pos) else { panic!("stand") };
        assert_eq!(stand.brew_time, 0);
        assert_eq!(stand.fuel, 19);
        assert_eq!(stand.blaze_powder, 0);
        assert_eq!(stand.ingredient, Some((BrewingIngredient::NetherWart, 1)));
        assert!(stand.bottles.iter().all(|p| p.is_some_and(|p| p.kind == PotionKind::Awkward)));
    }

    #[test]
    fn w_be_spawner_respects_player_radius_delay_cap_and_determinism() {
        let _guard = world_guard();
        let dim = DimId::End;
        let pos = (23440, 220, 23440);
        let spawner = MobSpawnerBlockEntity {
            entity_id: "minecraft:zombie".into(),
            delay: 2,
            min_spawn_delay: 5,
            max_spawn_delay: 10,
            spawn_count: 4,
            max_nearby_entities: 2,
            ..Default::default()
        };
        let far = [[f64::from(pos.0) + 17.0, 220.5, f64::from(pos.2) + 0.5]];
        let near = [[f64::from(pos.0) + 0.5, 220.5, f64::from(pos.2) + 0.5]];
        place(dim, pos, VanillaBlockEntity::MobSpawner(spawner.clone()));
        assert!(tick_world_block_entities(dim, 0, &far, 33).is_empty());
        assert!(tick_world_block_entities(dim, 1, &near, 33).is_empty());
        assert!(tick_world_block_entities(dim, 2, &near, 33).is_empty());
        let events = tick_world_block_entities(dim, 3, &near, 33);
        assert_eq!(events.len(), 2);
        for event in &events {
            let WorldBlockEntityEvent::Spawn { pos: origin, entity_id, location } = event else { panic!("spawn") };
            assert_eq!(*origin, pos);
            assert_eq!(entity_id, "minecraft:zombie");
            assert!((location[0] - f64::from(pos.0) - 0.5).abs() <= 4.0);
            assert!((location[2] - f64::from(pos.2) - 0.5).abs() <= 4.0);
        }
        let VanillaBlockEntity::MobSpawner(spawner_after) = remove(dim, pos) else { panic!("spawner") };
        assert!((5..=10).contains(&spawner_after.delay));
        place(dim, pos, VanillaBlockEntity::MobSpawner(MobSpawnerBlockEntity { delay: 0, ..spawner }));
        assert_eq!(tick_world_block_entities(dim, 3, &near, 33), events);
        remove(dim, pos);
    }

    #[test]
    fn w_be_spawner_unknown_entity_does_not_emit_invalid_spawn() {
        let _guard = world_guard();
        let dim = DimId::End;
        let pos = (23445, 220, 23445);
        place(dim, pos, VanillaBlockEntity::MobSpawner(MobSpawnerBlockEntity {
            entity_id: "minecraft:not_a_real_mob".into(),
            delay: 0,
            min_spawn_delay: 7,
            max_spawn_delay: 8,
            ..Default::default()
        }));
        let nearby = [[23445.5, 220.5, 23445.5]];
        assert!(tick_world_block_entities(dim, 0, &nearby, 0).is_empty());
        let VanillaBlockEntity::MobSpawner(spawner) = remove(dim, pos) else { panic!("spawner") };
        assert_eq!(spawner.delay, 7);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn w_be_beacon_pyramid_effect_every_80_ticks_and_radius() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (23450, 220, 23450);
        let reg = breg().expect("block registry");
        let iron = reg.state_of("minecraft:iron_block", None);
        for x in -1..=1 {
            for z in -1..=1 {
                set_block_dim(dim, pos.0 + x, pos.1 - 1, pos.2 + z, iron);
            }
        }
        place(dim, pos, VanillaBlockEntity::Beacon(BeaconBlockEntity {
            primary_effect: Some("minecraft:haste".into()),
            ..Default::default()
        }));
        let players = [
            [f64::from(pos.0) + 0.5, 220.5, f64::from(pos.2) + 0.5],
            [f64::from(pos.0) + 30.5, 220.5, f64::from(pos.2) + 0.5],
        ];
        let events = tick_world_block_entities(dim, 80, &players, 123);
        assert_eq!(events, [WorldBlockEntityEvent::BeaconEffect {
            pos, player: 0, effect: "minecraft:haste".into(), amplifier: 0, duration_ticks: 220,
        }]);
        assert!(tick_world_block_entities(dim, 81, &players, 123).is_empty());
        let VanillaBlockEntity::Beacon(beacon) = remove(dim, pos) else { panic!("beacon") };
        assert_eq!(beacon.levels, 1);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn w_be_beacon_keeps_pyramid_levels_when_the_beam_is_roofed() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (23455, 220, 23455);
        let reg = breg().expect("block registry");
        let iron = reg.state_of("minecraft:iron_block", None);
        for x in -1..=1 {
            for z in -1..=1 {
                set_block_dim(dim, pos.0 + x, pos.1 - 1, pos.2 + z, iron);
            }
        }
        // A roof over the beacon stops the beam column.
        let stone = reg.state_of("minecraft:stone", None);
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, stone);
        place(dim, pos, VanillaBlockEntity::Beacon(BeaconBlockEntity {
            primary_effect: Some("minecraft:haste".into()),
            ..Default::default()
        }));
        let players = [[f64::from(pos.0) + 0.5, 220.5, f64::from(pos.2) + 0.5]];
        assert!(tick_world_block_entities(dim, 80, &players, 123).is_empty());
        // `BeaconBlockEntity.updateBase` is a pyramid measurement, so the buff
        // tier survives the roof; only the effect pass is gated on the beam.
        let VanillaBlockEntity::Beacon(roofed) = remove(dim, pos) else { panic!("beacon") };
        assert_eq!(roofed.levels, 1);
        // Unroofing restores the powers without the level ever dropping to 0.
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, 0);
        place(dim, pos, VanillaBlockEntity::Beacon(roofed));
        let events = tick_world_block_entities(dim, 160, &players, 123);
        assert_eq!(
            events,
            [WorldBlockEntityEvent::BeaconEffect {
                pos,
                player: 0,
                effect: "minecraft:haste".into(),
                amplifier: 0,
                duration_ticks: 220,
            }]
        );
        let VanillaBlockEntity::Beacon(unroofed) = remove(dim, pos) else { panic!("beacon") };
        assert_eq!(unroofed.levels, 1);
    }

    #[test]
    fn w_be_jukebox_ticks_and_comparator_output() {
        let _guard = world_guard();
        let dim = DimId::End;
        let pos = (23460, 220, 23460);
        place(dim, pos, VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
            record_item: Some("minecraft:music_disc_5".into()),
            ..Default::default()
        }));
        for tick in 1..=8 {
            assert!(tick_world_block_entities(dim, tick, &[], 0).is_empty());
        }
        assert_eq!(comparator_signal_at_be(dim, pos), Some(15));
        let VanillaBlockEntity::Jukebox(jukebox) = remove(dim, pos) else { panic!("jukebox") };
        assert_eq!(jukebox.ticks_since_song_started, 8);
    }

    #[test]
    fn w_be_lectern_pages_and_book_insertion_removal() {
        let _guard = world_guard();
        let dim = DimId::End;
        let pos = (23470, 220, 23470);
        place(dim, pos, VanillaBlockEntity::Lectern(LecternBlockEntity {
            book_item: Some("minecraft:written_book".into()),
            page: 0, total_pages: 5,
        }));
        assert_eq!(interact_world_block_entity(dim, pos, None, Some(3)), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(11));
        assert_eq!(interact_world_block_entity(dim, pos, None, None), Some(WorldBlockEntityInteraction::ReturnedItem("minecraft:written_book".into())));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(0));
        assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:writable_book"), None), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(15));
        remove(dim, pos);
    }

    #[test]
    fn w_be_bookshelf_slot_bounds_and_last_interacted_comparator() {
        let _guard = world_guard();
        let dim = DimId::End;
        let pos = (23480, 220, 23480);
        place(dim, pos, VanillaBlockEntity::ChiseledBookshelf(ChiseledBookshelfBlockEntity::default()));
        assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:book"), Some(6)), None);
        assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:enchanted_book"), Some(5)), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(6));
        assert_eq!(interact_world_block_entity(dim, pos, None, Some(5)), Some(WorldBlockEntityInteraction::ReturnedItem("enchanted_book".into())));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(6));
        assert_eq!(interact_world_block_entity(dim, pos, None, Some(5)), None);
        remove(dim, pos);
    }

    #[test]
    fn w_be_pot_insert_extract_respects_stack_capacity() {
        let _guard = world_guard();
        let dim = DimId::Nether;
        let pos = (23490, 220, 23490);
        place(dim, pos, VanillaBlockEntity::DecoratedPot(DecoratedPotBlockEntity::default()));
        assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:stone"), None), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(1));
        assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:dirt"), None), None);
        for _ in 1..64 {
            assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:stone"), None), Some(WorldBlockEntityInteraction::Changed));
        }
        assert_eq!(comparator_signal_at_be(dim, pos), Some(15));
        assert_eq!(interact_world_block_entity(dim, pos, Some("minecraft:stone"), None), None);
        assert_eq!(interact_world_block_entity(dim, pos, None, None), Some(WorldBlockEntityInteraction::ReturnedItem("minecraft:stone".into())));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(14));
        remove(dim, pos);
    }

    #[test]
    fn w_be_crafter_slot_toggle_and_rising_edge_pulse() {
        let _guard = world_guard();
        let dim = DimId::End;
        let pos = (23500, 220, 23500);
        place(dim, pos, VanillaBlockEntity::Crafter(CrafterBlockEntity::default()));
        assert_eq!(interact_world_block_entity(dim, pos, None, Some(9)), None);
        assert_eq!(interact_world_block_entity(dim, pos, None, Some(0)), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(1));
        assert_eq!(interact_world_block_entity(dim, pos, None, Some(0)), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(comparator_signal_at_be(dim, pos), Some(0));
        assert_eq!(interact_world_block_entity(dim, pos, Some("powered"), None), Some(WorldBlockEntityInteraction::CrafterPulse));
        assert_eq!(interact_world_block_entity(dim, pos, Some("powered"), None), None);
        assert_eq!(interact_world_block_entity(dim, pos, Some("unpowered"), None), Some(WorldBlockEntityInteraction::Changed));
        assert_eq!(interact_world_block_entity(dim, pos, Some("powered"), None), Some(WorldBlockEntityInteraction::CrafterPulse));
        remove(dim, pos);
    }

    /// The sorted-position cache must reuse its list while the dimension's
    /// ticking `(pos, kind)` set is unchanged and rebuild exactly once per
    /// insert/remove — including mutations that bypass the register/remove
    /// wrappers, like `save_v2`'s direct registry rewrite.
    #[test]
    fn w_be_position_cache_hits_until_registry_changes() {
        // A raw dimension slot no other fixture uses, so the fingerprint
        // sees only this test's entities; the guard keeps a concurrent
        // save/load from wiping the registry mid-assertion.
        let _guard = world_guard();
        let dim = DimId::from_raw(0xBE01);
        let pos = (61_000, 80, 61_000);
        let counts = || be_counts_for(dim);
        let rebuild_hits = |c: &BeDimCounts| {
            (
                c.rebuilds.load(Ordering::Relaxed),
                c.hits.load(Ordering::Relaxed),
            )
        };

        // A dimension with only a lectern has an empty ticking set.
        place(dim, pos, VanillaBlockEntity::Lectern(LecternBlockEntity::default()));
        tick_world_block_entities(dim, 0, &[], 7);
        let (r0, h0) = rebuild_hits(&counts());
        tick_world_block_entities(dim, 1, &[], 7);
        let (r1, h1) = rebuild_hits(&counts());
        assert_eq!(r1, r0, "unchanged registry must not re-sort");
        assert_eq!(h1, h0 + 1, "unchanged registry must hit the cache");

        // A non-ticking entity does not change the ticking set: still a hit.
        place(dim, (pos.0 - 1, pos.1, pos.2), VanillaBlockEntity::Lectern(LecternBlockEntity::default()));
        tick_world_block_entities(dim, 2, &[], 7);
        assert_eq!(rebuild_hits(&counts()).0, r0, "a lectern must not invalidate the tick list");

        // A ticking insert and a direct-registry remove each invalidate once.
        place(dim, (pos.0 + 1, pos.1, pos.2), VanillaBlockEntity::Jukebox(JukeboxBlockEntity::default()));
        tick_world_block_entities(dim, 3, &[], 7);
        assert_eq!(rebuild_hits(&counts()).0, r0 + 1, "insert must invalidate the cache");
        remove(dim, (pos.0 + 1, pos.1, pos.2));
        tick_world_block_entities(dim, 4, &[], 7);
        assert_eq!(rebuild_hits(&counts()).0, r0 + 2, "remove must invalidate the cache");

        // A kind swap at an existing key changes the fingerprint too.
        world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (dim, pos.0, pos.1, pos.2),
                VanillaBlockEntity::Jukebox(JukeboxBlockEntity::default()),
            );
        tick_world_block_entities(dim, 5, &[], 7);
        assert_eq!(rebuild_hits(&counts()).0, r0 + 3, "kind swap must invalidate the cache");
        remove(dim, pos);
        remove(dim, (pos.0 - 1, pos.1, pos.2));
        tick_world_block_entities(dim, 6, &[], 7);
    }

    /// 500 idle machines plus two live ones: the idle half must skip
    /// dispatch entirely while the brewing stand finishes its 400-tick brew
    /// and the jukebox counts every pass — vanilla progression, zero idle
    /// work.
    #[test]
    fn w_be_idle_machines_sleep_while_active_ones_tick() {
        // The guard keeps a concurrent save/load or world-tick stage from
        // touching the shared registry while the 402-tick fixture runs.
        let _guard = world_guard();
        let dim = DimId::from_raw(0xBE02);
        let base = (62_000, 80, 62_000);
        for i in 0..250 {
            place(
                dim,
                (base.0 + i, base.1, base.2),
                VanillaBlockEntity::Campfire(CampfireBlockEntity::default()),
            );
            place(
                dim,
                (base.0, base.1, base.2 + 1_000 + i),
                VanillaBlockEntity::Jukebox(JukeboxBlockEntity::default()),
            );
        }
        let brew_pos = (base.0, base.1, base.2 + 2_000);
        place(
            dim,
            brew_pos,
            VanillaBlockEntity::BrewingStand(BrewingStand {
                bottles: [Some(BrewingPotion {
                    kind: PotionKind::Water,
                    form: PotionForm::Drinkable,
                }); 3],
                ingredient: Some((BrewingIngredient::NetherWart, 2)),
                blaze_powder: 1,
                ..Default::default()
            }),
        );
        let music_pos = (base.0 + 1, base.1, base.2 + 2_000);
        place(
            dim,
            music_pos,
            VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                record_item: Some("minecraft:music_disc_cat".into()),
                ..Default::default()
            }),
        );

        let counts = be_counts_for(dim);
        let dispatched = || counts.dispatched.load(Ordering::Relaxed);
        let skipped = || counts.skipped.load(Ordering::Relaxed);
        let (d0, s0) = (dispatched(), skipped());
        assert!(tick_world_block_entities(dim, 0, &[], 42).is_empty());
        assert_eq!(
            dispatched() - d0,
            2,
            "only the brewing stand and the record jukebox may dispatch"
        );
        assert_eq!(skipped() - s0, 500, "every idle machine must skip");

        // Vanilla progression: 399 quiet ticks, then the brew completes on
        // tick 400 exactly like the single-stand fixture.
        for tick in 1..400 {
            assert!(tick_world_block_entities(dim, tick, &[], 42).is_empty());
        }
        assert_eq!(tick_world_block_entities(dim, 400, &[], 42), [
            WorldBlockEntityEvent::Brewed { pos: brew_pos, ejected_glass_bottle: false }
        ]);

        // Once the brew finishes the stand goes inert and sleeps too: the
        // next pass dispatches only the still-playing jukebox.
        let (d1, s1) = (dispatched(), skipped());
        assert!(tick_world_block_entities(dim, 401, &[], 42).is_empty());
        assert_eq!(dispatched() - d1, 1);
        assert_eq!(skipped() - s1, 501);

        // The jukebox counted every dispatched pass: ticks 0..=401 = 402.
        let VanillaBlockEntity::Jukebox(jukebox) = remove(dim, music_pos) else {
            panic!("jukebox")
        };
        assert_eq!(jukebox.ticks_since_song_started, 402);
        // The finished stand is inert with its brew state intact.
        let VanillaBlockEntity::BrewingStand(stand) = remove(dim, brew_pos) else {
            panic!("stand")
        };
        assert_eq!(stand.brew_time, 0);
        assert_eq!(stand.fuel, 19);
        for i in 0..250 {
            remove(dim, (base.0 + i, base.1, base.2));
            remove(dim, (base.0, base.1, base.2 + 1_000 + i));
        }
    }
}

#[cfg(test)]
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
            } else if name == "minecraft:cactus" {
                RandomTickDispatch::Cactus
            } else if matches!(name.as_str(), "minecraft:bamboo" | "minecraft:bamboo_sapling") {
                RandomTickDispatch::Bamboo
            } else if name == "minecraft:kelp" {
                RandomTickDispatch::Kelp
            } else if name == "minecraft:vine" {
                RandomTickDispatch::Vine
            } else {
                RandomTickDispatch::Other
            };
            samples.push(RandomTickSample { pos, dispatch });
        }
    }
    samples
}

#[cfg(test)]
const DEFAULT_RANDOM_TICK_SPEED: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
struct FireTickResult {
    extinguished: bool,
    age: Option<u8>,
    spread: [bool; 6],
    burned_out: bool,
}

fn eval_fire_tick(input: FireTickInput) -> FireTickResult {
    let mut result = FireTickResult {
        extinguished: false,
        age: Some(input.age.min(15)),
        spread: [false; 6],
        burned_out: false,
    };
    let age = input.age.min(15);
    // `FireBlock.tick`: rain puts a fire out only on
    // `random.nextFloat() < 0.2F + age * 0.03F`, and infiniburn blocks (netherrack,
    // magma, soul soil — the `soul_fire` carrier this port models) are immune.
    // `burnout_roll` is the tick's float sample; the two consumers are exclusive
    // because an extinguished fire returns here.
    let rain_sample = f32::from(input.burnout_roll) / 256.0;
    if input.rain_exposed && !input.soul_fire && rain_sample < 0.2 + 0.03 * f32::from(age) {
        result.extinguished = true;
        result.age = None;
        return result;
    }
    if input.soul_fire {
        return result;
    }
    result.age = Some(if input.age_roll % 3 == 2 {
        age.saturating_add(1).min(15)
    } else {
        age
    });
    for index in 0..6 {
        result.spread[index] = input.flammable_neighbors[index] && input.spread_rolls[index] == 0;
    }
    // `FireBlock.tick`: `age == 15 && random.nextInt(4) == 0 && !canBurn(below)`.
    // Index 3 of the caller's fixed direction table is the cell below the fire.
    result.burned_out =
        age == 15 && input.burnout_roll.is_multiple_of(4) && !input.flammable_neighbors[3];
    if result.burned_out {
        result.age = None;
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeafDecayInput {
    persistent: bool,
    distance: u8,
    random_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeafDecayResult {
    decay: bool,
    drop_item: bool,
}

fn eval_leaf_decay(input: LeafDecayInput) -> LeafDecayResult {
    let decay = !input.persistent && input.distance >= 7;
    // `LeavesBlock.randomTick` calls `dropResources` unconditionally once the
    // distance read reaches 7; the rarity of the drops lives in the block's
    // loot table, not in a one-in-twenty gate here. The caller's roll stays part
    // of the input snapshot but no longer decides whether the loot is offered.
    let _ = input.random_roll;
    LeafDecayResult {
        decay,
        drop_item: decay,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SnowIceTickInput {
    cold_biome: bool,
    sky_exposed: bool,
    light: u8,
    support_is_full_solid: bool,
    water_source: bool,
    random_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnowIceTickResult {
    NoChange,
    AccumulateSnow,
    FreezeWater,
    MeltIce,
}

fn eval_snow_accumulation(input: SnowIceTickInput) -> SnowIceTickResult {
    if input.cold_biome
        && input.sky_exposed
        && !input.support_is_full_solid
        && input.light < 10
        && input.random_roll.is_multiple_of(16)
    {
        SnowIceTickResult::AccumulateSnow
    } else {
        SnowIceTickResult::NoChange
    }
}

fn eval_ice_freeze_melt(input: SnowIceTickInput, is_ice: bool) -> SnowIceTickResult {
    // `IceBlock.randomTick`: `getBrightness(BLOCK, pos) > 11 - lightBlock(ice)`;
    // ice is a skylight-transparent block, so its own light block is 0 and the
    // test is a strict `> 11` — a block light of exactly 11 does not melt.
    if is_ice && input.light > 11 {
        return SnowIceTickResult::MeltIce;
    }
    if !is_ice && input.water_source && input.cold_biome && input.sky_exposed {
        return SnowIceTickResult::FreezeWater;
    }
    SnowIceTickResult::NoChange
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SaplingTickInput {
    stage: u8,
    /// `Level.getMaxLocalRawBrightness(pos.above())`: full sky light at the cell
    /// above a sky-exposed sapling, or the local block light under cover.
    light: u8,
    /// The canopy envelope is clear: every cell the grown tree would occupy is
    /// air (or the sapling cell itself) before the grow is offered.
    headroom: bool,
    /// Raw caller-provided roll; vanilla draws `random.nextInt(7)` and grows on 0.
    random_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaplingTickResult {
    NoChange,
    AdvanceStage,
    Grow,
}

/// `SaplingBlock.randomTick` + `BonemealableFeaturePlaceSection`: a stage-0
/// sapling under at least light 9 advances to stage 1 on a one-in-seven roll;
/// a stage-1 sapling with headroom grows. Dark or crowded saplings never move.
fn eval_sapling_random_tick(input: SaplingTickInput) -> SaplingTickResult {
    if input.light < 9 || !input.random_roll.is_multiple_of(7) {
        return SaplingTickResult::NoChange;
    }
    if input.stage == 0 {
        return SaplingTickResult::AdvanceStage;
    }
    if !input.headroom {
        return SaplingTickResult::NoChange;
    }
    SaplingTickResult::Grow
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GrassSpreadInput {
    /// A full-occluding block sits on the spreader.
    covered_opaque: bool,
    /// The light level at the cell above the spreader.
    light_above: u8,
    /// The sampled spread target holds `minecraft:dirt`.
    target_is_dirt: bool,
    /// The cell above the sampled target is opaque or liquid and would seal the
    /// new grass off from the sky.
    target_above_blocks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrassSpreadResult {
    NoChange,
    DieToDirt,
    SpreadToNeighbor,
}

/// `SpreadingSnowyDirtBlock` (`GrassBlock`/`MyceliumBlock`) random tick.
///
/// `canBeGrass` is an opacity test, not a light test: anything that occludes the
/// cell above (or a full water source, which the opaque test already covers for
/// a solid cover) kills the grass back to dirt regardless of the light level,
/// while a one-layer snow cover is exempt. The light level only gates the spread
/// half, matching vanilla's `getMaxLocalRawBrightness(pos.above()) >= 9`.
fn eval_grass_spread(input: GrassSpreadInput) -> GrassSpreadResult {
    if input.covered_opaque {
        return GrassSpreadResult::DieToDirt;
    }
    if input.light_above >= 9 && input.target_is_dirt && !input.target_above_blocks {
        return GrassSpreadResult::SpreadToNeighbor;
    }
    GrassSpreadResult::NoChange
}

// ---------- P2: remaining vanilla gamerules + plant random ticks ----------
//
// Live gamerule reads go through `gamerule_live_value`: the atomic registry
// `/gamerule` writes first, then the stored command map for rules that keep
// no dedicated atomic — the same state the command reports and mutates.
fn gamerule_live_value(name: &str) -> i32 {
    gamerule_atomic_value(name)
        .or_else(|| command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).get(name).copied())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CactusTickInput {
    /// `CactusBlock.AGE` 0..=15; a tick landing on 15 grows the segment above.
    age: u8,
    /// Cactus cells in this column including the ticked one.
    column_height: u8,
    /// The growth cell above is air.
    space_above: bool,
    /// A solid block horizontally touches the growth cell: `canSurvive`
    /// rejects a cactus there, so the tick refuses to grow into it.
    solid_touching_growth: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CactusTickResult {
    NoChange,
    /// `age` advances toward the 15 that extends the column.
    AdvanceAge(u8),
    /// The cell above becomes a fresh cactus; this block's age resets to 0.
    GrowSegment,
}

/// `CactusBlock.randomTick`: age counts to 15, then a segment grows into the
/// air above while the column stays under the vanilla 3-block cap and no
/// solid block touches the new cell sideways.
fn eval_cactus_random_tick(input: CactusTickInput) -> CactusTickResult {
    if !input.space_above || input.column_height >= 3 || input.solid_touching_growth {
        return CactusTickResult::NoChange;
    }
    if input.age >= 15 {
        CactusTickResult::GrowSegment
    } else {
        CactusTickResult::AdvanceAge(input.age + 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BambooLeaves {
    None,
    Small,
    Large,
}

impl BambooLeaves {
    fn prop(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Small => "small",
            Self::Large => "large",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BambooTickInput {
    is_sapling: bool,
    /// `BambooStalkBlock.STAGE`: only a stage-0 stalk may grow.
    stage: u8,
    /// Stalk cells in the column including this one; the sapling counts as 1.
    column_height: u8,
    /// `Level.getRawBrightness(pos.above(), 0)`: growth needs at least 9.
    light_above: u8,
    /// The growth cell above is air (a sapling reuses its own cell instead).
    space_above: bool,
    /// Raw roll; vanilla draws `random.nextInt(3)` and grows on 0.
    random_roll: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BambooTickResult {
    NoChange,
    /// `BambooSaplingBlock` becomes a bamboo stalk with `leaves=small`.
    SaplingToStalk,
    /// A new stalk goes above with `leaves=large`; the old top relaxes to
    /// `leaves=small`.
    GrowStalk,
}

/// `BambooSaplingBlock`/`BambooStalkBlock.randomTick`: a one-in-three roll
/// with light 9+ above turns the sapling into a stalk or extends the column
/// up to the vanilla 16-block cap.
fn eval_bamboo_random_tick(input: BambooTickInput) -> BambooTickResult {
    if !input.random_roll.is_multiple_of(3) || input.light_above < 9 {
        return BambooTickResult::NoChange;
    }
    if input.is_sapling {
        return BambooTickResult::SaplingToStalk;
    }
    if input.stage != 0 || !input.space_above || input.column_height >= 16 {
        return BambooTickResult::NoChange;
    }
    BambooTickResult::GrowStalk
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KelpTickInput {
    /// `KelpBlock` head `AGE` 0..=25.
    age: u8,
    /// The cell above holds a water source — the only cell kelp grows into.
    above_is_water_source: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KelpTickResult {
    NoChange,
    /// The water above becomes a kelp head of `new_age`; the old head turns
    /// into the ageless `kelp_plant` body.
    GrowIntoWater { new_age: u8 },
}

/// `KelpPlantBlock`/`GrowingPlantHeadBlock.randomTick`: kelp stops aging at
/// 25 and only ever grows into a water source cell.
fn eval_kelp_random_tick(input: KelpTickInput) -> KelpTickResult {
    if input.age >= 25 || !input.above_is_water_source {
        return KelpTickResult::NoChange;
    }
    KelpTickResult::GrowIntoWater {
        new_age: input.age + 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VineTickInput {
    /// `doVinesSpread` gamerule; vanilla gates the entire spread pass on it.
    spread_allowed: bool,
    /// Raw roll; vanilla draws `random.nextInt(4)` and spreads on 0.
    random_roll: u8,
    /// Air below — vines creep downward without needing a face to attach to.
    space_below: bool,
    /// A horizontally adjacent air cell has a solid face a vine can claim.
    side_can_attach: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VineTickResult {
    NoChange,
    /// Extend the vine into the air below.
    SpreadDown,
    /// Claim one face of a horizontally adjacent air cell.
    SpreadSide,
}

/// `VineBlock.randomTick` under the `doVinesSpread` gate: on a one-in-four
/// roll the vine creeps down first, else onto a neighboring face.
fn eval_vine_random_tick(input: VineTickInput) -> VineTickResult {
    if !input.spread_allowed || !input.random_roll.is_multiple_of(4) {
        return VineTickResult::NoChange;
    }
    if input.space_below {
        VineTickResult::SpreadDown
    } else if input.side_can_attach {
        VineTickResult::SpreadSide
    } else {
        VineTickResult::NoChange
    }
}

/// The vine faces 1.21.1's `VineBlock` state carries.
const VINE_SIDES: &[(&str, (i32, i32))] = &[
    ("north", (0, -1)),
    ("east", (1, 0)),
    ("south", (0, 1)),
    ("west", (-1, 0)),
];

fn cactus_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    // Only the column top may grow: vanilla requires `pos.above()` to be air,
    // so a tick on a middle segment does nothing at all.
    let mut height = 1u8;
    while height < 3
        && reg.name_of(block_at_dim(dim, x, y - i32::from(height), z)) == "minecraft:cactus"
    {
        height += 1;
    }
    let age = reg
        .get_prop(st, "age")
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    let solid_touching = [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .iter()
        .any(|(dx, dz)| {
            reg.is_collision_shape_full_block(block_at_dim(dim, x + dx, y + 1, z + dz))
        });
    match eval_cactus_random_tick(CactusTickInput {
        age,
        column_height: height,
        space_above: reg.is_air(block_at_dim(dim, x, y + 1, z)),
        solid_touching_growth: solid_touching,
    }) {
        CactusTickResult::NoChange => {}
        CactusTickResult::AdvanceAge(next) => {
            set_block_dim(dim, x, y, z, reg.with_prop(st, "age", &next.to_string()));
        }
        CactusTickResult::GrowSegment => {
            let fresh = reg.state_of("minecraft:cactus", None);
            set_block_dim(dim, x, y + 1, z, fresh);
            note_block_update_dim(dim, x, y + 1, z, fresh);
            set_block_dim(dim, x, y, z, reg.with_prop(st, "age", "0"));
        }
    }
}

fn bamboo_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    let is_sapling = reg.name_of(st) == "minecraft:bamboo_sapling";
    let mut height = 1u8;
    if !is_sapling {
        while height < 16
            && reg.name_of(block_at_dim(dim, x, y - i32::from(height), z))
                == "minecraft:bamboo"
        {
            height += 1;
        }
    }
    let stage = reg
        .get_prop(st, "stage")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let light_above = if random_tick_sky_exposed(dim, x, y + 1, z) {
        15
    } else {
        random_tick_block_light(reg, dim, x, y + 1, z)
    };
    match eval_bamboo_random_tick(BambooTickInput {
        is_sapling,
        stage,
        column_height: height,
        light_above,
        space_above: reg.is_air(block_at_dim(dim, x, y + 1, z)),
        random_roll: world_rng::next_below(3) as u8,
    }) {
        BambooTickResult::NoChange => {}
        BambooTickResult::SaplingToStalk => {
            let stalk = reg.with_prop(
                reg.state_of("minecraft:bamboo", None),
                "leaves",
                BambooLeaves::Small.prop(),
            );
            set_and_note_world_block(dim, (x, y, z), stalk);
        }
        BambooTickResult::GrowStalk => {
            let top = reg.with_prop(
                reg.state_of("minecraft:bamboo", None),
                "leaves",
                BambooLeaves::Large.prop(),
            );
            set_block_dim(dim, x, y + 1, z, top);
            note_block_update_dim(dim, x, y + 1, z, top);
            set_block_dim(
                dim,
                x,
                y,
                z,
                reg.with_prop(st, "leaves", BambooLeaves::Small.prop()),
            );
            // Vanilla's leaf gradient: the new top is `large`, the stalk
            // under it `small`, and every deeper stalk `none`.
            let below = block_at_dim(dim, x, y - 1, z);
            if reg.name_of(below) == "minecraft:bamboo" {
                set_block_dim(
                    dim,
                    x,
                    y - 1,
                    z,
                    reg.with_prop(below, "leaves", BambooLeaves::None.prop()),
                );
            }
        }
    }
}

fn kelp_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    let age = reg
        .get_prop(st, "age")
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    let above_is_water_source = fluid_from_block(block_at_dim(dim, x, y + 1, z))
        .is_some_and(|f| f.kind == flowing_fluid::Kind::Water && f.source());
    match eval_kelp_random_tick(KelpTickInput {
        age,
        above_is_water_source,
    }) {
        KelpTickResult::NoChange => {}
        KelpTickResult::GrowIntoWater { new_age } => {
            let head = reg.with_prop(
                reg.state_of("minecraft:kelp", None),
                "age",
                &new_age.to_string(),
            );
            set_block_dim(dim, x, y + 1, z, head);
            note_block_update_dim(dim, x, y + 1, z, head);
            set_block_dim(dim, x, y, z, reg.state_of("minecraft:kelp_plant", None));
        }
    }
}

fn vine_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    let space_below = reg.is_air(block_at_dim(dim, x, y - 1, z));
    let side = VINE_SIDES.iter().find(|(_, (dx, dz))| {
        reg.is_air(block_at_dim(dim, x + dx, y, z + dz))
            && reg.is_collision_shape_full_block(block_at_dim(dim, x + dx * 2, y, z + dz * 2))
    });
    match eval_vine_random_tick(VineTickInput {
        spread_allowed: gamerule_live_value("doVinesSpread") != 0,
        random_roll: world_rng::next_below(4) as u8,
        space_below,
        side_can_attach: side.is_some(),
    }) {
        VineTickResult::NoChange => {}
        VineTickResult::SpreadDown => {
            set_block_dim(dim, x, y - 1, z, st);
            note_block_update_dim(dim, x, y - 1, z, st);
        }
        VineTickResult::SpreadSide => {
            let Some((face, (dx, dz))) = side else {
                return;
            };
            let mut grown = reg.state_of("minecraft:vine", None);
            // The claimed face is the side of the new cell touching the solid
            // neighbor behind it — the opposite face from the spread direction.
            let opposite = VINE_SIDES
                .iter()
                .find(|(_, d)| *d == (-dx, -dz))
                .map(|(name, _)| *name)
                .unwrap_or(face);
            grown = reg.with_prop(grown, opposite, "true");
            set_block_dim(dim, x + dx, y, z + dz, grown);
            note_block_update_dim(dim, x + dx, y, z + dz, grown);
        }
    }
}

/// The P2 random-tick arm: cactus, bamboo, kelp and vines dispatch here,
/// matching the name-based arms `on_random_tick_dim` already runs. Returns
/// `true` when the block name was claimed by one of the handlers.
fn p2_plant_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) -> bool {
    // The block-entity registry is populated lazily: a sampled cell whose
    // block family carries a [`VanillaBlockEntity`] materializes its default
    // record, and a registered position whose block left its family is
    // dropped. Mirrors vanilla `Level.setBlock` create/remove-block-entity,
    // performed lazily here because placement paths write bare states.
    wbe_reconcile_at(reg, dim, (x, y, z), st);
    match reg.name_of(st) {
        "minecraft:cactus" => cactus_random_tick(reg, dim, x, y, z, st),
        "minecraft:bamboo" | "minecraft:bamboo_sapling" => {
            bamboo_random_tick(reg, dim, x, y, z, st)
        }
        "minecraft:kelp" => kelp_random_tick(reg, dim, x, y, z, st),
        "minecraft:vine" => vine_random_tick(reg, dim, x, y, z, st),
        "minecraft:cauldron" | "minecraft:water_cauldron" | "minecraft:lava_cauldron"
        | "minecraft:powder_snow_cauldron" => {
            cauldron_random_tick(reg, dim, x, y, z, st)
        }
        "minecraft:pointed_dripstone" => dripstone_random_tick(reg, dim, x, y, z, st),
        "minecraft:composter" => composter_random_tick(reg, dim, x, y, z, st),
        "minecraft:wet_sponge" => wet_sponge_random_tick(reg, dim, x, y, z, st),
        "minecraft:turtle_egg" | "minecraft:sniffer_egg" => {
            egg_random_tick(reg, dim, x, y, z, st);
        }
        _ => return false,
    }
    true
}

/// A fresh [`VanillaBlockEntity`] for a block family that needs one, or `None`
/// for blocks without a world block-entity record.
fn wbe_default_for(name: &str) -> Option<VanillaBlockEntity> {
    Some(match name {
        "minecraft:campfire" | "minecraft:soul_campfire" => {
            VanillaBlockEntity::Campfire(CampfireBlockEntity::default())
        }
        "minecraft:brewing_stand" => {
            VanillaBlockEntity::BrewingStand(BrewingStand::default())
        }
        "minecraft:spawner" => {
            VanillaBlockEntity::MobSpawner(MobSpawnerBlockEntity::default())
        }
        "minecraft:jukebox" => VanillaBlockEntity::Jukebox(JukeboxBlockEntity::default()),
        "minecraft:lectern" => VanillaBlockEntity::Lectern(LecternBlockEntity::default()),
        "minecraft:chiseled_bookshelf" => {
            VanillaBlockEntity::ChiseledBookshelf(ChiseledBookshelfBlockEntity::default())
        }
        "minecraft:decorated_pot" => {
            VanillaBlockEntity::DecoratedPot(DecoratedPotBlockEntity::default())
        }
        "minecraft:beacon" => VanillaBlockEntity::Beacon(BeaconBlockEntity::default()),
        "minecraft:crafter" => VanillaBlockEntity::Crafter(CrafterBlockEntity::default()),
        _ => return None,
    })
}

fn set_and_note_world_block(dim: DimId, (x, y, z): BlockPos, state: u32) {
    set_block_dim(dim, x, y, z, state);
    note_block_update_dim(dim, x, y, z, state);
}

/// `LayeredCauldronBlock.randomTick`: under open sky during rain or snow an
/// exposed cauldron slowly fills — rain adds water, snow adds powder snow.
fn cauldron_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    if dim != DimId::Overworld
        || RAINING.load(Ordering::Relaxed) == 0
        || !random_tick_sky_exposed(dim, x, y, z)
    {
        return;
    }
    let Some(state) = cauldron_state_of(reg, st) else {
        return;
    };
    let precip = if cold_biome_at(dim, x, y, z) {
        CauldronPrecip::Snow
    } else {
        CauldronPrecip::Rain
    };
    let roll = f64::from(world_rng::next_below(1 << 24)) / f64::from(1 << 24);
    let filled = cauldron_weather_fill(state, precip, roll);
    if filled == state {
        return;
    }
    if let Some(next) = filled.to_state(reg) {
        set_and_note_world_block(dim, (x, y, z), next);
    }
}

/// `PointedDripstoneBlock.randomTick`: walk the hanging column down to its
/// tip; when a cauldron waits below it and a fluid source sits on the
/// `dripstone_block` cap, occasionally drip one charge into the cauldron.
fn dripstone_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    _st: u32,
) {
    // Find the lowest pointed dripstone in this column (the tip) and the
    // block above the column, which must be `dripstone_block`.
    let mut top = y;
    while reg.name_of(block_at_dim(dim, x, top + 1, z)) == "minecraft:pointed_dripstone" {
        top += 1;
    }
    if reg.name_of(block_at_dim(dim, x, top + 1, z)) != "minecraft:dripstone_block" {
        return;
    }
    let fluid = match reg.name_of(block_at_dim(dim, x, top + 2, z)) {
        "minecraft:water" => CauldronDripFluid::Water,
        "minecraft:lava" => CauldronDripFluid::Lava,
        _ => return,
    };
    let mut tip = y;
    while reg.name_of(block_at_dim(dim, x, tip - 1, z)) == "minecraft:pointed_dripstone" {
        tip -= 1;
    }
    let cauldron_y = tip - 1;
    let cauldron_st = block_at_dim(dim, x, cauldron_y, z);
    let Some(state) = cauldron_state_of(reg, cauldron_st) else {
        return;
    };
    // Vanilla rolls 45/256 for water and 15/256 for lava per random tick.
    let threshold = match fluid {
        CauldronDripFluid::Water => 45,
        CauldronDripFluid::Lava => 15,
    };
    if world_rng::next_below(256) >= threshold {
        return;
    }
    let filled = cauldron_drip_fill(state, fluid);
    if filled == state {
        return;
    }
    if let Some(next) = filled.to_state(reg) {
        set_and_note_world_block(dim, (x, cauldron_y, z), next);
    }
}

/// `ComposterBlock.tick` (`dhj`): a level-7 composter matures into the
/// harvest-ready level 8.
fn composter_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    let level: u8 = match reg
        .get_prop(st, "level")
        .and_then(|value| value.parse().ok())
    {
        Some(level) => level,
        None => return,
    };
    let matured = composter_mature(level);
    if matured == level {
        return;
    }
    let next = reg.with_prop(st, "level", &matured.to_string());
    set_and_note_world_block(dim, (x, y, z), next);
}

/// `SpongeBlock` in an ultrawarm dimension: a wet sponge dries out. Vanilla
/// runs this from `onPlace`; the random-tick arm also keeps a wet sponge
/// carried into the Nether by `/fill`, `/clone` or a saved edit honest.
fn wet_sponge_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    _st: u32,
) {
    if !sponge_nether_dries(dim) {
        return;
    }
    let dry = reg.state_of("minecraft:sponge", None);
    set_and_note_world_block(dim, (x, y, z), dry);
}

/// Lazy registry maintenance for the cell a random tick landed on: create the
/// default entity when a block-entity family has no record, drop a record
/// whose block left its family.
fn wbe_reconcile_at(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    pos: BlockPos,
    st: u32,
) {
    use std::collections::hash_map::Entry;
    use std::mem::discriminant;

    let mut store = lock_world_block_entities();
    let key = (dim, pos.0, pos.1, pos.2);
    match (store.entry(key), wbe_default_for(reg.name_of(st))) {
        (Entry::Vacant(entry), Some(default)) => {
            entry.insert(default);
        }
        (Entry::Occupied(entry), default)
            if default.as_ref().is_none_or(|entity| discriminant(entity) != discriminant(entry.get())) =>
        {
            entry.remove();
        }
        _ => {}
    }
}

// ---- Explosion drop decay + world griefing gates ----

/// Vanilla `Explosion.BlockInteraction` sources: which `*ExplosionDropDecay`
/// rule chooses between `DESTROY` (everything drops) and `DESTROY_WITH_DECAY`
/// (each broken block drops with probability `1 / power`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplosionDropKind {
    /// `blockExplosionDropDecay` — beds, respawn anchors, etc.
    #[allow(dead_code)]
    Block,
    /// `mobExplosionDropDecay` — creepers, ghasts, withers, end crystals.
    Mob,
    /// `tntExplosionDropDecay` — primed TNT (vanilla default: off).
    Tnt,
}

/// Live read of the drop-decay rule for an explosion source.
fn explosion_drop_decay_enabled(kind: ExplosionDropKind) -> bool {
    gamerule_live_value(match kind {
        ExplosionDropKind::Block => "blockExplosionDropDecay",
        ExplosionDropKind::Mob => "mobExplosionDropDecay",
        ExplosionDropKind::Tnt => "tntExplosionDropDecay",
    }) != 0
}

/// `Block.dropResources` inside an explosion: `DESTROY` always drops, while
/// `DESTROY_WITH_DECAY` drops when `ExplosionCondition#test` succeeds —
/// `random.nextFloat() <= 1.0F / radius`, inclusive on both ends.
fn eval_explosion_block_drop(drop_decay: bool, power: f32, roll_0_to_1: f32) -> bool {
    !drop_decay || roll_0_to_1 <= 1.0 / power.max(1.0)
}

/// `mobGriefing` for the world-modification arm of explosions, block carries
/// and crop griefing — the same atomic `/gamerule` writes and the live mob
/// explosion path reads.
fn world_griefing_enabled() -> bool {
    gamerule_live_value("mobGriefing") != 0
}

/// Vanilla 1.21.1 `rewards.experience` payouts for every catalogue
/// advancement that grants experience points (sorted by advancement id).
const VANILLA_ADVANCEMENT_EXPERIENCE: &[(&str, i32)] = &[
    ("minecraft:adventure/adventuring_time", 500),
    ("minecraft:adventure/arbalistic", 85),
    ("minecraft:adventure/blowback", 40),
    ("minecraft:adventure/bullseye", 50),
    ("minecraft:adventure/hero_of_the_village", 100),
    ("minecraft:adventure/kill_all_mobs", 100),
    ("minecraft:adventure/overoverkill", 50),
    ("minecraft:adventure/sniper_duel", 50),
    ("minecraft:adventure/trim_with_all_exclusive_armor_patterns", 150),
    ("minecraft:adventure/two_birds_one_arrow", 65),
    ("minecraft:end/levitate", 50),
    ("minecraft:husbandry/balanced_diet", 100),
    ("minecraft:husbandry/bred_all_animals", 100),
    ("minecraft:husbandry/complete_catalogue", 50),
    ("minecraft:husbandry/obtain_netherite_hoe", 100),
    ("minecraft:husbandry/whole_pack", 50),
    ("minecraft:nether/all_effects", 1000),
    ("minecraft:nether/all_potions", 100),
    ("minecraft:nether/explore_nether", 500),
    ("minecraft:nether/fast_travel", 100),
    ("minecraft:nether/netherite_armor", 100),
    ("minecraft:nether/return_to_sender", 50),
    ("minecraft:nether/uneasy_alliance", 100),
];

/// Every criterion `trigger` type 1.21.1 registers in
/// `BuiltInRegistries.TRIGGER_TYPES` (56 entries, `minecraft:`-prefixed and
/// sorted). Taken from the 1.21.1 client jar's `CriterionTriggers` static
/// initializer, where each entry is one `Registry.register(TRIGGER_TYPES,
/// "<name>", new <Trigger>())` pair; `voluntary_exile`, `used_ender_eye`,
/// `default_block_use` and `any_block_use` are registry entries even though no
/// vanilla advancement references them, so a datapack advancement may use
/// them. Trigger firing resolves a game event to a named criterion on a
/// definition; `grant_criterion` accepts the criterion only when it appears in
/// that definition's requirement groups.
#[cfg(all(test, megu_vanilla_data))]
const VANILLA_ADVANCEMENT_TRIGGERS: &[&str] = &[
    "minecraft:allay_drop_item_on_block",
    "minecraft:any_block_use",
    "minecraft:avoid_vibration",
    "minecraft:bee_nest_destroyed",
    "minecraft:bred_animals",
    "minecraft:brewed_potion",
    "minecraft:changed_dimension",
    "minecraft:channeled_lightning",
    "minecraft:construct_beacon",
    "minecraft:consume_item",
    "minecraft:crafter_recipe_crafted",
    "minecraft:cured_zombie_villager",
    "minecraft:default_block_use",
    "minecraft:effects_changed",
    "minecraft:enchanted_item",
    "minecraft:enter_block",
    "minecraft:entity_hurt_player",
    "minecraft:entity_killed_player",
    "minecraft:fall_after_explosion",
    "minecraft:fall_from_height",
    "minecraft:filled_bucket",
    "minecraft:fishing_rod_hooked",
    "minecraft:hero_of_the_village",
    "minecraft:impossible",
    "minecraft:inventory_changed",
    "minecraft:item_durability_changed",
    "minecraft:item_used_on_block",
    "minecraft:kill_mob_near_sculk_catalyst",
    "minecraft:killed_by_crossbow",
    "minecraft:levitation",
    "minecraft:lightning_strike",
    "minecraft:location",
    "minecraft:nether_travel",
    "minecraft:placed_block",
    "minecraft:player_generates_container_loot",
    "minecraft:player_hurt_entity",
    "minecraft:player_interacted_with_entity",
    "minecraft:player_killed_entity",
    "minecraft:recipe_crafted",
    "minecraft:recipe_unlocked",
    "minecraft:ride_entity_in_lava",
    "minecraft:shot_crossbow",
    "minecraft:slept_in_bed",
    "minecraft:slide_down_block",
    "minecraft:started_riding",
    "minecraft:summoned_entity",
    "minecraft:tame_animal",
    "minecraft:target_hit",
    "minecraft:thrown_item_picked_up_by_entity",
    "minecraft:thrown_item_picked_up_by_player",
    "minecraft:tick",
    "minecraft:used_ender_eye",
    "minecraft:used_totem",
    "minecraft:using_item",
    "minecraft:villager_trade",
    "minecraft:voluntary_exile",
];

/// Whether `trigger` (bare or `minecraft:`-prefixed) is one of the trigger
/// types 1.21.1 registers.
#[cfg(all(test, megu_vanilla_data))]
fn advancement_trigger_supported(trigger: &str) -> bool {
    let bare = vanilla_id_path(trigger);
    VANILLA_ADVANCEMENT_TRIGGERS
        .iter()
        .any(|known| vanilla_id_path(known) == bare)
}

// ---- Advancement rewards ----

/// The `rewards` object of a vanilla advancement JSON, distilled to the fields
/// the server can execute: experience points, recipe ids to unlock, loot
/// tables to roll into drops, and an optional function to run as the player.
/// Kept `const`-friendly so `AdvancementDefinition` can carry it inline.
#[derive(Clone, Copy, Debug)]
struct AdvancementRewardSpec {
    experience: i32,
    recipes: &'static [&'static str],
    loot: &'static [&'static str],
    function: Option<&'static str>,
}

impl AdvancementRewardSpec {
    fn to_rewards(self) -> AdvancementRewards {
        AdvancementRewards {
            experience: self.experience,
            recipes: self.recipes.iter().map(|id| (*id).to_string()).collect(),
            loot: self.loot.iter().map(|id| (*id).to_string()).collect(),
            function: self.function.map(str::to_string),
        }
    }
}

/// The `rewards` block of a built-in advancement, mirroring its
/// `data/minecraft/advancement/<tab>/<name>.json`.
///
/// Vanilla 1.21.1 pays exactly one kind of built-in reward — `experience` — in
/// 23 catalogue entries, and the 16 `story/*` milestones pay nothing at all:
/// no story JSON carries a `rewards` block, so completing one grants no XP and
/// unlocks no recipes. Recipe, loot-table and function rewards stay
/// representable for datapack advancements, but the built-in tree never uses
/// them.
fn advancement_reward_spec(id: &str) -> Option<AdvancementRewardSpec> {
    let experience = VANILLA_ADVANCEMENT_EXPERIENCE
        .iter()
        .find(|(advancement, _)| *advancement == id)
        .map(|(_, xp)| *xp)?;
    Some(AdvancementRewardSpec {
        experience,
        recipes: &[],
        loot: &[],
        function: None,
    })
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
        // `CropBlock.getGrowthSpeed` also halves once when only a diagonal
        // carries the same crop (`flag2`), which the both-axes case covers.
        assert_eq!(
            crop_growth_speed_with_diagonal(wet, false, false, true),
            5.0
        );
        assert_eq!(
            crop_growth_speed_with_diagonal(wet, false, true, true),
            5.0
        );
        assert_eq!(
            crop_growth_speed_with_diagonal(wet, false, false, false),
            10.0
        );

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
        // `CropBlock.randomTick` has no farmland requirement: the farmland below
        // only contributes through the growth-speed sum, so a crop on bare dirt
        // still ticks (with a bound of 25 / 1 + 1).
        let no_farmland = eval_crop_random_tick(CropTickInput {
            farmland_below: false,
            farmland_3x3: empty,
            ..input
        });
        assert_eq!(no_farmland, Some(3));
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
            eval_random_ticks(&sections, DEFAULT_RANDOM_TICK_SPEED, seed, |pos| match pos.0.rem_euclid(6) {
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
        let crops = eval_random_ticks(&sections, DEFAULT_RANDOM_TICK_SPEED, 1234, |_| {
            "minecraft:wheat".to_owned()
        });
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
        input.age = 15;
        let result = eval_fire_tick(input);
        assert_eq!(result.age, None);
        assert!(result.burned_out);
        input.rain_exposed = true;
        assert!(eval_fire_tick(input).extinguished);
        input.soul_fire = true;
        let soul = eval_fire_tick(input);
        assert!(!soul.extinguished);
        assert_eq!(soul.age, Some(15));
    }

    #[test]
    fn only_nonpersistent_distance_seven_leaves_decay_and_always_offer_the_loot() {
        for persistent in [true, false] {
            for distance in 1..=7 {
                for random_roll in [0, 1, 19, 255] {
                    let result = eval_leaf_decay(LeafDecayInput {
                        persistent,
                        distance,
                        random_roll,
                    });
                    assert_eq!(result.decay, !persistent && distance == 7);
                    // `LeavesBlock.randomTick` calls `dropResources` for every
                    // decayed leaf; the rarity lives in the loot table.
                    assert_eq!(result.drop_item, result.decay);
                }
            }
        }
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
            SnowIceTickResult::NoChange,
            "the melt boundary is a strict block light > 11"
        );
        assert_eq!(
            eval_ice_freeze_melt(SnowIceTickInput { light: 12, ..base }, true),
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
            for definition in tracker.definitions {
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
            assert_eq!(tracker.progress["Pat"].len(), tracker.definitions.len());
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
        assert_eq!(initial.added.len(), CORE_ADVANCEMENTS.len());
        assert_eq!(initial.added.len(), 122, "every vanilla advancement ships on join");
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
        for definition in tracker.definitions {
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
        assert_eq!(read_varint(&mut packet), Some(122));
        for definition in tracker.definitions {
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
    fn advancements_default_tracker_covers_every_vanilla_advancement() {
        // P0-2: the tracker starts from the whole catalogue, not a prefix of
        // it, so every vanilla 1.21.1 gameplay advancement is grantable and
        // reaches the client.
        let tracker = AdvancementTracker::default();
        assert_eq!(tracker.definitions.len(), 122, "live advancement coverage");
        assert_eq!(tracker.definitions.len(), CORE_ADVANCEMENTS.len());
        assert_eq!(
            tracker.definitions.iter().map(|d| d.id).collect::<Vec<_>>(),
            CORE_ADVANCEMENTS.iter().map(|d| d.id).collect::<Vec<_>>(),
            "the live set is the whole catalogue, in catalogue order"
        );

        for (id, criterion) in [
            ("minecraft:adventure/root", "killed_something"),
            ("minecraft:husbandry/root", "consumed_item"),
            ("minecraft:end/root", "entered_end"),
        ] {
            let mut tracker = AdvancementTracker::default();
            assert!(tracker.grant_only("Alex", id, 4321) > 0, "{id} grants");
            assert!(tracker.completed("Alex", id), "{id} completes");
            // The join reset ships all 122 definitions, so the screen is
            // populated even for a player with no progress in the tab.
            let (packet_id, body) = tracker.take_delta("Alex", true).unwrap().into_packet();
            assert_eq!(packet_id, 0x74);
            assert_eq!(body[0], 1, "{id} reset flag");
            let mut reset = &body[1..];
            assert_eq!(
                read_varint(&mut reset),
                Some(122),
                "{id} adds every definition"
            );
            assert!(tracker.take_delta("Alex", false).is_none(), "{id} drained");

            // The earned criteria ride a progress entry the client decodes.
            let mut client = AdvancementTracker::default();
            assert!(client.grant_criterion("Alex", id, criterion, 4321));
            let (packet_id, body) = client.take_delta("Alex", false).unwrap().into_packet();
            assert_eq!(packet_id, 0x74);
            let mut packet = body.as_slice();
            assert_eq!(packet[0], 0, "{id} incremental delta");
            packet = &packet[1..];
            assert_eq!(read_varint(&mut packet), Some(0), "{id} no definition changes");
            assert_eq!(read_varint(&mut packet), Some(0), "{id} no removed definitions");
            assert_eq!(read_varint(&mut packet), Some(1), "{id} one progress entry");
            assert_eq!(read_packet_string(&mut packet), id);
            let definition = client.definition(id).unwrap();
            let criteria = definition
                .requirements
                .iter()
                .copied()
                .flatten()
                .copied()
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                read_varint(&mut packet),
                Some(criteria.len() as i64),
                "{id} criteria"
            );
            for name in criteria {
                assert_eq!(read_packet_string(&mut packet), name);
                let earned = packet[0] == 1;
                assert_eq!(earned, name == criterion, "{id} {name} earned state");
                packet = &packet[1..];
                if earned {
                    assert_eq!(&packet[..8], &4321i64.to_be_bytes(), "{id} {name} time");
                    packet = &packet[8..];
                }
            }
            assert!(packet.is_empty(), "{id} packet fully consumed");
        }
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
    #[cfg(megu_vanilla_data)]
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

    #[test]
    fn crop_growth_probability_boundaries_across_ages_light_and_species() {
        use CropKind::*;
        let wet = [[Some(7); 3]; 3];
        let base = |kind, age, light, farmland, random_value| CropTickInput {
            kind,
            age,
            light,
            farmland_below: farmland,
            farmland_3x3: wet,
            same_crop_north_south: false,
            same_crop_east_west: false,
            random_value,
        };

        assert_eq!(eval_crop_random_tick(base(Wheat, 0, 9, true, 0)), Some(1));
        assert_eq!(eval_crop_random_tick(base(Wheat, 0, 8, true, 0)), None);
        // No farmland gate: only the light level and the speed roll decide.
        assert_eq!(eval_crop_random_tick(base(Wheat, 0, 9, false, 0)), Some(1));

        // A fully wet 3x3 has speed 10, giving a bound of floor(25 / 10) + 1 = 3:
        // only every third raw value grows the crop.
        assert_eq!(eval_crop_random_tick(base(Wheat, 3, 15, true, 0)), Some(4));
        for roll in [1, 2, 4, 5] {
            assert_eq!(eval_crop_random_tick(base(Wheat, 3, 15, true, roll)), None);
        }
        assert_eq!(eval_crop_random_tick(base(Wheat, 3, 15, true, 3)), Some(4));
        // Same crop on both axes halves the speed to 5, giving a bound of six.
        assert_eq!(
            eval_crop_random_tick(CropTickInput {
                same_crop_north_south: true,
                same_crop_east_west: true,
                ..base(Wheat, 3, 15, true, 0)
            }),
            Some(4)
        );
        for roll in [1, 5] {
            assert_eq!(
                eval_crop_random_tick(CropTickInput {
                    same_crop_north_south: true,
                    same_crop_east_west: true,
                    ..base(Wheat, 3, 15, true, roll)
                }),
                None
            );
        }
        assert_eq!(
            eval_crop_random_tick(CropTickInput {
                same_crop_north_south: true,
                same_crop_east_west: true,
                ..base(Wheat, 3, 15, true, 6)
            }),
            Some(4)
        );

        for (kind, max_age) in [
            (Wheat, 7),
            (Carrots, 7),
            (Potatoes, 7),
            (Beetroots, 3),
            (NetherWart, 3),
            (Cocoa, 2),
            (SweetBerryBush, 3),
            (MelonStem, 7),
            (PumpkinStem, 7),
        ] {
            assert_eq!(
                eval_crop_random_tick(base(kind, max_age, 15, true, 0)),
                None,
                "{kind:?}"
            );
        }

        // `CropBlock.getGrowthSpeed` halves once for a single diagonal
        // neighbour too: a lone diagonal takes the speed from 10 to 5, so the
        // bound moves from three to six.
        assert_eq!(
            eval_crop_random_tick_with_diagonal(base(Wheat, 3, 15, true, 0), true),
            Some(4)
        );
        for roll in [1, 2, 4, 5] {
            assert_eq!(
                eval_crop_random_tick_with_diagonal(base(Wheat, 3, 15, true, roll), true),
                None
            );
        }
        assert_eq!(
            eval_crop_random_tick_with_diagonal(base(Wheat, 3, 15, true, 3), true),
            None,
            "a lone diagonal's bound is six, so three does not grow"
        );
        assert_eq!(
            eval_crop_random_tick_with_diagonal(base(Wheat, 3, 15, true, 6), true),
            Some(4)
        );
        assert_eq!(
            eval_crop_random_tick_with_diagonal(base(Wheat, 3, 15, true, 3), false),
            Some(4),
            "without a diagonal neighbour the bound stays at three"
        );

        // Species that do not need farmland ignore light and use their own modulo.
        assert_eq!(eval_crop_random_tick(base(NetherWart, 2, 0, false, 9)), None);
        assert_eq!(
            eval_crop_random_tick(base(NetherWart, 2, 0, false, 10)),
            Some(3)
        );
        assert_eq!(eval_crop_random_tick(base(Cocoa, 1, 0, false, 4)), None);
        assert_eq!(eval_crop_random_tick(base(Cocoa, 1, 0, false, 5)), Some(2));
        assert_eq!(
            eval_crop_random_tick(base(SweetBerryBush, 0, 0, false, 20)),
            Some(1)
        );
    }

    #[test]
    fn random_tick_speed_and_dispatch_cover_every_registered_name_and_offset() {
        use RandomTickDispatch::*;
        let sections = [RandomTickSection {
            chunk_x: -2,
            section_y: 4,
            chunk_z: 3,
        }];
        assert!(eval_random_ticks(&sections, 0, 1, |_| "minecraft:wheat".to_owned()).is_empty());
        let samples = eval_random_ticks(&sections, 5, 1, |_| "minecraft:wheat".to_owned());
        assert_eq!(samples.len(), 5);
        for sample in &samples {
            assert!((-32..-16).contains(&sample.pos.0));
            assert!((64..80).contains(&sample.pos.1));
            assert!((48..64).contains(&sample.pos.2));
            assert_eq!(sample.dispatch, Crop);
        }
        assert_eq!(
            eval_random_ticks(&sections, DEFAULT_RANDOM_TICK_SPEED, 1, |_| {
                "minecraft:wheat".to_owned()
            }),
            eval_random_ticks(
                &sections,
                DEFAULT_RANDOM_TICK_SPEED,
                1,
                |_| "minecraft:wheat".to_owned()
            )
        );

        for (name, expected) in [
            ("minecraft:wheat", Crop),
            ("minecraft:carrots", Crop),
            ("minecraft:potatoes", Crop),
            ("minecraft:beetroots", Crop),
            ("minecraft:nether_wart", Crop),
            ("minecraft:melon_stem", Crop),
            ("minecraft:pumpkin_stem", Crop),
            ("minecraft:sweet_berry_bush", Crop),
            ("minecraft:cocoa", Crop),
            ("minecraft:fire", Fire),
            ("minecraft:soul_fire", Fire),
            ("minecraft:oak_leaves", Leaves),
            ("minecraft:dark_oak_leaves", Leaves),
            ("minecraft:snow", Snow),
            ("minecraft:ice", Ice),
            ("minecraft:stone", Other),
        ] {
            let sampled = eval_random_ticks(&sections, 1, 7, |_| name.to_owned());
            assert_eq!(sampled.len(), 1);
            assert_eq!(sampled[0].dispatch, expected, "{name}");
        }

        // Across seeds the section-local x offset covers all sixteen columns.
        let mut seen = [false; 16];
        for seed in 0..64 {
            for sample in eval_random_ticks(&sections, 3, seed, |_| "minecraft:stone".to_owned()) {
                seen[(sample.pos.0 - sections[0].chunk_x * 16) as usize] = true;
            }
        }
        assert!(seen.iter().all(|covered| *covered), "x offsets: {seen:?}");
    }

    #[test]
    fn fire_age_burnout_rain_and_soul_fire_boundaries() {
        let base = FireTickInput {
            age: 15,
            rain_exposed: false,
            soul_fire: false,
            flammable_neighbors: [false; 6],
            spread_rolls: [0; 6],
            age_roll: 2,
            burnout_roll: 1,
        };
        assert_eq!(eval_fire_tick(base).age, Some(15));
        assert_eq!(eval_fire_tick(FireTickInput { age: 14, ..base }).age, Some(15));
        assert_eq!(
            eval_fire_tick(FireTickInput {
                age: 14,
                age_roll: 1,
                ..base
            })
            .age,
            Some(14)
        );
        assert_eq!(
            eval_fire_tick(FireTickInput {
                age: 14,
                age_roll: 0,
                ..base
            })
            .age,
            Some(14)
        );
        assert_eq!(
            eval_fire_tick(FireTickInput {
                age: 255,
                age_roll: 2,
                ..base
            })
            .age,
            Some(15)
        );

        let spread = eval_fire_tick(FireTickInput {
            flammable_neighbors: [true, true, false, false, false, false],
            spread_rolls: [0, 1, 0, 0, 0, 0],
            ..base
        });
        assert_eq!(spread.spread, [true, false, false, false, false, false]);
        // `FireBlock.tick` burns out on `age == 15 && nextInt(4) == 0`, and the
        // only burnability test in that arm is the cell below (index 3 of the
        // caller's direction table).
        assert!(eval_fire_tick(FireTickInput { burnout_roll: 0, ..base }).burned_out);
        assert!(!eval_fire_tick(FireTickInput { burnout_roll: 1, ..base }).burned_out);
        assert!(
            !eval_fire_tick(FireTickInput {
                age: 14,
                burnout_roll: 0,
                ..base
            })
            .burned_out,
            "burnout needs the fire to have reached age 15"
        );
        assert!(
            !eval_fire_tick(FireTickInput {
                burnout_roll: 4,
                flammable_neighbors: [false, false, false, true, false, false],
                ..base
            })
            .burned_out,
            "a flammable block below keeps the fire alive"
        );
        assert!(
            eval_fire_tick(FireTickInput {
                burnout_roll: 4,
                flammable_neighbors: [true, true, true, false, true, true],
                ..base
            })
            .burned_out,
            "flammable neighbours beside or above do not stop the burnout"
        );

        // Rain puts a fire out on `nextFloat() < 0.2 + age * 0.03`: 20% at age 0
        // and 65% at age 15, with the sample taken from the tick's float roll.
        let rain = eval_fire_tick(FireTickInput {
            rain_exposed: true,
            flammable_neighbors: [true; 6],
            ..base
        });
        assert!(rain.extinguished);
        assert_eq!(rain.age, None);
        assert_eq!(rain.spread, [false; 6]);
        let extinguish = |age, roll| {
            eval_fire_tick(FireTickInput {
                age,
                rain_exposed: true,
                burnout_roll: roll,
                flammable_neighbors: [true; 6],
                ..base
            })
            .extinguished
        };
        assert!(extinguish(0, 51), "51/256 < 0.20");
        assert!(!extinguish(0, 52), "52/256 >= 0.20");
        assert!(extinguish(15, 166), "166/256 < 0.65");
        assert!(!extinguish(15, 167), "167/256 >= 0.65");
        let soul = eval_fire_tick(FireTickInput {
            rain_exposed: true,
            soul_fire: true,
            age: 7,
            burnout_roll: 0,
            flammable_neighbors: [true; 6],
            ..base
        });
        assert_eq!(
            soul,
            FireTickResult {
                extinguished: false,
                age: Some(7),
                spread: [false; 6],
                burned_out: false,
            }
        );
    }

    #[test]
    fn leaf_decay_boundary_is_exactly_seven_and_the_loot_is_always_offered() {
        for distance in [6, 7, 8, 255] {
            let result = eval_leaf_decay(LeafDecayInput {
                persistent: false,
                distance,
                random_roll: 0,
            });
            assert_eq!(result.decay, distance >= 7);
            assert_eq!(result.drop_item, distance >= 7);
        }
        assert!(
            !eval_leaf_decay(LeafDecayInput {
                persistent: true,
                distance: 7,
                random_roll: 0
            })
            .decay
        );
        for roll in [0, 19, 20, 39, 40] {
            let result = eval_leaf_decay(LeafDecayInput {
                persistent: false,
                distance: 7,
                random_roll: roll,
            });
            assert!(result.drop_item, "roll {roll} must still offer the loot");
        }
    }

    #[test]
    fn snow_and_ice_boundaries_for_light_roll_support_and_source_water() {
        use SnowIceTickResult::*;
        let base = SnowIceTickInput {
            cold_biome: true,
            sky_exposed: true,
            light: 9,
            support_is_full_solid: false,
            water_source: true,
            random_roll: 0,
        };
        for roll in [0, 15, 16, 31, 32] {
            let accumulate =
                eval_snow_accumulation(SnowIceTickInput { random_roll: roll, ..base });
            assert_eq!(accumulate == AccumulateSnow, roll % 16 == 0, "roll {roll}");
        }
        assert_eq!(
            eval_snow_accumulation(SnowIceTickInput { light: 10, ..base }),
            NoChange
        );
        assert_eq!(
            eval_snow_accumulation(SnowIceTickInput {
                support_is_full_solid: true,
                ..base
            }),
            NoChange
        );
        // `IceBlock.randomTick` melts on `blockLight > 11 - lightBlock(ice)`;
        // ice passes skylight, so its own light block is 0 and the boundary is
        // a strict `> 11` — a block light of exactly 11 does not melt.
        assert_eq!(eval_ice_freeze_melt(SnowIceTickInput { light: 10, ..base }, true), NoChange);
        assert_eq!(eval_ice_freeze_melt(SnowIceTickInput { light: 11, ..base }, true), NoChange);
        assert_eq!(eval_ice_freeze_melt(SnowIceTickInput { light: 12, ..base }, true), MeltIce);
        assert_eq!(eval_ice_freeze_melt(SnowIceTickInput { light: 15, ..base }, true), MeltIce);
        // Freezing does not consult light; melting does not consult water source.
        assert_eq!(
            eval_ice_freeze_melt(SnowIceTickInput { light: 15, ..base }, false),
            FreezeWater
        );
        for changed in [
            SnowIceTickInput { cold_biome: false, ..base },
            SnowIceTickInput { sky_exposed: false, ..base },
            SnowIceTickInput { water_source: false, ..base },
        ] {
            assert_eq!(eval_ice_freeze_melt(changed, false), NoChange);
        }
    }
}

#[cfg(all(test, megu_vanilla_data))]
fn world_mechanism_test_client(name: &str, eid: i32, uuid: [u8; 16], game_mode: u8) -> Arc<Client> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stream = std::net::TcpStream::connect(addr).unwrap();
    let _accepted = listener.accept().unwrap();
    Arc::new(Client {
        eid,
        uuid,
        name: name.to_string(),
        player_attack_damage_id: None,
        pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0, true)),
        teleport: Mutex::new(Default::default()),
        hp: Mutex::new(20.0),
        sock: Mutex::new(stream),
        inv: Mutex::new(HashMap::new()),
        cursor: Mutex::new((0, 0, Vec::new())),
        drag: Mutex::new((0, Vec::new())),
        open_chest: Mutex::new(None),
        chest_sync: Mutex::new(None),
        open_furnace: Mutex::new(None),
        craft: Mutex::new(HashMap::new()),
        craft9: Mutex::new(HashMap::new()),
        open_table: Mutex::new(false),
        open_merchant: Mutex::new(false),
        trade_sel: Mutex::new(0),
        merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
        bow_draw: Mutex::new(None),
        dig: Mutex::new(None),
        eat_start: Mutex::new(None),
        blocking: Mutex::new(false),
        sprinting: Mutex::new(false),
        sneaking: Mutex::new(false),
        last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
        fall_fly: Mutex::new(false),
        invulnerable: Mutex::new(0),
        last_hurt: Mutex::new(0.0),
        shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
        absorb: Mutex::new(0.0),
        pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
        xp: Mutex::new(0),
        skin: Mutex::new(None),
        seen: Mutex::new(HashSet::new()),
        chunks: Mutex::new(HashSet::from([(-1, -1), (-1, 0), (0, -1), (0, 0)])),
        chunk_ack: Mutex::new(ChunkFeedback::new(1)),
        gen_sources: Mutex::new(GenSources::default()),
        gm: Mutex::new(game_mode),
        fall_from: Mutex::new(65.0),
        food: Mutex::new(20),
        air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        armor_pts: Mutex::new(0),
        sat: Mutex::new(5.0),
        walk_acc: Mutex::new(0.0),
        lastp: Mutex::new((0.5, 65.0, 0.5)),
        spawn_pt: Mutex::new(None),
        dim: Mutex::new(DimId::Overworld),
        spawn_dim: Mutex::new(DimId::Overworld),
        view: Mutex::new(2),
        in_portal: Mutex::new(false),
        portal_at: Mutex::new(None),
        held: Mutex::new(0),
        effs: Mutex::new(Vec::new()),
        lod_v1_enabled: AtomicBool::new(false),
        underground_revealed: AtomicBool::new(false),
        sleeping: Mutex::new(None),
        inv_state_id: AtomicI32::new(0),
        generic_kill_pending: AtomicBool::new(false),
        use_on_resend: Mutex::new(None),
    })
}

/// P1-A live wiring tests: production `on_random_tick` and the live gamerule
/// atomics, not just the pure evaluators.
#[cfg(all(test, megu_vanilla_data))]
mod p1a_random_tick_and_gamerule_tests {
    use super::*;

    /// Records the original state of every cell a test writes and restores
    /// them on drop, so a failed assertion cannot leak world edits into the
    /// next test under the world lock.
    struct Cells(Vec<(i32, i32, i32, u32)>);
    impl Cells {
        fn new() -> Self {
            Cells(Vec::new())
        }
        fn set(&mut self, x: i32, y: i32, z: i32, st: u32) {
            self.record(x, y, z);
            set_block(x, y, z, st);
        }
        fn record(&mut self, x: i32, y: i32, z: i32) {
            self.0.push((x, y, z, block_at(x, y, z)));
        }
    }
    impl std::ops::Drop for Cells {
        fn drop(&mut self) {
            for (x, y, z, st) in self.0.drain(..).rev() {
                set_block(x, y, z, st);
            }
        }
    }

    /// Restores the gamerule atomics a test changed, on every exit path.
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

    fn prop_u8(reg: &crate::block_registry::BReg, st: u32, key: &str) -> Option<u8> {
        reg.get_prop(st, key).and_then(|v| v.parse().ok())
    }

    /// Calls `on_random_tick` until `done` holds, returning false when the cap
    /// was hit first. Random-tick outcomes are rolls; the caps sit orders of
    /// magnitude past the expected count so a miss means the wiring broke, not
    /// that the roll was unlucky.
    fn tick_until(
        reg: &crate::block_registry::BReg,
        x: i32,
        y: i32,
        z: i32,
        cap: usize,
        done: impl Fn() -> bool,
    ) -> bool {
        for _ in 0..cap {
            on_random_tick(reg, x, y, z);
            if done() {
                return true;
            }
        }
        false
    }

    fn moist_farmland(reg: &crate::block_registry::BReg) -> u32 {
        reg.with_prop(reg.state_of("minecraft:farmland", None), "moisture", "7")
    }

    fn new_mob(eid: i32, etype: i64, x: f64, y: f64, z: f64, hp: f32, air: i32) -> Arc<Mob> {
        let last_action = Instant::now() - Duration::from_secs(10);
        let mut uuid = [0; 16];
        uuid[12..].copy_from_slice(&eid.to_be_bytes());
        Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            uuid,
            etype,
            pos: Mutex::new((x, y, z, 0.0, 0.0)),
            hp: Mutex::new(hp),
            last_hit: Mutex::new(last_action),
            hurt_at: Mutex::new(last_action),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(air),
        })
    }

    #[test]
    fn random_tick_wires_crops_fire_leaves_snow_ice_saplings_and_grass() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let mut cells = Cells::new();
        let old_rain = RAINING.swap(0, Ordering::Relaxed);
        let rules = Rules::take(&["doFireTick"]);

        let name = |x: i32, y: i32, z: i32| reg.name_of(block_at(x, y, z)).to_owned();
        let age = |x: i32, y: i32, z: i32| prop_u8(reg, block_at(x, y, z), "age");

        // --- crops: wheat and carrots on hydrated farmland, nether wart on
        // soul sand. All sit well above the surface, so the sky-light arm
        // supplies the >=9 the evaluator asks for.
        let farmland = moist_farmland(reg);
        cells.set(0, 149, 0, farmland);
        cells.set(0, 150, 0, reg.state_of("minecraft:wheat", None));
        assert!(
            tick_until(reg, 0, 150, 0, 5000, || {
                age(0, 150, 0).is_some_and(|a| a > 0)
            }),
            "production on_random_tick must grow wheat on farmland"
        );
        cells.set(4, 149, 0, farmland);
        cells.set(4, 150, 0, reg.state_of("minecraft:carrots", None));
        assert!(
            tick_until(reg, 4, 150, 0, 5000, || {
                age(4, 150, 0).is_some_and(|a| a > 0)
            }),
            "production on_random_tick must grow carrots on farmland"
        );
        cells.set(8, 149, 0, reg.state_of("minecraft:soul_sand", None));
        cells.set(8, 150, 0, reg.state_of("minecraft:nether_wart", None));
        assert!(
            tick_until(reg, 8, 150, 0, 3000, || {
                age(8, 150, 0).is_some_and(|a| a > 0)
            }),
            "production on_random_tick must grow nether wart on soul sand"
        );

        // --- fire: rain extinguishes a sky-exposed fire; a fire with no
        // flammable neighbour burns out; doFireTick=false freezes both. Clear
        // the neighbourhoods first so the fixture does not depend on whatever
        // the generator happened to leave in the air cells.
        for x in 12..=15 {
            for y in 149..=151 {
                for z in -1..=1 {
                    cells.set(x, y, z, 0);
                }
            }
        }
        cells.set(12, 150, 0, reg.state_of("minecraft:fire", None));
        RAINING.store(60, Ordering::Relaxed);
        on_random_tick(reg, 12, 150, 0);
        assert_eq!(
            block_at(12, 150, 0),
            0,
            "rain on a sky-exposed fire must extinguish it"
        );
        RAINING.store(0, Ordering::Relaxed);

        cells.set(13, 150, 0, reg.state_of("minecraft:fire", None));
        assert!(
            tick_until(reg, 13, 150, 0, 200, || block_at(13, 150, 0) == 0),
            "a fire with no flammable neighbours must burn out"
        );

        let fire = reg.state_of("minecraft:fire", None);
        cells.set(14, 150, 0, fire);
        cells.set(14, 150, 1, reg.state_of("minecraft:oak_planks", None));
        GR_DO_FIRE_TICK.store(0, Ordering::Relaxed);
        for _ in 0..64 {
            on_random_tick(reg, 14, 150, 0);
        }
        assert_eq!(
            block_at(14, 150, 0),
            fire,
            "doFireTick=false must freeze the fire tick entirely"
        );
        assert_eq!(
            name(14, 150, 1),
            "minecraft:oak_planks",
            "doFireTick=false must stop spread onto flammable neighbours"
        );
        GR_DO_FIRE_TICK.store(1, Ordering::Relaxed);
        // ...and re-enabled, the same fixture must ignite the planks.
        assert!(
            tick_until(reg, 14, 150, 0, 4000, || block_at(14, 150, 1) == fire),
            "doFireTick=true must let fire spread to a flammable neighbour"
        );

        // --- leaves: a distance-7 non-persistent leaf decays; a persistent one
        // does not.
        let mut decaying = reg.state_of("minecraft:oak_leaves", None);
        decaying = reg.with_prop(decaying, "persistent", "false");
        decaying = reg.with_prop(decaying, "distance", "7");
        cells.set(20, 150, 0, decaying);
        on_random_tick(reg, 20, 150, 0);
        assert_eq!(
            block_at(20, 150, 0),
            0,
            "non-persistent distance-7 leaves must decay to air on one tick"
        );
        let mut persistent = reg.state_of("minecraft:oak_leaves", None);
        persistent = reg.with_prop(persistent, "persistent", "true");
        persistent = reg.with_prop(persistent, "distance", "7");
        cells.set(21, 150, 0, persistent);
        for _ in 0..8 {
            on_random_tick(reg, 21, 150, 0);
        }
        assert_eq!(
            block_at(21, 150, 0),
            persistent,
            "persistent leaves must survive random ticks"
        );

        // --- ice: block light >= 11 melts it to a water source.
        cells.set(24, 150, 0, reg.state_of("minecraft:ice", None));
        cells.set(25, 150, 0, reg.state_of("minecraft:torch", None));
        on_random_tick(reg, 24, 150, 0);
        assert_eq!(
            name(24, 150, 0),
            "minecraft:water",
            "ice next to a torch must melt into water"
        );

        // --- sapling: stage 0 -> stage 1 -> grown tree.
        cells.set(28, 150, 0, reg.state_of("minecraft:oak_sapling", None));
        for dy in 1..6 {
            cells.set(28, 150 + dy, 0, 0);
        }
        assert!(
            tick_until(reg, 28, 150, 0, 30000, || {
                name(28, 150, 0) == "minecraft:oak_log"
            }),
            "a lit sapling must grow into a tree through production ticks"
        );
        assert_eq!(name(28, 151, 0), "minecraft:oak_log");
        assert_eq!(
            name(29, 152, 0),
            "minecraft:oak_leaves",
            "the grown tree must carry a leaf canopy"
        );

        // --- grass: spreads to adjacent dirt in the light, dies to dirt under
        // an opaque cap in the dark.
        cells.set(32, 150, 0, reg.state_of("minecraft:grass_block", None));
        cells.set(33, 150, 0, reg.state_of("minecraft:dirt", None));
        assert!(
            tick_until(reg, 32, 150, 0, 2000, || {
                name(33, 150, 0) == "minecraft:grass_block"
            }),
            "lit grass must spread to adjacent dirt"
        );
        cells.set(36, 5, 0, reg.state_of("minecraft:grass_block", None));
        cells.set(36, 6, 0, reg.state_of("minecraft:stone", None));
        assert!(
            tick_until(reg, 36, 5, 0, 8, || name(36, 5, 0) == "minecraft:dirt"),
            "grass buried under an opaque block must die back to dirt"
        );

        RAINING.store(old_rain, Ordering::Relaxed);
        drop(rules);
    }

    #[test]
    fn gamerule_random_tick_speed_and_damage_rules_take_effect_live() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let mut cells = Cells::new();
        let rules = Rules::take(&[
            "randomTickSpeed",
            "doFireTick",
            "fallDamage",
            "fireDamage",
            "drowningDamage",
            "commandModificationBlockLimit",
            "waterSourceConversion",
            "lavaSourceConversion",
        ]);
        let ctx = CmdCtx {
            me: None,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        };
        let mut sink: Option<&Client> = None;
        let name = |x: i32, y: i32, z: i32| reg.name_of(block_at(x, y, z)).to_owned();
        let gamerule = |sink: &mut Option<&Client>, name: &str| {
            let (cmd, value) = name.split_once(' ').unwrap();
            run_command_ctx(&ctx, &format!("/gamerule {cmd} {value}"), sink)
        };
        assert_eq!(
            gamerule(&mut sink, "randomTickSpeed 0"),
            "Gamerule randomTickSpeed is now set to: 0"
        );

        // randomTickSpeed 0: the pass leaves the world untouched; a non-zero
        // speed reaches the cells the pass samples.
        let farmland = moist_farmland(reg);
        cells.set(0, 149, 0, farmland);
        cells.set(0, 150, 0, reg.state_of("minecraft:wheat", None));
        let old_forced = std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()));
        forceloaded().lock().unwrap_or_else(|e| e.into_inner()).insert((0, 0));
        quiescent_chunk_tick();
        for _ in 0..3 {
            random_tick_pass();
        }
        assert_eq!(
            prop_u8(reg, block_at(0, 150, 0), "age"),
            Some(0),
            "randomTickSpeed=0 must stop every random tick"
        );
        assert_eq!(
            gamerule(&mut sink, "randomTickSpeed 4096"),
            "Gamerule randomTickSpeed is now set to: 4096"
        );
        assert_eq!(gr_random_tick_speed(), 4096);
        let mut grew = false;
        for _ in 0..80 {
            random_tick_pass();
            if prop_u8(reg, block_at(0, 150, 0), "age").is_some_and(|a| a > 0) {
                grew = true;
                break;
            }
        }
        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = old_forced;
        assert!(grew, "a non-zero randomTickSpeed must reach the wheat");

        // doFireTick false: production on_random_tick leaves fire alone.
        gamerule(&mut sink, "doFireTick false");
        let fire = reg.state_of("minecraft:fire", None);
        for x in 4..=5 {
            for z in -1..=1 {
                cells.set(x, 150, z, 0);
            }
        }
        cells.set(4, 150, 0, fire);
        cells.set(4, 150, 1, reg.state_of("minecraft:oak_planks", None));
        for _ in 0..40 {
            on_random_tick(reg, 4, 150, 0);
        }
        assert_eq!(block_at(4, 150, 0), fire, "doFireTick=false froze the fire");
        assert_eq!(name(4, 150, 1), "minecraft:oak_planks");

        // fallDamage: mob_snap_y's fall arm honors it.
        gamerule(&mut sink, "fallDamage false");
        let ground = surface_y(48, 0) as i32;
        for dy in 0..14 {
            cells.set(48, ground + dy, 0, 0);
        }
        let safe = new_mob(9601, 22, 48.5, ground as f64 + 12.0, 0.5, 20.0, 300);
        let mut pos = *safe.pos.lock().unwrap_or_else(|e| e.into_inner());
        mob_snap_y(&safe, &mut pos);
        assert_eq!(
            *safe.hp.lock().unwrap_or_else(|e| e.into_inner()),
            20.0,
            "fallDamage=false suppresses mob fall damage"
        );
        gamerule(&mut sink, "fallDamage true");
        let hurt = new_mob(9602, 22, 48.5, ground as f64 + 12.0, 0.5, 20.0, 300);
        let mut pos = *hurt.pos.lock().unwrap_or_else(|e| e.into_inner());
        mob_snap_y(&hurt, &mut pos);
        assert!(
            *hurt.hp.lock().unwrap_or_else(|e| e.into_inner()) < 20.0,
            "fallDamage=true must let the same fall hurt: {}",
            *hurt.hp.lock().unwrap_or_else(|e| e.into_inner())
        );

        // Exercise the live mob tick's fire and drowning damage.
        let dummy_client = world_mechanism_test_client("gamerule_tester", 9999, [9u8; 16], 1);
        let old_clients = std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner()));
        let old_mobs = std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner()));
        let old_tick = TICK.load(Ordering::Relaxed);
        let old_world_time = WORLD_TIME.load(Ordering::Relaxed);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(dummy_client);
        TICK.store(server_ticks(20), Ordering::Relaxed);
        WORLD_TIME.store(6000, Ordering::Relaxed);

        let pig_in_lava = |eid: i32| {
            let mob = new_mob(
                eid,
                77,
                -2.5,
                60.0,
                -2.5,
                10.0,
                server_ticks(AIR_SUPPLY_GT) as i32,
            );
            mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
            mob
        };
        let submerged_chicken = |eid: i32| {
            let mob = new_mob(eid, 19, 0.5, 60.0, 0.5, 4.0, 0);
            mobs().lock().unwrap_or_else(|e| e.into_inner()).push(mob.clone());
            mob
        };
        cells.set(-3, 60, -3, 102); // flowing lava under the pig
        cells.set(0, 60, 0, 81); // water at the chicken's eye level
        cells.set(0, 61, 0, 81);
        gamerule(&mut sink, "fireDamage false");
        gamerule(&mut sink, "drowningDamage false");
        let pig = pig_in_lava(9610);
        let chicken = submerged_chicken(9611);
        refresh_hot();
        mob_tick();
        assert_eq!(
            *pig.hp.lock().unwrap_or_else(|e| e.into_inner()),
            10.0,
            "fireDamage=false suppresses lava damage"
        );
        assert_eq!(
            *chicken.hp.lock().unwrap_or_else(|e| e.into_inner()),
            4.0,
            "drowningDamage=false suppresses drowning"
        );
        mobs().lock().unwrap_or_else(|e| e.into_inner()).clear();
        gamerule(&mut sink, "fireDamage true");
        gamerule(&mut sink, "drowningDamage true");
        let pig = pig_in_lava(9612);
        let chicken = submerged_chicken(9613);
        mob_tick();
        assert!(
            *pig.hp.lock().unwrap_or_else(|e| e.into_inner()) < 10.0,
            "fireDamage=true must let lava hurt: {}",
            *pig.hp.lock().unwrap_or_else(|e| e.into_inner())
        );
        assert!(
            *chicken.hp.lock().unwrap_or_else(|e| e.into_inner()) < 4.0,
            "drowningDamage=true must let drowning hurt: {}",
            *chicken.hp.lock().unwrap_or_else(|e| e.into_inner())
        );
        mobs().lock().unwrap_or_else(|e| e.into_inner()).clear();
        *clients().lock().unwrap_or_else(|e| e.into_inner()) = old_clients;
        *mobs().lock().unwrap_or_else(|e| e.into_inner()) = old_mobs;
        TICK.store(old_tick, Ordering::Relaxed);
        WORLD_TIME.store(old_world_time, Ordering::Relaxed);

        // commandModificationBlockLimit: /fill honors it.
        gamerule(&mut sink, "commandModificationBlockLimit 4");
        for x in 0..5 {
            cells.set(x, 150, 4, 0);
        }
        let reply = run_command_ctx(&ctx, "/fill 0 150 4 4 150 4 stone", &mut sink);
        assert!(
            reply.contains("Too many blocks"),
            "a 5-block fill over a limit of 4 must be refused: {reply}"
        );
        gamerule(&mut sink, "commandModificationBlockLimit 32768");
        let reply = run_command_ctx(&ctx, "/fill 0 150 4 4 150 4 stone", &mut sink);
        assert!(
            !reply.contains("Too many blocks"),
            "the default limit must allow a 5-block fill: {reply}"
        );
        assert_eq!(name(0, 150, 4), "minecraft:stone");

        check_source_conversion_rules(reg, &mut cells, &mut sink, gamerule);
        drop(rules);
    }

    fn check_source_conversion_rules(
        reg: &crate::block_registry::BReg,
        cells: &mut Cells,
        sink: &mut Option<&Client>,
        gamerule: impl Fn(&mut Option<&Client>, &str) -> String,
    ) {
        // Source conversion: a flowing cell flanked by two same-kind sources
        // over solid ground converts to a source only while the rule is on.
        // Ticking the cell itself runs `FlowingFluid.getNewLiquid`, the exact
        // place the gamerule feeds.
        let run_fluid = |cells: &mut Cells, pos: (i32, i32, i32), kind: flowing_fluid::Kind| {
            let plan = flowing_fluid::tick(
                &mut FluidReader,
                pos,
                kind,
                false,
                source_conversion_enabled(kind),
            )
            .expect("fluid tick over a stone floor must not be refused");
            for (p, cell) in plan.writes {
                cells.record(p.0, p.1, p.2);
                match cell {
                    flowing_fluid::Cell::Fluid(f) => {
                        let base = if f.kind == flowing_fluid::Kind::Water {
                            80
                        } else {
                            96
                        };
                        set_block(p.0, p.1, p.2, base + u32::from(f.level));
                    }
                    flowing_fluid::Cell::Air => set_block(p.0, p.1, p.2, 0),
                    _ => {}
                }
            }
        };
        cells.set(52, 150, 0, 80);
        cells.set(53, 149, 0, reg.state_of("minecraft:stone", None));
        cells.set(53, 150, 0, 81);
        cells.set(53, 151, 0, 0);
        cells.set(54, 150, 0, 80);
        gamerule(sink, "waterSourceConversion false");
        run_fluid(cells, (53, 150, 0), flowing_fluid::Kind::Water);
        assert_ne!(
            block_at(53, 150, 0),
            80,
            "waterSourceConversion=false must leave the gap flowing"
        );
        cells.set(52, 150, 0, 80);
        cells.set(53, 150, 0, 81);
        cells.set(54, 150, 0, 80);
        gamerule(sink, "waterSourceConversion true");
        run_fluid(cells, (53, 150, 0), flowing_fluid::Kind::Water);
        assert_eq!(
            block_at(53, 150, 0),
            80,
            "waterSourceConversion=true must convert the gap to a source"
        );

        cells.set(58, 150, 0, 96);
        cells.set(59, 149, 0, reg.state_of("minecraft:stone", None));
        cells.set(59, 150, 0, 100);
        cells.set(59, 151, 0, 0);
        cells.set(60, 150, 0, 96);
        run_fluid(cells, (59, 150, 0), flowing_fluid::Kind::Lava);
        assert_ne!(
            block_at(59, 150, 0),
            96,
            "lavaSourceConversion=false must leave the gap flowing"
        );
        cells.set(58, 150, 0, 96);
        cells.set(59, 150, 0, 100);
        cells.set(60, 150, 0, 96);
        gamerule(sink, "lavaSourceConversion true");
        run_fluid(cells, (59, 150, 0), flowing_fluid::Kind::Lava);
        assert_eq!(
            block_at(59, 150, 0),
            96,
            "lavaSourceConversion=true must convert the gap to a source"
        );
    }

    #[test]
    fn gamerule_save_v2_syncs_new_runtime_atomics() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let rules = Rules::take(&[
            "randomTickSpeed",
            "doFireTick",
            "commandModificationBlockLimit",
            "lavaSourceConversion",
            "fallDamage",
            "showDeathMessages",
        ]);
        // Change rules, save, flip them back, then load: the atomics must come
        // back to the saved values without any further writes.
        let changed: Vec<(&'static str, i32)> = vec![
            ("randomTickSpeed", 7),
            ("doFireTick", 0),
            ("commandModificationBlockLimit", 64),
            ("lavaSourceConversion", 1),
            ("fallDamage", 0),
            ("showDeathMessages", 0),
        ];
        for (name, value) in &changed {
            gamerule_atomic_store(name, *value);
            command_gamerules().lock().unwrap_or_else(|e| e.into_inner()).insert(name, *value);
        }
        let mut saved = Vec::new();
        save_world_into(&mut saved);
        for (name, _) in &changed {
            gamerule_atomic_store(name, 12345);
        }
        load_world_from(&saved).expect("a save written by save_world_into must load");
        for (name, value) in &changed {
            assert_eq!(
                gamerule_atomic_value(name),
                Some(*value),
                "loading a save must restore {name} into its runtime atomic"
            );
        }
        drop(rules);
    }
}

/// P2 parity tests: the plant random ticks, the remaining gamerule reads and
/// the advancement rewards grant path.
#[cfg(all(test, megu_vanilla_data))]
mod p2_parity_world_tests {
    use super::*;

    /// Records the original state of every cell a test writes and restores
    /// them on drop, so a failed assertion cannot leak world edits into the
    /// next test under the world lock.
    struct Cells(Vec<(i32, i32, i32, u32)>);
    impl Cells {
        fn new() -> Self {
            Cells(Vec::new())
        }
        fn set(&mut self, x: i32, y: i32, z: i32, st: u32) {
            self.0.push((x, y, z, block_at(x, y, z)));
            set_block(x, y, z, st);
        }
        fn record(&mut self, x: i32, y: i32, z: i32) {
            self.0.push((x, y, z, block_at(x, y, z)));
        }
    }
    impl std::ops::Drop for Cells {
        fn drop(&mut self) {
            for (x, y, z, st) in self.0.drain(..).rev() {
                set_block(x, y, z, st);
            }
        }
    }

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

    fn prop(reg: &crate::block_registry::BReg, st: u32, key: &str) -> Option<String> {
        reg.get_prop(st, key)
    }

    #[test]
    fn p2_parity_cactus_age_growth_height_cap_and_side_block_check() {
        let eval = |age: u8, height: u8, space: bool, solid: bool| {
            eval_cactus_random_tick(CactusTickInput {
                age,
                column_height: height,
                space_above: space,
                solid_touching_growth: solid,
            })
        };
        assert_eq!(eval(0, 1, true, false), CactusTickResult::AdvanceAge(1));
        assert_eq!(eval(14, 1, true, false), CactusTickResult::AdvanceAge(15));
        assert_eq!(eval(15, 1, true, false), CactusTickResult::GrowSegment);
        assert_eq!(eval(15, 3, true, false), CactusTickResult::NoChange);
        assert_eq!(eval(15, 2, true, true), CactusTickResult::NoChange);
        assert_eq!(eval(15, 1, false, false), CactusTickResult::NoChange);

        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let cactus = reg.state_of("minecraft:cactus", None);
        if reg.name_of(cactus) != "minecraft:cactus" {
            return; // registry without cactus: evaluator assertions above still ran
        }
        let sand = reg.state_of("minecraft:sand", None);
        let stone = reg.state_of("minecraft:stone", None);
        let mut cells = Cells::new();

        // One cactus at age 15 grows a fresh segment and resets its age.
        cells.set(0, 99, 0, sand);
        cells.set(0, 100, 0, reg.with_prop(cactus, "age", "15"));
        for y in 100..=102 {
            cells.record(0, y, 0);
        }
        cactus_random_tick(reg, DimId::Overworld, 0, 100, 0, block_at(0, 100, 0));
        assert_eq!(reg.name_of(block_at(0, 101, 0)), "minecraft:cactus");
        assert_eq!(prop(reg, block_at(0, 100, 0), "age").as_deref(), Some("0"));

        // A three-high column is capped: no fourth segment.
        cells.set(4, 99, 0, sand);
        for y in 100..=102 {
            cells.set(4, y, 0, reg.with_prop(cactus, "age", "15"));
        }
        cells.record(4, 103, 0);
        cactus_random_tick(reg, DimId::Overworld, 4, 100, 0, block_at(4, 100, 0));
        assert!(
            reg.is_air(block_at(4, 103, 0)),
            "height-3 cactus must not grow a fourth segment"
        );

        // A solid block touching the growth cell sideways blocks the segment.
        cells.set(8, 99, 0, sand);
        cells.set(8, 100, 0, reg.with_prop(cactus, "age", "15"));
        cells.set(9, 101, 0, stone);
        cells.record(8, 101, 0);
        cactus_random_tick(reg, DimId::Overworld, 8, 100, 0, block_at(8, 100, 0));
        assert!(
            reg.is_air(block_at(8, 101, 0)),
            "a solid side neighbor at growth level must block cactus growth"
        );
    }

    #[test]
    fn p2_parity_bamboo_sapling_stalk_growth_height_cap_and_leaves() {
        let eval = |is_sapling: bool, stage: u8, height: u8, light: u8, space: bool, roll: u8| {
            eval_bamboo_random_tick(BambooTickInput {
                is_sapling,
                stage,
                column_height: height,
                light_above: light,
                space_above: space,
                random_roll: roll,
            })
        };
        assert_eq!(eval(true, 0, 1, 15, true, 0), BambooTickResult::SaplingToStalk);
        assert_eq!(eval(true, 0, 1, 8, true, 0), BambooTickResult::NoChange);
        assert_eq!(eval(true, 0, 1, 15, true, 1), BambooTickResult::NoChange);
        assert_eq!(eval(false, 0, 4, 15, true, 3), BambooTickResult::GrowStalk);
        assert_eq!(eval(false, 0, 16, 15, true, 0), BambooTickResult::NoChange);
        assert_eq!(eval(false, 1, 4, 15, true, 0), BambooTickResult::NoChange);
        assert_eq!(eval(false, 0, 4, 15, false, 0), BambooTickResult::NoChange);

        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let sapling = reg.state_of("minecraft:bamboo_sapling", None);
        let bamboo = reg.state_of("minecraft:bamboo", None);
        if reg.name_of(sapling) != "minecraft:bamboo_sapling"
            || reg.name_of(bamboo) != "minecraft:bamboo"
        {
            return;
        }
        let mut cells = Cells::new();

        // High above the surface so the growth cell is sky-lit; a sapling
        // turns into a small-leaved stalk on the one-in-three roll.
        cells.set(0, 149, 0, reg.state_of("minecraft:dirt", None));
        cells.set(0, 150, 0, sapling);
        let mut grew = false;
        for _ in 0..64 {
            bamboo_random_tick(reg, DimId::Overworld, 0, 150, 0, block_at(0, 150, 0));
            if reg.name_of(block_at(0, 150, 0)) == "minecraft:bamboo" {
                grew = true;
                break;
            }
        }
        assert!(grew, "sapling must transition to a bamboo stalk");
        assert_eq!(prop(reg, block_at(0, 150, 0), "leaves").as_deref(), Some("small"));

        // A stage-0 stalk grows a new top with leaves=large; the old top
        // relaxes to leaves=small.
        cells.set(4, 149, 0, reg.state_of("minecraft:dirt", None));
        cells.set(4, 150, 0, reg.with_prop(bamboo, "stage", "0"));
        let mut grew = false;
        for _ in 0..64 {
            bamboo_random_tick(reg, DimId::Overworld, 4, 150, 0, block_at(4, 150, 0));
            if reg.name_of(block_at(4, 151, 0)) == "minecraft:bamboo" {
                grew = true;
                break;
            }
        }
        assert!(grew, "a stage-0 stalk must grow a new top");
        assert_eq!(prop(reg, block_at(4, 151, 0), "leaves").as_deref(), Some("large"));
        assert_eq!(prop(reg, block_at(4, 150, 0), "leaves").as_deref(), Some("small"));

        // A 16-high column is capped.
        cells.set(8, 149, 0, reg.state_of("minecraft:dirt", None));
        for y in 135..=150 {
            cells.set(8, y, 0, bamboo);
        }
        for _ in 0..64 {
            bamboo_random_tick(reg, DimId::Overworld, 8, 150, 0, block_at(8, 150, 0));
        }
        assert!(
            reg.is_air(block_at(8, 151, 0)),
            "a 16-high bamboo column must not grow"
        );
    }

    #[test]
    fn p2_parity_kelp_grows_into_water_only_and_ages_out_at_25() {
        let eval = |age: u8, water: bool| {
            eval_kelp_random_tick(KelpTickInput {
                age,
                above_is_water_source: water,
            })
        };
        assert_eq!(eval(0, true), KelpTickResult::GrowIntoWater { new_age: 1 });
        assert_eq!(eval(24, true), KelpTickResult::GrowIntoWater { new_age: 25 });
        assert_eq!(eval(25, true), KelpTickResult::NoChange);
        assert_eq!(eval(0, false), KelpTickResult::NoChange);

        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let kelp = reg.state_of("minecraft:kelp", None);
        if reg.name_of(kelp) != "minecraft:kelp" {
            return;
        }
        let water = reg.state_of("minecraft:water", None);
        let mut cells = Cells::new();

        // A young head under a water source grows; the old head becomes body.
        cells.set(0, 100, 0, reg.with_prop(kelp, "age", "5"));
        cells.set(0, 101, 0, water);
        kelp_random_tick(reg, DimId::Overworld, 0, 100, 0, block_at(0, 100, 0));
        assert_eq!(reg.name_of(block_at(0, 101, 0)), "minecraft:kelp");
        assert_eq!(prop(reg, block_at(0, 101, 0), "age").as_deref(), Some("6"));

        // Age 25 is the cap, and air above is not a growth cell.
        cells.set(4, 100, 0, reg.with_prop(kelp, "age", "25"));
        cells.set(4, 101, 0, water);
        kelp_random_tick(reg, DimId::Overworld, 4, 100, 0, block_at(4, 100, 0));
        assert_eq!(
            reg.name_of(block_at(4, 101, 0)),
            "minecraft:water",
            "age-25 kelp must not grow"
        );
        cells.set(8, 100, 0, reg.with_prop(kelp, "age", "5"));
        cells.record(8, 101, 0);
        kelp_random_tick(reg, DimId::Overworld, 8, 100, 0, block_at(8, 100, 0));
        assert!(
            reg.is_air(block_at(8, 101, 0)),
            "kelp must not grow into air"
        );
    }

    #[test]
    fn p2_parity_vine_spread_obeys_do_vines_spread() {
        let eval = |allowed: bool, roll: u8, below: bool, side: bool| {
            eval_vine_random_tick(VineTickInput {
                spread_allowed: allowed,
                random_roll: roll,
                space_below: below,
                side_can_attach: side,
            })
        };
        assert_eq!(eval(false, 0, true, false), VineTickResult::NoChange);
        assert_eq!(eval(true, 1, true, true), VineTickResult::NoChange);
        assert_eq!(eval(true, 0, true, true), VineTickResult::SpreadDown);
        assert_eq!(eval(true, 0, false, true), VineTickResult::SpreadSide);
        assert_eq!(eval(true, 0, false, false), VineTickResult::NoChange);

        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _rules = Rules::take(&["doVinesSpread"]);
        let reg = crate::block_registry::breg().unwrap();
        let vine = reg.state_of("minecraft:vine", None);
        if reg.name_of(vine) != "minecraft:vine" {
            return;
        }
        let mut cells = Cells::new();

        // doVinesSpread=false: a hanging vine never creeps down.
        gamerule_atomic_store("doVinesSpread", 0);
        cells.set(0, 100, 0, vine);
        cells.record(0, 99, 0);
        for _ in 0..64 {
            vine_random_tick(reg, DimId::Overworld, 0, 100, 0, block_at(0, 100, 0));
        }
        assert!(
            reg.is_air(block_at(0, 99, 0)),
            "doVinesSpread=false must block every spread attempt"
        );

        // doVinesSpread=true: the same vine creeps down on the one-in-four
        // roll.
        gamerule_atomic_store("doVinesSpread", 1);
        let mut spread = false;
        for _ in 0..64 {
            vine_random_tick(reg, DimId::Overworld, 0, 100, 0, block_at(0, 100, 0));
            if reg.name_of(block_at(0, 99, 0)) == "minecraft:vine" {
                spread = true;
                break;
            }
        }
        assert!(spread, "doVinesSpread=true must let the vine creep down");
    }

    #[test]
    fn p2_parity_plant_tick_driver_claims_only_the_four_blocks() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        // The driver claims the four P2 plants and nothing else; on a live
        // cell it also performs the block's own tick.
        for name in [
            "minecraft:cactus",
            "minecraft:bamboo",
            "minecraft:bamboo_sapling",
            "minecraft:kelp",
            "minecraft:vine",
        ] {
            let st = reg.state_of(name, None);
            if reg.name_of(st) != name {
                continue;
            }
            assert!(
                p2_plant_random_tick(reg, DimId::Overworld, 64, 200, 64, st),
                "{name} must be claimed by the P2 random-tick arm"
            );
        }
        let stone = reg.state_of("minecraft:stone", None);
        assert!(
            !p2_plant_random_tick(reg, DimId::Overworld, 64, 200, 64, stone),
            "unrelated blocks must fall through to other dispatch arms"
        );
    }

    #[test]
    fn p2_parity_random_tick_dispatch_classifies_the_new_plants() {
        let sections = [RandomTickSection {
            chunk_x: 0,
            section_y: 0,
            chunk_z: 0,
        }];
        let names = [
            "minecraft:cactus",
            "minecraft:bamboo",
            "minecraft:bamboo_sapling",
            "minecraft:kelp",
            "minecraft:vine",
            "minecraft:stone",
        ];
        let mut index = 0usize;
        let samples = eval_random_ticks(&sections, 6, 7, |_| {
            let name = names[index % names.len()];
            index += 1;
            name.to_string()
        });
        let dispatches: Vec<RandomTickDispatch> =
            samples.iter().map(|sample| sample.dispatch).collect();
        assert_eq!(dispatches.len(), 6);
        assert_eq!(dispatches[0], RandomTickDispatch::Cactus);
        assert_eq!(dispatches[1], RandomTickDispatch::Bamboo);
        assert_eq!(dispatches[2], RandomTickDispatch::Bamboo);
        assert_eq!(dispatches[3], RandomTickDispatch::Kelp);
        assert_eq!(dispatches[4], RandomTickDispatch::Vine);
        assert_eq!(dispatches[5], RandomTickDispatch::Other);
    }

    #[test]
    fn p2_parity_source_conversion_rules_stay_live() {
        let _rules = Rules::take(&["waterSourceConversion", "lavaSourceConversion"]);
        gamerule_atomic_store("waterSourceConversion", 0);
        assert!(!source_conversion_enabled(flowing_fluid::Kind::Water));
        gamerule_atomic_store("waterSourceConversion", 1);
        assert!(source_conversion_enabled(flowing_fluid::Kind::Water));
        gamerule_atomic_store("lavaSourceConversion", 1);
        assert!(source_conversion_enabled(flowing_fluid::Kind::Lava));
        gamerule_atomic_store("lavaSourceConversion", 0);
        assert!(!source_conversion_enabled(flowing_fluid::Kind::Lava));
        // And the unified live read agrees with the atomics.
        gamerule_atomic_store("waterSourceConversion", 0);
        assert_eq!(gamerule_live_value("waterSourceConversion"), 0);
        gamerule_atomic_store("waterSourceConversion", 1);
        assert_eq!(gamerule_live_value("waterSourceConversion"), 1);
    }

    #[test]
    fn p2_parity_explosion_drop_decay_rules_and_roll() {
        let _stored = StoredRules::take(&[
            "blockExplosionDropDecay",
            "mobExplosionDropDecay",
            "tntExplosionDropDecay",
        ]);
        _stored.set("blockExplosionDropDecay", 0);
        assert!(!explosion_drop_decay_enabled(ExplosionDropKind::Block));
        _stored.set("blockExplosionDropDecay", 1);
        assert!(explosion_drop_decay_enabled(ExplosionDropKind::Block));
        _stored.set("mobExplosionDropDecay", 0);
        assert!(!explosion_drop_decay_enabled(ExplosionDropKind::Mob));
        _stored.set("tntExplosionDropDecay", 1);
        assert!(explosion_drop_decay_enabled(ExplosionDropKind::Tnt));
        _stored.set("tntExplosionDropDecay", 0);
        assert!(!explosion_drop_decay_enabled(ExplosionDropKind::Tnt));

        // DESTROY drops everything; DESTROY_WITH_DECAY rolls under 1/power.
        assert!(eval_explosion_block_drop(false, 4.0, 0.99));
        assert!(eval_explosion_block_drop(true, 4.0, 0.2));
        assert!(!eval_explosion_block_drop(true, 4.0, 0.3));
        assert!(eval_explosion_block_drop(true, 0.5, 0.99));
        // `ExplosionCondition#test` is `nextFloat() <= 1.0F / radius`, so the
        // exact boundary still drops.
        assert!(eval_explosion_block_drop(true, 4.0, 0.25));
        assert!(!eval_explosion_block_drop(true, 4.0, 0.26));
        assert!(eval_explosion_block_drop(true, 2.0, 0.5));
        assert!(!eval_explosion_block_drop(true, 2.0, 0.5001));
    }

    #[test]
    fn p2_parity_world_griefing_rule_reads_the_live_atomic() {
        let _rules = Rules::take(&["mobGriefing"]);
        gamerule_atomic_store("mobGriefing", 0);
        assert!(!world_griefing_enabled());
        gamerule_atomic_store("mobGriefing", 1);
        assert!(world_griefing_enabled());
    }

    #[test]
    fn p2_parity_advancement_rewards_match_vanilla_and_fire_once() {
        // Vanilla 1.21.1 payouts are experience-only: every advancement that
        // carries a `rewards` block pays XP and nothing else.
        for (id, xp) in VANILLA_ADVANCEMENT_EXPERIENCE {
            let spec = advancement_reward_spec(id).unwrap_or_else(|| panic!("{id} reward"));
            assert_eq!(spec.experience, *xp, "{id} experience");
            assert!(spec.recipes.is_empty(), "{id} unlocks no recipes in vanilla");
            assert!(spec.loot.is_empty(), "{id} rolls no loot in vanilla");
            assert!(spec.function.is_none(), "{id} runs no function in vanilla");
        }
        // No `story/*` JSON carries a `rewards` block, so a milestone pays no
        // XP and unlocks no recipes.
        for id in [
            "minecraft:story/root",
            "minecraft:story/mine_stone",
            "minecraft:story/smelt_iron",
            "minecraft:story/obtain_armor",
            "minecraft:story/lava_bucket",
            "minecraft:story/form_obsidian",
            "minecraft:story/enter_the_nether",
        ] {
            assert!(
                advancement_reward_spec(id).is_none(),
                "{id} pays nothing in vanilla"
            );
        }
        // `Advancement.performRewards` runs on the false->true completion edge
        // only: the second grant of a completed definition adds no criteria, so
        // `live_grant_advancement_rewards` cannot pay twice.
        let mut tracker = AdvancementTracker::default();
        let id = "minecraft:husbandry/balanced_diet";
        let definition = tracker.definition(id).unwrap();
        assert!(
            !tracker.criteria_complete("Alex", definition),
            "no completion edge before the first grant"
        );
        assert!(tracker.grant_only("Alex", id, 0) > 0);
        let definition = tracker.definition(id).unwrap();
        assert!(
            tracker.criteria_complete("Alex", definition),
            "the first grant takes the completion edge"
        );
        assert_eq!(
            tracker.grant_only("Alex", id, 0),
            0,
            "a regrant must not take a second edge"
        );
    }
}

// ------------------------------------------------- O(1) sleeping hopper engine
//
// Inventory and suction changes wake the hoppers that can see them.
// A settled array has no awake positions to visit.

/// Vanilla `HopperBlockEntity.setCooldown(8)`: one item per eight ticks.
const HOPPER_TRANSFER_COOLDOWN: i64 = 8;

/// Vanilla hopper inventory size.
const HOPPER_INVENTORY_SIZE: usize = 5;

/// The `facing` block-state direction a hopper pushes toward — the same 0..4
/// encoding `hopper_tick` reads out of block states 9225..=9229. A hopper
/// never points up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HopperFacing {
    Down,
    North,
    South,
    West,
    East,
}

impl HopperFacing {
    /// Unit vector from the hopper toward its push target.
    fn offset(self) -> BlockPos {
        let [down, _, north, south, west, east] = LEAF_NEIGHBORS;
        match self {
            Self::Down => down,
            Self::North => north,
            Self::South => south,
            Self::West => west,
            Self::East => east,
        }
    }
}

#[cfg(test)]
struct SleepingHopper {
    facing: HopperFacing,
    redstone_locked: bool,
    /// Vanilla `cooldownTime`: decremented every tick, attempts run at <= 0,
    /// reset to [`HOPPER_TRANSFER_COOLDOWN`] on a successful transfer.
    cooldown_ticks: i64,
    /// The hopper's own five vanilla inventory slots.
    slots: Vec<Option<Stack>>,
}

#[cfg(test)]
impl SleepingHopper {
    fn new(facing: HopperFacing) -> Self {
        Self {
            facing,
            redstone_locked: false,
            cooldown_ticks: 0,
            slots: vec![None; HOPPER_INVENTORY_SIZE],
        }
    }

    fn above(pos: DimBlockPos) -> DimBlockPos {
        (pos.0, pos.1, pos.2 + 1, pos.3)
    }

    fn target(pos: DimBlockPos, facing: HopperFacing) -> DimBlockPos {
        let (dx, dy, dz) = facing.offset();
        (pos.0, pos.1 + dx, pos.2 + dy, pos.3 + dz)
    }
}

/// Per-tick accounting proving the sleep is real: how many hoppers ran the
/// scan path, how many were skipped while asleep, and how many items moved.
/// `slot_reads` counts individual slot inspections — a fully settled array
/// reports zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(test)]
struct HopperTickStats {
    active_scanned: u64,
    sleeping_skipped: u64,
    items_transferred: u64,
    slot_reads: u64,
}

/// The scheduler. It owns a world image of the inventories it can touch —
/// hopper slots live on their [`SleepingHopper`], other block containers in
/// `containers`, and item entities waiting in suction cells in `suction` — so
/// every inventory mutation can wake its dependent hoppers here.
#[derive(Default)]
#[cfg(test)]
struct SleepingHopperEngine {
    awake: std::collections::BTreeSet<DimBlockPos>,
    hoppers: BTreeMap<DimBlockPos, SleepingHopper>,
    containers: HashMap<DimBlockPos, Vec<Option<Stack>>>,
    /// Item entities resting in each hopper's suction cell, keyed by hopper pos.
    suction: HashMap<DimBlockPos, Vec<Stack>>,
    /// Slot inspections performed inside transfer attempts — the counter the
    /// "0 slot scans while asleep" claim reads.
    slot_reads: u64,
}

#[cfg(test)]
impl SleepingHopperEngine {
    /// Place a hopper. It starts awake at cooldown 0: the world around it may
    /// already hold items, which is exactly what the first tick must look at.
    fn place_hopper(&mut self, pos: DimBlockPos, facing: HopperFacing) {
        self.hoppers.insert(pos, SleepingHopper::new(facing));
        self.awake.insert(pos);
        // The new inventory also wakes its neighbours.
        self.notify_container_changed(pos);
    }

    fn remove_hopper(&mut self, pos: DimBlockPos) -> bool {
        self.suction.remove(&pos);
        self.awake.remove(&pos);
        let removed = self.hoppers.remove(&pos).is_some();
        if removed {
            // The block changed for neighbours: a hopper above may have been
            // pushing into it, one below pulling from it.
            self.notify_container_changed(pos);
        }
        removed
    }

    /// Install or replace a non-hopper container's slot array and wake watchers.
    fn set_container(&mut self, pos: DimBlockPos, slots: Vec<Option<Stack>>) {
        self.containers.insert(pos, slots);
        self.notify_container_changed(pos);
    }

    fn remove_container(&mut self, pos: DimBlockPos) -> bool {
        let removed = self.containers.remove(&pos).is_some();
        if removed {
            self.notify_container_changed(pos);
        }
        removed
    }

    /// Write one inventory slot — container or hopper; `mutate_slots`
    /// resolves the store. This is the world's mutation point — every write
    /// wakes the hoppers that can see this inventory.
    fn set_slot(&mut self, pos: DimBlockPos, index: usize, stack: Option<Stack>) -> bool {
        self.mutate_slots(pos, |slots| {
            let Some(slot) = slots.get_mut(index) else {
                return (false, false);
            };
            *slot = stack;
            (true, true)
        })
        .unwrap_or(false)
    }

    /// Slots at `pos`, whichever store owns them: a hopper's own inventory
    /// first, then the container map (hoppers are containers to other hoppers,
    /// which is what makes a chain of hoppers work at all).
    fn slots_of(&self, pos: DimBlockPos) -> Option<&[Option<Stack>]> {
        if let Some(hopper) = self.hoppers.get(&pos) {
            Some(&hopper.slots)
        } else {
            self.containers.get(&pos).map(Vec::as_slice)
        }
    }

    /// Read one slot, counting the read — `slot_reads` is the number that
    /// must stay at zero while an array sleeps.
    fn read_slot(&mut self, pos: DimBlockPos, index: usize) -> Option<&Stack> {
        self.slot_reads += 1;
        self.slots_of(pos).and_then(|slots| slots.get(index)).and_then(Option::as_ref)
    }

    /// Inventory changes wake the hopper here, the one pulling from above,
    /// and adjacent hoppers pushing toward this position.
    fn notify_container_changed(&mut self, pos: DimBlockPos) {
        let (dim, x, y, z) = pos;
        let neighbors = LEAF_NEIGHBORS.into_iter().map(|(dx, dy, dz)| (dim, x + dx, y + dy, z + dz));
        for candidate in std::iter::once(pos).chain(neighbors) {
            if let Some(hopper) = self.hoppers.get(&candidate) {
                if candidate == pos
                    || SleepingHopper::above(candidate) == pos
                    || SleepingHopper::target(candidate, hopper.facing) == pos
                {
                    self.awake.insert(candidate);
                }
            }
        }
    }

    /// An item entity entering the suction cell wakes its hopper.
    fn notify_item_entered_cell(&mut self, hopper_pos: DimBlockPos, stack: Stack) {
        self.suction.entry(hopper_pos).or_default().push(stack);
        if self.hoppers.contains_key(&hopper_pos) {
            self.awake.insert(hopper_pos);
        }
    }

    /// Unlocking wakes the hopper. Powered hoppers still count down.
    fn set_redstone_locked(&mut self, pos: DimBlockPos, locked: bool) {
        if let Some(hopper) = self.hoppers.get_mut(&pos) {
            hopper.redstone_locked = locked;
            if !locked {
                self.awake.insert(pos);
            }
        }
    }

    /// One item of `stack` into `slots`: merge onto a same-item partial first
    /// (vanilla `addItem` order), else the first empty slot.
    fn insert_one(slots: &mut [Option<Stack>], item: i64, comps: &Comps, max: u8) -> bool {
        if let Some(slot) = slots.iter_mut().find(|slot| {
            matches!(slot, Some((it, count, c)) if *it == item && *c == *comps && *count < max)
        }) {
            if let Some(stack) = slot {
                stack.1 += 1;
            }
            return true;
        }
        if let Some(slot) = slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((item, 1, comps.clone()));
            return true;
        }
        false
    }

    /// Resolve the same inventory as `slots_of` and wake watchers after a write.
    fn mutate_slots<R>(
        &mut self,
        pos: DimBlockPos,
        f: impl FnOnce(&mut Vec<Option<Stack>>) -> (R, bool),
    ) -> Option<R> {
        let slots = match self.hoppers.get_mut(&pos) {
            Some(hopper) => &mut hopper.slots,
            None => self.containers.get_mut(&pos)?,
        };
        let (out, changed) = f(slots);
        if changed {
            self.notify_container_changed(pos);
        }
        Some(out)
    }

    fn insert_at(&mut self, pos: DimBlockPos, item: i64, comps: &Comps, max: u8) -> bool {
        self.mutate_slots(pos, |slots| {
            let inserted = Self::insert_one(slots, item, comps, max);
            (inserted, inserted)
        })
        .unwrap_or(false)
    }

    /// Remove one item from slot `index` at `pos` — a prior read found it
    /// non-empty, so the decrement always lands.
    fn take_one(&mut self, pos: DimBlockPos, index: usize) {
        self.mutate_slots(pos, |slots| {
            if let Some(stack) = slots.get_mut(index).and_then(Option::as_mut) {
                stack.1 -= 1;
                if stack.1 == 0 {
                    slots[index] = None;
                }
            }
            ((), true)
        });
    }

    /// Push: the first non-empty own slot loses one item into the facing
    /// container. Vanilla `ejectItems` — runs before any pull.
    fn attempt_push(&mut self, pos: DimBlockPos, target: DimBlockPos) -> bool {
        if self.slots_of(target).is_none() {
            return false;
        }
        let own_len = self.hoppers.get(&pos).map(|h| h.slots.len()).unwrap_or(0);
        for index in 0..own_len {
            let Some((item, _, comps)) = self.read_slot(pos, index).cloned() else {
                continue;
            };
            let max = item_stack_max(item, &comps).clamp(1, 255) as u8;
            if self.insert_at(target, item, &comps, max) {
                self.take_one(pos, index);
                return true;
            }
        }
        false
    }

    /// Pull: one item from the first extractable slot of the container above
    /// into the hopper's own inventory. Vanilla `suckInItems`' container half:
    /// source slots are tried in order until one can be placed.
    fn attempt_pull(&mut self, pos: DimBlockPos, above: DimBlockPos) -> bool {
        let Some(above_len) = self.slots_of(above).map(<[_]>::len) else {
            return false;
        };
        let own_len = self.hoppers.get(&pos).map(|h| h.slots.len()).unwrap_or(0);
        for index in 0..above_len {
            let Some((item, _, comps)) = self.read_slot(above, index).cloned() else {
                continue;
            };
            let max = item_stack_max(item, &comps).clamp(1, 255) as u8;
            let mut room = false;
            for own_index in 0..own_len {
                match self.read_slot(pos, own_index) {
                    None => {
                        room = true;
                        break;
                    }
                    Some((it, count, c)) if *it == item && *c == comps && *count < max => {
                        room = true;
                        break;
                    }
                    _ => {}
                }
            }
            if !room {
                continue;
            }
            // Room was just verified, so the insert cannot fail — take first,
            // then place, and a bug that breaks that pairing debug-asserts.
            self.take_one(above, index);
            let inserted = self.insert_at(pos, item, &comps, max);
            debug_assert!(inserted, "pull destination lost room between probe and insert");
            return true;
        }
        false
    }

    /// Suck: pending item entities in the cell are inserted into the hopper's
    /// inventory, as much of the stack as fits, like vanilla
    /// `addItem(Hopper, ItemEntity)` — entities are tried in order until one
    /// fits, one entity per activation.
    fn attempt_suck(&mut self, pos: DimBlockPos) -> u64 {
        let pending = self.suction.get(&pos).map(|items| items.len()).unwrap_or(0);
        for index in 0..pending {
            let Some((item, mut count, comps)) = self
                .suction
                .get(&pos)
                .and_then(|items| items.get(index))
                .cloned()
            else {
                continue;
            };
            let max = item_stack_max(item, &comps).clamp(1, 255) as u8;
            let moved = self
                .mutate_slots(pos, |slots| {
                    let mut moved = 0u64;
                    while count > 0 && Self::insert_one(slots, item, &comps, max) {
                        count -= 1;
                        moved += 1;
                    }
                    (moved, moved > 0)
                })
                .unwrap_or(0);
            if moved == 0 {
                continue;
            }
            let items = self.suction.get_mut(&pos).unwrap();
            if count == 0 {
                items.remove(index);
            } else {
                items[index].1 = count;
            }
            if items.is_empty() {
                self.suction.remove(&pos);
            }
            return moved;
        }
        0
    }

    /// Vanilla `tryMoveItems`: `ejectItems | supplier` is an *eager* OR — a
    /// hopper with a buffered item and a non-empty source does both in one
    /// activation (the push first). The supplier half is `suckInItems`: pull
    /// from the container above, else item entities in the cell.
    fn attempt_transfers(&mut self, pos: DimBlockPos, facing: HopperFacing) -> u64 {
        let target = SleepingHopper::target(pos, facing);
        let above = SleepingHopper::above(pos);
        let mut moved = u64::from(self.attempt_push(pos, target));
        if self.attempt_pull(pos, above) {
            moved += 1;
        } else {
            moved += self.attempt_suck(pos);
        }
        moved
    }

    /// Advance awake hoppers in block-position order. A transfer can wake a
    /// later position for this tick, or an earlier one for the next tick.
    fn tick(&mut self) -> HopperTickStats {
        use std::ops::Bound::{Excluded, Unbounded};

        let mut stats = HopperTickStats {
            sleeping_skipped: self.hoppers.len() as u64,
            ..HopperTickStats::default()
        };
        self.slot_reads = 0;
        let mut lower = Unbounded;
        while let Some(pos) = self.awake.range((lower, Unbounded)).next().copied() {
            lower = Excluded(pos);
            stats.sleeping_skipped -= 1;
            let hopper = self.hoppers.get_mut(&pos).expect("awake hopper must exist");
            hopper.cooldown_ticks -= 1;
            if hopper.redstone_locked || hopper.cooldown_ticks > 0 {
                continue;
            }
            let facing = hopper.facing;
            stats.active_scanned += 1;
            let moved = self.attempt_transfers(pos, facing);
            if moved > 0 {
                stats.items_transferred += moved;
                self.hoppers.get_mut(&pos).unwrap().cooldown_ticks = HOPPER_TRANSFER_COOLDOWN;
            } else {
                self.awake.remove(&pos);
            }
        }
        stats.slot_reads = self.slot_reads;
        stats
    }
}

#[cfg(test)]
mod perf1_sleeping_hopper_tests {
    use super::*;

    const DIM: DimId = DimId::Overworld;
    const IRON: i64 = 811; // minecraft:iron_ingot in the item table

    fn stack(count: u8) -> Stack {
        (IRON, count, Vec::new())
    }

    fn pos(x: i32, y: i32, z: i32) -> DimBlockPos {
        (DIM, x, y, z)
    }

    /// A classic sorting-system row: `n` hoppers at y=64, each fed by an input
    /// chest directly above and pushing Down into its own output chest below.
    /// No hopper feeds another hopper, so wakes are attributable one-to-one.
    fn sorting_array(n: i32) -> SleepingHopperEngine {
        let mut engine = SleepingHopperEngine::default();
        for x in 0..n {
            engine.place_hopper(pos(x, 64, 0), HopperFacing::Down);
            engine.set_container(pos(x, 65, 0), vec![None; 27]);
            engine.set_container(pos(x, 63, 0), vec![None; 27]);
        }
        engine
    }

    fn settle(engine: &mut SleepingHopperEngine, ticks: usize) {
        for _ in 0..ticks {
            engine.tick();
        }
    }

    fn container_item_count(engine: &SleepingHopperEngine, at: DimBlockPos) -> u64 {
        engine
            .slots_of(at)
            .map(|slots| {
                slots
                    .iter()
                    .flatten()
                    .map(|(_, count, _)| u64::from(*count))
                    .sum()
            })
            .unwrap_or(0)
    }

    #[test]
    fn perf1_idle_1000_hopper_sorting_array_sleeps_with_zero_slot_reads() {
        let mut engine = sorting_array(1000);
        settle(&mut engine, 3);

        let stats = engine.tick();
        assert_eq!(
            stats.active_scanned, 0,
            "a settled array must run no scans at all"
        );
        assert_eq!(stats.sleeping_skipped, 1000);
        assert_eq!(stats.items_transferred, 0);
        assert_eq!(stats.slot_reads, 0, "sleeping means literally 0 slot scans");

        // Stays settled: one hundred more ticks, still zero work.
        for _ in 0..100 {
            let stats = engine.tick();
            assert_eq!(stats.active_scanned, 0);
            assert_eq!(stats.sleeping_skipped, 1000);
            assert_eq!(stats.slot_reads, 0);
        }
    }

    #[test]
    fn perf1_hopper_wake_during_tick_preserves_position_order() {
        for (facing, step, moves_on_push_tick) in
            [(HopperFacing::East, 1, 2), (HopperFacing::West, -1, 1)]
        {
            let mut engine = SleepingHopperEngine::default();
            let (source, next, output) = (pos(0, 64, 0), pos(step, 64, 0), pos(2 * step, 64, 0));
            engine.place_hopper(source, facing);
            engine.place_hopper(next, facing);
            engine.set_container(output, vec![None]);
            assert_eq!(engine.tick().active_scanned, 2);
            assert_eq!(engine.tick().sleeping_skipped, 2);

            assert!(engine.set_slot(source, 0, Some(stack(1))));
            assert_eq!(engine.tick().items_transferred, moves_on_push_tick);
            assert_eq!(container_item_count(&engine, output), moves_on_push_tick - 1);
            assert_eq!(engine.tick().items_transferred, 2 - moves_on_push_tick);
            assert_eq!(container_item_count(&engine, output), 1);
        }
    }

    #[test]
    fn perf1_chest_insert_wakes_only_the_adjacent_hopper() {
        let mut engine = sorting_array(1000);
        settle(&mut engine, 3);
        assert_eq!(engine.tick().sleeping_skipped, 1000);

        // One item lands in input chest #42.
        assert!(engine.set_slot(pos(42, 65, 0), 0, Some(stack(3))));
        let stats = engine.tick();
        assert_eq!(
            stats.active_scanned, 1,
            "only hopper #42 may wake — the other 999 stay asleep"
        );
        assert_eq!(stats.sleeping_skipped, 999);
        assert_eq!(stats.items_transferred, 1, "the wake tick pulls one item");

        // The pull went chest -> hopper inventory.
        assert_eq!(container_item_count(&engine, pos(42, 65, 0)), 2);
        assert_eq!(container_item_count(&engine, pos(42, 64, 0)), 1);
        assert_eq!(container_item_count(&engine, pos(42, 63, 0)), 0);

        // Vanilla moves at the exact 8-tick cadence, and each activation does
        // the *eager* push+pull pair once the buffer is primed: t8 pushes the
        // pulled item down AND pulls item two (2 items), t16 the same (2),
        // t24 pushes the last item (1).
        for _ in 0..7 {
            assert_eq!(engine.tick().items_transferred, 0, "cooldown must hold");
        }
        assert_eq!(engine.tick().items_transferred, 2);
        assert_eq!(container_item_count(&engine, pos(42, 63, 0)), 1);
        assert_eq!(container_item_count(&engine, pos(42, 64, 0)), 1);
        for _ in 0..7 {
            assert_eq!(engine.tick().items_transferred, 0);
        }
        assert_eq!(engine.tick().items_transferred, 2);
        for _ in 0..7 {
            assert_eq!(engine.tick().items_transferred, 0);
        }
        assert_eq!(engine.tick().items_transferred, 1);
        assert_eq!(container_item_count(&engine, pos(42, 65, 0)), 0);
        assert_eq!(container_item_count(&engine, pos(42, 63, 0)), 3);
        assert_eq!(container_item_count(&engine, pos(42, 64, 0)), 0);

        // Drained: the last push put hopper 42 on cooldown — after those 8
        // ticks its empty-handed scan fails and the whole array re-sleeps.
        settle(&mut engine, 12);
        let stats = engine.tick();
        assert_eq!(stats.active_scanned, 0);
        assert_eq!(stats.sleeping_skipped, 1000);
        assert_eq!(stats.slot_reads, 0);
    }

    #[test]
    fn perf1_hopper_chain_matches_vanilla_8_tick_order_exactly() {
        // chest -> hopper -> chest: three items must move one per activation,
        // push first then pull, at the vanilla 8-tick cooldown.
        let mut engine = SleepingHopperEngine::default();
        let chest = pos(0, 66, 0);
        let hopper = pos(0, 65, 0);
        let output = pos(0, 64, 0);
        engine.place_hopper(hopper, HopperFacing::Down);
        engine.set_container(chest, vec![Some(stack(3)), None]);
        engine.set_container(output, vec![None; 27]);

        // Push order: seed the hopper's own slot — vanilla ejects that first
        // even with a full source above it.
        engine
            .hoppers
            .get_mut(&hopper)
            .unwrap()
            .slots[0] = Some(stack(1));

        let mut pull_ticks = Vec::new();
        let mut push_ticks = Vec::new();
        for tick in 0..40 {
            let before_above = container_item_count(&engine, chest);
            let before_out = container_item_count(&engine, output);
            engine.tick();
            if container_item_count(&engine, chest) < before_above {
                pull_ticks.push(tick);
            }
            if container_item_count(&engine, output) > before_out {
                push_ticks.push(tick);
            }
        }
        // Activation ticks: t0 pushes the seeded own item AND pulls item one
        // (the eager pair); t8 pushes item one and pulls item two; t16 pushes
        // two and pulls three; t24 pushes three and the chest is drained.
        assert_eq!(pull_ticks, vec![0, 8, 16], "pull cadence");
        assert_eq!(push_ticks, vec![0, 8, 16, 24], "push cadence");
        assert_eq!(container_item_count(&engine, chest), 0);
        assert_eq!(container_item_count(&engine, output), 4);
        assert_eq!(container_item_count(&engine, hopper), 0);
    }

    #[test]
    fn perf1_redstone_locked_hopper_holds_items_and_unlock_resumes() {
        let mut engine = SleepingHopperEngine::default();
        let chest = pos(0, 66, 0);
        let hopper = pos(0, 65, 0);
        let output = pos(0, 64, 0);
        engine.place_hopper(hopper, HopperFacing::Down);
        engine.set_container(chest, vec![Some(stack(2)), None]);
        engine.set_container(output, vec![None; 27]);

        engine.set_redstone_locked(hopper, true);
        for _ in 0..20 {
            let stats = engine.tick();
            assert_eq!(stats.items_transferred, 0, "a powered hopper holds items");
        }
        assert_eq!(container_item_count(&engine, chest), 2);

        engine.set_redstone_locked(hopper, false);
        let stats = engine.tick();
        assert_eq!(stats.items_transferred, 1, "unlock resumes on the next tick");
    }

    #[test]
    fn perf1_dropped_item_in_suction_cell_wakes_sleeping_hopper() {
        let mut engine = SleepingHopperEngine::default();
        let hopper = pos(0, 65, 0);
        let output = pos(0, 64, 0);
        engine.place_hopper(hopper, HopperFacing::Down);
        engine.set_container(output, vec![None; 27]);
        settle(&mut engine, 2);
        assert_eq!(engine.tick().sleeping_skipped, 1);
        assert!(!engine.awake.contains(&hopper));

        // A drop lands on the hopper: only that hopper wakes, and the entity
        // is sucked into its inventory on the wake tick.
        engine.notify_item_entered_cell(hopper, stack(5));
        assert!(engine.awake.contains(&hopper));
        let stats = engine.tick();
        assert_eq!(stats.items_transferred, 5, "the whole entity stack is sucked");
        assert_eq!(container_item_count(&engine, hopper), 5);

        // It flows out on the normal 8-tick cadence.
        for _ in 0..7 {
            assert_eq!(engine.tick().items_transferred, 0);
        }
        assert_eq!(engine.tick().items_transferred, 1);
    }

    #[test]
    fn perf1_sleeping_hopper_wakes_when_a_watched_container_is_removed() {
        let mut engine = sorting_array(4);
        settle(&mut engine, 2);
        assert_eq!(engine.tick().sleeping_skipped, 4);

        // Breaking the input chest above hopper 1 must wake it — vanilla would
        // re-scan and find the source gone.
        assert!(engine.remove_container(pos(1, 65, 0)));
        let stats = engine.tick();
        assert_eq!(stats.active_scanned, 1, "only hopper 1 re-scans");
        assert_eq!(stats.items_transferred, 0);
        // ... and having found nothing, it goes back to sleep.
        assert_eq!(engine.tick().sleeping_skipped, 4);
    }
}

#[cfg(test)]
mod perf1_sleeping_hopper_facing_tests {
    use super::*;

    const DIM: DimId = DimId::Overworld;
    const IRON: i64 = 811;

    fn pos(x: i32, y: i32, z: i32) -> DimBlockPos {
        (DIM, x, y, z)
    }

    #[test]
    fn perf1_every_facing_pushes_to_its_vanilla_target() {
        // One hopper per facing, each seeded with one item and a chest on the
        // matching side — the offset table is the wire facing order.
        let mut engine = SleepingHopperEngine::default();
        let cases = [
            (HopperFacing::Down, (0, -1, 0)),
            (HopperFacing::North, (0, 0, -1)),
            (HopperFacing::South, (0, 0, 1)),
            (HopperFacing::West, (-1, 0, 0)),
            (HopperFacing::East, (1, 0, 0)),
        ];
        for (i, (facing, (dx, dy, dz))) in cases.iter().enumerate() {
            let at = pos(i as i32 * 4, 64, 0);
            engine.place_hopper(at, *facing);
            let target = pos(at.1 + dx, at.2 + dy, at.3 + dz);
            engine.set_container(target, vec![None; 27]);
            engine.hoppers.get_mut(&at).unwrap().slots[0] = Some((IRON, 1, Vec::new()));
            engine.notify_container_changed(at);
        }
        engine.tick();
        for (i, (_, (dx, dy, dz))) in cases.iter().enumerate() {
            let at = pos(i as i32 * 4, 64, 0);
            let target = pos(at.1 + dx, at.2 + dy, at.3 + dz);
            let moved: u64 = engine
                .slots_of(target)
                .unwrap()
                .iter()
                .flatten()
                .map(|(_, c, _)| u64::from(*c))
                .sum();
            assert_eq!(moved, 1, "hopper at {at:?} pushed the wrong way");
        }
    }

    #[test]
    fn perf1_removing_a_hopper_wakes_the_hopper_pulling_from_it() {
        // Hopper B pulls from hopper A above it; A is then broken. Vanilla B
        // re-scans and finds the source gone — sleeping B must do the same.
        let mut engine = SleepingHopperEngine::default();
        let a = pos(0, 66, 0);
        let b = pos(0, 65, 0);
        let out = pos(0, 64, 0);
        engine.place_hopper(a, HopperFacing::Down);
        engine.place_hopper(b, HopperFacing::Down);
        engine.set_container(out, vec![None; 27]);
        // A pushes nowhere (no container below it other than B's inventory —
        // which it will fill): let the pair settle into sleep with A empty.
        for _ in 0..3 {
            engine.tick();
        }
        assert_eq!(engine.tick().sleeping_skipped, 2);

        assert!(engine.remove_hopper(a));
        let stats = engine.tick();
        assert_eq!(stats.active_scanned, 1, "only B re-scans when A vanishes");
        assert_eq!(engine.tick().sleeping_skipped, 1, "B settles again");
        assert!(!engine.remove_hopper(a), "second removal is a no-op");
    }
}

// ============================================================================
// VANILLA-BLOCKS lane — native mechanisms recovered from Minecraft 1.21.1:
// note blocks, cauldrons, composters, respawn anchors, lodestone compasses,
// and the sponge 65-block BFS water absorption.
// ============================================================================

/// Vanilla `NoteBlockInstrument` (1.21.1 `dud`), in the pinned `instrument`
/// state-property order: the 16 tunable instruments, six mob heads, then
/// `custom_head` for player heads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteBlockInstrument {
    Harp,
    Basedrum,
    Snare,
    Hat,
    Bass,
    Flute,
    Bell,
    Guitar,
    Chime,
    Xylophone,
    IronXylophone,
    CowBell,
    Didgeridoo,
    Bit,
    Banjo,
    Pling,
    Zombie,
    Skeleton,
    Creeper,
    Dragon,
    WitherSkeleton,
    Piglin,
    CustomHead,
}

impl NoteBlockInstrument {
    /// `note_block[instrument=...]` state value.
    pub(crate) fn state_name(self) -> &'static str {
        match self {
            NoteBlockInstrument::Harp => "harp",
            NoteBlockInstrument::Basedrum => "basedrum",
            NoteBlockInstrument::Snare => "snare",
            NoteBlockInstrument::Hat => "hat",
            NoteBlockInstrument::Bass => "bass",
            NoteBlockInstrument::Flute => "flute",
            NoteBlockInstrument::Bell => "bell",
            NoteBlockInstrument::Guitar => "guitar",
            NoteBlockInstrument::Chime => "chime",
            NoteBlockInstrument::Xylophone => "xylophone",
            NoteBlockInstrument::IronXylophone => "iron_xylophone",
            NoteBlockInstrument::CowBell => "cow_bell",
            NoteBlockInstrument::Didgeridoo => "didgeridoo",
            NoteBlockInstrument::Bit => "bit",
            NoteBlockInstrument::Banjo => "banjo",
            NoteBlockInstrument::Pling => "pling",
            NoteBlockInstrument::Zombie => "zombie",
            NoteBlockInstrument::Skeleton => "skeleton",
            NoteBlockInstrument::Creeper => "creeper",
            NoteBlockInstrument::Dragon => "dragon",
            NoteBlockInstrument::WitherSkeleton => "wither_skeleton",
            NoteBlockInstrument::Piglin => "piglin",
            NoteBlockInstrument::CustomHead => "custom_head",
        }
    }

    /// `NoteBlockInstrument.worksAboveNoteBlock()`: head instruments play the
    /// skull's ambient sound from the mob head sitting on the note block; the
    /// note block itself does not play a pitched note for them.
    pub(crate) fn works_above_note_block(self) -> bool {
        matches!(
            self,
            NoteBlockInstrument::Zombie
                | NoteBlockInstrument::Skeleton
                | NoteBlockInstrument::Creeper
                | NoteBlockInstrument::Dragon
                | NoteBlockInstrument::WitherSkeleton
                | NoteBlockInstrument::Piglin
                | NoteBlockInstrument::CustomHead
        )
    }

    /// `NoteBlockInstrument.isTunable()`: the sixteen player-tunable
    /// instruments — exactly the non-head instruments.
    #[cfg(test)]
    pub(crate) fn is_tunable(self) -> bool {
        !self.works_above_note_block()
    }
}

/// Vanilla `BlockState.instrument()` for 1.21.1: the per-block `instrument`
/// flag baked into `Blocks` (`dga`). Blocks not listed resolve to HARP, which
/// is also what wall heads produce (they never register an instrument).
pub(crate) fn note_block_instrument_from_below(block_name: &str) -> NoteBlockInstrument {
    match vanilla_id_path(block_name) {
            "andesite" | "andesite_slab" | "andesite_stairs" | "andesite_wall" | "basalt" | "bedrock" | "black_concrete" | "black_glazed_terracotta" | "black_terracotta" | "blackstone" | "blackstone_slab" | "blackstone_stairs" | "blackstone_wall" | "blast_furnace" | "blue_concrete" | "blue_glazed_terracotta" | "blue_terracotta" | "brain_coral_block" | "brick_slab" | "brick_stairs" | "brick_wall" | "bricks" | "brown_concrete" | "brown_glazed_terracotta" | "brown_terracotta" | "bubble_coral_block" | "calcite" | "chiseled_deepslate" | "chiseled_nether_bricks" | "chiseled_polished_blackstone" | "chiseled_quartz_block" | "chiseled_red_sandstone" | "chiseled_sandstone" | "chiseled_stone_bricks" | "chiseled_tuff" | "chiseled_tuff_bricks" | "coal_block" | "coal_ore" | "cobbled_deepslate" | "cobbled_deepslate_slab" | "cobbled_deepslate_stairs" | "cobbled_deepslate_wall" | "cobblestone" | "cobblestone_slab" | "cobblestone_stairs" | "cobblestone_wall" | "copper_ore" | "cracked_deepslate_bricks" | "cracked_deepslate_tiles" | "cracked_nether_bricks" | "cracked_polished_blackstone_bricks" | "cracked_stone_bricks" | "crimson_nylium" | "crying_obsidian" | "cut_red_sandstone" | "cut_red_sandstone_slab" | "cut_sandstone" | "cut_sandstone_slab" | "cyan_concrete" | "cyan_glazed_terracotta" | "cyan_terracotta" | "dark_prismarine" | "dark_prismarine_slab" | "dark_prismarine_stairs" | "dead_brain_coral" | "dead_brain_coral_block" | "dead_brain_coral_fan" | "dead_brain_coral_wall_fan" | "dead_bubble_coral" | "dead_bubble_coral_block" | "dead_bubble_coral_fan" | "dead_bubble_coral_wall_fan" | "dead_fire_coral" | "dead_fire_coral_block" | "dead_fire_coral_fan" | "dead_fire_coral_wall_fan" | "dead_horn_coral" | "dead_horn_coral_block" | "dead_horn_coral_fan" | "dead_horn_coral_wall_fan" | "dead_tube_coral" | "dead_tube_coral_block" | "dead_tube_coral_fan" | "dead_tube_coral_wall_fan" | "deepslate" | "deepslate_brick_slab" | "deepslate_brick_stairs" | "deepslate_brick_wall" | "deepslate_bricks" | "deepslate_coal_ore" | "deepslate_copper_ore" | "deepslate_diamond_ore" | "deepslate_emerald_ore" | "deepslate_gold_ore" | "deepslate_iron_ore" | "deepslate_lapis_ore" | "deepslate_redstone_ore" | "deepslate_tile_slab" | "deepslate_tile_stairs" | "deepslate_tile_wall" | "deepslate_tiles" | "diamond_ore" | "diorite" | "diorite_slab" | "diorite_stairs" | "diorite_wall" | "dispenser" | "dripstone_block" | "dropper" | "emerald_ore" | "enchanting_table" | "end_portal_frame" | "end_stone" | "end_stone_brick_slab" | "end_stone_brick_stairs" | "end_stone_brick_wall" | "end_stone_bricks" | "ender_chest" | "fire_coral_block" | "furnace" | "gilded_blackstone" | "gold_ore" | "granite" | "granite_slab" | "granite_stairs" | "granite_wall" | "gray_concrete" | "gray_glazed_terracotta" | "gray_terracotta" | "green_concrete" | "green_glazed_terracotta" | "green_terracotta" | "horn_coral_block" | "iron_ore" | "lapis_ore" | "light_blue_concrete" | "light_blue_glazed_terracotta" | "light_blue_terracotta" | "light_gray_concrete" | "light_gray_glazed_terracotta" | "light_gray_terracotta" | "lime_concrete" | "lime_glazed_terracotta" | "lime_terracotta" | "magenta_concrete" | "magenta_glazed_terracotta" | "magenta_terracotta" | "magma_block" | "mossy_cobblestone" | "mossy_cobblestone_slab" | "mossy_cobblestone_stairs" | "mossy_cobblestone_wall" | "mossy_stone_brick_slab" | "mossy_stone_brick_stairs" | "mossy_stone_brick_wall" | "mossy_stone_bricks" | "mud_brick_slab" | "mud_brick_stairs" | "mud_brick_wall" | "mud_bricks" | "nether_brick_fence" | "nether_brick_slab" | "nether_brick_stairs" | "nether_brick_wall" | "nether_bricks" | "nether_gold_ore" | "nether_quartz_ore" | "netherrack" | "observer" | "obsidian" | "orange_concrete" | "orange_glazed_terracotta" | "orange_terracotta" | "petrified_oak_slab" | "pink_concrete" | "pink_glazed_terracotta" | "pink_terracotta" | "pointed_dripstone" | "polished_andesite" | "polished_andesite_slab" | "polished_andesite_stairs" | "polished_basalt" | "polished_blackstone" | "polished_blackstone_brick_slab" | "polished_blackstone_brick_stairs" | "polished_blackstone_brick_wall" | "polished_blackstone_bricks" | "polished_blackstone_pressure_plate" | "polished_blackstone_slab" | "polished_blackstone_stairs" | "polished_blackstone_wall" | "polished_deepslate" | "polished_deepslate_slab" | "polished_deepslate_stairs" | "polished_deepslate_wall" | "polished_diorite" | "polished_diorite_slab" | "polished_diorite_stairs" | "polished_granite" | "polished_granite_slab" | "polished_granite_stairs" | "polished_tuff" | "polished_tuff_slab" | "polished_tuff_stairs" | "polished_tuff_wall" | "prismarine" | "prismarine_brick_slab" | "prismarine_brick_stairs" | "prismarine_bricks" | "prismarine_slab" | "prismarine_stairs" | "prismarine_wall" | "purple_concrete" | "purple_glazed_terracotta" | "purple_terracotta" | "purpur_block" | "purpur_pillar" | "purpur_slab" | "purpur_stairs" | "quartz_block" | "quartz_bricks" | "quartz_pillar" | "quartz_slab" | "quartz_stairs" | "raw_copper_block" | "raw_gold_block" | "raw_iron_block" | "red_concrete" | "red_glazed_terracotta" | "red_nether_brick_slab" | "red_nether_brick_stairs" | "red_nether_brick_wall" | "red_nether_bricks" | "red_sandstone" | "red_sandstone_slab" | "red_sandstone_stairs" | "red_sandstone_wall" | "red_terracotta" | "redstone_ore" | "reinforced_deepslate" | "respawn_anchor" | "sandstone" | "sandstone_slab" | "sandstone_stairs" | "sandstone_wall" | "smoker" | "smooth_basalt" | "smooth_quartz" | "smooth_quartz_slab" | "smooth_quartz_stairs" | "smooth_red_sandstone" | "smooth_red_sandstone_slab" | "smooth_red_sandstone_stairs" | "smooth_sandstone" | "smooth_sandstone_slab" | "smooth_sandstone_stairs" | "smooth_stone" | "smooth_stone_slab" | "spawner" | "stone" | "stone_brick_slab" | "stone_brick_stairs" | "stone_brick_wall" | "stone_bricks" | "stone_pressure_plate" | "stone_slab" | "stone_stairs" | "stonecutter" | "terracotta" | "trial_spawner" | "tube_coral_block" | "tuff" | "tuff_brick_slab" | "tuff_brick_stairs" | "tuff_brick_wall" | "tuff_bricks" | "tuff_slab" | "tuff_stairs" | "tuff_wall" | "vault" | "warped_nylium" | "white_concrete" | "white_glazed_terracotta" | "white_terracotta" | "yellow_concrete" | "yellow_glazed_terracotta" | "yellow_terracotta" => NoteBlockInstrument::Basedrum,
            "acacia_door" | "acacia_fence" | "acacia_fence_gate" | "acacia_hanging_sign" | "acacia_log" | "acacia_planks" | "acacia_pressure_plate" | "acacia_sign" | "acacia_slab" | "acacia_stairs" | "acacia_trapdoor" | "acacia_wall_hanging_sign" | "acacia_wall_sign" | "acacia_wood" | "bamboo_block" | "bamboo_door" | "bamboo_fence" | "bamboo_fence_gate" | "bamboo_hanging_sign" | "bamboo_mosaic" | "bamboo_mosaic_slab" | "bamboo_mosaic_stairs" | "bamboo_planks" | "bamboo_pressure_plate" | "bamboo_sign" | "bamboo_slab" | "bamboo_stairs" | "bamboo_trapdoor" | "bamboo_wall_hanging_sign" | "bamboo_wall_sign" | "barrel" | "bee_nest" | "beehive" | "birch_door" | "birch_fence" | "birch_fence_gate" | "birch_hanging_sign" | "birch_log" | "birch_planks" | "birch_pressure_plate" | "birch_sign" | "birch_slab" | "birch_stairs" | "birch_trapdoor" | "birch_wall_hanging_sign" | "birch_wall_sign" | "birch_wood" | "black_banner" | "black_wall_banner" | "blue_banner" | "blue_wall_banner" | "bookshelf" | "brown_banner" | "brown_mushroom_block" | "brown_wall_banner" | "campfire" | "cartography_table" | "cherry_door" | "cherry_fence" | "cherry_fence_gate" | "cherry_hanging_sign" | "cherry_log" | "cherry_planks" | "cherry_pressure_plate" | "cherry_sign" | "cherry_slab" | "cherry_stairs" | "cherry_trapdoor" | "cherry_wall_hanging_sign" | "cherry_wall_sign" | "cherry_wood" | "chest" | "chiseled_bookshelf" | "composter" | "crafting_table" | "crimson_door" | "crimson_fence" | "crimson_fence_gate" | "crimson_hanging_sign" | "crimson_hyphae" | "crimson_planks" | "crimson_pressure_plate" | "crimson_sign" | "crimson_slab" | "crimson_stairs" | "crimson_stem" | "crimson_trapdoor" | "crimson_wall_hanging_sign" | "crimson_wall_sign" | "cyan_banner" | "cyan_wall_banner" | "dark_oak_door" | "dark_oak_fence" | "dark_oak_fence_gate" | "dark_oak_hanging_sign" | "dark_oak_log" | "dark_oak_planks" | "dark_oak_pressure_plate" | "dark_oak_sign" | "dark_oak_slab" | "dark_oak_stairs" | "dark_oak_trapdoor" | "dark_oak_wall_hanging_sign" | "dark_oak_wall_sign" | "dark_oak_wood" | "daylight_detector" | "fletching_table" | "gray_banner" | "gray_wall_banner" | "green_banner" | "green_wall_banner" | "jukebox" | "jungle_door" | "jungle_fence" | "jungle_fence_gate" | "jungle_hanging_sign" | "jungle_log" | "jungle_planks" | "jungle_pressure_plate" | "jungle_sign" | "jungle_slab" | "jungle_stairs" | "jungle_trapdoor" | "jungle_wall_hanging_sign" | "jungle_wall_sign" | "jungle_wood" | "lectern" | "light_blue_banner" | "light_blue_wall_banner" | "light_gray_banner" | "light_gray_wall_banner" | "lime_banner" | "lime_wall_banner" | "loom" | "magenta_banner" | "magenta_wall_banner" | "mangrove_door" | "mangrove_fence" | "mangrove_fence_gate" | "mangrove_hanging_sign" | "mangrove_log" | "mangrove_planks" | "mangrove_pressure_plate" | "mangrove_roots" | "mangrove_sign" | "mangrove_slab" | "mangrove_stairs" | "mangrove_trapdoor" | "mangrove_wall_hanging_sign" | "mangrove_wall_sign" | "mangrove_wood" | "mushroom_stem" | "note_block" | "oak_door" | "oak_fence" | "oak_fence_gate" | "oak_hanging_sign" | "oak_log" | "oak_planks" | "oak_pressure_plate" | "oak_sign" | "oak_slab" | "oak_stairs" | "oak_trapdoor" | "oak_wall_hanging_sign" | "oak_wall_sign" | "oak_wood" | "orange_banner" | "orange_wall_banner" | "pink_banner" | "pink_wall_banner" | "purple_banner" | "purple_wall_banner" | "red_banner" | "red_mushroom_block" | "red_wall_banner" | "smithing_table" | "soul_campfire" | "spruce_door" | "spruce_fence" | "spruce_fence_gate" | "spruce_hanging_sign" | "spruce_log" | "spruce_planks" | "spruce_pressure_plate" | "spruce_sign" | "spruce_slab" | "spruce_stairs" | "spruce_trapdoor" | "spruce_wall_hanging_sign" | "spruce_wall_sign" | "spruce_wood" | "stripped_acacia_log" | "stripped_acacia_wood" | "stripped_bamboo_block" | "stripped_birch_log" | "stripped_birch_wood" | "stripped_cherry_log" | "stripped_cherry_wood" | "stripped_crimson_hyphae" | "stripped_crimson_stem" | "stripped_dark_oak_log" | "stripped_dark_oak_wood" | "stripped_jungle_log" | "stripped_jungle_wood" | "stripped_mangrove_log" | "stripped_mangrove_wood" | "stripped_oak_log" | "stripped_oak_wood" | "stripped_spruce_log" | "stripped_spruce_wood" | "stripped_warped_hyphae" | "stripped_warped_stem" | "trapped_chest" | "warped_door" | "warped_fence" | "warped_fence_gate" | "warped_hanging_sign" | "warped_hyphae" | "warped_planks" | "warped_pressure_plate" | "warped_sign" | "warped_slab" | "warped_stairs" | "warped_stem" | "warped_trapdoor" | "warped_wall_hanging_sign" | "warped_wall_sign" | "white_banner" | "white_wall_banner" | "yellow_banner" | "yellow_wall_banner" => NoteBlockInstrument::Bass,
            "black_concrete_powder" | "blue_concrete_powder" | "brown_concrete_powder" | "cyan_concrete_powder" | "gravel" | "gray_concrete_powder" | "green_concrete_powder" | "heavy_core" | "light_blue_concrete_powder" | "light_gray_concrete_powder" | "lime_concrete_powder" | "magenta_concrete_powder" | "orange_concrete_powder" | "pink_concrete_powder" | "purple_concrete_powder" | "red_concrete_powder" | "red_sand" | "sand" | "suspicious_gravel" | "suspicious_sand" | "white_concrete_powder" | "yellow_concrete_powder" => NoteBlockInstrument::Snare,
            "beacon" | "black_stained_glass" | "black_stained_glass_pane" | "blue_stained_glass" | "blue_stained_glass_pane" | "brown_stained_glass" | "brown_stained_glass_pane" | "conduit" | "cyan_stained_glass" | "cyan_stained_glass_pane" | "glass" | "glass_pane" | "gray_stained_glass" | "gray_stained_glass_pane" | "green_stained_glass" | "green_stained_glass_pane" | "light_blue_stained_glass" | "light_blue_stained_glass_pane" | "light_gray_stained_glass" | "light_gray_stained_glass_pane" | "lime_stained_glass" | "lime_stained_glass_pane" | "magenta_stained_glass" | "magenta_stained_glass_pane" | "orange_stained_glass" | "orange_stained_glass_pane" | "pink_stained_glass" | "pink_stained_glass_pane" | "purple_stained_glass" | "purple_stained_glass_pane" | "red_stained_glass" | "red_stained_glass_pane" | "sea_lantern" | "tinted_glass" | "white_stained_glass" | "white_stained_glass_pane" | "yellow_stained_glass" | "yellow_stained_glass_pane" => NoteBlockInstrument::Hat,
            "black_wool" | "blue_wool" | "brown_wool" | "cyan_wool" | "gray_wool" | "green_wool" | "light_blue_wool" | "light_gray_wool" | "lime_wool" | "magenta_wool" | "orange_wool" | "pink_wool" | "purple_wool" | "red_wool" | "white_wool" | "yellow_wool" => NoteBlockInstrument::Guitar,
            "clay" => NoteBlockInstrument::Flute,
            "gold_block" => NoteBlockInstrument::Bell,
            "packed_ice" => NoteBlockInstrument::Chime,
            "bone_block" => NoteBlockInstrument::Xylophone,
            "iron_block" => NoteBlockInstrument::IronXylophone,
            "soul_sand" => NoteBlockInstrument::CowBell,
            "pumpkin" => NoteBlockInstrument::Didgeridoo,
            "emerald_block" => NoteBlockInstrument::Bit,
            "hay_block" => NoteBlockInstrument::Banjo,
            "glowstone" => NoteBlockInstrument::Pling,
            "zombie_head" => NoteBlockInstrument::Zombie,
            "skeleton_skull" => NoteBlockInstrument::Skeleton,
            "creeper_head" => NoteBlockInstrument::Creeper,
            "dragon_head" => NoteBlockInstrument::Dragon,
            "wither_skeleton_skull" => NoteBlockInstrument::WitherSkeleton,
            "piglin_head" => NoteBlockInstrument::Piglin,
            "player_head" => NoteBlockInstrument::CustomHead,
        _ => NoteBlockInstrument::Harp,
    }
}

/// Vanilla `NoteBlock.updateNoteBlockState` (`dlh.b`): a mob head directly
/// above supplies the instrument; otherwise the block directly below does,
/// falling back to HARP when that block is itself a head.
pub(crate) fn note_block_update_instrument(
    above_block: &str,
    below_block: &str,
) -> NoteBlockInstrument {
    let above = note_block_instrument_from_below(above_block);
    if above.works_above_note_block() {
        return above;
    }
    let below = note_block_instrument_from_below(below_block);
    if below.works_above_note_block() {
        NoteBlockInstrument::Harp
    } else {
        below
    }
}

/// Vanilla `NoteBlock.getPitchFromNote` (`dlh`): `2^((note - 12) / 12)` for
/// note values 0..=24, i.e. F#1 through F#3.
pub(crate) fn note_block_pitch(note: u8) -> f32 {
    2.0_f32.powf((note as f32 - 12.0) / 12.0)
}

/// Right-click tuning: `useWithoutItem` cycles `(note + 1) % 25` and resets
/// `powered`; returns the new note and its pitch.
pub(crate) fn note_block_tune(note: u8) -> (u8, f32) {
    let next = (note + 1) % 25;
    (next, note_block_pitch(next))
}

/// Vanilla `playNote`/`triggerEvent` gate: the block event fires when the
/// block above is air; with a head instrument it targets the head above and
/// plays the skull's ambient sound instead.
pub(crate) fn note_block_trigger(instrument: NoteBlockInstrument, above_is_air: bool) -> bool {
    above_is_air || instrument.works_above_note_block()
}

/// Vanilla `neighborUpdate`: persist the new `powered` flag and fire only on
/// the rising edge (unpowered -> powered). Returns `(new_powered, trigger)`.
pub(crate) fn note_block_neighbor_update(powered: bool, has_signal: bool) -> (bool, bool) {
    (has_signal, has_signal && !powered)
}

/// Resolve the effective instrument for the note block at `pos`, mirroring
/// `updateNoteBlockState`: mob head above wins, else the block below.
pub(crate) fn note_block_instrument_at_dim(
    dim: DimId,
    pos: BlockPos,
) -> Option<NoteBlockInstrument> {
    let reg = breg()?;
    let above = reg.name_of(block_at_dim(dim, pos.0, pos.1 + 1, pos.2));
    let below = reg.name_of(block_at_dim(dim, pos.0, pos.1 - 1, pos.2));
    Some(note_block_update_instrument(above, below))
}

/// Recompute and persist the `instrument` property of the note block at `pos`
/// and queue the visible update — the vanilla `updateNoteBlockState` effect.
pub(crate) fn note_block_refresh_instrument_dim(
    dim: DimId,
    pos: BlockPos,
) -> Option<NoteBlockInstrument> {
    let reg = breg()?;
    let st = block_at_dim(dim, pos.0, pos.1, pos.2);
    if reg.name_of(st) != "minecraft:note_block" {
        return None;
    }
    let inst = note_block_instrument_at_dim(dim, pos)?;
    let new_st = reg.with_prop(st, "instrument", inst.state_name());
    // Vanilla `updateNoteBlockState` only writes when the recomputed state
    // differs; an unchanged instrument must not rebroadcast a block update.
    if new_st != st {
        set_and_note_world_block(dim, pos, new_st);
    }
    Some(inst)
}

/// Right-click (`useWithoutItem`) a note block: cycle `note`, clear `powered`,
/// refresh `instrument`, then play iff the gate passes. Returns
/// `(instrument, new_note, pitch, played)`; None when `pos` is not a note block.
pub(crate) fn note_block_use_dim(
    dim: DimId,
    pos: BlockPos,
) -> Option<(NoteBlockInstrument, u8, f32, bool)> {
    let reg = breg()?;
    let st = block_at_dim(dim, pos.0, pos.1, pos.2);
    if reg.name_of(st) != "minecraft:note_block" {
        return None;
    }
    let note: u8 = reg.get_prop(st, "note")?.parse().ok()?;
    let (next, pitch) = note_block_tune(note);
    let inst = note_block_instrument_at_dim(dim, pos)?;
    let new_st = reg.with_prop(
        reg.with_prop(reg.with_prop(st, "note", &next.to_string()), "instrument", inst.state_name()),
        "powered",
        "false",
    );
    set_and_note_world_block(dim, pos, new_st);
    let above_is_air = reg.is_air(block_at_dim(dim, pos.0, pos.1 + 1, pos.2));
    let played = note_block_trigger(inst, above_is_air);
    Some((inst, next, pitch, played))
}

/// Redstone handling for `neighborUpdate`: flip `powered` to `has_signal`,
/// refresh the instrument, and report whether the block event fired (rising
/// edge plus the `playNote` gate). Returns `(new_powered, played)`.
pub(crate) fn note_block_power_dim(
    dim: DimId,
    pos: BlockPos,
    has_signal: bool,
) -> Option<(bool, bool)> {
    let reg = breg()?;
    let st = block_at_dim(dim, pos.0, pos.1, pos.2);
    if reg.name_of(st) != "minecraft:note_block" {
        return None;
    }
    let powered = reg.get_prop(st, "powered").as_deref() == Some("true");
    let (next_powered, rising) = note_block_neighbor_update(powered, has_signal);
    if next_powered == powered {
        return Some((powered, false));
    }
    let inst = note_block_instrument_at_dim(dim, pos)?;
    let new_st = reg.with_prop(st, "powered", if next_powered { "true" } else { "false" });
    set_and_note_world_block(dim, pos, new_st);
    let above_is_air = reg.is_air(block_at_dim(dim, pos.0, pos.1 + 1, pos.2));
    let played = rising && note_block_trigger(inst, above_is_air);
    Some((next_powered, played))
}

/// Vanilla cauldron contents (1.21.1 `dgt`/`dkh`/`dkg`): plain cauldron, and
/// the layered water / powder-snow cauldrons at level 1..=3, plus full lava.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CauldronState {
    Empty,
    Water(u8),
    Lava,
    PowderSnow(u8),
}

impl CauldronState {
    pub(crate) fn block_name(self) -> &'static str {
        match self {
            CauldronState::Empty => "minecraft:cauldron",
            CauldronState::Water(_) => "minecraft:water_cauldron",
            CauldronState::Lava => "minecraft:lava_cauldron",
            CauldronState::PowderSnow(_) => "minecraft:powder_snow_cauldron",
        }
    }

    /// `LayeredCauldronBlock.LEVEL` value (None for non-layered contents).
    pub(crate) fn level(self) -> Option<u8> {
        match self {
            CauldronState::Water(l) | CauldronState::PowderSnow(l) => Some(l),
            _ => None,
        }
    }

    /// `AbstractCauldronBlock.getAnalogOutputSignal`: layered cauldrons emit
    /// their level; lava emits 3; an empty cauldron emits 0.
    pub(crate) fn comparator_signal(self) -> u8 {
        match self {
            CauldronState::Empty => 0,
            CauldronState::Water(l) | CauldronState::PowderSnow(l) => l,
            CauldronState::Lava => 3,
        }
    }

    /// Registry block state for this content.
    pub(crate) fn to_state(self, reg: &crate::block_registry::BReg) -> Option<u32> {
        let st = reg.state_of(self.block_name(), None);
        if reg.is_air(st) && self.block_name() != "minecraft:air" && reg.name_of(st) == "minecraft:air" {
            return None;
        }
        Some(match self.level() {
            Some(l) => reg.with_prop(st, "level", &l.to_string()),
            None => st,
        })
    }
}

/// Read the cauldron content from a registry block state; None for non-cauldrons.
pub(crate) fn cauldron_state_of(reg: &crate::block_registry::BReg, st: u32) -> Option<CauldronState> {
    let level = |name: &str| {
        reg.get_prop(st, "level")
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|l| (1..=3).contains(l))
            .unwrap_or(if name == "minecraft:lava_cauldron" { 3 } else { 1 })
    };
    match reg.name_of(st) {
        "minecraft:cauldron" => Some(CauldronState::Empty),
        "minecraft:water_cauldron" => Some(CauldronState::Water(level("minecraft:water_cauldron"))),
        "minecraft:lava_cauldron" => Some(CauldronState::Lava),
        "minecraft:powder_snow_cauldron" => {
            Some(CauldronState::PowderSnow(level("minecraft:powder_snow_cauldron")))
        }
        _ => None,
    }
}

/// The result of a `CauldronInteraction`: the cauldron's next content plus the
/// item the player's stack becomes (`returns`, e.g. an emptied bucket or a
/// filled bottle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CauldronOutcome {
    pub new_state: CauldronState,
    pub returns: Option<String>,
}

/// Vanilla `CauldronInteraction` map (1.21.1 `kj`): the bucket fills apply on
/// every content map; empties apply only to their own content at the right
/// level; bottle/potion fills and the dye wash families live on the water map.
/// Returns None when the interaction is a PASS (state untouched).
///
/// The wash families need facts this name-only entry point cannot see, so it
/// assumes the decorated case (one banner pattern layer, a dyed stack) to keep
/// the historical behaviour of callers that only hold an item id. Callers that
/// hold the real stack should use [`cauldron_use_stack`].
pub(crate) fn cauldron_use(state: CauldronState, item_name: &str) -> Option<CauldronOutcome> {
    cauldron_use_stack(state, item_name, 1, true)
}

/// `#minecraft:dyeable`: the only item family the water map's dye wash accepts.
fn cauldron_item_is_dyeable(item: &str) -> bool {
    matches!(
        item,
        "leather_helmet"
            | "leather_chestplate"
            | "leather_leggings"
            | "leather_boots"
            | "leather_horse_armor"
            | "wolf_armor"
    )
}

/// Vanilla `CauldronInteraction` with the component facts the wash families
/// read: `pattern_layers` is the banner's `banner_patterns` layer count and
/// `has_dyed_color` is the presence of the `dyed_color` component.
pub(crate) fn cauldron_use_stack(
    state: CauldronState,
    item_name: &str,
    pattern_layers: usize,
    has_dyed_color: bool,
) -> Option<CauldronOutcome> {
    let item = vanilla_id_path(item_name);
    let outcome = |new_state, returns: Option<&str>| {
        Some(CauldronOutcome { new_state, returns: returns.map(str::to_owned) })
    };
    let lower_water = |state| match state {
        CauldronState::Water(1) => Some(CauldronState::Empty),
        CauldronState::Water(l) => Some(CauldronState::Water(l - 1)),
        _ => None,
    };
    match item {
        // Default bucket fills are registered on every map.
        "water_bucket" => outcome(CauldronState::Water(3), Some("minecraft:bucket")),
        "lava_bucket" => outcome(CauldronState::Lava, Some("minecraft:bucket")),
        "powder_snow_bucket" => outcome(CauldronState::PowderSnow(3), Some("minecraft:bucket")),
        "bucket" => match state {
            CauldronState::Water(3) => outcome(CauldronState::Empty, Some("minecraft:water_bucket")),
            CauldronState::Lava => outcome(CauldronState::Empty, Some("minecraft:lava_bucket")),
            CauldronState::PowderSnow(3) => {
                outcome(CauldronState::Empty, Some("minecraft:powder_snow_bucket"))
            }
            _ => None,
        },
        "glass_bottle" => match state {
            CauldronState::Water(1) => outcome(CauldronState::Empty, Some("minecraft:potion")),
            CauldronState::Water(l) => outcome(CauldronState::Water(l - 1), Some("minecraft:potion")),
            _ => None,
        },
        "potion" => match state {
            CauldronState::Empty => outcome(CauldronState::Water(1), Some("minecraft:glass_bottle")),
            CauldronState::Water(l) if l < 3 => {
                outcome(CauldronState::Water(l + 1), Some("minecraft:glass_bottle"))
            }
            _ => None,
        },
        // `DYED_ITEM` (`kj.a`): a `#minecraft:dyeable` stack is washed only when
        // it actually carries a `dyed_color` component; an undyed one is a PASS
        // and costs no water. Washing clears the component and keeps the item.
        _ if cauldron_item_is_dyeable(item) => has_dyed_color
            .then(|| lower_water(state))
            .flatten()
            .map(|next| CauldronOutcome { new_state: next, returns: Some(item_name.to_owned()) }),
        // `BANNER` (`kj.b`): an undecorated banner is a PASS; a decorated one
        // loses exactly one pattern layer on the top of the list, so the base
        // colour and the remaining layers survive.
        _ if item.ends_with("_banner") => (pattern_layers > 0)
            .then(|| lower_water(state))
            .flatten()
            .map(|next| CauldronOutcome { new_state: next, returns: Some(item_name.to_owned()) }),
        _ if item.ends_with("_shulker_box") => lower_water(state).map(|next| CauldronOutcome {
            new_state: next,
            returns: Some("minecraft:shulker_box".to_owned()),
        }),
        _ => None,
    }
}

/// Precipitation kinds for `CauldronBlock.handlePrecipitation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CauldronPrecip {
    Rain,
    Snow,
}

/// Vanilla `handlePrecipitation` (`dgt`/`dkh`): with the pre-rolled random
/// `roll` under the 0.05 (rain) / 0.10 (snow) chance, rain grows only water
/// cauldrons and snow only powder-snow cauldrons (each while level < 3); an
/// empty cauldron takes level 1 of the matching content.
pub(crate) fn cauldron_weather_fill(
    state: CauldronState,
    precip: CauldronPrecip,
    roll: f64,
) -> CauldronState {
    let chance = match precip {
        CauldronPrecip::Rain => 0.05,
        CauldronPrecip::Snow => 0.10,
    };
    if roll >= chance {
        return state;
    }
    match (state, precip) {
        (CauldronState::Empty, CauldronPrecip::Rain) => CauldronState::Water(1),
        (CauldronState::Empty, CauldronPrecip::Snow) => CauldronState::PowderSnow(1),
        (CauldronState::Water(l), CauldronPrecip::Rain) if l < 3 => CauldronState::Water(l + 1),
        (CauldronState::PowderSnow(l), CauldronPrecip::Snow) if l < 3 => {
            CauldronState::PowderSnow(l + 1)
        }
        _ => state,
    }
}

/// Fluids a pointed dripstone can drip into a cauldron.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CauldronDripFluid {
    Water,
    Lava,
}

/// Vanilla `receiveStalactiteDrip`/`canReceiveStalactiteDrip` (`dgt`/`dkh`):
/// an empty cauldron accepts water -> Water(1) or lava -> Lava; a water
/// cauldron below full accepts water drips (+1). Powder-snow and lava
/// cauldrons receive nothing.
pub(crate) fn cauldron_drip_fill(state: CauldronState, fluid: CauldronDripFluid) -> CauldronState {
    match (state, fluid) {
        (CauldronState::Empty, CauldronDripFluid::Water) => CauldronState::Water(1),
        (CauldronState::Empty, CauldronDripFluid::Lava) => CauldronState::Lava,
        (CauldronState::Water(l), CauldronDripFluid::Water) if l < 3 => CauldronState::Water(l + 1),
        _ => state,
    }
}

/// `LayeredCauldronBlock.entityInside` (`dkh.e`): a burning entity inside the
/// content extinguishes — water loses a level (level 1 empties), powder snow
/// turns into water at the same level.
pub(crate) fn cauldron_extinguish(state: CauldronState) -> CauldronState {
    match state {
        CauldronState::Water(1) => CauldronState::Empty,
        CauldronState::Water(l) => CauldronState::Water(l - 1),
        CauldronState::PowderSnow(l) => CauldronState::Water(l),
        s => s,
    }
}

/// Vanilla `ComposterBlock.COMPOSTABLES` (1.21.1 `dhj` clinit): the item ->
/// chance table. Returns None for non-compostable items.
pub(crate) fn composter_chance(item_name: &str) -> Option<f64> {
    match vanilla_id_path(item_name) {
        "acacia_leaves" | "acacia_sapling" | "azalea_leaves" | "beetroot_seeds" | "birch_leaves" | "birch_sapling" | "cherry_leaves" | "cherry_sapling" | "dark_oak_leaves" | "dark_oak_sapling" | "dried_kelp" | "glow_berries" | "hanging_roots" | "jungle_leaves" | "jungle_sapling" | "kelp" | "mangrove_leaves" | "mangrove_propagule" | "mangrove_roots" | "melon_seeds" | "moss_carpet" | "oak_leaves" | "oak_sapling" | "pink_petals" | "pitcher_pod" | "pumpkin_seeds" | "seagrass" | "short_grass" | "small_dripleaf" | "spruce_leaves" | "spruce_sapling" | "sweet_berries" | "torchflower_seeds" | "wheat_seeds" => Some(0.3),
        "cactus" | "dried_kelp_block" | "flowering_azalea_leaves" | "glow_lichen" | "melon_slice" | "nether_sprouts" | "sugar_cane" | "tall_grass" | "twisting_vines" | "vine" | "weeping_vines" => Some(0.5),
        "allium" | "apple" | "azalea" | "azure_bluet" | "beetroot" | "big_dripleaf" | "blue_orchid" | "brown_mushroom" | "carrot" | "carved_pumpkin" | "cocoa_beans" | "cornflower" | "crimson_fungus" | "crimson_roots" | "dandelion" | "fern" | "large_fern" | "lilac" | "lily_of_the_valley" | "lily_pad" | "melon" | "moss_block" | "mushroom_stem" | "nether_wart" | "orange_tulip" | "oxeye_daisy" | "peony" | "pink_tulip" | "poppy" | "potato" | "pumpkin" | "red_mushroom" | "red_tulip" | "rose_bush" | "sea_pickle" | "shroomlight" | "spore_blossom" | "sunflower" | "warped_fungus" | "warped_roots" | "wheat" | "white_tulip" | "wither_rose" => Some(0.65),
        "baked_potato" | "bread" | "brown_mushroom_block" | "cookie" | "flowering_azalea" | "hay_block" | "nether_wart_block" | "pitcher_plant" | "red_mushroom_block" | "torchflower" | "warped_wart_block" => Some(0.85),
        "cake" | "pumpkin_pie" => Some(1.0),
        _ => None,
    }
}

/// Result of a composter insert attempt (`dhj` use path).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ComposterInsert {
    /// Item not compostable, or the composter already reads level 7+ — the
    /// interaction is a PASS and nothing is consumed.
    Rejected,
    /// Compostable consumed but the chance roll failed — level unchanged.
    ConsumedNoAdvance,
    /// Compostable consumed and the roll passed: `level -> level + 1`. When
    /// the new level is 7 vanilla schedules the ~20-tick maturation that later
    /// shows level 8.
    Advanced { new_level: u8, schedules_maturation: bool },
}

/// Vanilla `addItem`: accepts a compostable only while level < 7 and bumps the
/// level when the roll passes the item's chance — except on an empty composter,
/// where any positive chance always advances so the first item is never wasted
/// (`if ((i != 0 || f <= 0.0F) && !(random.nextDouble() < f)) { /* no advance */ }`).
pub(crate) fn composter_insert(level: u8, item_name: &str, roll: f64) -> ComposterInsert {
    if level >= 7 {
        return ComposterInsert::Rejected;
    }
    let Some(chance) = composter_chance(item_name) else {
        return ComposterInsert::Rejected;
    };
    if level != 0 && roll >= chance {
        return ComposterInsert::ConsumedNoAdvance;
    }
    let new_level = level + 1;
    ComposterInsert::Advanced { new_level, schedules_maturation: new_level == 7 }
}

/// Scheduled-tick maturation (`dhj.tick`): level 7 becomes 8 — the "ready"
/// state a player harvests.
pub(crate) fn composter_mature(level: u8) -> u8 {
    if level == 7 {
        8
    } else {
        level
    }
}

/// `extractProduce` on level 8: pop one `minecraft:bone_meal` and reset to 0.
/// Returns the new level and produced item, or None below level 8.
pub(crate) fn composter_harvest(level: u8) -> Option<(u8, &'static str)> {
    if level == 8 {
        Some((0, "minecraft:bone_meal"))
    } else {
        None
    }
}

/// `ComposterBlock.getAnalogOutputSignal`: the level itself (0..=8).
pub(crate) fn composter_comparator(level: u8) -> u8 {
    level
}

/// Vanilla `RespawnAnchorBlock` (1.21.1 `dmh`) interact outcome. `charges` is
/// the block's `charges` property (0..=4) before the interaction.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AnchorOutcome {
    /// Glowstone accepted: `charges -> charges + 1`.
    Charged { charges: u8 },
    /// Glowstone on a full anchor: vanilla consumes it but the state is
    /// unchanged (already at max charge).
    Full,
    /// Charged anchor clicked where anchors work (`respawnAnchorWorks`, i.e.
    /// the Nether): the player respawn point is set; the charge is only
    /// consumed on the actual respawn.
    RespawnSet,
    /// Charged anchor clicked where anchors don't work (Overworld/End):
    /// power-5 explosion and the anchor is destroyed.
    Explode { power: f32 },
    /// Uncharged anchor: PASS, nothing happens.
    Pass,
}

/// `dmh` interact: `held_item` is the used stack's item name (None = empty
/// hand); `anchors_work` is `dimensionType.respawnAnchorWorks` (true for the
/// Nether and nether-like custom dimensions).
pub(crate) fn respawn_anchor_use(
    charges: u8,
    held_item: Option<&str>,
    anchors_work: bool,
) -> AnchorOutcome {
    let is_glowstone = held_item
        .map(|i| vanilla_id_path(i) == "glowstone")
        .unwrap_or(false);
    if is_glowstone {
        return if charges < 4 {
            AnchorOutcome::Charged { charges: charges + 1 }
        } else {
            AnchorOutcome::Full
        };
    }
    if charges == 0 {
        return AnchorOutcome::Pass;
    }
    if anchors_work {
        AnchorOutcome::RespawnSet
    } else {
        AnchorOutcome::Explode { power: 5.0 }
    }
}

/// One charge is consumed when the player actually respawns at the anchor.
pub(crate) fn respawn_anchor_consume(charges: u8) -> u8 {
    charges.saturating_sub(1)
}

/// `RespawnAnchorBlock.getAnalogOutputSignal`: `min(15, charges * 4 - 1)` —
/// exactly `[0, 3, 7, 11, 15]` for charges 0..=4.
pub(crate) fn respawn_anchor_comparator(charges: u8) -> u8 {
    [0, 3, 7, 11, 15][charges.min(4) as usize]
}

/// What the lodestone dimension+position binding stores on the compass (the
/// 1.21.1 `lodestone_target` component's tracked pos and dimension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(all(test, megu_vanilla_data))]
pub(crate) struct LodestoneTarget {
    pub dim: DimId,
    pub pos: BlockPos,
}

/// Vanilla `LodestoneTracker` (compass component): a compass bound by
/// right-clicking a lodestone. `validate` mirrors `LodestoneTracker` refresh —
/// the binding clears (and the needle spins) once the block at the target is
/// no longer a lodestone.
#[derive(Debug, Clone, Default)]
#[cfg(all(test, megu_vanilla_data))]
pub(crate) struct LodestoneTracker {
    pub target: Option<LodestoneTarget>,
}

#[cfg(all(test, megu_vanilla_data))]
impl LodestoneTracker {
    /// Right-click `lodestone` with a `compass`: store `(dim, x, y, z)`.
    pub(crate) fn bind(&mut self, dim: DimId, pos: BlockPos) -> LodestoneTarget {
        let target = LodestoneTarget { dim, pos };
        self.target = Some(target);
        target
    }

    /// True while the tracked block is still `minecraft:lodestone`; clears the
    /// binding and returns false once it is broken or replaced.
    pub(crate) fn validate(&mut self, reg: &crate::block_registry::BReg) -> bool {
        let Some(target) = self.target else { return false };
        let st = block_at_dim(target.dim, target.pos.0, target.pos.1, target.pos.2);
        if reg.name_of(st) == "minecraft:lodestone" {
            true
        } else {
            self.target = None;
            false
        }
    }
}

/// Vanilla `SpongeBlock` (1.21.1 `dno`): `BlockPos.breadthFirstTraversal`
/// from the sponge — Manhattan depth <= 6, at most 65 visited cells, and only
/// cells whose contents pass the absorb predicate expand the frontier. The
/// sponge position itself always expands. Returns the absorbed positions in
/// BFS order (does not include the sponge cell).
pub(crate) fn sponge_absorb_water(dim: DimId, sponge: BlockPos) -> Vec<BlockPos> {
    let Some(reg) = breg() else { return Vec::new() };
    let mut reader = DimFluidReader(dim);
    let Ok(plan) = flowing_fluid::sponge_absorb_water(&mut reader, sponge) else {
        return Vec::new();
    };
    let mut absorbed = Vec::with_capacity(plan.len());
    for &(x, y, z) in plan.iter() {
        let st = block_at_dim(dim, x, y, z);
        if reg.is_air(st) {
            continue;
        }
        let removable = matches!(
            reg.name_of(st),
            "minecraft:water"
                | "minecraft:kelp"
                | "minecraft:kelp_plant"
                | "minecraft:seagrass"
                | "minecraft:tall_seagrass"
                | "minecraft:bubble_column"
        );
        if !removable && !reg.has_water(st) {
            continue;
        }
        if reg.get_prop(st, "waterlogged").as_deref() == Some("true") {
            set_block_dim(dim, x, y, z, reg.with_prop(st, "waterlogged", "false"));
        } else if removable {
            set_block_dim(dim, x, y, z, reg.state_of("minecraft:air", None));
        }
        absorbed.push((x, y, z));
    }
    absorbed
}

/// `WetSpongeBlock.onPlace` / `SpongeBlock` nether behavior: in an ultrawarm
/// dimension (`dimension_type.ultrawarm`, i.e. the Nether) a placed sponge
/// instantly dries — `wet_sponge` evaporates back to `sponge` rather than
/// staying wet.
pub(crate) fn sponge_nether_dries(dim: DimId) -> bool {
    dim_ultrawarm(dim)
}

#[cfg(test)]
mod vanilla_blocks_tests {
    use super::*;
    #[cfg(megu_vanilla_data)]
    use crate::block_registry::BReg;

    #[cfg(megu_vanilla_data)]
    fn reg() -> &'static BReg {
        breg().expect("block registry")
    }
    #[cfg(megu_vanilla_data)]
    fn state(name: &str) -> u32 {
        reg().state_of(name, None)
    }
    #[cfg(megu_vanilla_data)]
    fn name_at(dim: DimId, x: i32, y: i32, z: i32) -> String {
        reg().name_of(block_at_dim(dim, x, y, z)).to_owned()
    }

    #[test]
    fn vanilla_blocks_note_instrument_table() {
        use NoteBlockInstrument::*;
        assert_eq!(note_block_instrument_from_below("minecraft:stone"), Basedrum);
        assert_eq!(note_block_instrument_from_below("minecraft:netherrack"), Basedrum);
        assert_eq!(note_block_instrument_from_below("minecraft:oak_log"), Bass);
        assert_eq!(note_block_instrument_from_below("minecraft:oak_planks"), Bass);
        assert_eq!(note_block_instrument_from_below("minecraft:note_block"), Bass);
        assert_eq!(note_block_instrument_from_below("minecraft:sand"), Snare);
        assert_eq!(note_block_instrument_from_below("minecraft:gravel"), Snare);
        assert_eq!(note_block_instrument_from_below("minecraft:glass"), Hat);
        assert_eq!(note_block_instrument_from_below("minecraft:gold_block"), Bell);
        assert_eq!(note_block_instrument_from_below("minecraft:clay"), Flute);
        assert_eq!(note_block_instrument_from_below("minecraft:packed_ice"), Chime);
        assert_eq!(note_block_instrument_from_below("minecraft:white_wool"), Guitar);
        assert_eq!(note_block_instrument_from_below("minecraft:bone_block"), Xylophone);
        assert_eq!(note_block_instrument_from_below("minecraft:iron_block"), IronXylophone);
        assert_eq!(note_block_instrument_from_below("minecraft:soul_sand"), CowBell);
        assert_eq!(note_block_instrument_from_below("minecraft:pumpkin"), Didgeridoo);
        assert_eq!(note_block_instrument_from_below("minecraft:emerald_block"), Bit);
        assert_eq!(note_block_instrument_from_below("minecraft:hay_block"), Banjo);
        assert_eq!(note_block_instrument_from_below("minecraft:glowstone"), Pling);
        assert_eq!(note_block_instrument_from_below("minecraft:zombie_head"), Zombie);
        assert_eq!(note_block_instrument_from_below("minecraft:skeleton_skull"), Skeleton);
        assert_eq!(note_block_instrument_from_below("minecraft:creeper_head"), Creeper);
        assert_eq!(note_block_instrument_from_below("minecraft:dragon_head"), Dragon);
        assert_eq!(note_block_instrument_from_below("minecraft:wither_skeleton_skull"), WitherSkeleton);
        assert_eq!(note_block_instrument_from_below("minecraft:piglin_head"), Piglin);
        assert_eq!(note_block_instrument_from_below("minecraft:player_head"), CustomHead);
        // 1.21.1 fallbacks: wall heads and unlisted blocks are HARP
        assert_eq!(note_block_instrument_from_below("minecraft:zombie_wall_head"), Harp);
        assert_eq!(note_block_instrument_from_below("minecraft:wither_skeleton_wall_skull"), Harp);
        assert_eq!(note_block_instrument_from_below("minecraft:dirt"), Harp);
        assert_eq!(note_block_instrument_from_below("minecraft:air"), Harp);
        assert_eq!(note_block_instrument_from_below("minecraft:copper_block"), Harp);
        // isTunable: the 16 player instruments only; heads are not tunable.
        for inst in [
            Harp, Basedrum, Snare, Hat, Bass, Flute, Bell, Guitar, Chime, Xylophone,
            IronXylophone, CowBell, Didgeridoo, Bit, Banjo, Pling,
        ] {
            assert!(inst.is_tunable(), "{inst:?}");
            assert!(!inst.works_above_note_block(), "{inst:?}");
        }
        for inst in [Zombie, Skeleton, Creeper, Dragon, WitherSkeleton, Piglin, CustomHead] {
            assert!(!inst.is_tunable(), "{inst:?}");
            assert!(inst.works_above_note_block(), "{inst:?}");
        }
    }

    #[test]
    fn vanilla_blocks_note_update_instrument_prefers_head_above() {
        use NoteBlockInstrument::*;
        assert_eq!(
            note_block_update_instrument("minecraft:creeper_head", "minecraft:oak_log"),
            Creeper
        );
        assert_eq!(
            note_block_update_instrument("minecraft:player_head", "minecraft:stone"),
            CustomHead
        );
        assert_eq!(
            note_block_update_instrument("minecraft:air", "minecraft:oak_log"),
            Bass
        );
        // A head *below* is not a valid host: falls back to HARP.
        assert_eq!(
            note_block_update_instrument("minecraft:air", "minecraft:zombie_head"),
            Harp
        );
    }

    #[test]
    fn vanilla_blocks_note_pitch_and_tune() {
        assert!((note_block_pitch(12) - 1.0).abs() < 1e-6);
        assert!((note_block_pitch(24) - 2.0).abs() < 1e-6);
        assert!((note_block_pitch(0) - 0.5).abs() < 1e-6);
        let (n13, p13) = note_block_tune(12);
        assert_eq!(n13, 13);
        assert!((p13 - note_block_pitch(13)).abs() < 1e-6);
        // note wraps at 25
        assert_eq!(note_block_tune(24).0, 0);
        assert!((note_block_tune(24).1 - 0.5).abs() < 1e-6);
    }

    #[test]
    fn vanilla_blocks_note_trigger_gate() {
        use NoteBlockInstrument::*;
        assert!(note_block_trigger(Harp, true));
        assert!(!note_block_trigger(Harp, false));
        // Head instruments fire even though the block above isn't air.
        assert!(note_block_trigger(Zombie, false));
        assert!(note_block_trigger(CustomHead, false));
        assert_eq!(note_block_neighbor_update(false, true), (true, true));
        assert_eq!(note_block_neighbor_update(true, true), (true, false));
        assert_eq!(note_block_neighbor_update(true, false), (false, false));
        assert_eq!(note_block_neighbor_update(false, false), (false, false));
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn vanilla_blocks_note_dim_use_power_and_obstruction() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let reg = reg();
        let dim = DimId::Overworld;
        let pos = (24300, 210, 24300);
        let nb = state("minecraft:note_block");
        set_block_dim(dim, pos.0, pos.1, pos.2, reg.with_prop(nb, "note", "12"));
        set_block_dim(dim, pos.0, pos.1 - 1, pos.2, state("minecraft:gold_block"));
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, state("minecraft:air"));

        let (inst, note, pitch, played) = note_block_use_dim(dim, pos).unwrap();
        assert_eq!(inst, NoteBlockInstrument::Bell);
        assert_eq!(note, 13);
        assert!((pitch - note_block_pitch(13)).abs() < 1e-6);
        assert!(played);
        assert_eq!(reg.get_prop(block_at_dim(dim, pos.0, pos.1, pos.2), "note").unwrap(), "13");
        assert_eq!(
            reg.get_prop(block_at_dim(dim, pos.0, pos.1, pos.2), "instrument").unwrap(),
            "bell"
        );

        // Obstructed above: no playback.
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, state("minecraft:stone"));
        let (_, _, _, played2) = note_block_use_dim(dim, pos).unwrap();
        assert!(!played2);

        // Powered rising edge only.
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, state("minecraft:air"));
        assert_eq!(note_block_power_dim(dim, pos, true), Some((true, true)));
        assert_eq!(note_block_power_dim(dim, pos, true), Some((true, false)));
        assert_eq!(note_block_power_dim(dim, pos, false), Some((false, false)));
        assert_eq!(note_block_power_dim(dim, pos, true), Some((true, true)));
        // Not a note block -> None.
        set_block_dim(dim, pos.0 + 4, pos.1, pos.2, state("minecraft:stone"));
        assert_eq!(note_block_use_dim(dim, (pos.0 + 4, pos.1, pos.2)), None);

        // updateNoteBlockState refresh: swap the host below and the stored
        // instrument follows.
        set_block_dim(dim, pos.0, pos.1 - 1, pos.2, state("minecraft:oak_log"));
        assert_eq!(note_block_refresh_instrument_dim(dim, pos), Some(NoteBlockInstrument::Bass));
        assert_eq!(
            reg.get_prop(block_at_dim(dim, pos.0, pos.1, pos.2), "instrument").unwrap(),
            "bass"
        );
        // A mob head above overrides the host below.
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, state("minecraft:creeper_head"));
        assert_eq!(note_block_refresh_instrument_dim(dim, pos), Some(NoteBlockInstrument::Creeper));
        set_block_dim(dim, pos.0, pos.1 + 1, pos.2, state("minecraft:air"));
        assert_eq!(
            note_block_refresh_instrument_dim(dim, (pos.0 + 4, pos.1, pos.2)),
            None
        );
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn vanilla_blocks_cauldron_state_and_comparator() {
        use CauldronState::*;
        let reg = reg();
        let w2 = reg.with_prop(state("minecraft:water_cauldron"), "level", "2");
        assert_eq!(cauldron_state_of(reg, w2), Some(Water(2)));
        assert_eq!(cauldron_state_of(reg, state("minecraft:lava_cauldron")), Some(Lava));
        assert_eq!(
            cauldron_state_of(reg, reg.with_prop(state("minecraft:powder_snow_cauldron"), "level", "3")),
            Some(PowderSnow(3))
        );
        assert_eq!(cauldron_state_of(reg, state("minecraft:cauldron")), Some(Empty));
        assert_eq!(cauldron_state_of(reg, state("minecraft:stone")), None);
        assert_eq!(Empty.comparator_signal(), 0);
        assert_eq!(Water(1).comparator_signal(), 1);
        assert_eq!(Water(3).comparator_signal(), 3);
        assert_eq!(PowderSnow(2).comparator_signal(), 2);
        assert_eq!(Lava.comparator_signal(), 3);
        // Round-trip through the registry: block_name/level/to_state.
        assert_eq!(Water(2).block_name(), "minecraft:water_cauldron");
        assert_eq!(Water(2).level(), Some(2));
        assert_eq!(Lava.level(), None);
        let st = Water(2).to_state(reg).unwrap();
        assert_eq!(reg.name_of(st), "minecraft:water_cauldron");
        assert_eq!(reg.get_prop(st, "level").unwrap(), "2");
        assert_eq!(cauldron_state_of(reg, st), Some(Water(2)));
        let st = Lava.to_state(reg).unwrap();
        assert_eq!(reg.name_of(st), "minecraft:lava_cauldron");
    }

    #[test]
    fn vanilla_blocks_cauldron_bucket_and_bottle() {
        use CauldronState::*;
        // Bucket fills work on every content map and overwrite.
        assert_eq!(
            cauldron_use(Empty, "minecraft:water_bucket"),
            Some(CauldronOutcome { new_state: Water(3), returns: Some("minecraft:bucket".into()) })
        );
        assert_eq!(cauldron_use(Empty, "minecraft:lava_bucket").unwrap().new_state, Lava);
        assert_eq!(
            cauldron_use(Empty, "minecraft:powder_snow_bucket").unwrap().new_state,
            PowderSnow(3)
        );
        assert_eq!(cauldron_use(Water(2), "minecraft:lava_bucket").unwrap().new_state, Lava);
        assert_eq!(
            cauldron_use(Lava, "minecraft:powder_snow_bucket").unwrap().new_state,
            PowderSnow(3)
        );
        // Empty buckets take their own content only at the full level.
        assert_eq!(
            cauldron_use(Water(3), "minecraft:bucket").unwrap().returns.unwrap(),
            "minecraft:water_bucket"
        );
        assert_eq!(cauldron_use(Water(3), "minecraft:bucket").unwrap().new_state, Empty);
        assert_eq!(cauldron_use(Water(2), "minecraft:bucket"), None);
        assert_eq!(
            cauldron_use(Lava, "minecraft:bucket").unwrap().returns.unwrap(),
            "minecraft:lava_bucket"
        );
        assert_eq!(
            cauldron_use(PowderSnow(3), "minecraft:bucket").unwrap().returns.unwrap(),
            "minecraft:powder_snow_bucket"
        );
        assert_eq!(cauldron_use(PowderSnow(2), "minecraft:bucket"), None);
        assert_eq!(cauldron_use(Empty, "minecraft:bucket"), None);
        // Glass bottle draws one level.
        assert_eq!(
            cauldron_use(Water(3), "minecraft:glass_bottle"),
            Some(CauldronOutcome { new_state: Water(2), returns: Some("minecraft:potion".into()) })
        );
        assert_eq!(cauldron_use(Water(1), "minecraft:glass_bottle").unwrap().new_state, Empty);
        assert_eq!(cauldron_use(Empty, "minecraft:glass_bottle"), None);
        // Water potion fills empty -> 1 or partial -> +1.
        assert_eq!(cauldron_use(Empty, "minecraft:potion").unwrap().new_state, Water(1));
        assert_eq!(cauldron_use(Water(2), "minecraft:potion").unwrap().new_state, Water(3));
        assert_eq!(cauldron_use(Water(3), "minecraft:potion"), None);
    }

    #[test]
    fn vanilla_blocks_cauldron_wash_weather_drip_extinguish() {
        use CauldronState::*;
        // Washing costs one water level.
        assert_eq!(cauldron_use(Water(3), "minecraft:leather_chestplate").unwrap().new_state, Water(2));
        assert_eq!(cauldron_use(Water(2), "minecraft:red_banner").unwrap().new_state, Water(1));
        assert_eq!(cauldron_use(Water(1), "minecraft:blue_shulker_box").unwrap().new_state, Empty);
        assert_eq!(
            cauldron_use(Water(1), "minecraft:blue_shulker_box").unwrap().returns.unwrap(),
            "minecraft:shulker_box"
        );
        // Lava and powder snow don't wash.
        assert_eq!(cauldron_use(Lava, "minecraft:red_banner"), None);
        assert_eq!(cauldron_use(PowderSnow(2), "minecraft:leather_boots"), None);
        assert_eq!(cauldron_use(Empty, "minecraft:red_banner"), None);

        use CauldronPrecip::*;
        assert_eq!(cauldron_weather_fill(Empty, Rain, 0.01), Water(1));
        assert_eq!(cauldron_weather_fill(Empty, Snow, 0.09), PowderSnow(1));
        assert_eq!(cauldron_weather_fill(Water(1), Rain, 0.049), Water(2));
        assert_eq!(cauldron_weather_fill(Water(3), Rain, 0.01), Water(3));
        assert_eq!(cauldron_weather_fill(PowderSnow(1), Rain, 0.01), PowderSnow(1));
        assert_eq!(cauldron_weather_fill(PowderSnow(2), Snow, 0.099), PowderSnow(3));
        assert_eq!(cauldron_weather_fill(Empty, Rain, 0.05), Empty);
        assert_eq!(cauldron_weather_fill(Empty, Snow, 0.10), Empty);

        assert_eq!(cauldron_drip_fill(Empty, CauldronDripFluid::Water), Water(1));
        assert_eq!(cauldron_drip_fill(Empty, CauldronDripFluid::Lava), Lava);
        assert_eq!(cauldron_drip_fill(Water(2), CauldronDripFluid::Water), Water(3));
        assert_eq!(cauldron_drip_fill(Water(3), CauldronDripFluid::Water), Water(3));
        assert_eq!(
            cauldron_drip_fill(PowderSnow(1), CauldronDripFluid::Water),
            PowderSnow(1)
        );
        assert_eq!(cauldron_drip_fill(Lava, CauldronDripFluid::Water), Lava);

        assert_eq!(cauldron_extinguish(Water(3)), Water(2));
        assert_eq!(cauldron_extinguish(Water(1)), Empty);
        assert_eq!(cauldron_extinguish(PowderSnow(2)), Water(2));
        assert_eq!(cauldron_extinguish(PowderSnow(1)), Water(1));
        assert_eq!(cauldron_extinguish(Lava), Lava);
        assert_eq!(cauldron_extinguish(Empty), Empty);
    }

    #[test]
    fn vanilla_blocks_cauldron_wash_needs_the_stack_components() {
        use CauldronState::*;
        // `CauldronInteraction.BANNER`: an undecorated banner is a PASS and
        // costs no water.
        assert_eq!(
            cauldron_use_stack(Water(3), "minecraft:red_banner", 0, false),
            None
        );
        // `CauldronInteraction.DYED_ITEM`: only a dyed `#minecraft:dyeable`
        // stack is washed; an undyed one is a PASS and costs no water.
        assert_eq!(
            cauldron_use_stack(Water(3), "minecraft:leather_chestplate", 0, false),
            None
        );
        assert_eq!(
            cauldron_use_stack(Water(3), "minecraft:leather_boots", 0, false),
            None
        );
        // A decorated banner loses one pattern layer but keeps its base colour
        // and its item, and the wash costs exactly one water level.
        let decorated = cauldron_use_stack(Water(3), "minecraft:red_banner", 2, false).unwrap();
        assert_eq!(decorated.new_state, Water(2));
        assert_eq!(decorated.returns.as_deref(), Some("minecraft:red_banner"));
        let last = cauldron_use_stack(Water(1), "minecraft:blue_banner", 1, false).unwrap();
        assert_eq!(last.new_state, Empty);
        assert_eq!(last.returns.as_deref(), Some("minecraft:blue_banner"));
        // A dyed dyable item is cleared and keeps its item id.
        let dyed = cauldron_use_stack(Water(2), "minecraft:leather_helmet", 0, true).unwrap();
        assert_eq!(dyed.new_state, Water(1));
        assert_eq!(dyed.returns.as_deref(), Some("minecraft:leather_helmet"));
        // A stack outside `#minecraft:dyeable` is never washed, whatever the
        // component flag says.
        assert_eq!(
            cauldron_use_stack(Water(3), "minecraft:iron_helmet", 0, true),
            None
        );
        // The name-only entry point still assumes a decorated/dyed stack, which
        // is what a caller that holds only the item id can observe.
        assert_eq!(
            cauldron_use(Water(1), "minecraft:red_banner")
                .unwrap()
                .new_state,
            Empty
        );
        assert_eq!(
            cauldron_use(Water(2), "minecraft:leather_leggings")
                .unwrap()
                .new_state,
            Water(1)
        );
    }

    #[test]
    fn grass_dies_under_an_opaque_cover_regardless_of_light() {
        let input = |covered_opaque, light_above, target_is_dirt, target_above_blocks| {
            GrassSpreadInput {
                covered_opaque,
                light_above,
                target_is_dirt,
                target_above_blocks,
            }
        };
        // `SpreadingSnowyDirtBlock.canBeGrass` is an opacity test: any full cover
        // kills the grass even at light 15, and a bright covered cell dies too.
        assert_eq!(
            eval_grass_spread(input(true, 15, false, false)),
            GrassSpreadResult::DieToDirt
        );
        assert_eq!(
            eval_grass_spread(input(true, 0, false, false)),
            GrassSpreadResult::DieToDirt
        );
        // Without a cover the light level gates only the spread half.
        assert_eq!(
            eval_grass_spread(input(false, 0, false, false)),
            GrassSpreadResult::NoChange
        );
        assert_eq!(
            eval_grass_spread(input(false, 8, true, false)),
            GrassSpreadResult::NoChange
        );
        assert_eq!(
            eval_grass_spread(input(false, 9, true, false)),
            GrassSpreadResult::SpreadToNeighbor
        );
        assert_eq!(
            eval_grass_spread(input(false, 15, true, true)),
            GrassSpreadResult::NoChange
        );
        assert_eq!(
            eval_grass_spread(input(false, 15, false, false)),
            GrassSpreadResult::NoChange
        );
    }

    #[test]
    fn vanilla_blocks_composter_chance_table() {
        assert_eq!(composter_chance("minecraft:wheat_seeds"), Some(0.3));
        assert_eq!(composter_chance("minecraft:oak_leaves"), Some(0.3));
        assert_eq!(composter_chance("minecraft:kelp"), Some(0.3));
        assert_eq!(composter_chance("minecraft:pumpkin_seeds"), Some(0.3));
        assert_eq!(composter_chance("minecraft:melon_seeds"), Some(0.3));
        assert_eq!(composter_chance("minecraft:cactus"), Some(0.5));
        assert_eq!(composter_chance("minecraft:sugar_cane"), Some(0.5));
        assert_eq!(composter_chance("minecraft:melon_slice"), Some(0.5));
        assert_eq!(composter_chance("minecraft:apple"), Some(0.65));
        assert_eq!(composter_chance("minecraft:wheat"), Some(0.65));
        assert_eq!(composter_chance("minecraft:potato"), Some(0.65));
        assert_eq!(composter_chance("minecraft:pumpkin"), Some(0.65));
        assert_eq!(composter_chance("minecraft:baked_potato"), Some(0.85));
        assert_eq!(composter_chance("minecraft:bread"), Some(0.85));
        assert_eq!(composter_chance("minecraft:hay_block"), Some(0.85));
        assert_eq!(composter_chance("minecraft:cookie"), Some(0.85));
        assert_eq!(composter_chance("minecraft:cake"), Some(1.0));
        assert_eq!(composter_chance("minecraft:pumpkin_pie"), Some(1.0));
        // `ComposterBlock.COMPOSTABLES` stops at cake and pumpkin pie at 1.0;
        // bone meal is not compostable in 1.21.1.
        assert_eq!(composter_chance("minecraft:bone_meal"), None);
        assert_eq!(composter_chance("minecraft:stick"), None);
        assert_eq!(composter_chance("minecraft:stone"), None);
        assert_eq!(composter_chance("minecraft:poisonous_potato"), None);
    }

    #[test]
    fn vanilla_blocks_composter_lifecycle_rng_and_comparator() {
        use ComposterInsert::*;
        // Level >= 7 or non-compostable: PASS — the RNG is not advanced for it.
        assert_eq!(composter_insert(7, "minecraft:apple", 0.0), Rejected);
        assert_eq!(composter_insert(8, "minecraft:apple", 0.0), Rejected);
        assert_eq!(composter_insert(0, "minecraft:stick", 0.0), Rejected);
        assert_eq!(composter_insert(0, "minecraft:bone_meal", 0.0), Rejected);
        // The first item always advances: `addItem` skips the roll when the
        // composter is still empty and the item's chance is positive.
        assert_eq!(
            composter_insert(0, "minecraft:wheat_seeds", 0.31),
            Advanced { new_level: 1, schedules_maturation: false }
        );
        assert_eq!(
            composter_insert(0, "minecraft:cake", 0.99),
            Advanced { new_level: 1, schedules_maturation: false }
        );
        // Above level 0 the chance decides: roll >= chance is consumed but
        // does not advance (the roll was still spent).
        assert_eq!(composter_insert(1, "minecraft:wheat_seeds", 0.31), ConsumedNoAdvance);
        // roll < chance: level advances.
        assert_eq!(
            composter_insert(1, "minecraft:wheat_seeds", 0.29),
            Advanced { new_level: 2, schedules_maturation: false }
        );
        // roll == chance fails (strict <).
        assert_eq!(composter_insert(1, "minecraft:cake", 1.0), ConsumedNoAdvance);
        // Reaching 7 schedules the ~20-tick maturation delay.
        assert_eq!(
            composter_insert(6, "minecraft:cake", 0.5),
            Advanced { new_level: 7, schedules_maturation: true }
        );
        assert_eq!(composter_mature(7), 8);
        assert_eq!(composter_mature(6), 6);
        assert_eq!(composter_harvest(8), Some((0, "minecraft:bone_meal")));
        assert_eq!(composter_harvest(7), None);
        assert_eq!(
            (0..=8).map(composter_comparator).collect::<Vec<_>>(),
            (0u8..=8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn vanilla_blocks_respawn_anchor_use_and_comparator() {
        use AnchorOutcome::*;
        assert_eq!(respawn_anchor_use(0, Some("minecraft:glowstone"), true), Charged { charges: 1 });
        assert_eq!(respawn_anchor_use(3, Some("minecraft:glowstone"), true), Charged { charges: 4 });
        assert_eq!(respawn_anchor_use(4, Some("minecraft:glowstone"), true), Full);
        // Nether: charged anchor sets the respawn point; charge is kept until
        // the player actually respawns.
        assert_eq!(respawn_anchor_use(0, None, true), Pass);
        assert_eq!(respawn_anchor_use(2, None, true), RespawnSet);
        assert_eq!(respawn_anchor_use(2, Some("minecraft:stick"), true), RespawnSet);
        // Overworld/End: charged anchor explodes at power 5.0.
        assert_eq!(respawn_anchor_use(1, None, false), Explode { power: 5.0 });
        assert_eq!(respawn_anchor_use(4, Some("minecraft:diamond"), false), Explode { power: 5.0 });
        assert_eq!(respawn_anchor_use(0, None, false), Pass);
        // Respawn consumption.
        assert_eq!(respawn_anchor_consume(4), 3);
        assert_eq!(respawn_anchor_consume(1), 0);
        // Comparator table [0, 3, 7, 11, 15].
        assert_eq!(
            (0..=4).map(respawn_anchor_comparator).collect::<Vec<_>>(),
            vec![0, 3, 7, 11, 15]
        );
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn vanilla_blocks_lodestone_bind_and_break() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let reg = reg();
        let dim = DimId::End;
        let pos = (24400, 120, 24400);
        set_block_dim(dim, pos.0, pos.1, pos.2, state("minecraft:lodestone"));
        let mut tracker = LodestoneTracker::default();
        assert!(!tracker.validate(reg));
        let target = tracker.bind(dim, pos);
        assert_eq!(target, LodestoneTarget { dim, pos });
        assert!(tracker.validate(reg));
        // Breaking the lodestone clears the binding (compass spins).
        set_block_dim(dim, pos.0, pos.1, pos.2, state("minecraft:air"));
        assert!(!tracker.validate(reg));
        assert_eq!(tracker.target, None);
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn vanilla_blocks_sponge_bfs_depth_and_connectivity() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let sponge = (24500, 210, 24500);
        set_block_dim(dim, sponge.0, sponge.1, sponge.2, state("minecraft:sponge"));
        // A line of water along +x, plus a disconnected pool behind an air gap.
        for i in 1..=10 {
            set_block_dim(dim, sponge.0 + i, sponge.1, sponge.2, state("minecraft:water"));
        }
        set_block_dim(dim, sponge.0 - 2, sponge.1, sponge.2, state("minecraft:water"));
        set_block_dim(dim, sponge.0 - 1, sponge.1, sponge.2, state("minecraft:air"));
        let absorbed = sponge_absorb_water(dim, sponge);
        // Contiguous water absorbed through Manhattan depth <= 6.
        for i in 1..=6 {
            assert!(absorbed.contains(&(sponge.0 + i, sponge.1, sponge.2)));
            assert_eq!(name_at(dim, sponge.0 + i, sponge.1, sponge.2), "minecraft:air");
        }
        // Depth 7 is never reached.
        assert!(!absorbed.contains(&(sponge.0 + 7, sponge.1, sponge.2)));
        assert_eq!(name_at(dim, sponge.0 + 7, sponge.1, sponge.2), "minecraft:water");
        // Disconnected water behind an air gap is not absorbed.
        assert!(!absorbed.contains(&(sponge.0 - 2, sponge.1, sponge.2)));
        assert_eq!(name_at(dim, sponge.0 - 2, sponge.1, sponge.2), "minecraft:water");
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn vanilla_blocks_sponge_visited_cap_and_waterlogged() {
        // Writes into the shared live `world()` overlay — must
        // not run beside other world fixtures.
        let _guard = world_guard();
        let reg = reg();
        let dim = DimId::Overworld;
        let sponge = (24600, 210, 24600);
        set_block_dim(dim, sponge.0, sponge.1, sponge.2, state("minecraft:sponge"));
        // A 5x5x5 water cube swallows the sponge; only 65 cells may be visited.
        for dx in -2..=2 {
            for dy in -2..=2 {
                for dz in -2..=2 {
                    if dx == 0 && dy == 0 && dz == 0 {
                        continue;
                    }
                    set_block_dim(dim, sponge.0 + dx, sponge.1 + dy, sponge.2 + dz,
                        state("minecraft:water"));
                }
            }
        }
        let absorbed = sponge_absorb_water(dim, sponge);
        assert!(absorbed.len() <= 64, "root + <=64 absorbed under the 65-cell cap");
        assert!(!absorbed.is_empty());
        let remaining_water = (-2..=2)
            .flat_map(|dx| (-2..=2).flat_map(move |dy| (-2..=2).map(move |dz| (dx, dy, dz))))
            .filter(|&(dx, dy, dz)| {
                name_at(dim, sponge.0 + dx, sponge.1 + dy, sponge.2 + dz) == "minecraft:water"
            })
            .count();
        assert!(remaining_water > 0, "cap leaves unabsorbed water behind");

        // A waterlogged slab is dried in place, not removed.
        let slab = reg.with_prop(state("minecraft:oak_slab"), "waterlogged", "true");
        let at2 = (24620, 210, 24620);
        set_block_dim(dim, at2.0, at2.1, at2.2, state("minecraft:sponge"));
        set_block_dim(dim, at2.0 + 1, at2.1, at2.2, slab);
        let absorbed2 = sponge_absorb_water(dim, at2);
        assert!(absorbed2.contains(&(at2.0 + 1, at2.1, at2.2)));
        assert_eq!(name_at(dim, at2.0 + 1, at2.1, at2.2), "minecraft:oak_slab");
        assert_eq!(
            reg.get_prop(block_at_dim(dim, at2.0 + 1, at2.1, at2.2), "waterlogged").unwrap(),
            "false"
        );
    }

    #[test]
    fn vanilla_blocks_sponge_nether_dry() {
        assert!(sponge_nether_dries(DimId::Nether));
        assert!(!sponge_nether_dries(DimId::Overworld));
        assert!(!sponge_nether_dries(DimId::End));
    }

    /// XPOLL-REDSTONE-TICKS: the sponge BFS rides the bounded thread-local
    /// scratch; two identical water shells must absorb in the identical
    /// relative BFS order, and the retained scratch stays tiny.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn xpoll_redstone_ticks_sponge_bfs_reuses_scratch_bit_identical() {
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let shell = |origin: BlockPos| {
            for dx in -2..=2 {
                for dy in -2..=2 {
                    for dz in -2..=2 {
                        if dx == 0 && dy == 0 && dz == 0 {
                            continue;
                        }
                        set_block_dim(
                            dim,
                            origin.0 + dx,
                            origin.1 + dy,
                            origin.2 + dz,
                            state("minecraft:water"),
                        );
                    }
                }
            }
        };
        let sponge_a = (24700, 210, 24700);
        let sponge_b = (24800, 210, 24800);
        set_block_dim(dim, sponge_a.0, sponge_a.1, sponge_a.2, state("minecraft:sponge"));
        shell(sponge_a);
        let first = sponge_absorb_water(dim, sponge_a);
        set_block_dim(dim, sponge_b.0, sponge_b.1, sponge_b.2, state("minecraft:sponge"));
        shell(sponge_b);
        let second = sponge_absorb_water(dim, sponge_b);

        let relative = |sponge: BlockPos, absorbed: &[BlockPos]| -> Vec<(i32, i32, i32)> {
            absorbed
                .iter()
                .map(|p| (p.0 - sponge.0, p.1 - sponge.1, p.2 - sponge.2))
                .collect()
        };
        assert!(!first.is_empty());
        assert_eq!(
            relative(sponge_a, &first),
            relative(sponge_b, &second),
            "reused BFS scratch changed the absorption order"
        );
        // Vanilla bounds the walk at 65 visited cells (stack-allocated
        // `flowing_fluid::SpongeAbsorb`, zero heap scratch).
        assert!(first.len() < flowing_fluid::SPONGE_MAX_CELLS);
        assert!(second.len() < flowing_fluid::SPONGE_MAX_CELLS);
    }

    /// XPOLL-REDSTONE-TICKS: recomputing an unchanged instrument must not
    /// rewrite the block or queue a neighbor broadcast; a real change still
    /// does, and the next identical refresh is silent again.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn xpoll_redstone_ticks_unchanged_state_skips_broadcast() {
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (24900, 210, 24900);
        set_block_dim(dim, pos.0, pos.1, pos.2, state("minecraft:note_block"));
        set_block_dim(dim, pos.0, pos.1 - 1, pos.2, state("minecraft:air"));
        let section = (
            dim,
            pos.0.div_euclid(16),
            pos.1.div_euclid(16),
            pos.2.div_euclid(16),
        );
        let pending_len = || {
            pending_blocks()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&section)
                .map_or(0, |entries| entries.len())
        };

        let before = pending_len();
        assert_eq!(
            note_block_refresh_instrument_dim(dim, pos),
            Some(NoteBlockInstrument::Harp),
            "air below resolves to the default harp instrument"
        );
        assert_eq!(
            pending_len(),
            before,
            "an unchanged instrument queued a redundant block update"
        );

        set_block_dim(dim, pos.0, pos.1 - 1, pos.2, state("minecraft:gold_block"));
        assert_eq!(
            note_block_refresh_instrument_dim(dim, pos),
            Some(NoteBlockInstrument::Bell)
        );
        let changed = pending_len();
        assert!(changed > before, "a real instrument change still broadcasts");
        assert_eq!(
            note_block_refresh_instrument_dim(dim, pos),
            Some(NoteBlockInstrument::Bell)
        );
        assert_eq!(
            pending_len(),
            changed,
            "the settled instrument must not rebroadcast"
        );
    }

    /// XPOLL-REDSTONE-TICKS: the per-tick block-entity pass reuses its
    /// bounded scratch; two identical ticks over an identical registry emit
    /// identical events and the scratch stays under the 256 KiB bound.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn xpoll_redstone_ticks_be_tick_scratch_bounded_and_deterministic() {
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let campfire = || {
            VanillaBlockEntity::Campfire(CampfireBlockEntity {
                slots: std::array::from_fn(|i| {
                    (i == 0).then(|| ("minecraft:beef".to_string(), 0, 1))
                }),
            })
        };
        let positions = [(25200, 210, 25200), (25201, 210, 25200), (25202, 210, 25200)];
        for (i, pos) in positions.iter().enumerate() {
            let (entity, block) = match i {
                0 => (
                    campfire(),
                    reg().with_prop(
                        reg().state_of("minecraft:campfire", None),
                        "lit",
                        "true",
                    ),
                ),
                1 => (
                    VanillaBlockEntity::Jukebox(JukeboxBlockEntity::default()),
                    reg().state_of("minecraft:jukebox", None),
                ),
                _ => (
                    VanillaBlockEntity::Crafter(CrafterBlockEntity::default()),
                    reg().state_of("minecraft:crafter", None),
                ),
            };
            register_world_block_entity(dim, *pos, entity);
            set_block_dim(dim, pos.0, pos.1, pos.2, block);
        }

        let first = tick_world_block_entities(dim, 7, &[], 42);
        // Re-arm the finished campfire slot so the second tick is identical.
        register_world_block_entity(dim, positions[0], campfire());
        let second = tick_world_block_entities(dim, 7, &[], 42);
        assert_eq!(first, second, "reused scratch changed the tick's events");
        assert!(
            first.iter().any(|event| matches!(event, WorldBlockEntityEvent::Cooked { .. })),
            "the lit campfire cooked through the live tick"
        );
        BE_TICK_SCRATCH.with(|cell| {
            let scratch = cell.borrow();
            assert!(
                scratch.live_mobs.capacity() * std::mem::size_of::<(i64, [f64; 3])>()
                    <= 256 * 1024
            );
            assert!(
                scratch.crafter_power.capacity() * std::mem::size_of::<(BlockPos, bool)>()
                    <= 256 * 1024
            );
        });
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod parity_live_wire_world_tests {
    use super::*;

    #[test]
    fn parity_live_wire_world_mechanisms() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let reg = crate::block_registry::breg().unwrap();
        let (x, y, z) = (34_000, 90, 34_000);

        // Composter: the live random-tick dispatcher (`p2_plant_random_tick`,
        // called by `on_random_tick_dim` in main.rs) matures a level-7
        // composter to its harvestable level 8 through composter_random_tick.
        let composter7 = reg.with_prop(
            reg.state_of("minecraft:composter", None),
            "level",
            "7",
        );
        set_block_dim(DimId::Overworld, x, y, z, composter7);
        assert!(p2_plant_random_tick(reg, DimId::Overworld, x, y, z, composter7));
        assert_eq!(
            reg.get_prop(block_at_dim(DimId::Overworld, x, y, z), "level").as_deref(),
            Some("8"),
            "level-7 composter matures through the live random tick",
        );

        // Wet sponge: carried into the Nether it dries through the same arm.
        let wet = reg.state_of("minecraft:wet_sponge", None);
        set_block_dim(DimId::Nether, x, y, z, wet);
        assert!(p2_plant_random_tick(reg, DimId::Nether, x, y, z, wet));
        assert_eq!(
            reg.name_of(block_at_dim(DimId::Nether, x, y, z)),
            "minecraft:sponge",
            "wet sponge dries in the Nether through the live random tick",
        );

        // Turtle and sniffer eggs: the dispatcher routes them into the live
        // incubation path (`egg_random_tick` -> `tick_incubation`).
        for egg in ["minecraft:turtle_egg", "minecraft:sniffer_egg"] {
            let st = reg.state_of(egg, None);
            set_block_dim(DimId::Overworld, x + 4, y, z, st);
            assert!(
                p2_plant_random_tick(reg, DimId::Overworld, x + 4, y, z, st),
                "{egg} must be claimed by the live random-tick dispatch",
            );
        }

        // Cauldron and dripstone arms claim their blocks even when the
        // weather/lava rolls produce no fill this tick.
        let cauldron = reg.state_of("minecraft:cauldron", None);
        set_block_dim(DimId::Overworld, x + 8, y, z, cauldron);
        assert!(p2_plant_random_tick(reg, DimId::Overworld, x + 8, y, z, cauldron));
        let dripstone = reg.with_prop(
            reg.state_of("minecraft:pointed_dripstone", None),
            "thickness",
            "tip",
        );
        set_block_dim(DimId::Overworld, x + 8, y + 2, z, dripstone);
        assert!(p2_plant_random_tick(reg, DimId::Overworld, x + 8, y + 2, z, dripstone));

        // Comparator analog-output fallback (`getAnalogOutputSignal`): blocks
        // without a block-entity record read the signal straight off state —
        // composter level and respawn-anchor charges.
        let composter3 = reg.with_prop(composter7, "level", "3");
        set_block_dim(DimId::Overworld, x, y + 4, z, composter3);
        assert_eq!(
            comparator_signal_at_be(DimId::Overworld, (x, y + 4, z)),
            Some(composter_comparator(3)),
        );
        let anchor2 = reg.with_prop(
            reg.state_of("minecraft:respawn_anchor", None),
            "charges",
            "2",
        );
        set_block_dim(DimId::Nether, x, y + 4, z, anchor2);
        assert_eq!(
            comparator_signal_at_be(DimId::Nether, (x, y + 4, z)),
            Some(respawn_anchor_comparator(2)),
        );

        // Note block: `note_block_power_dim` — the power-state write the live
        // wind-charge trigger performs — runs `note_block_neighbor_update`;
        // only a rising edge plays the note.
        let note = reg.state_of("minecraft:note_block", None);
        set_block_dim(DimId::Overworld, x, y + 6, z, note);
        assert_eq!(
            note_block_power_dim(DimId::Overworld, (x, y + 6, z), true),
            Some((true, true)),
        );
        assert_eq!(
            note_block_power_dim(DimId::Overworld, (x, y + 6, z), true),
            Some((true, false)),
            "a held-high signal is not a new trigger",
        );

        // Lazy block-entity reconcile: the dispatcher materializes a jukebox
        // record the first time a live tick visits its cell, and the
        // comparator then reads the entity signal instead of state.
        let jukebox = reg.state_of("minecraft:jukebox", None);
        set_block_dim(DimId::Overworld, x, y + 8, z, jukebox);
        p2_plant_random_tick(reg, DimId::Overworld, x, y + 8, z, jukebox);
        assert!(
            comparator_signal_at_be(DimId::Overworld, (x, y + 8, z)).is_some(),
            "the reconciled jukebox record answers comparator queries",
        );
    }
    // ---- v1211_adv_loot: vanilla 1.21.1 advancement catalogue ----

    #[test]
    fn v1211_adv_loot_catalogue_covers_all_five_vanilla_tabs() {
        // The complete 1.21.1 advancement set: 16 story + 24 nether + 9 end +
        // 44 adventure + 29 husbandry.
        assert_eq!(CORE_ADVANCEMENTS.len(), 122);
        for tab in ["story", "nether", "end", "adventure", "husbandry"] {
            let prefix = format!("minecraft:{tab}/");
            assert!(
                CORE_ADVANCEMENTS.iter().any(|d| d.id.starts_with(&prefix)),
                "tab {tab} missing from the catalogue"
            );
        }
        // Every definition is ordered parent-before-child and every parent
        // resolves inside the catalogue.
        let mut placed = std::collections::HashSet::new();
        for definition in CORE_ADVANCEMENTS {
            if let Some(parent) = definition.parent {
                assert!(
                    placed.contains(parent),
                    "{} listed before its parent {parent}",
                    definition.id
                );
            }
            placed.insert(definition.id);
        }
        assert_eq!(placed.len(), 122, "duplicate advancement ids");
        // Every requirement group is non-empty and names real criteria.
        for definition in CORE_ADVANCEMENTS {
            assert!(!definition.requirements.is_empty(), "{}", definition.id);
            for group in definition.requirements {
                assert!(!group.is_empty(), "{} has an empty group", definition.id);
                for criterion in *group {
                    assert!(!criterion.is_empty(), "{} empty criterion", definition.id);
                }
            }
        }
        // Every vanilla gameplay advancement is live: the default definition
        // set is the whole catalogue, not a prefix of it.
        let live = AdvancementTracker::default();
        assert_eq!(live.definitions.len(), 122, "live advancement coverage");
        assert_eq!(
            live.definitions.iter().map(|d| d.id).collect::<Vec<_>>(),
            CORE_ADVANCEMENTS.iter().map(|d| d.id).collect::<Vec<_>>(),
            "the live set is the whole catalogue, in catalogue order"
        );
        for root in [
            "minecraft:story/root",
            "minecraft:nether/root",
            "minecraft:end/root",
            "minecraft:adventure/root",
            "minecraft:husbandry/root",
        ] {
            assert!(
                live.definition(root).is_some(),
                "tab root {root} is grantable"
            );
        }
    }

    #[test]
    fn v1211_adv_loot_trigger_firing_completion_and_parent_chain() {
        let mut tracker = AdvancementTracker::default();
        // adventure/root fires on either kill direction (one OR group).
        assert!(tracker.grant_criterion(
            "Alex",
            "minecraft:adventure/root",
            "killed_something",
            10
        ));
        assert!(tracker.completed("Alex", "minecraft:adventure/root"));
        // adventure/kill_a_mob completes on any single mob criterion; its
        // parent must already be complete for `completed` to see it.
        assert!(!tracker.completed("Alex", "minecraft:adventure/kill_a_mob"));
        assert!(tracker.grant_criterion(
            "Alex",
            "minecraft:adventure/kill_a_mob",
            "minecraft:creeper",
            11
        ));
        assert!(tracker.completed("Alex", "minecraft:adventure/kill_a_mob"));
        // kill_all_mobs requires every mob group; grant them all.
        let mobs = [
            "minecraft:blaze", "minecraft:bogged", "minecraft:breeze",
            "minecraft:cave_spider", "minecraft:creeper", "minecraft:drowned",
            "minecraft:elder_guardian", "minecraft:ender_dragon",
            "minecraft:enderman", "minecraft:endermite", "minecraft:evoker",
            "minecraft:ghast", "minecraft:guardian", "minecraft:hoglin",
            "minecraft:husk", "minecraft:magma_cube", "minecraft:phantom",
            "minecraft:piglin", "minecraft:piglin_brute", "minecraft:pillager",
            "minecraft:ravager", "minecraft:shulker", "minecraft:silverfish",
            "minecraft:skeleton", "minecraft:slime", "minecraft:spider",
            "minecraft:stray", "minecraft:vex", "minecraft:vindicator",
            "minecraft:witch", "minecraft:wither_skeleton", "minecraft:wither",
            "minecraft:zoglin", "minecraft:zombie_villager", "minecraft:zombie",
            "minecraft:zombified_piglin",
        ];
        for mob in mobs {
            assert!(tracker.grant_criterion(
                "Alex",
                "minecraft:adventure/kill_all_mobs",
                mob,
                12
            ));
        }
        assert!(tracker.completed("Alex", "minecraft:adventure/kill_all_mobs"));
        // A criterion outside the requirement groups is rejected.
        assert!(!tracker.grant_criterion(
            "Alex",
            "minecraft:adventure/kill_all_mobs",
            "not_a_mob",
            13
        ));
        // Parent-chain traversal on the catalogue: grant_until walks
        // whole_pack -> tame_an_animal -> root.
        let mut chain = AdvancementTracker::default();
        // whole_pack's nine wolf-variant groups plus the two single-criterion
        // ancestors = 11 criterion grants.
        assert_eq!(
            chain.grant_until("Bob", "minecraft:husbandry/whole_pack", 20),
            11
        );
        for id in [
            "minecraft:husbandry/root",
            "minecraft:husbandry/tame_an_animal",
            "minecraft:husbandry/whole_pack",
        ] {
            assert!(chain.completed("Bob", id), "{id}");
        }
        assert!(!chain.completed("Bob", "minecraft:husbandry/breed_an_animal"));
        // has_ancestor resolves deep catalogue chains.
        assert!(chain.has_ancestor(
            "minecraft:adventure/spyglass_at_dragon",
            "minecraft:adventure/root"
        ));
        assert!(!chain.has_ancestor(
            "minecraft:adventure/spyglass_at_dragon",
            "minecraft:husbandry/root"
        ));
    }

    #[test]
    fn v1211_adv_loot_xp_rewards_and_trigger_registry() {
        // Vanilla 1.21.1 rewards.experience payouts.
        for (id, xp) in [
            ("minecraft:nether/all_effects", 1000),
            ("minecraft:nether/explore_nether", 500),
            ("minecraft:adventure/adventuring_time", 500),
            ("minecraft:adventure/kill_all_mobs", 100),
            ("minecraft:adventure/hero_of_the_village", 100),
            ("minecraft:end/levitate", 50),
            ("minecraft:husbandry/balanced_diet", 100),
            ("minecraft:husbandry/whole_pack", 50),
        ] {
            assert_eq!(
                advancement_reward_spec(id).map(|spec| spec.experience),
                Some(xp),
                "{id} XP reward"
            );
        }
        // Advancements without a vanilla rewards block stay unspecified, and no
        // story milestone has one: 1.21.1 ships zero `rewards` blocks under
        // `data/minecraft/advancement/story/`.
        assert!(advancement_reward_spec("minecraft:adventure/root").is_none());
        for definition in CORE_ADVANCEMENTS
            .iter()
            .filter(|definition| definition.id.starts_with("minecraft:story/"))
        {
            assert!(
                advancement_reward_spec(definition.id).is_none(),
                "{} must not fabricate rewards",
                definition.id
            );
        }
        // Every trigger type the 1.21.1 `CriterionTriggers` registry holds, with
        // and without the minecraft: prefix. The literal list pins the registry
        // so a typo or an invented trigger fails here instead of silently
        // rejecting a criterion at runtime.
        const EXPECTED_TRIGGERS: &[&str] = &[
            "minecraft:allay_drop_item_on_block",
            "minecraft:any_block_use",
            "minecraft:avoid_vibration",
            "minecraft:bee_nest_destroyed",
            "minecraft:bred_animals",
            "minecraft:brewed_potion",
            "minecraft:changed_dimension",
            "minecraft:channeled_lightning",
            "minecraft:construct_beacon",
            "minecraft:consume_item",
            "minecraft:crafter_recipe_crafted",
            "minecraft:cured_zombie_villager",
            "minecraft:default_block_use",
            "minecraft:effects_changed",
            "minecraft:enchanted_item",
            "minecraft:enter_block",
            "minecraft:entity_hurt_player",
            "minecraft:entity_killed_player",
            "minecraft:fall_after_explosion",
            "minecraft:fall_from_height",
            "minecraft:filled_bucket",
            "minecraft:fishing_rod_hooked",
            "minecraft:hero_of_the_village",
            "minecraft:impossible",
            "minecraft:inventory_changed",
            "minecraft:item_durability_changed",
            "minecraft:item_used_on_block",
            "minecraft:kill_mob_near_sculk_catalyst",
            "minecraft:killed_by_crossbow",
            "minecraft:levitation",
            "minecraft:lightning_strike",
            "minecraft:location",
            "minecraft:nether_travel",
            "minecraft:placed_block",
            "minecraft:player_generates_container_loot",
            "minecraft:player_hurt_entity",
            "minecraft:player_interacted_with_entity",
            "minecraft:player_killed_entity",
            "minecraft:recipe_crafted",
            "minecraft:recipe_unlocked",
            "minecraft:ride_entity_in_lava",
            "minecraft:shot_crossbow",
            "minecraft:slept_in_bed",
            "minecraft:slide_down_block",
            "minecraft:started_riding",
            "minecraft:summoned_entity",
            "minecraft:tame_animal",
            "minecraft:target_hit",
            "minecraft:thrown_item_picked_up_by_entity",
            "minecraft:thrown_item_picked_up_by_player",
            "minecraft:tick",
            "minecraft:used_ender_eye",
            "minecraft:used_totem",
            "minecraft:using_item",
            "minecraft:villager_trade",
            "minecraft:voluntary_exile",
        ];
        assert_eq!(VANILLA_ADVANCEMENT_TRIGGERS, EXPECTED_TRIGGERS);
        for trigger in EXPECTED_TRIGGERS {
            assert!(advancement_trigger_supported(trigger), "{trigger}");
            assert!(
                advancement_trigger_supported(trigger.trim_start_matches("minecraft:")),
                "bare {trigger}"
            );
        }
        // The three registry entries the old table dropped are real triggers.
        for trigger in [
            "minecraft:recipe_unlocked",
            "minecraft:tick",
            "minecraft:impossible",
        ] {
            assert!(advancement_trigger_supported(trigger), "{trigger}");
        }
        assert!(!advancement_trigger_supported("minecraft:not_a_trigger"));
        assert!(!advancement_trigger_supported("not_a_trigger"));
    }

}

/// Sparse wake scheduler for live chunk activity (`megu_core::quiescent`).
///
/// Every live mutation enters through `quiescent_note_block_mutation`: the
/// canonical section/chunk revision bump, horizontal border wakes, and the
/// lazy-schedule backoff reset share one ingress. Tick-scoped systems then
/// read dense active slices instead of scanning loaded-but-idle columns.
fn quiescent_scheduler() -> &'static Mutex<megu_core::quiescent::QuiescentChunkScheduler> {
    static SCHEDULER: OnceLock<Mutex<megu_core::quiescent::QuiescentChunkScheduler>> =
        OnceLock::new();
    SCHEDULER.get_or_init(|| Mutex::new(megu_core::quiescent::QuiescentChunkScheduler::new()))
}

/// Generic live-mutation wake bit. Vanilla categories are added when the old
/// or replacement state identifies a more specific owner.
fn quiescent_mutation_bit() -> megu_core::quiescent::ActivityMask {
    megu_core::quiescent::ActivityMask::custom(6)
}

/// Wake bits introduced by the block-mutation edge itself. They are valid for
/// the current scheduler round and are cleared on the next one; long-running
/// systems must re-wake chunks from their own persistent state rather than
/// relying on one historical edit to keep the column active forever.
fn quiescent_transient_mutation_bits() -> megu_core::quiescent::ActivityMask {
    use megu_core::quiescent::ActivityMask;
    quiescent_mutation_bit() | ActivityMask::FLUID | ActivityMask::REDSTONE | ActivityMask::BLOCK_ENTITY
}

fn quiescent_activity_for_mutation(
    reg: Option<&crate::block_registry::BReg>,
    dim: DimId,
    pos: (i32, i32, i32),
    old: u32,
    new: u32,
) -> megu_core::quiescent::ActivityMask {
    use megu_core::quiescent::ActivityMask;
    let mut bits = quiescent_mutation_bit();
    let Some(reg) = reg else {
        return bits;
    };
    if redstone_component(reg, old)
        || redstone_component(reg, new)
        || nearby_redstone(reg, dim, RsPos(pos.0, pos.1, pos.2))
    {
        bits.insert(ActivityMask::REDSTONE);
    }
    for state in [old, new] {
        let name = reg.name_of(state);
        if matches!(name, "minecraft:water" | "minecraft:lava")
            || name.ends_with("_water")
            || name.ends_with("_lava")
        {
            bits.insert(ActivityMask::FLUID);
        }
        if name.contains("sign")
            || name.contains("chest")
            || name.contains("furnace")
            || name.contains("hopper")
            || name.contains("spawner")
            || name.contains("beacon")
        {
            bits.insert(ActivityMask::BLOCK_ENTITY);
        }
    }
    bits
}

/// Wake the owning chunk and every touched horizontal border neighbor, bump
/// the shared revision spine exactly once, invalidate the shared chunk-packet
/// backing cache, and reset the documented lazy-schedule backoff cell plus
/// its six neighbors. Returns the canonical `(section_rev, chunk_rev)` pair.
fn quiescent_note_block_mutation(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    old: u32,
    new: u32,
) -> (u64, u64) {
    let bits = quiescent_activity_for_mutation(breg(), dim, (x, y, z), old, new);
    let dim_index = megu_core::coords::DimIndex(dim.raw());
    let revs = {
        let mut rev_table = shared_section_revs()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        quiescent_scheduler()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_block_mutated(dim_index, x, y, z, bits, &mut rev_table)
    };
    #[cfg(feature = "mod-suite")]
    state_chunk_packet_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .bump_block(dim.raw(), x, z);

    // The backoff contract above the registry says any neighbor block change
    // invalidates the cell back to full rate. The scheduler wake is the
    // mutation ingress for that reset; removing exact positions keeps the
    // reset precise rather than chunk-coarse.
    let mut backoff = backoff().lock().unwrap_or_else(|e| e.into_inner());
    if !backoff.is_empty() {
        backoff.remove(&(dim, x, y, z));
        for (dx, dy, dz) in LEAF_NEIGHBORS {
            backoff.remove(&(dim, x + dx, y + dy, z + dz));
        }
    }
    revs
}

fn quiescent_random_snapshot_store() -> &'static Mutex<Vec<(DimId, i32, i32)>> {
    static SNAPSHOT: OnceLock<Mutex<Vec<(DimId, i32, i32)>>> = OnceLock::new();
    SNAPSHOT.get_or_init(|| Mutex::new(Vec::new()))
}

fn quiescent_random_tick_chunks() -> Vec<(DimId, i32, i32)> {
    quiescent_random_snapshot_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Advance the scheduler once per server tick and publish the dense active
/// random-tick slice. Chunks outside the hot/forceloaded input are never
/// inserted at all; a chunk leaving the input has its random-tick bit removed
/// and physically leaves the dense set when no other wake source remains.
fn quiescent_chunk_tick() {
    use megu_core::coords::{ChunkKey, DimIndex};
    use megu_core::quiescent::ActivityMask;

    let ticked = ticked_chunks();
    let dims = if ticked.is_empty() {
        Vec::new()
    } else {
        DimId::all_registered()
    };
    let mut wanted = std::collections::HashSet::with_capacity(ticked.len() * dims.len());
    let mut wanted_keys = Vec::with_capacity(ticked.len() * dims.len());
    for dim in &dims {
        let dim_index = DimIndex(dim.raw());
        for &(cx, cz) in &ticked {
            let key = ChunkKey { dim: dim_index, cx, cz };
            wanted.insert(key);
            wanted_keys.push(key);
        }
    }

    let mut woken = Vec::new();
    let mut scheduler = quiescent_scheduler()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    scheduler.advance_tick(&mut woken);
    if wanted.is_empty() && scheduler.active_chunks().is_empty() {
        // A published random-tick chunk stays active until this pass clears it.
        return;
    }
    let current_tick = scheduler.current_tick();
    let active: Vec<_> = scheduler.active_chunks().to_vec();
    for entry in active {
        if entry.mask.intersects(ActivityMask::RANDOM_TICK) && !wanted.contains(&entry.key) {
            scheduler.clear_bits(entry.key, ActivityMask::RANDOM_TICK);
        }
        // Mutation wakes are one-tick edge events. The bit is cleared on the
        // next scheduler round so a single edit does not keep a column in the
        // dense active set forever; persistent owners re-wake themselves.
        if entry.last_woken_tick < current_tick {
            scheduler.clear_bits(entry.key, quiescent_transient_mutation_bits());
        }
    }
    for key in wanted_keys {
        scheduler.wake(key, ActivityMask::RANDOM_TICK);
    }
    let mut snapshot: Vec<(DimId, i32, i32)> = scheduler
        .active_chunks()
        .iter()
        .filter(|entry| entry.mask.intersects(ActivityMask::RANDOM_TICK))
        .filter_map(|entry| {
            DimId::from_index(entry.key.dim.0 as usize)
                .map(|dim| (dim, entry.key.cx, entry.key.cz))
        })
        .collect();
    snapshot.sort_by_key(|&(dim, cx, cz)| (dim.raw(), cx, cz));
    *quiescent_random_snapshot_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = snapshot;
}
