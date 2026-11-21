// ---------- Phase P3-B: teams (0x60), /tick (0x71/0x72), /locate, /random,
// /return & .mcfunction macros ----------
//
// This file is `include!`d into main.rs after `commands_execute.rs`, so every
// item here lives in the same crate module as commands_core.rs. The single
// dispatch entry point is `exec_p3b_command`: wiring the five commands into the
// command match is a five-line drop-in once the parallel lanes land.

// ---------------------------------------------------------------------------
// Teams (Vanilla 1.21.1 PlayerTeam) + protocol-767 S2C 0x60 SetPlayerTeam.
// ---------------------------------------------------------------------------

/// Vanilla `ChatFormatting` in wire-id order. Only 0..=15 (the named colours)
/// and 21 (`reset`) are legal team colours; the formatting codes (16..=20) are
/// rejected by `team_color_id` just like the vanilla brigadier suggestion set.
#[allow(dead_code)]
const TEAM_FORMATTING_NAMES: [&str; 22] = [
    "black",
    "dark_blue",
    "dark_green",
    "dark_aqua",
    "dark_red",
    "dark_purple",
    "gold",
    "gray",
    "dark_gray",
    "blue",
    "green",
    "aqua",
    "red",
    "light_purple",
    "yellow",
    "white",
    "obfuscated",
    "bold",
    "strikethrough",
    "underline",
    "italic",
    "reset",
];

/// `Team.Visibility` serialised names, enum order.
#[allow(dead_code)]
const TEAM_NAMETAG_VISIBILITIES: [&str; 4] =
    ["always", "never", "hideForOtherTeams", "hideForOwnTeam"];
/// `Team.CollisionRule` serialised names, enum order.
#[allow(dead_code)]
const TEAM_COLLISION_RULES: [&str; 4] =
    ["always", "never", "pushOtherTeams", "pushOwnTeam"];

/// A named colour or `reset`. Formatting codes 16..=20 are not team colours.
#[allow(dead_code)]
fn team_color_id(name: &str) -> Option<i32> {
    let id = TEAM_FORMATTING_NAMES.iter().position(|n| *n == name)? as i32;
    ((0..=15).contains(&id) || id == 21).then_some(id)
}

/// One vanilla `PlayerTeam`. `display_name`/`prefix`/`suffix` are kept as plain
/// text and serialised as `{"text": ...}` NBT components on the wire.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
struct Team {
    name: String,
    display_name: String,
    /// `ChatFormatting` id 0..=21; `team add` starts at 21 (`reset`).
    color: i32,
    friendly_fire: bool,
    see_friendly_invisibles: bool,
    nametag_visibility: String,
    collision_rule: String,
    prefix: String,
    suffix: String,
    members: BTreeSet<String>,
}

#[allow(dead_code)]
impl Team {
    fn new(name: &str, display_name: Option<&str>) -> Team {
        Team {
            name: name.to_owned(),
            display_name: display_name.unwrap_or(name).to_owned(),
            color: 21,
            friendly_fire: true,
            see_friendly_invisibles: false,
            nametag_visibility: "always".to_owned(),
            collision_rule: "always".to_owned(),
            prefix: String::new(),
            suffix: String::new(),
            members: BTreeSet::new(),
        }
    }
    fn friendly_flags(&self) -> u8 {
        u8::from(self.friendly_fire) | (u8::from(self.see_friendly_invisibles) << 1)
    }
}

/// Teams by name plus the reverse index from member to its single team. Vanilla
/// allows a score holder to sit on at most one team at a time: `join` removes
/// the member from any prior team first.
#[allow(dead_code)]
#[derive(Default)]
struct TeamRegistry {
    teams: BTreeMap<String, Team>,
    member_team: HashMap<String, String>,
}

#[allow(dead_code)]
impl TeamRegistry {
    fn add(&mut self, name: &str, display_name: Option<&str>) -> Result<(), String> {
        if self.teams.contains_key(name) {
            return Err(format!("A team already exists with the name '{name}'"));
        }
        self.teams.insert(name.to_owned(), Team::new(name, display_name));
        Ok(())
    }
    fn remove(&mut self, name: &str) -> Result<Team, String> {
        let team = self
            .teams
            .remove(name)
            .ok_or_else(|| format!("No team with name '{name}'"))?;
        for member in &team.members {
            self.member_team.remove(member);
        }
        Ok(team)
    }
    fn empty(&mut self, name: &str) -> Result<Vec<String>, String> {
        let team = self
            .teams
            .get_mut(name)
            .ok_or_else(|| format!("No team with name '{name}'"))?;
        let members: Vec<String> = team.members.iter().cloned().collect();
        for member in &members {
            self.member_team.remove(member);
        }
        team.members.clear();
        Ok(members)
    }
    /// Returns the team the member was removed from, if any.
    fn join(&mut self, name: &str, member: &str) -> Result<Option<String>, String> {
        if member.is_empty() || member.chars().count() > 40 {
            return Err(format!("'{member}' is not a valid member name"));
        }
        if !self.teams.contains_key(name) {
            return Err(format!("No team with name '{name}'"));
        }
        let previous = self.member_team.get(member).cloned();
        if previous.as_deref() == Some(name) {
            return Ok(None);
        }
        if let Some(old) = &previous {
            if let Some(team) = self.teams.get_mut(old) {
                team.members.remove(member);
            }
        }
        self.teams
            .get_mut(name)
            .expect("checked above")
            .members
            .insert(member.to_owned());
        self.member_team.insert(member.to_owned(), name.to_owned());
        Ok(previous)
    }
    /// Returns the team left, or an error when the member was on no team.
    fn leave(&mut self, member: &str) -> Result<String, String> {
        let Some(old) = self.member_team.remove(member) else {
            return Err(format!("{member} is not on a team"));
        };
        if let Some(team) = self.teams.get_mut(&old) {
            team.members.remove(member);
        }
        Ok(old)
    }
    fn list(&self) -> Vec<&Team> {
        self.teams.values().collect()
    }
    fn get(&self, name: &str) -> Option<&Team> {
        self.teams.get(name)
    }
    fn get_mut(&mut self, name: &str) -> Option<&mut Team> {
        self.teams.get_mut(name)
    }
    fn team_of(&self, member: &str) -> Option<&str> {
        self.member_team.get(member).map(String::as_str)
    }
    /// A `TeamMessage`: the sender must be on a team; every member (including
    /// the sender) gets the `[team] <sender> message` line. Returned as
    /// (recipient, text) pairs so the caller decides how to deliver them.
    fn teammsg(&self, sender: &str, message: &str) -> Result<Vec<(String, String)>, String> {
        let Some(team_name) = self.team_of(sender) else {
            return Err("You are not in a team".to_owned());
        };
        let team = self.teams.get(team_name).expect("member index is consistent");
        Ok(team
            .members
            .iter()
            .map(|member| {
                (
                    member.clone(),
                    format!("[{}] <{}> {}", team.name, sender, message),
                )
            })
            .collect())
    }
}

#[allow(dead_code)]
fn teams() -> &'static Mutex<TeamRegistry> {
    static TEAMS: OnceLock<Mutex<TeamRegistry>> = OnceLock::new();
    TEAMS.get_or_init(|| Mutex::new(TeamRegistry::default()))
}

/// `@a[team=...]` / `spreadplayers respectTeams` hook: the single team a member
/// belongs to. Selector lanes read this when wiring `candidate.team`.
#[allow(dead_code)]
fn p3b_team_of(member: &str) -> Option<String> {
    teams()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .team_of(member)
        .map(str::to_owned)
}

// ----- S2C 0x60 SetPlayerTeam (protocol 767) -----

/// `{"text": ...}` as the anonymousNbt compound the protocol's `writeComponent`
/// emits. Identical layout to `chat_nbt` but named for the team wire path.
#[allow(dead_code)]
fn team_component_nbt(text: &str) -> Vec<u8> {
    chat_nbt(text)
}

/// Modes of `ClientboundSetPlayerTeamPacket`: create, remove, update info,
/// add entities, remove entities.
#[allow(dead_code)]
const TEAM_MODE_ADD_TEAM: u8 = 0;
#[allow(dead_code)]
const TEAM_MODE_REMOVE_TEAM: u8 = 1;
#[allow(dead_code)]
const TEAM_MODE_UPDATE_INFO: u8 = 2;
#[allow(dead_code)]
const TEAM_MODE_ADD_ENTITIES: u8 = 3;
#[allow(dead_code)]
const TEAM_MODE_REMOVE_ENTITIES: u8 = 4;

/// `ClientboundSetPlayerTeamPacket.write`: team name, mode byte, parameter block
/// for modes 0/2, entity name list for modes 0/3/4.
#[allow(dead_code)]
fn set_player_team_packet(
    name: &str,
    mode: u8,
    team: Option<&Team>,
    entities: &[String],
) -> Vec<u8> {
    let mut p = Vec::new();
    write_str(&mut p, name);
    p.push(mode);
    if matches!(mode, TEAM_MODE_ADD_TEAM | TEAM_MODE_UPDATE_INFO) {
        let team = team.expect("modes 0 and 2 carry the parameter block");
        p.extend_from_slice(&team_component_nbt(&team.display_name));
        p.push(team.friendly_flags());
        write_str(&mut p, &team.nametag_visibility);
        write_str(&mut p, &team.collision_rule);
        write_varint(&mut p, i64::from(team.color));
        p.extend_from_slice(&team_component_nbt(&team.prefix));
        p.extend_from_slice(&team_component_nbt(&team.suffix));
    }
    if matches!(
        mode,
        TEAM_MODE_ADD_TEAM | TEAM_MODE_ADD_ENTITIES | TEAM_MODE_REMOVE_ENTITIES
    ) {
        write_varint(&mut p, entities.len() as i64);
        for entity in entities {
            write_str(&mut p, entity);
        }
    }
    p
}

/// What [`decode_set_player_team`] extracts back off the wire: every field the
/// encoder can emit. Parameter fields are `None` for modes that do not carry
/// the parameter block.
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
struct SetPlayerTeamPacket {
    name: String,
    mode: u8,
    display_name: Option<String>,
    friendly_flags: u8,
    nametag_visibility: Option<String>,
    collision_rule: Option<String>,
    color: i32,
    prefix: Option<String>,
    suffix: Option<String>,
    entities: Vec<String>,
}

/// A bounds-checked cursor over one packet body. Every read returns `None` on
/// truncation so a malformed 0x60/0x72 fails closed instead of panicking.
#[allow(dead_code)]
struct P3bReader<'a> {
    data: &'a [u8],
    p: usize,
}

#[allow(dead_code)]
impl<'a> P3bReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        P3bReader { data, p: 0 }
    }
    fn done(&self) -> bool {
        self.p == self.data.len()
    }
    fn byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.p)?;
        self.p += 1;
        Some(b)
    }
    fn varint(&mut self) -> Option<i64> {
        let mut v = 0i64;
        for i in 0..5 {
            let byte = *self.data.get(self.p)?;
            self.p += 1;
            v |= ((byte & 0x7f) as i64) << (7 * i);
            if byte & 0x80 == 0 {
                return Some(v as u32 as i64);
            }
        }
        None
    }
    fn utf(&mut self) -> Option<String> {
        let len = self.varint()?;
        if len < 0 || len > (1 << 20) {
            return None;
        }
        let bytes = self.data.get(self.p..self.p + len as usize)?;
        self.p += len as usize;
        std::str::from_utf8(bytes).ok().map(str::to_owned)
    }
    /// Extract the `text` member of one anonymousNbt component: a compound
    /// holding `{"text": s}` (what the encoder emits) or a bare TAG_String.
    /// Other members are skipped through the shared wire skip so extras decode.
    fn nbt_text(&mut self) -> Option<String> {
        let tag = self.byte()?;
        match tag {
            8 => {
                let n = u16::from_be_bytes(
                    self.data.get(self.p..self.p + 2)?.try_into().ok()?,
                ) as usize;
                self.p += 2;
                let s = std::str::from_utf8(self.data.get(self.p..self.p + n)?).ok()?;
                self.p += n;
                Some(s.to_owned())
            }
            10 => {
                let mut text = String::new();
                loop {
                    let child = self.byte()?;
                    if child == 0 {
                        return Some(text);
                    }
                    let nlen = u16::from_be_bytes(
                        self.data.get(self.p..self.p + 2)?.try_into().ok()?,
                    ) as usize;
                    self.p += 2;
                    let name = std::str::from_utf8(self.data.get(self.p..self.p + nlen)?)
                        .ok()?
                        .to_owned();
                    self.p += nlen;
                    if child == 8 && name == "text" {
                        let n = u16::from_be_bytes(
                            self.data.get(self.p..self.p + 2)?.try_into().ok()?,
                        ) as usize;
                        self.p += 2;
                        text = std::str::from_utf8(self.data.get(self.p..self.p + n)?)
                            .ok()?
                            .to_owned();
                        self.p += n;
                    } else if !nbt_wire_value(self.data, &mut self.p, child, 1) {
                        return None;
                    }
                }
            }
            // Any other legal component tag: skip it and report empty text
            // rather than failing the packet on a component we do not model.
            _ => {
                if nbt_wire_value(self.data, &mut self.p, tag, 0) {
                    Some(String::new())
                } else {
                    None
                }
            }
        }
    }
}

/// The exact inverse of [`set_player_team_packet`]: fails on truncation or on a
/// mode that requires blocks the packet does not contain.
#[allow(dead_code)]
fn decode_set_player_team(data: &[u8]) -> Option<SetPlayerTeamPacket> {
    let mut r = P3bReader::new(data);
    let name = r.utf()?;
    let mode = r.byte()?;
    let mut packet = SetPlayerTeamPacket {
        name,
        mode,
        display_name: None,
        friendly_flags: 0,
        nametag_visibility: None,
        collision_rule: None,
        color: 21,
        prefix: None,
        suffix: None,
        entities: Vec::new(),
    };
    if matches!(mode, TEAM_MODE_ADD_TEAM | TEAM_MODE_UPDATE_INFO) {
        packet.display_name = Some(r.nbt_text()?);
        packet.friendly_flags = r.byte()?;
        packet.nametag_visibility = Some(r.utf()?);
        packet.collision_rule = Some(r.utf()?);
        packet.color = r.varint()? as i32;
        packet.prefix = Some(r.nbt_text()?);
        packet.suffix = Some(r.nbt_text()?);
    }
    if matches!(
        mode,
        TEAM_MODE_ADD_TEAM | TEAM_MODE_ADD_ENTITIES | TEAM_MODE_REMOVE_ENTITIES
    ) {
        let count = r.varint()?;
        if !(0..=(1 << 16)).contains(&count) {
            return None;
        }
        for _ in 0..count {
            packet.entities.push(r.utf()?);
        }
    }
    r.done().then_some(packet)
}

/// Send one team packet to every connected client. All five modes broadcast to
/// the whole audience in vanilla (a client's tab list renders every team).
#[allow(dead_code)]
fn p3b_broadcast_team_packet(payload: Vec<u8>) {
    let pkt = payload.clone();
    fanout_in(&client_snapshot(), |_| true, |_| (0x60, pkt.clone()));
}

// ---------------------------------------------------------------------------
// /tick engine (Vanilla 1.21.1 TickRateManager) + S2C 0x71/0x72 codecs.
// ---------------------------------------------------------------------------

/// `TickRateManager`: target rate in ticks per second plus the frozen/step/
/// sprint override state. No wall clock is read anywhere in the controller —
/// `should_run_game_tick` consumes the queued step/sprint counters and the
/// caller paces the loop by `tick_interval_nanos`.
#[allow(dead_code)]
struct TickController {
    /// Ticks per second; vanilla clamps `setTickRate` to >= 1.0 and the command
    /// argument caps it at 10000.0.
    tick_rate: f32,
    frozen: bool,
    /// Ticks queued by `/tick step`; consumed only while frozen.
    step_remaining: i32,
    /// Ticks queued by `/tick sprint`; consumed regardless of frozen.
    sprint_remaining: i32,
    /// Vanilla `finishTickSprint`: stop the sprint on the next tick boundary.
    sprint_stop: bool,
    /// Rolling tick-time statistics for `/tick query`: bounded reservoir so the
    /// report tracks recent history instead of the server's whole lifetime.
    mspt_count: u64,
    mspt_sum_ns: u128,
}

#[allow(dead_code)]
impl TickController {
    const MIN_RATE: f32 = 1.0;
    const MAX_RATE: f32 = 10000.0;
    /// Vanilla keeps a bounded rolling window over recent ticks (the debug
    /// overlay's 100-sample chart); 128 keeps the same "recent" character.
    const MSPT_WINDOW: u64 = 128;

    fn new() -> Self {
        TickController {
            tick_rate: 20.0,
            frozen: false,
            step_remaining: 0,
            sprint_remaining: 0,
            sprint_stop: false,
            mspt_count: 0,
            mspt_sum_ns: 0,
        }
    }
    fn set_tick_rate(&mut self, rate: f32) -> Result<(), String> {
        if !rate.is_finite() || !(Self::MIN_RATE..=Self::MAX_RATE).contains(&rate) {
            return Err(format!(
                "Tick rate must be between {} and {}",
                Self::MIN_RATE as i32,
                Self::MAX_RATE as i32
            ));
        }
        self.tick_rate = rate;
        Ok(())
    }
    /// `runsNormally` && sprint/step overrides, in vanilla's order: an active
    /// sprint wins over everything (frozen included), a normally-running game
    /// ticks, and a frozen game ticks only by consuming queued step ticks.
    fn should_run_game_tick(&mut self) -> bool {
        if self.sprint_remaining > 0 {
            self.sprint_remaining -= 1;
            if self.sprint_stop {
                self.sprint_remaining = 0;
            }
            return true;
        }
        if !self.frozen {
            return true;
        }
        if self.step_remaining > 0 {
            self.step_remaining -= 1;
            return true;
        }
        false
    }
    /// Wall-clock pacing between game ticks. Vanilla runs a sprint flat out
    /// (`nanosecondsPerTick - sprintTimeSavings` collapses to the tick's own
    /// duration), so a sprinting loop has no wait interval at all.
    fn tick_interval_nanos(&self) -> u64 {
        if self.sprint_remaining > 0 {
            return 0;
        }
        (1_000_000_000.0_f64 / f64::from(self.tick_rate)) as u64
    }
    fn is_sprinting(&self) -> bool {
        self.sprint_remaining > 0
    }
    fn is_stepping_forward(&self) -> bool {
        self.step_remaining > 0
    }
    fn request_step(&mut self, ticks: i32) {
        self.step_remaining = ticks.max(0);
    }
    fn request_sprint(&mut self, ticks: i32) {
        self.sprint_remaining = ticks.max(0);
        self.sprint_stop = false;
    }
    fn stop_sprinting(&mut self) {
        self.sprint_stop = true;
    }
    /// Feed one measured tick duration for the `/tick query` average.
    fn record_tick_nanos(&mut self, ns: u64) {
        self.mspt_count += 1;
        self.mspt_sum_ns += u128::from(ns);
        if self.mspt_count > Self::MSPT_WINDOW {
            self.mspt_count /= 2;
            self.mspt_sum_ns /= 2;
        }
    }
    fn avg_mspt(&self) -> f64 {
        if self.mspt_count == 0 {
            return 0.0;
        }
        self.mspt_sum_ns as f64 / self.mspt_count as f64 / 1.0e6
    }
}

#[allow(dead_code)]
fn tick_controller() -> &'static Mutex<TickController> {
    static CTL: OnceLock<Mutex<TickController>> = OnceLock::new();
    CTL.get_or_init(|| Mutex::new(TickController::new()))
}

/// `ClientboundTickingStatePacket` (S2C 0x71): BE f32 tick rate + frozen bool.
#[allow(dead_code)]
fn ticking_state_packet(tick_rate: f32, is_frozen: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(5);
    p.extend_from_slice(&tick_rate.to_be_bytes());
    p.push(u8::from(is_frozen));
    p
}

#[allow(dead_code)]
fn decode_ticking_state(data: &[u8]) -> Option<(f32, bool)> {
    if data.len() != 5 {
        return None;
    }
    Some((
        f32::from_be_bytes(data[..4].try_into().ok()?),
        data[4] != 0,
    ))
}

/// `ClientboundTickingStepPacket` (S2C 0x72): VarInt tick steps.
#[allow(dead_code)]
fn ticking_step_packet(tick_steps: i32) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(tick_steps));
    p
}

#[allow(dead_code)]
fn decode_ticking_step(data: &[u8]) -> Option<i32> {
    let mut b = data;
    let steps = read_protocol_i32(&mut b)?;
    if !b.is_empty() || steps < 0 {
        return None;
    }
    Some(steps)
}

/// Vanilla `TimeArgument` units for `/tick step` and `/tick sprint`: bare
/// numbers and `t` are ticks, `s` is 20 ticks, `d` is 24000 ticks.
#[allow(dead_code)]
fn parse_tick_time_arg(arg: &str) -> Option<i32> {
    let (value, multiplier) = match arg.strip_suffix('t') {
        Some(rest) => (rest, 1.0_f64),
        None => match arg.strip_suffix('s') {
            Some(rest) => (rest, 20.0),
            None => match arg.strip_suffix('d') {
                Some(rest) => (rest, 24_000.0),
                None => (arg, 1.0),
            },
        },
    };
    let ticks = (value.parse::<f64>().ok()? * multiplier).round();
    if ticks < 1.0 || ticks > f64::from(i32::MAX) {
        return None;
    }
    Some(ticks as i32)
}

// ---------------------------------------------------------------------------
// /locate: biome, structure and POI engines over the real worldgen.
// ---------------------------------------------------------------------------

/// A hit from any `/locate` engine: block position plus the horizontal (or 3D,
/// for biomes) distance that feeds the vanilla "… blocks away" suffix.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct LocateHit {
    x: i32,
    y: i32,
    z: i32,
    distance: i32,
}

/// Resource-location syntax: `namespace:path` or bare `path` over
/// `[a-z0-9_./-]` (the set datapack ids use).
#[allow(dead_code)]
fn p3b_resource_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '.' | '/' | '-' | ':'))
        && s.matches(':').count() <= 1
}

#[allow(dead_code)]
fn p3b_ns(s: &str) -> String {
    if s.contains(':') {
        s.to_owned()
    } else {
        format!("minecraft:{s}")
    }
}

/// One datapack `worldgen/structure_set` file reduced to what locate needs.
#[allow(dead_code)]
struct P3bStructureSet {
    id: String,
    placement: P3bPlacement,
    /// (structure id, weight) in set order.
    entries: Vec<(String, i32)>,
}

#[allow(dead_code)]
enum P3bPlacement {
    Spread {
        spread: structure_placement::RandomSpread,
        reducer: structure_placement::FrequencyReduction,
        frequency: f32,
        salt: i32,
        spacing: i32,
        /// `ExclusionZone`: (other set id, chunk radius).
        exclusion: Option<(String, i32)>,
    },
    Rings {
        distance: i32,
        spread: i32,
        count: i32,
        preferred_biomes: HashSet<String>,
    },
}

/// `#tag` or list-form biome holder → the explicit biome-name set, following
/// tags recursively exactly the way the pipeline's `biome_set` does.
#[allow(dead_code)]
fn p3b_biome_set(value: &J, visiting: &mut HashSet<String>) -> HashSet<String> {
    if let Some(values) = value.as_arr() {
        return values
            .iter()
            .flat_map(|v| p3b_biome_set(v, visiting))
            .collect();
    }
    let s = value.str();
    if let Some(tag) = s.strip_prefix('#') {
        let tag = p3b_ns(tag);
        if !visiting.insert(tag.clone()) {
            return HashSet::new();
        }
        let result = worldgen::Gen::dp_resource("tags/worldgen/biome", &tag, "json")
            .and_then(|bytes| jparse(&bytes))
            .and_then(|j| j.get("values").map(|v| p3b_biome_set(v, visiting)))
            .unwrap_or_default();
        visiting.remove(&tag);
        result
    } else if s.is_empty() {
        HashSet::new()
    } else {
        HashSet::from([p3b_ns(s)])
    }
}

/// The biome-name set a structure is allowed to generate in (`worldgen/structure`).
#[allow(dead_code)]
fn p3b_structure_biomes(structure_id: &str) -> HashSet<String> {
    worldgen::Gen::dp_resource("worldgen/structure", structure_id, "json")
        .and_then(|bytes| jparse(&bytes))
        .and_then(|j| {
            j.get("biomes")
                .map(|v| p3b_biome_set(v, &mut HashSet::new()))
        })
        .unwrap_or_default()
}

/// The `type` field of a `worldgen/structure` file (`minecraft:mineshaft`, ...).
#[allow(dead_code)]
fn p3b_structure_type(structure_id: &str) -> Option<String> {
    worldgen::Gen::dp_resource("worldgen/structure", structure_id, "json")
        .and_then(|bytes| jparse(&bytes))
        .map(|j| j.get("type").map(|v| p3b_ns(v.str())).unwrap_or_default())
}

