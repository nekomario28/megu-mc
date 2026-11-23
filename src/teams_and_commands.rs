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
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let body = self.data.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(body)
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
        if !(0..=(1 << 20)).contains(&len) {
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
    chunk: (u64, i32, i32),
    all_mineshaft: bool,
    entry_biomes: &mut HashMap<String, HashSet<String>>,
) -> Option<String> {
    let (seed, cx, cz) = chunk;
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
    search: (i32, i32, i32),
    entry_biomes: &mut HashMap<String, HashSet<String>>,
) -> Option<(i32, i32, String)> {
    let (origin_cx, origin_cz, max_radius_chunks) = search;
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
                            (seed, cand.0, cand.1),
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
                    (seed, cx, cz),
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
            .and_then(|j| j.get("values").cloned());
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
                (origin_cx, origin_cz, max_radius_chunks),
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
                            g, leaf, (x, y, z), (x + dx, z + dz), qy0, qy1, &mut best,
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
    origin: (i32, i32, i32),
    column: (i32, i32),
    qy0: i32,
    qy1: i32,
    best: &mut Option<(i32, i32, i32, f64)>,
) {
    let (ox, oy, oz) = origin;
    let (x, z) = column;
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
    let w = world().read_all();
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
            let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
            match registry.lock().unwrap_or_else(|e| e.into_inner()).remove(name) {
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
            match registry.lock().unwrap_or_else(|e| e.into_inner()).empty(name) {
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
                let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
                    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
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
            let c = ctl.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut c = ctl.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = c.set_tick_rate(value) {
                    return e;
                }
            }
            let c = ctl.lock().unwrap_or_else(|e| e.into_inner());
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x71, ticking_state_packet(c.tick_rate, c.frozen))
            });
            format!("Set tick rate to {value}")
        }
        Some("freeze") => {
            ctl.lock().unwrap_or_else(|e| e.into_inner()).frozen = true;
            let c = ctl.lock().unwrap_or_else(|e| e.into_inner());
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x71, ticking_state_packet(c.tick_rate, c.frozen))
            });
            "Froze the game".to_owned()
        }
        Some("unfreeze") => {
            ctl.lock().unwrap_or_else(|e| e.into_inner()).frozen = false;
            let c = ctl.lock().unwrap_or_else(|e| e.into_inner());
            fanout_in(&client_snapshot(), |_| true, |_| {
                (0x71, ticking_state_packet(c.tick_rate, c.frozen))
            });
            "The game is no longer frozen".to_owned()
        }
        Some("step") => {
            if args.get(1) == Some(&"stop") {
                ctl.lock().unwrap_or_else(|e| e.into_inner()).step_remaining = 0;
                return "Stopped stepping".to_owned();
            }
            let ticks = match args.get(1) {
                None => 1,
                Some(v) => match parse_tick_time_arg(v) {
                    Some(t) => t,
                    None => return "Invalid step count (use ticks like 1, or time like 5s, 1d)".to_owned(),
                },
            };
            let mut c = ctl.lock().unwrap_or_else(|e| e.into_inner());
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
                ctl.lock().unwrap_or_else(|e| e.into_inner()).stop_sprinting();
                return "Stopped sprinting".to_owned();
            }
            let Some(v) = args.get(1) else {
                return "usage: /tick sprint <ticks|time> | /tick sprint stop".to_owned();
            };
            let Some(ticks) = parse_tick_time_arg(v) else {
                return "Invalid sprint length (use ticks like 1, or time like 5s, 1d)".to_owned();
            };
            ctl.lock().unwrap_or_else(|e| e.into_inner()).request_sprint(ticks);
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
        &mut random_sequences().lock().unwrap_or_else(|e| e.into_inner()),
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
        let mut live = mounts().lock().unwrap_or_else(|e| e.into_inner());
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
            let mut p = player.pos.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut p = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
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
    let packet = mounts().lock().unwrap_or_else(|e| e.into_inner()).registry.encode_set_passengers(v);
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
    let Some(at) = mounts().lock().unwrap_or_else(|e| e.into_inner()).registry.dismount(target.id()) else {
        return "Not riding anything".into();
    };
    if let ExecuteEntity::Mob(mob) = target {
        let mut p = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
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
    spectator_cameras().lock().unwrap_or_else(|e| e.into_inner()).insert(viewer.eid, camera);
    let mut body = Vec::new();
    write_varint(&mut body, i64::from(camera));
    let _ = send_packet(
        &mut viewer.sock.lock().unwrap_or_else(|e| e.into_inner()),
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
    let mut overrides = biome_overrides().lock().unwrap_or_else(|e| e.into_inner());
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
            place_dispatches().lock().unwrap_or_else(|e| e.into_inner()).push(PlaceDispatch {
                dim,
                pos,
                id: id.clone(),
                kind: PlaceKind::Feature,
            });
            format!("Placed feature '{id}' at [{}, {}, {}]", pos.0, pos.1, pos.2)
        }
        _ => match place_template_live(dim, pos, &id) {
            Ok(placed) => {
                place_dispatches().lock().unwrap_or_else(|e| e.into_inner()).push(PlaceDispatch {
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
        [] => profiler.lock().unwrap_or_else(|e| e.into_inner()).summary(5).format_human(),
        ["reset"] => {
            profiler.lock().unwrap_or_else(|e| e.into_inner()).reset();
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
            &mut target.sock.lock().unwrap_or_else(|e| e.into_inner()),
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
    let extras = data_entity_extras().lock().unwrap_or_else(|e| e.into_inner()).clone();
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
        ExecuteEntity::Mob(mob) => *mob.vel.lock().unwrap_or_else(|e| e.into_inner()),
        _ => [0.0; 3],
    };
    put("Motion", doubles(motion));
    match entity {
        ExecuteEntity::Player(player) => {
            let air = *player.air.lock().unwrap_or_else(|e| e.into_inner());
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
            let inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
            let held = *player.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(selected) = data_item_nbt(inv.get(&(36 + held))) {
                put("SelectedItem", selected);
            }
            put("ArmorItems", data_armor_items(&inv));
            put("HandItems", data_hand_items(&inv, held));
        }
        ExecuteEntity::Mob(mob) => {
            put(
                "Air",
                ExecTag::Short((*mob.air.lock().unwrap_or_else(|e| e.into_inner())).clamp(-300, 30_000) as i16),
            );
            put(
                "Fire",
                ExecTag::Short((*mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner())).clamp(-20, 30_000) as i16),
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
    entity_tags.sort_by_key(execute_tag_snbt);
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
fn data_item_nbt(stack: Option<&Stack>) -> Option<ExecTag> {
    let mut tag = stack_to_nbt(0, stack?)?;
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
                data_item_nbt(inv.get(slot))
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
                data_item_nbt(inv.get(slot))
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
    let (x, y, z) = *actor.pos.lock().unwrap_or_else(|e| e.into_inner());
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
            stacks_to_items_nbt(&actor.inv.lock().unwrap_or_else(|e| e.into_inner())),
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
            Ok((format!("{x}, {y}, {z}"), root.clone()))
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
        let mut extras = data_entity_extras().lock().unwrap_or_else(|e| e.into_inner());
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
                        let mut pos = player.pos.lock().unwrap_or_else(|e| e.into_inner());
                        (pos.0, pos.1, pos.2) = (x, y, z);
                    }
                    ExecuteEntity::Mob(mob) => {
                        let mut pos = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
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
                            let mut pos = player.pos.lock().unwrap_or_else(|e| e.into_inner());
                            (pos.3, pos.4) = (*yaw, *pitch);
                        }
                        ExecuteEntity::Mob(mob) => {
                            let mut pos = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
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
                    ExecuteEntity::Mob(mob) => *mob.vel.lock().unwrap_or_else(|e| e.into_inner()) = motion,
                    _ => extra(entity, name, value),
                }
            }
        }
        "Health" => {
            if let Some(hp) = data_nbt_f64(value) {
                match entity {
                    ExecuteEntity::Player(player) => {
                        *player.hp.lock().unwrap_or_else(|e| e.into_inner()) = hp as f32;
                    }
                    ExecuteEntity::Mob(mob) => *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = hp as f32,
                    ExecuteEntity::Other { .. } => {}
                }
            }
        }
        "Air" => {
            if let Some(ticks) = data_nbt_f64(value).map(|n| n as i32) {
                match entity {
                    ExecuteEntity::Player(player) => *player.air.lock().unwrap_or_else(|e| e.into_inner()) = ticks,
                    ExecuteEntity::Mob(mob) => *mob.air.lock().unwrap_or_else(|e| e.into_inner()) = ticks,
                    ExecuteEntity::Other { .. } => {}
                }
            }
        }
        "Fire" => {
            if let Some(ticks) = data_nbt_f64(value).map(|n| n as i32) {
                match entity {
                    ExecuteEntity::Mob(mob) => *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) = ticks,
                    _ => extra(entity, name, value),
                }
            }
        }
        "Invulnerable" => {
            if let ExecuteEntity::Player(player) = entity {
                let flag = data_nbt_f64(value).is_some_and(|n| n != 0.0);
                *player.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = if flag { i32::MAX } else { 0 };
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
                let mut store = tags().lock().unwrap_or_else(|e| e.into_inner());
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
                            *player.inv.lock().unwrap_or_else(|e| e.into_inner()) = slots;
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
                        let slot = 36 + *player.held.lock().unwrap_or_else(|e| e.into_inner());
                        let mut inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
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
                        let mut inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
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
                        let held = *player.held.lock().unwrap_or_else(|e| e.into_inner());
                        let mut inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
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
        if let Some(ExecTag::Compound(fields)) = data_entity_extras()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&entity.tag_key())
        {
            fields.remove(&NbtString::text(name));
        }
    };
    match name {
        "id" | "x" | "y" | "z" | "UUID" | "Pos" | "Rotation" | "Motion" | "Item" => {}
        "Health" => match entity {
            ExecuteEntity::Player(player) => *player.hp.lock().unwrap_or_else(|e| e.into_inner()) = 20.0,
            ExecuteEntity::Mob(mob) => {
                *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = entity_max_health_1211(mob.etype);
            }
            ExecuteEntity::Other { .. } => {}
        },
        "Air" => match entity {
            ExecuteEntity::Player(player) => *player.air.lock().unwrap_or_else(|e| e.into_inner()) = 300,
            ExecuteEntity::Mob(mob) => *mob.air.lock().unwrap_or_else(|e| e.into_inner()) = 300,
            ExecuteEntity::Other { .. } => {}
        },
        "Fire" => match entity {
            ExecuteEntity::Mob(mob) => *mob.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()) = -20,
            _ => drop_extra(entity, name),
        },
        "Invulnerable" => {
            if let ExecuteEntity::Player(player) = entity {
                *player.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            }
            drop_extra(entity, name);
        }
        "Tags" => {
            tags().lock().unwrap_or_else(|e| e.into_inner()).remove(&entity.tag_key());
        }
        "Inventory" => {
            if let ExecuteEntity::Player(player) = entity {
                player.inv.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
            drop_extra(entity, name);
        }
        "SelectedItem" => {
            if let ExecuteEntity::Player(player) = entity {
                let slot = 36 + *player.held.lock().unwrap_or_else(|e| e.into_inner());
                player.inv.lock().unwrap_or_else(|e| e.into_inner()).remove(&slot);
            }
            drop_extra(entity, name);
        }
        "ArmorItems" => {
            if let ExecuteEntity::Player(player) = entity {
                let mut inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
                for slot in [8, 7, 6, 5] {
                    inv.remove(&slot);
                }
            }
            drop_extra(entity, name);
        }
        "HandItems" => {
            if let ExecuteEntity::Player(player) = entity {
                let held = *player.held.lock().unwrap_or_else(|e| e.into_inner());
                let mut inv = player.inv.lock().unwrap_or_else(|e| e.into_inner());
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
    if kind == "minecraft:item" {
        if let Some(drop) = drops().lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|d| d.eid == id) {
            (drop.x, drop.y, drop.z) = (x, y, z);
        }
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
                {
                    let mut removed = false;
                    for entry in entries.iter_mut().filter(|entry| is_match(entry)) {
                        removed |= data_nbt_remove(entry, rest);
                    }
                    removed
                }
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
        DataTarget::Block(..) => format!("{label} has the following block data"),
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
                    if let Some(f) = furnaces().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&key) {
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
            chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&ContainerKey::Block(key));
            // A removed `Items` empties a live furnace only when it held
            // anything; `furnace_slots` reports the non-empty slots.
            if furnace && !furnace_slots((x, y, z)).is_empty() {
                if let Some(f) = furnaces().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&key) {
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
                [] => (vec![root], rest),
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
                ["set", rest @ ..] => (DataModifyOp::Set, rest),
                ["merge", rest @ ..] => (DataModifyOp::Merge, rest),
                ["append", rest @ ..] => (DataModifyOp::Append, rest),
                ["prepend", rest @ ..] => (DataModifyOp::Prepend, rest),
                ["insert", index, rest @ ..] => match index.parse::<i32>() {
                    Ok(index) => (DataModifyOp::Insert(index), rest),
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
        if advancements().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
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
            for id in advancements().lock().unwrap_or_else(|e| e.into_inner()).keys() {
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

// ---------------------------------------------------------------------------
// Lane P8c: `/debug`, `/jfr`, `/perf`, `/publish` (all owner-level 4) plus the
// remaining #225 player/entity subcommands — `/effect give` infinite/hide,
// `/damage` type/at/by/from, `/tag` add|remove|list, `/attribute` get and
// modifier verbs, `/clear` maxCount=0. The level gate in `server_ops` already
// maps the four new commands to 4; each handler still re-checks `level` the
// way `/profile` does so a level-2 `.mcfunction` frame can never reach them.
// ---------------------------------------------------------------------------

const P8C_DENIED: &str = "You do not have permission to run that command";

/// `/debug`'s profiling session: the `Instant` measures the wall-clock leg of
/// the Vanilla `commands.debug.stopped` report and `tick_start` bounds the
/// game-tick count to the session window.
struct DebugSession {
    started: Instant,
    tick_start: u64,
}

fn debug_session() -> &'static Mutex<Option<DebugSession>> {
    static SESSION: OnceLock<Mutex<Option<DebugSession>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

/// `true` while `/debug function` is tracing — Vanilla refuses a nested trace
/// (`commands.debug.function.noRecursion`), including a `debug function` line
/// inside the traced function itself.
static DEBUG_TRACING: AtomicBool = AtomicBool::new(false);

/// `/jfr`'s native telemetry window: the `Instant` records the session start;
/// the bounded ring inside `live_hotspot_profiler` is reset on `start` so the
/// stop summary covers exactly this session's recorded ticks.
fn jfr_session() -> &'static Mutex<Option<Instant>> {
    static SESSION: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

/// `/perf`'s bounded capture session — same story as `/jfr`.
fn perf_session() -> &'static Mutex<Option<Instant>> {
    static SESSION: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

/// The live "Open to LAN" metadata `/publish` mutates. No socket is bound:
/// the record mirrors what Vanilla's `publishServer` would publish so the
/// command stays metadata-only until a listener lane owns the bind.
#[allow(dead_code)]
#[derive(Clone, Copy)]
struct PublishState {
    port: u16,
    allow_commands: bool,
    /// `DEFAULT_GAME_MODE` value: 0 survival, 1 creative, 2 adventure,
    /// 3 spectator.
    gamemode: u8,
}

fn publish_state() -> &'static Mutex<Option<PublishState>> {
    static STATE: OnceLock<Mutex<Option<PublishState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

/// The port `/publish` advertises when the argument is omitted: the configured
/// game port (`MEGU_PORT`, default 25599) like the main listener.
fn publish_default_port() -> u16 {
    std::env::var("MEGU_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p >= 1024)
        .unwrap_or(25599)
}

/// `/debug start|stop|function <name>` — Vanilla `DebugCommand` on the live
/// hotspot ring. `start` zeroes the window so `stop`'s summary is exactly the
/// session; `function` replays the function line-walk counting per-line
/// results like Vanilla's trace.
fn run_debug_command(
    ctx: &CmdCtx,
    level: u8,
    args: &[&str],
    s: &mut Option<&Client>,
) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    match args {
        ["start"] => {
            let mut session = debug_session().lock().unwrap_or_else(|e| e.into_inner());
            if session.is_some() {
                return "Debug profiling has already started".into();
            }
            live_hotspot_profiler()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            *session = Some(DebugSession {
                started: Instant::now(),
                tick_start: TICK.load(Ordering::Relaxed),
            });
            "Started debug profiling".into()
        }
        ["stop"] => {
            let Some(session) = debug_session().lock().unwrap_or_else(|e| e.into_inner()).take()
            else {
                return "Can't stop profiling when we haven't started yet!".into();
            };
            let secs = session.started.elapsed().as_secs_f64();
            let ticks = TICK
                .load(Ordering::Relaxed)
                .saturating_sub(session.tick_start);
            let tps = if secs > 0.0 {
                ticks as f64 / secs
            } else {
                0.0
            };
            let report = live_hotspot_profiler()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .summary(5);
            format!(
                "Stopped debug profiling after {secs:.2} seconds and {ticks} ticks \
                 ({tps:.2} ticks per second); MSPT mean {:.1} ms, p50 {:.1}, \
                 p95 {:.1}, p99 {:.1} ({} ticks sampled)",
                report.mspt_mean,
                report.mspt_p50,
                report.mspt_p95,
                report.mspt_p99,
                report.ticks_recorded
            )
        }
        ["function", name] => {
            if DEBUG_TRACING.swap(true, Ordering::Relaxed) {
                return "Can't trace from inside a function".into();
            }
            let result = run_function_traced(name, ctx, s);
            DEBUG_TRACING.store(false, Ordering::Relaxed);
            match result {
                Some((executed, succeeded)) => format!(
                    "Finished tracing {executed} command(s) from function '{name}', \
                     {succeeded} succeeded and {} failed",
                    executed - succeeded
                ),
                None => "Failed to trace function".into(),
            }
        }
        _ => "usage: /debug start|stop|function <name>".into(),
    }
}

/// `/debug function`'s trace entry: `lookup_function` plus the same
/// macro-instantiation guard `run_function_depth` applies (a `$`-bearing
/// function called without `with` arguments fails the whole call).
fn run_function_traced(
    name: &str,
    ctx: &CmdCtx,
    s: &mut Option<&Client>,
) -> Option<(usize, usize)> {
    let lines = lookup_function(name)?;
    if lines.iter().any(|line| line.starts_with('$')) {
        return None;
    }
    Some(trace_function_lines(&lines, ctx, s, 0))
}

/// The same line walk `run_function_lines` performs — `function` lines nest
/// through `run_function_depth`, `return` ends the frame — except every
/// command's reply is captured so the trace can split succeeded from failed.
/// Returns `(executed, succeeded)`; nested calls count only executed commands
/// (their inner replies are not individually traced, same as Vanilla's
/// per-line trace attribution).
fn trace_function_lines(
    lines: &[String],
    ctx: &CmdCtx,
    s: &mut Option<&Client>,
    depth: usize,
) -> (usize, usize) {
    with_command_chain_budget(|| {
        let mut executed = 0usize;
        let mut succeeded = 0usize;
        for line in lines {
            if !command_chain_consume() {
                break;
            }
            if let Some(rest) = line.strip_prefix("function ") {
                let (inner, compound) = match rest.split_once(" with ") {
                    Some((name, compound)) => (name.trim(), Some(compound.trim())),
                    None => (rest.trim(), None),
                };
                let parsed = match compound {
                    Some(compound) => match function_macro_args(&["with", compound]) {
                        Ok(parsed) => parsed,
                        Err(_) => continue,
                    },
                    None => None,
                };
                let n =
                    run_function_depth(inner, ctx.me.clone(), s, depth + 1, parsed.as_ref());
                executed += n;
                succeeded += n;
            } else {
                let reply = run_command_ctx(ctx, &format!("/{line}"), s);
                executed += 1;
                if !command_reply_is_error(&reply) {
                    succeeded += 1;
                }
                if *line == "return" || line.starts_with("return ") {
                    break;
                }
            }
        }
        (executed, succeeded)
    })
}

/// `/jfr start|stop` — the native telemetry recording Vanilla's JFR command
/// wraps; backed by the same bounded tick ring as `/profile`.
fn run_jfr_command(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    match args {
        ["start"] => {
            let mut session = jfr_session().lock().unwrap_or_else(|e| e.into_inner());
            if session.is_some() {
                return "Native telemetry profiling is already running".into();
            }
            live_hotspot_profiler()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            *session = Some(Instant::now());
            "Started native telemetry profiling (Rust JFR equivalent)".into()
        }
        ["stop"] => {
            let Some(started) = jfr_session().lock().unwrap_or_else(|e| e.into_inner()).take()
            else {
                return "Native telemetry profiling is not running".into();
            };
            let report = live_hotspot_profiler()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .summary(5);
            format!(
                "Stopped native telemetry profiling after {:.2} seconds \
                 ({} ticks; MSPT p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms)",
                started.elapsed().as_secs_f64(),
                report.ticks_recorded,
                report.mspt_p50,
                report.mspt_p95,
                report.mspt_p99
            )
        }
        _ => "usage: /jfr start|stop".into(),
    }
}

/// `/perf start|stop` — a bounded capture over the same ring: `start` resets
/// the window, `stop` prints the `format_human` report (TPS, MSPT
/// mean/p50/p95/p99/max, top chunk hotspots).
fn run_perf_command(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    match args {
        ["start"] => {
            let mut session = perf_session().lock().unwrap_or_else(|e| e.into_inner());
            if session.is_some() {
                return "Performance capture is already running".into();
            }
            live_hotspot_profiler()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset();
            *session = Some(Instant::now());
            "Started performance capture".into()
        }
        ["stop"] => {
            let Some(started) = perf_session().lock().unwrap_or_else(|e| e.into_inner()).take()
            else {
                return "No performance capture is running".into();
            };
            let report = live_hotspot_profiler()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .summary(5)
                .format_human();
            format!(
                "Stopped performance capture after {:.2} seconds\n{}",
                started.elapsed().as_secs_f64(),
                report
            )
        }
        _ => "usage: /perf start|stop".into(),
    }
}

/// `/publish [<allowCommands>] [<gamemode>] [<port>]` — Vanilla 1.21.1
/// `PublishCommand` reduced to its metadata half: arguments are validated the
/// same way and the publish state is recorded, but no socket is bound.
fn run_publish_command(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    if args.len() > 3 {
        return "usage: /publish [<allowCommands>] [<gamemode>] [<port>]".into();
    }
    let allow_commands = match args.first() {
        None => false,
        Some(&"true") => true,
        Some(&"false") => false,
        Some(raw) => return format!("Invalid allowCommands '{raw}'"),
    };
    let gamemode = match args.get(1) {
        None => None,
        Some(&"survival") => Some(0u8),
        Some(&"creative") => Some(1u8),
        Some(&"adventure") => Some(2u8),
        Some(&"spectator") => Some(3u8),
        Some(raw) => return format!("Invalid gamemode '{raw}'"),
    };
    let port = match args.get(2) {
        None => publish_default_port(),
        Some(raw) => match raw.parse::<u32>() {
            Ok(p) if (1024..=65535).contains(&p) => p as u16,
            _ => return format!("Invalid port '{raw}'"),
        },
    };
    let mut state = publish_state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = *state {
        return format!("Local game already hosted on port {}", active.port);
    }
    let gamemode = gamemode.unwrap_or_else(|| DEFAULT_GAME_MODE.load(Ordering::Relaxed));
    *state = Some(PublishState {
        port,
        allow_commands,
        gamemode,
    });
    format!("Local game hosted on port {port}")
}

// ---------------------------------------------------------------------------
// Lane POLICY-CMD: one owner-level `/megu policy` surface for the server's
// opt-in gameplay policies. Every default is the vanilla/opt-in-safe value;
// `set` validates the whole key/value pair before mutating so a rejected
// command can never leave a half-applied policy behind.
// ---------------------------------------------------------------------------

const POLICY_USAGE: &str = "usage: /megu policy list|get <key>|set <key> <value>|reset";

#[derive(Clone, Debug, PartialEq)]
struct LiveServerPolicy {
    minimap_mode: String,
    allow_cave_map: bool,
    allow_mob_radar: bool,
    player_radar_scope: String,
    allow_waypoint_teleport: bool,
    mine_all: String,
    cut_all: String,
    xp_merge_mode: String,
    max_frame_mib: u32,
    /// OPS-SRE: Vanilla lets every hit land, so both friendly-fire switches
    /// default `true`; `false` is the strict opt-in that protects a pet's
    /// owner/party allies and party-vs-party damage respectively.
    pub pet_friendly_fire: bool,
    pub party_friendly_fire: bool,
    /// Opt-in QOL-DEATH toggle: own death drops restore to the slot they were
    /// scattered from on pickup. Off is the pure-Vanilla pickup path.
    pub death_slot_restore: bool,
}

impl Default for LiveServerPolicy {
    fn default() -> Self {
        Self {
            minimap_mode: "unrestricted".to_owned(),
            allow_cave_map: true,
            allow_mob_radar: true,
            player_radar_scope: "all".to_owned(),
            allow_waypoint_teleport: true,
            mine_all: "off".to_owned(),
            cut_all: "off".to_owned(),
            xp_merge_mode: "vanilla".to_owned(),
            max_frame_mib: 2,
            pet_friendly_fire: true,
            party_friendly_fire: true,
            death_slot_restore: false,
        }
    }
}

fn policy_invalid_value(key: &str, value: &str) -> String {
    format!("Invalid value '{value}' for policy '{key}'")
}

fn policy_bool(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(policy_invalid_value(key, value)),
    }
}

fn policy_choice<'a>(key: &str, value: &'a str, allowed: &[&str]) -> Result<&'a str, String> {
    if allowed.contains(&value) {
        Ok(value)
    } else {
        Err(policy_invalid_value(key, value))
    }
}

impl LiveServerPolicy {
    fn get(&self, key: &str) -> Result<String, String> {
        match key {
            "minimap_mode" => Ok(self.minimap_mode.clone()),
            "allow_cave_map" => Ok(self.allow_cave_map.to_string()),
            "allow_mob_radar" => Ok(self.allow_mob_radar.to_string()),
            "player_radar_scope" => Ok(self.player_radar_scope.clone()),
            "allow_waypoint_teleport" => Ok(self.allow_waypoint_teleport.to_string()),
            "mine_all" => Ok(self.mine_all.clone()),
            "cut_all" => Ok(self.cut_all.clone()),
            "xp_merge_mode" => Ok(self.xp_merge_mode.clone()),
            "max_frame_mib" => Ok(self.max_frame_mib.to_string()),
            "pet_friendly_fire" => Ok(self.pet_friendly_fire.to_string()),
            "party_friendly_fire" => Ok(self.party_friendly_fire.to_string()),
            "death_slot_restore" => Ok(self.death_slot_restore.to_string()),
            _ => Err(format!("Unknown policy '{key}'")),
        }
    }

    fn list(&self) -> Vec<(&'static str, String)> {
        vec![
            ("minimap_mode", self.minimap_mode.clone()),
            ("allow_cave_map", self.allow_cave_map.to_string()),
            ("allow_mob_radar", self.allow_mob_radar.to_string()),
            ("player_radar_scope", self.player_radar_scope.clone()),
            ("allow_waypoint_teleport", self.allow_waypoint_teleport.to_string()),
            ("mine_all", self.mine_all.clone()),
            ("cut_all", self.cut_all.clone()),
            ("xp_merge_mode", self.xp_merge_mode.clone()),
            ("max_frame_mib", self.max_frame_mib.to_string()),
            ("pet_friendly_fire", self.pet_friendly_fire.to_string()),
            ("party_friendly_fire", self.party_friendly_fire.to_string()),
            ("death_slot_restore", self.death_slot_restore.to_string()),
        ]
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "minimap_mode" => {
                self.minimap_mode = policy_choice(
                    key,
                    value,
                    &["unrestricted", "fair_play", "disabled"],
                )?
                .to_owned();
            }
            "allow_cave_map" => self.allow_cave_map = policy_bool(key, value)?,
            "allow_mob_radar" => self.allow_mob_radar = policy_bool(key, value)?,
            "player_radar_scope" => {
                self.player_radar_scope = policy_choice(key, value, &["all", "party_only", "none"])?
                    .to_owned();
            }
            "allow_waypoint_teleport" => {
                self.allow_waypoint_teleport = policy_bool(key, value)?
            }
            "mine_all" | "cut_all" => {
                let mode = policy_choice(
                    key,
                    value,
                    &["off", "sneak_only", "while_not_sneaking", "always"],
                )?
                .to_owned();
                if key == "mine_all" {
                    self.mine_all = mode;
                } else {
                    self.cut_all = mode;
                }
            }
            "xp_merge_mode" => {
                self.xp_merge_mode = policy_choice(
                    key,
                    value,
                    &["vanilla", "overflow_only", "aggressive"],
                )?
                .to_owned();
            }
            "max_frame_mib" => {
                // Vanilla's CompressionDecoder rejects past 8 MiB inflated —
                // no compatible client can legitimately send more, so the
                // knob may only widen the strict 2 MiB default up to that.
                let mib = match value.parse::<u32>() {
                    Ok(mib) if (2..=8).contains(&mib) => mib,
                    _ => return Err(policy_invalid_value(key, value)),
                };
                self.max_frame_mib = mib;
            }
            "pet_friendly_fire" => self.pet_friendly_fire = policy_bool(key, value)?,
            "party_friendly_fire" => self.party_friendly_fire = policy_bool(key, value)?,
            "death_slot_restore" => {
                self.death_slot_restore = policy_bool(key, value)?
            }
            _ => return Err(format!("Unknown policy '{key}'")),
        }
        Ok(())
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn live_server_policy() -> &'static Mutex<LiveServerPolicy> {
    static POLICY: OnceLock<Mutex<LiveServerPolicy>> = OnceLock::new();
    POLICY.get_or_init(|| Mutex::new(LiveServerPolicy::default()))
}

/// Lane WIRE-MINECUT-ORBITAL-SYNC: the `(mine_all, cut_all)` trigger modes
/// the live block-break path reads on every confirmed break. Both default
/// `"off"`; values were validated by `/megu policy set` (or the `server.toml`
/// load path) to `off`/`sneak_only`/`while_not_sneaking`/`always`.
#[cfg(feature = "mod-suite")]
fn live_mine_cut_all_modes() -> (String, String) {
    let policy = live_server_policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    (policy.mine_all.clone(), policy.cut_all.clone())
}

/// OPS-SRE friendly-fire gates. Both read the live policy so a
/// `/megu policy set` lands on the very next hit, and both are Vanilla by
/// default: `true` means every hit connects exactly like stock, and only an
/// explicit `false` arms the protection below.
///
/// May `attacker_uuid` damage the pet owned by `pet_owner_uuid`? With
/// `pet_friendly_fire` on (the default) the answer is always yes. With it off
/// the owner cannot clip their own pet and a party ally cannot either, while
/// hostile mobs — whose entity uuid is never the owner's player uuid — and
/// unrelated players still deal normal damage. An untamed pet (`None` owner)
/// is wild: the owner check cannot match and anyone may hurt it.
#[allow(dead_code)]
fn should_allow_pet_damage(
    attacker_uuid: [u8; 16],
    pet_owner_uuid: Option<[u8; 16]>,
    attacker_is_party_ally: bool,
) -> bool {
    if live_server_policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pet_friendly_fire
    {
        return true;
    }
    !attacker_is_party_ally && pet_owner_uuid != Some(attacker_uuid)
}

/// May a hit between party allies land? `party_friendly_fire` on (the
/// default) is Vanilla: yes, always. Off, allies are protected and every
/// non-ally pairing fights normally.
#[allow(dead_code)]
fn should_allow_party_pvp(attacker_is_party_ally: bool) -> bool {
    live_server_policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .party_friendly_fire
        || !attacker_is_party_ally
}

/// `/megu policy ...` is owner-level (4) like `/profile`: the advertised tree
/// only exposes it at level 4 and the handler re-checks the level so a
/// level-2 `.mcfunction` frame can never mutate the policy store.
fn run_policy_command(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    let mut policy = live_server_policy()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match args {
        ["policy", "list"] => policy
            .list()
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join("; "),
        ["policy", "get", key] => match policy.get(key) {
            Ok(value) => format!("{key} = {value}"),
            Err(error) => error,
        },
        ["policy", "set", key, value] => match policy.set(key, value) {
            Ok(()) => {
                // The QOL-DEATH atomic backs `drop_tick`'s pickup path; keep it
                // in step with the policy so a `/megu policy set` lands on the
                // very next pickup.
                set_death_slot_restore(policy.death_slot_restore);
                match policy.get(key) {
                    Ok(current) => format!("{key} = {current}"),
                    Err(error) => error,
                }
            }
            Err(error) => error,
        },
        ["policy", "reset"] => {
            policy.reset();
            set_death_slot_restore(policy.death_slot_restore);
            "All server policies reset to defaults".into()
        }
        _ => POLICY_USAGE.into(),
    }
}

// ---------------------------------------------------------------------------
// Lane OPS-ADMIN-BUILTIN: `/megu audit|rollback|claim|backup|metrics` — the
// built-in replacement for the block-audit/rollback, land-claim and
// backup/metrics admin plugins. `audit`, `rollback`, `backup` and `metrics`
// are OP-4 only and re-check `level` here so a level-2 `.mcfunction` frame
// can never reach them; `claim` self-management (`here|unclaim|trust|
// untrust|info`) is open to normal players for their own chunks while the
// admin overrides (`admin-unclaim`, `spawn-radius`, `spawn-protect`) stay
// OP-4 inside the same handler.
// ---------------------------------------------------------------------------

const MEGU_USAGE: &str = "usage: /megu policy list|get <key>|set <key> <value>|reset \
| audit [player] [radius] | rollback <player> <radius> [max_blocks] \
| claim here|unclaim|trust <player>|untrust <player>|info|admin-unclaim|spawn-radius <n>|spawn-protect <n> \
| backup plan|status | metrics";

/// `/megu <sub> ...` dispatch. `policy` keeps its own usage/handler; every
/// other subcommand is owned by this lane.
fn run_megu_command(ctx: &CmdCtx, level: u8, args: &[&str]) -> String {
    match args.first().copied().unwrap_or("") {
        // Bare `/megu` keeps the policy usage line — the pre-existing
        // POLICY-CMD contract — while unknown subcommands get the full map.
        "" | "policy" => run_policy_command(level, args),
        "audit" => run_megu_audit(ctx, level, &args[1..]),
        "rollback" => run_megu_rollback(ctx, level, &args[1..]),
        "claim" => run_megu_claim(ctx, level, &args[1..]),
        "backup" => run_megu_backup(level, &args[1..]),
        "metrics" => run_megu_metrics(level, &args[1..]),
        _ => MEGU_USAGE.into(),
    }
}

/// Registry state id -> operator-readable block name (`state#<id>` when the
/// registry is not loaded).
fn audit_block_label(state: u32) -> String {
    breg().map_or_else(
        || format!("state#{state}"),
        |reg| reg.name_of(state).to_owned(),
    )
}

fn audit_entry_line(entry: &LiveBlockAuditEntry) -> String {
    format!(
        "tick {} {} {}, {}, {}: {}: {} -> {}",
        entry.tick,
        profiler_dim_name(entry.dim),
        entry.x,
        entry.y,
        entry.z,
        entry.actor,
        audit_block_label(entry.old_state),
        audit_block_label(entry.new_state),
    )
}

/// `/megu audit [player] [radius]` — OP-4. Queries the bounded live audit
/// ring around the executor (dimension, position, radius) for one actor, or
/// every actor when `player` is omitted/`all`. Newest first, capped display.
fn run_megu_audit(ctx: &CmdCtx, level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    let mut actor: Option<String> = None;
    let mut radius: i32 = 64;
    for arg in args {
        // A bare integer is always the radius; the first non-numeric token
        // (other than the `all` wildcard) is the actor filter.
        if let Ok(r) = arg.parse::<i32>() {
            if !(0..=30_000_000).contains(&r) {
                return "audit radius must be in 0..=30000000".into();
            }
            radius = r;
        } else if *arg == "all" {
            // Wildcard: every actor — only valid before any actor was named.
            if actor.is_some() {
                return MEGU_USAGE.into();
            }
        } else if actor.is_none() {
            actor = Some((*arg).to_owned());
        } else {
            return MEGU_USAGE.into();
        }
    }
    let dim = cmd_dim(ctx);
    let center = (
        dim,
        ctx.x.floor() as i32,
        ctx.y.floor() as i32,
        ctx.z.floor() as i32,
    );
    let mut entries = query_live_block_audit(&LiveBlockAuditQuery {
        actor,
        dim: Some(dim),
        center: Some(center),
        radius,
        since_tick: None,
    });
    entries.reverse();
    let matched = entries.len();
    let mut out = format!(
        "Block audit: {matched} edit(s) within {radius} of ({}, {}, {}) in {}\n",
        center.1,
        center.2,
        center.3,
        profiler_dim_name(dim),
    );
    for entry in entries.iter().take(25) {
        out.push_str(&audit_entry_line(entry));
        out.push('\n');
    }
    if matched > 25 {
        out.push_str(&format!("… and {} more (ring holds {})\n", matched - 25, live_block_audit_len()));
    }
    out
}

/// `/megu rollback <player> <radius> [max_blocks]` — OP-4. Restores the
/// audited pre-edit state of every matching block in reverse chronological
/// order; cells that diverged since the recorded edit are skipped, and the
/// pass is summarised into the rollback log.
fn run_megu_rollback(ctx: &CmdCtx, level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    let (Some(target), Some(radius_arg)) = (args.first(), args.get(1)) else {
        return "usage: /megu rollback <player> <radius> [max_blocks]".into();
    };
    let Ok(radius) = radius_arg.parse::<i32>() else {
        return "usage: /megu rollback <player> <radius> [max_blocks]".into();
    };
    if !(0..=30_000_000).contains(&radius) {
        return "rollback radius must be in 0..=30000000".into();
    }
    let max_blocks = match args.get(2) {
        None => 1_024usize,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=65_536).contains(&n) => n,
            _ => return "max_blocks must be in 1..=65536".into(),
        },
    };
    if args.len() > 3 {
        return "usage: /megu rollback <player> <radius> [max_blocks]".into();
    }
    let dim = cmd_dim(ctx);
    let center = (
        dim,
        ctx.x.floor() as i32,
        ctx.y.floor() as i32,
        ctx.z.floor() as i32,
    );
    let mut entries = query_live_block_audit(&LiveBlockAuditQuery {
        actor: Some((*target).to_owned()),
        dim: Some(dim),
        center: Some(center),
        radius,
        since_tick: None,
    });
    // Reverse chronological: newest edit rolls back first so a chain of
    // edits on one cell unwinds cleanly to the oldest original state.
    entries.reverse();
    let matched = entries.len();
    let (restored, skipped, applied) =
        rollback_live_block_entries(&entries, max_blocks);
    let op = ctx
        .me
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Server".to_owned());
    record_live_rollback_summary(LiveRollbackSummary {
        tick: TICK.load(Ordering::Relaxed),
        op,
        target: (*target).to_owned(),
        dim,
        x: center.1,
        y: center.2,
        z: center.3,
        radius,
        matched,
        restored,
        skipped,
        truncated: matched > applied,
    });
    format!(
        "Rollback for {target}: restored {restored} block(s), skipped {skipped} diverged, \
         of {matched} matched edit(s){}",
        if matched > applied {
            format!(" (truncated at {max_blocks})")
        } else {
            String::new()
        }
    )
}

/// The claim subcommands a normal player may run for their own chunks. The
/// outer permission gate (`command_allowed_for_player`) consults this list so
/// `/megu claim here` parses for level-0 players while every admin override
/// stays behind the OP-4 `megu` root gate — fail closed either way.
fn megu_claim_player_subcommand(args: &[&str]) -> bool {
    args.first().copied() == Some("claim")
        && matches!(
            args.get(1).copied(),
            Some("here" | "unclaim" | "trust" | "untrust" | "info")
        )
}

/// `/megu claim ...` — self-management for normal players, OP-4 overrides.
/// Requires a player executor for every subcommand except the OP-4 admin
/// ones, so command blocks and console frames can never stake a claim.
fn run_megu_claim(ctx: &CmdCtx, level: u8, args: &[&str]) -> String {
    let dim = cmd_dim(ctx);
    let (x, z) = (ctx.x.floor() as i32, ctx.z.floor() as i32);
    let key = claim_chunk_key(dim, x, z);
    let player = ctx.me.as_ref().map(|c| c.name.clone());
    match args {
        ["here"] => {
            let Some(player) = player else {
                return "Claims require a player".into();
            };
            let tick = TICK.load(Ordering::Relaxed);
            let mut claims = live_claims()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(record) = claims.get(&key) {
                return if record.owner == player {
                    format!("This chunk is already claimed by you")
                } else {
                    format!("This chunk is already claimed by {}", record.owner)
                };
            }
            let owned = claims
                .values()
                .filter(|rec| rec.owner == player)
                .count();
            let quota = live_claim_quota();
            if owned >= quota {
                return format!(
                    "Claim quota reached ({owned}/{quota} chunks) — unclaim elsewhere first"
                );
            }
            claims.insert(
                key,
                LiveClaimRecord {
                    owner: player.clone(),
                    trusted: std::collections::BTreeSet::new(),
                    created_tick: tick,
                },
            );
            format!(
                "Claimed chunk ({}, {}) in {} for {player} ({owned}/{quota} used)",
                key.1,
                key.2,
                profiler_dim_name(dim),
            )
        }
        ["unclaim"] => {
            let Some(player) = player else {
                return "Claims require a player".into();
            };
            let mut claims = live_claims()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match claims.get(&key) {
                Some(record) if record.owner == player => {
                    claims.remove(&key);
                    format!("Unclaimed chunk ({}, {})", key.1, key.2)
                }
                Some(record) => format!("This chunk is claimed by {}, not you", record.owner),
                None => "This chunk is not claimed".into(),
            }
        }
        ["trust", name] | ["untrust", name] => {
            let trust = args[0] == "trust";
            let Some(player) = player else {
                return "Claims require a player".into();
            };
            let mut claims = live_claims()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(record) = claims.get_mut(&key) else {
                return "This chunk is not claimed".into();
            };
            if record.owner != player && level < 4 {
                return format!("Only the claim owner ({}) or an operator may do that", record.owner);
            }
            if trust {
                record.trusted.insert((*name).to_owned());
                format!("Trusted {name} on this claim")
            } else {
                record.trusted.remove(*name);
                format!("Removed trust for {name} on this claim")
            }
        }
        ["info"] => {
            let claims = live_claims()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut out = String::new();
            match claims.get(&key) {
                Some(record) => {
                    out.push_str(&format!(
                        "Chunk ({}, {}) in {}: claimed by {} since tick {}\n",
                        key.1,
                        key.2,
                        profiler_dim_name(dim),
                        record.owner,
                        record.created_tick,
                    ));
                    if record.trusted.is_empty() {
                        out.push_str("Trusted: none\n");
                    } else {
                        out.push_str(&format!(
                            "Trusted: {}\n",
                            record.trusted.iter().cloned().collect::<Vec<_>>().join(", ")
                        ));
                    }
                }
                None => out.push_str(&format!(
                    "Chunk ({}, {}) in {}: unclaimed\n",
                    key.1,
                    key.2,
                    profiler_dim_name(dim),
                )),
            }
            let radius = live_spawn_protect_radius();
            if radius > 0 {
                out.push_str(&format!(
                    "Spawn protection: {radius} blocks around world spawn (Overworld)\n"
                ));
            } else {
                out.push_str("Spawn protection: off\n");
            }
            if let Some(player) = player {
                let owned = claims
                    .values()
                    .filter(|rec| rec.owner == player)
                    .count();
                out.push_str(&format!(
                    "Your claims: {owned}/{} chunks\n",
                    live_claim_quota()
                ));
            }
            out
        }
        ["admin-unclaim"] => {
            if level < 4 {
                return P8C_DENIED.into();
            }
            let mut claims = live_claims()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match claims.remove(&key) {
                Some(record) => format!("Admin-unclaimed chunk ({}, {}) from {}", key.1, key.2, record.owner),
                None => "This chunk is not claimed".into(),
            }
        }
        ["spawn-radius", raw] | ["spawn-protect", raw] => {
            if level < 4 {
                return P8C_DENIED.into();
            }
            match raw.parse::<i32>() {
                Ok(radius) => match set_live_spawn_protect_radius(radius) {
                    Ok(()) if radius == 0 => "Spawn protection disabled".into(),
                    Ok(()) => format!("Spawn protection set to {radius} blocks"),
                    Err(error) => error,
                },
                Err(_) => MEGU_USAGE.into(),
            }
        }
        _ => MEGU_USAGE.into(),
    }
}

/// `/megu backup plan|status` — OP-4. Wires the live `plan_backup_rotation`
/// decision and the on-disk backup directory into an instant operator
/// answer; no external plugin required.
fn run_megu_backup(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    match args {
        ["plan"] => format_backup_plan_report(),
        ["status"] => format_backup_status_report(),
        _ => "usage: /megu backup plan|status".into(),
    }
}

/// `/megu metrics` — OP-4. The live Prometheus text exposition
/// (`format_prometheus` over a snapshot gathered from the running server),
/// the same scrape a localhost collector would pull.
fn run_megu_metrics(level: u8, args: &[&str]) -> String {
    if level < 4 {
        return P8C_DENIED.into();
    }
    if !args.is_empty() {
        return "usage: /megu metrics".into();
    }
    live_operator_metrics_prometheus()
}

/// MOD-SUITE bridge: the bounded pregen sweep `megu-world-migrate
/// pregen-run` drives offline, offered to the live mod suite over a real
/// `ServerContext` — the same `PregenJob` spiral, the same ticket-LRU
/// unloader cap, the same counters. `generate` is the mod host's real
/// worldgen hook; `flushed` receives every evicted overlay in LRU order for
/// persistence, so nothing evicted is silently dropped. Gated exactly like
/// the world-snapshot bridge in `anvil_io.rs`: `mod-suite` builds get the
/// production call site the pregen controller needs; default builds keep an
/// unchanged dependency graph.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn modsuite_pregen_run(
    ctx: &megu_state_store::sharded::ServerContext,
    job: &mut megu_state_store::pregen::PregenJob,
    config: &megu_state_store::pregen::PregenRunConfig<'_>,
    generate: impl FnMut(i32, i32),
    flushed: impl FnMut(megu_state_store::pregen::FlushedChunk),
) -> megu_state_store::pregen::PregenRunReport {
    job.run(ctx, config, generate, flushed)
}

// ------------------------------------------------ #225: player/entity gaps

/// The entity-effect wire packet with an explicit duration and particle
/// visibility: the shared `entity_effect_pkt` always emits a finite duration
/// with particles+icon, so `infinite` (Vanilla wire `-1`) and
/// `hideParticles` need this variant.
fn entity_effect_pkt_flags(
    eid: i32,
    id: i32,
    amp: i32,
    ticks: i32,
    hide_particles: bool,
) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, eid as i64);
    write_varint(&mut p, effect_wire_id(id));
    write_varint(&mut p, amp as i64);
    write_varint(&mut p, ticks as i64);
    // Flags byte: 0x02 show particles, 0x04 show icon — Vanilla's encoding.
    p.push(if hide_particles { 0x04 } else { 0x06 });
    p
}

/// `/effect give <targets> <effect> [infinite|<seconds>] [<amplifier>]
/// [<hideParticles>]` and `/effect clear [<targets>] [<effect>]`. `infinite`
/// stores a never-expiring `left` (the tick loop only ends an effect when it
/// counts to zero) and reports Vanilla's wire duration of `-1`.
fn run_effect_command(args: &[&str], me: Option<&Arc<Client>>) -> String {
    let sub = args.first().copied().unwrap_or("");
    let sel = args.get(1).copied().unwrap_or("@s");
    let targets = if sel.starts_with('@') {
        resolve_selector(sel, me)
    } else {
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|c| c.name == sel)
            .map(|c| vec![c.eid.to_string()])
            .unwrap_or_default()
    };
    // hold a snapshot of client Arcs, not the clients() mutex —
    // broadcast_seen re-locks clients() internally.
    let cls_snap: Vec<Arc<Client>> = client_snapshot();
    let mut touched = 0;
    let mut fx: Vec<(Arc<Client>, Vec<u8>)> = Vec::new(); // (target, entity_effect pkt)
    let mut rx: Vec<(Arc<Client>, Vec<u8>)> = Vec::new(); // (target, remove_effect pkt)
    let reply2 = match sub {
        "give" => {
            let ename = args.get(2).copied().unwrap_or("");
            let Some(eid2) = name_to_effect(ename) else {
                return format!("unknown effect '{ename}'");
            };
            let (secs, infinite) = match args.get(3) {
                None => (30, false),
                Some(&"infinite") => (0, true),
                Some(raw) => match raw.parse::<i32>() {
                    Ok(s) if (1..=1_000_000).contains(&s) => (s, false),
                    _ => return format!("Invalid duration '{raw}'"),
                },
            };
            let amp = match args.get(4) {
                None => 0,
                Some(raw) => match raw.parse::<i32>() {
                    Ok(a) if (0..=255).contains(&a) => a,
                    _ => return format!("Invalid amplifier '{raw}'"),
                },
            };
            let hide = match args.get(5) {
                None => false,
                Some(&"true") => true,
                Some(&"false") => false,
                Some(raw) => return format!("Invalid hideParticles '{raw}'"),
            };
            if args.len() > 6 {
                return "usage: /effect give <targets> <effect> [infinite|<seconds>] [<amplifier>] [<hideParticles>]".into();
            }
            for t in &targets {
                if let Some(c) = cls_snap.iter().find(|c| c.eid.to_string() == *t) {
                    match eid2 {
                        // instant effects apply once, not stored
                        5 => {
                            let mut h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
                            *h = (*h + 4.0 * (amp + 1) as f32).min(20.0);
                            if let Ok(mut w) = c.sock.lock() {
                                let _ = send_packet(&mut w, 0x5d, health_pkt_c(*h, c));
                            }
                        }
                        6 => {
                            let mut h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
                            *h = (*h - 6.0 * (amp + 1) as f32).max(0.0);
                            if *h <= 0.0 {
                                drop(h);
                                totem_check(c);
                                h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
                            }
                            if let Ok(mut w) = c.sock.lock() {
                                let _ = send_packet(&mut w, 0x5d, health_pkt_c(*h, c));
                            }
                        }
                        22 => {
                            let mut f = c.food.lock().unwrap_or_else(|e| e.into_inner());
                            *f = (*f + amp + 1).min(20);
                            drop(f);
                            if let Ok(mut w) = c.sock.lock() {
                                let _ = send_packet(
                                    &mut w,
                                    0x5d,
                                    health_pkt_c(*c.hp.lock().unwrap_or_else(|e| e.into_inner()), c),
                                );
                            }
                        }
                        _ => {
                            let ticks = if infinite {
                                // The effect tick only expires an entry that
                                // counts to zero; i32::MAX is the storable
                                // "infinite" while the wire gets Vanilla's -1.
                                i32::MAX
                            } else {
                                secs * TICKS_PER_SECOND as i32
                            };
                            {
                                let mut ef = c.effs.lock().unwrap_or_else(|e| e.into_inner());
                                ef.retain(|e| e.id != eid2);
                                ef.push(Eff {
                                    id: eid2,
                                    amp,
                                    left: ticks,
                                });
                            }
                            fx.push((
                                (*c).clone(),
                                entity_effect_pkt_flags(
                                    c.eid,
                                    eid2,
                                    amp,
                                    if infinite { -1 } else { ticks },
                                    hide,
                                ),
                            ));
                        }
                    }
                    touched += 1;
                }
            }
            format!("Applied effect to {} target(s)", touched)
        }
        "clear" => {
            let ename = args.get(2).copied().unwrap_or("");
            let want: Option<i32> = if ename.is_empty() {
                None
            } else {
                match name_to_effect(ename) {
                    Some(effect) => Some(effect),
                    None => return format!("unknown effect '{ename}'"),
                }
            };
            for t in &targets {
                if let Some(c) = cls_snap.iter().find(|c| c.eid.to_string() == *t) {
                    let gone: Vec<i32> = {
                        let mut ef = c.effs.lock().unwrap_or_else(|e| e.into_inner());
                        let g: Vec<i32> = ef
                            .iter()
                            .filter(|e| want.map(|w| w == e.id).unwrap_or(true))
                            .map(|e| e.id)
                            .collect();
                        ef.retain(|e| want.map(|w| w != e.id).unwrap_or(false));
                        g
                    };
                    for id in gone {
                        rx.push(((*c).clone(), remove_effect_pkt(c.eid, id)));
                    }
                    touched += 1;
                }
            }
            format!("Cleared effects from {} target(s)", touched)
        }
        _ => "usage: /effect give|clear <target> [effect] [infinite|seconds] [amplifier] [hideParticles]".into(),
    };
    // send outside any clients() lock
    for (c, pk) in &fx {
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x76, pk.clone());
        }
    }
    for (c, pk) in &rx {
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x43, pk.clone());
        }
    }
    for (_c, pk) in &fx {
        broadcast_seen(_c.eid, |_o| (0x76, pk.clone()));
    }
    for (_c, pk) in &rx {
        broadcast_seen(_c.eid, |_o| (0x43, pk.clone()));
    }
    reply2
}