/// Parse one `worldgen/structure_set` JSON into the locate-time shape.
#[allow(dead_code)]
fn p3b_parse_structure_set(id: &str, bytes: &[u8]) -> Result<P3bStructureSet, String> {
    let j = jparse(bytes).ok_or_else(|| format!("malformed structure_set {id}"))?;
    let p = j
        .get("placement")
        .ok_or_else(|| format!("structure_set {id} has no placement"))?;
    let int = |field: &str| -> Result<i32, String> {
        match p.get(field) {
            Some(J::Num(v)) if v.fract() == 0.0 && *v >= i32::MIN as f64 && *v <= i32::MAX as f64 => {
                Ok(*v as i32)
            }
            _ => Err(format!("invalid {field} in structure_set {id}")),
        }
    };
    let placement = match p.get("type").map(|v| p3b_ns(v.str())) {
        Some(ty) if ty == "minecraft:concentric_rings" => P3bPlacement::Rings {
            distance: int("distance")?,
            spread: int("spread")?,
            count: int("count")?,
            preferred_biomes: p
                .get("preferred_biomes")
                .map(|v| p3b_biome_set(v, &mut HashSet::new()))
                .unwrap_or_default(),
        },
        Some(ty) if ty == "minecraft:random_spread" => {
            let spread_kind = match p.get("spread_type").map(|v| v.str()).unwrap_or("linear") {
                "linear" => structure_placement::Spread::Linear,
                "triangular" => structure_placement::Spread::Triangular,
                other => {
                    return Err(format!("unknown spread_type {other} in {id}"));
                }
            };
            let reducer = match p
                .get("frequency_reduction_method")
                .map(|v| v.str())
                .unwrap_or("default")
            {
                "default" => structure_placement::FrequencyReduction::Default,
                "legacy_type_1" => structure_placement::FrequencyReduction::LegacyType1,
                "legacy_type_2" => structure_placement::FrequencyReduction::LegacyType2,
                "legacy_type_3" => structure_placement::FrequencyReduction::LegacyType3,
                other => {
                    return Err(format!("unknown frequency method {other} in {id}"));
                }
            };
            let frequency = match p.get("frequency") {
                None => 1.0,
                Some(J::Num(v)) if (0.0..=1.0).contains(v) => *v as f32,
                _ => return Err(format!("invalid placement frequency in {id}")),
            };
            let spacing = int("spacing")?;
            P3bPlacement::Spread {
                spread: structure_placement::RandomSpread::new(
                    spacing,
                    int("separation")?,
                    int("salt")?,
                    spread_kind,
                )
                .map_err(|e| format!("{id}: {e}"))?,
                reducer,
                frequency,
                salt: int("salt")?,
                spacing,
                exclusion: p.get("exclusion_zone").map(|zone| {
                    (
                        p3b_ns(
                            zone.get("other_set")
                                .map(|v| v.str())
                                .unwrap_or_default(),
                        ),
                        zone.get("chunk_count").map(|v| v.num() as i32).unwrap_or(0),
                    )
                }),
            }
        }
        other => {
            return Err(format!(
                "unsupported placement type {other:?} in {id}"
            ));
        }
    };
    let mut entries = Vec::new();
    if let Some(list) = j.get("structures").and_then(|v| v.as_arr()) {
        for entry in list {
            let structure = entry.get("structure").map(|v| p3b_ns(v.str()));
            let weight = entry.get("weight").map(|v| v.num() as i32).unwrap_or(1);
            if let Some(s) = structure {
                entries.push((s, weight.max(1)));
            }
        }
    }
    Ok(P3bStructureSet {
        id: p3b_ns(id),
        placement,
        entries,
    })
}

/// All `worldgen/structure_set` files, in registry enumeration order.
#[allow(dead_code)]
fn p3b_structure_sets() -> Vec<P3bStructureSet> {
    let mut files: Vec<(String, Vec<u8>)> =
        worldgen::Gen::dp_files("structure_set").into_iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
        .iter()
        .filter_map(|(id, bytes)| p3b_parse_structure_set(id, bytes).ok())
        .collect()
}

/// Does `set`'s placement admit a start chunk at `(cx, cz)`? This is the
/// `isStructureChunk` half of `ChunkGeneratorStructureState`: candidate +
/// frequency + exclusion, with the entry/biome half answered by the caller.
#[allow(dead_code)]
fn p3b_set_admits_placement(
    sets: &BTreeMap<String, P3bStructureSet>,
    set: &P3bStructureSet,
    seed: u64,
    cx: i32,
    cz: i32,
    depth: u32,
) -> bool {
    if depth > 8 {
        return false;
    }
    match &set.placement {
        P3bPlacement::Spread {
            spread,
            reducer,
            frequency,
            salt,
            spacing,
            exclusion,
        } => {
            let (rx, rz) = (cx.div_euclid(*spacing), cz.div_euclid(*spacing));
            if spread.candidate(seed, rx, rz) != (cx, cz) {
                return false;
            }
            if !reducer.allows(seed, *salt, cx, cz, *frequency) {
                return false;
            }
            if let Some((other, count)) = exclusion {
                if let Some(other_set) = sets.get(other) {
                    for ox in cx - count..=cx + count {
                        for oz in cz - count..=cz + count {
                            if p3b_set_admits_placement(
                                sets, other_set, seed, ox, oz, depth + 1,
                            ) {
                                return false;
                            }
                        }
                    }
                }
            }
            true
        }
        P3bPlacement::Rings {
            distance,
            spread,
            count,
            preferred_biomes: _,
        } => p3b_ring_chunks(seed, *distance, *spread, *count, None)
            .contains(&(cx, cz)),
    }
}

/// The admitted chunks of a `concentric_rings` set, biome relocation included
/// when a generator is supplied (the ring's `preferred_biomes` check samples
/// the noise quart source at y-quart 0, same as the pipeline's ring path).
#[allow(dead_code)]
fn p3b_ring_chunks(
    seed: u64,
    distance: i32,
    spread: i32,
    count: i32,
    relocate: Option<(&mut worldgen::Gen, &HashSet<String>)>,
) -> Vec<(i32, i32)> {
    let Some((g, preferred)) = relocate else {
        return structure_placement::concentric_rings(seed, distance, spread, count, |_, _| {
            None
        })
        .unwrap_or_default();
    };
    let preferred_leaves: HashSet<u16> = preferred
        .iter()
        .filter_map(|name| g.biome_leaf_of_name(name))
        .collect();
    let mut last: Option<u16> = None;
    structure_placement::concentric_rings(seed, distance, spread, count, |candidate, rng| {
        structure_placement::concentric_rings_target(
            candidate,
            rng,
            112,
            |qx, qy, qz| {
                let res = g.biome_quart(qx, qy, qz, last);
                if let Some((_, leaf)) = res {
                    last = Some(leaf);
                }
                res
            },
            |b| b.is_some_and(|(_, leaf)| preferred_leaves.contains(&leaf)),
        )
    })
    .unwrap_or_default()
}

/// The position `/locate structure` probes for an entry's biome: the same
/// stub-height convention the pipeline uses (surface heightmap in the
/// Overworld/End, sea level in the Nether where structures hang under a roof).
#[allow(dead_code)]
fn p3b_probe_biome_at(
    g: &mut worldgen::Gen,
    dim: DimId,
    allowed: &HashSet<String>,
    x: i32,
    z: i32,
) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let y = if dim == DimId::Nether {
        g.sea
    } else {
        g.top_ws(x, z) - 1
    };
    let Some((_, leaf)) = g.biome_quart(x >> 2, y >> 2, z >> 2, None) else {
        return false;
    };
    g.biome_name_of_leaf(leaf)
        .is_some_and(|name| allowed.contains(&name))
}

/// Evaluate one admitted chunk: among the set entries whose biome gate passes
/// at the probe point, draw the weighted winner (`WeightedRandomList` over the
/// `forLargeFeature` chunk seed, the same draw shape mineshaft::admit uses) and
/// report it when the request covers it. All-mineshaft sets go through the
/// verified `mineshaft::admit` instead — it answers the real placed start.
#[allow(dead_code)]
fn p3b_pick_structure_at(
    g: &mut worldgen::Gen,
    set: &P3bStructureSet,
    wanted: &HashSet<String>,
    dim: DimId,
    seed: u64,
    cx: i32,
    cz: i32,
    all_mineshaft: bool,
    entry_biomes: &mut HashMap<String, HashSet<String>>,
) -> Option<String> {
    if all_mineshaft {
        let start = mineshaft::admit(g, cx, cz)?;
        let id = mineshaft::start_id(&start).to_owned();
        return wanted.contains(&id).then_some(id);
    }
    let (x, z) = (cx * 16 + 8, cz * 16 + 8);
    let mut rng = generation_math::Lcg::for_large_feature(seed, cx, cz);
    let mut choices: Vec<usize> = Vec::new();
    for (i, (id, _)) in set.entries.iter().enumerate() {
        let biomes = entry_biomes
            .entry(id.clone())
            .or_insert_with(|| p3b_structure_biomes(id));
        if p3b_probe_biome_at(g, dim, biomes, x, z) {
            choices.push(i);
        }
    }
    if choices.is_empty() {
        return None;
    }
    let weight: i32 = choices.iter().map(|&i| set.entries[i].1).sum();
    let mut draw = rng.next_bound(weight);
    let winner = choices
        .iter()
        .position(|&i| {
            draw -= set.entries[i].1;
            draw < 0
        })
        .map(|pos| choices[pos])
        .unwrap_or(*choices.last()?);
    let id = &set.entries[winner].0;
    wanted.contains(id).then(|| id.clone())
}

/// Locate the nearest chunk a set can place any of `wanted` structures at.
/// `wanted` is the request's resolved structure-id set.
#[allow(dead_code)]
fn p3b_set_locate(
    g: &mut worldgen::Gen,
    sets: &BTreeMap<String, P3bStructureSet>,
    set: &P3bStructureSet,
    wanted: &HashSet<String>,
    dim: DimId,
    origin_cx: i32,
    origin_cz: i32,
    max_radius_chunks: i32,
    entry_biomes: &mut HashMap<String, HashSet<String>>,
) -> Option<(i32, i32, String)> {
    let seed = g.wseed();
    let all_mineshaft = set
        .entries
        .iter()
        .all(|(id, _)| p3b_structure_type(id).as_deref() == Some("minecraft:mineshaft"));
    match &set.placement {
        P3bPlacement::Spread { spacing, spread, .. } => {
            let (rx0, rz0) = (
                origin_cx.div_euclid(*spacing),
                origin_cz.div_euclid(*spacing),
            );
            // Region cells in expanding square rings; stop once the whole ring
            // is beyond the radius (a cell's candidate lies inside its spacing
            // square, so scan until the ring's inner edge passes).
            let spacing = *spacing;
            let mut ring = 0i32;
            loop {
                if (ring * spacing - spacing).max(0) > max_radius_chunks {
                    break;
                }
                for dx in -ring..=ring {
                    for dz in -ring..=ring {
                        if ring > 0 && dx.abs() != ring && dz.abs() != ring {
                            continue;
                        }
                        let cand = spread.candidate(seed, rx0 + dx, rz0 + dz);
                        let dist = (cand.0 - origin_cx)
                            .abs()
                            .max((cand.1 - origin_cz).abs());
                        if dist > max_radius_chunks {
                            continue;
                        }
                        if !p3b_set_admits_placement(sets, set, seed, cand.0, cand.1, 0) {
                            continue;
                        }
                        if let Some(id) = p3b_pick_structure_at(
                            g,
                            set,
                            wanted,
                            dim,
                            seed,
                            cand.0,
                            cand.1,
                            all_mineshaft,
                            entry_biomes,
                        ) {
                            return Some((cand.0, cand.1, id));
                        }
                    }
                }
                ring += 1;
            }
            None
        }
        P3bPlacement::Rings {
            distance,
            spread,
            count,
            preferred_biomes,
        } => {
            let chunks = p3b_ring_chunks(
                seed,
                *distance,
                *spread,
                *count,
                Some((g, preferred_biomes)),
            );
            let mut best: Option<(i32, i32, String, i32)> = None;
            for (cx, cz) in chunks {
                let d = (cx - origin_cx).abs().max((cz - origin_cz).abs());
                if d > max_radius_chunks {
                    continue;
                }
                if !p3b_set_admits_placement(sets, set, seed, cx, cz, 0) {
                    continue;
                }
                if let Some(id) = p3b_pick_structure_at(
                    g,
                    set,
                    wanted,
                    dim,
                    seed,
                    cx,
                    cz,
                    all_mineshaft,
                    entry_biomes,
                ) {
                    if best.as_ref().is_none_or(|b| d < b.3) {
                        best = Some((cx, cz, id, d));
                    }
                }
            }
            best.map(|(cx, cz, id, _)| (cx, cz, id))
        }
    }
}

/// `/locate structure`: resolve the argument to structure ids, then ask each
/// set containing any of them for its nearest admitted chunk inside
/// `max_radius_chunks`. The reported position is the start chunk's centre, the
/// same `[x, ~, z]` coordinate vanilla prints.
#[allow(dead_code)]
fn locate_structure_spiral(
    dim: DimId,
    structure_or_tag: &str,
    x: i32,
    z: i32,
    max_radius_chunks: i32,
) -> Result<Option<LocateHit>, String> {
    // Resolve the request to a set of structure ids: `#tag`, a structure id, or
    // (an extension over vanilla) a structure-set name.
    let mut wanted: HashSet<String> = HashSet::new();
    if let Some(tag) = structure_or_tag.strip_prefix('#') {
        let tag_id = p3b_ns(tag);
        let values = worldgen::Gen::dp_resource("tags/worldgen/structure", &tag_id, "json")
            .and_then(|bytes| jparse(&bytes))
            .and_then(|j| j.get("values").map(|v| v.clone()));
        let Some(values) = values else {
            return Err(format!("Unknown structure tag '{tag_id}'"));
        };
        if let Some(list) = values.as_arr() {
            for v in list {
                wanted.insert(p3b_ns(v.str()));
            }
        }
    } else if p3b_resource_name(structure_or_tag) {
        let id = p3b_ns(structure_or_tag);
        if worldgen::Gen::dp_resource("worldgen/structure", &id, "json").is_some() {
            wanted.insert(id);
        } else if worldgen::Gen::dp_resource("worldgen/structure_set", &id, "json").is_some()
        {
            // Set-name extension: `villages` covers every entry of that set.
            if let Some(set) = p3b_structure_sets().into_iter().find(|s| s.id == id) {
                wanted.extend(set.entries.iter().map(|(s, _)| s.clone()));
            }
        }
    }
    if wanted.is_empty() {
        return Err(format!("Unknown structure '{structure_or_tag}'"));
    }
    let sets: BTreeMap<String, P3bStructureSet> = p3b_structure_sets()
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect();
    let relevant: Vec<&P3bStructureSet> = sets
        .values()
        .filter(|set| set.entries.iter().any(|(id, _)| wanted.contains(id)))
        .collect();
    let (origin_cx, origin_cz) = (x >> 4, z >> 4);
    let mut entry_biomes: HashMap<String, HashSet<String>> = HashMap::new();
    let found = worldgen::with_gen_dim(dim, |g| {
        let mut best: Option<(i32, i32, i32)> = None;
        for set in &relevant {
            if let Some((cx, cz, _)) = p3b_set_locate(
                g,
                &sets,
                set,
                &wanted,
                dim,
                origin_cx,
                origin_cz,
                max_radius_chunks,
                &mut entry_biomes,
            ) {
                let dx = f64::from(cx * 16 + 8 - x);
                let dz = f64::from(cz * 16 + 8 - z);
                let dist = (dx * dx + dz * dz).sqrt() as i32;
                if best.is_none_or(|(_, _, b)| dist < b) {
                    best = Some((cx, cz, dist));
                }
            }
        }
        best
    });
    let Some(best) = found else {
        return Err("worldgen is not available in this dimension".to_owned());
    };
    Ok(best.map(|(cx, cz, dist)| LocateHit {
        x: cx * 16 + 8,
        y: 0,
        z: cz * 16 + 8,
        distance: dist,
    }))
}

/// `/locate biome`: expanding square spiral of 8-block horizontal steps, every
/// y quart of the world window per column — `BiomeSource.findClosestBiome3d`'s
/// shape — stopped early once the ring radius exceeds the best 3D distance
/// found (a farther ring cannot beat it) and hard-capped at `max_radius`.
#[allow(dead_code)]
fn locate_biome_spiral(
    dim: DimId,
    biome: &str,
    x: i32,
    y: i32,
    z: i32,
    max_radius: i32,
) -> Option<LocateHit> {
    let biome = p3b_ns(biome);
    worldgen::with_gen_dim(dim, |g| {
        let leaf = g.biome_leaf_of_name(&biome)?;
        let (qy0, qy1) = (
            g.world_min_y >> 2,
            (g.world_min_y + g.world_height - 1) >> 2,
        );
        let mut best: Option<(i32, i32, i32, f64)> = None;
        // Square rings of 8-block horizontal offsets. A hit in a later ring has
        // horizontal distance >= r, so once the best 3D distance is inside the
        // current ring radius no farther ring can beat it.
        let mut r = 0i32;
        while r <= max_radius {
            if best.is_some_and(|(_, _, _, d)| (d as i32) < r) {
                break;
            }
            let mut dx = -r;
            while dx <= r {
                let mut dz = -r;
                while dz <= r {
                    if r == 0 || dx.abs() == r || dz.abs() == r {
                        p3b_locate_biome_column(
                            g, leaf, x, y, z, x + dx, z + dz, qy0, qy1, &mut best,
                        );
                    }
                    dz += 8;
                }
                dx += 8;
            }
            r += 8;
        }
        best.map(|(hx, hy, hz, d)| LocateHit {
            x: hx,
            y: hy,
            z: hz,
            distance: d as i32,
        })
    })
    .flatten()
}

/// Probe one x/z column across all y quarts; on a leaf match, keep it when its
/// 3D distance to the search origin beats the current best.
#[allow(dead_code)]
fn p3b_locate_biome_column(
    g: &mut worldgen::Gen,
    leaf: u16,
    ox: i32,
    oy: i32,
    oz: i32,
    x: i32,
    z: i32,
    qy0: i32,
    qy1: i32,
    best: &mut Option<(i32, i32, i32, f64)>,
) {
    let mut last: Option<u16> = None;
    let mut qy = qy0;
    while qy <= qy1 {
        if let Some((_, l)) = g.biome_quart(x >> 2, qy, z >> 2, last) {
            last = Some(l);
            if l == leaf {
                let hy = qy * 4;
                let d = (((x - ox) as f64).powi(2)
                    + ((z - oz) as f64).powi(2)
                    + ((hy - oy) as f64).powi(2))
                .sqrt();
                if best.is_none_or(|b| d < b.3) {
                    *best = Some((x, hy, z, d));
                }
            }
        }
        qy += 1;
    }
}

/// The vanilla point-of-interest id a block provides, mirroring
/// `PoiTypes`: beds → `home`, bell → `meeting`, job-site blocks → the
/// profession id, plus the block-named types (`nether_portal`, `bee_nest`,
/// `beehive`, `lodestone`, `lightning_rod`).
#[allow(dead_code)]
fn p3b_poi_id_of_block(block_name: &str) -> Option<&'static str> {
    const BLOCK_POIS: [(&str, &str); 21] = [
        ("bell", "minecraft:meeting"),
        ("nether_portal", "minecraft:nether_portal"),
        ("bee_nest", "minecraft:bee_nest"),
        ("beehive", "minecraft:beehive"),
        ("lodestone", "minecraft:lodestone"),
        ("lightning_rod", "minecraft:lightning_rod"),
        ("blast_furnace", "minecraft:armorer"),
        ("smoker", "minecraft:butcher"),
        ("cartography_table", "minecraft:cartographer"),
        ("brewing_stand", "minecraft:cleric"),
        ("composter", "minecraft:farmer"),
        ("barrel", "minecraft:fisherman"),
        ("fletching_table", "minecraft:fletcher"),
        ("lectern", "minecraft:librarian"),
        ("stonecutter", "minecraft:mason"),
        ("loom", "minecraft:shepherd"),
        ("smithing_table", "minecraft:toolsmith"),
        ("grindstone", "minecraft:weaponsmith"),
        ("cauldron", "minecraft:leatherworker"),
        ("water_cauldron", "minecraft:leatherworker"),
        ("lava_cauldron", "minecraft:leatherworker"),
    ];
    let name = block_name.strip_prefix("minecraft:").unwrap_or(block_name);
    if name.strip_suffix("_bed").is_some() {
        return Some("minecraft:home");
    }
    BLOCK_POIS
        .iter()
        .find(|(block, _)| *block == name)
        .map(|(_, poi)| *poi)
}

/// `/locate poi`: the nearest matching POI inside `max_radius` blocks. POI
/// positions live where POI blocks do; the only index this build keeps is the
/// edited-block overlay, so the scan walks the overlay's chunk buckets in the
/// dimension and decodes each stored cell — bounded by the overlay's size and
/// the radius, never by a terrain re-evaluation.
#[allow(dead_code)]
fn locate_poi_nearest(
    dim: DimId,
    poi: &str,
    x: i32,
    y: i32,
    z: i32,
    max_radius: i32,
) -> Option<LocateHit> {
    let poi = p3b_ns(poi);
    let reg = block_registry::breg()?;
    let w = world().lock().unwrap_or_else(|e| e.into_inner());
    let mut best: Option<(i32, i32, i32, i64)> = None;
    for (&(d, cx, cz), cells) in w.iter() {
        if d != dim {
            continue;
        }
        let bx = cx * 16;
        let bz = cz * 16;
        if (bx + 8 - x).abs() > max_radius + 8 || (bz + 8 - z).abs() > max_radius + 8 {
            continue;
        }
        for (&idx, &st) in cells {
            let (bx0, by0, bz0) = (bx + (idx & 15) as i32, (idx >> 8) as i32 + MIN_Y, bz + ((idx >> 4) & 15) as i32);
            if (bx0 - x).abs() > max_radius || (bz0 - z).abs() > max_radius {
                continue;
            }
            let Some(id) = p3b_poi_id_of_block(reg.name_of(st)) else {
                continue;
            };
            if id != poi {
                continue;
            }
            let dist2 = ((bx0 - x).pow(2) + (by0 - y).pow(2) + (bz0 - z).pow(2)) as i64;
            if best.is_none_or(|b| dist2 < b.3) {
                best = Some((bx0, by0, bz0, dist2));
            }
        }
    }
    best.map(|(bx, by, bz, dist2)| LocateHit {
        x: bx,
        y: by,
        z: bz,
        distance: (dist2 as f64).sqrt() as i32,
    })
}

// ---------------------------------------------------------------------------
// /random (Vanilla 1.21.1 RandomCommands) — deterministic named sequences.
// ---------------------------------------------------------------------------

/// Named RNG sequences for `/random`. Vanilla keeps one `RandomSequence` per id
/// and a `minecraft:default` sequence for bare `value`/`roll`; each is seeded
/// from the world seed so the draws replay across restarts.
#[allow(dead_code)]
#[derive(Default)]
struct RandomSequenceStore {
    /// Sequence id -> xorshift64* state (same generator as `world_rng`).
    sequences: BTreeMap<String, u64>,
}

/// One xorshift64* step — bit-identical to `world_rng::next_u64`, so a sequence
/// seeded identically draws the identical values the world stream would.
#[allow(dead_code)]
fn p3b_xorshift64s(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545F4914F6CDD1D)
}

/// `random`'s seed for a named sequence: `seed ^ sequence-name hash`, Java's
/// `RandomSupport.seedUniquifier` style — the same seed and name always reopen
/// the same sequence, while different names never share a stream.
#[allow(dead_code)]
fn p3b_sequence_seed(base_seed: u64, name: &str, mix_name: bool) -> u64 {
    const NAME_HASH_SEED: u64 = 0x811C9DC5;
    let name_hash = if mix_name {
        name.bytes()
            .fold(NAME_HASH_SEED, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001B3))
    } else {
        0
    };
    let seed = base_seed ^ name_hash;
    // A zero state is a fixed point of xorshift; swap in the same default the
    // world stream uses rather than letting a reset stick the sequence.
    if seed == 0 {
        0x9E3779B97F4A7C15
    } else {
        seed
    }
}

#[allow(dead_code)]
impl RandomSequenceStore {
    fn draw(&mut self, sequence: &str, world_seed: u64) -> u64 {
        let state = self
            .sequences
            .entry(sequence.to_owned())
            .or_insert_with(|| p3b_sequence_seed(world_seed, sequence, true));
        p3b_xorshift64s(state)
    }
    /// `random reset <sequence|*>`; returns the human-readable reply.
    fn reset(
        &mut self,
        sequence: &str,
        world_seed: u64,
        seed: Option<u64>,
        include_world_seed: bool,
        include_sequence_id: bool,
    ) -> String {
        if sequence == "*" {
            self.sequences.clear();
            return "Reset all random number sequences".to_owned();
        }
        let base = match seed {
            Some(s) => {
                let mut base = s;
                if include_world_seed {
                    base ^= world_seed;
                }
                base
            }
            None => world_seed,
        };
        self.sequences.insert(
            sequence.to_owned(),
            p3b_sequence_seed(base, sequence, include_sequence_id),
        );
        format!("Reset random number sequence {sequence}")
    }
}

/// Valid `/random` sequence names: resource locations, `*` (reset-all).
#[allow(dead_code)]
fn p3b_sequence_name(s: &str) -> bool {
    s == "*" || p3b_resource_name(s)
}

/// `a..b` / `a b` / `..b` / `a..` integer ranges, like brigadier's
/// `IntegerRangeArgument`.
#[allow(dead_code)]
fn p3b_parse_range(args: &[&str]) -> Result<(i32, i32, usize), String> {
    let Some(first) = args.first() else {
        return Err("Expected a range like 1..10".to_owned());
    };
    if let Some((a, b)) = first.split_once("..") {
        let min = if a.is_empty() {
            i32::MIN
        } else {
            a.parse::<i32>()
                .map_err(|_| "Expected integer as range minimum".to_owned())?
        };
        let max = if b.is_empty() {
            i32::MAX
        } else {
            b.parse::<i32>()
                .map_err(|_| "Expected integer as range maximum".to_owned())?
        };
        if min > max {
            return Err("Range bounds are reversed".to_owned());
        }
        return Ok((min, max, 1));
    }
    if args.len() >= 2 && args[0].parse::<i32>().is_ok() && args[1].parse::<i32>().is_ok() {
        let (min, max) = (args[0].parse().unwrap(), args[1].parse().unwrap());
        if min > max {
            return Err("Range bounds are reversed".to_owned());
        }
        return Ok((min, max, 2));
    }
    if let Ok(v) = first.parse::<i32>() {
        return Ok((v, v, 1));
    }
    Err("Expected a range like 1..10".to_owned())
}

/// The `/random` engine, pure of wire or client state: `value` reports to the
/// source only, `roll` composes the broadcast line the caller fans out.
#[allow(dead_code)]
fn exec_random_command(
    store: &mut RandomSequenceStore,
    world_seed: u64,
    args: &[&str],
    roller_name: &str,
) -> String {
    match args.first().copied() {
        Some("value") | Some("roll") => {
            let roll = args[0] == "roll";
            let (min, max, used) = match p3b_parse_range(&args[1..]) {
                Ok(r) => r,
                Err(e) => return e,
            };
            let sequence = args.get(1 + used).copied().unwrap_or("minecraft:default");
            if !p3b_sequence_name(sequence) || sequence == "*" {
                return format!("Invalid sequence name '{sequence}'");
            }
            let sequence = p3b_ns(sequence);
            let span = (i64::from(max) - i64::from(min) + 1) as u64;
            let v = (store.draw(&sequence, world_seed) % span) as i64 + i64::from(min);
            if roll {
                format!("{roller_name} rolled a {v} between {min} and {max}")
            } else {
                format!("Random number between {min} and {max}: {v}")
            }
        }
        Some("reset") => {
            let sequence = args.get(1).copied().unwrap_or("*");
            if !p3b_sequence_name(sequence) {
                return format!("Invalid sequence name '{sequence}'");
            }
            let seed = match args.get(2) {
                None => None,
                Some(s) => match s.parse::<i64>() {
                    Ok(v) => Some(v as u64),
                    Err(_) => return format!("Invalid seed '{s}'"),
                },
            };
            let include_world = args.get(3).map(|v| *v == "true").unwrap_or(false);
            let include_seq = args.get(4).map(|v| *v == "true").unwrap_or(true);
            let seq_key = if sequence == "*" {
                "*".to_owned()
            } else {
                p3b_ns(sequence)
            };
            store.reset(&seq_key, world_seed, seed, include_world, include_seq)
        }
        _ => "usage: /random value|roll <range> [sequence] | /random reset <sequence|*> [seed] [includeWorldSeed] [includeSequenceId]".to_owned(),
    }
}

#[allow(dead_code)]
fn random_sequences() -> &'static Mutex<RandomSequenceStore> {
    static SEQS: OnceLock<Mutex<RandomSequenceStore>> = OnceLock::new();
    SEQS.get_or_init(|| Mutex::new(RandomSequenceStore::default()))
}

/// The world seed a `/random` sequence is mixed against, or 0 when no
/// generator is loaded yet — deterministic either way, and never wall-clock.
#[allow(dead_code)]
fn p3b_world_seed() -> u64 {
    worldgen::with_gen(|g| g.wseed()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// /return and .mcfunction macro lines.
// ---------------------------------------------------------------------------

/// What a function's `return` produces in Vanilla 1.21.1.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
enum FunctionReturn {
    Value(i32),
    Fail,
    Run(String),
}

/// `/return <value>` | `/return fail` | `/return run <command>`.
#[allow(dead_code)]
fn parse_return_command(args: &[&str]) -> Result<FunctionReturn, String> {
    match args {
        [] => Err("usage: /return <value> | /return fail | /return run <command>".to_owned()),
        ["fail"] => Ok(FunctionReturn::Fail),
        ["run"] => Err("usage: /return run <command>".to_owned()),
        ["run", rest @ ..] => Ok(FunctionReturn::Run(rest.join(" "))),
        [value] => value
            .parse::<i32>()
            .map(FunctionReturn::Value)
            .map_err(|_| format!("Expected integer return value, got '{value}'")),
        _ => Err(
            "usage: /return <value> | /return fail | /return run <command>".to_owned(),
        ),
    }
}

/// A `.mcfunction` line starting with `$` is a macro line: `$$` escapes to a
/// literal `$` and `$(name)` is substituted from the invocation's compound
/// args. Vanilla rejects the whole line when a variable is unbound or its name
/// is not a legal macro variable — names are unquoted strings of
/// `[A-Za-z0-9_.-]` (the charset datapack arguments use).
#[allow(dead_code)]
fn expand_mcfunction_macro(
    line: &str,
    args: &BTreeMap<String, String>,
) -> Result<String, String> {
    if !line.starts_with('$') {
        return Err("macro line must start with '$'".to_owned());
    }
    let valid_name = |name: &str| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    };
    let rest = &line[1..];
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some((_, '$')) => out.push('$'),
            Some((_, '(')) => {
                let start = chars.peek().map(|&(i, _)| i);
                let mut name = String::new();
                let mut closed = false;
                for (_, ch) in chars.by_ref() {
                    if ch == ')' {
                        closed = true;
                        break;
                    }
                    name.push(ch);
                }
                if !closed {
                    return Err("unterminated macro variable".to_owned());
                }
                let _ = start;
                if !valid_name(&name) {
                    return Err(format!("invalid macro variable name '{name}'"));
                }
                match args.get(&name) {
                    Some(value) => out.push_str(value),
                    None => return Err(format!("missing macro argument '{name}'")),
                }
            }
            Some((_, other)) => return Err(format!("unexpected '${other}' in macro line")),
            None => return Err("macro line ends with a bare '$'".to_owned()),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Command bodies + the unified dispatch.
// ---------------------------------------------------------------------------

/// Resolve a `/team` member token: a bare name, or a `@`-selector expanded to
/// entity names (online players give their names; others give their ids).
#[allow(dead_code)]
fn p3b_team_members(token: &str, ctx: &CmdCtx) -> Vec<String> {
    if !token.starts_with('@') {
        return vec![token.to_owned()];
    }
    let eids: HashSet<String> = resolve_selector(token, ctx.me.as_ref())
        .into_iter()
        .collect();
    client_snapshot()
        .iter()
        .filter(|c| eids.contains(&c.eid.to_string()))
        .map(|c| c.name.clone())
        .collect()
}

/// `/team` — every mutation broadcasts the matching 0x60 mode to all clients.
#[allow(dead_code)]
fn team_command(ctx: &CmdCtx, args: &[&str]) -> String {
    let Some(sub) = args.first().copied() else {
        return "usage: /team list [<team>] | add <team> [<displayName>] | remove <team> | empty <team> | join <team> [<members>] | leave <members> | modify <team> <option> <value>".to_owned();
    };
    let registry = teams();
    match sub {
        "add" => {
            let [_, name] = args[..2.min(args.len())] else {
                return "usage: /team add <team> [<displayName>]".to_owned();
            };
            if name.is_empty() || name.chars().count() > 64 {
                return format!("'{name}' is not a valid team name");
            }
            let display = if args.len() > 2 {
                Some(args[2..].join(" "))
            } else {
                None
            };
            let mut reg = registry.lock().unwrap();
            if let Err(e) = reg.add(name, display.as_deref()) {
                return e;
            }
            let team = reg.get(name).expect("just added").clone();
            drop(reg);
            let members: Vec<String> = team.members.iter().cloned().collect();
            p3b_broadcast_team_packet(set_player_team_packet(
                name,
                TEAM_MODE_ADD_TEAM,
                Some(&team),
                &members,
            ));
            format!("Created team {name}")
        }
        "remove" => {
            let [_, name] = args[..2.min(args.len())] else {
                return "usage: /team remove <team>".to_owned();
            };
            match registry.lock().unwrap().remove(name) {
                Ok(_) => {
                    p3b_broadcast_team_packet(set_player_team_packet(
                        name,
                        TEAM_MODE_REMOVE_TEAM,
                        None,
                        &[],
                    ));
                    format!("Removed team {name}")
                }
                Err(e) => e,
            }
        }
        "empty" => {
            let [_, name] = args[..2.min(args.len())] else {
                return "usage: /team empty <team>".to_owned();
            };
            match registry.lock().unwrap().empty(name) {
                Ok(members) => {
                    p3b_broadcast_team_packet(set_player_team_packet(
                        name,
                        TEAM_MODE_REMOVE_ENTITIES,
                        None,
                        &members,
                    ));
                    format!("Removed all {} member(s) from team {name}", members.len())
                }
                Err(e) => e,
            }
        }
        "join" => {
            let [_, name] = args[..2.min(args.len())] else {
                return "usage: /team join <team> [<members>]".to_owned();
            };
            let member_tokens: Vec<String> = if args.len() > 2 {
                args[2..]
                    .iter()
                    .flat_map(|t| p3b_team_members(t, ctx))
                    .collect()
            } else {
                vec![ctx
                    .me
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "@".to_owned())]
            };
            let mut joined = Vec::new();
            let mut errors = Vec::new();
            {
                let mut reg = registry.lock().unwrap();
                for member in &member_tokens {
                    match reg.join(name, member) {
                        Ok(_) => joined.push(member.clone()),
                        Err(e) => errors.push(e),
                    }
                }
            }
            if !joined.is_empty() {
                p3b_broadcast_team_packet(set_player_team_packet(
                    name,
                    TEAM_MODE_ADD_ENTITIES,
                    None,
                    &joined,
                ));
            }
            match (joined.len(), errors.len()) {
                (1, 0) => format!("Added {} to team {name}", joined[0]),
                (n, 0) => format!("Added {n} members to team {name}"),
                (0, _) => errors.join("; "),
                (n, _) => format!("Added {n} members to team {name} ({})", errors.join("; ")),
            }
        }
        "leave" => {
            if args.len() < 2 {
                return "usage: /team leave <members>".to_owned();
            }
            let members: Vec<String> = args[1..]
                .iter()
                .flat_map(|t| p3b_team_members(t, ctx))
                .collect();
            let mut left: Vec<(String, String)> = Vec::new(); // (member, team)
            let mut errors = Vec::new();
            {
                let mut reg = registry.lock().unwrap();
                for member in &members {
                    match reg.leave(member) {
                        Ok(team) => left.push((member.clone(), team)),
                        Err(e) => errors.push(e),
                    }
                }
            }
            // Vanilla mode-4 packets target a single team name, so members who
            // left different teams are grouped into one packet per team.
            if !left.is_empty() {
                let mut by_team: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for (member, team) in &left {
                    by_team
                        .entry(team.clone())
                        .or_default()
                        .push(member.clone());
                }
                for (team, members) in by_team {
                    p3b_broadcast_team_packet(set_player_team_packet(
                        &team,
                        TEAM_MODE_REMOVE_ENTITIES,
                        None,
                        &members,
                    ));
                }
            }
            let left: Vec<String> = left.into_iter().map(|(m, _)| m).collect();
            match (left.len(), errors.len()) {
                (1, 0) => format!("Removed {} from their team", left[0]),
                (n, 0) => format!("Removed {n} members from their teams"),
                (0, _) => errors.join("; "),
                (n, _) => format!("Removed {n} members ({})", errors.join("; ")),
            }
        }
        "list" => {
            let reg = registry.lock().unwrap();
            match args.get(1) {
                None => {
                    let names: Vec<&str> = reg.list().iter().map(|t| t.name.as_str()).collect();
                    if names.is_empty() {
                        "There are no teams".to_owned()
                    } else {
                        format!("There are {} teams: {}", names.len(), names.join(", "))
                    }
                }
                Some(&name) => match reg.get(name) {
                    Some(team) => {
                        let members: Vec<&str> =
                            team.members.iter().map(String::as_str).collect();
                        format!(
                            "Team {} has {} members: {}",
                            team.name,
                            members.len(),
                            members.join(", ")
                        )
                    }
                    None => format!("No team with name '{name}'"),
                },
            }
        }
        "modify" => {
            let (name, option) = match (args.get(1), args.get(2)) {
                (Some(&n), Some(&o)) => (n, o),
                _ => return "usage: /team modify <team> <option> [<value>]".to_owned(),
            };
            let value = args[3..].join(" ");
            let result = {
                let mut reg = registry.lock().unwrap();
                let Some(team) = reg.get_mut(name) else {
                    return format!("No team with name '{name}'");
                };
                match option {
                    "displayName" => {
                        team.display_name = value.clone();
                        Ok(format!("Updated display name for team {name}"))
                    }
                    "color" => match team_color_id(&value) {
                        Some(id) => {
                            team.color = id;
                            Ok(format!("Updated color for team {name} to {value}"))
                        }
                        None => Err(format!("Invalid color '{value}'")),
                    },
                    "friendlyFire" | "seeFriendlyInvisibles" => match value.as_str() {
                        "true" | "false" => {
                            let v = value == "true";
                            if option == "friendlyFire" {
                                team.friendly_fire = v;
                            } else {
                                team.see_friendly_invisibles = v;
                            }
                            Ok(format!("Updated {option} for team {name} to {value}"))
                        }
                        _ => Err(format!("Expected true or false, got '{value}'")),
                    },
                    "nametagVisibility" => {
                        if TEAM_NAMETAG_VISIBILITIES.contains(&value.as_str()) {
                            team.nametag_visibility = value.clone();
                            Ok(format!("Updated nametag visibility for team {name} to {value}"))
                        } else {
                            Err(format!("Invalid nametag visibility '{value}'"))
                        }
                    }
                    "collisionRule" => {
                        if TEAM_COLLISION_RULES.contains(&value.as_str()) {
                            team.collision_rule = value.clone();
                            Ok(format!("Updated collision rule for team {name} to {value}"))
                        } else {
                            Err(format!("Invalid collision rule '{value}'"))
                        }
                    }
                    "prefix" => {
                        team.prefix = value.clone();
                        Ok(format!("Updated prefix for team {name}"))
                    }
                    "suffix" => {
                        team.suffix = value.clone();
                        Ok(format!("Updated suffix for team {name}"))
                    }
                    other => Err(format!("Unknown team option '{other}'")),
                }
            };
            match result {
                Ok(reply) => {
                    let reg = registry.lock().unwrap();
                    let team = reg.get(name).expect("present").clone();
                    drop(reg);
                    p3b_broadcast_team_packet(set_player_team_packet(
                        name,
                        TEAM_MODE_UPDATE_INFO,
                        Some(&team),
                        &[],
                    ));
                    reply
                }
                Err(e) => e,
            }
        }
        _ => format!("unknown /team subcommand '{sub}'"),
    }
}

/// `/teammsg` (`/tm`) — vanilla `TeamMessage`: the sender's team sees the line;
/// the reply string reports delivery like the other chat commands.
#[allow(dead_code)]
fn teammsg_command(ctx: &CmdCtx, args: &[&str]) -> String {
    if args.is_empty() {
        return "usage: /teammsg <message>".to_owned();
    }
    let sender = ctx
        .me
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Server".to_owned());
    let message = args.join(" ");
    let deliveries = match teams()
        .lock()
        .unwrap()
        .teammsg(&sender, &message)
    {
        Ok(d) => d,
        Err(e) => return e,
    };
    let names: HashSet<&str> = deliveries.iter().map(|(n, _)| n.as_str()).collect();
    let text = deliveries.first().map(|(_, t)| t.clone()).unwrap_or_default();
    let mut sent = 0;
    for client in client_snapshot() {
        if names.contains(client.name.as_str()) {
            if let Ok(mut w) = client.sock.lock() {
                let _ = send_packet(&mut w, 0x6c, system_chat(&text));
                sent += 1;
            }
        }
    }
    format!("{text} (delivered to {sent})")
}

/// `/tick` — query | rate | freeze | unfreeze | step | sprint.
#[allow(dead_code)]
fn tick_command(args: &[&str]) -> String {
    let ctl = tick_controller();
    match args.first().copied() {
        Some("query") => {
            let c = ctl.lock().unwrap();
            let mut lines = vec![format!(
                "Target tick rate: {:.1} per second",
                c.tick_rate
            )];
            lines.push(format!(
                "Average time per tick: {:.1}ms (Target: {:.1}ms)",
                c.avg_mspt(),
                1000.0 / f64::from(c.tick_rate)
            ));
            lines.push(
                if c.frozen {
                    "The game is frozen".to_owned()
                } else {
                    "The game is running normally".to_owned()
                },
            );
            if c.is_sprinting() {
                lines.push(format!("The game is in tick sprint ({} ticks remaining)", c.sprint_remaining));
            }
            if c.is_stepping_forward() {
                lines.push(format!("{} step tick(s) queued", c.step_remaining));
            }
            lines.join(" ")
        }
        Some("rate") => {
            let Some(value) = args.get(1).and_then(|v| v.parse::<f32>().ok()) else {
                return "usage: /tick rate <1.0..10000.0>".to_owned();
            };
            {
                let mut c = ctl.lock().unwrap();
                if let Err(e) = c.set_tick_rate(value) {
                    return e;
                }
            }
            let c = ctl.lock().unwrap();
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x71, ticking_state_packet(c.tick_rate, c.frozen))
            });
            format!("Set tick rate to {value}")
        }
        Some("freeze") => {
            ctl.lock().unwrap().frozen = true;
            let c = ctl.lock().unwrap();
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x71, ticking_state_packet(c.tick_rate, c.frozen))
            });
            "Froze the game".to_owned()
        }
        Some("unfreeze") => {
            ctl.lock().unwrap().frozen = false;
            let c = ctl.lock().unwrap();
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x71, ticking_state_packet(c.tick_rate, c.frozen))
            });
            "The game is no longer frozen".to_owned()
        }
        Some("step") => {
            if args.get(1) == Some(&"stop") {
                ctl.lock().unwrap().step_remaining = 0;
                return "Stopped stepping".to_owned();
            }
            let ticks = match args.get(1) {
                None => 1,
                Some(v) => match parse_tick_time_arg(v) {
                    Some(t) => t,
                    None => return "Invalid step count (use ticks like 1, or time like 5s, 1d)".to_owned(),
                },
            };
            let mut c = ctl.lock().unwrap();
            if !c.frozen {
                return "Game is not frozen; /tick step only works while frozen".to_owned();
            }
            c.request_step(ticks);
            drop(c);
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x72, ticking_step_packet(ticks))
            });
            format!("Stepping forward {ticks} tick(s)")
        }
        Some("sprint") => {
            if args.get(1) == Some(&"stop") {
                ctl.lock().unwrap().stop_sprinting();
                return "Stopped sprinting".to_owned();
            }
            let Some(v) = args.get(1) else {
                return "usage: /tick sprint <ticks|time> | /tick sprint stop".to_owned();
            };
            let Some(ticks) = parse_tick_time_arg(v) else {
                return "Invalid sprint length (use ticks like 1, or time like 5s, 1d)".to_owned();
            };
            ctl.lock().unwrap().request_sprint(ticks);
            format!("Sprinting {ticks} tick(s)")
        }
        _ => "usage: /tick query | rate <rate> | freeze | unfreeze | step [ticks|stop] | sprint <ticks|stop>".to_owned(),
    }
}

/// `/locate` — structure | biome | poi.
#[allow(dead_code)]
fn locate_command(ctx: &CmdCtx, args: &[&str]) -> String {
    let (kind, arg) = match (args.first().copied(), args.get(1).copied()) {
        (Some(k), Some(a)) => (k, a),
        _ => return "usage: /locate structure <structure> | biome <biome> | poi <poi>".to_owned(),
    };
    let (x, y, z) = (ctx.x as i32, ctx.y as i32, ctx.z as i32);
    let dim = cmd_dim(ctx);
    let reply = |kind: &str, found: Option<LocateHit>| match found {
        Some(hit) => format!(
            "The nearest {arg} is at [{}, ~, {}] ({} blocks away)",
            hit.x, hit.z, hit.distance
        ),
        None => {
            format!("Could not find a {kind} of type \"{arg}\" within reasonable distance")
        }
    };
    match kind {
        "structure" => match locate_structure_spiral(dim, arg, x, z, 100) {
            Ok(found) => reply("structure", found),
            Err(e) => e,
        },
        "biome" => reply("biome", locate_biome_spiral(dim, arg, x, y, z, 6400)),
        "poi" => reply("poi", locate_poi_nearest(dim, arg, x, y, z, 256)),
        _ => format!("unknown /locate kind '{kind}'"),
    }
}

/// `/random` — value | roll | reset.
#[allow(dead_code)]
fn random_command(ctx: &CmdCtx, args: &[&str]) -> String {
    let roller = ctx
        .me
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Server".to_owned());
    let reply = exec_random_command(
        &mut random_sequences().lock().unwrap(),
        p3b_world_seed(),
        args,
        &roller,
    );
    if args.first() == Some(&"roll") && !reply.starts_with("usage:") {
        let text = reply.clone();
        fanout_in(&client_snapshot(), |_| true, |_| {
            (0x6c, system_chat(&text))
        });
    }
    reply
}