/// `/clear [<targets>] [<item>] [<maxCount>]` — `maxCount = 0` is Vanilla's
/// dry-run: it counts matching items without removing any (pinned by
/// `clear_max_count_applies_per_player_and_zero_only_counts`).
fn run_clear_command(args: &[&str], me: Option<&Arc<Client>>) -> String {
    let targets = match args.first() {
        Some(sel) if sel.starts_with('@') => resolve_selector(sel, me),
        Some(n) if name_to_item(n).is_none() => {
            clients()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|c| c.name == *n)
                .map(|c| vec![c.eid.to_string()])
                .unwrap_or_default()
        }
        _ => me.map(|c| vec![c.eid.to_string()]).unwrap_or_default(),
    };
    if args.len() > 3 {
        return "usage: /clear [target] [item] [maxCount]".into();
    }
    let want = match args.get(1) {
        Some(item) => match name_to_item(item) {
            Some(id) => Some(id),
            None => return format!("Unknown item '{item}'"),
        },
        None => None,
    };
    let max = match args.get(2) {
        Some(count) => match count.parse::<u32>() {
            Ok(value) => Some(value),
            Err(_) => return "Invalid maximum count".into(),
        },
        None => None,
    };
    let mut removed = 0i64;
    let cls: Vec<Arc<Client>> = client_snapshot();
    for t in &targets {
        if let Some(c) = cls.iter().find(|c| c.eid.to_string() == *t) {
            let mut inv = c.inv.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<i16> = inv
                .iter()
                .filter(|(_, (it, _, _))| want.map(|w| w == *it).unwrap_or(true))
                .map(|(&k, _)| k)
                .collect();
            let mut player_removed = 0u32;
            for k in keys {
                let (it, cnt, comps) = inv[&k].clone();
                let take = if max == Some(0) {
                    u32::from(cnt)
                } else {
                    max.map(|m| u32::from(cnt).min(m.saturating_sub(player_removed)))
                        .unwrap_or(u32::from(cnt))
                };
                if take == 0 {
                    break;
                }
                player_removed += take;
                removed += i64::from(take);
                if max != Some(0) {
                    if take == u32::from(cnt) {
                        inv.remove(&k);
                    } else {
                        inv.insert(k, (it, cnt - take as u8, comps));
                    }
                }
            }
            // resync window 0 so the client drops the cleared items
            let mut q = vec![0u8];
            write_varint(&mut q, i64::from(inv_state_next(c)));
            write_varint(&mut q, 46);
            for i in 0..46i16 {
                let (it2, c2, _cc) = inv.get(&i).cloned().unwrap_or((0, 0, Vec::new()));
                q.extend_from_slice(&slot_wire(&(it2, c2, _cc)));
            }
            q.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x13, q);
            }
        }
    }
    format!("Cleared {} item(s)", removed)
}

/// Vanilla 1.21.1 `DamageTypes` registry ids (the `minecraft:` prefix is
/// implied). The `/damage` argument is a registry lookup, so unknown ids fail
/// closed rather than silently re-typing the hit as `generic`.
fn damage_type_known(name: &str) -> bool {
    const TYPES: &[&str] = &[
        "arrow",
        "bad_respawn_point",
        "cactus",
        "campfire",
        "cramming",
        "dragon_breath",
        "drown",
        "dry_out",
        "ender_pearl",
        "explosion",
        "fall",
        "falling_anvil",
        "falling_block",
        "falling_stalactite",
        "fireball",
        "fireworks",
        "fly_into_wall",
        "freeze",
        "generic",
        "generic_kill",
        "hot_floor",
        "in_fire",
        "in_wall",
        "indirect_magic",
        "lava",
        "lightning_bolt",
        "magic",
        "mob_attack",
        "mob_attack_no_aggro",
        "mob_projectile",
        "on_fire",
        "out_of_world",
        "outside_border",
        "player_attack",
        "player_explosion",
        "sonic_boom",
        "spit",
        "stalagmite",
        "starve",
        "sting",
        "sweet_berry_bush",
        "thorns",
        "thrown",
        "trident",
        "unattributed_fireball",
        "wind_charge",
        "wither",
        "wither_skull",
    ];
    TYPES.contains(&name.trim_start_matches("minecraft:"))
}

/// `by <entity>`/`from <entity>` each take Vanilla's single-entity argument:
/// anything resolving to a count other than one is an error, not a pick.
fn damage_single_entity(sel: &str, me: Option<&Arc<Client>>) -> Result<String, String> {
    let found = resolve_selector(sel, me);
    if found.len() == 1 {
        Ok(found[0].clone())
    } else {
        Err("Expected a single entity".to_owned())
    }
}

/// `/damage <target> <amount> [<damageType>] [at <pos>|by <entity> [from
/// <cause>]]`. The type, position and attacker clauses are validated like
/// Vanilla's registry/entity arguments; the physical model (armor, totem,
/// hurt animation) is the same whatever the declared source is.
fn run_damage_command(ctx: &CmdCtx, args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str =
        "usage: /damage <target> <amount> [<damageType>] [at <pos>|by <entity> [from <cause>]]";
    let sel = args.first().copied().unwrap_or("");
    let Some(raw_amount) = args.get(1) else {
        return USAGE.into();
    };
    let amt = match raw_amount.parse::<f32>() {
        Ok(a) if a.is_finite() && a >= 0.0 => a,
        _ => return format!("Invalid damage amount '{raw_amount}'"),
    };
    // The optional tail starts at the damage type; `at`/`by` require it.
    match args.get(2..).unwrap_or_default() {
        [] | [_] => {}
        [_, "at", x, y, z] => {
            let _ = (
                coord(x, ctx.x),
                coord(y, ctx.y),
                coord(z, ctx.z),
            );
        }
        [_, "by", entity] => {
            if let Err(e) = damage_single_entity(entity, me) {
                return e;
            }
        }
        [_, "by", entity, "from", cause] => {
            for who in [entity, cause] {
                if let Err(e) = damage_single_entity(who, me) {
                    return e;
                }
            }
        }
        _ => return USAGE.into(),
    }
    if let Some(dtype) = args.get(2) {
        if !damage_type_known(dtype) {
            return format!("Invalid damage type '{dtype}'");
        }
    }
    let tgts = if sel.starts_with('@') {
        resolve_selector(sel, me)
    } else {
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|c| c.name == sel)
            .map(|c| vec![c.eid.to_string()])
            .unwrap_or_default()
    };
    let mut n = 0;
    let cls_snap: Vec<Arc<Client>> = client_snapshot();
    let mut mob_hits: Vec<(i32, f32)> = Vec::new();
    for t in &tgts {
        if let Some(e) = t.strip_prefix("m:").and_then(|e| e.parse::<i32>().ok()) {
            mob_hits.push((e, amt));
        } else if let Some(c) = cls_snap.iter().find(|c| c.eid.to_string() == *t) {
            if *c.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1 {
                continue;
            }
            // OPS-SRE `party_friendly_fire` gate: a player-issued `/damage` on a
            // teammate respects the policy exactly like a melee hit would.
            if let Some(attacker) = me {
                let team = p3b_team_of(&attacker.name);
                let ally = team.is_some() && team == p3b_team_of(&c.name);
                if !should_allow_party_pvp(ally) {
                    continue;
                }
            }
            let mut h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
            *h = (*h - hurt_absorb(c, dmg_after(c, amt, 0))).max(0.0);
            if *h <= 0.0 {
                drop(h);
                totem_check(c);
                h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
            }
            let hh = *h;
            drop(h);
            damage_armor(c, amt);
            if let Ok(mut w) = c.sock.lock() {
                let _ = send_packet(&mut w, 0x24, hurt_anim(c.eid));
                let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, c));
            }
            n += 1;
        }
    }
    for (eid, amt) in mob_hits {
        let dead = {
            let ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
            ms.iter()
                .find(|m| m.eid == eid)
                .map(|m| {
                    let mut h = m.hp.lock().unwrap_or_else(|e| e.into_inner());
                    *h -= amt;
                    *h <= 0.0
                })
                .unwrap_or(false)
        };
        let mob = mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|m| m.eid == eid)
            .cloned();
        if let Some(m) = mob {
            // OPS-SRE `pet_friendly_fire` gate: a player-issued `/damage` on a
            // tamed pet is refused for its owner and party allies; mob and
            // command-block sources (`me == None`) deal normal damage.
            if let Some(attacker) = me {
                let owner = live_mob_states()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&m.eid)
                    .and_then(|state| match &state.behavior {
                        Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                            pet.owner
                        }
                        _ => None,
                    });
                let owner_name = owner.and_then(|uuid| {
                    cls_snap
                        .iter()
                        .find(|c| c.uuid == uuid)
                        .map(|c| c.name.clone())
                });
                let team = p3b_team_of(&attacker.name);
                let ally = team.is_some()
                    && owner_name.as_deref().is_some_and(|name| team == p3b_team_of(name));
                if !should_allow_pet_damage(attacker.uuid, owner, ally) {
                    continue;
                }
            }
            *m.hurt_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            broadcast_seen(eid, |_c| (0x24, hurt_anim(eid)));
            if dead {
                let (mx, my, mz) = {
                    let p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                    (p.0, p.1, p.2)
                };
                let et = m.etype;
                kill_mob(eid, et, mx, my, mz);
            }
            n += 1;
        }
    }
    format!("Applied {} damage to {} target(s)", amt, n)
}

/// `/tag <targets> add <name> | remove <name> | list` — Vanilla's score-holder
/// tag verbs over the shared `tags()` store; one report per matched target on
/// `list` (pinned by `tag_list_reports_every_matched_target`).
fn run_tag_command(args: &[&str], me: Option<&Arc<Client>>) -> String {
    let sel = args.first().copied().unwrap_or("");
    let sub = args.get(1).copied().unwrap_or("");
    let tg = args.get(2).copied().unwrap_or("");
    let tgts = if sel.starts_with('@') {
        resolve_selector(sel, me)
    } else {
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|c| c.name == sel)
            .map(|c| vec![c.eid.to_string()])
            .unwrap_or_default()
    };
    let tm = tags().lock().unwrap_or_else(|e| e.into_inner());
    match sub {
        "add" | "remove" if !tg.is_empty() => {
            let mut tm = tm;
            let mut n = 0;
            for t in &tgts {
                let key = if let Some(e) = t.strip_prefix("m:") {
                    format!("e:{}", e)
                } else if t.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
                    && t.contains(':')
                {
                    t.clone()
                } else {
                    clients()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|c| c.eid.to_string() == *t)
                        .map(|c| format!("p:{}", c.name))
                        .unwrap_or_default()
                };
                if key.is_empty() {
                    continue;
                }
                match sub {
                    "add" => {
                        tm.entry(key).or_default().insert(tg.to_string());
                    }
                    _ => {
                        if let Some(st) = tm.get_mut(&key) {
                            st.remove(tg);
                        }
                    }
                }
                n += 1;
            }
            format!(
                "{}ed {} target(s)",
                if sub == "add" { "tagg" } else { "untagg" },
                n
            )
        }
        "list" => {
            // one report per matched target: a `return` inside the loop
            // answered only for the first one and named the selector
            // (`@a`) instead of the target it was reporting.
            let mut lines: Vec<String> = Vec::new();
            for t in &tgts {
                let key = if let Some(e) = t.strip_prefix("m:") {
                    format!("e:{}", e)
                } else {
                    clients()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .find(|c| c.eid.to_string() == *t)
                        .map(|c| format!("p:{}", c.name))
                        .unwrap_or_default()
                };
                if key.is_empty() {
                    continue;
                }
                let who = key.strip_prefix("p:").unwrap_or(t).to_string();
                let st = tm.get(&key);
                let mut tags: Vec<String> =
                    st.map(|s| s.iter().cloned().collect()).unwrap_or_default();
                tags.sort();
                lines.push(format!(
                    "{} has {} tag(s): {}",
                    who,
                    tags.len(),
                    tags.join(", ")
                ));
            }
            if lines.is_empty() {
                "no targets".into()
            } else {
                lines.join(", ")
            }
        }
        _ => "usage: /tag <target> add|remove|list [tag]".into(),
    }
}

// ---------------------------------------------------------- /attribute #225

/// Vanilla `AttributeModifier.Operation` — `add` applies to the base,
/// `multiply_base` scales the post-add base, `multiply` scales the total.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum AttrModOp {
    Add,
    MultiplyBase,
    MultiplyTotal,
}

/// `/attribute`'s command-side modifier store, keyed the way `attrs()` is:
/// `p:<name>` for players and `e:<eid>` for transient entities, then the bare
/// attribute name, then the modifier id.
type AttributeModifiers = HashMap<String, HashMap<String, BTreeMap<String, (f64, AttrModOp)>>>;

fn attr_modifiers() -> &'static Mutex<AttributeModifiers> {
    static STORE: OnceLock<Mutex<AttributeModifiers>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A modifier id is a Vanilla `ResourceLocation`: lowercase namespace + path
/// of `[a-z0-9_.-/]` (path may also contain `/`).
fn modifier_id_valid(id: &str) -> bool {
    let (ns, path) = match id.split_once(':') {
        Some((ns, path)) => (Some(ns), path),
        None => (None, id),
    };
    let part_ok = |s: &str, slash: bool| {
        !s.is_empty()
            && s.bytes().all(|b| {
                matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-')
                    || (slash && b == b'/')
            })
    };
    ns.is_none_or(|n| part_ok(n, false)) && part_ok(path, true)
}

/// The stored base (or Vanilla default) for `base get` — equipment and
/// command modifiers stay out of it, matching `AttributeInstance.getBase`.
fn attr_base_of(key: &str, name: &str) -> f64 {
    attrs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .and_then(|m| m.get(name))
        .copied()
        .unwrap_or_else(|| attr_default(name))
}

/// Vanilla `AttributeInstance.calculateValue` over the command modifier set,
/// on top of `attr_of`'s base-plus-equipment value:
/// `(base + Σadd) → + that × Σmultiply_base → × Π(1 + multiply)`.
fn attr_effective_of(key: &str, name: &str) -> f64 {
    let base = attr_of(key, name);
    let mods = attr_modifiers()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(mods) = mods.get(key).and_then(|m| m.get(name)) else {
        return base;
    };
    let (mut add, mut mb, mut mt) = (0.0, 0.0, 1.0);
    for &(amount, op) in mods.values() {
        match op {
            AttrModOp::Add => add += amount,
            AttrModOp::MultiplyBase => mb += amount,
            AttrModOp::MultiplyTotal => mt *= 1.0 + amount,
        }
    }
    let post_add = base + add;
    (post_add + post_add * mb) * mt
}

/// `/attribute <target> <attr> get [<scale>] | base get [<scale>] | base set
/// <v> | modifier add <id> <v> <op> | modifier remove <id> | modifier value
/// get <id> [<scale>]` — Vanilla 1.21.1's single-entity attribute command.
fn run_attribute_command(args: &[&str], me: Option<&Arc<Client>>) -> String {
    const USAGE: &str = "usage: /attribute <target> <attribute> get [<scale>] | \
        base get [<scale>] | base set <value> | modifier add <id> <value> \
        add|multiply|multiply_base | modifier remove <id> | modifier value get <id> [<scale>]";
    let sel = args.first().copied().unwrap_or("");
    let aname = args.get(1).copied().unwrap_or("");
    if attr_key_id(aname).is_none() {
        return format!("Unknown attribute '{aname}'");
    }
    let attr = aname.trim_start_matches("minecraft:").to_string();
    // Vanilla's target is `EntityArgument.entity()`: exactly one entity.
    let eids = resolve_selector(sel, me);
    let [target] = eids.as_slice() else {
        return "Expected a single entity".into();
    };
    let (key, who) = {
        let cls: Vec<Arc<Client>> = client_snapshot();
        match cls.iter().find(|c| c.eid.to_string() == *target) {
            Some(c) => (format!("p:{}", c.name), c.name.clone()),
            None => {
                let eid = target.strip_prefix("m:").unwrap_or(target);
                (format!("e:{eid}"), format!("entity {eid}"))
            }
        }
    };
    let broadcast_attr = |eid: &str, value: f64| {
        let eid: i32 = eid
            .strip_prefix("m:")
            .unwrap_or(eid)
            .parse()
            .unwrap_or(0);
        let mut m = HashMap::new();
        m.insert(attr.clone(), value);
        let pk = attrs_pkt(eid, &m);
        // vanilla pushes attribute updates to every client tracking the entity
        broadcast(-1, |_c| (0x75, pk.clone()));
    };
    let scale_of = |arg: Option<&&str>| -> Result<f64, String> {
        match arg {
            None => Ok(1.0),
            Some(raw) => raw
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| format!("Invalid scale '{raw}'")),
        }
    };
    match args.get(2..).unwrap_or_default() {
        ["get", rest @ ..] => {
            if rest.len() > 1 {
                return USAGE.into();
            }
            let scale = match scale_of(rest.first()) {
                Ok(s) => s,
                Err(e) => return e,
            };
            format!(
                "Value of attribute {aname} for entity {who} is {}",
                attr_effective_of(&key, &attr) * scale
            )
        }
        ["base", "get", rest @ ..] => {
            if rest.len() > 1 {
                return USAGE.into();
            }
            let scale = match scale_of(rest.first()) {
                Ok(s) => s,
                Err(e) => return e,
            };
            format!(
                "Base value of attribute {aname} for entity {who} is {}",
                attr_base_of(&key, &attr) * scale
            )
        }
        ["base", "set", raw] => {
            let Ok(value) = raw.parse::<f64>() else {
                return format!("Invalid value '{raw}'");
            };
            if !value.is_finite() {
                return format!("Invalid value '{raw}'");
            }
            attrs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(key.clone())
                .or_default()
                .insert(attr.clone(), value);
            broadcast_attr(target, value);
            format!("Set base value of attribute {aname} for entity {who} to {value}")
        }
        ["modifier", "add", id, raw, op] => {
            if !modifier_id_valid(id) {
                return format!("Invalid modifier id '{id}'");
            }
            let Ok(value) = raw.parse::<f64>() else {
                return format!("Invalid value '{raw}'");
            };
            if !value.is_finite() {
                return format!("Invalid value '{raw}'");
            }
            let op = match *op {
                "add" => AttrModOp::Add,
                "multiply" => AttrModOp::MultiplyTotal,
                "multiply_base" => AttrModOp::MultiplyBase,
                _ => return USAGE.into(),
            };
            let mut store = attr_modifiers().lock().unwrap_or_else(|e| e.into_inner());
            let mods = store
                .entry(key.clone())
                .or_default()
                .entry(attr.clone())
                .or_default();
            if mods.contains_key(*id) {
                return format!(
                    "Modifier {id} is already present on attribute {aname} for entity {who}"
                );
            }
            mods.insert((*id).to_owned(), (value, op));
            drop(store);
            broadcast_attr(target, attr_effective_of(&key, &attr));
            format!("Added modifier {id} to attribute {aname} for entity {who}")
        }
        ["modifier", "remove", id] => {
            let mut store = attr_modifiers().lock().unwrap_or_else(|e| e.into_inner());
            let mods = store
                .entry(key.clone())
                .or_default()
                .entry(attr.clone())
                .or_default();
            if mods.remove(*id).is_none() {
                return format!("Attribute {aname} on entity {who} has no modifier {id}");
            }
            drop(store);
            broadcast_attr(target, attr_effective_of(&key, &attr));
            format!("Removed modifier {id} from attribute {aname} for entity {who}")
        }
        ["modifier", "value", "get", id, rest @ ..] => {
            if rest.len() > 1 {
                return USAGE.into();
            }
            let scale = match scale_of(rest.first()) {
                Ok(s) => s,
                Err(e) => return e,
            };
            let store = attr_modifiers().lock().unwrap_or_else(|e| e.into_inner());
            match store
                .get(&key)
                .and_then(|m| m.get(&attr))
                .and_then(|m| m.get(*id))
            {
                Some((amount, _)) => format!(
                    "Value of modifier {id} on attribute {aname} for entity {who} is {}",
                    amount * scale
                ),
                None => {
                    format!("Attribute {aname} on entity {who} has no modifier {id}")
                }
            }
        }
        _ => USAGE.into(),
    }
}

// ---------------------------------------------------------------------------
// Lane VANILLA-CMDBLK — vanilla 1.21.1 command blocks + command minecarts.
//
// Security contract (hard invariants):
//   * editing a command block/minecart requires OP level >= 2 AND creative
//     mode (gamemode == 1) with the enable-command-block toggle on — refused
//     fail-closed otherwise;
//   * every command runs through `run_command_ctx_level` pinned at exactly OP
//     level 2, never the console's 4, and the `COMMAND_BLOCK_ROOT_DENY` roots
//     are refused outright so a block can never stop/op/ban/transfer/profile
//     the server no matter how the shared level table evolves.
// ---------------------------------------------------------------------------

/// `minecraft:command_block`/`repeating`/`chain` — the wire enum order of the
/// 767 `set_command_block` mode VarInt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum CommandBlockMode {
    /// Chain command block: runs immediately when the block facing into it
    /// executed (and its own `powered || automatic` holds).
    Sequence,
    /// Repeating command block: runs every tick while powered or always-active.
    Auto,
    /// Impulse command block: runs once on the rising edge of
    /// `powered || automatic`.
    Redstone,
}

/// `minecraft:command_block[facing=*]` — the direction a finished block
/// triggers next; also where the comparator-side of the block reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum CommandBlockFacing {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

impl CommandBlockFacing {
    #[allow(dead_code)]
    fn delta(self) -> (i32, i32, i32) {
        match self {
            CommandBlockFacing::Down => (0, -1, 0),
            CommandBlockFacing::Up => (0, 1, 0),
            CommandBlockFacing::North => (0, 0, -1),
            CommandBlockFacing::South => (0, 0, 1),
            CommandBlockFacing::West => (-1, 0, 0),
            CommandBlockFacing::East => (1, 0, 0),
        }
    }
}

/// One placed command block — the fields the `set_command_block` packet writes
/// plus the runtime state vanilla keeps on the block entity.
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct CommandBlockRecord {
    dim: DimId,
    pos: (i32, i32, i32),
    mode: CommandBlockMode,
    facing: CommandBlockFacing,
    command: String,
    /// Wire `trackOutput` — whether `last_output` is refreshed per run.
    track_output: bool,
    last_output: String,
    /// Conditional chain blocks run only when the block facing into them left
    /// `success_count > 0`.
    conditional: bool,
    /// Vanilla `auto` flag ("Always Active"): no redstone power needed.
    automatic: bool,
    /// Set by the redstone layer; combined with `automatic` for the
    /// edge/run checks.
    powered: bool,
    last_powered: bool,
    /// Vanilla `successCount`: `1` when the last run did not produce an error
    /// reply, `0` otherwise; drives conditionals and the comparator signal.
    success_count: i32,
    /// Whether the conditional check passed at the last trigger.
    condition_met: bool,
    /// Vanilla `updateLastExecution` record flag. No wall-clock execution
    /// timestamp is kept; here it instead gates the `last_powered`
    /// edge-bookkeeping refresh — clearing it freezes the recorded power edge
    /// (the vanilla side effect of not updating the execution record).
    update_last_execution: bool,
}