/// `/return` — evaluates to a `FunctionReturn`; a `run` re-enters the command
/// dispatcher so the executed command's reply becomes this command's result.
#[allow(dead_code)]
fn return_command(ctx: &CmdCtx, args: &[&str], s: &mut Option<&Client>) -> String {
    match parse_return_command(args) {
        Ok(FunctionReturn::Value(v)) => format!("Return value: {v}"),
        Ok(FunctionReturn::Fail) => "Command failed".to_owned(),
        Ok(FunctionReturn::Run(command)) => {
            let msg = format!("/{}", command.trim_start_matches('/'));
            run_command_ctx(ctx, &msg, s)
        }
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// /ride (vanilla RideCommand, level 2): `ride <target> mount <vehicle>` and
// `ride <target> dismount`. Reuses the live MountRegistry + 0x5f SetPassengers
// broadcast that right-click mounting uses; unlike `try_mount_vehicle` it also
// mounts non-vehicle entities — riders/vehicles without a natural kind get the
// one-seat Horse bound, and self/cycle rejection comes from the registry.
// ---------------------------------------------------------------------------

fn ride_select(source: &ExecuteSource, sel: &str) -> Result<ExecuteEntity, String> {
    match source.select(sel) {
        Ok(v) if v.len() == 1 => Ok(v.into_iter().next().expect("len checked")),
        Ok(v) if v.is_empty() => Err("No entity was found".into()),
        Ok(_) => Err("Only one entity is allowed".into()),
        Err(e) => Err(e),
    }
}

/// `entity_types` with a natural vehicle kind use their seat table; every
/// other vehicle (including players) rides one deep, matching Horse's bound.
fn ride_vehicle_kind(entity: &ExecuteEntity) -> (VehicleKind, [f64; 3]) {
    let pos = entity.location().0;
    let kind = match entity {
        ExecuteEntity::Mob(mob) => vehicle_kind_of(mob.etype),
        ExecuteEntity::Other { kind, .. } => {
            entity_spec_by_name(kind.strip_prefix("minecraft:").unwrap_or(kind))
                .and_then(|spec| vehicle_kind_of(spec.id))
        }
        ExecuteEntity::Player(_) => None,
    }
    .unwrap_or(VehicleKind::Horse);
    (kind, pos)
}

fn ride_mount(target: &ExecuteEntity, vehicle: &ExecuteEntity) -> String {
    let (t, v) = (target.id(), vehicle.id());
    let (kind, vpos) = ride_vehicle_kind(vehicle);
    let seat = {
        let mut live = mounts().lock().unwrap();
        live.registry.register_vehicle(v, kind, vpos);
        live.registry.register_rider(t, target.location().0);
        match live.registry.mount(v, t) {
            Ok(()) => live.registry.passengers_of(v).len() - 1,
            Err(MountError::SelfMount) => return "Entity cannot ride itself".into(),
            Err(MountError::Cycle) => {
                return "Can't ride an entity that is already riding this entity, this would create a circular rider list".into()
            }
            Err(MountError::Full) => return "Vehicle has no free seat".into(),
            Err(MountError::UnknownVehicle) => return "Unknown vehicle".into(),
        }
    };
    // `Entity.positionRider`: snap the passenger to its attachment offset and
    // credit the carry so anti-cheat does not flag the seat move.
    let off = kind.seat_offset(seat);
    match target {
        ExecuteEntity::Player(player) => {
            let mut p = player.pos.lock().unwrap();
            *p = (
                vpos[0] + off[0],
                vpos[1] + off[1],
                vpos[2] + off[2],
                p.3,
                p.4,
                p.5,
            );
            ac_note_carried_motion(player.eid, off);
        }
        ExecuteEntity::Mob(mob) => {
            let mut p = mob.pos.lock().unwrap();
            *p = (
                vpos[0] + off[0],
                vpos[1] + off[1],
                vpos[2] + off[2],
                p.3,
                p.4,
            );
        }
        ExecuteEntity::Other { .. } => {}
    }
    let packet = mounts().lock().unwrap().registry.encode_set_passengers(v);
    broadcast_seen(v, |_c| (0x5f, packet.clone()));
    format!("Now riding {}", vehicle.kind())
}

fn ride_dismount(target: &ExecuteEntity) -> String {
    let Some(vehicle) = vehicle_of(target.id()) else {
        return "Not riding anything".into();
    };
    if let ExecuteEntity::Player(player) = target {
        // The C2S-dismount path already re-seats + broadcasts to trackers.
        player_dismount(player, true);
        return "No longer riding".into();
    }
    let Some(at) = mounts().lock().unwrap().registry.dismount(target.id()) else {
        return "Not riding anything".into();
    };
    if let ExecuteEntity::Mob(mob) = target {
        let mut p = mob.pos.lock().unwrap();
        *p = (at[0], at[1], at[2], p.3, p.4);
    }
    let packet = mounts()
        .lock()
        .unwrap()
        .registry
        .encode_set_passengers(vehicle);
    broadcast_seen(vehicle, |_c| (0x5f, packet.clone()));
    "No longer riding".into()
}

fn ride_command(ctx: &CmdCtx, args: &[&str]) -> String {
    const USAGE: &str = "usage: /ride <target> mount <vehicle> | /ride <target> dismount";
    let source = ExecuteSource::new(ctx);
    let Some(&sel) = args.first() else {
        return USAGE.into();
    };
    let target = match ride_select(&source, sel) {
        Ok(target) => target,
        Err(e) => return e,
    };
    match &args[1..] {
        ["mount", vehicle] => match ride_select(&source, vehicle) {
            Ok(vehicle) => ride_mount(&target, &vehicle),
            Err(e) => e,
        },
        ["dismount"] => ride_dismount(&target),
        _ => USAGE.into(),
    }
}

// ---------------------------------------------------------------------------
// /spectate (vanilla SpectateCommand, level 2): spectator-only (gamemode 3),
// `spectate <target>` sets the camera entity, bare `spectate` resets it to the
// viewer. The client is moved with the protocol-767 set_camera packet —
// 0x52, verified against `proto/src/data_v767.rs` — payload VarInt eid.
// ---------------------------------------------------------------------------

const SET_CAMERA_PACKET_767: i64 = 0x52;

fn spectate_command(ctx: &CmdCtx, args: &[&str]) -> String {
    const USAGE: &str = "usage: /spectate [<target>] [<player>]";
    let source = ExecuteSource::new(ctx);
    let (target_sel, player_sel) = match args {
        [] => (None, None),
        [t] => (Some(*t), None),
        [t, p] => (Some(*t), Some(*p)),
        _ => return USAGE.into(),
    };
    let viewer = match player_sel {
        Some(sel) => match source.select(sel) {
            Ok(v) if v.len() == 1 => match &v[0] {
                ExecuteEntity::Player(player) => player.clone(),
                _ => return "Only players may be affected by this command".into(),
            },
            Ok(v) if v.is_empty() => return "No player was found".into(),
            Ok(_) => return "Only one player is allowed".into(),
            Err(e) => return e,
        },
        None => match &ctx.me {
            Some(me) => me.clone(),
            None => return "Only players may use this command".into(),
        },
    };
    if !viewer.is_spectator() {
        return "Only players in spectator mode can use this command".into();
    }
    let (camera, label) = match target_sel {
        Some(sel) => match source.select(sel) {
            Ok(v) if v.len() == 1 => (v[0].id(), v[0].kind().to_owned()),
            Ok(v) if v.is_empty() => return "No entity was found".into(),
            Ok(_) => return "Only one entity is allowed".into(),
            Err(e) => return e,
        },
        None => (viewer.eid, String::new()),
    };
    spectator_cameras().lock().unwrap().insert(viewer.eid, camera);
    let mut body = Vec::new();
    write_varint(&mut body, i64::from(camera));
    let _ = send_packet(
        &mut viewer.sock.lock().unwrap(),
        SET_CAMERA_PACKET_767,
        body,
    );
    if target_sel.is_some() {
        format!("Now spectating {label}")
    } else {
        "No longer spectating".into()
    }
}

// ---------------------------------------------------------------------------
// /fillbiome (vanilla FillBiomeCommand, level 2): quart-cell box, the live
// commandModificationBlockLimit bound, and a `replace` predicate that accepts
// a biome id or a `#biome` tag against the effective (override-aware) biome.
// ---------------------------------------------------------------------------

fn fillbiome_command(ctx: &CmdCtx, args: &[&str]) -> String {
    const USAGE: &str = "usage: /fillbiome <from> <to> <biome> [replace <filter>]";
    let (from_t, to_t, biome, filter) = match args {
        [a0, a1, a2, b0, b1, b2, bm] => ((*a0, *a1, *a2), (*b0, *b1, *b2), *bm, None),
        [a0, a1, a2, b0, b1, b2, bm, "replace", f] => {
            ((*a0, *a1, *a2), (*b0, *b1, *b2), *bm, Some(*f))
        }
        _ => return USAGE.into(),
    };
    let parse = |t: &(&str, &str, &str)| {
        (
            coord(t.0, ctx.x).floor() as i32,
            coord(t.1, ctx.y).floor() as i32,
            coord(t.2, ctx.z).floor() as i32,
        )
    };
    let (from, to) = (parse(&from_t), parse(&to_t));
    let dim = cmd_dim(ctx);
    if let Err(e) =
        check_modification_bounds(dim, from).and_then(|_| check_modification_bounds(dim, to))
    {
        return e;
    }
    let biome_id = p3b_ns(biome);
    if !known_biome(&biome_id) {
        return format!("Unknown biome '{biome}'");
    }
    if let Some(f) = filter {
        match f.strip_prefix('#') {
            Some(tag) if !biome_tag_known(&p3b_ns(tag)) => {
                return format!("Unknown biome tag '{f}'");
            }
            None if !known_biome(&p3b_ns(f)) => {
                return format!("Unknown biome filter '{f}'");
            }
            _ => {}
        }
    }
    // Vanilla maps each block pos corner through CubePos.fromBlock (>>2); the
    // two corners can span different quart cells, so normalize the quart box.
    let q = |p: (i32, i32, i32)| (p.0 >> 2, p.1 >> 2, p.2 >> 2);
    let (qa, qb) = (q(from), q(to));
    let (x0, x1) = (qa.0.min(qb.0), qa.0.max(qb.0));
    let (y0, y1) = (qa.1.min(qb.1), qa.1.max(qb.1));
    let (z0, z1) = (qa.2.min(qb.2), qa.2.max(qb.2));
    let volume = (x1 - x0 + 1) as i64 * (y1 - y0 + 1) as i64 * (z1 - z0 + 1) as i64;
    let limit = GR_COMMAND_MODIFICATION_BLOCK_LIMIT
        .load(Ordering::Relaxed)
        .clamp(1, i32::MAX) as i64;
    if volume > limit {
        return format!("Too many blocks ({volume}, maximum {limit})");
    }
    let (qy0, qy1) = (
        dim.min_y() >> 2,
        (dim.min_y() + dim.height() - 1) >> 2,
    );
    let mut cells = Vec::new();
    for qy in y0..=y1 {
        if !(qy0..=qy1).contains(&qy) {
            continue;
        }
        for qz in z0..=z1 {
            for qx in x0..=x1 {
                if let Some(f) = filter {
                    if !fillbiome_filter_matches(f, dim, qx, qy, qz) {
                        continue;
                    }
                }
                cells.push((qx, qy, qz));
            }
        }
    }
    let changed = cells.len();
    let mut overrides = biome_overrides().lock().unwrap();
    let store = overrides.entry(dim).or_default();
    for cell in cells {
        store.insert(cell, biome_id.clone());
    }
    format!("Changed biome of {changed} cells to {biome_id}")
}

fn fillbiome_filter_matches(filter: &str, dim: DimId, qx: i32, qy: i32, qz: i32) -> bool {
    let Some(current) = effective_biome_name(dim, qx, qy, qz) else {
        return false;
    };
    match filter.strip_prefix('#') {
        Some(tag) => {
            let tag = p3b_ns(tag);
            datapack_tag_contains("biome", &tag, &current)
                || datapack_tag_contains("worldgen/biome", &tag, &current)
        }
        None => current == p3b_ns(filter),
    }
}

// ---------------------------------------------------------------------------
// /place (vanilla PlaceCommand, level 2): `place feature <id> [pos]` validates
// the configured-feature id and records the accepted dispatch (no live `FC`
// seam exists — see datapack.rs); `place template <id> [pos]` stamps the
// structure's palette-0 blocks into the world.
// ---------------------------------------------------------------------------

fn place_command(ctx: &CmdCtx, args: &[&str]) -> String {
    const USAGE: &str = "usage: /place feature <id> [<pos>] | /place template <id> [<pos>]";
    let Some((&kind, rest)) = args.split_first() else {
        return USAGE.into();
    };
    let Some((&id, pos_args)) = rest.split_first() else {
        return USAGE.into();
    };
    if !matches!(kind, "feature" | "template") {
        return USAGE.into();
    }
    if !p3b_resource_name(id) {
        return format!("Invalid id '{id}'");
    }
    let pos = match pos_args {
        [] => (
            ctx.x.floor() as i32,
            ctx.y.floor() as i32,
            ctx.z.floor() as i32,
        ),
        [x, y, z] => (
            coord(x, ctx.x).floor() as i32,
            coord(y, ctx.y).floor() as i32,
            coord(z, ctx.z).floor() as i32,
        ),
        _ => return USAGE.into(),
    };
    let dim = cmd_dim(ctx);
    if let Err(e) = check_modification_bounds(dim, pos) {
        return e;
    }
    let id = p3b_ns(id);
    match kind {
        "feature" => {
            let known = worldgen::Gen::dp_resource("worldgen/configured_feature", &id, "json")
                .is_some()
                || worldgen::Gen::dp_resource("worldgen/feature", &id, "json").is_some();
            if !known {
                return format!("No feature with the name '{id}' exists");
            }
            place_dispatches().lock().unwrap().push(PlaceDispatch {
                dim,
                pos,
                id: id.clone(),
                kind: PlaceKind::Feature,
            });
            format!("Placed feature '{id}' at [{}, {}, {}]", pos.0, pos.1, pos.2)
        }
        _ => match place_template_live(dim, pos, &id) {
            Ok(placed) => {
                place_dispatches().lock().unwrap().push(PlaceDispatch {
                    dim,
                    pos,
                    id: id.clone(),
                    kind: PlaceKind::Template,
                });
                format!("Placed template '{id}' ({placed} blocks)")
            }
            Err(e) => e,
        },
    }
}

/// The one entry point the command match in `commands_core.rs` calls for every
/// P3-B command name. Kept narrow on purpose: a drop-in arm per command.
#[allow(dead_code)]
fn exec_p3b_command(ctx: &CmdCtx, cmd: &str, args: &[&str], s: &mut Option<&Client>) -> String {
    match cmd {
        "team" => team_command(ctx, args),
        "teammsg" | "tm" => teammsg_command(ctx, args),
        "tick" => tick_command(args),
        "locate" => locate_command(ctx, args),
        "random" => random_command(ctx, args),
        "return" => return_command(ctx, args, s),
        "ride" => ride_command(ctx, args),
        "spectate" => spectate_command(ctx, args),
        "fillbiome" => fillbiome_command(ctx, args),
        "place" => place_command(ctx, args),
        _ => format!("unknown P3-B command '{cmd}'"),
    }
}

// ---------------------------------------------------------------------------
// Lane P6d: `/profile` on the live PERF-1 hotspot profiler, protocol-767
// `/transfer` (#149), expanded `/data` (#170) and `/advancement query` (#174).
// ---------------------------------------------------------------------------

/// The shared `ServerHotspotProfiler` from `operator_ux.rs` (PERF-1). The tick
/// pipeline records tick durations and per-chunk subsystem work into this one
/// store; `/profile` renders the same live instance, so the report a level-4
/// operator reads always reflects real server state.
fn live_hotspot_profiler() -> &'static Mutex<ServerHotspotProfiler> {
    static PROFILER: OnceLock<Mutex<ServerHotspotProfiler>> = OnceLock::new();
    PROFILER.get_or_init(|| Mutex::new(ServerHotspotProfiler::default()))
}

/// `/profile` is an owner-level (4) diagnostic. `required_permission_level`
/// lives outside this lane's files, so the gate is re-checked inside the
/// handler: a level-2/3 operator — and therefore a level-2 `.mcfunction` frame
/// too — is refused with the same reply the outer permission gate produces.
fn run_profile_command(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return "You do not have permission to run that command".into();
    }
    let profiler = live_hotspot_profiler();
    match args {
        [] => profiler.lock().unwrap().summary(5).format_human(),
        ["reset"] => {
            profiler.lock().unwrap().reset();
            "Profiler counters reset".into()
        }
        [raw] => match raw.parse::<usize>() {
            Ok(n) => profiler
                .lock()
                .unwrap()
                .summary(n.clamp(1, 20))
                .format_human(),
            Err(_) => "usage: /profile [<top_n>|reset]".into(),
        },
        _ => "usage: /profile [<top_n>|reset]".into(),
    }
}

/// Protocol 767 clientbound `transfer` — `proto/src/data_v767.rs` lists it as
/// `(Play, Clientbound, "transfer", 0x73)`.
const TRANSFER_PACKET_ID: i64 = 0x73;

/// A `String` field on the wire must be non-empty and bounded; control
/// characters, whitespace and NUL have no business in a hostname either, so
/// the host is restricted to printable non-space ASCII within 255 bytes.
fn transfer_hostname_valid(host: &str) -> bool {
    (1..=255).contains(&host.len()) && host.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// `[String host][VarInt port]` — the full clientbound `transfer` body.
fn transfer_packet(host: &str, port: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(host.len() + 6);
    write_str(&mut payload, host);
    write_varint(&mut payload, i64::from(port));
    payload
}

/// Vanilla `/transfer` is a level-3 literal (`requires(source.hasPermission(3))`).
/// Like `/profile` the level is re-checked here: a level-2 operator or
/// `.mcfunction` frame must never reach the packet write.
fn run_transfer_command(ctx: &CmdCtx, level: u8, args: &[&str]) -> String {
    if level < 3 {
        return "You do not have permission to run that command".into();
    }
    let usage = "usage: /transfer <hostname> [<port>] [<players>]";
    let [host, rest @ ..] = args else {
        return usage.into();
    };
    if rest.len() > 2 {
        return usage.into();
    }
    if !transfer_hostname_valid(host) {
        return format!("Invalid hostname '{host}'");
    }
    let port = match rest.first() {
        None => 25565,
        Some(raw) => match raw.parse::<u16>() {
            Ok(0) | Err(_) => return format!("Invalid port '{raw}'"),
            Ok(port) => port,
        },
    };
    let targets = player_command_targets(rest.get(1).copied().unwrap_or("@s"), ctx.me.as_ref());
    if targets.is_empty() {
        return "No player found".into();
    }
    let payload = transfer_packet(host, port);
    let mut sent = 0usize;
    for target in &targets {
        if send_packet(
            &mut target.sock.lock().unwrap(),
            TRANSFER_PACKET_ID,
            payload.clone(),
        )
        .is_ok()
        {
            sent += 1;
        }
    }
    format!("Transferred {sent} player(s) to {host}:{port}")
}

// ------------------------------------------------------------- /data (#170)

/// One `/data` verb target: a live block entity, a single entity, or a named
/// storage id. Block positions resolve `~` against the source like Vanilla.
enum DataTarget {
    Block(DimId, BlockPos),
    Entity(String),
    Storage(String),
}

/// `/data merge|modify` writes land here when the live container store cannot
/// express the resulting NBT — the overlay is seeded from the live block-entity
/// data on the first write and stays authoritative for `/data` reads. `Items`
/// is still synced back to `chests()`/`furnaces()` after every write.
fn data_block_overrides() -> &'static LockedMap<DimBlockPos, ExecTag> {
    static STORE: Registry<DimBlockPos, ExecTag> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

/// Extra per-entity NBT fields the live entity model does not carry
/// (`CustomName`, `NoAI`, `ArmorItems`, `HandItems`, arbitrary merged keys).
/// Keyed by the same `tag_key` the `/tag` store uses (`p:name`, `e:eid`, ...).
fn data_entity_extras() -> &'static LockedMap<String, ExecTag> {
    static STORE: Registry<String, ExecTag> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

fn data_parse_target<'a>(
    source: &ExecuteSource,
    args: &'a [&'a str],
) -> Result<(DataTarget, &'a [&'a str]), String> {
    match args {
        ["block", x, y, z, rest @ ..] => {
            let [sx, sy, sz] = source.position();
            let pos = execute_block_pos([coord(x, sx), coord(y, sy), coord(z, sz)]);
            Ok((DataTarget::Block(source.dimension, pos), rest))
        }
        ["entity", selector, rest @ ..] => {
            Ok((DataTarget::Entity((*selector).to_owned()), rest))
        }
        ["storage", id, rest @ ..] => {
            Ok((DataTarget::Storage(block_registry::ns_name(id)), rest))
        }
        _ => Err(
            "usage: /data <verb> block <x> <y> <z>|entity <target>|storage <id> ...".into(),
        ),
    }
}

/// One entity's NBT root: the `execute` read model plus the fields `/data`
/// needs (`Motion`, `Air`, `Fire`, `Tags`, `SelectedItem`, `ArmorItems`,
/// `HandItems`) and any extras earlier writes recorded in `data_entity_extras`.
fn data_entity_root(entity: &ExecuteEntity) -> ExecTag {
    let mut root = execute_entity_data(entity);
    let ExecTag::Compound(fields) = &mut root else {
        return root;
    };
    let mut put = |name: &str, value: ExecTag| {
        fields.insert(NbtString::text(name), value);
    };
    let extras = data_entity_extras().lock().unwrap().clone();
    let extras = extras.get(&entity.tag_key());
    let extra = |name: &str| -> Option<&ExecTag> {
        extras.and_then(|tag| tag.get(name))
    };
    let doubles = |values: [f64; 3]| -> ExecTag {
        ExecTag::List(
            6,
            values
                .iter()
                .map(|v| ExecTag::Double(v.to_bits()))
                .collect(),
        )
    };
    let motion = match entity {
        ExecuteEntity::Mob(mob) => *mob.vel.lock().unwrap(),
        _ => [0.0; 3],
    };
    put("Motion", doubles(motion));
    match entity {
        ExecuteEntity::Player(player) => {
            let air = *player.air.lock().unwrap();
            put("Air", ExecTag::Short(air.clamp(-300, 30_000) as i16));
            put(
                "Fire",
                extra("Fire").cloned().unwrap_or(ExecTag::Short(-20)),
            );
            put(
                "Invulnerable",
                extra("Invulnerable")
                    .cloned()
                    .unwrap_or(ExecTag::Byte(0)),
            );
            let inv = player.inv.lock().unwrap();
            let held = *player.held.lock().unwrap();
            if let Some(selected) = inv
                .get(&(36 + held))
                .and_then(|stack| data_item_nbt(stack))
            {
                put("SelectedItem", selected);
            }
            put("ArmorItems", data_armor_items(&inv));
            put("HandItems", data_hand_items(&inv, held));
        }
        ExecuteEntity::Mob(mob) => {
            put(
                "Air",
                ExecTag::Short((*mob.air.lock().unwrap()).clamp(-300, 30_000) as i16),
            );
            put(
                "Fire",
                ExecTag::Short((*mob.fire_ticks.lock().unwrap()).clamp(-20, 30_000) as i16),
            );
            put(
                "Invulnerable",
                extra("Invulnerable").cloned().unwrap_or(ExecTag::Byte(0)),
            );
            put("NoAI", extra("NoAI").cloned().unwrap_or(ExecTag::Byte(0)));
            put(
                "ArmorItems",
                extra("ArmorItems")
                    .cloned()
                    .unwrap_or_else(|| ExecTag::List(10, vec![data_empty_item(); 4])),
            );
            put(
                "HandItems",
                extra("HandItems")
                    .cloned()
                    .unwrap_or_else(|| ExecTag::List(10, vec![data_empty_item(); 2])),
            );
        }
        ExecuteEntity::Other { .. } => {
            put(
                "Invulnerable",
                extra("Invulnerable").cloned().unwrap_or(ExecTag::Byte(0)),
            );
        }
    }
    let mut entity_tags: Vec<ExecTag> = tags()
        .lock()
        .unwrap()
        .get(&entity.tag_key())
        .into_iter()
        .flatten()
        .map(|tag| ExecTag::String(NbtString::text(tag)))
        .collect();
    entity_tags.sort_by(|a, b| execute_tag_snbt(a).cmp(&execute_tag_snbt(b)));
    put("Tags", ExecTag::List(8, entity_tags));
    if let Some(ExecTag::Compound(extra_fields)) = extras {
        for (key, value) in extra_fields {
            fields.insert(key.clone(), value.clone());
        }
    }
    root
}

fn data_empty_item() -> ExecTag {
    ExecTag::Compound(Default::default())
}

/// A live stack rendered the way vanilla writes it inside `SelectedItem` /
/// `ArmorItems`/`HandItems`: the compound without the `Slot` index.
fn data_item_nbt(stack: &Stack) -> Option<ExecTag> {
    let mut tag = stack_to_nbt(0, stack)?;
    if let ExecTag::Compound(fields) = &mut tag {
        fields.remove(&NbtString::text("Slot"));
    }
    Some(tag)
}

/// Vanilla `ArmorItems` order is feet, legs, chest, head — window-0 slots
/// 8, 7, 6, 5 on this server.
fn data_armor_items(inv: &HashMap<i16, Stack>) -> ExecTag {
    ExecTag::List(
        10,
        [8, 7, 6, 5]
            .iter()
            .map(|slot| {
                inv.get(slot)
                    .and_then(data_item_nbt)
                    .unwrap_or_else(data_empty_item)
            })
            .collect(),
    )
}

/// Vanilla `HandItems` order is main hand then off hand — the held hotbar
/// stack and window slot 45.
fn data_hand_items(inv: &HashMap<i16, Stack>, held: i16) -> ExecTag {
    ExecTag::List(
        10,
        [36 + held, 45]
            .iter()
            .map(|slot| {
                inv.get(slot)
                    .and_then(data_item_nbt)
                    .unwrap_or_else(data_empty_item)
            })
            .collect(),
    )
}

/// Parse one vanilla item-stack compound (`{id:"minecraft:x",count:n}`) into a
/// live `Stack`; an empty compound is vanilla's empty-slot marker.
fn data_item_stack(tag: &ExecTag) -> Option<Stack> {
    let ExecTag::Compound(fields) = tag else {
        return None;
    };
    if fields.is_empty() {
        return None;
    }
    let mut with_slot = fields.clone();
    with_slot.insert(NbtString::text("Slot"), ExecTag::Byte(0));
    items_nbt_to_stacks(&ExecTag::List(10, vec![ExecTag::Compound(with_slot)]))
        .ok()?
        .remove(&0)
}

fn data_actor_root(actor: &Actor) -> ExecTag {
    let (x, y, z) = *actor.pos.lock().unwrap();
    execute_nbt_compound([
        (
            "Pos",
            ExecTag::List(
                6,
                [x, y, z]
                    .iter()
                    .map(|v| ExecTag::Double(v.to_bits()))
                    .collect(),
            ),
        ),
        (
            "Inventory",
            stacks_to_items_nbt(&actor.inv.lock().unwrap()),
        ),
    ])
}

/// The compound a `/data` verb reads for `target`, plus the subject name the
/// reply interpolates. Vanilla's "is not a block entity" and "no entity"
/// failures come back as `Err`.
fn data_read_root(source: &ExecuteSource, target: &DataTarget) -> Result<(String, ExecTag), String> {
    match target {
        DataTarget::Block(dim, pos @ (x, y, z)) => {
            let key = (*dim, *x, *y, *z);
            let mut root = data_block_overrides()
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .or_else(|| execute_block_data(*dim, *pos));
            let Some(root) = root.as_mut() else {
                return Err(format!("The block at {x}, {y}, {z} is not a block entity"));
            };
            if let ExecTag::Compound(fields) = root {
                for (name, value) in [("x", x), ("y", y), ("z", z)] {
                    fields.insert(NbtString::text(name), ExecTag::Int(*value));
                }
            }
            Ok((format!("Storage at {x}, {y}, {z}"), root.clone()))
        }
        DataTarget::Entity(selector) => {
            let selected = source.select(selector)?;
            match selected.as_slice() {
                [entity] => Ok((entity.score_holder(), data_entity_root(entity))),
                [] => actors()
                    .lock()
                    .unwrap()
                    .get(selector.as_str())
                    .map(|actor| (actor.name.clone(), data_actor_root(actor)))
                    .ok_or_else(|| "No entity found".to_owned()),
                _ => Err("Expected a single entity".into()),
            }
        }
        DataTarget::Storage(id) => execute_storage()
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .map(|root| (format!("Storage '{id}'"), root))
            .ok_or_else(|| format!("Unknown storage '{id}'")),
    }
}