#[allow(dead_code)]
impl CommandBlockRecord {
    fn new(dim: DimId, pos: (i32, i32, i32)) -> Self {
        CommandBlockRecord {
            dim,
            pos,
            mode: CommandBlockMode::Redstone,
            facing: CommandBlockFacing::Up,
            command: String::new(),
            track_output: false,
            last_output: String::new(),
            conditional: false,
            automatic: false,
            powered: false,
            last_powered: false,
            success_count: 0,
            condition_met: false,
            update_last_execution: true,
        }
    }
}

/// One `minecraft:command_block_minecart` — its command runs at OP level 2
/// every 4 ticks while the cart sits on a powered activator rail.
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct CommandMinecartRecord {
    entity_id: i32,
    dim: DimId,
    x: f64,
    y: f64,
    z: f64,
    command: String,
    track_output: bool,
    last_output: String,
    success_count: i32,
    /// Set by the activator-rail logic; execution cadence is counted only
    /// while this holds.
    activated: bool,
    powered_ticks: u32,
}

/// Every live command block, keyed `(dim, x, y, z)`.
#[allow(dead_code)]
type CommandBlockMap = HashMap<(DimId, i32, i32, i32), CommandBlockRecord>;

fn live_command_blocks() -> &'static Mutex<CommandBlockMap> {
    static BLOCKS: OnceLock<Mutex<CommandBlockMap>> = OnceLock::new();
    BLOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every live command minecart, keyed entity id.
#[allow(dead_code)]
fn command_minecarts() -> &'static Mutex<HashMap<i32, CommandMinecartRecord>> {
    static CARTS: OnceLock<Mutex<HashMap<i32, CommandMinecartRecord>>> =
        OnceLock::new();
    CARTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `enable-command-block` policy: `MEGU_ENABLE_COMMAND_BLOCK` in the
/// environment (server policy may layer the same switch). Anything but an
/// explicit false-y value keeps the vanilla default `true`.
#[allow(dead_code)]
fn command_blocks_enabled() -> bool {
    std::env::var("MEGU_ENABLE_COMMAND_BLOCK")
        .map(|value| {
            !matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true)
}

/// The permission level every command block and command minecart executes
/// under — vanilla pins these sources at level 2, never higher.
const COMMAND_BLOCK_OP_LEVEL: u8 = 2;

/// Root literals a command block is never allowed to run even if the shared
/// `required_permission_level` table would (today or after an edit) rate them
/// at level 2. `transfer`/`profile` are exactly such roots today; the rest of
/// the spec'd refuse-list is already refused by the level-2 gate and is kept
/// here as defence in depth.
const COMMAND_BLOCK_ROOT_DENY: &[&str] = &[
    "stop", "op", "deop", "ban", "ban-ip", "banlist", "pardon", "pardon-ip",
    "kick", "whitelist", "setidletimeout", "tick", "save", "save-all",
    "save-on", "save-off", "reload", "debug", "jfr", "perf", "publish",
    "transfer", "profile", "megu",
];

/// Whether `root` may run from a command block source: the level-2 gate plus
/// the explicit root deny-list. The dispatcher's own permission check still
/// applies on top — this is the extra invariant, not a replacement.
#[allow(dead_code)]
fn command_block_source_allowed(root: &str) -> bool {
    command_allowed_for_source(root, COMMAND_BLOCK_OP_LEVEL)
        && !COMMAND_BLOCK_ROOT_DENY.contains(&root)
}

/// Longest command string the `set_command_block` / `set_command_minecart`
/// decoders accept — below vanilla's hard `MAX_COMMAND_LENGTH` of 32767 and
/// well inside any surrounding packet bound.
const COMMAND_BLOCK_COMMAND_MAX: usize = 32_500;

/// Execute `command` from a command source at `(x, y, z)` in `dim` — shared
/// by command blocks (block-centre position) and command minecarts (cart
/// position). Returns `(success_count, reply)`.
#[allow(dead_code)]
fn command_source_run(
    _dim: DimId,
    x: f64,
    y: f64,
    z: f64,
    command: &str,
) -> (i32, String) {
    let trimmed = command.trim();
    let root = trimmed
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("");
    let reply = if trimmed.is_empty() {
        String::new()
    } else if !command_block_source_allowed(root) {
        "You do not have permission to run that command".to_owned()
    } else {
        let ctx = CmdCtx {
            me: None,
            x,
            y,
            z,
        };
        run_command_ctx_level(
            &ctx,
            &format!("/{}", trimmed.trim_start_matches('/')),
            &mut None,
            Some(COMMAND_BLOCK_OP_LEVEL),
        )
    };
    let success = if trimmed.is_empty() || command_reply_is_error(&reply) {
        0
    } else {
        1
    };
    // `commandBlockOutput` gates the vanilla admin-channel echo of the reply
    // to online operators (`[@: ...]`); the block's `last_output` bookkeeping
    // in the caller is unaffected either way.
    if !reply.is_empty() && gr_command_block_output() {
        let payload = system_chat(&format!("[@: {reply}]"));
        fanout_in(
            &client_snapshot(),
            |client| player_permission_level(&client.name) >= 1,
            |_| (0x6c, payload.clone()),
        );
    }
    (success, reply)
}

/// Run the record at `cursor` once and propagate the chain trigger along
/// `facing` — the shared tail of the impulse/repeating tick and of each chain
/// step. `budget` (`maxCommandChainLength`) is consumed per *chain-block
/// execution* only, exactly bounding execution cycles. `consecutive_skips`
/// bounds the walk itself: while no block runs, `success_count` state is
/// frozen, so skipping more distinct blocks than exist can only repeat
/// already-skipped nodes forever — a guaranteed non-executing cycle.
#[allow(dead_code)]
fn command_block_execute_trigger(
    trigger: (DimId, i32, i32, i32),
    budget: &mut u32,
) {
    let block_total = live_command_blocks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    let mut cursor = trigger;
    let mut prev_success = 0i32;
    let mut consecutive_skips = 0usize;
    let mut first = true;
    loop {
        if !first && consecutive_skips > block_total {
            break;
        }
        let step = {
            let mut map = live_command_blocks()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(rec) = map.get_mut(&cursor) else {
                break;
            };
            if !first {
                if rec.mode != CommandBlockMode::Sequence {
                    break;
                }
                let powered = rec.powered || rec.automatic;
                let cond_ok = !rec.conditional || prev_success > 0;
                rec.condition_met = powered && cond_ok;
                // Vanilla propagates the trigger through chain blocks that do
                // not run (unpowered, or a failed condition) — the next
                // block then sees this block's *stored* success count.
                prev_success = rec.success_count;
                if !powered || !cond_ok {
                    consecutive_skips += 1;
                    cursor = (
                        cursor.0,
                        cursor.1 + rec.facing.delta().0,
                        cursor.2 + rec.facing.delta().1,
                        cursor.3 + rec.facing.delta().2,
                    );
                    continue;
                }
                if *budget == 0 {
                    // `maxCommandChainLength` exhausted — the chain stops
                    // cleanly, cutting any execution cycle.
                    break;
                }
                *budget -= 1;
            }
            (
                rec.command.clone(),
                rec.track_output,
                rec.facing,
            )
        };
        let (dim, x, y, z) = cursor;
        let (success, reply) = command_source_run(
            dim,
            x as f64 + 0.5,
            y as f64 + 0.5,
            z as f64 + 0.5,
            &step.0,
        );
        {
            let mut map = live_command_blocks()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(rec) = map.get_mut(&cursor) {
                rec.success_count = success;
                rec.last_output = if step.1 { reply } else { String::new() };
            }
        }
        consecutive_skips = 0;
        prev_success = success;
        let (dx, dy, dz) = step.2.delta();
        cursor = (dim, x + dx, y + dy, z + dz);
        first = false;
    }
}

/// One server tick of the command-block engine: impulse blocks fire on the
/// rising edge of `powered || automatic`, repeating blocks every tick while
/// it holds; chain blocks never fire here — they only propagate off a block
/// that just ran.
#[allow(dead_code)]
fn command_block_tick() {
    if !command_blocks_enabled() {
        return;
    }
    // `CommandBlock.neighborChanged` latches the redstone input into `powered`
    // — here the pass polls the live signal once per tick per block so the
    // edge bookkeeping below sees vanilla's `powered` transitions.
    if let Some(reg) = breg() {
        let positions: Vec<(DimId, i32, i32, i32)> = live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        for (dim, x, y, z) in positions {
            // A record without a placed command-block cell keeps its scripted
            // `powered` — `neighborChanged` only exists for a real block.
            if !command_block_name(reg.name_of(block_at_dim(dim, x, y, z))) {
                continue;
            }
            let powered = nearby_redstone(reg, dim, RsPos(x, y, z));
            set_command_block_powered(dim, (x, y, z), powered);
        }
    }
    let triggers: Vec<(DimId, i32, i32, i32)> = {
        let mut map = live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.iter_mut()
            .filter_map(|(key, rec)| {
                let active = rec.powered || rec.automatic;
                let rising = active && !rec.last_powered;
                if rec.update_last_execution {
                    rec.last_powered = active;
                }
                match rec.mode {
                    CommandBlockMode::Auto => active.then_some(*key),
                    CommandBlockMode::Redstone => rising.then_some(*key),
                    CommandBlockMode::Sequence => None,
                }
            })
            .collect()
    };
    for trigger in triggers {
        let mut budget = gr_max_command_chain_length();
        command_block_execute_trigger(trigger, &mut budget);
    }
}

/// The comparator read-out vanilla exposes from a command block's
/// `successCount` (clamped to the 0..15 signal range).
#[allow(dead_code)]
fn command_block_comparator_signal(dim: DimId, pos: (i32, i32, i32)) -> u8 {
    live_command_blocks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(dim, pos.0, pos.1, pos.2))
        .map_or(0, |rec| rec.success_count.clamp(0, 15) as u8)
}

/// Insert or update a live command block record (world layer's place/edit
/// path) — replaces the whole record except `dim`/`pos` which key it.
#[allow(dead_code)]
fn upsert_command_block(rec: CommandBlockRecord) {
    live_command_blocks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((rec.dim, rec.pos.0, rec.pos.1, rec.pos.2), rec);
}

/// Set the externally-powered flag the tick's edge detection reads.
#[allow(dead_code)]
fn set_command_block_powered(dim: DimId, pos: (i32, i32, i32), powered: bool) {
    if let Some(rec) = live_command_blocks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&(dim, pos.0, pos.1, pos.2))
    {
        rec.powered = powered;
    }
}

/// Drop the record when the world block is broken.
#[allow(dead_code)]
fn remove_command_block(
    dim: DimId,
    pos: (i32, i32, i32),
) -> Option<CommandBlockRecord> {
    live_command_blocks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(dim, pos.0, pos.1, pos.2))
}

/// Register a live command minecart (spawn or `set_command_minecart` edit).
#[allow(dead_code)]
fn upsert_command_minecart(rec: CommandMinecartRecord) {
    command_minecarts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(rec.entity_id, rec);
}

/// The activator-rail signal: powered rail -> `activated = true`.
#[allow(dead_code)]
fn set_command_minecart_activated(entity_id: i32, activated: bool) {
    if let Some(rec) = command_minecarts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&entity_id)
    {
        if !activated {
            rec.powered_ticks = 0;
        }
        rec.activated = activated;
    }
}

/// Drop the minecart record when the entity dies.
#[allow(dead_code)]
fn remove_command_minecart(entity_id: i32) -> Option<CommandMinecartRecord> {
    command_minecarts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&entity_id)
}

/// `minecraft:(chain_|repeating_)?command_block` — the three placed forms.
#[allow(dead_code)]
fn command_block_name(name: &str) -> bool {
    matches!(
        name,
        "minecraft:command_block"
            | "minecraft:chain_command_block"
            | "minecraft:repeating_command_block"
    )
}

/// The record a freshly placed command-block state starts: mode from the block
/// name, facing and conditional from the block-state properties the placer
/// oriented — the vanilla `CommandBlockEntity` defaults otherwise.
#[allow(dead_code)]
fn command_block_record_of(
    reg: &crate::block_registry::BReg,
    state: u32,
    dim: DimId,
    pos: (i32, i32, i32),
) -> Option<CommandBlockRecord> {
    let mut rec = CommandBlockRecord::new(dim, pos);
    rec.mode = match reg.name_of(state) {
        "minecraft:chain_command_block" => CommandBlockMode::Sequence,
        "minecraft:repeating_command_block" => CommandBlockMode::Auto,
        "minecraft:command_block" => CommandBlockMode::Redstone,
        _ => return None,
    };
    rec.facing = match reg.get_prop(state, "facing").as_deref() {
        Some("down") => CommandBlockFacing::Down,
        Some("north") => CommandBlockFacing::North,
        Some("south") => CommandBlockFacing::South,
        Some("west") => CommandBlockFacing::West,
        Some("east") => CommandBlockFacing::East,
        _ => CommandBlockFacing::Up,
    };
    rec.conditional = reg.get_prop(state, "conditional").as_deref() == Some("true");
    Some(rec)
}

/// One server tick of the command-minecart engine: an activated cart runs its
/// command every 4 ticks (vanilla's `tickCount % 4 == 0` cadence) at OP level
/// 2, positioned at the cart.
#[allow(dead_code)]
fn command_minecart_tick() {
    if !command_blocks_enabled() {
        return;
    }
    // Sync each record with its live entity: the cart's position feeds the
    // command source, and a record for a removed entity is dropped.
    {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let mut carts = command_minecarts()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let reg = breg();
        let mut rail_updates = Vec::new();
        for (id, rec) in carts.iter_mut() {
            // Sync each record with its live entity: the cart's position feeds
            // the command source. A record without a live entity keeps its
            // scripted fields (`remove_live_mob` owns cleanup).
            let Some(mob) = mobs.iter().find(|mob| mob.eid == *id) else {
                continue;
            };
            let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            rec.dim = mob.dim;
            rec.x = x;
            rec.y = y;
            rec.z = z;
            // `AbstractMinecart.onActivatorRail`: a powered activator rail
            // under the cart latches `activated` for the run cadence below.
            if let Some(reg) = reg {
                let st = block_at_dim(
                    rec.dim,
                    x.floor() as i32,
                    y.floor() as i32,
                    z.floor() as i32,
                );
                rail_updates.push((
                    *id,
                    reg.name_of(st) == "minecraft:activator_rail"
                        && reg.get_prop(st, "powered").as_deref() == Some("true"),
                ));
            }
        }
        drop(carts);
        for (id, powered) in rail_updates {
            set_command_minecart_activated(id, powered);
        }
    }
    let due: Vec<i32> = {
        let mut carts = command_minecarts()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        carts
            .iter_mut()
            .filter_map(|(id, rec)| {
                if !rec.activated {
                    return None;
                }
                let run = rec.powered_ticks % 4 == 0;
                rec.powered_ticks = rec.powered_ticks.saturating_add(1);
                run.then_some(*id)
            })
            .collect()
    };
    for entity_id in due {
        let snapshot = {
            let carts = command_minecarts()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            carts.get(&entity_id).map(|rec| {
                (rec.dim, rec.x, rec.y, rec.z, rec.command.clone(), rec.track_output)
            })
        };
        let Some((dim, x, y, z, command, track_output)) = snapshot else {
            continue;
        };
        let (success, reply) = command_source_run(dim, x, y, z, &command);
        if let Some(rec) = command_minecarts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&entity_id)
        {
            rec.success_count = success;
            rec.last_output = if track_output { reply } else { String::new() };
        }
    }
}

/// 767 `set_command_block` (C2S 0x30) decoded: packed BlockPos, command
/// string, mode VarInt, flags byte — exactly the vanilla field order.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
struct SetCommandBlockUpdate {
    pos: (i32, i32, i32),
    command: String,
    mode: CommandBlockMode,
    track_output: bool,
    conditional: bool,
    automatic: bool,
}

/// 767 `set_command_minecart` (C2S 0x31) decoded: entity VarInt, command
/// string, track-output bool.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
struct SetCommandMinecartUpdate {
    entity_id: i32,
    command: String,
    track_output: bool,
}

/// Vanilla `FriendlyByteBuf.readUtf`-style string for the two command-block
/// packets — VarInt length, hard cap, strict UTF-8, fail closed.
#[allow(dead_code)]
fn command_block_wire_string(r: &mut P3bReader<'_>) -> Option<String> {
    let len = r.varint()?;
    if !(0..=COMMAND_BLOCK_COMMAND_MAX as i64).contains(&len) {
        return None;
    }
    let body = r.bytes(len as usize)?;
    std::str::from_utf8(body).ok().map(str::to_owned)
}

/// Vanilla `BlockPos` packed-i64 layout: `x << 38 | z << 12 | y`, the y field
/// signed at 12 bits.
#[allow(dead_code)]
fn command_block_unpack_pos(packed: i64) -> (i32, i32, i32) {
    (
        (packed >> 38) as i32,
        ((packed << 52) >> 52) as i32,
        ((packed << 26) >> 38) as i32,
    )
}

/// Decode the protocol-767 serverbound `set_command_block` body. Truncation,
/// an oversized command, a mode outside `0..=2` or trailing garbage all fail
/// closed with `None`.
#[allow(dead_code)]
fn decode_set_command_block_c2s(body: &[u8]) -> Option<SetCommandBlockUpdate> {
    let packed = i64::from_be_bytes(body.get(..8)?.try_into().ok()?);
    let mut r = P3bReader::new(body.get(8..)?);
    let command = command_block_wire_string(&mut r)?;
    let mode = match r.varint()? {
        0 => CommandBlockMode::Sequence,
        1 => CommandBlockMode::Auto,
        2 => CommandBlockMode::Redstone,
        _ => return None,
    };
    let flags = r.byte()?;
    if !r.done() {
        return None;
    }
    Some(SetCommandBlockUpdate {
        pos: command_block_unpack_pos(packed),
        command,
        mode,
        track_output: flags & 0x01 != 0,
        conditional: flags & 0x02 != 0,
        automatic: flags & 0x04 != 0,
    })
}

/// Decode the protocol-767 serverbound `set_command_minecart` body — VarInt
/// entity id, capped command string, single boolean byte — fail closed.
#[allow(dead_code)]
fn decode_set_command_minecart_c2s(body: &[u8]) -> Option<SetCommandMinecartUpdate> {
    let mut r = P3bReader::new(body);
    let entity_id = i32::try_from(r.varint()?).ok()?;
    let command = command_block_wire_string(&mut r)?;
    let track_output = match r.byte()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    if !r.done() {
        return None;
    }
    Some(SetCommandMinecartUpdate {
        entity_id,
        command,
        track_output,
    })
}

/// The shared edit gate for both packets: enabled toggle + OP level >= 2 +
/// creative (`gamemode == 1`) — refused fail-closed, matching vanilla's
/// `Player.canUseGameMasterBlocks` check.
#[allow(dead_code)]
fn command_block_edit_permitted(
    caller_op_level: u8,
    caller_gamemode: u8,
    enable_cmd_block: bool,
) -> Result<(), String> {
    if !(enable_cmd_block && command_blocks_enabled()) {
        return Err("Command blocks are not enabled on this server".into());
    }
    if caller_op_level < 2 || caller_gamemode != 1 {
        return Err(
            "You do not have permission to use command blocks".into(),
        );
    }
    Ok(())
}

/// Apply a decoded `set_command_block` update for `dim`. Runtime state the
/// packet does not carry (`powered`, `last_powered`, `success_count`,
/// `facing`, `condition_met`, `update_last_execution`) survives an edit;
/// a fresh position starts a default record.
#[allow(dead_code)]
fn apply_set_command_block(
    caller_op_level: u8,
    caller_gamemode: u8,
    enable_cmd_block: bool,
    dim: DimId,
    pkt: SetCommandBlockUpdate,
) -> Result<(), String> {
    command_block_edit_permitted(caller_op_level, caller_gamemode, enable_cmd_block)?;
    let mut map = live_command_blocks()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rec = map
        .entry((dim, pkt.pos.0, pkt.pos.1, pkt.pos.2))
        .or_insert_with(|| CommandBlockRecord::new(dim, pkt.pos));
    rec.mode = pkt.mode;
    rec.command = pkt.command;
    rec.track_output = pkt.track_output;
    rec.conditional = pkt.conditional;
    rec.automatic = pkt.automatic;
    Ok(())
}

/// Apply a decoded `set_command_minecart` update under the same edit gate.
#[allow(dead_code)]
fn apply_set_command_minecart(
    caller_op_level: u8,
    caller_gamemode: u8,
    enable_cmd_block: bool,
    dim: DimId,
    pkt: SetCommandMinecartUpdate,
) -> Result<(), String> {
    command_block_edit_permitted(caller_op_level, caller_gamemode, enable_cmd_block)?;
    let mut carts = command_minecarts()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rec = carts.entry(pkt.entity_id).or_insert_with(|| {
        CommandMinecartRecord {
            entity_id: pkt.entity_id,
            dim,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            command: String::new(),
            track_output: false,
            last_output: String::new(),
            success_count: 0,
            activated: false,
            powered_ticks: 0,
        }
    });
    rec.command = pkt.command;
    rec.track_output = pkt.track_output;
    Ok(())
}

// ------------------------------------------------------------------ tests ---

#[cfg(all(test, megu_vanilla_data))]
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
        // Generator initialisation is process-global: run in a child like the
        // other generated-world fixtures, or later tests read generated terrain
        // instead of an uninitialised world.
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        // Overlay restore on any path: a panic must not leak the seeded blocks.
        let saved_world = world().take_all();
        struct RestoreWorld(HashMap<DimChunkKey, HashMap<u32, u32>>);
        impl std::ops::Drop for RestoreWorld {
            fn drop(&mut self) {
                world().restore_all(std::mem::take(&mut self.0));
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

#[cfg(all(test, megu_vanilla_data))]
mod p3_parity_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn p3_packet_client(eid: i32, name: &str, x: f64, y: f64, z: f64) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.name = name.into();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0.0, 0.0, true);
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
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
        assert!(insert_live_mob(
            name_to_etype("horse").unwrap(),
            DimId::Overworld,
            (4.5, 16.0, 4.5, 0.0, 0.0),
            20.0,
            None,
        ));
        let mob_eid = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
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
            let live = mounts().lock().unwrap_or_else(|e| e.into_inner());
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
        let a = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
        p3_tag_entity(a, "p3ride_a");
        assert!(insert_live_mob(
            name_to_etype("zombie").unwrap(),
            DimId::Overworld,
            (9.5, 16.0, 4.5, 0.0, 0.0),
            20.0,
            None,
        ));
        let b = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().eid;
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
        mounts().lock().unwrap_or_else(|e| e.into_inner()).registry.dismount(player.eid);
        mounts().lock().unwrap_or_else(|e| e.into_inner()).registry.dismount(a);
        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != player.eid);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m.eid != mob_eid && m.eid != a && m.eid != b);
        tags().lock().unwrap_or_else(|e| e.into_inner()).remove(&format!("e:{mob_eid}"));
        tags().lock().unwrap_or_else(|e| e.into_inner()).remove(&format!("e:{a}"));
        tags().lock().unwrap_or_else(|e| e.into_inner()).remove(&format!("e:{b}"));
    }

    #[test]
    fn p3_parity_spectate_requires_spectator_and_sets_camera() {
        let _world = world_guard();
        let (viewer, mut recv) = p3_packet_client(9201, "p3watcher", 0.5, 16.0, 0.5);
        let (target, _rt) = p3_packet_client(9202, "p3prey", 1.5, 16.0, 0.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).extend([viewer.clone(), target.clone()]);
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
        *viewer.gm.lock().unwrap_or_else(|e| e.into_inner()) = 3;
        assert_eq!(
            p3_run(&ctx, "/spectate p3prey p3watcher"),
            "Now spectating minecraft:player"
        );
        let (id, payload) = p3_read_frame(&mut recv).expect("set_camera frame");
        assert_eq!(id, 0x52, "767 set_camera is 0x52 (proto/src/data_v767.rs)");
        let mut body = Vec::new();
        write_varint(&mut body, i64::from(target.eid));
        assert_eq!(payload, body, "camera payload is the target's VarInt eid");
        assert_eq!(spectator_cameras().lock().unwrap_or_else(|e| e.into_inner()).get(&viewer.eid), Some(&target.eid));

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
        assert_eq!(spectator_cameras().lock().unwrap_or_else(|e| e.into_inner()).get(&viewer.eid), Some(&viewer.eid));

        // non-player `<player>` arg and console default viewer are refused
        assert_eq!(
            p3_run(&ctx, "/spectate p3prey @e[tag=p3notanentity]"),
            "No player was found"
        );
        assert_eq!(p3_run(&ctx, "/spectate"), "Only players may use this command");

        *viewer.gm.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        spectator_cameras().lock().unwrap_or_else(|e| e.into_inner()).remove(&viewer.eid);
        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != viewer.eid && c.eid != target.eid);
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
            let store = biome_overrides().lock().unwrap_or_else(|e| e.into_inner());
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
            let mut registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
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

        biome_overrides().lock().unwrap_or_else(|e| e.into_inner()).remove(&dim);
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
        let before = place_dispatches().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(
            p3_run(&ctx, "/place feature minecraft:patch_taiga_grass 3000 90 3000"),
            "Placed feature 'minecraft:patch_taiga_grass' at [3000, 90, 3000]"
        );
        {
            let log = place_dispatches().lock().unwrap_or_else(|e| e.into_inner());
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
            let log = place_dispatches().lock().unwrap_or_else(|e| e.into_inner());
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
        let before = place_dispatches().lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(
            p3_run(&ctx, "/place feature minecraft:patch_taiga_grass"),
            "Placed feature 'minecraft:patch_taiga_grass' at [0, 16, 0]"
        );
        assert_eq!(place_dispatches().lock().unwrap_or_else(|e| e.into_inner()).len(), before + 1);
    }
}

#[cfg(all(test, megu_vanilla_data))]
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
        let saved = std::mem::take(&mut *sched_fns().lock().unwrap_or_else(|e| e.into_inner()));
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
            let scheduled = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
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
            let scheduled = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(sched_fns().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        // malformed time and unknown mode are rejected like vanilla parse errors.
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:x nope"),
            "usage: /schedule function <fn> <time>[t|s|d] [append|replace]"
        );
        assert_eq!(
            p5d_run(&ctx, "/schedule function p5d:x 1t sideways"),
            "usage: /schedule function <fn> <time>[t|s|d] [append|replace]"
        );
        *sched_fns().lock().unwrap_or_else(|e| e.into_inner()) = saved;
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
        let previous = std::mem::take(&mut *forceloaded().lock().unwrap_or_else(|e| e.into_inner()));
        let ctx = p5d_ctx(None);

        let added = p5d_run(&ctx, "/forceload add 32 48");
        assert!(added.contains("1 chunks"), "{added}");
        assert!(forceloaded().lock().unwrap_or_else(|e| e.into_inner()).contains(&(2, 3)));
        // every block inside the chunk reports forced; the neighbour does not.
        assert_eq!(p5d_run(&ctx, "/forceload query 32 48"), "Chunk 2, 3 is force loaded");
        assert_eq!(p5d_run(&ctx, "/forceload query 47 63"), "Chunk 2, 3 is force loaded");
        assert_eq!(p5d_run(&ctx, "/forceload query 16 48"), "Chunk 1, 3 is not force loaded");
        assert_eq!(p5d_run(&ctx, "/forceload query -1 -1"), "Chunk -1, -1 is not force loaded");
        let removed = p5d_run(&ctx, "/forceload remove all");
        assert_eq!(removed, "Removed 1 forceloaded chunk(s)");
        assert!(forceloaded().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert_eq!(p5d_run(&ctx, "/forceload query"), "0 forceloaded chunk(s)");

        *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = previous;
    }

    // #172 — /setworldspawn and /spawnpoint take the optional yaw and persist
    // it beside the coordinates.
    #[test]
    fn p5d_parity_spawn_commands_store_angle() {
        let _world = world_guard();
        let ctx = p5d_ctx(None);
        let previous = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());

        assert_eq!(spawn_yaw(Some("270.5"), None), Some(-89.5));
        assert_eq!(spawn_yaw(None, None), Some(0.0));
        assert_eq!(spawn_yaw(Some("NaN"), None), None);

        let reply = p5d_run(&ctx, "/setworldspawn 10 70 20 135");
        assert_eq!(reply, "Set the world spawn point to 10, 70, 20 facing 135");
        assert_eq!(*WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()), (10.0, 70.0, 20.0, 135.0));
        // no angle defaults to 0.0
        p5d_run(&ctx, "/setworldspawn 1 2 3");
        assert_eq!(*WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()), (1.0, 2.0, 3.0, 0.0));
        assert_eq!(p5d_run(&ctx, "/setworldspawn 1 2 3 nope"), "Invalid spawn angle");

        let (player, _recv) = p5d_client(60002, 0x52, "p5d_spawn");
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
        let reply = p5d_run(&ctx, "/spawnpoint p5d_spawn 5 65 15 -90");
        assert_eq!(reply, "Set spawn point to 5, 65, 15 facing -90 for p5d_spawn");
        assert_eq!(*player.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()), Some((5.0, 65.0, 15.0, -90.0)));
        // executor spawnpoint without pos uses the live position and yaw 0.
        ops_registry().lock().unwrap_or_else(|e| e.into_inner()).set("p5d_spawn", 4, false);
        *player.pos.lock().unwrap_or_else(|e| e.into_inner()) = (7.5, 64.0, -3.5, 0.0, 0.0, true);
        let player_ctx = p5d_ctx(Some(player.clone()));
        assert_eq!(
            p5d_run(&player_ctx, "/spawnpoint"),
            "Set spawn point to 7.5, 64, -3.5 facing 0 for p5d_spawn"
        );
        assert_eq!(*player.spawn_pt.lock().unwrap_or_else(|e| e.into_inner()), Some((7.5, 64.0, -3.5, 0.0)));
        ops_registry()
            .lock()
            .unwrap()
            .entries
            .retain(|e| e.name != "p5d_spawn");

        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 60002);
        *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = previous;
    }

    // #177 — /bossbar set players assigns the viewer list (empty clears it)
    // and /bossbar get reports exact integer results.
    #[test]
    fn p5d_parity_bossbar_players_and_exact_get_results() {
        let _world = world_guard();
        let saved = std::mem::take(&mut *bossbars().lock().unwrap_or_else(|e| e.into_inner()));
        let (a, _ra) = p5d_client(60011, 0x61, "p5d_a");
        let (b, _rb) = p5d_client(60012, 0x62, "p5d_b");
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(a.clone());
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(b.clone());

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

        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 60011 && c.eid != 60012);
        *bossbars().lock().unwrap_or_else(|e| e.into_inner()) = saved;
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
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(a.clone());
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(b.clone());
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(c.clone());

        // team members share one pile; unteamed players keep their own.
        {
            let mut registry = teams().lock().unwrap_or_else(|e| e.into_inner());
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
            let (pa, pb) = (*a.pos.lock().unwrap_or_else(|e| e.into_inner()), *b.pos.lock().unwrap_or_else(|e| e.into_inner()));
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
        let mob = mobs().lock().unwrap_or_else(|e| e.into_inner()).last().unwrap().clone();
        let mob_key = uuid_text(&mob.uuid);
        {
            let mut registry = teams().lock().unwrap_or_else(|e| e.into_inner());
            registry.join("p5d_team", &mob_key).unwrap();
        }
        assert_eq!(
            spreadplayers_team(&SpreadTarget::Mob(mob.clone())),
            Some("p5d_team".to_string())
        );

        // teardown: team, clients, mob and the floor.
        {
            let mut registry = teams().lock().unwrap_or_else(|e| e.into_inner());
            let _ = registry.remove("p5d_team");
        }
        clients()
            .lock()
            .unwrap()
            .retain(|p| !matches!(p.eid, 60021..=60023));
        mobs().lock().unwrap_or_else(|e| e.into_inner()).retain(|m| m.eid != mob.eid);
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
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0.0, 0.0, true);
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
        ops_registry().lock().unwrap_or_else(|e| e.into_inner()).remove(name);
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
        let mut profiler = live_hotspot_profiler().lock().unwrap_or_else(|e| e.into_inner());
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
        let report = live_hotspot_profiler().lock().unwrap_or_else(|e| e.into_inner()).summary(5);
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
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
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
        ops_registry().lock().unwrap_or_else(|e| e.into_inner()).set("p6d_xfer4", 4, false);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(admin.clone());
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
        ops_registry().lock().unwrap_or_else(|e| e.into_inner()).remove("p6d_xfer4");
    }

    #[test]
    fn p6d_parity_transfer_fail_closed_and_permission() {
        let _world = world_guard();
        let (player, mut recv) = p6d_client(97_110, "p6d_xfer3", 0.5, 16.0, 0.5);
        ops_registry()
            .lock()
            .unwrap()
            .set("p6d_xfer3", 3, false);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
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
        ops_registry().lock().unwrap_or_else(|e| e.into_inner()).remove("p6d_xfer3");
        ops_registry().lock().unwrap_or_else(|e| e.into_inner()).remove("p6d_xfer2");
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
            format!("Storage '{store}' has the following contents: [1, 2]")
        );
        // merge merges deep, not whole-compound replace.
        assert_eq!(
            p6d_run(&ctx, &format!("/data merge storage {store} {{nested:{{y:7}}}}")),
            "Modified data"
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} nested")),
            format!("Storage '{store}' has the following contents: {{x: 5, y: 7}}")
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
            format!("Storage '{store}' has the following contents: [0, 1, 2, 3]")
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
            format!("Storage '{store}' has the following contents: {{y: 7}}")
        );
        assert_eq!(
            p6d_run(&ctx, &format!("/data get storage {store} missing")),
            "Found no elements at 'missing'".to_string()
        );
        execute_storage().lock().unwrap_or_else(|e| e.into_inner()).remove(store);
    }

    #[test]
    fn p6d_parity_data_entity_roundtrip() {
        let _world = world_guard();
        let (player, _recv) = p6d_client(97_200, "p6d_data", 1.5, 64.0, -2.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
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
            format!("p6d_data has the following entity data: {}s", *player.air.lock().unwrap_or_else(|e| e.into_inner()))
        );
        assert_eq!(
            p6d_run(&ctx, "/data get entity p6d_data Motion"),
            "p6d_data has the following entity data: [0d, 0d, 0d]"
        );
        // merge writes reach the live model: Health lands in the hp mutex.
        assert_eq!(
            p6d_run(&ctx, "/data merge entity p6d_data {Health:5.0f,Tags:[\"marker\"],NoAI:1b}"),
            "Modified data"
        );
        assert_eq!(*player.hp.lock().unwrap_or_else(|e| e.into_inner()), 5.0);
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
        assert_eq!(*player.hp.lock().unwrap_or_else(|e| e.into_inner()), 20.0);
        // SelectedItem/Inventory write through to the live inventory map.
        let stone = name_to_item("stone").unwrap();
        assert_eq!(
            p6d_run(
                &ctx,
                "/data merge entity p6d_data {SelectedItem:{id:\"minecraft:stone\",count:2}}"
            ),
            "Modified data"
        );
        let held_slot = 36 + *player.held.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            player.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&held_slot).map(|s| (s.0, s.1)),
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
        let pos = *player.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((pos.3, pos.4), (90.0, 45.0));
        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 97_200);
        tags().lock().unwrap_or_else(|e| e.into_inner()).remove("p:p6d_data");
        data_entity_extras().lock().unwrap_or_else(|e| e.into_inner()).remove("p:p6d_data");
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
        assert!(got.starts_with("40, 90, 40 has the following block data"), "{got}");
        assert!(got.contains("\"minecraft:stone\""), "{got}");
        assert!(got.contains("count: 3"), "{got}");
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
            let live = chests().lock().unwrap_or_else(|e| e.into_inner());
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
            "40, 90, 40 has the following block data: 7"
        );
        assert_eq!(
            chests().lock().unwrap_or_else(|e| e.into_inner()).get(&key).and_then(|s| s.get(&2)).map(|s| s.1),
            Some(7)
        );
        // remove Items empties the live container.
        assert_eq!(
            p6d_run(&ctx, "/data remove block 40 90 40 Items"),
            "Removed data"
        );
        assert!(chests().lock().unwrap_or_else(|e| e.into_inner()).get(&key).is_none()
            || chests().lock().unwrap_or_else(|e| e.into_inner()).get(&key).is_some_and(|s| s.is_empty()));
        // A block position with no block entity fails closed.
        assert_eq!(
            p6d_run(&ctx, "/data get block 60 90 60"),
            "The block at 60, 90, 60 is not a block entity"
        );
        chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
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
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
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
        clients().lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.eid != 97_300);
        let mut tracker = player_advancements().lock().unwrap_or_else(|e| e.into_inner());
        tracker.progress.remove("p6d_adv");
        tracker.sent_progress.remove("p6d_adv");
        adv_done().lock().unwrap_or_else(|e| e.into_inner()).remove("p6d_adv");
    }
}

// ------------------------------------------------------------ p8c_parity ---

#[cfg(test)]
mod p8c_parity_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn p8c_client(eid: i32, name: &str, x: f64, y: f64, z: f64) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.name = name.into();
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0.0, 0.0, true);
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .unwrap();
        (Arc::new(client), receiver)
    }

    fn p8c_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn p8c_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    /// A level-`level` operator context for `name`: the ops registry is the
    /// real `player_permission_level` source, no test-only bypass.
    fn p8c_op_ctx(name: &str, level: u8) -> CmdCtx {
        if level > 0 {
            ops_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set(name, level, false);
        }
        let (client, _recv) = p8c_client(98_000 + level as i32, name, 0.5, 16.0, 0.5);
        p8c_ctx(Some(client))
    }

    fn p8c_deop(name: &str) {
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    fn p8c_frame_varint(r: &mut TcpStream) -> Option<(i64, usize)> {
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
    fn p8c_read_frame(r: &mut TcpStream) -> Option<(i64, Vec<u8>)> {
        let (len, _) = p8c_frame_varint(r)?;
        let (id, used) = p8c_frame_varint(r)?;
        let mut payload = vec![0u8; (len as usize).checked_sub(used)?];
        r.read_exact(&mut payload).ok()?;
        Some((id, payload))
    }

    // #145-#148: every cluster-C command is owner-level (4) — refused at 0..3,
    // refused inside a level-2 .mcfunction, never advertised below 4.
    #[test]
    fn p8c_parity_cluster_c_commands_require_op_level_four() {
        let _world = world_guard();
        for (name, level) in [("p8c_l0", 0u8), ("p8c_l2", 2), ("p8c_l3", 3)] {
            let ctx = p8c_op_ctx(name, level);
            for cmd in ["/debug start", "/jfr start", "/perf start", "/publish"] {
                assert_eq!(
                    p8c_run(&ctx, cmd),
                    "You do not have permission to run that command",
                    "level-{level} source must be refused {cmd}"
                );
            }
            p8c_deop(name);
        }
        // The advertised tree hides all four literals below level 4 and shows
        // them at exactly 4.
        for level in 0..4u8 {
            let root = native_command_dispatcher(level, None);
            for cmd in ["debug", "jfr", "perf", "publish"] {
                assert!(
                    root.root().child(cmd).is_none(),
                    "{cmd} literal must not be advertised at level {level}"
                );
            }
        }
        let level4 = native_command_dispatcher(4, None);
        for cmd in ["debug", "jfr", "perf", "publish"] {
            assert!(
                level4.root().child(cmd).is_some(),
                "{cmd} literal must be advertised at level 4"
            );
        }
        // A level-2 `.mcfunction` frame re-enters run_command_ctx with the
        // same source: run_function_lines is that path, not an approximation.
        let ctx = p8c_op_ctx("p8c_fn2", 2);
        run_function_lines(
            &[
                "debug start".to_owned(),
                "jfr start".to_owned(),
                "perf start".to_owned(),
                "publish".to_owned(),
            ],
            ctx.me.clone(),
            &mut None,
            0,
        );
        assert!(
            debug_session().lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "level-2 .mcfunction must not start /debug"
        );
        assert!(
            jfr_session().lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "level-2 .mcfunction must not start /jfr"
        );
        assert!(
            perf_session().lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "level-2 .mcfunction must not start /perf"
        );
        assert!(
            publish_state().lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "level-2 .mcfunction must not publish"
        );
        p8c_deop("p8c_fn2");
    }

    // #145 /debug start|stop|function — session lifecycle on the live ring.
    #[test]
    fn p8c_parity_debug_session_and_function_trace() {
        let _world = world_guard();
        let ctx = p8c_ctx(None); // console: level 4
        // stop-when-inactive refuses before anything else.
        assert_eq!(
            p8c_run(&ctx, "/debug stop"),
            "Can't stop profiling when we haven't started yet!"
        );
        assert_eq!(p8c_run(&ctx, "/debug start"), "Started debug profiling");
        assert_eq!(
            p8c_run(&ctx, "/debug start"),
            "Debug profiling has already started"
        );
        // Seed the session window, then the stop report carries the vanilla
        // seconds/ticks/TPS line plus the profiler's MSPT summary.
        {
            let mut profiler = live_hotspot_profiler().lock().unwrap_or_else(|e| e.into_inner());
            for us in [40_000u32, 50_000, 60_000, 50_000] {
                profiler.record_tick_us(us);
            }
        }
        let stop = p8c_run(&ctx, "/debug stop");
        assert!(stop.starts_with("Stopped debug profiling after "), "{stop}");
        assert!(stop.contains("ticks ("), "{stop}");
        assert!(stop.contains("ticks per second"), "{stop}");
        assert!(stop.contains("MSPT mean 50.0 ms"), "{stop}");
        assert!(stop.contains("p50 50.0"), "{stop}");
        assert!(stop.contains("(4 ticks sampled)"), "{stop}");
        // A second stop is refused again.
        assert_eq!(
            p8c_run(&ctx, "/debug stop"),
            "Can't stop profiling when we haven't started yet!"
        );

        // /debug function: trace a real datapack function, splitting
        // succeeded from failed lines and honouring `return`.
        let mut functions = HashMap::new();
        functions.insert(
            "p8c:traced".to_owned(),
            vec![
                "say first".to_owned(),
                "notacommand at all".to_owned(),
                "say second".to_owned(),
            ],
        );
        functions.insert(
            "p8c:early_return".to_owned(),
            vec!["say one".to_owned(), "return 0".to_owned(), "say skipped".to_owned()],
        );
        functions.insert(
            "p8c:recursive".to_owned(),
            vec!["debug function p8c:traced".to_owned()],
        );
        functions.insert(
            "p8c:macro".to_owned(),
            vec!["$say $(x)".to_owned()],
        );
        packs().lock().unwrap_or_else(|e| e.into_inner()).push(Datapack {
            name: "p8c_dbg".to_owned(),
            functions,
            load: Vec::new(),
            tick: Vec::new(),
        });
        // say broadcasts to every connected client: a quiet audience keeps the
        // trace deterministic either way.
        let trace = p8c_run(&ctx, "/debug function p8c:traced");
        assert_eq!(
            trace,
            "Finished tracing 3 command(s) from function 'p8c:traced', 2 succeeded and 1 failed"
        );
        // `return` ends the frame like Vanilla's discarded command frame.
        let early = p8c_run(&ctx, "/debug function p8c:early_return");
        assert_eq!(
            early,
            "Finished tracing 2 command(s) from function 'p8c:early_return', 2 succeeded and 0 failed"
        );
        // A nested `debug function` line is refused (noRecursion) and counts
        // as the failed line it is.
        let nested = p8c_run(&ctx, "/debug function p8c:recursive");
        assert_eq!(
            nested,
            "Finished tracing 1 command(s) from function 'p8c:recursive', 0 succeeded and 1 failed"
        );
        // Unknown and macro-bearing functions fail the trace outright.
        assert_eq!(
            p8c_run(&ctx, "/debug function p8c:missing"),
            "Failed to trace function"
        );
        assert_eq!(
            p8c_run(&ctx, "/debug function p8c:macro"),
            "Failed to trace function"
        );
        packs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|p| p.name != "p8c_dbg");
        // usage forms refuse cleanly.
        assert_eq!(
            p8c_run(&ctx, "/debug"),
            "usage: /debug start|stop|function <name>"
        );
        assert_eq!(
            p8c_run(&ctx, "/debug function"),
            "usage: /debug start|stop|function <name>"
        );
    }

    // #146 /jfr start|stop — native telemetry recording on the live ring.
    #[test]
    fn p8c_parity_jfr_native_telemetry_session() {
        let _world = world_guard();
        let ctx = p8c_ctx(None);
        assert_eq!(
            p8c_run(&ctx, "/jfr stop"),
            "Native telemetry profiling is not running"
        );
        assert_eq!(
            p8c_run(&ctx, "/jfr start"),
            "Started native telemetry profiling (Rust JFR equivalent)"
        );
        assert_eq!(
            p8c_run(&ctx, "/jfr start"),
            "Native telemetry profiling is already running"
        );
        {
            let mut profiler = live_hotspot_profiler().lock().unwrap_or_else(|e| e.into_inner());
            for us in [10_000u32, 20_000, 30_000, 100_000] {
                profiler.record_tick_us(us);
            }
        }
        let stop = p8c_run(&ctx, "/jfr stop");
        assert!(stop.contains("4 ticks"), "{stop}");
        // nearest-rank over [10,20,30,100]: p50 = 20, p95/p99 = 100.
        assert!(stop.contains("p50 20.0 ms"), "{stop}");
        assert!(stop.contains("p95 100.0 ms"), "{stop}");
        assert!(stop.contains("p99 100.0 ms"), "{stop}");
        assert_eq!(
            p8c_run(&ctx, "/jfr stop"),
            "Native telemetry profiling is not running"
        );
        assert_eq!(p8c_run(&ctx, "/jfr bogus"), "usage: /jfr start|stop");
    }

    // #147 /perf start|stop — bounded capture with the full MSPT + hotspot
    // report on stop.
    #[test]
    fn p8c_parity_perf_bounded_capture_reports_hotspots() {
        let _world = world_guard();
        let ctx = p8c_ctx(None);
        assert_eq!(
            p8c_run(&ctx, "/perf stop"),
            "No performance capture is running"
        );
        assert_eq!(p8c_run(&ctx, "/perf start"), "Started performance capture");
        assert_eq!(
            p8c_run(&ctx, "/perf start"),
            "Performance capture is already running"
        );
        // start() reset the window: only the ticks seeded after it count.
        {
            let mut profiler = live_hotspot_profiler().lock().unwrap_or_else(|e| e.into_inner());
            for us in [50_000u32; 8] {
                profiler.record_tick_us(us);
            }
            profiler.record_chunk_work(DimId::Overworld, 3, -9, ProfilerSubsystem::Mobs, 4_200);
        }
        let stop = p8c_run(&ctx, "/perf stop");
        assert!(stop.starts_with("Stopped performance capture"), "{stop}");
        assert!(stop.contains("TPS 20.0"), "{stop}");
        assert!(stop.contains("MSPT mean 50.0 ms"), "{stop}");
        assert!(stop.contains("p50 50.0"), "{stop}");
        assert!(stop.contains("p95 50.0"), "{stop}");
        assert!(stop.contains("p99 50.0"), "{stop}");
        assert!(stop.contains("max 50.0 ms"), "{stop}");
        assert!(stop.contains("Top chunk hotspots:"), "{stop}");
        assert!(stop.contains("Overworld chunk (3, -9)"), "{stop}");
        assert!(stop.contains("mobs"), "{stop}");
        assert_eq!(
            p8c_run(&ctx, "/perf stop"),
            "No performance capture is running"
        );
        assert_eq!(p8c_run(&ctx, "/perf x"), "usage: /perf start|stop");
    }

    // #148 /publish — vanilla argument order, port bound to 1024..=65535,
    // metadata-only publish state, no socket.
    #[test]
    fn p8c_parity_publish_parses_validates_and_updates_state() {
        let _world = world_guard();
        let ctx = p8c_ctx(None);
        assert!(
            publish_state().lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "publish state must start empty for the test"
        );
        // Bare /publish uses the configured default port and gamemode.
        let default_gm = DEFAULT_GAME_MODE.swap(1, Ordering::Relaxed);
        let reply = p8c_run(&ctx, "/publish");
        assert_eq!(
            reply,
            format!("Local game hosted on port {}", publish_default_port())
        );
        {
            let state = publish_state().lock().unwrap_or_else(|e| e.into_inner());
            let active = state.expect("published");
            assert_eq!(active.port, publish_default_port());
            assert!(!active.allow_commands);
            assert_eq!(active.gamemode, 1, "default gamemode = creative");
        }
        DEFAULT_GAME_MODE.store(default_gm, Ordering::Relaxed);
        // A second publish reports the active port like Vanilla.
        assert_eq!(
            p8c_run(&ctx, "/publish"),
            format!("Local game already hosted on port {}", publish_default_port())
        );
        publish_state().lock().unwrap_or_else(|e| e.into_inner()).take();
        // Full argument form updates the metadata exactly.
        assert_eq!(
            p8c_run(&ctx, "/publish true adventure 30000"),
            "Local game hosted on port 30000"
        );
        {
            let state = publish_state().lock().unwrap_or_else(|e| e.into_inner());
            let active = state.expect("published");
            assert_eq!((active.port, active.allow_commands, active.gamemode), (30000, true, 2));
        }
        publish_state().lock().unwrap_or_else(|e| e.into_inner()).take();
        // Validation is fail-closed: bad bool, bad gamemode, out-of-range
        // ports, and too many arguments all refuse before any state change.
        assert_eq!(
            p8c_run(&ctx, "/publish maybe"),
            "Invalid allowCommands 'maybe'"
        );
        assert_eq!(
            p8c_run(&ctx, "/publish true hardcore"),
            "Invalid gamemode 'hardcore'"
        );
        for port in ["80", "1023", "65536", "abc", "-1"] {
            assert_eq!(
                p8c_run(&ctx, &format!("/publish false survival {port}")),
                format!("Invalid port '{port}'")
            );
        }
        // Boundary ports publish cleanly.
        for port in ["1024", "65535"] {
            assert_eq!(
                p8c_run(&ctx, &format!("/publish false survival {port}")),
                format!("Local game hosted on port {port}")
            );
            publish_state().lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        assert_eq!(
            p8c_run(&ctx, "/publish true survival 25000 extra"),
            "usage: /publish [<allowCommands>] [<gamemode>] [<port>]"
        );
        assert!(
            publish_state().lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "refused publishes must not leave state behind"
        );
    }

    // #225 /effect give — `infinite` duration and `hideParticles`.
    #[test]
    fn p8c_parity_effect_infinite_duration_and_hidden_particles() {
        let _world = world_guard();
        let (player, mut recv) = p8c_client(98_100, "p8c_fx", 0.5, 16.0, 0.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
        let ctx = p8c_ctx(None);
        // infinite: stored left never reaches zero; the wire duration is -1
        // and the flags byte keeps particles+icon (0x06).
        assert_eq!(
            p8c_run(&ctx, "/effect give p8c_fx speed infinite"),
            "Applied effect to 1 target(s)"
        );
        {
            let effs = player.effs.lock().unwrap_or_else(|e| e.into_inner());
            let speed = effs.iter().find(|e| e.id == 0).expect("speed stored");
            assert_eq!(speed.left, i32::MAX, "infinite never counts to zero");
        }
        let (id, payload) = p8c_read_frame(&mut recv).expect("effect frame");
        assert_eq!(id, 0x76);
        // [eid][effect][amp][duration = -1 as 5-byte varint][flags]
        assert_eq!(*payload.last().unwrap(), 0x06, "particles shown");
        assert!(
            payload
                .windows(5)
                .any(|w| w == [0xff, 0xff, 0xff, 0xff, 0x0f]),
            "wire duration -1 expected in {payload:?}"
        );
        // hidden particles flag the icon-only byte 0x04.
        assert_eq!(
            p8c_run(&ctx, "/effect give p8c_fx speed 60 2 true"),
            "Applied effect to 1 target(s)"
        );
        {
            let effs = player.effs.lock().unwrap_or_else(|e| e.into_inner());
            let speed = effs.iter().find(|e| e.id == 0).expect("speed stored");
            assert_eq!(speed.left, 60 * TICKS_PER_SECOND as i32);
            assert_eq!(speed.amp, 2);
        }
        let (id, payload) = p8c_read_frame(&mut recv).expect("effect frame");
        assert_eq!(id, 0x76);
        assert_eq!(*payload.last().unwrap(), 0x04, "particles hidden");
        // Validation refuses bad durations, amplifiers and flags outright.
        for (line, want) in [
            ("/effect give p8c_fx speed abc", "Invalid duration 'abc'"),
            ("/effect give p8c_fx speed 0", "Invalid duration '0'"),
            ("/effect give p8c_fx speed 1000001", "Invalid duration '1000001'"),
            ("/effect give p8c_fx speed 30 256", "Invalid amplifier '256'"),
            ("/effect give p8c_fx speed 30 1 maybe", "Invalid hideParticles 'maybe'"),
        ] {
            assert_eq!(p8c_run(&ctx, line), want, "{line}");
        }
        player.effs.lock().unwrap_or_else(|e| e.into_inner()).clear();
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != 98_100);
    }

    // #225 /damage — type validation and the at/by/from source clauses.
    #[test]
    fn p8c_parity_damage_type_and_source_clauses() {
        let _world = world_guard();
        let (player, _recv) = p8c_client(98_200, "p8c_dmg", 0.5, 16.0, 0.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set("p8c_dmg", 4, false);
        let ctx = p8c_ctx(Some(player.clone()));
        let hp0 = *player.hp.lock().unwrap_or_else(|e| e.into_inner());
        // Bare and typed forms apply the same physical damage.
        assert_eq!(
            p8c_run(&ctx, "/damage @s 5"),
            "Applied 5 damage to 1 target(s)"
        );
        let hp1 = *player.hp.lock().unwrap_or_else(|e| e.into_inner());
        assert!(hp1 < hp0, "bare damage must land: {hp0} -> {hp1}");
        assert_eq!(
            p8c_run(&ctx, "/damage @s 3 minecraft:generic"),
            "Applied 3 damage to 1 target(s)"
        );
        // Registry lookup: unknown types refuse before any damage lands.
        let hp2 = *player.hp.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            p8c_run(&ctx, "/damage @s 9 not_a_type"),
            "Invalid damage type 'not_a_type'"
        );
        assert_eq!(
            *player.hp.lock().unwrap_or_else(|e| e.into_inner()),
            hp2,
            "invalid type must not damage"
        );
        // at <pos> accepts vanilla coordinates including ~.
        assert_eq!(
            p8c_run(&ctx, "/damage @s 2 fall at ~ ~ ~"),
            "Applied 2 damage to 1 target(s)"
        );
        assert_eq!(
            p8c_run(&ctx, "/damage @s 2 magic at 10 64 -20"),
            "Applied 2 damage to 1 target(s)"
        );
        // by <entity> [from <cause>] each resolve exactly one entity.
        assert_eq!(
            p8c_run(&ctx, "/damage @s 2 mob_attack by @s"),
            "Applied 2 damage to 1 target(s)"
        );
        assert_eq!(
            p8c_run(&ctx, "/damage @s 2 arrow by @s from @s"),
            "Applied 2 damage to 1 target(s)"
        );
        assert_eq!(
            p8c_run(&ctx, "/damage @s 2 arrow by @e[type=!player]"),
            "Expected a single entity"
        );
        // Missing amount, bad amount and malformed tails fail closed.
        assert_eq!(
            p8c_run(&ctx, "/damage @s"),
            "usage: /damage <target> <amount> [<damageType>] [at <pos>|by <entity> [from <cause>]]"
        );
        assert_eq!(
            p8c_run(&ctx, "/damage @s nope"),
            "Invalid damage amount 'nope'"
        );
        assert_eq!(
            p8c_run(&ctx, "/damage @s -5"),
            "Invalid damage amount '-5'"
        );
        assert_eq!(
            p8c_run(&ctx, "/damage @s 5 at 1 2 3"),
            "usage: /damage <target> <amount> [<damageType>] [at <pos>|by <entity> [from <cause>]]"
        );
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != 98_200);
        p8c_deop("p8c_dmg");
    }

    // #225 /attribute — get, base get/set and the modifier verbs.
    #[test]
    fn p8c_parity_attribute_get_base_and_modifier_cycle() {
        let _world = world_guard();
        let (player, _recv) = p8c_client(98_300, "p8c_attr", 0.5, 16.0, 0.5);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
        let ctx = p8c_ctx(None);
        let attr = "minecraft:generic.max_health";
        // get reports the effective value, base get the stored base.
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} get")),
            format!("Value of attribute {attr} for entity p8c_attr is 20")
        );
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} base get")),
            format!("Base value of attribute {attr} for entity p8c_attr is 20")
        );
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} base set 30")),
            format!("Set base value of attribute {attr} for entity p8c_attr to 30")
        );
        // modifier add — Vanilla's three operations feed calculateValue:
        // (base + add) then multiply_base on that, multiply on the total.
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier add spine:boost 2 add")
            ),
            format!("Added modifier spine:boost to attribute {attr} for entity p8c_attr")
        );
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier add spine:boost 2 add")
            ),
            format!("Modifier spine:boost is already present on attribute {attr} for entity p8c_attr")
        );
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} get")),
            format!("Value of attribute {attr} for entity p8c_attr is 32")
        );
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier add spine:scale 0.5 multiply_base")
            ),
            format!("Added modifier spine:scale to attribute {attr} for entity p8c_attr")
        );
        // (30 + 2) + (30 + 2) * 0.5 = 48.
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} get")),
            format!("Value of attribute {attr} for entity p8c_attr is 48")
        );
        // base get still reports the untouched base.
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} base get")),
            format!("Base value of attribute {attr} for entity p8c_attr is 30")
        );
        // modifier value get reports the stored amount, with an optional scale.
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier value get spine:boost")
            ),
            format!("Value of modifier spine:boost on attribute {attr} for entity p8c_attr is 2")
        );
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier value get spine:scale 2")
            ),
            format!("Value of modifier spine:scale on attribute {attr} for entity p8c_attr is 1")
        );
        // remove unrolls one modifier at a time; a second remove errors.
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier remove spine:scale")
            ),
            format!("Removed modifier spine:scale from attribute {attr} for entity p8c_attr")
        );
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute p8c_attr {attr} get")),
            format!("Value of attribute {attr} for entity p8c_attr is 32")
        );
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier remove spine:scale")
            ),
            format!("Attribute {attr} on entity p8c_attr has no modifier spine:scale")
        );
        // Invalid modifier ids and values fail closed; get on a missing
        // modifier errors too.
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier add BAD_ID 1 add")
            ),
            "Invalid modifier id 'BAD_ID'"
        );
        assert_eq!(
            p8c_run(
                &ctx,
                &format!("/attribute p8c_attr {attr} modifier value get spine:nope")
            ),
            format!("Attribute {attr} on entity p8c_attr has no modifier spine:nope")
        );
        // The target is a single-entity argument, not a multi-selector.
        assert_eq!(
            p8c_run(&ctx, &format!("/attribute @e[type=!player] {attr} get")),
            "Expected a single entity"
        );
        attr_modifiers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("p:p8c_attr");
        attrs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("p:p8c_attr");
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != 98_300);
    }

    // #225 /tag verbs + /clear maxCount=0 — the already-live semantics stay
    // pinned through the P8c handlers.
    #[test]
    fn p8c_parity_tag_and_clear_subcommands() {
        let _world = world_guard();
        let (player, _recv) = p8c_client(98_400, "p8c_tag", 0.5, 16.0, 0.5);
        let stone = name_to_item("stone").unwrap();
        player
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(0, (stone, 8, Vec::new()));
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(player.clone());
        let ctx = p8c_ctx(None);
        // tag add | list | remove on a live player target.
        assert_eq!(p8c_run(&ctx, "/tag p8c_tag add p8c_mark"), "tagged 1 target(s)");
        let listed = p8c_run(&ctx, "/tag p8c_tag list");
        assert!(listed.contains("p8c_tag"), "{listed}");
        assert!(listed.contains("p8c_mark"), "{listed}");
        assert_eq!(
            p8c_run(&ctx, "/tag p8c_tag remove p8c_mark"),
            "untagged 1 target(s)"
        );
        assert_eq!(p8c_run(&ctx, "/tag p8c_tag"), "usage: /tag <target> add|remove|list [tag]");
        // clear maxCount=0 counts without removing; a real clear removes.
        assert_eq!(p8c_run(&ctx, "/clear p8c_tag stone 0"), "Cleared 8 item(s)");
        assert_eq!(
            player.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&0).map(|s| s.1),
            Some(8),
            "maxCount=0 must not remove items"
        );
        assert_eq!(p8c_run(&ctx, "/clear p8c_tag stone 3"), "Cleared 3 item(s)");
        assert_eq!(
            player.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&0).map(|s| s.1),
            Some(5)
        );
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != 98_400);
        tags().lock().unwrap_or_else(|e| e.into_inner()).remove("p:p8c_tag");
    }
}