/// `data_entity_root`'s write half: land one top-level field where the live
/// model keeps it, file it under `data_entity_extras` otherwise.
fn data_apply_entity_field(entity: &ExecuteEntity, name: &str, value: &ExecTag) {
    let extra = |entity: &ExecuteEntity, name: &str, value: &ExecTag| {
        let mut extras = data_entity_extras().lock().unwrap();
        let entry = extras
            .entry(entity.tag_key())
            .or_insert_with(|| ExecTag::Compound(Default::default()));
        if let ExecTag::Compound(fields) = entry {
            fields.insert(NbtString::text(name), value.clone());
        }
    };
    match name {
        "id" | "x" | "y" | "z" | "UUID" => {}
        "Pos" => {
            if let Some([x, y, z]) = data_nbt_vec3(value) {
                match entity {
                    ExecuteEntity::Player(player) => {
                        let mut pos = player.pos.lock().unwrap();
                        (pos.0, pos.1, pos.2) = (x, y, z);
                    }
                    ExecuteEntity::Mob(mob) => {
                        let mut pos = mob.pos.lock().unwrap();
                        (pos.0, pos.1, pos.2) = (x, y, z);
                    }
                    ExecuteEntity::Other { id, kind, .. } => {
                        data_move_other_entity(*id, kind, [x, y, z]);
                    }
                }
            }
        }
        "Rotation" => {
            if let ExecTag::List(_, v) = value {
                let angles: Vec<f32> = v
                    .iter()
                    .filter_map(|tag| data_nbt_f64(tag).map(|n| n as f32))
                    .collect();
                if let [yaw, pitch, ..] = angles.as_slice() {
                    match entity {
                        ExecuteEntity::Player(player) => {
                            let mut pos = player.pos.lock().unwrap();
                            (pos.3, pos.4) = (*yaw, *pitch);
                        }
                        ExecuteEntity::Mob(mob) => {
                            let mut pos = mob.pos.lock().unwrap();
                            (pos.3, pos.4) = (*yaw, *pitch);
                        }
                        ExecuteEntity::Other { .. } => {}
                    }
                }
            }
        }
        "Motion" => {
            if let Some(motion) = data_nbt_vec3(value) {
                match entity {
                    ExecuteEntity::Mob(mob) => *mob.vel.lock().unwrap() = motion,
                    _ => extra(entity, name, value),
                }
            }
        }
        "Health" => {
            if let Some(hp) = data_nbt_f64(value) {
                match entity {
                    ExecuteEntity::Player(player) => {
                        *player.hp.lock().unwrap() = hp as f32;
                    }
                    ExecuteEntity::Mob(mob) => *mob.hp.lock().unwrap() = hp as f32,
                    ExecuteEntity::Other { .. } => {}
                }
            }
        }
        "Air" => {
            if let Some(ticks) = data_nbt_f64(value).map(|n| n as i32) {
                match entity {
                    ExecuteEntity::Player(player) => *player.air.lock().unwrap() = ticks,
                    ExecuteEntity::Mob(mob) => *mob.air.lock().unwrap() = ticks,
                    ExecuteEntity::Other { .. } => {}
                }
            }
        }
        "Fire" => {
            if let Some(ticks) = data_nbt_f64(value).map(|n| n as i32) {
                match entity {
                    ExecuteEntity::Mob(mob) => *mob.fire_ticks.lock().unwrap() = ticks,
                    _ => extra(entity, name, value),
                }
            }
        }
        "Invulnerable" => {
            if let ExecuteEntity::Player(player) = entity {
                let flag = data_nbt_f64(value).map_or(false, |n| n != 0.0);
                *player.invulnerable.lock().unwrap() = if flag { i32::MAX } else { 0 };
            }
            extra(entity, name, value);
        }
        "Tags" => {
            if let ExecTag::List(_, entries) = value {
                let mut set = HashSet::new();
                for entry in entries {
                    if let ExecTag::String(tag) = entry {
                        if let Ok(tag) = tag.to_utf8() {
                            set.insert(tag);
                        }
                    }
                }
                let mut store = tags().lock().unwrap();
                let key = entity.tag_key();
                if set.is_empty() {
                    store.remove(&key);
                } else {
                    store.insert(key, set);
                }
            }
        }
        "Inventory" => {
            if let ExecTag::List(..) = value {
                match entity {
                    ExecuteEntity::Player(player) => {
                        if let Ok(slots) = items_nbt_to_stacks(value) {
                            *player.inv.lock().unwrap() = slots;
                        }
                    }
                    _ => extra(entity, name, value),
                }
            }
        }
        "SelectedItem" => {
            if let ExecTag::Compound(..) = value {
                match entity {
                    ExecuteEntity::Player(player) => {
                        let slot = 36 + *player.held.lock().unwrap();
                        let mut inv = player.inv.lock().unwrap();
                        match data_item_stack(value) {
                            Some(stack) => {
                                inv.insert(slot, stack);
                            }
                            None => {
                                inv.remove(&slot);
                            }
                        }
                    }
                    _ => extra(entity, name, value),
                }
            }
        }
        "ArmorItems" => {
            if let ExecTag::List(_, entries) = value {
                match entity {
                    ExecuteEntity::Player(player) => {
                        let mut inv = player.inv.lock().unwrap();
                        for (slot, entry) in [8, 7, 6, 5].into_iter().zip(entries.iter()) {
                            match data_item_stack(entry) {
                                Some(stack) => {
                                    inv.insert(slot, stack);
                                }
                                None => {
                                    inv.remove(&slot);
                                }
                            }
                        }
                    }
                    _ => extra(entity, name, value),
                }
            }
        }
        "HandItems" => {
            if let ExecTag::List(_, entries) = value {
                match entity {
                    ExecuteEntity::Player(player) => {
                        let held = *player.held.lock().unwrap();
                        let mut inv = player.inv.lock().unwrap();
                        for (slot, entry) in [36 + held, 45].into_iter().zip(entries.iter()) {
                            match data_item_stack(entry) {
                                Some(stack) => {
                                    inv.insert(slot, stack);
                                }
                                None => {
                                    inv.remove(&slot);
                                }
                            }
                        }
                    }
                    _ => extra(entity, name, value),
                }
            }
        }
        "CustomName" | "NoAI" => extra(entity, name, value),
        _ => extra(entity, name, value),
    }
}

/// `data_entity_root`'s remove half: fields the live model re-derives on write
/// (`Pos`, `Rotation`, `Motion`, ids) behave like Vanilla — the value a later
/// save produces is unaffected; the rest reset to their load defaults.
fn data_remove_entity_field(entity: &ExecuteEntity, name: &str) {
    let drop_extra = |entity: &ExecuteEntity, name: &str| {
        if let Some(tag) = data_entity_extras().lock().unwrap().get_mut(&entity.tag_key()) {
            if let ExecTag::Compound(fields) = tag {
                fields.remove(&NbtString::text(name));
            }
        }
    };
    match name {
        "id" | "x" | "y" | "z" | "UUID" | "Pos" | "Rotation" | "Motion" | "Item" => {}
        "Health" => match entity {
            ExecuteEntity::Player(player) => *player.hp.lock().unwrap() = 20.0,
            ExecuteEntity::Mob(mob) => {
                *mob.hp.lock().unwrap() = entity_max_health_1211(mob.etype);
            }
            ExecuteEntity::Other { .. } => {}
        },
        "Air" => match entity {
            ExecuteEntity::Player(player) => *player.air.lock().unwrap() = 300,
            ExecuteEntity::Mob(mob) => *mob.air.lock().unwrap() = 300,
            ExecuteEntity::Other { .. } => {}
        },
        "Fire" => match entity {
            ExecuteEntity::Mob(mob) => *mob.fire_ticks.lock().unwrap() = -20,
            _ => drop_extra(entity, name),
        },
        "Invulnerable" => {
            if let ExecuteEntity::Player(player) = entity {
                *player.invulnerable.lock().unwrap() = 0;
            }
            drop_extra(entity, name);
        }
        "Tags" => {
            tags().lock().unwrap().remove(&entity.tag_key());
        }
        "Inventory" => {
            if let ExecuteEntity::Player(player) = entity {
                player.inv.lock().unwrap().clear();
            }
            drop_extra(entity, name);
        }
        "SelectedItem" => {
            if let ExecuteEntity::Player(player) = entity {
                let slot = 36 + *player.held.lock().unwrap();
                player.inv.lock().unwrap().remove(&slot);
            }
            drop_extra(entity, name);
        }
        "ArmorItems" => {
            if let ExecuteEntity::Player(player) = entity {
                let mut inv = player.inv.lock().unwrap();
                for slot in [8, 7, 6, 5] {
                    inv.remove(&slot);
                }
            }
            drop_extra(entity, name);
        }
        "HandItems" => {
            if let ExecuteEntity::Player(player) = entity {
                let held = *player.held.lock().unwrap();
                let mut inv = player.inv.lock().unwrap();
                for slot in [36 + held, 45] {
                    inv.remove(&slot);
                }
            }
            drop_extra(entity, name);
        }
        _ => drop_extra(entity, name),
    }
}

fn data_move_other_entity(id: i32, kind: &str, [x, y, z]: [f64; 3]) {
    match kind {
        "minecraft:item" => {
            if let Some(drop) = drops().lock().unwrap().iter_mut().find(|d| d.eid == id) {
                (drop.x, drop.y, drop.z) = (x, y, z);
            }
        }
        _ => {}
    }
}

fn data_nbt_f64(tag: &ExecTag) -> Option<f64> {
    match tag {
        ExecTag::Byte(v) => Some(f64::from(*v)),
        ExecTag::Short(v) => Some(f64::from(*v)),
        ExecTag::Int(v) => Some(f64::from(*v)),
        ExecTag::Long(v) => Some(*v as f64),
        ExecTag::Float(v) => Some(f64::from(f32::from_bits(*v))),
        ExecTag::Double(v) => Some(f64::from_bits(*v)),
        _ => None,
    }
}

fn data_nbt_vec3(tag: &ExecTag) -> Option<[f64; 3]> {
    let ExecTag::List(_, values) = tag else {
        return None;
    };
    let coords: Vec<f64> = values.iter().filter_map(data_nbt_f64).collect();
    match coords.as_slice() {
        [x, y, z] => Some([*x, *y, *z]),
        _ => None,
    }
}