// ------------------------------------------------------- policy_cmd_tests ---

#[cfg(test)]
mod policy_cmd_tests {
    use super::*;

    fn policy_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn policy_client(name: &str) -> (Arc<Client>, std::net::TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.name = name.into();
        (Arc::new(client), receiver)
    }

    fn policy_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn policy_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    fn policy_op_ctx(name: &str, level: u8) -> (CmdCtx, std::net::TcpStream) {
        if level > 0 {
            ops_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set(name, level, false);
        }
        let (client, receiver) = policy_client(name);
        (policy_ctx(Some(client)), receiver)
    }

    fn policy_deop(name: &str) {
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    fn policy_reset() {
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset();
    }

    #[test]
    fn policy_cmd_defaults_are_vanilla_permissive_and_opt_in() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        {
            let policy = live_server_policy()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert_eq!(policy.minimap_mode, "unrestricted");
            assert!(policy.allow_cave_map);
            assert!(policy.allow_mob_radar);
            assert_eq!(policy.player_radar_scope, "all");
            assert!(policy.allow_waypoint_teleport);
            assert_eq!(policy.mine_all, "off");
            assert_eq!(policy.cut_all, "off");
            assert_eq!(policy.xp_merge_mode, "vanilla");
            assert_eq!(policy.max_frame_mib, 2);
            assert!(policy.pet_friendly_fire);
            assert!(policy.party_friendly_fire);
            assert!(!policy.death_slot_restore);
            assert_eq!(policy.list().len(), 12);
        }
        let ctx = policy_ctx(None);
        for (key, value) in [
            ("minimap_mode", "unrestricted"),
            ("allow_cave_map", "true"),
            ("allow_mob_radar", "true"),
            ("player_radar_scope", "all"),
            ("allow_waypoint_teleport", "true"),
            ("mine_all", "off"),
            ("cut_all", "off"),
            ("xp_merge_mode", "vanilla"),
            ("max_frame_mib", "2"),
            ("pet_friendly_fire", "true"),
            ("party_friendly_fire", "true"),
            ("death_slot_restore", "false"),
        ] {
            assert_eq!(
                policy_run(&ctx, &format!("/megu policy get {key}")),
                format!("{key} = {value}")
            );
        }
        let list = policy_run(&ctx, "/megu policy list");
        for key in [
            "minimap_mode",
            "allow_cave_map",
            "allow_mob_radar",
            "player_radar_scope",
            "allow_waypoint_teleport",
            "mine_all",
            "cut_all",
            "xp_merge_mode",
            "max_frame_mib",
            "pet_friendly_fire",
            "party_friendly_fire",
            "death_slot_restore",
        ] {
            assert!(list.contains(&format!("{key} = ")), "{list}");
        }
        policy_reset();
    }

    #[test]
    fn policy_cmd_op_level_four_sets_gets_lists_and_resets() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        let ctx = policy_ctx(None);
        for (line, want) in [
            (
                "/megu policy set cut_all sneak_only",
                "cut_all = sneak_only",
            ),
            (
                "/megu policy set minimap_mode fair_play",
                "minimap_mode = fair_play",
            ),
            (
                "/megu policy set player_radar_scope party_only",
                "player_radar_scope = party_only",
            ),
            ("/megu policy set max_frame_mib 8", "max_frame_mib = 8"),
            (
                "/megu policy set allow_mob_radar false",
                "allow_mob_radar = false",
            ),
            (
                "/megu policy set xp_merge_mode aggressive",
                "xp_merge_mode = aggressive",
            ),
        ] {
            assert_eq!(policy_run(&ctx, line), want, "{line}");
        }
        for (key, value) in [
            ("cut_all", "sneak_only"),
            ("minimap_mode", "fair_play"),
            ("player_radar_scope", "party_only"),
            ("max_frame_mib", "8"),
            ("allow_mob_radar", "false"),
            ("xp_merge_mode", "aggressive"),
        ] {
            assert_eq!(
                policy_run(&ctx, &format!("/megu policy get {key}")),
                format!("{key} = {value}")
            );
        }
        let list = policy_run(&ctx, "/megu policy list");
        assert!(list.contains("cut_all = sneak_only"), "{list}");
        assert!(list.contains("max_frame_mib = 8"), "{list}");
        assert_eq!(
            policy_run(&ctx, "/megu policy reset"),
            "All server policies reset to defaults"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get cut_all"),
            "cut_all = off"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get max_frame_mib"),
            "max_frame_mib = 2"
        );
        // An ops.json level-4 player has the same access as the console.
        let (op4, _recv) = policy_op_ctx("policy_cmd_l4", 4);
        assert_eq!(
            policy_run(&op4, "/megu policy set mine_all always"),
            "mine_all = always"
        );
        policy_deop("policy_cmd_l4");
        policy_reset();
    }

    #[test]
    fn policy_cmd_refuses_levels_below_four_and_level_two_functions() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        for (name, level) in [
            ("policy_cmd_l0", 0u8),
            ("policy_cmd_l1", 1),
            ("policy_cmd_l2", 2),
            ("policy_cmd_l3", 3),
        ] {
            let (ctx, _recv) = policy_op_ctx(name, level);
            for msg in [
                "/megu policy list",
                "/megu policy get cut_all",
                "/megu policy set cut_all sneak_only",
                "/megu policy reset",
            ] {
                assert_eq!(
                    policy_run(&ctx, msg),
                    "You do not have permission to run that command",
                    "level-{level} source must be refused {msg}"
                );
            }
            policy_deop(name);
        }
        for level in 0..4u8 {
            assert!(
                command_tree_at_level(level).child("megu").is_none(),
                "megu must not be advertised at level {level}"
            );
            let error = live_brigadier_tree()
                .parse_command_for_level("/megu policy list", level)
                .expect_err("megu must be permission-denied below level 4");
            assert_eq!(
                error.kind(),
                brigadier::BrigadierErrorKind::PermissionDenied
            );
        }
        assert!(command_tree_at_level(4).child("megu").is_some());
        live_brigadier_tree()
            .parse_command_for_level("/megu policy list", 4)
            .expect("level 4 parses /megu policy list");
        // A level-2 `.mcfunction` frame re-enters run_command_ctx with the
        // same source: run_function_lines is that path, not an approximation.
        let (ctx, _recv) = policy_op_ctx("policy_cmd_fn2", 2);
        run_function_lines(
            &["megu policy set cut_all sneak_only".to_owned()],
            ctx.me.clone(),
            &mut None,
            0,
        );
        policy_deop("policy_cmd_fn2");
        let policy = live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(policy.cut_all, "off", "level-2 function mutated policy");
        drop(policy);
        policy_reset();
    }

    #[test]
    fn policy_cmd_invalid_key_or_value_fails_closed_without_mutating() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        let ctx = policy_ctx(None);
        assert_eq!(
            policy_run(&ctx, "/megu policy set bogus off"),
            "Unknown policy 'bogus'"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get bogus"),
            "Unknown policy 'bogus'"
        );
        for (line, want) in [
            (
                "/megu policy set max_frame_mib 64",
                "Invalid value '64' for policy 'max_frame_mib'",
            ),
            (
                "/megu policy set max_frame_mib 1",
                "Invalid value '1' for policy 'max_frame_mib'",
            ),
            (
                "/megu policy set max_frame_mib abc",
                "Invalid value 'abc' for policy 'max_frame_mib'",
            ),
            (
                "/megu policy set minimap_mode bogus",
                "Invalid value 'bogus' for policy 'minimap_mode'",
            ),
            (
                "/megu policy set allow_cave_map maybe",
                "Invalid value 'maybe' for policy 'allow_cave_map'",
            ),
            (
                "/megu policy set mine_all sometimes",
                "Invalid value 'sometimes' for policy 'mine_all'",
            ),
        ] {
            assert_eq!(policy_run(&ctx, line), want, "{line}");
        }
        // A rejected set leaves the previous valid value in place.
        assert_eq!(
            policy_run(&ctx, "/megu policy set cut_all sneak_only"),
            "cut_all = sneak_only"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy set cut_all bogus"),
            "Invalid value 'bogus' for policy 'cut_all'"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get cut_all"),
            "cut_all = sneak_only"
        );
        for line in [
            "/megu",
            "/megu policy",
            "/megu policy get",
            "/megu policy set cut_all",
            "/megu policy reset now",
        ] {
            assert_eq!(policy_run(&ctx, line), POLICY_USAGE, "{line}");
        }
        policy_reset();
    }

    #[test]
    fn ops_sre_policy_friendly_fire_keys_default_true_opt_in_and_reset() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        let ctx = policy_ctx(None);
        // Vanilla-faithful defaults: friendly fire allowed for pets and party.
        for key in ["pet_friendly_fire", "party_friendly_fire"] {
            assert_eq!(
                policy_run(&ctx, &format!("/megu policy get {key}")),
                format!("{key} = true")
            );
        }
        let list = policy_run(&ctx, "/megu policy list");
        assert!(list.contains("pet_friendly_fire = true"), "{list}");
        assert!(list.contains("party_friendly_fire = true"), "{list}");
        // Strict opt-in: only an explicit `false` arms the protection.
        assert_eq!(
            policy_run(&ctx, "/megu policy set pet_friendly_fire false"),
            "pet_friendly_fire = false"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy set party_friendly_fire false"),
            "party_friendly_fire = false"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get pet_friendly_fire"),
            "pet_friendly_fire = false"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get party_friendly_fire"),
            "party_friendly_fire = false"
        );
        // A rejected value fails closed and leaves the stored flag alone.
        assert_eq!(
            policy_run(&ctx, "/megu policy set pet_friendly_fire maybe"),
            "Invalid value 'maybe' for policy 'pet_friendly_fire'"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy set party_friendly_fire 0"),
            "Invalid value '0' for policy 'party_friendly_fire'"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy get pet_friendly_fire"),
            "pet_friendly_fire = false"
        );
        assert_eq!(
            policy_run(&ctx, "/megu policy reset"),
            "All server policies reset to defaults"
        );
        for key in ["pet_friendly_fire", "party_friendly_fire"] {
            assert_eq!(
                policy_run(&ctx, &format!("/megu policy get {key}")),
                format!("{key} = true")
            );
        }
    }

    #[test]
    fn ops_sre_friendly_fire_policy_refused_below_op_level_four() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        for level in 0..4u8 {
            let (ctx, _recv) = policy_op_ctx(&format!("ops_sre_ff_l{level}"), level);
            for msg in [
                "/megu policy set pet_friendly_fire false",
                "/megu policy set party_friendly_fire false",
                "/megu policy get pet_friendly_fire",
            ] {
                assert_eq!(
                    policy_run(&ctx, msg),
                    "You do not have permission to run that command",
                    "level-{level} source must be refused {msg}"
                );
            }
            policy_deop(&format!("ops_sre_ff_l{level}"));
        }
        // The stored flags were never touched by the refused commands.
        let policy = live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(policy.pet_friendly_fire);
        assert!(policy.party_friendly_fire);
        drop(policy);
        policy_reset();
    }

    #[test]
    fn ops_sre_pet_damage_gate_blocks_owner_and_ally_only_when_disabled() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        let owner = [0x0Au8; 16];
        let other = [0x0Bu8; 16];
        let pet = Some(owner);
        // Default `true` is Vanilla: every pairing's hit lands.
        assert!(should_allow_pet_damage(owner, pet, false));
        assert!(should_allow_pet_damage(owner, pet, true));
        assert!(should_allow_pet_damage(other, pet, false));
        assert!(should_allow_pet_damage(other, pet, true));
        assert!(should_allow_pet_damage(other, None, false));
        // Opt-in `false`: owner and party-ally hits on the pet are blocked,
        // hostile mobs and unrelated players still deal normal damage.
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pet_friendly_fire = false;
        assert!(!should_allow_pet_damage(owner, pet, false), "owner self-hit blocked");
        assert!(!should_allow_pet_damage(other, pet, true), "party-ally hit blocked");
        assert!(should_allow_pet_damage(other, pet, false), "non-ally hit allowed");
        assert!(
            should_allow_pet_damage([0xEEu8; 16], pet, false),
            "a mob uuid never equals the owner's player uuid"
        );
        assert!(
            should_allow_pet_damage(other, None, false),
            "an untamed pet is wild"
        );
        assert!(
            !should_allow_pet_damage(other, None, true),
            "party allies are still protected even on untamed pets"
        );
        // Re-enabling restores Vanilla instantly.
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pet_friendly_fire = true;
        assert!(should_allow_pet_damage(owner, pet, false));
        policy_reset();
    }

    #[test]
    fn ops_sre_party_pvp_gate_blocks_allies_only_when_disabled() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        // Default `true`: allies and non-allies both take hits (Vanilla).
        assert!(should_allow_party_pvp(true));
        assert!(should_allow_party_pvp(false));
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .party_friendly_fire = false;
        assert!(!should_allow_party_pvp(true), "ally hit blocked");
        assert!(should_allow_party_pvp(false), "non-ally hit allowed");
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .party_friendly_fire = true;
        assert!(should_allow_party_pvp(true));
        policy_reset();
    }

    /// Save/restore for the world fixtures the live `drop_tick` reads —
    /// clients, drops, the death-slot hint table and the hot region — plus
    /// the `DEATH_SLOT_RESTORE` atomic the policy now drives. The death
    /// lane's own `DeathDropWorld` guard is private to its test module, so
    /// this mirrors it for the policy lane.
    struct GapPolicyDeathWorld {
        clients: Vec<Arc<Client>>,
        drops: Vec<Drop>,
        hints: DeathSlotHintTable,
        region: RegionSets,
        restore_flag: bool,
    }
    impl GapPolicyDeathWorld {
        fn take() -> Self {
            Self {
                clients: std::mem::take(
                    &mut *clients().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                drops: std::mem::take(
                    &mut *drops().lock().unwrap_or_else(|e| e.into_inner()),
                ),
                hints: std::mem::replace(
                    &mut *death_slot_hints()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                    DeathSlotHintTable {
                        map: HashMap::new(),
                        order: std::collections::VecDeque::new(),
                    },
                ),
                region: test_region(),
                restore_flag: death_slot_restore_enabled(),
            }
        }
    }
    impl std::ops::Drop for GapPolicyDeathWorld {
        fn drop(&mut self) {
            *clients().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.clients);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.drops);
            *death_slot_hints()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                std::mem::replace(
                    &mut self.hints,
                    DeathSlotHintTable {
                        map: HashMap::new(),
                        order: std::collections::VecDeque::new(),
                    },
                );
            set_test_region(self.region.0.clone(), self.region.1.clone());
            DEATH_SLOT_RESTORE.store(self.restore_flag, Ordering::Relaxed);
        }
    }

    /// A live client standing on the origin drop spot, wired the way the
    /// death lane's `live_client` is: pushed into `clients()` so `drop_tick`
    /// can see it.
    fn gap_live_client(name: &str) -> Arc<Client> {
        let (client, _receiver) = policy_client(name);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(client.clone());
        client
    }

    /// Spawn an aged death drop owned by `owner` hinted to `slot`, exactly
    /// the entry point `drop_tick` sees after 30+ live ticks.
    fn gap_death_drop(item: i64, cnt: u8, owner: &str, slot: i16) {
        let eid = spawn_death_drop((0.0, 70.0, 0.0), (item, cnt, Vec::new()), owner, slot);
        let mut ds = drops().lock().unwrap_or_else(|e| e.into_inner());
        ds.iter_mut().find(|d| d.eid == eid).unwrap().age = 100;
    }

    /// Vanilla-faithful default: the policy key exists everywhere at `false`
    /// and the live pickup flag agrees — the feature is strictly opt-in.
    #[test]
    fn gap_qol_policy_death_slot_restore_defaults_off() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        set_death_slot_restore(false);
        let ctx = policy_ctx(None);
        assert_eq!(
            policy_run(&ctx, "/megu policy get death_slot_restore"),
            "death_slot_restore = false"
        );
        let list = policy_run(&ctx, "/megu policy list");
        assert!(list.contains("death_slot_restore = false"), "{list}");
        assert!(
            !live_server_policy()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .death_slot_restore
        );
        assert!(!death_slot_restore_enabled(), "live flag on by default");
        policy_reset();
    }

    /// OP levels 0..3 are refused the whole `/megu policy` surface; a refused
    /// `set` must never reach the stored policy or the live flag.
    #[test]
    fn gap_qol_policy_death_slot_restore_refused_below_op_four() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        set_death_slot_restore(false);
        for level in 0..4u8 {
            let name = format!("gap_qol_dsr_l{level}");
            let (ctx, _recv) = policy_op_ctx(&name, level);
            for msg in [
                "/megu policy set death_slot_restore true",
                "/megu policy get death_slot_restore",
                "/megu policy list",
            ] {
                assert_eq!(
                    policy_run(&ctx, msg),
                    P8C_DENIED,
                    "level-{level} source must be refused {msg}"
                );
            }
            policy_deop(&name);
        }
        assert!(
            !live_server_policy()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .death_slot_restore,
            "a refused set mutated the stored policy"
        );
        assert!(!death_slot_restore_enabled(), "a refused set armed the live flag");
        policy_reset();
    }

    /// OP level 4 `set true` lands on the very next pickup: a death-scattered
    /// sword hinted to slot 36 comes home to slot 36 through the real
    /// `drop_tick`, and its hint is retired with the entity.
    #[test]
    fn gap_qol_policy_death_slot_restore_set_true_restores_pickup_slot() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        set_death_slot_restore(false);
        let _snapshot = GapPolicyDeathWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let (op4, _recv) = policy_op_ctx("gap_qol_dsr_op", 4);
        assert_eq!(
            policy_run(&op4, "/megu policy set death_slot_restore true"),
            "death_slot_restore = true"
        );
        assert!(
            death_slot_restore_enabled(),
            "policy set did not reach the live flag"
        );
        policy_deop("gap_qol_dsr_op");

        let sword = name_to_item("diamond_sword").unwrap();
        let cobble = name_to_item("cobblestone").unwrap();
        let client = gap_live_client("gap_alice");
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (cobble, 32, Vec::new()));
        gap_death_drop(sword, 1, "gap_alice", 36);
        drop_tick();
        let inv = client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            inv.get(&36),
            Some(&(sword, 1, Vec::new())),
            "sword did not restore to its original slot"
        );
        assert_eq!(
            inv.get(&37),
            Some(&(cobble, 32, Vec::new())),
            "the collider was lost instead of relocated"
        );
        assert!(drops().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        policy_reset();
        set_death_slot_restore(false);
    }

    /// `reset` is the way back to Vanilla: the stored key and the live flag
    /// both return to `false`, and the next pickup takes the vanilla
    /// first-free-slot path even with a live hint.
    #[test]
    fn gap_qol_policy_death_slot_restore_reset_returns_to_vanilla() {
        let _world = world_guard();
        let _serial = policy_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        policy_reset();
        set_death_slot_restore(false);
        let _snapshot = GapPolicyDeathWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let (op4, _recv) = policy_op_ctx("gap_qol_dsr_op2", 4);
        assert_eq!(
            policy_run(&op4, "/megu policy set death_slot_restore true"),
            "death_slot_restore = true"
        );
        assert!(death_slot_restore_enabled());
        assert_eq!(
            policy_run(&op4, "/megu policy reset"),
            "All server policies reset to defaults"
        );
        assert_eq!(
            policy_run(&op4, "/megu policy get death_slot_restore"),
            "death_slot_restore = false"
        );
        assert!(
            !death_slot_restore_enabled(),
            "policy reset did not clear the live flag"
        );
        policy_deop("gap_qol_dsr_op2");

        // Vanilla pickup resumes: the hinted slot 20 is ignored and the
        // sword lands in the first free hotbar slot.
        let sword = name_to_item("diamond_sword").unwrap();
        let client = gap_live_client("gap_bob");
        gap_death_drop(sword, 1, "gap_bob", 20);
        drop_tick();
        let inv = client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            inv,
            HashMap::from([(36i16, (sword, 1, Vec::new()))]),
            "hint applied while the policy was reset"
        );
        policy_reset();
        set_death_slot_restore(false);
    }
}

// ------------------------------------------------------------- tests (VANILLA-CMDBLK)

#[cfg(test)]
mod vanilla_cmdblk_tests {
    use super::*;

    fn clear_cmd_state() {
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        command_minecarts()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn put_block(
        dim: DimId,
        pos: (i32, i32, i32),
        mode: CommandBlockMode,
        facing: CommandBlockFacing,
        command: &str,
        flags: (bool, bool, bool),
    ) {
        let mut rec = CommandBlockRecord::new(dim, pos);
        rec.mode = mode;
        rec.facing = facing;
        rec.command = command.to_owned();
        (rec.conditional, rec.automatic, rec.powered) = flags;
        upsert_command_block(rec);
    }

    fn block_rec(dim: DimId, pos: (i32, i32, i32)) -> CommandBlockRecord {
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            [ &(dim, pos.0, pos.1, pos.2)]
        .clone()
    }

    fn world_time() -> u64 {
        WORLD_TIME.load(Ordering::Relaxed)
    }

    fn update(
        pos: (i32, i32, i32),
        command: &str,
        mode: CommandBlockMode,
        flags: (bool, bool, bool),
    ) -> SetCommandBlockUpdate {
        SetCommandBlockUpdate {
            pos,
            command: command.to_owned(),
            mode,
            track_output: flags.0,
            conditional: flags.1,
            automatic: flags.2,
        }
    }

    #[test]
    fn vanilla_cmdblk_impulse_runs_once_per_rising_edge() {
        let _world = world_guard();
        clear_cmd_state();
        let (dim, pos) = (DimId::Overworld, (10, 64, 10));
        put_block(dim, pos, CommandBlockMode::Redstone, CommandBlockFacing::Up, "time add 7", (false, false, false));
        let t0 = world_time();
        command_block_tick();
        assert_eq!(world_time(), t0, "unpowered impulse must not run");
        set_command_block_powered(dim, pos, true);
        command_block_tick();
        assert_eq!(world_time(), t0 + 7, "rising edge runs once");
        command_block_tick();
        command_block_tick();
        assert_eq!(world_time(), t0 + 7, "held power is not a new edge");
        set_command_block_powered(dim, pos, false);
        command_block_tick();
        assert_eq!(world_time(), t0 + 7, "falling edge runs nothing");
        set_command_block_powered(dim, pos, true);
        command_block_tick();
        assert_eq!(world_time(), t0 + 14, "re-powering is a new edge");
        assert_eq!(block_rec(dim, pos).success_count, 1);
    }

    #[test]
    fn vanilla_cmdblk_repeating_runs_every_tick_when_always_active() {
        let _world = world_guard();
        clear_cmd_state();
        let (dim, pos) = (DimId::Overworld, (11, 64, 10));
        put_block(dim, pos, CommandBlockMode::Auto, CommandBlockFacing::Up, "time add 2", (false, true, false));
        let t0 = world_time();
        for _ in 0..3 {
            command_block_tick();
        }
        assert_eq!(world_time(), t0 + 6, "always-active repeating runs each tick");
        // Powered (but not automatic) repeating blocks run each tick too.
        put_block(dim, (11, 64, 11), CommandBlockMode::Auto, CommandBlockFacing::Up, "time add 3", (false, false, true));
        command_block_tick();
        assert_eq!(world_time(), t0 + 6 + 2 + 3);
        set_command_block_powered(dim, (11, 64, 11), false);
        command_block_tick();
        assert_eq!(world_time(), t0 + 6 + 2 + 3 + 2, "unpowered repeating idles");
    }

    #[test]
    fn vanilla_cmdblk_chain_runs_unconditional_in_facing_order() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::Overworld;
        put_block(dim, (0, 64, 20), CommandBlockMode::Redstone, CommandBlockFacing::East, "time add 1", (false, false, true));
        put_block(dim, (1, 64, 20), CommandBlockMode::Sequence, CommandBlockFacing::East, "time add 1", (false, true, false));
        put_block(dim, (2, 64, 20), CommandBlockMode::Sequence, CommandBlockFacing::East, "time add 1", (false, true, false));
        let t0 = world_time();
        command_block_tick();
        assert_eq!(world_time(), t0 + 3, "trigger + two chain blocks");
        for x in 0..=2 {
            assert_eq!(block_rec(dim, (x, 64, 20)).success_count, 1);
        }
    }

    #[test]
    fn vanilla_cmdblk_chain_conditional_gates_on_predecessor_success() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::Overworld;
        let chain1 = (1, 64, 30);
        let chain2 = (2, 64, 30);
        put_block(dim, (0, 64, 30), CommandBlockMode::Redstone, CommandBlockFacing::East, "time add 1", (false, false, true));
        put_block(dim, chain1, CommandBlockMode::Sequence, CommandBlockFacing::East, "nonexistent_root_xyz", (false, true, false));
        put_block(dim, chain2, CommandBlockMode::Sequence, CommandBlockFacing::East, "time add 3", (true, true, false));
        let t0 = world_time();
        command_block_tick();
        assert_eq!(world_time(), t0 + 1, "only the trigger ran");
        assert_eq!(block_rec(dim, chain1).success_count, 0, "unknown command fails");
        assert_eq!(block_rec(dim, chain2).success_count, 0);
        assert!(!block_rec(dim, chain2).condition_met, "condition failed");
        // Same chain, succeeding predecessor: the conditional block runs.
        set_command_block_powered(dim, (0, 64, 30), false);
        command_block_tick();
        {
            let mut map = live_command_blocks().lock().unwrap_or_else(|e| e.into_inner());
            map.get_mut(&(dim, chain1.0, chain1.1, chain1.2)).unwrap().command = "time add 2".to_owned();
        }
        set_command_block_powered(dim, (0, 64, 30), true);
        command_block_tick();
        assert_eq!(world_time(), t0 + 1 + 1 + 2 + 3);
        assert!(block_rec(dim, chain2).condition_met);
        assert_eq!(block_rec(dim, chain2).success_count, 1);
        // An unpowered (needs-redstone) chain block is skipped but the trigger
        // still propagates past it, matching vanilla chain propagation.
        let dim2 = DimId::Nether;
        put_block(dim2, (0, 64, 30), CommandBlockMode::Redstone, CommandBlockFacing::East, "time add 1", (false, false, true));
        put_block(dim2, (1, 64, 30), CommandBlockMode::Sequence, CommandBlockFacing::East, "time add 10", (false, false, false));
        put_block(dim2, (2, 64, 30), CommandBlockMode::Sequence, CommandBlockFacing::East, "time add 4", (false, true, false));
        let t1 = world_time();
        command_block_tick();
        assert_eq!(world_time(), t1 + 1 + 4, "unpowered link skipped, tail still ran");
    }

    #[test]
    fn vanilla_cmdblk_max_chain_length_bounds_execution_cycle() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::End;
        put_block(dim, (0, 64, 40), CommandBlockMode::Redstone, CommandBlockFacing::East, "time add 1", (false, false, true));
        put_block(dim, (1, 64, 40), CommandBlockMode::Sequence, CommandBlockFacing::East, "time add 1", (false, true, false));
        put_block(dim, (2, 64, 40), CommandBlockMode::Sequence, CommandBlockFacing::West, "time add 1", (false, true, false));
        {
            let mut rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            rules.insert("maxCommandChainLength", 5);
        }
        let t0 = world_time();
        command_block_tick();
        {
            let mut rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
            rules.insert("maxCommandChainLength", 65536);
        }
        // Trigger (1) + exactly 5 chained executions — the A<->B cycle is cut
        // by the gamerule, never allowed to spin.
        assert_eq!(world_time(), t0 + 6, "chain executions capped at maxCommandChainLength");
    }

    #[test]
    fn vanilla_cmdblk_runs_at_op_level_two_never_admin() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::Overworld;
        for (i, root) in [
            "stop", "op", "ban", "transfer", "profile", "debug", "jfr", "perf",
            "publish", "megu", "deop", "whitelist", "save-all", "tick",
        ]
        .iter()
        .enumerate()
        {
            let pos = (100 + i as i32, 64, 50);
            put_block(dim, pos, CommandBlockMode::Auto, CommandBlockFacing::Up, root, (false, true, false));
            command_block_tick();
            assert_eq!(
                block_rec(dim, pos).success_count,
                0,
                "/{root} must be refused from a command block"
            );
        }
        // ...but a level-2 command still runs — the source is level 2, not 0.
        let pos = (120, 64, 50);
        put_block(dim, pos, CommandBlockMode::Auto, CommandBlockFacing::Up, "time add 5", (false, true, false));
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(dim, pos.0, pos.1, pos.2))
            .unwrap()
            .track_output = true;
        let t0 = world_time();
        command_block_tick();
        assert_eq!(world_time(), t0 + 5);
        let rec = block_rec(dim, pos);
        assert_eq!(rec.success_count, 1);
        assert!(rec.last_output.contains("Set the time"), "track_output captured");
    }

    #[test]
    fn vanilla_cmdblk_edit_requires_op_level_two_and_creative() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::Overworld;
        let pkt = || update((1, 2, 3), "time add 1", CommandBlockMode::Auto, (true, false, true));
        assert!(apply_set_command_block(0, 1, true, dim, pkt()).is_err(), "non-OP refused");
        assert!(apply_set_command_block(1, 1, true, dim, pkt()).is_err(), "level 1 refused");
        assert!(apply_set_command_block(2, 0, true, dim, pkt()).is_err(), "survival refused");
        assert!(apply_set_command_block(2, 2, true, dim, pkt()).is_err(), "adventure refused");
        assert!(apply_set_command_block(2, 3, true, dim, pkt()).is_err(), "spectator refused");
        assert!(apply_set_command_block(2, 1, false, dim, pkt()).is_err(), "toggle off refused");
        assert!(apply_set_command_block(2, 1, true, dim, pkt()).is_ok(), "OP2 creative allowed");
        let rec = block_rec(dim, (1, 2, 3));
        assert_eq!(rec.mode, CommandBlockMode::Auto);
        assert_eq!(rec.command, "time add 1");
        assert!(rec.track_output && rec.automatic && !rec.conditional);
        // Minecart edits sit under the identical gate.
        let cart = SetCommandMinecartUpdate { entity_id: 77, command: "say hi".into(), track_output: true };
        assert!(apply_set_command_minecart(0, 1, true, dim, cart.clone()).is_err());
        assert!(apply_set_command_minecart(2, 0, true, dim, cart.clone()).is_err());
        assert!(apply_set_command_minecart(2, 1, true, dim, cart.clone()).is_ok());
        let carts = command_minecarts().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(carts[&77].command, "say hi");
        assert!(carts[&77].track_output);
    }

    fn wire_varint(v: i64, out: &mut Vec<u8>) {
        let mut v = v as u32;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn pack_block_pos(x: i32, y: i32, z: i32) -> i64 {
        ((x as i64 & 0x3ff_ffff) << 38) | ((z as i64 & 0x3ff_ffff) << 12) | (y as i64 & 0xfff)
    }

    #[test]
    fn vanilla_cmdblk_decode_set_command_block_wire() {
        let _world = world_guard();
        let mut body = Vec::new();
        body.extend_from_slice(&pack_block_pos(300, -64, -200).to_be_bytes());
        wire_varint(10, &mut body);
        body.extend_from_slice(b"time add 1");
        wire_varint(1, &mut body); // Auto (repeating)
        body.push(0x05); // track_output + automatic
        let pkt = decode_set_command_block_c2s(&body).expect("valid packet");
        assert_eq!(pkt.pos, (300, -64, -200));
        assert_eq!(pkt.command, "time add 1");
        assert_eq!(pkt.mode, CommandBlockMode::Auto);
        assert!(pkt.track_output && pkt.automatic && !pkt.conditional);
        // Chain mode + conditional flag decode too.
        let mut body2 = body.clone();
        *body2.last_mut().unwrap() = 0x02;
        let mode_pos = body2.len() - 2;
        body2[mode_pos] = 0x00;
        let pkt2 = decode_set_command_block_c2s(&body2).expect("chain packet");
        assert_eq!(pkt2.mode, CommandBlockMode::Sequence);
        assert!(pkt2.conditional && !pkt2.track_output && !pkt2.automatic);
        // Fail closed: truncation, bad mode, oversized command, trailing byte.
        assert!(decode_set_command_block_c2s(&body[..body.len() - 1]).is_none());
        assert!(decode_set_command_block_c2s(&body[..4]).is_none());
        let mut bad_mode = body.clone();
        bad_mode[mode_pos] = 3;
        assert!(decode_set_command_block_c2s(&bad_mode).is_none());
        let mut oversized = Vec::new();
        oversized.extend_from_slice(&pack_block_pos(0, 0, 0).to_be_bytes());
        wire_varint(32_501, &mut oversized);
        oversized.extend_from_slice(&vec![b'a'; 32_501]);
        oversized.push(0);
        oversized.push(0);
        assert!(decode_set_command_block_c2s(&oversized).is_none());
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(decode_set_command_block_c2s(&trailing).is_none());
        // Boundary: exactly 32_500 bytes still decodes.
        let mut boundary = Vec::new();
        boundary.extend_from_slice(&pack_block_pos(0, 0, 0).to_be_bytes());
        wire_varint(32_500, &mut boundary);
        boundary.extend_from_slice(&vec![b'a'; 32_500]);
        wire_varint(2, &mut boundary);
        boundary.push(0);
        assert!(decode_set_command_block_c2s(&boundary).is_some());
    }

    #[test]
    fn vanilla_cmdblk_decode_set_command_minecart_wire() {
        let _world = world_guard();
        let mut body = Vec::new();
        wire_varint(42, &mut body);
        wire_varint(6, &mut body);
        body.extend_from_slice(b"say hi");
        body.push(1);
        let pkt = decode_set_command_minecart_c2s(&body).expect("valid minecart packet");
        assert_eq!(pkt.entity_id, 42);
        assert_eq!(pkt.command, "say hi");
        assert!(pkt.track_output);
        assert!(decode_set_command_minecart_c2s(&body[..body.len() - 1]).is_none());
        let mut bad_bool = body.clone();
        *bad_bool.last_mut().unwrap() = 2;
        assert!(decode_set_command_minecart_c2s(&bad_bool).is_none());
        assert!(decode_set_command_minecart_c2s(&[]).is_none());
    }

    #[test]
    fn vanilla_cmdblk_comparator_signal_is_success_count_clamped() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::Overworld;
        let pos = (50, 64, 60);
        assert_eq!(command_block_comparator_signal(dim, pos), 0, "absent block reads 0");
        put_block(dim, pos, CommandBlockMode::Auto, CommandBlockFacing::Up, "time add 1", (false, true, false));
        command_block_tick();
        assert_eq!(command_block_comparator_signal(dim, pos), 1);
        {
            let mut map = live_command_blocks().lock().unwrap_or_else(|e| e.into_inner());
            map.get_mut(&(dim, pos.0, pos.1, pos.2)).unwrap().success_count = 40;
        }
        assert_eq!(command_block_comparator_signal(dim, pos), 15, "clamped to 15");
    }

    #[test]
    fn vanilla_cmdblk_minecart_runs_every_fourth_tick_activated() {
        let _world = world_guard();
        clear_cmd_state();
        upsert_command_minecart(CommandMinecartRecord {
            entity_id: 9,
            dim: DimId::Overworld,
            x: 3.5,
            y: 64.0,
            z: 3.5,
            command: "time add 1".into(),
            track_output: true,
            last_output: String::new(),
            success_count: 0,
            activated: false,
            powered_ticks: 0,
        });
        let t0 = world_time();
        command_minecart_tick();
        assert_eq!(world_time(), t0, "inactivated cart does not run");
        set_command_minecart_activated(9, true);
        for _ in 0..10 {
            command_minecart_tick();
        }
        // Activation ticks 0, 4, 8 -> three executions.
        assert_eq!(world_time(), t0 + 3);
        set_command_minecart_activated(9, false);
        command_minecart_tick();
        assert_eq!(world_time(), t0 + 3, "deactivation stops the cadence");
        let carts = command_minecarts().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(carts[&9].success_count, 1);
        assert!(carts[&9].last_output.contains("Set the time"));
    }

    #[test]
    fn vanilla_cmdblk_disabled_toggle_refuses_edit_and_execution() {
        let _world = world_guard();
        clear_cmd_state();
        let dim = DimId::Overworld;
        std::env::set_var("MEGU_ENABLE_COMMAND_BLOCK", "0");
        let pkt = update((7, 7, 7), "time add 1", CommandBlockMode::Auto, (false, false, true));
        assert!(apply_set_command_block(2, 1, true, dim, pkt).is_err(), "env-disabled edit refused");
        put_block(dim, (7, 7, 8), CommandBlockMode::Auto, CommandBlockFacing::Up, "time add 1", (false, true, false));
        let t0 = world_time();
        command_block_tick();
        assert_eq!(world_time(), t0, "disabled engine must not tick");
        std::env::remove_var("MEGU_ENABLE_COMMAND_BLOCK");
        command_block_tick();
        assert_eq!(world_time(), t0 + 1, "re-enabled engine ticks again");
    }

    // ---- datapack zip + DPC1 cache ----------------------------------------

    struct CmdblkFixture {
        dir: std::path::PathBuf,
    }

    impl CmdblkFixture {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "spine-cmdblk-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
        fn put(&self, rel: &str, value: &[u8]) {
            let file = self.dir.join(rel);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, value).unwrap();
        }
        fn root(&self) -> std::path::PathBuf {
            self.dir.join("datapacks")
        }
    }

    impl std::ops::Drop for CmdblkFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Minimal stored-method ZIP writer matching `zip_datapack_entries`'s
    /// contiguous-layout validation.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, body) in entries {
            let offset = out.len() as u32;
            let crc = {
                let mut c = flate2::Crc::new();
                c.update(body);
                c.sum()
            };
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&[0u8; 4]); // mod time/date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(body);
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&20u16.to_le_bytes()); // made by
            central.extend_from_slice(&20u16.to_le_bytes()); // needed
            central.extend_from_slice(&0x0800u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&[0u8; 4]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0u8; 8]); // extra/comment/disk/int-attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // ext attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_start = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0u8; 4]); // disk + central disk
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    fn clear_pack_state() {
        disabled_datapacks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        datapack_order()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn pack_named(name: &str) -> Option<Datapack> {
        packs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|p| p.name == name)
            .map(|p| Datapack {
                name: p.name.clone(),
                functions: p.functions.clone(),
                load: p.load.clone(),
                tick: p.tick.clone(),
            })
    }

    #[test]
    fn vanilla_cmdblk_zip_datapack_loads_functions_tags_recipes() {
        let _world = world_guard();
        clear_pack_state();
        let f = CmdblkFixture::new("zip");
        let zip = make_zip(&[
            ("pack.mcmeta", br#"{"pack":{"pack_format":48,"description":"z"}}"#),
            ("data/zz/function/hello.mcfunction", b"say hello\ntime add 1\n"),
            ("data/zz/tags/item/probe.json", br#"{"values":["minecraft:stone"]}"#),
            ("data/zz/tags/function/tick.json", br#"{"values":["zz:hello"]}"#),
            ("data/zz/recipe/cut.json", br#"{"type":"minecraft:stonecutting","ingredient":{"item":"minecraft:stone"},"result":{"id":"minecraft:diamond","count":2}}"#),
        ]);
        f.put("datapacks/ztest.zip", &zip);
        reload_datapacks(&f.root(), None, false).expect("zip datapack reload");
        let pack = pack_named("ztest.zip").expect("zip pack loaded");
        assert_eq!(
            pack.functions["zz:hello"],
            vec!["say hello".to_owned(), "time add 1".to_owned()]
        );
        assert_eq!(
            pack.tick,
            vec![Arc::<str>::from("zz:hello")],
            "tick tag hook loaded"
        );
        let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            registry.tags["item"]["zz:probe"],
            vec!["minecraft:stone".to_owned()]
        );
        assert_eq!(registry.stonecutting.len(), 1);
        assert_eq!(registry.stonecutting[0].count, 2);
        drop(registry);
        assert!(recipe_ids()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains("zz:cut"));
        assert_eq!(
            pack_format_verdicts().lock().unwrap_or_else(|e| e.into_inner())["ztest.zip"],
            PackFormatVerdict::Compatible
        );
        // A `../` traversal member rejects the whole archive, fail closed.
        let evil = make_zip(&[
            ("pack.mcmeta", br#"{"pack":{"pack_format":48}}"#),
            ("../evil.json", b"{}"),
            ("data/zz/function/evil.mcfunction", b"say evil\n"),
        ]);
        f.put("datapacks/evil.zip", &evil);
        reload_datapacks(&f.root(), None, false).expect("reload with evil zip");
        assert!(pack_named("evil.zip").is_none(), "traversal zip rejected wholesale");
        assert!(pack_named("ztest.zip").is_some());
    }

    #[test]
    fn vanilla_cmdblk_dpc1_cache_warm_hit_and_invalidation() {
        let _world = world_guard();
        clear_pack_state();
        let f = CmdblkFixture::new("dpc1");
        f.put(
            "datapacks/dpack/pack.mcmeta",
            br#"{"pack":{"pack_format":48}}"#,
        );
        f.put(
            "datapacks/dpack/data/zz/tags/item/probe.json",
            br#"{"values":["minecraft:diamond"]}"#,
        );
        f.put(
            "datapacks/dpack/data/zz/function/run.mcfunction",
            b"time add 1\n",
        );
        let cache = f.dir.join("cache");
        std::env::set_var("MEGU_DATAPACK_CACHE_DIR", &cache);
        reload_datapacks(&f.root(), None, false).expect("cold reload");
        assert!(!DPC1_LAST_RELOAD_WARM.load(Ordering::Relaxed), "first load is cold");
        assert!(cache.join("datapacks.dpc1").is_file(), "cache file written");
        // Warm hit: identical published content without re-parsing tags.
        reload_datapacks(&f.root(), None, false).expect("warm reload");
        assert!(DPC1_LAST_RELOAD_WARM.load(Ordering::Relaxed), "second load is warm");
        let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            registry.tags["item"]["zz:probe"],
            vec!["minecraft:diamond".to_owned()],
            "warm tag restored from snapshot"
        );
        drop(registry);
        let pack = pack_named("dpack").expect("pack still present on warm hit");
        assert_eq!(pack.functions["zz:run"], vec!["time add 1".to_owned()]);
        // One changed byte invalidates the digest -> cold rebuild.
        f.put(
            "datapacks/dpack/data/zz/tags/item/probe.json",
            br#"{"values":["minecraft:emerald"]}"#,
        );
        reload_datapacks(&f.root(), None, false).expect("invalidated reload");
        assert!(
            !DPC1_LAST_RELOAD_WARM.load(Ordering::Relaxed),
            "one changed byte must invalidate"
        );
        let registry = datapack_registry().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(registry.tags["item"]["zz:probe"], vec!["minecraft:emerald".to_owned()]);
        drop(registry);
        std::env::remove_var("MEGU_DATAPACK_CACHE_DIR");
    }
}

// ------------------------------------------------------ tests (PARITY-51-GR)

#[cfg(test)]
mod parity_51_gr_tests {
    use super::*;
    use std::net::TcpStream;

    fn p51_client(eid: i32, name: &str) -> (Arc<Client>, TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.eid = eid;
        client.name = name.into();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .unwrap();
        (Arc::new(client), receiver)
    }

    fn p51_ctx(me: Option<Arc<Client>>) -> CmdCtx {
        CmdCtx {
            me,
            x: 0.5,
            y: 16.0,
            z: 0.5,
        }
    }

    fn p51_put_block(
        dim: DimId,
        pos: (i32, i32, i32),
        mode: CommandBlockMode,
        facing: CommandBlockFacing,
        command: &str,
        flags: (bool, bool, bool),
    ) {
        let mut rec = CommandBlockRecord::new(dim, pos);
        rec.mode = mode;
        rec.facing = facing;
        rec.command = command.to_owned();
        (rec.conditional, rec.automatic, rec.powered) = flags;
        upsert_command_block(rec);
    }

    fn p51_frame(r: &mut TcpStream) -> Option<(i64, Vec<u8>)> {
        // The assertion is about the `0x6c` admin-channel echo specifically;
        // world-state broadcasts a command legitimately emits (e.g. the
        // `0x64` set_time update `/time` pushes on change) are not it.
        let deadline = Instant::now() + Duration::from_millis(300);
        while let Some(frame) = read_packet(r, deadline) {
            if frame.0 == 0x6c {
                return Some(frame);
            }
        }
        None
    }

    /// `commandBlockOutput=false`: the "@" admin-channel echo to operators is
    /// suppressed while the block still records `last_output` (track_output).
    #[test]
    fn parity_51_gr_command_block_output_gates_admin_broadcast() {
        let _world = world_guard();
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let dim = DimId::Overworld;
        let pos = (40, 64, 60);
        p51_put_block(
            dim,
            pos,
            CommandBlockMode::Auto,
            CommandBlockFacing::Up,
            "time add 1", (false, true, false));
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(dim, pos.0, pos.1, pos.2))
            .unwrap()
            .track_output = true;
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set("p51_gr_op", 2, false);
        let (op_client, mut op_recv) = p51_client(95_101, "p51_gr_op");
        let (user_client, mut user_recv) = p51_client(95_102, "p51_gr_user");
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend([op_client.clone(), user_client.clone()]);
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("commandBlockOutput", 0);
        command_block_tick();
        let rec = live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())[&(dim, pos.0, pos.1, pos.2)]
        .clone();
        assert!(
            rec.last_output.contains("Set the time"),
            "last_output is still recorded when commandBlockOutput is off"
        );
        assert!(
            p51_frame(&mut op_recv).is_none(),
            "no operator broadcast when commandBlockOutput is off"
        );
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("commandBlockOutput", 1);
        command_block_tick();
        let frame = p51_frame(&mut op_recv)
            .expect("operators receive the echo when commandBlockOutput is on");
        assert_eq!(frame.0, 0x6c);
        assert!(
            String::from_utf8_lossy(&frame.1).contains("[@:"),
            "vanilla admin-channel prefix on the command-block echo"
        );
        assert!(
            p51_frame(&mut user_recv).is_none(),
            "non-operators never see the admin channel"
        );
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != 95_101 && c.eid != 95_102);
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("p51_gr_op");
    }

    /// `logAdminCommands=false`: player-issued operator commands stop echoing
    /// to the server console; `true` restores the `[admin]` line.
    #[test]
    fn parity_51_gr_log_admin_commands_gates_console_echo() {
        let _world = world_guard();
        admin_command_log()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set("p51_gr_admin", 4, false);
        let (admin, _a) = p51_client(95_110, "p51_gr_admin");
        let ctx = p51_ctx(Some(admin));
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("logAdminCommands", 0);
        run_command_ctx(&ctx, "/time add 1", &mut None);
        assert!(
            admin_command_log()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "admin echo suppressed when logAdminCommands is off"
        );
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("logAdminCommands", 1);
        run_command_ctx(&ctx, "/time add 2", &mut None);
        assert_eq!(
            admin_command_log()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .back()
                .map(String::as_str),
            Some("[admin] p51_gr_admin: /time add 2"),
        );
        // A public (level-0) command never logs, and neither does a
        // non-operator player running a level-0-allowed form.
        run_command_ctx(&ctx, "/help", &mut None);
        let (user, _u) = p51_client(95_111, "p51_gr_guest");
        run_command_ctx(&p51_ctx(Some(user)), "/time query daytime", &mut None);
        assert_eq!(
            admin_command_log()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .back()
                .map(String::as_str),
            Some("[admin] p51_gr_admin: /time add 2"),
            "neither /help nor a non-operator's query may append a line"
        );
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("p51_gr_admin");
    }

    /// `maxCommandForkCount`: `/execute as @e as @e` squares the source list
    /// and must stop cleanly at the gamerule instead of pinning the tick.
    #[test]
    fn parity_51_gr_max_command_fork_count_caps_execute_fanout() {
        let _world = world_guard();
        let players: Vec<Arc<Client>> = (0..3)
            .map(|i| p51_client(95_120 + i, &format!("p51_gr_fork{i}")).0)
            .collect();
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(players.iter().cloned());
        let console = p51_ctx(None);
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("maxCommandForkCount", 4);
        let reply =
            run_command_ctx(&console, "/execute as @e as @e run say boom", &mut None);
        assert_eq!(
            reply, "execute fork limit",
            "nine-fork fanout is cut at the configured ceiling"
        );
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("maxCommandForkCount", 65536);
        let reply =
            run_command_ctx(&console, "/execute as @e as @e run say boom", &mut None);
        assert_ne!(
            reply, "execute fork limit",
            "vanilla default ceiling lets a small fanout finish"
        );
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| !(95_120..=95_122).contains(&c.eid));
    }

    /// `maxCommandChainLength`: `.mcfunction` lines and nested `function`
    /// calls share one bounded queue; the command-block chain keeps its
    /// per-trigger cap.
    #[test]
    fn parity_51_gr_max_command_chain_length_caps_mcfunction_and_blocks() {
        let _world = world_guard();
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let mut functions = HashMap::new();
        functions.insert(
            "p51:long".to_owned(),
            (0..10).map(|_| "time add 1".to_owned()).collect(),
        );
        functions.insert(
            "p51:outer".to_owned(),
            vec!["function p51:inner".to_owned(), "time add 1".to_owned()],
        );
        functions.insert(
            "p51:inner".to_owned(),
            vec!["time add 1".to_owned(), "time add 1".to_owned()],
        );
        packs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Datapack {
                name: "p51_gr".to_owned(),
                functions,
                load: Vec::new(),
                tick: Vec::new(),
            });
        let console = p51_ctx(None);
        let world_time = || WORLD_TIME.load(Ordering::Relaxed);
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("maxCommandChainLength", 3);
        let t0 = world_time();
        let reply = run_command_ctx(&console, "/function p51:long", &mut None);
        assert_eq!(
            world_time(),
            t0 + 3,
            "function lines stop at maxCommandChainLength"
        );
        assert_eq!(reply, "ran 3 commands");
        // A nested `function` call shares the chain queue — no fresh budget:
        // outer line (1) + first inner line (1) = the whole cap.
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("maxCommandChainLength", 2);
        let t1 = world_time();
        run_command_ctx(&console, "/function p51:outer", &mut None);
        assert_eq!(
            world_time(),
            t1 + 1,
            "nested function consumes the shared chain budget"
        );
        // Command-block chains keep their per-trigger cap on the same rule.
        let dim = DimId::End;
        p51_put_block(
            dim,
            (0, 64, 40),
            CommandBlockMode::Redstone,
            CommandBlockFacing::East,
            "time add 1", (false, false, true));
        p51_put_block(
            dim,
            (1, 64, 40),
            CommandBlockMode::Sequence,
            CommandBlockFacing::East,
            "time add 1", (false, true, false));
        p51_put_block(
            dim,
            (2, 64, 40),
            CommandBlockMode::Sequence,
            CommandBlockFacing::West,
            "time add 1", (false, true, false));
        let t2 = world_time();
        command_block_tick();
        assert_eq!(
            world_time(),
            t2 + 3,
            "trigger + exactly two chained executions under cap 2"
        );
        live_command_blocks()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        command_gamerules()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("maxCommandChainLength", 65536);
        packs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|p| p.name != "p51_gr");
    }
}