/// `execute_nbt_merge`'s recursive compound merge: compound fields merge
/// depth-first, everything else replaces outright.
fn data_nbt_merge(dst: &mut ExecTag, src: &ExecTag) {
    match (dst, src) {
        (ExecTag::Compound(dst_fields), ExecTag::Compound(src_fields)) => {
            for (key, value) in src_fields {
                match dst_fields.get_mut(key) {
                    Some(existing) => data_nbt_merge(existing, value),
                    None => {
                        dst_fields.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (dst, src) => *dst = src.clone(),
    }
}

/// The `remove` half of `ExecuteNbtPath`'s grammar: `Key` drops a compound
/// member, `Index` drops a list slot, `All`/`Match` fan the removal out.
fn data_nbt_remove(root: &mut ExecTag, nodes: &[ExecuteNbtNode]) -> bool {
    use ExecuteNbtNode::*;
    let Some((node, rest)) = nodes.split_first() else {
        return false;
    };
    match (node, root) {
        (Key(key), ExecTag::Compound(fields)) => {
            if rest.is_empty() {
                fields.remove(key).is_some()
            } else {
                fields
                    .get_mut(key)
                    .is_some_and(|value| data_nbt_remove(value, rest))
            }
        }
        (Index(index), ExecTag::List(_, entries)) => {
            execute_nbt_index(*index, entries.len()).is_some_and(|index| {
                if rest.is_empty() {
                    entries.remove(index);
                    true
                } else {
                    data_nbt_remove(&mut entries[index], rest)
                }
            })
        }
        (All | Match(_, true), ExecTag::List(_, entries)) => {
            let is_match = |entry: &ExecTag| match node {
                All => true,
                Match(pattern, _) => execute_nbt_matches(pattern, entry),
                _ => unreachable!(),
            };
            if rest.is_empty() {
                let before = entries.len();
                entries.retain(|entry| !is_match(entry));
                before != entries.len()
            } else {
                entries
                    .iter_mut()
                    .filter(|entry| is_match(entry))
                    .fold(false, |removed, entry| {
                        data_nbt_remove(entry, rest) || removed
                    })
            }
        }
        (Match(pattern, false), value)
            if !rest.is_empty() && execute_nbt_matches(pattern, value) =>
        {
            data_nbt_remove(value, rest)
        }
        _ => false,
    }
}

/// Mutable fan-out for `data modify`'s merge/append/prepend/insert: resolves
/// every existing tag the path names, the same set `ExecuteNbtPath::get` reads.
fn data_nbt_resolve_mut<'a>(
    root: &'a mut ExecTag,
    nodes: &[ExecuteNbtNode],
) -> Vec<&'a mut ExecTag> {
    use ExecuteNbtNode::*;
    let mut values = vec![root];
    for node in nodes {
        let mut next = Vec::new();
        for value in values {
            match (node, value) {
                (Key(key), ExecTag::Compound(fields)) => {
                    if let Some(value) = fields.get_mut(key) {
                        next.push(value);
                    }
                }
                (Index(index), ExecTag::List(_, entries)) => {
                    if let Some(index) = execute_nbt_index(*index, entries.len()) {
                        next.push(&mut entries[index]);
                    }
                }
                (All, ExecTag::List(_, entries)) => next.extend(entries.iter_mut()),
                (Match(pattern, true), ExecTag::List(_, entries)) => next.extend(
                    entries
                        .iter_mut()
                        .filter(|entry| execute_nbt_matches(pattern, entry)),
                ),
                (Match(pattern, false), value)
                    if execute_nbt_matches(pattern, value) =>
                {
                    next.push(value);
                }
                _ => {}
            }
        }
        if next.is_empty() {
            return next;
        }
        values = next;
    }
    values
}

enum DataModifyOp {
    Set,
    Merge,
    Append,
    Prepend,
    Insert(i32),
}

fn data_apply_modify(
    root: &mut ExecTag,
    path: &ExecuteNbtPath,
    op: &DataModifyOp,
    value: &ExecTag,
) -> bool {
    match op {
        DataModifyOp::Set => path.set(root, value),
        DataModifyOp::Merge => data_nbt_resolve_mut(root, &path.0)
            .into_iter()
            .fold(false, |changed, target| {
                if let (ExecTag::Compound(_), ExecTag::Compound(_)) = (&*target, value) {
                    data_nbt_merge(target, value);
                    true
                } else {
                    changed
                }
            }),
        DataModifyOp::Append | DataModifyOp::Prepend | DataModifyOp::Insert(_) => {
            data_nbt_resolve_mut(root, &path.0)
                .into_iter()
                .fold(false, |changed, target| {
                    let ExecTag::List(kind, entries) = target else {
                        return changed;
                    };
                    if entries.is_empty() {
                        *kind = value.id();
                    }
                    match op {
                        DataModifyOp::Append => entries.push(value.clone()),
                        DataModifyOp::Prepend => entries.insert(0, value.clone()),
                        DataModifyOp::Insert(index) => {
                            let len = entries.len() as i32;
                            let index = if *index < 0 { len + index } else { *index };
                            if !(0..=len).contains(&index) {
                                return changed;
                            }
                            entries.insert(index as usize, value.clone());
                        }
                        _ => unreachable!(),
                    }
                    true
                })
        }
    }
}

/// `data get`'s reply body for one target: path-less reads print the whole
/// root, a path resolves to its values, and `scale` multiplies a lone number.
fn data_get(
    source: &ExecuteSource,
    target: &DataTarget,
    path: Option<&str>,
    scale: Option<&str>,
) -> Result<String, String> {
    let (label, root) = data_read_root(source, target)?;
    let values: Vec<ExecTag> = match path {
        Some(path) => {
            let parsed = ExecuteNbtPath::parse(path)?;
            let values = parsed.get(&root);
            if values.is_empty() {
                return Err(format!("Found no elements at '{path}'"));
            }
            values.into_iter().map(|v| v.into_owned()).collect()
        }
        None => vec![root],
    };
    if values.len() != 1 {
        return Err(format!(
            "Found {} elements at '{}'",
            values.len(),
            path.unwrap_or("")
        ));
    }
    let rendered = match scale {
        Some(raw) => {
            let scale: f64 = raw
                .parse()
                .map_err(|_| format!("Invalid scale '{raw}'"))?;
            let value = data_nbt_f64(&values[0])
                .ok_or_else(|| "Cannot scale non-numeric data".to_owned())?;
            format!("{}", value * scale)
        }
        None => execute_tag_snbt(&values[0]),
    };
    let head = match target {
        DataTarget::Block(..) => "Storage has the following contents".to_owned(),
        DataTarget::Entity(_) => format!("{label} has the following entity data"),
        DataTarget::Storage(_) => format!("{label} has the following contents"),
    };
    Ok(format!("{head}: {rendered}"))
}

/// Reconcile a written block-entity root with the live container store:
/// `Items` lands in `chests()` (or the furnace slots) so the opened container
/// shows exactly what `/data` reports; its removal empties the container.
fn data_sync_block_items(dim: DimId, (x, y, z): BlockPos, root: &ExecTag) {
    let key = (dim, x, y, z);
    let furnace = dim == DimId::Overworld && is_furnace_at(x, y, z);
    match root.get("Items") {
        Some(items) => {
            if let Ok(mut slots) = items_nbt_to_stacks(items) {
                if furnace {
                    if let Some(f) = furnaces().lock().unwrap().get_mut(&key) {
                        f.input = slots.remove(&0).unwrap_or_default();
                        f.fuel = slots.remove(&1).unwrap_or_default();
                        f.output = slots.remove(&2).unwrap_or_default();
                    }
                } else {
                    chests()
                        .lock()
                        .unwrap()
                        .insert(ContainerKey::Block(key), slots);
                }
            }
        }
        None => {
            chests().lock().unwrap().remove(&ContainerKey::Block(key));
            // A removed `Items` empties a live furnace only when it held
            // anything; `furnace_slots` reports the non-empty slots.
            if furnace && !furnace_slots((x, y, z)).is_empty() {
                if let Some(f) = furnaces().lock().unwrap().get_mut(&key) {
                    f.input = (0, 0, Vec::new());
                    f.fuel = (0, 0, Vec::new());
                    f.output = (0, 0, Vec::new());
                }
            }
        }
    }
}

/// Land a rebuilt root back on the target. Entities apply each field through
/// `data_apply_entity_field` and clear fields the rebuild dropped; blocks go
/// to the overlay plus the `Items` sync; storage swaps the stored tag.
fn data_write_root(
    source: &ExecuteSource,
    target: &DataTarget,
    previous: &ExecTag,
    root: &ExecTag,
) -> Result<(), String> {
    match target {
        DataTarget::Block(dim, pos @ (x, y, z)) => {
            data_block_overrides()
                .lock()
                .unwrap()
                .insert((*dim, *x, *y, *z), root.clone());
            data_sync_block_items(*dim, *pos, root);
            Ok(())
        }
        DataTarget::Entity(selector) => {
            let selected = source.select(selector)?;
            let [entity] = selected.as_slice() else {
                return Err(if selected.is_empty() {
                    "No entity found".into()
                } else {
                    "Expected a single entity".into()
                });
            };
            if let ExecTag::Compound(fields) = root {
                for (key, value) in fields {
                    if let Ok(name) = key.to_utf8() {
                        data_apply_entity_field(entity, &name, value);
                    }
                }
            }
            if let ExecTag::Compound(before) = previous {
                for key in before.keys() {
                    let still = matches!(root, ExecTag::Compound(fields) if fields.contains_key(key));
                    if !still {
                        if let Ok(name) = key.to_utf8() {
                            data_remove_entity_field(entity, &name);
                        }
                    }
                }
            }
            Ok(())
        }
        DataTarget::Storage(id) => {
            execute_storage()
                .lock()
                .unwrap()
                .insert(id.clone(), root.clone());
            Ok(())
        }
    }
}

/// `data modify`'s source clause: `from <target> [<path>]`, `string <target>
/// [<path>] [<start>] [<end>]` or a literal `value <snbt>`.
fn data_modify_source(
    source: &ExecuteSource,
    args: &[&str],
) -> Result<Vec<ExecTag>, String> {
    let usage = || {
        "usage: /data modify <target> <path> <set|merge|append|prepend|insert n> \
         from|string <block|entity|storage> ... [<path>] | value <nbt>"
            .to_owned()
    };
    match args {
        ["value", snbt] => {
            let mut reader = brigadier::StringReader::new(*snbt);
            match execute_read_snbt(&mut reader, 0) {
                Some(tag) if !reader.can_read() => Ok(vec![tag]),
                _ => Err(format!("Invalid NBT '{snbt}'")),
            }
        }
        [mode @ ("from" | "string"), rest @ ..] => {
            let (target, rest) = data_parse_target(source, rest).map_err(|_| usage())?;
            let (_, root) = data_read_root(source, &target)?;
            let (values, rest) = match rest {
                [] => (vec![root], &rest[..]),
                [path, rest @ ..] => {
                    let parsed = ExecuteNbtPath::parse(path)?;
                    let values: Vec<ExecTag> = parsed
                        .get(&root)
                        .into_iter()
                        .map(|v| v.into_owned())
                        .collect();
                    if values.is_empty() {
                        return Err(format!("Found no elements at '{path}'"));
                    }
                    (values, rest)
                }
            };
            if *mode == "from" {
                if !rest.is_empty() {
                    return Err(usage());
                }
                return Ok(values);
            }
            let to_index = |raw: Option<&&str>, default: i64| -> Result<i64, String> {
                match raw {
                    None => Ok(default),
                    Some(raw) => raw
                        .parse::<i64>()
                        .map_err(|_| format!("Invalid index '{raw}'")),
                }
            };
            let start = to_index(rest.first(), 0)?;
            let end = to_index(rest.get(1), -1)?;
            if rest.len() > 2 {
                return Err(usage());
            }
            Ok(values
                .iter()
                .map(|value| {
                    let text = match value {
                        ExecTag::String(text) => text.to_utf8().unwrap_or_default(),
                        other => execute_tag_snbt(other),
                    };
                    let chars: Vec<char> = text.chars().collect();
                    let len = chars.len() as i64;
                    let start = if start < 0 { len + start } else { start }.clamp(0, len) as usize;
                    let end = if end < 0 { len + end + 1 } else { end }.clamp(start as i64, len) as usize;
                    ExecTag::String(NbtString::text(&chars[start..end].iter().collect::<String>()))
                })
                .collect())
        }
        _ => Err(usage()),
    }
}

fn run_data_command(ctx: &CmdCtx, args: &[&str]) -> String {
    let source = ExecuteSource::new(ctx);
    let reply = match args {
        ["get", rest @ ..] => {
            let parsed = data_parse_target(&source, rest)
                .and_then(|(target, rest)| match rest {
                    [] => data_get(&source, &target, None, None),
                    [path] => data_get(&source, &target, Some(path), None),
                    [path, scale] => data_get(&source, &target, Some(path), Some(scale)),
                    _ => Err(
                        "usage: /data get block <x> <y> <z>|entity <t>|storage <id> [<path>] [<scale>]"
                            .into(),
                    ),
                });
            parsed
        }
        ["merge", rest @ ..] => data_parse_target(&source, rest).and_then(|(target, rest)| {
            let [snbt] = rest else {
                return Err("usage: /data merge <target> <nbt>".into());
            };
            let mut reader = brigadier::StringReader::new(*snbt);
            let patch = match execute_read_snbt(&mut reader, 0) {
                Some(tag @ ExecTag::Compound(_)) if !reader.can_read() => tag,
                _ => return Err(format!("Expected a compound, got '{snbt}'")),
            };
            let (_, previous) = match data_read_root(&source, &target) {
                Ok(read) => read,
                Err(error) => match &target {
                    DataTarget::Storage(_) => (String::new(), execute_nbt_compound([])),
                    _ => return Err(error),
                },
            };
            let mut root = previous.clone();
            data_nbt_merge(&mut root, &patch);
            data_write_root(&source, &target, &previous, &root)?;
            Ok("Modified data".to_owned())
        }),
        ["modify", rest @ ..] => data_parse_target(&source, rest).and_then(|(target, rest)| {
            let [raw_path, rest @ ..] = rest else {
                return Err("usage: /data modify <target> <path> <op> <source>".into());
            };
            let path = ExecuteNbtPath::parse(raw_path)?;
            let (op, rest) = match rest {
                ["set", rest @ ..] => (DataModifyOp::Set, &rest[..]),
                ["merge", rest @ ..] => (DataModifyOp::Merge, &rest[..]),
                ["append", rest @ ..] => (DataModifyOp::Append, &rest[..]),
                ["prepend", rest @ ..] => (DataModifyOp::Prepend, &rest[..]),
                ["insert", index, rest @ ..] => match index.parse::<i32>() {
                    Ok(index) => (DataModifyOp::Insert(index), &rest[..]),
                    Err(_) => return Err(format!("Invalid insert index '{index}'")),
                },
                _ => return Err("usage: /data modify <target> <path> <op> <source>".into()),
            };
            let values = data_modify_source(&source, rest)?;
            let (_, previous) = data_read_root(&source, &target)?;
            let mut root = previous.clone();
            for value in &values {
                if !data_apply_modify(&mut root, &path, &op, value) {
                    return Err(format!("Could not modify '{raw_path}'"));
                }
            }
            data_write_root(&source, &target, &previous, &root)?;
            Ok("Modified data".to_owned())
        }),
        ["remove", rest @ ..] => data_parse_target(&source, rest).and_then(|(target, rest)| {
            let [path] = rest else {
                return Err("usage: /data remove <target> <path>".into());
            };
            let path = ExecuteNbtPath::parse(path)?;
            let (_, previous) = data_read_root(&source, &target)?;
            let mut root = previous.clone();
            data_nbt_remove(&mut root, &path.0);
            data_write_root(&source, &target, &previous, &root)?;
            Ok("Removed data".to_owned())
        }),
        _ => Err(
            "usage: /data get|merge|modify|remove block <x> <y> <z>|entity <t>|storage <id> ..."
                .into(),
        ),
    };
    reply.unwrap_or_else(|error| error)
}

// -------------------------------------------------- /advancement query (#174)

/// The scope grammar `query` shares with grant/revoke: `everything`, `only`,
/// `from`, `through`, `until`. The result is the flat id list to count.
fn advancement_query_scope(tracker: &AdvancementTracker, scope: &[&str]) -> Result<Vec<String>, String> {
    let resolve = |id: &str| -> Result<String, String> {
        if let Ok(definition) = resolve_command_advancement(tracker, id) {
            return Ok(definition.id.to_owned());
        }
        let id = if id.contains(':') {
            id.to_owned()
        } else {
            format!("minecraft:{id}")
        };
        if advancements().lock().unwrap().contains_key(&id) {
            Ok(id)
        } else {
            Err(format!("Unknown advancement: {id}"))
        }
    };
    match scope {
        ["everything"] => {
            let mut ids: Vec<String> = tracker
                .definitions
                .iter()
                .map(|definition| definition.id.to_owned())
                .collect();
            for id in advancements().lock().unwrap().keys() {
                if !ids.iter().any(|known| known == id) {
                    ids.push(id.clone());
                }
            }
            Ok(ids)
        }
        ["only", id] => resolve(id).map(|id| vec![id]),
        ["from", id] | ["through", id] | ["until", id] => {
            let id = resolve(id)?;
            let mut ids: Vec<String> = tracker
                .definitions
                .iter()
                .filter(|definition| {
                    let downward = tracker.has_ancestor(definition.id, &id);
                    let upward = tracker.has_ancestor(&id, definition.id);
                    match scope[0] {
                        "from" => downward,
                        "until" => upward,
                        _ => downward || upward,
                    }
                })
                .map(|definition| definition.id.to_owned())
                .collect();
            if ids.is_empty() {
                // A datapack-only id has no parent linkage; the scope is itself.
                ids.push(id);
            }
            Ok(ids)
        }
        _ => Err(
            "Usage: /advancement query <target> everything|only <adv>|from <adv>|through <adv>|until <adv>"
                .into(),
        ),
    }
}

/// Live-tracker completion: criteria-complete for registered definitions, the
/// persisted earned set for datapack-only ids.
fn advancement_query_done(tracker: &AdvancementTracker, player: &str, id: &str) -> bool {
    match tracker.definition(id) {
        Some(definition) => tracker.criteria_complete(player, definition),
        None => adv_done()
            .lock()
            .unwrap()
            .get(player)
            .is_some_and(|earned| earned.contains(id)),
    }
}

fn run_advancement_query(
    tracker: &AdvancementTracker,
    targets: &[Arc<Client>],
    scope: &[&str],
) -> String {
    let ids = match advancement_query_scope(tracker, scope) {
        Ok(ids) => ids,
        Err(error) => return error,
    };
    targets
        .iter()
        .map(|player| {
            if let ["only", id] = scope {
                let id = ids.first().map(String::as_str).unwrap_or(id);
                if advancement_query_done(tracker, &player.name, id) {
                    return format!("{} has completed advancement {}", player.name, id);
                }
                if let Some(definition) = tracker.definition(id) {
                    let required: HashSet<&str> = definition
                        .requirements
                        .iter()
                        .copied()
                        .flatten()
                        .copied()
                        .collect();
                    let earned = tracker
                        .progress
                        .get(&player.name)
                        .and_then(|rows| rows.get(definition.id))
                        .map_or(0, |criteria| {
                            criteria.keys().filter(|k| required.contains(**k)).count()
                        });
                    return format!(
                        "{} has completed {} of {} criteria for {}",
                        player.name,
                        earned,
                        required.len(),
                        id
                    );
                }
                return format!("{} has not completed advancement {}", player.name, id);
            }
            let granted = ids
                .iter()
                .filter(|id| advancement_query_done(tracker, &player.name, id))
                .count();
            format!(
                "{} has completed {} of {} advancements",
                player.name,
                granted,
                ids.len()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

// ------------------------------------------------------------------ tests ---

#[cfg(test)]
mod teams_and_commands_tests {
    use super::*;

    fn fresh_teams() -> TeamRegistry {
        TeamRegistry::default()
    }

    #[test]
    fn team_registry_crud_membership_teammsg_and_0x60_wire_roundtrip() {
        let mut reg = fresh_teams();
        // add / list / modify
        assert!(reg.add("red", Some("Red Team")).is_ok());
        assert!(reg.add("red", None).is_err(), "duplicate name rejected");
        assert!(reg.add("blue", None).is_ok());
        assert_eq!(reg.get("red").unwrap().display_name, "Red Team");
        assert_eq!(reg.get("red").unwrap().color, 21, "default color is reset");
        assert_eq!(
            reg.list().iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["blue", "red"],
            "list is sorted by name"
        );

        // join moves members between teams automatically
        assert_eq!(reg.join("red", "Alice").unwrap(), None);
        assert_eq!(reg.join("red", "Bob").unwrap(), None);
        assert_eq!(reg.team_of("Alice"), Some("red"));
        assert_eq!(reg.join("blue", "Alice").unwrap().as_deref(), Some("red"));
        assert_eq!(reg.team_of("Alice"), Some("blue"));
        assert!(!reg.get("red").unwrap().members.contains("Alice"));
        assert!(reg.join("noteam", "Carol").is_err());

        // leave / empty
        assert_eq!(reg.leave("Bob").unwrap(), "red");
        assert!(reg.leave("Bob").is_err());
        let emptied = reg.empty("blue").unwrap();
        assert_eq!(emptied, vec!["Alice".to_owned()]);
        assert_eq!(reg.team_of("Alice"), None);

        // teammsg
        reg.join("red", "Alice").unwrap();
        reg.join("red", "Bob").unwrap();
        let msgs = reg.teammsg("Alice", "hello team").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs[0],
            ("Alice".to_owned(), "[red] <Alice> hello team".to_owned())
        );
        assert!(reg.teammsg("Carol", "hi").is_err(), "sender not on a team");

        // modify fields -> packet params
        let team = {
            let t = reg.get_mut("red").unwrap();
            t.color = team_color_id("gold").unwrap();
            t.friendly_fire = false;
            t.see_friendly_invisibles = true;
            t.nametag_visibility = "hideForOtherTeams".to_owned();
            t.collision_rule = "pushOwnTeam".to_owned();
            t.prefix = "[R] ".to_owned();
            t.suffix = "!".to_owned();
            t.clone()
        };
        assert_eq!(team_color_id("gold"), Some(6));
        assert_eq!(team_color_id("bold"), None, "format codes are not colors");
        assert_eq!(team_color_id("reset"), Some(21));

        // mode 0 roundtrip: params + full member list
        let members: Vec<String> = team.members.iter().cloned().collect();
        let pkt = set_player_team_packet("red", TEAM_MODE_ADD_TEAM, Some(&team), &members);
        let back = decode_set_player_team(&pkt).expect("mode 0 decode");
        assert_eq!(back.name, "red");
        assert_eq!(back.mode, 0);
        assert_eq!(back.display_name.as_deref(), Some("Red Team"));
        assert_eq!(back.friendly_flags & 0x01, 0, "friendlyFire=false");
        assert_eq!(back.friendly_flags & 0x02, 0x02, "seeFriendlyInvisibles=true");
        assert_eq!(back.nametag_visibility.as_deref(), Some("hideForOtherTeams"));
        assert_eq!(back.collision_rule.as_deref(), Some("pushOwnTeam"));
        assert_eq!(back.color, 6);
        assert_eq!(back.prefix.as_deref(), Some("[R] "));
        assert_eq!(back.suffix.as_deref(), Some("!"));
        assert_eq!(back.entities, vec!["Alice", "Bob"]);

        // mode 1: name only
        let pkt = set_player_team_packet("red", TEAM_MODE_REMOVE_TEAM, None, &[]);
        let back = decode_set_player_team(&pkt).unwrap();
        assert_eq!(back.mode, 1);
        assert!(back.display_name.is_none() && back.entities.is_empty());

        // mode 2: params only
        let pkt = set_player_team_packet("red", TEAM_MODE_UPDATE_INFO, Some(&team), &[]);
        let back = decode_set_player_team(&pkt).unwrap();
        assert_eq!(back.mode, 2);
        assert_eq!(back.display_name.as_deref(), Some("Red Team"));
        assert!(back.entities.is_empty());

        // modes 3/4: entity deltas only
        for (mode, tag) in [(3u8, "add"), (4u8, "remove")] {
            let pkt = set_player_team_packet(
                "red",
                mode,
                None,
                &["Carol".to_owned(), "Dave".to_owned()],
            );
            let back = decode_set_player_team(&pkt).unwrap_or_else(|| panic!("mode {tag}"));
            assert_eq!(back.mode, mode);
            assert_eq!(back.entities, vec!["Carol", "Dave"]);
            assert!(back.display_name.is_none());
        }
        // trailing garbage and truncation both fail closed
        let mut pkt = set_player_team_packet("red", TEAM_MODE_REMOVE_TEAM, None, &[]);
        pkt.push(0xaa);
        assert!(decode_set_player_team(&pkt[..pkt.len() - 1]).is_some());
        assert!(decode_set_player_team(&pkt[..2]).is_none());
    }

    #[test]
    fn tick_controller_freeze_step_sprint_rate_and_0x71_0x72_wire_roundtrip() {
        let mut c = TickController::new();
        assert_eq!(c.tick_rate, 20.0);
        assert_eq!(c.tick_interval_nanos(), 50_000_000);
        assert!(c.should_run_game_tick(), "unfrozen runs normally");

        // rate bounds
        assert!(c.set_tick_rate(10.0).is_ok());
        assert_eq!(c.tick_interval_nanos(), 100_000_000);
        assert!(c.set_tick_rate(0.5).is_err());
        assert!(c.set_tick_rate(20000.0).is_err());
        assert!(c.set_tick_rate(f32::NAN).is_err());
        c.set_tick_rate(20.0).unwrap();

        // freeze stops ticks; step queues consumed one per call
        c.frozen = true;
        assert!(!c.should_run_game_tick());
        c.request_step(2);
        assert!(c.should_run_game_tick());
        assert!(c.should_run_game_tick());
        assert!(!c.should_run_game_tick(), "step budget exhausted");
        assert!(!c.is_stepping_forward());

        // sprint bypasses freeze and consumes its own budget
        c.request_sprint(3);
        assert_eq!(c.tick_interval_nanos(), 0, "sprint runs flat out");
        assert!(c.should_run_game_tick());
        assert!(c.is_sprinting());
        c.stop_sprinting();
        assert!(c.should_run_game_tick(), "stop applies on next tick");
        assert!(!c.should_run_game_tick(), "sprint ended early via stop");
        assert!(!c.is_sprinting());

        // unfreeze restores normal running
        c.frozen = false;
        assert!(c.should_run_game_tick());

        // query statistics
        for _ in 0..10 {
            c.record_tick_nanos(50_000_000);
        }
        assert_eq!(c.avg_mspt(), 50.0);

        // 0x71 roundtrip
        let pkt = ticking_state_packet(20.0, true);
        assert_eq!(pkt, vec![0x41, 0xa0, 0x00, 0x00, 0x01]);
        assert_eq!(decode_ticking_state(&pkt), Some((20.0, true)));
        assert_eq!(decode_ticking_state(&pkt[..3]), None);
        // 0x72 roundtrip incl. multi-byte varints
        for steps in [1, 20, 300] {
            let pkt = ticking_step_packet(steps);
            assert_eq!(decode_ticking_step(&pkt), Some(steps));
        }
        assert_eq!(decode_ticking_step(&[]), None);

        // time units
        assert_eq!(parse_tick_time_arg("1"), Some(1));
        assert_eq!(parse_tick_time_arg("5t"), Some(5));
        assert_eq!(parse_tick_time_arg("2s"), Some(40));
        assert_eq!(parse_tick_time_arg("1d"), Some(24_000));
        assert_eq!(parse_tick_time_arg("0.5s"), Some(10));
        assert_eq!(parse_tick_time_arg("0"), None);
        assert_eq!(parse_tick_time_arg("-3s"), None);
        assert_eq!(parse_tick_time_arg("abc"), None);
    }

    #[test]
    fn locate_biome_and_poi_find_real_positions_and_respect_bounds() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        // Overlay restore on any path: a panic must not leak the seeded blocks.
        let saved_world = std::mem::take(&mut *world().lock().unwrap());
        struct RestoreWorld(HashMap<DimChunkKey, HashMap<u32, u32>>);
        impl std::ops::Drop for RestoreWorld {
            fn drop(&mut self) {
                *world().lock().unwrap_or_else(|e| e.into_inner()) =
                    std::mem::take(&mut self.0);
            }
        }
        let _restore = RestoreWorld(saved_world);

        assert!(worldgen::init(0, name_to_state), "overworld gen loads");

        // The biome under the search origin must locate at distance ~0.
        let here = worldgen::with_gen(|g| g.biome_name_at(0, 64, 0))
            .flatten()
            .expect("origin biome resolves");
        let hit = locate_biome_spiral(DimId::Overworld, &here, 0, 64, 0, 6400)
            .expect("the origin's own biome is found");
        assert!(hit.distance <= 3, "origin biome found within a quart: {hit:?}");
        // Verify the hit's biome by asking the generator again at that spot.
        let at_hit = worldgen::with_gen(|g| g.biome_name_at(hit.x, hit.y, hit.z))
            .flatten()
            .unwrap();
        assert_eq!(at_hit, here, "the located position is really {here}");
        // An unknown biome id is a bounded miss, never a hang.
        assert!(locate_biome_spiral(DimId::Overworld, "minecraft:plains_plus_plus", 0, 64, 0, 64).is_none()
            || locate_biome_spiral(DimId::Overworld, "the_void", 0, 64, 0, 0).is_none());

        // Structures: mineshafts are dense enough that a 100-chunk radius is a
        // certain hit, and the found chunk independently passes the real
        // admission check.
        let hit = locate_structure_spiral(
            DimId::Overworld,
            "minecraft:mineshaft",
            0,
            0,
            100,
        )
        .expect("locate runs")
        .expect("a mineshaft within 1600 blocks of spawn on seed 0");
        let (cx, cz) = (hit.x >> 4, hit.z >> 4);
        let independent = worldgen::with_gen(|g| mineshaft::admit(g, cx, cz))
            .flatten()
            .expect("the located chunk is really admitted");
        assert_eq!(mineshaft::start_id(&independent), "minecraft:mineshaft");
        // A bogus structure id is an error, and radius 0 cannot reach far
        // structures but must still answer.
        assert!(locate_structure_spiral(DimId::Overworld, "minecraft:no_such_structure", 0, 0, 4).is_err());
        let bounded = locate_structure_spiral(DimId::Overworld, "minecraft:stronghold", 0, 0, 4)
            .expect("bounded stronghold scan");
        assert!(bounded.is_none(), "no stronghold start inside 4 chunks of spawn");

        // POI: seed a bell and a bed into the overlay, then find them.
        let reg = block_registry::breg().expect("block registry");
        let bell = reg.state_of("minecraft:bell", None);
        let bed = reg.state_of("minecraft:red_bed", None);
        set_block_dim(DimId::Overworld, 48, 80, -32, bell);
        set_block_dim(DimId::Overworld, 96, 70, 96, bed);
        let hit = locate_poi_nearest(DimId::Overworld, "minecraft:meeting", 0, 70, 0, 256)
            .expect("the bell is a meeting POI");
        assert_eq!((hit.x, hit.y, hit.z), (48, 80, -32));
        let hit = locate_poi_nearest(DimId::Overworld, "home", 0, 70, 0, 256)
            .expect("the bed is a home POI");
        assert_eq!((hit.x, hit.y, hit.z), (96, 70, 96));
        // The radius cap is honoured: same POI, too small a radius -> miss.
        assert!(locate_poi_nearest(DimId::Overworld, "minecraft:meeting", 0, 70, 0, 16).is_none());
        // A POI that was never placed misses inside a large radius too.
        assert!(locate_poi_nearest(DimId::Overworld, "minecraft:armorer", 0, 70, 0, 256).is_none());
    }

    #[test]
    fn random_command_value_roll_and_deterministic_sequence_reset() {
        let mut store = RandomSequenceStore::default();
        // value: in-range, sequence advances, replay after `*` reset is identical
        let a1 = exec_random_command(&mut store, 42, &["value", "1..10"], "Tester");
        let a2 = exec_random_command(&mut store, 42, &["value", "1..10"], "Tester");
        assert_ne!(a1, a2, "the sequence advances between draws");
        assert!(a1.starts_with("Random number between 1 and 10: "));
        let v: i32 = a1.rsplit(' ').next().unwrap().parse().unwrap();
        assert!((1..=10).contains(&v));

        let reset = exec_random_command(&mut store, 42, &["reset", "*"], "Tester");
        assert_eq!(reset, "Reset all random number sequences");
        let a3 = exec_random_command(&mut store, 42, &["value", "1..10"], "Tester");
        assert_eq!(a1, a3, "reset replays the deterministic sequence");

        // roll: broadcast phrasing
        exec_random_command(&mut store, 42, &["reset", "*"], "Tester");
        let roll = exec_random_command(&mut store, 42, &["roll", "5..5"], "Tester");
        assert_eq!(roll, "Tester rolled a 5 between 5 and 5");

        // named sequences are independent of the default stream
        exec_random_command(&mut store, 42, &["reset", "*"], "Tester");
        let d1 = exec_random_command(&mut store, 42, &["value", "0..1000"], "T");
        let n1 = exec_random_command(&mut store, 42, &["value", "0..1000", "spine:test"], "T");
        assert_ne!(d1, n1);
        // resetting one sequence replays it alone
        let n2 = exec_random_command(&mut store, 42, &["value", "0..1000", "spine:test"], "T");
        exec_random_command(&mut store, 42, &["reset", "spine:test"], "T");
        let n3 = exec_random_command(&mut store, 42, &["value", "0..1000", "spine:test"], "T");
        assert_eq!(n1, n3);
        assert_ne!(n2, n3, "the earlier draw consumed state before the reset");

        // explicit-seed reset is deterministic across stores
        let mut store2 = RandomSequenceStore::default();
        exec_random_command(&mut store, 42, &["reset", "spine:test", "7"], "T");
        exec_random_command(&mut store2, 42, &["reset", "spine:test", "7"], "T");
        let s1 = exec_random_command(&mut store, 42, &["value", "0..1000000", "spine:test"], "T");
        let s2 = exec_random_command(&mut store2, 42, &["value", "0..1000000", "spine:test"], "T");
        assert_eq!(s1, s2, "same seed -> same sequence, across stores");

        // validation
        assert!(exec_random_command(&mut store, 42, &["value", "9..1"], "T").contains("reversed"));
        assert!(exec_random_command(&mut store, 42, &["nonsense"], "T").starts_with("usage:"));
    }

    #[test]
    fn mcfunction_macro_expansion_and_return_command_parsing() {
        let args: BTreeMap<String, String> = [
            ("name".to_owned(), "Steve".to_owned()),
            ("n".to_owned(), "7".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            expand_mcfunction_macro("$say hello $(name), you got $(n)", &args).unwrap(),
            "say hello Steve, you got 7"
        );
        // $$ escapes to a literal $; a lone $ inside the command is verbatim.
        assert_eq!(
            expand_mcfunction_macro("$give $(name) $$5 bills", &args).unwrap(),
            "give Steve $5 bills"
        );
        // missing key -> the whole line fails, per vanilla macro rules
        assert!(expand_mcfunction_macro("$say hi $(who)", &args).is_err());
        // invalid variable names are rejected cleanly
        assert!(expand_mcfunction_macro("$say $(a b)", &args).is_err());
        assert!(expand_mcfunction_macro("$say $()", &args).is_err());
        assert!(expand_mcfunction_macro("$say $(name", &args).is_err());
        // not a macro line at all
        assert!(expand_mcfunction_macro("say hi", &args).is_err());
        // a macro line with no placeholders expands to itself
        assert_eq!(
            expand_mcfunction_macro("$say static", &args).unwrap(),
            "say static"
        );

        // /return
        assert_eq!(parse_return_command(&["5"]).unwrap(), FunctionReturn::Value(5));
        assert_eq!(parse_return_command(&["-3"]).unwrap(), FunctionReturn::Value(-3));
        assert_eq!(parse_return_command(&["fail"]).unwrap(), FunctionReturn::Fail);
        assert_eq!(
            parse_return_command(&["run", "say", "hi", "there"]).unwrap(),
            FunctionReturn::Run("say hi there".to_owned())
        );
        assert!(parse_return_command(&[]).is_err());
        assert!(parse_return_command(&["run"]).is_err());
        assert!(parse_return_command(&["abc"]).is_err());
        assert!(parse_return_command(&["1", "2"]).is_err());
    }
}

// ------------------------------------------------------------ p3_parity ---

#[cfg(test)]
mod p3_parity_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn p3_packet_client(eid: i32, name: &str, x: f64, y: f64, z: f64) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.name = name.into();
        *client.pos.lock().unwrap() = (x, y, z, 0.0, 0.0, true);
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .unwrap();
        (Arc::new(client), receiver)
    }

    fn p3_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn p3_frame_varint(r: &mut TcpStream) -> Option<(i64, usize)> {
        let mut value = 0i64;
        for i in 0..5 {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).ok()?;
            value |= ((b[0] & 0x7f) as i64) << (7 * i);
            if b[0] & 0x80 == 0 {
                return Some((value, i + 1));
            }
        }
        None
    }

    /// One uncompressed frame: varint length, varint packet id, payload.
    fn p3_read_frame(r: &mut TcpStream) -> Option<(i64, Vec<u8>)> {
        let (len, _) = p3_frame_varint(r)?;
        let (id, used) = p3_frame_varint(r)?;
        let mut payload = vec![0u8; (len as usize).checked_sub(used)?];
        r.read_exact(&mut payload).ok()?;
        Some((id, payload))
    }

    fn p3_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    fn p3_tag_entity(eid: i32, tag: &str) {
        tags()
            .lock()
            .unwrap()
            .entry(format!("e:{eid}"))
            .or_default()
            .insert(tag.into());
    }

    #[test]
    fn p3_parity_ride_mount_dismount_self_reject_and_level_two() {
        let _world = world_guard();
        let (player, _recv) = p3_packet_client(9101, "p3rider", 0.5, 16.0, 0.5);
        clients().lock().unwrap().push(player.clone());
        assert!(insert_live_mob(
            name_to_etype("horse").unwrap(),
            DimId::Overworld,
            (4.5, 16.0, 4.5, 0.0, 0.0),
            20.0,
            None,
        ));
        let mob_eid = mobs().lock().unwrap().last().unwrap().eid;
        p3_tag_entity(mob_eid, "p3ride_vehicle");

        // Level-2 enforcement through the real permission table + the wire
        // gate: a level-0 player context is refused before dispatch.
        assert!(!command_allowed_at_level("ride", 1));
        assert!(command_allowed_at_level("ride", 2));
        let denied = p3_run(&p3_ctx(Some(player.clone())), "/ride p3rider mount @e[tag=p3ride_vehicle]");
        assert!(
            denied.contains("permission"),
            "level-0 player must be refused: {denied}"
        );

        let ctx = p3_ctx(None); // console: level 4
        assert_eq!(
            p3_run(&ctx, "/ride p3rider mount @e[tag=p3ride_vehicle]"),
            "Now riding minecraft:horse"
        );
        assert_eq!(vehicle_of(player.eid), Some(mob_eid));
        {
            let live = mounts().lock().unwrap();
            assert_eq!(live.registry.passengers_of(mob_eid), &[player.eid]);
        }
        // self-mount is rejected by the registry (vanilla EntityLoopException)
        assert_eq!(
            p3_run(&ctx, "/ride p3rider mount p3rider"),
            "Entity cannot ride itself"
        );
        assert_eq!(p3_run(&ctx, "/ride p3rider dismount"), "No longer riding");
        assert!(vehicle_of(player.eid).is_none());
        assert_eq!(p3_run(&ctx, "/ride p3rider dismount"), "Not riding anything");

        // cycle: A rides B, so B cannot ride A.
        assert!(insert_live_mob(
            name_to_etype("zombie").unwrap(),
            DimId::Overworld,
            (8.5, 16.0, 4.5, 0.0, 0.0),
            20.0,
            None,
        ));
        let a = mobs().lock().unwrap().last().unwrap().eid;
        p3_tag_entity(a, "p3ride_a");
        assert!(insert_live_mob(
            name_to_etype("zombie").unwrap(),
            DimId::Overworld,
            (9.5, 16.0, 4.5, 0.0, 0.0),
            20.0,
            None,
        ));
        let b = mobs().lock().unwrap().last().unwrap().eid;
        p3_tag_entity(b, "p3ride_b");
        assert_eq!(
            p3_run(&ctx, "/ride @e[tag=p3ride_a,limit=1] mount @e[tag=p3ride_b,limit=1]"),
            "Now riding minecraft:zombie"
        );
        assert_eq!(
            p3_run(&ctx, "/ride @e[tag=p3ride_b,limit=1] mount @e[tag=p3ride_a,limit=1]"),
            "Can't ride an entity that is already riding this entity, this would create a circular rider list"
        );
        p3_run(&ctx, "/ride @e[tag=p3ride_a,limit=1] dismount");

        // teardown
        mounts().lock().unwrap().registry.dismount(player.eid);
        mounts().lock().unwrap().registry.dismount(a);
        clients().lock().unwrap().retain(|c| c.eid != player.eid);
        mobs().lock().unwrap().retain(|m| m.eid != mob_eid && m.eid != a && m.eid != b);
        tags().lock().unwrap().remove(&format!("e:{mob_eid}"));
        tags().lock().unwrap().remove(&format!("e:{a}"));
        tags().lock().unwrap().remove(&format!("e:{b}"));
    }

    #[test]
    fn p3_parity_spectate_requires_spectator_and_sets_camera() {
        let _world = world_guard();
        let (viewer, mut recv) = p3_packet_client(9201, "p3watcher", 0.5, 16.0, 0.5);
        let (target, _rt) = p3_packet_client(9202, "p3prey", 1.5, 16.0, 0.5);
        clients().lock().unwrap().extend([viewer.clone(), target.clone()]);
        let ctx = p3_ctx(None);

        // level 2 gate + spectator-only check
        assert!(!command_allowed_at_level("spectate", 1));
        assert!(command_allowed_at_level("spectate", 2));
        assert_eq!(
            p3_run(&ctx, "/spectate p3prey p3watcher"),
            "Only players in spectator mode can use this command"
        );

        // Spectator: target set sends the verified protocol-767 set_camera id.
        assert_eq!(SET_CAMERA_PACKET_767, 0x52);
        *viewer.gm.lock().unwrap() = 3;
        assert_eq!(
            p3_run(&ctx, "/spectate p3prey p3watcher"),
            "Now spectating minecraft:player"
        );
        let (id, payload) = p3_read_frame(&mut recv).expect("set_camera frame");
        assert_eq!(id, 0x52, "767 set_camera is 0x52 (proto/src/data_v767.rs)");
        let mut body = Vec::new();
        write_varint(&mut body, i64::from(target.eid));
        assert_eq!(payload, body, "camera payload is the target's VarInt eid");
        assert_eq!(spectator_cameras().lock().unwrap().get(&viewer.eid), Some(&target.eid));

        // Bare `/spectate` on the viewer resets the camera to their own eid.
        assert_eq!(
            spectate_command(&p3_ctx(Some(viewer.clone())), &[]),
            "No longer spectating"
        );
        let (id, payload) = p3_read_frame(&mut recv).expect("reset frame");
        assert_eq!(id, 0x52);
        let mut body = Vec::new();
        write_varint(&mut body, i64::from(viewer.eid));
        assert_eq!(payload, body);
        assert_eq!(spectator_cameras().lock().unwrap().get(&viewer.eid), Some(&viewer.eid));

        // non-player `<player>` arg and console default viewer are refused
        assert_eq!(
            p3_run(&ctx, "/spectate p3prey @e[tag=p3notanentity]"),
            "No player was found"
        );
        assert_eq!(p3_run(&ctx, "/spectate"), "Only players may use this command");

        *viewer.gm.lock().unwrap() = 0;
        spectator_cameras().lock().unwrap().remove(&viewer.eid);
        clients().lock().unwrap().retain(|c| c.eid != viewer.eid && c.eid != target.eid);
    }

    #[test]
    fn p3_parity_fillbiome_box_replace_filter_and_fail_closed() {
        let _world = world_guard();
        let ctx = p3_ctx(None);
        let dim = cmd_dim(&ctx);
        assert!(!command_allowed_at_level("fillbiome", 1));
        assert!(command_allowed_at_level("fillbiome", 2));

        // Box 1000..1007 x 100..107 x 2000..2007 -> quart (250..=251)^2 x (25) —
        // exactly 8 cells, matching CubePos.fromBlock (>>2) corner mapping.
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1007 107 2007 plains"),
            "Changed biome of 8 cells to minecraft:plains"
        );
        {
            let store = biome_overrides().lock().unwrap();
            let dim_map = &store[&dim];
            for qy in 25..=25 {
                for qz in 500..=501 {
                    for qx in 250..=251 {
                        assert_eq!(dim_map[&(qx, qy, qz)], "minecraft:plains");
                    }
                }
            }
        }
        assert_eq!(
            effective_biome_name(dim, 250, 25, 500).as_deref(),
            Some("minecraft:plains")
        );

        // `replace` predicate matches the override-aware current biome.
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1007 107 2007 desert replace plains"),
            "Changed biome of 8 cells to minecraft:desert"
        );
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1007 107 2007 forest replace plains"),
            "Changed biome of 0 cells to minecraft:forest",
            "no cell is plains any more"
        );

        // `#tag` filter through the loaded registry (injected tag fixture).
        {
            let mut registry = datapack_registry().lock().unwrap();
            registry
                .tags
                .entry("biome".to_owned())
                .or_default()
                .insert("test:p3_is_desert".to_owned(), vec!["minecraft:desert".to_owned()]);
        }
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1007 107 2007 jungle replace #test:p3_is_desert"),
            "Changed biome of 8 cells to minecraft:jungle"
        );
        datapack_registry()
            .lock()
            .unwrap()
            .tags
            .entry("biome".to_owned())
            .or_default()
            .remove("test:p3_is_desert");

        // fail-closed: unknown biome id, unknown filter, out-of-world, limit.
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1007 107 2007 notabiome"),
            "Unknown biome 'notabiome'"
        );
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1007 107 2007 plains replace notabiome"),
            "Unknown biome filter 'notabiome'"
        );
        assert_eq!(
            p3_run(&ctx, "/fillbiome 0 400 0 0 400 0 plains"),
            "Block position is outside the world"
        );
        GR_COMMAND_MODIFICATION_BLOCK_LIMIT.store(2, Ordering::Relaxed);
        assert_eq!(
            p3_run(&ctx, "/fillbiome 1000 100 2000 1016 100 2000 plains"),
            "Too many blocks (5, maximum 2)"
        );
        GR_COMMAND_MODIFICATION_BLOCK_LIMIT.store(32768, Ordering::Relaxed);

        // relative `~` coords resolve against the executor position (0.5,16,0.5)
        assert_eq!(
            p3_run(&ctx, "/fillbiome ~400 ~80 ~600 ~401 ~81 ~601 savanna"),
            "Changed biome of 1 cells to minecraft:savanna"
        );

        biome_overrides().lock().unwrap().remove(&dim);
    }

    #[test]
    fn p3_parity_place_feature_and_template_validation_and_dispatch() {
        crate::test_support::require_vanilla_data();
        let _world = world_guard();
        let ctx = p3_ctx(None);
        let dim = cmd_dim(&ctx);
        assert!(!command_allowed_at_level("place", 1));
        assert!(command_allowed_at_level("place", 2));

        // feature: real configured_feature from datapacks/vanilla dispatches.
        let before = place_dispatches().lock().unwrap().len();
        assert_eq!(
            p3_run(&ctx, "/place feature minecraft:patch_taiga_grass 3000 90 3000"),
            "Placed feature 'minecraft:patch_taiga_grass' at [3000, 90, 3000]"
        );
        {
            let log = place_dispatches().lock().unwrap();
            assert_eq!(log.len(), before + 1);
            let dispatch = log.last().unwrap();
            assert_eq!(dispatch.kind, PlaceKind::Feature);
            assert_eq!(dispatch.dim, dim);
            assert_eq!(dispatch.id, "minecraft:patch_taiga_grass");
            assert_eq!(dispatch.pos, (3000, 90, 3000));
        }
        assert_eq!(
            p3_run(&ctx, "/place feature minecraft:not_a_feature 0 90 0"),
            "No feature with the name 'minecraft:not_a_feature' exists"
        );

        // template: the real woodland_mansion carpet palette is stamped for
        // real — capture the pre-existing states so the fixture restores.
        let anchor = (5000, 250, 5000);
        let reply = p3_run(&ctx, "/place template minecraft:woodland_mansion/carpet_north 5000 250 5000");
        assert!(
            reply.starts_with("Placed template 'minecraft:woodland_mansion/carpet_north' ("),
            "unexpected: {reply}"
        );
        {
            let log = place_dispatches().lock().unwrap();
            assert_eq!(log.last().unwrap().kind, PlaceKind::Template);
        }
        // restore the area to air (fixture was placed over untouched terrain)
        for dx in -2..8 {
            for dz in -2..8 {
                for dy in 0..4 {
                    place_block_dim(dim, anchor.0 + dx, anchor.1 + dy, anchor.2 + dz, 0);
                }
            }
        }
        assert_eq!(
            p3_run(&ctx, "/place template minecraft:not_a_template 0 90 0"),
            "No such template 'minecraft:not_a_template'"
        );

        // validation: usage, unknown subcommand, bad id, out-of-world pos
        assert!(p3_run(&ctx, "/place").starts_with("usage:"));
        assert!(p3_run(&ctx, "/place jigsaw minecraft:x").starts_with("usage:"));
        assert!(p3_run(&ctx, "/place feature Bad ID").contains("Invalid id"));
        assert_eq!(
            p3_run(&ctx, "/place feature minecraft:oak 0 400 0"),
            "Block position is outside the world"
        );
        // default pos = executor feet (0.5,16,0.5 -> 0,16,0)
        let before = place_dispatches().lock().unwrap().len();
        assert_eq!(
            p3_run(&ctx, "/place feature minecraft:patch_taiga_grass"),
            "Placed feature 'minecraft:patch_taiga_grass' at [0, 16, 0]"
        );
        assert_eq!(place_dispatches().lock().unwrap().len(), before + 1);
    }
}