// ------------------------------------------------- ops_admin_builtin_tests ---

#[cfg(test)]
mod ops_admin_builtin_tests {
    use super::*;

    fn ops_admin_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn ops_client(name: &str) -> (Arc<Client>, std::net::TcpStream) {
        let (mut client, receiver) = crate::effect_tests::client_pair();
        client.name = name.into();
        (Arc::new(client), receiver)
    }

    fn ops_ctx(me: Option<Arc<Client>>, x: f64, y: f64, z: f64) -> CmdCtx {
        CmdCtx { me, x, y, z }
    }

    fn ops_run(ctx: &CmdCtx, msg: &str) -> String {
        run_command_ctx(ctx, msg, &mut None)
    }

    fn ops_op_ctx(name: &str, level: u8, x: f64, y: f64, z: f64) -> CmdCtx {
        if level > 0 {
            ops_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set(name, level, false);
        }
        let (client, _receiver) = ops_client(name);
        ops_ctx(Some(client), x, y, z)
    }

    fn ops_deop(name: &str) {
        ops_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    fn ops_reset_claims() {
        live_claims()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let _ = set_live_spawn_protect_radius(0);
        LIVE_CLAIM_QUOTA.store(256, Ordering::Relaxed);
        live_block_audit()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        live_rollback_log()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn ops_admin_builtin_audit_records_and_rollback_restores_reverse_order() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        // Unique coords + actor so parallel lanes' audit entries cannot match.
        let (bx, by, bz) = (777_i32, 200_i32, 777_i32);
        let dim = DimId::Overworld;
        let before = block_at_dim(dim, bx, by, bz);
        let griefer = ops_op_ctx("ops_audit_griefer", 2, bx as f64 + 0.5, by as f64, bz as f64 + 0.5);
        let stone = state_from_arg("minecraft:stone").expect("stone state");
        let gold = state_from_arg("minecraft:gold_block").expect("gold state");
        assert!(ops_run(&griefer, &format!("/setblock {bx} {by} {bz} minecraft:stone")).starts_with("Set block"));
        assert!(ops_run(&griefer, &format!("/setblock {bx} {by} {bz} minecraft:gold_block")).starts_with("Set block"));
        assert_eq!(block_at_dim(dim, bx, by, bz), gold);
        // The audit ring saw both transitions, attributed to the griefer.
        let entries = query_live_block_audit(&LiveBlockAuditQuery {
            actor: Some("ops_audit_griefer".to_owned()),
            dim: Some(dim),
            center: Some((dim, bx, by, bz)),
            radius: 2,
            since_tick: None,
        });
        assert_eq!(entries.len(), 2, "expected air->stone and stone->gold entries");
        assert_eq!(entries[0].old_state, before);
        assert_eq!(entries[0].new_state, stone);
        assert_eq!(entries[1].old_state, stone);
        assert_eq!(entries[1].new_state, gold);
        // OP-4 audit query around the executor sees the same edits.
        let op = ops_op_ctx("ops_audit_op", 4, bx as f64 + 0.5, by as f64, bz as f64 + 0.5);
        let audit = ops_run(&op, "/megu audit ops_audit_griefer 8");
        assert!(audit.contains("ops_audit_griefer"), "{audit}");
        assert!(audit.contains("2 edit(s)"), "{audit}");
        // Rollback unwinds newest-first back to the original state.
        let reply = ops_run(&op, "/megu rollback ops_audit_griefer 8");
        assert!(reply.contains("restored 2"), "{reply}");
        assert!(reply.contains("skipped 0"), "{reply}");
        assert_eq!(block_at_dim(dim, bx, by, bz), before, "rollback must restore the original state");
        let summaries = live_rollback_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].target, "ops_audit_griefer");
        assert_eq!(summaries[0].restored, 2);
        ops_deop("ops_audit_griefer");
        ops_deop("ops_audit_op");
        ops_reset_claims();
    }

    #[test]
    fn ops_admin_builtin_rollback_skips_diverged_cells() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        let (bx, by, bz) = (778_i32, 200_i32, 778_i32);
        let dim = DimId::Overworld;
        let before = block_at_dim(dim, bx, by, bz);
        let actor = "ops_audit_diverge";
        record_live_block_audit(dim, (bx, by, bz), actor, before, 1, TICK.load(Ordering::Relaxed));
        // Someone else changed the cell after the audited edit: rollback must
        // skip rather than clobber the diverged state.
        set_block_dim(dim, bx, by, bz, 2);
        let op = ops_op_ctx("ops_audit_op2", 4, bx as f64 + 0.5, by as f64, bz as f64 + 0.5);
        let reply = ops_run(&op, &format!("/megu rollback {actor} 8"));
        assert!(reply.contains("restored 0"), "{reply}");
        assert!(reply.contains("skipped 1"), "{reply}");
        assert_eq!(block_at_dim(dim, bx, by, bz), 2, "diverged cell must not be clobbered");
        ops_deop("ops_audit_op2");
        ops_reset_claims();
    }

    #[test]
    fn ops_admin_builtin_audit_rollback_refuse_below_op4_and_command_blocks() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        for level in 0u8..4 {
            let name = format!("ops_gate_l{level}");
            let ctx = ops_op_ctx(&name, level, 10.5, 70.0, 10.5);
            for msg in [
                "/megu audit",
                "/megu audit someone 8",
                "/megu rollback someone 8",
                "/megu backup plan",
                "/megu backup status",
                "/megu metrics",
                "/megu policy list",
            ] {
                assert_eq!(
                    ops_run(&ctx, msg),
                    P8C_DENIED,
                    "level-{level} must be refused {msg}"
                );
            }
            // The claim admin overrides are OP-4 too.
            for msg in [
                "/megu claim admin-unclaim",
                "/megu claim spawn-radius 5",
                "/megu claim spawn-protect 5",
            ] {
                assert_eq!(
                    ops_run(&ctx, msg),
                    "You do not have permission to run that command",
                    "level-{level} must be refused {msg}"
                );
            }
            ops_deop(&name);
        }
        // Command blocks: the root deny-list refuses `megu` outright, and a
        // pinned level-2 dispatch re-checks the same gate.
        assert!(!command_block_source_allowed("megu"));
        let ctx = ops_ctx(None, 0.5, 64.0, 0.5);
        for msg in ["/megu audit", "/megu rollback p 8", "/megu backup plan", "/megu metrics"] {
            assert_eq!(
                run_command_ctx_level(&ctx, msg, &mut None, Some(COMMAND_BLOCK_OP_LEVEL)),
                "You do not have permission to run that command",
                "command-block level must be refused {msg}"
            );
        }
        // The level-4 tree still advertises `megu`; lower levels do not.
        for level in 0u8..4 {
            assert!(command_tree_at_level(level).child("megu").is_none());
        }
        assert!(command_tree_at_level(4).child("megu").is_some());
        ops_reset_claims();
    }

    #[test]
    fn ops_admin_builtin_claim_here_trust_unclaim_info_and_quota() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        let dim = DimId::Overworld;
        let (x, z) = (400.5_f64, 400.5_f64);
        let alice = ops_op_ctx("ops_claim_alice", 0, x, 70.0, z);
        let bob = ops_op_ctx("ops_claim_bob", 0, x, 70.0, z);
        // Normal players claim their own chunk through the level-0 path.
        assert!(ops_run(&alice, "/megu claim here").contains("Claimed chunk"));
        assert!(ops_run(&bob, "/megu claim here").contains("already claimed by ops_claim_alice"));
        // Protection: owner and OPs pass, strangers fail closed.
        assert!(can_player_modify_block(dim, 400, 400, "ops_claim_alice", 0));
        assert!(!can_player_modify_block(dim, 400, 400, "ops_claim_bob", 0));
        assert!(can_player_modify_block(dim, 400, 400, "ops_claim_bob", 4));
        // Trusted allies may build; untrust revokes.
        assert!(ops_run(&alice, "/megu claim trust ops_claim_bob").contains("Trusted"));
        assert!(can_player_modify_block(dim, 400, 400, "ops_claim_bob", 0));
        // A stranger cannot manage the claim.
        assert!(ops_run(&bob, "/megu claim untrust ops_claim_bob").contains("claim owner"));
        assert!(ops_run(&alice, "/megu claim info").contains("claimed by ops_claim_alice"));
        assert!(ops_run(&alice, "/megu claim untrust ops_claim_bob").contains("Removed trust"));
        assert!(!can_player_modify_block(dim, 400, 400, "ops_claim_bob", 0));
        // Unclaim frees the chunk for the next player.
        assert!(ops_run(&alice, "/megu claim unclaim").contains("Unclaimed"));
        assert!(can_player_modify_block(dim, 400, 400, "ops_claim_bob", 0));
        assert!(ops_run(&bob, "/megu claim here").contains("Claimed chunk"));
        // Quota is per-player: with the cap at 1, carol's second chunk is
        // refused while a different player may still claim their first.
        LIVE_CLAIM_QUOTA.store(1, Ordering::Relaxed);
        let carol = ops_op_ctx("ops_claim_carol", 0, 432.5, 70.0, 432.5);
        assert!(ops_run(&carol, "/megu claim here").contains("Claimed chunk"));
        let carol2 = ops_op_ctx("ops_claim_carol", 0, 448.5, 70.0, 448.5);
        assert!(ops_run(&carol2, "/megu claim here").contains("Claim quota reached"));
        let dave = ops_op_ctx("ops_claim_dave", 0, 464.5, 70.0, 464.5);
        assert!(ops_run(&dave, "/megu claim here").contains("Claimed chunk"));
        LIVE_CLAIM_QUOTA.store(256, Ordering::Relaxed);
        // Admin override: OP-4 removes anyone's claim at the executor chunk.
        let op = ops_op_ctx("ops_claim_op", 4, 432.5, 70.0, 432.5);
        assert!(ops_run(&op, "/megu claim admin-unclaim").contains("Admin-unclaimed"));
        assert!(can_player_modify_block(dim, 432, 432, "ops_claim_carol", 0));
        for name in ["ops_claim_alice", "ops_claim_bob", "ops_claim_carol", "ops_claim_dave", "ops_claim_op"] {
            ops_deop(name);
        }
        ops_reset_claims();
    }

    #[test]
    fn ops_admin_builtin_spawn_protection_radius_gates_non_ops() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        let dim = DimId::Overworld;
        let spawn = *WORLD_SPAWN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (sx, sz) = (spawn.0.floor() as i32, spawn.2.floor() as i32);
        let op = ops_op_ctx("ops_spawn_op", 4, sx as f64 + 0.5, 70.0, sz as f64 + 0.5);
        assert_eq!(ops_run(&op, "/megu claim spawn-radius 12"), "Spawn protection set to 12 blocks");
        // Inside the radius a level-0 player is refused; outside it is open;
        // OP-4 always passes — the vanilla spawn-protection semantic.
        assert!(!can_player_modify_block(dim, sx, sz, "ops_spawn_victim", 0));
        assert!(can_player_modify_block(dim, sx + 100, sz + 100, "ops_spawn_victim", 0));
        assert!(can_player_modify_block(dim, sx, sz, "ops_spawn_victim", 4));
        assert_eq!(ops_run(&op, "/megu claim info").lines().any(|l| l.contains("Spawn protection: 12")), true);
        // `spawn-protect` is the same setter; 0 disables cleanly.
        assert_eq!(ops_run(&op, "/megu claim spawn-protect 0"), "Spawn protection disabled");
        assert!(can_player_modify_block(dim, sx, sz, "ops_spawn_victim", 0));
        assert!(ops_run(&op, "/megu claim spawn-radius 2048").contains("must be in 0..=1024"));
        ops_deop("ops_spawn_op");
        ops_reset_claims();
    }

    #[test]
    fn ops_admin_builtin_backup_plan_status_and_metrics_are_live_and_gated() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        let op = ops_op_ctx("ops_bm_op", 4, 0.5, 70.0, 0.5);
        let plan = ops_run(&op, "/megu backup plan");
        assert!(plan.contains("Next backup: world-backup-"), "{plan}");
        assert!(plan.contains("Prune"), "{plan}");
        assert!(plan.contains("Disk guard"), "{plan}");
        let status = ops_run(&op, "/megu backup status");
        assert!(status.contains("Retention: keep at most 12"), "{status}");
        assert!(status.contains("Existing backups:"), "{status}");
        let metrics = ops_run(&op, "/megu metrics");
        assert!(metrics.starts_with("# HELP megu_tps"), "{metrics}");
        assert!(metrics.contains("megu_players_online"), "{metrics}");
        assert!(metrics.contains("megu_uptime_seconds"), "{metrics}");
        // Usage failures are clean, and levels below 4 never reach the handlers.
        assert_eq!(ops_run(&op, "/megu backup"), "usage: /megu backup plan|status");
        assert_eq!(ops_run(&op, "/megu metrics now"), "usage: /megu metrics");
        let pleb = ops_op_ctx("ops_bm_pleb", 0, 0.5, 70.0, 0.5);
        assert_eq!(ops_run(&pleb, "/megu backup plan"), P8C_DENIED);
        assert_eq!(ops_run(&pleb, "/megu metrics"), P8C_DENIED);
        // A level-0 player CAN still self-manage claims — the one exception.
        assert!(ops_run(&pleb, "/megu claim info").contains("unclaimed"));
        ops_deop("ops_bm_op");
        ops_deop("ops_bm_pleb");
        ops_reset_claims();
    }

    #[test]
    fn ops_admin_builtin_claim_requires_player_and_unknown_subcommand_fails_closed() {
        let _world = world_guard();
        let _serial = ops_admin_lock().lock().unwrap_or_else(|e| e.into_inner());
        ops_reset_claims();
        // Console (no player) cannot stake claims.
        let console = ops_ctx(None, 0.5, 70.0, 0.5);
        for msg in ["/megu claim here", "/megu claim unclaim", "/megu claim trust x"] {
            let reply = ops_run(&console, msg);
            assert!(reply.contains("Claims require a player") || reply == P8C_DENIED || reply == MEGU_USAGE, "{msg} -> {reply}");
        }
        // Bare `/megu` keeps the POLICY-CMD usage contract; unknown
        // subcommands and a bare `claim` get the full OPS-ADMIN map.
        let op = ops_op_ctx("ops_misc_op", 4, 0.5, 70.0, 0.5);
        assert_eq!(ops_run(&op, "/megu"), POLICY_USAGE);
        assert_eq!(ops_run(&op, "/megu bogus"), MEGU_USAGE);
        assert_eq!(ops_run(&op, "/megu claim"), MEGU_USAGE);
        ops_deop("ops_misc_op");
        ops_reset_claims();
    }
}