#[cfg(test)]
mod p5d_parity_tests {
    // Lane P5d — issues #159, #163, #166, #168, #171, #172, #177, #224.
    use super::*;
    use std::net::TcpStream;

    fn p5d_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn p5d_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    fn p5d_client(eid: i32, uuid_byte: u8, name: &str) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.uuid = [uuid_byte; 16];
        client.name = name.into();
        (Arc::new(client), receiver)
    }

    // #168 — /time add accepts the vanilla TimeArgument suffixes and keeps the
    // live day-time store coherent with set/query.
    #[test]
    fn p5d_parity_time_add_accepts_t_s_d_suffixes() {
        assert_eq!(time_argument_ticks("7"), Some(7));
        assert_eq!(time_argument_ticks("7t"), Some(7));
        assert_eq!(time_argument_ticks("2s"), Some(40));
        assert_eq!(time_argument_ticks("1d"), Some(24_000));
        assert_eq!(time_argument_ticks("0.5s"), Some(10));
        assert_eq!(time_argument_ticks("0"), Some(0));
        assert_eq!(time_argument_ticks("-1"), None);
        assert_eq!(time_argument_ticks("bogus"), None);
        assert_eq!(time_argument_ticks("1x"), None);
        assert_eq!(time_argument_ticks("NaN"), None);

        let _world = world_guard();
        let ctx = p5d_ctx(None);
        let before = WORLD_TIME.load(Ordering::Relaxed);
        let reply = p5d_run(&ctx, "/time add 1d");
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), before + 24_000);
        assert_eq!(reply, format!("Set the time to {}", before + 24_000));
        assert_eq!(
            p5d_run(&ctx, "/time add 2s"),
            format!("Set the time to {}", before + 24_040)
        );
        assert_eq!(p5d_run(&ctx, "/time add -1"), "usage: /time add <ticks>");
        assert_eq!(p5d_run(&ctx, "/time add bogus"), "usage: /time add <ticks>");
        assert_eq!(WORLD_TIME.load(Ordering::Relaxed), before + 24_040);
        assert_eq!(
            p5d_run(&ctx, "/time query daytime"),
            format!("The time is {}", (before + 24_040) % 24_000)
        );
        WORLD_TIME.store(before, Ordering::Relaxed);
    }

    // #166 — /schedule function honours t/s/d times plus append/replace, and
    // /schedule clear reports the removed entry count.
    #[test]
    fn p5d_parity_schedule_function_times_and_clear() {
        let _world = world_guard();
        let ctx = p5d_ctx(None);
        let saved = std::mem::take(&mut *sched_fns().lock().unwrap());
        let now = TICK.load(Ordering::Relaxed);

        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:alpha 1s"),
            "Scheduled function 'p5d:alpha' in 20 ticks"
        );
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:beta 2d append"),
            "Scheduled function 'p5d:beta' in 48000 ticks"
        );
        // append keeps the existing entry for the same id.
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:alpha 5t append"),
            "Scheduled function 'p5d:alpha' in 5 ticks"
        );
        {
            let scheduled = sched_fns().lock().unwrap();
            let alpha: Vec<u64> = scheduled
                .iter()
                .filter(|(name, _)| name == "p5d:alpha")
                .map(|(_, due)| *due)
                .collect();
            assert_eq!(alpha, vec![now + 20, now + 5], "{scheduled:?}");
            assert_eq!(scheduled.len(), 3);
        }
        // replace (the default) collapses to a single entry.
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:alpha 3t"),
            "Scheduled function 'p5d:alpha' in 3 ticks"
        );
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:alpha 3t replace"),
            "Scheduled function 'p5d:alpha' in 3 ticks"
        );
        {
            let scheduled = sched_fns().lock().unwrap();
            let alpha: Vec<u64> = scheduled
                .iter()
                .filter(|(name, _)| name == "p5d:alpha")
                .map(|(_, due)| *due)
                .collect();
            assert_eq!(alpha, vec![now + 3], "{scheduled:?}");
        }
        // clear <id> removes every pending entry and reports the count.
        assert_eq!(p5d_run(&ctx, "/schedule clear p5d:alpha"), "Removed 1 scheduled function(s)");
        assert_eq!(p5d_run(&ctx, "/schedule clear p5d:alpha"), "Removed 0 scheduled function(s)");
        assert_eq!(p5d_run(&ctx, "/schedule clear"), "Removed 1 scheduled function(s)");
        assert!(sched_fns().lock().unwrap().is_empty());
        // malformed time and unknown mode are rejected like vanilla parse errors.
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:x nope"),
            "usage: /schedule function <fn> <time>[t|s|d] [append|replace]"
        );
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:x 1t sideways"),
            "usage: /schedule function <fn> <time>[t|s|d] [append|replace]"
        );
        *sched_fns().lock().unwrap() = saved;
    }

    // #163 — /help lists the caller-visible commands, /help <command> prints
    // exact usage or the vanilla unknown/unpermitted error, and /? aliases it.
    #[test]
    fn p5d_parity_help_lists_usage_and_question_alias() {
        let _world = world_guard();
        let console = p5d_ctx(None);

        let listed = p5d_run(&console, "/help");
        assert!(listed.starts_with("commands: "), "{listed}");
        for name in ["help", "time", "spreadplayers", "scoreboard", "bossbar"] {
            assert!(listed.split_whitespace().any(|c| c == name), "{name} missing from {listed}");
        }
        assert_eq!(
            p5d_run(&console, "/help time"),
            "Usage: /time add <time>[t|s|d] | set <day|noon|night|midnight|ticks> | query <day|daytime|gametime>"
        );
        assert_eq!(
            p5d_run(&console, "/help spreadplayers"),
            "Usage: /spreadplayers <x> <z> <spreadDistance> <maxRange> [under <maxHeight>] <respectTeams> <targets>"
        );
        assert_eq!(
            p5d_run(&console, "/help nosuchthing"),
            "Unknown command or insufficient permissions"
        );
        assert_eq!(
            p5d_run(&console, "/help /time"),
            p5d_run(&console, "/help time"),
            "leading slash accepted"
        );

        // /? behaves identically for operators and the console.
        assert_eq!(p5d_run(&console, "/?"), listed);
        assert_eq!(p5d_run(&console, "/? time"), p5d_run(&console, "/help time"));
        assert_eq!(p5d_run(&console, "/? nosuchthing"), "Unknown command or insufficient permissions");
        assert_eq!(command_usage("?"), "Usage: /? [<command>]");
        // The pinned contract: ? is an operator alias, not a level-0 command.
        assert!(command_allowed_at_level("?", 2));
        assert!(!command_allowed_at_level("?", 0));
        assert!(!command_allowed_for_player("?", &[]));

        // A level-0 player gets /help filtered to level-0 commands, exact usage
        // for permitted commands, and the vanilla error for unpermitted ones.
        let (player, _recv) = p5d_client(60001, 0x51, "p5d_visitor");
        let player_ctx = p5d_ctx(Some(player));
        let player_help = p5d_run(&player_ctx, "/help");
        assert!(player_help.split_whitespace().any(|c| c == "help"), "{player_help}");
        assert!(!player_help.split_whitespace().any(|c| c == "op"), "{player_help}");
        assert_eq!(p5d_run(&player_ctx, "/help list"), "Usage: /list no arguments; list online players");
        assert_eq!(
            p5d_run(&player_ctx, "/help op"),
            "Unknown command or insufficient permissions"
        );
        // `?` is gated at level 2, so a visitor cannot run the alias at all.
        assert_eq!(
            p5d_run(&player_ctx, "/?"),
            "You do not have permission to run that command"
        );
    }

    // #171 — /forceload query maps block coords >> 4 to the chunk and
    // /forceload remove all clears the set while reporting the count.
    #[test]
    fn p5d_parity_forceload_query_chunk_and_remove_all() {
        let _world = world_guard();
        let previous = std::mem::replace(&mut *forceloaded().lock().unwrap(), HashSet::new());
        let ctx = p5d_ctx(None);

        let added = p5d_run(&ctx, "/forceload add 32 48");
        assert!(added.contains("1 chunks"), "{added}");
        assert!(forceloaded().lock().unwrap().contains(&(2, 3)));
        // every block inside the chunk reports forced; the neighbour does not.
        assert_eq!(p5d_run(&ctx, "/forceload query 32 48"), "Chunk 2, 3 is force loaded");
        assert_eq!(p5d_run(&ctx, "/forceload query 47 63"), "Chunk 2, 3 is force loaded");
        assert_eq!(p5d_run(&ctx, "/forceload query 16 48"), "Chunk 1, 3 is not force loaded");
        assert_eq!(p5d_run(&ctx, "/forceload query -1 -1"), "Chunk -1, -1 is not force loaded");
        let removed = p5d_run(&ctx, "/forceload remove all");
        assert_eq!(removed, "Removed 1 forceloaded chunk(s)");
        assert!(forceloaded().lock().unwrap().is_empty());
        assert_eq!(p5d_run(&ctx, "/forceload query"), "0 forceloaded chunk(s)");

        *forceloaded().lock().unwrap() = previous;
    }

    // #172 — /setworldspawn and /spawnpoint take the optional yaw and persist
    // it beside the coordinates.
    #[test]
    fn p5d_parity_spawn_commands_store_angle() {
        let _world = world_guard();
        let ctx = p5d_ctx(None);
        let previous = *WORLD_SPAWN.lock().unwrap();

        assert_eq!(spawn_yaw(Some("270.5"), None), Some(-89.5));
        assert_eq!(spawn_yaw(None, None), Some(0.0));
        assert_eq!(spawn_yaw(Some("NaN"), None), None);

        let reply = p5d_run(&ctx, "/setworldspawn 10 70 20 135");
        assert_eq!(reply, "Set the world spawn point to 10, 70, 20 facing 135");
        assert_eq!(*WORLD_SPAWN.lock().unwrap(), (10.0, 70.0, 20.0, 135.0));
        // no angle defaults to 0.0
        p5d_run(&ctx, "/setworldspawn 1 2 3");
        assert_eq!(*WORLD_SPAWN.lock().unwrap(), (1.0, 2.0, 3.0, 0.0));
        assert_eq!(p5d_run(&ctx, "/setworldspawn 1 2 3 nope"), "Invalid spawn angle");

        let (player, _recv) = p5d_client(60002, 0x52, "p5d_spawn");
        clients().lock().unwrap().push(player.clone());
        let reply = p5d_run(&ctx, "/spawnpoint p5d_spawn 5 65 15 -90");
        assert_eq!(reply, "Set spawn point to 5, 65, 15 facing -90 for p5d_spawn");
        assert_eq!(*player.spawn_pt.lock().unwrap(), Some((5.0, 65.0, 15.0, -90.0)));
        // executor spawnpoint without pos uses the live position and yaw 0.
        ops_registry().lock().unwrap().set("p5d_spawn", 4, false);
        *player.pos.lock().unwrap() = (7.5, 64.0, -3.5, 0.0, 0.0, true);
        let player_ctx = p5d_ctx(Some(player.clone()));
        assert_eq!(
            p5d_run(&player_ctx, "/spawnpoint"),
            "Set spawn point to 7.5, 64, -3.5 facing 0 for p5d_spawn"
        );
        assert_eq!(*player.spawn_pt.lock().unwrap(), Some((7.5, 64.0, -3.5, 0.0)));
        ops_registry()
            .lock()
            .unwrap()
            .entries
            .retain(|e| e.name != "p5d_spawn");

        clients().lock().unwrap().retain(|c| c.eid != 60002);
        *WORLD_SPAWN.lock().unwrap() = previous;
    }

    // #177 — /bossbar set players assigns the viewer list (empty clears it)
    // and /bossbar get reports exact integer results.
    #[test]
    fn p5d_parity_bossbar_players_and_exact_get_results() {
        let _world = world_guard();
        let saved = std::mem::take(&mut *bossbars().lock().unwrap());
        let (a, _ra) = p5d_client(60011, 0x61, "p5d_a");
        let (b, _rb) = p5d_client(60012, 0x62, "p5d_b");
        clients().lock().unwrap().push(a.clone());
        clients().lock().unwrap().push(b.clone());

        assert_eq!(
            run_bossbar_command(&["add", "bar", "Title"], None),
            "Created bossbar minecraft:bar"
        );
        // get max/value/visible decode to their exact vanilla results.
        for (args, expect) in [
            (vec!["get", "bar", "max"], 100),
            (vec!["get", "bar", "value"], 0),
            (vec!["get", "bar", "visible"], 1),
        ] {
            let reply = run_bossbar_command(&args, None);
            assert_eq!(
                execute_legacy_result("bossbar", &reply),
                Some(expect),
                "{args:?} -> {reply}"
            );
        }
        run_bossbar_command(&["set", "bar", "max", "50"], None);
        run_bossbar_command(&["set", "bar", "value", "30"], None);
        run_bossbar_command(&["set", "bar", "visible", "false"], None);
        for (args, expect) in [
            (vec!["get", "bar", "max"], 50),
            (vec!["get", "bar", "value"], 30),
            (vec!["get", "bar", "visible"], 0),
        ] {
            let reply = run_bossbar_command(&args, None);
            assert_eq!(execute_legacy_result("bossbar", &reply), Some(expect), "{reply}");
        }

        // set players assigns the viewer list; get players reports the count.
        assert_eq!(
            run_bossbar_command(&["set", "bar", "players", "p5d_a"], None),
            "Updated bossbar minecraft:bar"
        );
        let reply = run_bossbar_command(&["get", "bar", "players"], None);
        assert!(reply.starts_with("1 player(s) on bossbar minecraft:bar"), "{reply}");
        assert_eq!(execute_legacy_result("bossbar", &reply), Some(1));
        run_bossbar_command(&["set", "bar", "players", "@a"], None);
        let reply = run_bossbar_command(&["get", "bar", "players"], None);
        assert!(reply.starts_with("2 player(s)"), "{reply}");
        assert_eq!(execute_legacy_result("bossbar", &reply), Some(2));
        // an empty argument list clears the viewers entirely.
        run_bossbar_command(&["set", "bar", "players"], None);
        let reply = run_bossbar_command(&["get", "bar", "players"], None);
        assert!(reply.starts_with("0 player(s)"), "{reply}");
        assert_eq!(execute_legacy_result("bossbar", &reply), Some(0));
        // a selector that matches nobody is the vanilla failure.
        assert_eq!(
            run_bossbar_command(&["set", "bar", "players", "nobody_here"], None),
            "No players found"
        );
        assert_eq!(
            run_bossbar_command(&["get", "none", "max"], None),
            "Unknown bossbar 'minecraft:none'"
        );

        clients().lock().unwrap().retain(|c| c.eid != 60011 && c.eid != 60012);
        *bossbars().lock().unwrap() = saved;
    }

    // #224 — objectives modify displayname/rendertype, players list forms, and
    // the full vanilla criteria alias set.
    #[test]
    fn p5d_parity_scoreboard_modify_list_and_criteria_aliases() {
        let _world = world_guard();
        let mut board = ScoreboardEngine::default();

        // every vanilla criteria alias is accepted, bogus names still fail.
        for criteria in [
            "dummy", "trigger", "health", "deathCount", "playerKillCount",
            "totalKillCount", "xp", "level", "food", "air", "armor",
        ] {
            let name = format!("obj_{criteria}");
            let reply = apply_scoreboard_command(&mut board, &["objectives", "add", &name, criteria]);
            assert!(reply.is_ok(), "{criteria}: {reply:?}");
        }
        assert_eq!(
            apply_scoreboard_command(&mut board, &["objectives", "add", "bad", "notacriterion"]),
            Err("Unknown scoreboard criterion: notacriterion".into())
        );
        // the stat aliases map onto a usable objective (writable dummy form).
        assert_eq!(
            apply_scoreboard_command(&mut board, &["players", "set", "s1", "obj_xp", "9"]),
            Ok("s1 has 9 in obj_xp".into())
        );

        // objectives modify displayname + rendertype.
        assert_eq!(
            apply_scoreboard_command(
                &mut board,
                &["objectives", "modify", "obj_xp", "displayname", "XP", "Bar"]
            ),
            Ok("Changed display name of objective obj_xp".into())
        );
        assert_eq!(board.objective("obj_xp").unwrap().display_name, "XP Bar");
        assert_eq!(
            apply_scoreboard_command(
                &mut board,
                &["objectives", "modify", "obj_xp", "rendertype", "hearts"]
            ),
            Ok("Changed render type of objective obj_xp".into())
        );
        assert_eq!(
            board.objective("obj_xp").unwrap().render_type,
            ScoreboardRenderType::Hearts
        );
        assert_eq!(
            apply_scoreboard_command(
                &mut board,
                &["objectives", "modify", "obj_xp", "rendertype", "integer"]
            ),
            Ok("Changed render type of objective obj_xp".into())
        );
        assert_eq!(
            board.objective("obj_xp").unwrap().render_type,
            ScoreboardRenderType::Integer
        );
        assert_eq!(
            apply_scoreboard_command(
                &mut board,
                &["objectives", "modify", "missing", "rendertype", "hearts"]
            ),
            Err("Unknown objective: missing".into())
        );
        assert_eq!(
            apply_scoreboard_command(
                &mut board,
                &["objectives", "modify", "obj_xp", "rendertype", "fancy"]
            ),
            Err("Unknown render type: fancy".into())
        );

        // players list: all tracked holders, then per-holder breakdown.
        apply_scoreboard_command(&mut board, &["players", "set", "s2", "obj_xp", "4"]).unwrap();
        apply_scoreboard_command(&mut board, &["players", "set", "s1", "obj_air", "2"]).unwrap();
        assert_eq!(
            apply_scoreboard_command(&mut board, &["players", "list"]),
            Ok("2 score holders: s1, s2".into())
        );
        assert_eq!(
            apply_scoreboard_command(&mut board, &["players", "list", "s1"]),
            Ok("2 scores for s1: [2] obj_air, [9] obj_xp".into())
        );
        assert_eq!(
            apply_scoreboard_command(&mut board, &["players", "list", "ghost"]),
            Ok("0 scores for ghost: ".into())
        );
        // counts decode exactly for /execute store result.
        assert_eq!(
            execute_legacy_result("scoreboard", "2 score holders: s1, s2"),
            Some(2)
        );
        assert_eq!(
            execute_legacy_result("scoreboard", "2 scores for s1: [2] obj_air, [9] obj_xp"),
            Some(2)
        );
    }

    // #159 — /spreadplayers validates its numeric window, parses respectTeams,
    // groups teammates into one pile, and is registered at permission level 2.
    #[test]
    fn p5d_parity_spreadplayers_validation_and_team_piles() {
        crate::test_support::require_vanilla_data();
        let _world = world_guard();
        let ctx = p5d_ctx(None);

        // level-2 registration, like every other cheat literal.
        assert!(!command_allowed_at_level("spreadplayers", 1));
        assert!(command_allowed_at_level("spreadplayers", 2));
        // argument validation precedes any target or world lookup.
        assert_eq!(
            p5d_run(&ctx, "/spreadplayers 0 0 -1 10 false @a"),
            "Distance is too small."
        );
        assert_eq!(
            p5d_run(&ctx, "/spreadplayers 0 0 9 9 false @a"),
            "Max range is too small."
        );
        assert_eq!(
            p5d_run(&ctx, "/spreadplayers 0 0 1 10 maybe @a"),
            "'maybe' is not true or false"
        );
        assert!(p5d_run(&ctx, "/spreadplayers 0 0 1 10 true").starts_with("usage:"));
        assert_eq!(
            p5d_run(&ctx, "/spreadplayers 0 0 1 10 under 4000 true @a"),
            "Invalid max height"
        );

        // a guaranteed safe surface: stone floor at y=240 over the spread area.
        let dim = DimId::Overworld;
        for x in -11..=11 {
            for z in -11..=11 {
                place_block_dim(dim, x, 240, z, 1);
            }
        }
        let (a, _ra) = p5d_client(60021, 0x6a, "p5d_alpha");
        let (b, _rb) = p5d_client(60022, 0x6b, "p5d_beta");
        let (c, _rc) = p5d_client(60023, 0x6c, "p5d_solo");
        clients().lock().unwrap().push(a.clone());
        clients().lock().unwrap().push(b.clone());
        clients().lock().unwrap().push(c.clone());

        // team members share one pile; unteamed players keep their own.
        {
            let mut registry = teams().lock().unwrap();
            registry.add("p5d_team", None).unwrap();
            registry.join("p5d_team", "p5d_alpha").unwrap();
            registry.join("p5d_team", "p5d_beta").unwrap();
        }
        assert_eq!(
            spreadplayers_team(&SpreadTarget::Player(a.clone())),
            Some("p5d_team".to_string())
        );
        assert_eq!(spreadplayers_team(&SpreadTarget::Player(c.clone())), None);

        let reply = p5d_run(&ctx, "/spreadplayers 0 0 4 10 true @a");
        assert!(
            reply.starts_with("Successfully spread 2 teams around 0, 0"),
            "{reply}"
        );
        {
            let (pa, pb) = (*a.pos.lock().unwrap(), *b.pos.lock().unwrap());
            assert_eq!((pa.0, pa.1, pa.2), (pb.0, pb.1, pb.2), "teammates share a pile");
            assert_eq!(pa.1, 241.0, "feet y sits on the floor");
        }
        // respectTeams=false gives every entity its own pile.
        let reply = p5d_run(&ctx, "/spreadplayers 0 0 4 10 false @a");
        assert!(
            reply.starts_with("Successfully spread 3 entities around 0, 0"),
            "{reply}"
        );

        // mob lookup uses the string UUID key just like /team join stores it.
        assert!(insert_live_mob(
            name_to_etype("zombie").unwrap(),
            DimId::Overworld,
            (1.5, 241.0, 1.5, 0.0, 0.0),
            20.0,
            None,
        ));
        let mob = mobs().lock().unwrap().last().unwrap().clone();
        let mob_key = uuid_text(&mob.uuid);
        {
            let mut registry = teams().lock().unwrap();
            registry.join("p5d_team", &mob_key).unwrap();
        }
        assert_eq!(
            spreadplayers_team(&SpreadTarget::Mob(mob.clone())),
            Some("p5d_team".to_string())
        );

        // teardown: team, clients, mob and the floor.
        {
            let mut registry = teams().lock().unwrap();
            let _ = registry.remove("p5d_team");
        }
        clients()
            .lock()
            .unwrap()
            .retain(|p| !matches!(p.eid, 60021 | 60022 | 60023));
        mobs().lock().unwrap().retain(|m| m.eid != mob.eid);
        for x in -11..=11 {
            for z in -11..=11 {
                place_block_dim(dim, x, 240, z, 0);
            }
        }
    }
}

// ------------------------------------------------------------ p6d_parity ---

#[cfg(test)]
mod p6d_parity_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn p6d_client(eid: i32, name: &str, x: f64, y: f64, z: f64) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.name = name.into();
        *client.pos.lock().unwrap() = (x, y, z, 0.0, 0.0, true);
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .unwrap();
        (Arc::new(client), receiver)
    }

    fn p6d_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn p6d_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    /// A level-`level` operator context for `name`: the ops registry is the
    /// real `player_permission_level` source, no test-only bypass.
    fn p6d_op_ctx(name: &str, level: u8) -> CmdCtx {
        if level > 0 {
            ops_registry()
                .lock()
                .unwrap()
                .set(name, level, false);
        }
        let (client, _recv) = p6d_client(97_000 + level as i32, name, 0.5, 16.0, 0.5);
        p6d_ctx(Some(client))
    }

    fn p6d_deop(name: &str) {
        ops_registry().lock().unwrap().remove(name);
    }

    fn p6d_frame_varint(r: &mut TcpStream) -> Option<(i64, usize)> {
        let mut value = 0i64;
        for i in 0..5 {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).ok()?;
            value |= ((b[0] & 0x7f) as i64) << (7 * i);
            if b[0] & 0x80 == 0 {
                return Some((value, i + 1));
            }
        }
        None
    }

    /// One uncompressed frame: varint length, varint packet id, payload.
    fn p6d_read_frame(r: &mut TcpStream) -> Option<(i64, Vec<u8>)> {
        let (len, _) = p6d_frame_varint(r)?;
        let (id, used) = p6d_frame_varint(r)?;
        let mut payload = vec![0u8; (len as usize).checked_sub(used)?];
        r.read_exact(&mut payload).ok()?;
        Some((id, payload))
    }

    fn p6d_seed_profiler() {
        let mut profiler = live_hotspot_profiler().lock().unwrap();
        profiler.reset();
        for us in [9_000u32, 11_000, 14_000, 10_000, 12_000] {
            profiler.record_tick_us(us);
        }
        profiler.record_chunk_work(DimId::Overworld, 12, -4, ProfilerSubsystem::Hoppers, 8_500);
        profiler.record_chunk_work(DimId::Overworld, 12, -4, ProfilerSubsystem::Hoppers, 5_700);
        profiler.record_chunk_work(DimId::Nether, 2, 7, ProfilerSubsystem::Mobs, 3_100);
    }

    // /profile: owner-level only, live profiler output, clean reset.
    #[test]
    fn p6d_parity_profile_permission_levels_refuse_below_four() {
        let _world = world_guard();
        let denied = "You do not have permission to run that command";
        for (name, level) in [
            ("p6d_l0", 0u8),
            ("p6d_l2", 2),
            ("p6d_l3", 3),
        ] {
            let ctx = p6d_op_ctx(name, level);
            assert_eq!(
                p6d_run(&ctx, "/profile"),
                denied,
                "level-{level} source must be refused /profile"
            );
            assert_eq!(
                p6d_run(&ctx, "/profile reset"),
                denied,
                "level-{level} source must be refused /profile reset"
            );
            p6d_deop(name);
        }
        // The advertised tree keeps the literals away from lower levels.
        for level in 0..4u8 {
            let root = native_command_dispatcher(level, None);
            assert!(
                root.root().child("profile").is_none(),
                "profile literal must not be advertised at level {level}"
            );
        }
        for level in 0..3u8 {
            let root = native_command_dispatcher(level, None);
            assert!(
                root.root().child("transfer").is_none(),
                "transfer literal must not be advertised at level {level}"
            );
        }
        let level3 = native_command_dispatcher(3, None);
        assert!(level3.root().child("transfer").is_some());
        assert!(level3.root().child("profile").is_none());
        let level4 = native_command_dispatcher(4, None);
        assert!(level4.root().child("profile").is_some());
        assert!(level4.root().child("transfer").is_some());
    }

    #[test]
    fn p6d_parity_profile_level_two_mcfunction_refused() {
        let _world = world_guard();
        p6d_seed_profiler();
        let ctx = p6d_op_ctx("p6d_fn2", 2);
        // A `.mcfunction` frame re-enters run_command_ctx with the same source;
        // run_function_lines is that path, not a test approximation.
        run_function_lines(
            &["profile reset".to_owned()],
            ctx.me.clone(),
            &mut None,
            0,
        );
        let report = live_hotspot_profiler().lock().unwrap().summary(5);
        assert_eq!(
            report.ticks_recorded, 5,
            "level-2 .mcfunction `profile reset` must not touch the profiler"
        );
        p6d_deop("p6d_fn2");
    }

    #[test]
    fn p6d_parity_profile_live_hotspots_and_reset() {
        let _world = world_guard();
        p6d_seed_profiler();
        let ctx = p6d_ctx(None); // console: level 4
        let report = p6d_run(&ctx, "/profile");
        assert!(report.contains("TPS "), "{report}");
        assert!(report.contains("MSPT mean"), "{report}");
        assert!(report.contains("Top chunk hotspots:"), "{report}");
        assert!(
            report.contains("Overworld chunk (12, -4) [x=200, z=-56]"),
            "{report}"
        );
        assert!(report.contains("hoppers"), "{report}");
        assert!(report.contains("Nether chunk (2, 7)"), "{report}");
        // top_n clamps at 1..=20: a bare `1` prints only the hottest chunk.
        let top1 = p6d_run(&ctx, "/profile 1");
        assert!(top1.contains("Overworld chunk (12, -4)"), "{top1}");
        assert!(!top1.contains("Nether chunk"), "{top1}");
        assert_eq!(
            p6d_run(&ctx, "/profile reset"),
            "Profiler counters reset"
        );
        let after = p6d_run(&ctx, "/profile");
        assert!(after.contains("No chunk hotspots recorded."), "{after}");
        assert!(after.contains("(0 ticks)"), "{after}");
        // Usage rejects unknown subcommands and extra arguments.
        assert_eq!(
            p6d_run(&ctx, "/profile bogus"),
            "usage: /profile [<top_n>|reset]"
        );
        assert_eq!(
            p6d_run(&ctx, "/profile 1 2"),
            "usage: /profile [<top_n>|reset]"
        );
        // An op-4 player runs it too.
        let admin = p6d_op_ctx("p6d_l4", 4);
        assert!(p6d_run(&admin, "/profile").contains("MSPT"), "op-4 refused");
        p6d_deop("p6d_l4");
    }

    // /transfer: protocol-767 clientbound 0x73, fail-closed validation, op >= 3.
    #[test]
    fn p6d_parity_transfer_packet_id_and_wire_bytes() {
        let _world = world_guard();
        // `proto/src/data_v767.rs` pins (Play, Clientbound, "transfer", 0x73).
        assert_eq!(TRANSFER_PACKET_ID, 0x73);
        let (player, mut recv) = p6d_client(97_100, "p6d_xfer", 0.5, 16.0, 0.5);
        clients().lock().unwrap().push(player.clone());
        let ctx = p6d_ctx(None);
        assert_eq!(
            p6d_run(&ctx, "/transfer example.org 25565 p6d_xfer"),
            "Transferred 1 player(s) to example.org:25565"
        );
        let (id, payload) = p6d_read_frame(&mut recv).expect("transfer frame");
        assert_eq!(id, 0x73, "protocol-767 transfer packet id");
        // String("example.org") + VarInt(25565): [0x0b]["example.org"][0xdd 0xc7 0x01].
        let mut expected = vec![11u8];
        expected.extend_from_slice(b"example.org");
        expected.extend_from_slice(&[0xdd, 0xc7, 0x01]);
        assert_eq!(payload, expected, "transfer wire bytes");
        // Default port 25565 when omitted, executor = default target.
        let (admin, mut recv2) = p6d_client(97_101, "p6d_xfer4", 0.5, 16.0, 0.5);
        ops_registry().lock().unwrap().set("p6d_xfer4", 4, false);
        clients().lock().unwrap().push(admin.clone());
        let admin_ctx = p6d_ctx(Some(admin));
        assert_eq!(
            p6d_run(&admin_ctx, "/transfer localhost"),
            "Transferred 1 player(s) to localhost:25565"
        );
        let (id, payload) = p6d_read_frame(&mut recv2).expect("default-port frame");
        assert_eq!(id, 0x73);
        let mut expected = vec![9u8];
        expected.extend_from_slice(b"localhost");
        expected.extend_from_slice(&[0xdd, 0xc7, 0x01]);
        assert_eq!(payload, expected);
        clients()
            .lock()
            .unwrap()
            .retain(|c| !matches!(c.eid, 97_100 | 97_101));
        ops_registry().lock().unwrap().remove("p6d_xfer4");
    }

    #[test]
    fn p6d_parity_transfer_fail_closed_and_permission() {
        let _world = world_guard();
        let (player, mut recv) = p6d_client(97_110, "p6d_xfer3", 0.5, 16.0, 0.5);
        ops_registry()
            .lock()
            .unwrap()
            .set("p6d_xfer3", 3, false);
        clients().lock().unwrap().push(player.clone());
        let ctx3 = p6d_ctx(Some(player.clone()));
        // Level 3 is exactly enough — vanilla hasPermission(3).
        assert!(p6d_run(&ctx3, "/transfer ok.example 7777")
            .starts_with("Transferred 1 player(s)"));
        let _ = p6d_read_frame(&mut recv).expect("level-3 transfer");
        // Fail-closed validation: no packet leaves for any of these.
        assert!(
            p6d_run(&ctx3, "/transfer \"bad host\" 7777").starts_with("Invalid hostname"),
            "quoted hostname containing a space must fail closed"
        );
        assert_eq!(p6d_run(&ctx3, "/transfer host 0"), "Invalid port '0'");
        assert_eq!(p6d_run(&ctx3, "/transfer host 65536"), "Invalid port '65536'");
        assert_eq!(p6d_run(&ctx3, "/transfer host -1"), "Invalid port '-1'");
        assert_eq!(p6d_run(&ctx3, "/transfer host abc"), "Invalid port 'abc'");
        assert_eq!(
            p6d_run(&ctx3, &format!("/transfer {}", "x".repeat(256))),
            format!("Invalid hostname '{}'", "x".repeat(256))
        );
        assert_eq!(
            p6d_run(&ctx3, "/transfer"),
            "usage: /transfer <hostname> [<port>] [<players>]"
        );
        // Nothing reached the wire for the refused commands above.
        let mut sink = [0u8; 1];
        assert!(recv.read_exact(&mut sink).is_err(), "refused transfers sent a packet");
        // A level-2 operator (and therefore a level-2 function frame) is refused.
        let (weak, _r) = p6d_client(97_111, "p6d_xfer2", 0.5, 16.0, 0.5);
        ops_registry()
            .lock()
            .unwrap()
            .set("p6d_xfer2", 2, false);
        let ctx2 = p6d_ctx(Some(weak));
        assert_eq!(
            p6d_run(&ctx2, "/transfer host"),
            "You do not have permission to run that command"
        );
        run_function_lines(
            &["transfer host 7777 p6d_xfer3".to_owned()],
            ctx2.me.clone(),
            &mut None,
            0,
        );
        assert!(recv.read_exact(&mut sink).is_err(), "level-2 function sent a packet");
        clients()
            .lock()
            .unwrap()
            .retain(|c| !matches!(c.eid, 97_110 | 97_111));
        ops_registry().lock().unwrap().remove("p6d_xfer3");
        ops_registry().lock().unwrap().remove("p6d_xfer2");
    }

    // /data: storage, entity, block roundtrips through get/merge/modify/remove.
    #[test]
    fn p6d_parity_data_storage_roundtrip() {
        let _world = world_guard();
        let ctx = p6d_ctx(None);
        let store = "minecraft:p6d_db";
        assert_eq!(
            p6d_run(&ctx, &format!("/data merge storage {store} {{a:1b,list:[1,2],nested:{{x:5}}}}")),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} a")),
            format!("Storage '{store}' has the following contents: 1b")
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} nested.x")),
            format!("Storage '{store}' has the following contents: 5")
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} list")),
            format!("Storage '{store}' has the following contents: [1,2]")
        );
        // merge merges deep, not whole-compound replace.
        assert_eq!(
            p6d_run(&ctx, &format!("/data merge storage {store} {{nested:{{y:7}}}}")),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} nested")),
            format!("Storage '{store}' has the following contents: {{\"x\":5,\"y\":7}}")
        );
        // modify set / append / insert / merge.
        assert_eq!(
            p6d_run(&ctx, &format!("/data modify storage {store} a set value 9b")),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} a")),
            format!("Storage '{store}' has the following contents: 9b")
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data modify storage {store} list append value 3")),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data modify storage {store} list insert 0 value 0")),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} list")),
            format!("Storage '{store}' has the following contents: [0,1,2,3]")
        );
        // from-source modify copies a live value.
        assert_eq!(
            p6d_run(
                &ctx,
                &format!("/data modify storage {store} copied set from storage {store} nested.x")
            ),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} copied")),
            format!("Storage '{store}' has the following contents: 5")
        );
        // remove prunes nested paths and list indices.
        assert_eq!(
            p6d_run(&ctx, &format!("/data remove storage {store} nested.x")),
            "Removed data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} nested")),
            format!("Storage '{store}' has the following contents: {{\"y\":7}}")
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} missing")),
            format!("Found no elements at 'missing'")
        );
        execute_storage().lock().unwrap().remove(store);
    }

    #[test]
    fn p6d_parity_data_entity_roundtrip() {
        let _world = world_guard();
        let (player, _recv) = p6d_client(97_200, "p6d_data", 1.5, 64.0, -2.5);
        clients().lock().unwrap().push(player.clone());
        let ctx = p6d_ctx(None);
        // Live reads: Pos/Health/Motion/Air/Tags out of the real player state.
        let pos = p6d_run(&ctx, "/data get entity p6d_data Pos");
        assert!(pos.contains("1.5d"), "{pos}");
        assert!(pos.contains("64d"), "{pos}");
        assert!(pos.contains("-2.5d"), "{pos}");
        assert_eq!(
            p6d_run(&ctx, "/data get entity p6d_data Health"),
            "p6d_data has the following entity data: 20f"
        );
        assert_eq!(
            p6d_run(&ctx, "/data get entity p6d_data Air"),
            format!("p6d_data has the following entity data: {}s", *player.air.lock().unwrap())
        );
        assert_eq!(
            p6d_run(&ctx, "/data get entity p6d_data Motion"),
            "p6d_data has the following entity data: [0d,0d,0d]"
        );
        // merge writes reach the live model: Health lands in the hp mutex.
        assert_eq!(
            p6d_run(&ctx, "/data merge entity p6d_data {Health:5.0f,Tags:[\"marker\"],NoAI:1b}"),
            "Modified data"
        );
        assert_eq!(*player.hp.lock().unwrap(), 5.0);
        assert!(tags()
            .lock()
            .unwrap()
            .get("p:p6d_data")
            .is_some_and(|set| set.contains("marker")));
        assert_eq!(
            p6d_run(&ctx, "/data get entity p6d_data NoAI"),
            "p6d_data has the following entity data: 1b"
        );
        // modify append on Tags, then remove Health restores the load default.
        assert_eq!(
            p6d_run(
                &ctx,
                "/data modify entity p6d_data Tags append value \"second\""
            ),
            "Modified data"
        );
        let tags_out = p6d_run(&ctx, "/data get entity p6d_data Tags");
        assert!(tags_out.contains("\"marker\"") && tags_out.contains("\"second\""), "{tags_out}");
        assert_eq!(
            p6d_run(&ctx, "/data remove entity p6d_data Health"),
            "Removed data"
        );
        assert_eq!(*player.hp.lock().unwrap(), 20.0);
        // SelectedItem/Inventory write through to the live inventory map.
        let stone = name_to_item("stone").unwrap();
        assert_eq!(
            p6d_run(
                &ctx,
                "/data merge entity p6d_data {SelectedItem:{id:\"minecraft:stone\",count:2}}"
            ),
            "Modified data"
        );
        let held_slot = 36 + *player.held.lock().unwrap();
        assert_eq!(
            player.inv.lock().unwrap().get(&held_slot).map(|s| (s.0, s.1)),
            Some((stone, 2))
        );
        assert_eq!(
            p6d_run(&ctx, "/data get entity p6d_data SelectedItem.id"),
            "p6d_data has the following entity data: \"minecraft:stone\""
        );
        // Rotation/Pos land in the live position tuple.
        assert_eq!(
            p6d_run(&ctx, "/data merge entity p6d_data {Rotation:[90.0f,45.0f]}"),
            "Modified data"
        );
        let pos = *player.pos.lock().unwrap();
        assert_eq!((pos.3, pos.4), (90.0, 45.0));
        clients().lock().unwrap().retain(|c| c.eid != 97_200);
        tags().lock().unwrap().remove("p:p6d_data");
        data_entity_extras().lock().unwrap().remove("p:p6d_data");
    }

    #[test]
    fn p6d_parity_data_block_roundtrip() {
        let _world = world_guard();
        let ctx = p6d_ctx(None);
        let key = ContainerKey::Block((DimId::Overworld, 40, 90, 40));
        let stone = name_to_item("stone").unwrap();
        chests()
            .lock()
            .unwrap()
            .insert(key, HashMap::from([(0i16, (stone, 3u8, Vec::new()))]));
        let got = p6d_run(&ctx, "/data get block 40 90 40 Items");
        assert!(got.starts_with("Storage has the following contents"), "{got}");
        assert!(got.contains("\"minecraft:stone\""), "{got}");
        assert!(got.contains("\"count\":3"), "{got}");
        // merge lands in the live container store — a list field replaces
        // whole (vanilla `Items` is a flat list, not slot-keyed).
        assert_eq!(
            p6d_run(
                &ctx,
                "/data merge block 40 90 40 {Items:[{Slot:2b,id:\"minecraft:diamond\",count:1}]}"
            ),
            "Modified data"
        );
        {
            let live = chests().lock().unwrap();
            let slots = live.get(&key).unwrap();
            assert_eq!(slots.get(&2).map(|s| s.1), Some(1));
            assert_eq!(slots.len(), 1);
        }
        // modify set rewrites a single nested value in place.
        assert_eq!(
            p6d_run(&ctx, "/data modify block 40 90 40 Items[0].count set value 7"),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, "/data get block 40 90 40 Items[0].count"),
            "Storage has the following contents: 7"
        );
        assert_eq!(
            chests().lock().unwrap().get(&key).and_then(|s| s.get(&2)).map(|s| s.1),
            Some(7)
        );
        // remove Items empties the live container.
        assert_eq!(
            p6d_run(&ctx, "/data remove block 40 90 40 Items"),
            "Removed data"
        );
        assert!(chests().lock().unwrap().get(&key).is_none()
            || chests().lock().unwrap().get(&key).is_some_and(|s| s.is_empty()));
        // A block position with no block entity fails closed.
        assert_eq!(
            p6d_run(&ctx, "/data get block 60 90 60"),
            "The block at 60, 90, 60 is not a block entity"
        );
        chests().lock().unwrap().remove(&key);
        data_block_overrides()
            .lock()
            .unwrap()
            .remove(&(DimId::Overworld, 40, 90, 40));
        data_block_overrides()
            .lock()
            .unwrap()
            .remove(&(DimId::Overworld, 60, 90, 60));
    }

    // /advancement query: exact granted/total counts against the live tracker.
    #[test]
    fn p6d_parity_advancement_query_exact_counts() {
        let _world = world_guard();
        let (player, _recv) = p6d_client(97_300, "p6d_adv", 0.5, 16.0, 0.5);
        clients().lock().unwrap().push(player.clone());
        let ctx = p6d_ctx(None);
        // 19 live definitions in CORE_ADVANCEMENTS; nothing granted yet.
        assert_eq!(
            p6d_run(&ctx, "/advancement query p6d_adv everything"),
            "p6d_adv has completed 0 of 19 advancements"
        );
        // only: the single-id scope reports the id itself.
        assert_eq!(
            p6d_run(&ctx, "/advancement query p6d_adv only minecraft:story/root"),
            "p6d_adv has completed 0 of 1 criteria for minecraft:story/root"
        );
        // Grant the root, then the mine_diamond chain (root, mine_stone,
        // upgrade_tools, smelt_iron, iron_tools, mine_diamond = 6).
        let grant = p6d_run(&ctx, "/advancement grant p6d_adv until minecraft:story/mine_diamond");
        assert!(grant.contains("6 advancement"), "{grant}");
        assert_eq!(
            p6d_run(&ctx, "/advancement query p6d_adv everything"),
            "p6d_adv has completed 6 of 19 advancements"
        );
        // until smelt_iron: smelt_iron + upgrade_tools + mine_stone + root = 4.
        assert_eq!(
            p6d_run(&ctx, "/advancement query p6d_adv until minecraft:story/smelt_iron"),
            "p6d_adv has completed 4 of 4 advancements"
        );
        // from mine_diamond: mine_diamond + shiny_gear + enchant_item = 3, of
        // which only mine_diamond itself is granted.
        assert_eq!(
            p6d_run(&ctx, "/advancement query p6d_adv from minecraft:story/mine_diamond"),
            "p6d_adv has completed 1 of 3 advancements"
        );
        // through smelt_iron covers the whole story tree (16): until-side 4
        // plus descendants, with the 6 granted so far.
        assert_eq!(
            p6d_run(&ctx, "/advancement query p6d_adv through minecraft:story/smelt_iron"),
            "p6d_adv has completed 6 of 16 advancements"
        );
        // only on an incomplete advancement reports criteria progress.
        let only = p6d_run(&ctx, "/advancement query p6d_adv only minecraft:story/obtain_armor");
        assert!(
            only.contains("obtain_armor"),
            "{only}"
        );
        clients().lock().unwrap().retain(|c| c.eid != 97_300);
        let mut tracker = player_advancements().lock().unwrap();
        tracker.progress.remove("p6d_adv");
        tracker.sent_progress.remove("p6d_adv");
        adv_done().lock().unwrap().remove("p6d_adv");
    }
}
