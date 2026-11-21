// Anti-cheat foundation: the server decides the limits, the operator only switches things.
//
// `server.toml` has three anti-cheat keys: `antixray`, `antifullbright` and
// `check_ops`. The movement, fall and placement checks are always on (only
// `MC_SPINE_ANTICHEAT=off`, meant for tests and development, turns them off). Every
// threshold is a constant here, derived from vanilla 1.21.1 movement physics, and is
// scaled per player, per packet, from state the server already owns: attributes
// (`movement_speed`, `jump_strength`, `gravity`, `step_height`, `flying_speed`, ...),
// effects, enchantments, elytra/riptide, game mode and the velocity the server itself
// sent. A mod that changes any of those through the server (attribute map, effects,
// `SetEntityMotion`, teleports) changes the limits with it; a modifier we cannot read
// makes the limits more lenient, never stricter.
//
// Every flag raises a violation level (VL) that decays over time. A failed check is
// corrected (the placement is cancelled, the player is pulled back, the fall is
// applied) and never kicks, so a false positive costs one rubber-band. Operators get an
// alert in chat.
//
// Hooks for server-side code that moves players on purpose: [`note_server_velocity`]
// (called for every `SetEntityMotion` the server sends), [`expect_motion`],
// [`expect_teleport`], [`grant_grace`] and [`note_explosion`].

// ---------- configuration ----------

/// Whether the movement, fall and placement checks run. Not a file key any more: they
/// are always on unless `MC_SPINE_ANTICHEAT=off` (tests, development) or an old file
/// still says `enabled = false` / `preset = "off"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcPreset {
    Off,
    Standard,
}

impl AcPreset {
    /// `on`/`off` plus the old preset names, which all mean "on" except `off`.
    pub(crate) fn parse(s: &str) -> Option<AcPreset> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" | "false" | "0" | "no" => AcPreset::Off,
            "on" | "true" | "1" | "yes" | "enabled" | "light" | "low" | "standard" | "default"
            | "normal" | "strict" | "high" => AcPreset::Standard,
            _ => return None,
        })
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            AcPreset::Off => "off",
            AcPreset::Standard => "on",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AcConfig {
    pub preset: AcPreset,
    /// Operators see flags in chat. Always on; only an old `alerts = false` turns it off.
    pub alerts: bool,
    /// Lower-cased player names the checks skip (old `exempt` key only).
    pub exempt: Vec<String>,
    /// `check_ops`: operators are checked too (default: they are not).
    pub check_ops: bool,
    pub anti_xray: bool,
    /// Hide light from sealed generated cavities (see `crate::anti_fullbright_enabled`).
    pub anti_fullbright: bool,
    /// Deprecated keys found in the file, reported once per load.
    pub notes: Vec<String>,
}

impl Default for AcConfig {
    fn default() -> Self {
        AcConfig {
            preset: AcPreset::Standard,
            alerts: true,
            exempt: Vec::new(),
            check_ops: false,
            anti_xray: true,
            anti_fullbright: true,
            notes: Vec::new(),
        }
    }
}

fn ac_env_bool(v: &str) -> bool {
    crate::anti_xray_value_enabled(v)
}

impl AcConfig {
    /// `[anticheat]` from the parsed file, then the environment on top:
    /// `MC_SPINE_ANTICHEAT` (on/off), `MC_SPINE_ANTI_XRAY`, `MC_SPINE_ANTI_FULLBRIGHT`.
    /// Old keys (`preset`, `enabled`, `alerts`, `exempt`, `exempt_ops`, `ops_bypass`,
    /// `anti_xray`) are still honoured, with a note asking to move off them.
    pub(crate) fn from_doc(
        doc: &ServerConfigDoc,
        env: impl Fn(&str) -> Option<String>,
    ) -> AcConfig {

        let mut c = AcConfig::default();
        let table = doc.get("anticheat");
        let get = |k: &str| table.and_then(|t| t.get(k));
        let mut old = |key: &str, instead: &str| {
            c.notes.push(format!(
                "[anticheat] {key} is deprecated and will be removed; {instead}"
            ))
        };
        let mut preset = None;
        let mut alerts = None;
        let mut exempt = Vec::new();
        let mut check_ops = None;
        if let Some(v) = get("preset") {
            old("preset", "the checks are always on now");
            preset = match v {
                ServerConfigValue::Str(s) => AcPreset::parse(s),
                ServerConfigValue::Bool(b) => Some(if *b { AcPreset::Standard } else { AcPreset::Off }),
                _ => None,
            };
        }
        if let Some(ServerConfigValue::Bool(b)) = get("enabled") {
            preset = Some(if *b { AcPreset::Standard } else { AcPreset::Off });
        }
        if let Some(v) = get("alerts") {
            old("alerts", "operators always get alerts");
            if let ServerConfigValue::Bool(b) = v {
                alerts = Some(*b);
            }
        }
        if let Some(v) = get("exempt") {
            old(
                "exempt",
                "creative/spectator players are skipped automatically",
            );
            if let ServerConfigValue::Array(names) = v {
                exempt = names
                    .iter()
                    .filter_map(|v| match v {
                        ServerConfigValue::Str(s) => Some(s.trim().to_ascii_lowercase()),
                        _ => None,
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
        for key in ["exempt_ops", "ops_bypass"] {
            if let Some(v) = get(key) {
                old(key, "use check_ops (the opposite meaning)");
                if let ServerConfigValue::Bool(b) = v {
                    check_ops = Some(!*b);
                }
            }
        }
        let mut anti_xray = None;
        if let Some(v) = get("anti_xray") {
            old("anti_xray", "use antixray");
            if let ServerConfigValue::Bool(b) = v {
                anti_xray = Some(*b);
            }
        }
        if let Some(ServerConfigValue::Bool(b)) = get("check_ops") {
            check_ops = Some(*b);
        }
        if let Some(ServerConfigValue::Bool(b)) = get("antixray") {
            anti_xray = Some(*b);
        }
        let mut anti_fullbright = None;
        if let Some(ServerConfigValue::Bool(b)) = get("antifullbright") {
            anti_fullbright = Some(*b);
        }
        for key in ["enabled", "antixray", "antifullbright", "check_ops"] {
            if let Some(v) = get(key) {
                if !matches!(v, ServerConfigValue::Bool(_)) {
                    c.notes.push(format!(
                        "[anticheat] {key} must be true or false, got {v:?}"
                    ));
                }
            }
        }
        if let Some(p) = preset {
            c.preset = p;
        }
        if let Some(a) = alerts {
            c.alerts = a;
        }
        c.exempt = exempt;
        if let Some(b) = check_ops {
            c.check_ops = b;
        }
        if let Some(b) = anti_xray {
            c.anti_xray = b;
        }
        if let Some(b) = anti_fullbright {
            c.anti_fullbright = b;
        }
        if let Some(v) = env("MC_SPINE_ANTICHEAT") {
            match AcPreset::parse(&v) {
                Some(p) => c.preset = p,
                None => c.notes.push(format!(
                    "[anticheat] MC_SPINE_ANTICHEAT={v:?} is not on/off; ignored"
                )),
            }
        }
        if let Some(v) = env("MC_SPINE_ANTI_XRAY") {
            c.anti_xray = ac_env_bool(&v);
        }
        if let Some(v) = env("MC_SPINE_ANTI_FULLBRIGHT") {
            c.anti_fullbright = ac_env_bool(&v);
        }
        c
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "checks={} antixray={} antifullbright={} check_ops={}{}{}",
            self.preset.name(),
            self.anti_xray,
            self.anti_fullbright,
            self.check_ops,
            if self.alerts { "" } else { " alerts=off" },
            if self.exempt.is_empty() {
                String::new()
            } else {
                format!(" exempt={}", self.exempt.len())
            }
        )
    }
}

static AC_CONFIG: std::sync::RwLock<Option<Arc<AcConfig>>> = std::sync::RwLock::new(None);
static AC_ANTI_XRAY: AtomicBool = AtomicBool::new(true);
static AC_ANTI_FULLBRIGHT: AtomicBool = AtomicBool::new(true);

/// Swap in new settings (boot and `/anticheat reload`).
fn ac_install(config: AcConfig) {
    AC_ANTI_XRAY.store(config.anti_xray, Ordering::Relaxed);
    AC_ANTI_FULLBRIGHT.store(config.anti_fullbright, Ordering::Relaxed);
    for note in &config.notes {
        eprintln!("{note}");
    }
    eprintln!("[anticheat] {}", config.summary());
    *AC_CONFIG.write().unwrap() = Some(Arc::new(config));
}

fn ac_config() -> Arc<AcConfig> {
    if let Some(c) = AC_CONFIG.read().unwrap().as_ref() {
        return c.clone();
    }
    Arc::new(ac_unbooted_default())
}

/// Before `server_config::boot` (tool modes, and the in-process test servers) the checks
/// stay off so existing fixtures keep their behaviour; anti-xray keeps its old default
/// and anti-fullbright stays off so existing light fixtures see the real light.
fn ac_unbooted_default() -> AcConfig {
    AcConfig {
        preset: if cfg!(test) {
            AcPreset::Off
        } else {
            AcPreset::Standard
        },
        anti_xray: std::env::var("MC_SPINE_ANTI_XRAY")
            .map(|v| ac_env_bool(&v))
            .unwrap_or(true),
        anti_fullbright: std::env::var("MC_SPINE_ANTI_FULLBRIGHT")
            .map(|v| ac_env_bool(&v))
            .unwrap_or(!cfg!(test)),
        ..AcConfig::default()
    }
}

fn ac_booted() -> bool {
    AC_CONFIG.read().map(|c| c.is_some()).unwrap_or(false)
}

/// Read on the chunk-encode hot path: one relaxed load.
fn ac_anti_xray_enabled() -> bool {
    if !ac_booted() {
        static UNBOOTED: OnceLock<bool> = OnceLock::new();
        return *UNBOOTED.get_or_init(|| ac_unbooted_default().anti_xray);
    }
    AC_ANTI_XRAY.load(Ordering::Relaxed)
}

/// Read when block light is built for a chunk.
fn ac_anti_fullbright_enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_FULLBRIGHT.with(|c| c.get()) {
        return v;
    }
    if !ac_booted() {
        static UNBOOTED: OnceLock<bool> = OnceLock::new();
        return *UNBOOTED.get_or_init(|| ac_unbooted_default().anti_fullbright);
    }
    AC_ANTI_FULLBRIGHT.load(Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    static TEST_FULLBRIGHT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Per-thread override so a light test does not change what concurrent tests see.
#[cfg(test)]
fn ac_test_set_anti_fullbright(on: Option<bool>) {
    TEST_FULLBRIGHT.with(|c| c.set(on));
}

#[cfg(test)]
static TEST_PRESETS: Mutex<Vec<(String, AcPreset)>> = Mutex::new(Vec::new());

/// Tests run many players against one in-process server; a per-name preset keeps an
/// anti-cheat fixture from changing what concurrent fixtures see.
#[cfg(test)]
fn ac_test_set_player_preset(name: &str, preset: AcPreset) {
    let mut v = TEST_PRESETS.lock().unwrap();
    v.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    v.push((name.to_owned(), preset));
}

fn ac_preset_for(name: &str) -> AcPreset {
    let _ = name; // only the test override is per name; exemptions live in `exemption`
    #[cfg(test)]
    if let Some((_, p)) = TEST_PRESETS
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return *p;
    }
    ac_config().preset
}

// ---------- check registry ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AcCheckId {
    Place,
    Movement,
    NoFall,
}

impl AcCheckId {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const ALL: [AcCheckId; 3] = [AcCheckId::Place, AcCheckId::Movement, AcCheckId::NoFall];
    fn index(self) -> usize {
        self as usize
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            AcCheckId::Place => "place",
            AcCheckId::Movement => "movement",
            AcCheckId::NoFall => "nofall",
        }
    }
}

/// What the server does when a check fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Refuse the action and resync the client.
    Cancel,
    /// Teleport the player back to the last accepted position.
    Setback,
    /// Replace the client's claim with the server's answer (e.g. on-ground).
    Correct,
}

/// One check's behaviour when the checks are on. All numbers are ours, not the operator's.
#[derive(Clone, Copy, Debug, PartialEq)]
struct AcPolicy {
    /// Multiplier on the attribute-derived allowance (movement).
    pub slack: f64,
    /// Movement credit that may build up while a client is quiet or lagging, in ticks.
    pub burst_ticks: f64,
    /// VL lost per second.
    pub decay_per_sec: f64,
}

trait AcCheck {
    const ID: AcCheckId;
    const ACTION: Action;
    /// `None` = the check is off at this preset.
    fn policy(preset: AcPreset) -> Option<AcPolicy>;
}

pub(crate) struct PlaceCheck;
pub(crate) struct MovementCheck;
pub(crate) struct NoFallCheck;

const fn ac_pol(slack: f64, burst_ticks: f64, decay_per_sec: f64) -> Option<AcPolicy> {
    Some(AcPolicy {
        slack,
        burst_ticks,
        decay_per_sec,
    })
}

impl AcCheck for PlaceCheck {
    const ID: AcCheckId = AcCheckId::Place;
    const ACTION: Action = Action::Cancel;
    fn policy(preset: AcPreset) -> Option<AcPolicy> {
        match preset {
            AcPreset::Off => None,
            AcPreset::Standard => ac_pol(1.0, 0.0, 0.5),
        }
    }
}

impl AcCheck for MovementCheck {
    const ID: AcCheckId = AcCheckId::Movement;
    const ACTION: Action = Action::Setback;
    fn policy(preset: AcPreset) -> Option<AcPolicy> {
        match preset {
            AcPreset::Off => None,
            // Two seconds of credit absorbs a lag burst; the allowance itself is exact.
            AcPreset::Standard => ac_pol(1.0, 40.0, 1.0),
        }
    }
}

impl AcCheck for NoFallCheck {
    const ID: AcCheckId = AcCheckId::NoFall;
    const ACTION: Action = Action::Correct;
    fn policy(preset: AcPreset) -> Option<AcPolicy> {
        match preset {
            AcPreset::Off => None,
            AcPreset::Standard => ac_pol(1.0, 0.0, 0.2),
        }
    }
}

/// One registered check.
#[derive(Clone, Copy)]
struct AcRegistered {
    pub id: AcCheckId,
    pub action: Action,
    pub policy: fn(AcPreset) -> Option<AcPolicy>,
}

const fn ac_register<C: AcCheck>() -> AcRegistered {
    AcRegistered {
        id: C::ID,
        action: C::ACTION,
        policy: C::policy,
    }
}

/// The registry: every check, its action and its policy per preset. A new check is a
/// type implementing [`Check`] plus one line here.
const AC_REGISTRY: [AcRegistered; 3] = [
    ac_register::<PlaceCheck>(),
    ac_register::<MovementCheck>(),
    ac_register::<NoFallCheck>(),
];

fn ac_registered(check: AcCheckId) -> &'static AcRegistered {
    AC_REGISTRY
        .iter()
        .find(|r| r.id == check)
        .expect("every CheckId is registered")
}

fn ac_policy(check: AcCheckId, preset: AcPreset) -> Option<AcPolicy> {
    (ac_registered(check).policy)(preset)
}

fn ac_action(check: AcCheckId) -> Action {
    ac_registered(check).action
}

// ---------- allowance: limits derived from server-known state ----------

// Vanilla 1.21.1 constants the limits are built from (blocks per tick unless noted).
/// Sustained sprint-jumping on flat ground at `movement_speed` 0.1: ~7.13 blocks/s.
const SPRINT_JUMP: f64 = 0.357;
/// Head-room for per-tick variance (jump boost tick, stairs, edge cases): +30 % + 0.1.
const SLACK_MUL: f64 = 1.3;
const SLACK_ADD: f64 = 0.1;
/// Ice family (friction 0.98/0.989): sprint-jumping under a ceiling reaches ~3.5x.
const ICE_MUL: f64 = 3.5;
const DEFAULT_MOVEMENT_SPEED: f64 = 0.1;
const DEFAULT_JUMP_STRENGTH: f64 = 0.42;
const DEFAULT_STEP_HEIGHT: f64 = 0.6;
const DEFAULT_GRAVITY: f64 = 0.08;
const DEFAULT_FLYING_SPEED: f64 = 0.05;
/// Terminal fall speed is gravity / (1 - 0.98 drag) = 3.92 at default gravity.
const DRAG: f64 = 0.02;
/// Swimming/sprint-swimming, before depth strider or dolphin's grace.
const SWIM: f64 = 0.6;
/// Elytra with rockets or a dive; riptide launches.
const LAUNCHED: f64 = 4.0;
/// Creative sprint-flying at `flying_speed` 0.05 is ~1.09; spectators can scroll faster.
const CREATIVE_FLY: f64 = 1.5;
const SPECTATOR_FLY: f64 = 6.0;
/// Bubble columns, slime bounces and fluids push upwards this fast at most.
const FLUID_UP: f64 = 1.0;
const BOUNCE_UP: f64 = 4.0;
/// Unknown (mod) modifiers present: everything this much more lenient.
const UNKNOWN_MUL: f64 = 2.0;

/// What the server knows about a player right now, gathered once per checked packet.
#[derive(Clone, Debug, Default, PartialEq)]
struct AcMoverState {
    pub movement_speed: f64,
    pub jump_strength: f64,
    pub gravity: f64,
    pub step_height: f64,
    pub flying_speed: f64,
    pub speed_level: i32,
    pub jump_level: i32,
    pub levitation_level: i32,
    pub dolphins_grace: bool,
    pub depth_strider: i64,
    pub soul_speed: i64,
    pub riptide: bool,
    pub gliding: bool,
    pub game_mode: u8,
    pub in_fluid: bool,
    pub slippery: bool,
    pub bouncy: bool,
    /// Standing on or touching a non-vanilla (modded) block such as a Create belt,
    /// bearing, gantry, elevator, or conveyor.
    pub mod_transport: bool,
    /// An attribute or effect we do not model is present (e.g. added by a mod).
    pub unknown_modifiers: bool,
}

impl AcMoverState {
    pub(crate) fn vanilla() -> AcMoverState {
        AcMoverState {
            movement_speed: DEFAULT_MOVEMENT_SPEED,
            jump_strength: DEFAULT_JUMP_STRENGTH,
            gravity: DEFAULT_GRAVITY,
            step_height: DEFAULT_STEP_HEIGHT,
            flying_speed: DEFAULT_FLYING_SPEED,
            ..AcMoverState::default()
        }
    }
}

/// Per-tick limits for one player at one moment.
#[derive(Clone, Copy, Debug, PartialEq)]
struct AcAllowance {
    pub horizontal: f64,
    pub up: f64,
    pub down: f64,
}

impl AcAllowance {
    pub(crate) fn from_state(s: &AcMoverState) -> AcAllowance {
        // Never below vanilla: slowness, a lowered attribute or a broken value must not
        // make an honest player fail.
        let speed_factor = (s.movement_speed / DEFAULT_MOVEMENT_SPEED).max(1.0)
            * (1.0 + 0.2 * f64::from(s.speed_level.max(0)));
        let mut h = SPRINT_JUMP * speed_factor * SLACK_MUL + SLACK_ADD;
        if s.slippery {
            h *= ICE_MUL;
        }
        if s.soul_speed > 0 {
            h *= 1.0 + 0.4 * s.soul_speed as f64;
        }
        if s.in_fluid {
            let mut swim = SWIM * speed_factor * (1.0 + s.depth_strider.clamp(0, 3) as f64);
            if s.dolphins_grace {
                swim *= 3.0;
            }
            h = h.max(swim);
        }
        let jump =
            s.jump_strength.max(DEFAULT_JUMP_STRENGTH) + 0.1 * f64::from(s.jump_level.max(0));
        let mut up = (jump + SLACK_ADD).max(s.step_height.max(DEFAULT_STEP_HEIGHT)) * SLACK_MUL;
        if s.levitation_level > 0 {
            // Levitation climbs at 0.05 * level blocks/tick terminal; lenient margin on top.
            up = up.max(0.1 * f64::from(s.levitation_level) + 0.5);
        }
        if s.in_fluid {
            up = up.max(FLUID_UP);
        }
        if s.bouncy {
            up = up.max(BOUNCE_UP);
        }
        let mut down = (s.gravity.max(DEFAULT_GRAVITY) / DRAG) * 1.05;
        if s.gliding || s.riptide {
            h = h.max(LAUNCHED);
            up = up.max(LAUNCHED);
            down = down.max(LAUNCHED);
        }
        match s.game_mode {
            1 => {
                let fly = CREATIVE_FLY * (s.flying_speed / DEFAULT_FLYING_SPEED).max(1.0);
                h = h.max(fly);
                up = up.max(fly);
            }
            3 => {
                let fly = SPECTATOR_FLY * (s.flying_speed / DEFAULT_FLYING_SPEED).max(1.0);
                h = h.max(fly);
                up = up.max(fly);
                down = down.max(fly);
            }
            _ => {}
        }
        if s.unknown_modifiers {
            h *= UNKNOWN_MUL;
            up *= UNKNOWN_MUL;
            down *= UNKNOWN_MUL;
        }
        AcAllowance {
            horizontal: h,
            up,
            down,
        }
    }
}

fn ac_attribute_known(name: &str) -> bool {
    attr_key_id(name).is_some()
}

/// Effects this module models (zero-based client ids) or that cannot move a player.
fn ac_effect_known(id: i32) -> bool {
    id == HUNGER_EFFECT || (0..=38).contains(&id)
}

fn ac_block_name(state: u32) -> &'static str {
    crate::block_registry::breg()
        .map(|reg| reg.name_of(state))
        .unwrap_or("")
}

fn ac_capture(me: &Client, (x, y, z): (f64, f64, f64)) -> AcMoverState {
    let mut s = AcMoverState::vanilla();
    if let Some(map) = attrs().lock().unwrap().get(&format!("p:{}", me.name)) {
        for (name, value) in map {
            let v = *value;
            if !v.is_finite() {
                s.unknown_modifiers = true;
                continue;
            }
            match name.trim_start_matches("minecraft:") {
                "generic.movement_speed" => s.movement_speed = v,
                "generic.jump_strength" => s.jump_strength = v,
                "generic.gravity" => s.gravity = v,
                "generic.step_height" => s.step_height = v,
                "generic.flying_speed" => s.flying_speed = v,
                other if !ac_attribute_known(other) => s.unknown_modifiers = true,
                _ => {}
            }
        }
    }
    for e in me.effs.lock().unwrap().iter() {
        let level = e.amp + 1;
        match e.id {
            0 => s.speed_level = s.speed_level.max(level),
            7 => s.jump_level = s.jump_level.max(level),
            24 => s.levitation_level = s.levitation_level.max(level),
            29 => s.dolphins_grace = true,
            id if !ac_effect_known(id) => s.unknown_modifiers = true,
            _ => {}
        }
    }
    {
        let inv = me.inv.lock().unwrap();
        let held = *me.held.lock().unwrap();
        if let Some((_, _, comps)) = inv.get(&8) {
            s.depth_strider = enchant_level(comps, 7);
            s.soul_speed = enchant_level(comps, 35);
        }
        s.riptide = [36 + held, 45]
            .iter()
            .filter_map(|slot| inv.get(slot))
            .any(|(_, _, comps)| enchant_level(comps, 31) > 0);
    }
    s.gliding = *me.fall_fly.lock().unwrap();
    s.game_mode = *me.gm.lock().unwrap();
    let dim = *me.dim.lock().unwrap();
    let (bx, bz) = (x.floor() as i32, z.floor() as i32);
    for dy in [-1.0f64, 0.0, 1.0] {
        let raw_name = ac_block_name(block_at_dim(dim, bx, (y + dy).floor() as i32, bz));
        if !raw_name.is_empty() && !raw_name.starts_with("minecraft:") {
            s.mod_transport = true;
            s.unknown_modifiers = true;
        }
        let name = raw_name.trim_start_matches("minecraft:");
        if dy < 0.5 && name.contains("ice") {
            s.slippery = true;
        }
        if name == "slime_block" || name.ends_with("_bed") || name == "honey_block" {
            s.bouncy = true;
        }
        if dy > -0.5 && (name == "water" || name == "lava" || name == "bubble_column") {
            s.in_fluid = true;
        }
    }
    // Under the feet for a step or two: slime/beds bounce from below; modded blocks
    // (belts/elevators/contraptions) keep generous transport allowance.
    let below = ac_block_name(block_at_dim(dim, bx, (y - 2.0).floor() as i32, bz));
    if !below.is_empty() && !below.starts_with("minecraft:") {
        s.mod_transport = true;
        s.unknown_modifiers = true;
    }
    if below.ends_with("slime_block") || below.ends_with("_bed") {
        s.bouncy = true;
    }
    s
}

// ---------- per-player state ----------

/// Movement credit, a token bucket per axis: it fills at the allowed rate per elapsed tick
/// up to `burst_ticks` worth, and every move spends what it covers. Bursty-but-honest
/// clients (lag, packet batching) spend saved credit; a speed or teleport cheat runs out.
#[derive(Clone, Debug)]
struct AcMoveState {
    credit: [f64; 3],
    bonus: [f64; 3],
    bonus_until: Option<Instant>,
    grace_until: Option<Instant>,
    slippery_until: Option<Instant>,
    mod_transport_until: Option<Instant>,
    carried_until: Option<Instant>,
    last: Option<Instant>,
    fresh: bool,
}

impl Default for AcMoveState {
    fn default() -> Self {
        AcMoveState {
            credit: [0.0; 3],
            bonus: [0.0; 3],
            bonus_until: None,
            grace_until: None,
            slippery_until: None,
            mod_transport_until: None,
            carried_until: None,
            last: None,
            fresh: true,
        }
    }
}

/// Below this many blocks over the allowance a move is accepted (float noise, rounding).
const MOVE_TOLERANCE: f64 = 0.05;
const BONUS_SECS: f64 = 3.0;
const BONUS_CAP: f64 = 96.0;

impl AcMoveState {
    /// Returns the largest excess (blocks) when the move does not fit, `None` when it does.
    pub(crate) fn evaluate(
        &mut self,
        now: Instant,
        a: &AcAllowance,
        p: &AcPolicy,
        need: [f64; 3],
    ) -> Option<f64> {
        let rate = [a.horizontal * p.slack, a.up * p.slack, a.down * p.slack];
        let cap = rate.map(|r| r * p.burst_ticks);
        if self.grace_until.is_some_and(|g| now < g) {
            self.fresh = false;
            self.credit = cap;
            self.last = Some(now);
            return None;
        }
        if self.fresh {
            // A new baseline (join, server teleport, game-mode change): full credit, but
            // the move itself is still judged, so a teleport cannot be chained into a blink.
            self.fresh = false;
            self.credit = cap;
            self.last = Some(now);
        }
        let ticks = self
            .last
            .map(|l| now.saturating_duration_since(l).as_secs_f64() * 20.0)
            .unwrap_or(p.burst_ticks);
        self.last = Some(now);
        for i in 0..3 {
            self.credit[i] = (self.credit[i] + rate[i] * ticks).min(cap[i]);
        }
        if self.bonus_until.is_some_and(|until| now > until) {
            self.bonus = [0.0; 3];
            self.bonus_until = None;
        }
        let excess = (0..3)
            .map(|i| need[i] - (self.credit[i] + self.bonus[i]))
            .fold(f64::MIN, f64::max);
        if excess > MOVE_TOLERANCE {
            return Some(excess);
        }
        for i in 0..3 {
            let from_bonus = need[i].min(self.bonus[i]);
            self.bonus[i] -= from_bonus;
            self.credit[i] = (self.credit[i] - (need[i] - from_bonus)).max(-MOVE_TOLERANCE);
        }
        None
    }

    fn grant(&mut self, extra: [f64; 3], now: Instant) {
        for i in 0..3 {
            self.bonus[i] = (self.bonus[i] + extra[i]).min(BONUS_CAP);
        }
        self.bonus_until = Some(now + Duration::from_secs_f64(BONUS_SECS));
    }
}

#[derive(Debug)]
struct PlayerAc {
    #[cfg_attr(not(test), allow(dead_code))]
    name: String,
    vl: [f64; 3],
    vl_at: [Option<Instant>; 3],
    alert_at: [Option<Instant>; 3],
    flags: [u64; 3],
    moves: AcMoveState,
    nofall_reported: bool,
    last_game_mode: Option<u8>,
    game_mode_grace_until: Option<Instant>,
}

impl PlayerAc {
    fn new(name: &str) -> PlayerAc {
        PlayerAc {
            name: name.to_owned(),
            vl: [0.0; 3],
            vl_at: [None; 3],
            alert_at: [None; 3],
            flags: [0; 3],
            moves: AcMoveState::default(),
            nofall_reported: false,
            last_game_mode: None,
            game_mode_grace_until: None,
        }
    }
    fn decayed_vl(&self, check: AcCheckId, now: Instant, decay: f64) -> f64 {
        let i = check.index();
        let elapsed = self.vl_at[i]
            .map(|t| now.saturating_duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        (self.vl[i] - elapsed * decay).max(0.0)
    }
}

fn ac_players() -> &'static Mutex<HashMap<i32, PlayerAc>> {
    static P: OnceLock<Mutex<HashMap<i32, PlayerAc>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ac_with_player<R>(me: &Client, f: impl FnOnce(&mut PlayerAc) -> R) -> R {
    let mut map = ac_players().lock().unwrap();
    let entry = map.entry(me.eid).or_insert_with(|| PlayerAc::new(&me.name));
    f(entry)
}

/// After leaving creative or spectator the player may still be flying or falling from a
/// height they reached legally; the checks wait this long before judging them again.
const GAME_MODE_GRACE: Duration = Duration::from_secs(3);

/// Why a player is not checked right now, if they are not. Evaluated on every packet, so
/// switching game mode or op status takes effect immediately.
fn ac_exemption(me: &Client) -> Option<&'static str> {
    let c = ac_config();
    if c.exempt.iter().any(|n| n.eq_ignore_ascii_case(&me.name)) {
        return Some("exempt list");
    }
    if !c.check_ops && player_is_admin(&me.name) {
        return Some("operator");
    }
    let gm = *me.gm.lock().unwrap();
    let now = Instant::now();
    let in_grace = ac_with_player(me, |ac| {
        if ac.last_game_mode != Some(gm) {
            let was_free = matches!(ac.last_game_mode, Some(1 | 3));
            ac.last_game_mode = Some(gm);
            if was_free && !matches!(gm, 1 | 3) {
                ac.game_mode_grace_until = Some(now + GAME_MODE_GRACE);
                // Judge the next move from wherever the player is, with full credit.
                ac.moves.fresh = true;
                ac.nofall_reported = false;
            }
        }
        ac.game_mode_grace_until.is_some_and(|t| now < t)
    });
    match gm {
        1 => Some("creative"),
        3 => Some("spectator"),
        _ if in_grace => Some("game mode change"),
        _ => None,
    }
}

/// The preset the checks use for this player now: `off` while exempt.
fn ac_checked_preset(me: &Client) -> AcPreset {
    if ac_exemption(me).is_some() {
        AcPreset::Off
    } else {
        ac_preset_for(&me.name)
    }
}

/// The connection is gone.
fn ac_forget(eid: i32) {
    if let Some(map) = ac_players().lock().ok().as_mut() {
        map.remove(&eid);
    }
}

// ---------- hooks for server-side motion (mods, knockback, explosions, teleports) ----------

/// The server told entity `eid` to move with velocity `v` (blocks/tick). Called from
/// `send_packet` for every `SetEntityMotion`, so knockback, launches and anything a mod
/// does through the server are allowed for without the caller knowing about anti-cheat.
fn ac_note_server_velocity(eid: i32, v: [f64; 3]) {
    let Ok(mut map) = ac_players().lock() else {
        return;
    };
    let Some(p) = map.get_mut(&eid) else { return };
    let h = v[0].hypot(v[2]);
    // Air drag 0.91/tick: a horizontal push travels at most v / (1 - 0.91) = 11.1 v.
    let rise = if v[1] > 0.0 {
        v[1] * v[1] / (2.0 * DEFAULT_GRAVITY) + v[1] * 2.0
    } else {
        0.0
    };
    let fall = if v[1] < 0.0 { -v[1] * 12.0 } else { 0.0 };
    p.moves
        .grant([h * 12.0 + 0.5, rise + 0.5, fall], Instant::now());
}

/// Public hook: server-side code is about to move this player by `v` blocks/tick.
#[allow(dead_code)]
fn ac_expect_motion(eid: i32, v: [f64; 3]) {
    ac_note_server_velocity(eid, v);
}

/// Public hook for Create contraptions, belts, elevators, and any modded carrier:
/// grants exact rigid-carry motion credit `[dx, dy, dz]` AND marks the player as
/// supported for NoFall for 250 ms so smooth riding never rubber-bands or deals false
/// fall damage, while still blocking real teleports.
#[allow(dead_code)]
fn ac_note_carried_motion(eid: i32, delta: [f64; 3]) {
    if !delta.iter().all(|v| v.is_finite()) {
        return;
    }
    let Ok(mut map) = ac_players().lock() else {
        return;
    };
    let Some(p) = map.get_mut(&eid) else { return };
    let now = Instant::now();
    let h = delta[0].hypot(delta[2]) + 0.25;
    let up = delta[1].max(0.0) + 0.25;
    let down = (-delta[1]).max(0.0) + 0.25;
    p.moves.grant([h, up, down], now);
    p.moves.carried_until = Some(now + Duration::from_millis(250));
}

/// The server placed the player somewhere: refill credit from the new position.
fn ac_expect_teleport(eid: i32) {
    if let Some(p) = ac_players().lock().unwrap().get_mut(&eid) {
        p.moves.fresh = true;
    }
}

/// Public hook: skip movement checks for `for_time` (custom abilities, cutscenes).
#[allow(dead_code)]
fn ac_grant_grace(eid: i32, for_time: Duration) {
    if let Some(p) = ac_players().lock().unwrap().get_mut(&eid) {
        p.moves.grace_until = Some(Instant::now() + for_time);
    }
}

/// An explosion's knockback is computed client-side from the explosion packet.
fn ac_note_explosion(dim: DimId, pos: [f64; 3], power: f64) {
    let targets: Vec<(i32, (f64, f64, f64))> = client_snapshot()
        .iter()
        .filter(|c| *c.dim.lock().unwrap() == dim)
        .map(|c| {
            let (x, y, z, ..) = *c.pos.lock().unwrap();
            (c.eid, (x, y, z))
        })
        .collect();
    ac_note_explosion_for(pos, power, &targets);
}

/// The same, for callers that already hold a snapshot of the players in range.
fn ac_note_explosion_for(pos: [f64; 3], power: f64, targets: &[(i32, (f64, f64, f64))]) {
    let reach = (power * 2.0).max(1.0);
    for &(eid, (x, y, z)) in targets {
        let d = ((x - pos[0]).powi(2) + (y - pos[1]).powi(2) + (z - pos[2]).powi(2)).sqrt();
        if d < reach {
            let push = (1.0 - d / reach) * 2.5;
            ac_note_server_velocity(eid, [push, push, push]);
        }
    }
}

// ---------- flags, VL, alerts ----------

/// Record a failed check. Returns `true` when the server should correct the action.
fn ac_flag(me: &Client, check: AcCheckId, amount: f64, detail: &str) -> bool {
    let preset = ac_preset_for(&me.name);
    let Some(policy) = ac_policy(check, preset) else {
        return false;
    };
    let config = ac_config();
    let now = Instant::now();
    let (vl, alert) = ac_with_player(me, |p| {
        let i = check.index();
        let vl = p.decayed_vl(check, now, policy.decay_per_sec) + amount;
        p.vl[i] = vl;
        p.vl_at[i] = Some(now);
        p.flags[i] += 1;
        let alert = p.alert_at[i]
            .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(2));
        if alert {
            p.alert_at[i] = Some(now);
        }
        (vl, alert)
    });
    if alert {
        let line = format!(
            "[AC] {} failed {} ({detail}) VL {:.1}, {}",
            me.name,
            check.name(),
            vl,
            format!("{:?}", ac_action(check)).to_lowercase()
        );
        eprintln!("[anticheat] {line}");
        if config.alerts {
            let packet = system_chat(&line);
            for op in client_snapshot() {
                if op.eid != me.eid && player_is_admin(&op.name) {
                    if let Ok(mut sock) = op.sock.lock() {
                        let _ = send_packet(&mut sock, 0x6c, packet.clone());
                    }
                }
            }
        }
    }
    true
}

#[cfg_attr(not(test), allow(dead_code))]
fn ac_violation_level(eid: i32, check: AcCheckId) -> f64 {
    let map = ac_players().lock().unwrap();
    let Some(p) = map.get(&eid) else { return 0.0 };
    let decay = ac_policy(check, ac_preset_for(&p.name))
        .map(|p| p.decay_per_sec)
        .unwrap_or(0.0);
    p.decayed_vl(check, Instant::now(), decay)
}

#[cfg_attr(not(test), allow(dead_code))]
fn ac_flag_count(eid: i32, check: AcCheckId) -> u64 {
    ac_players()
        .lock()
        .unwrap()
        .get(&eid)
        .map(|p| p.flags[check.index()])
        .unwrap_or(0)
}

// ---------- placement ----------

/// Vanilla refuses a placement that targets an occupied cell or clicks no block.
/// Returns `false` when the placement must be refused.
fn ac_check_place(
    me: &Client,
    dim: DimId,
    clicked_pos: BlockPos,
    clicked: u32,
    target: BlockPos,
) -> bool {
    if ac_policy(AcCheckId::Place, ac_checked_preset(me)).is_none() {
        return true;
    }
    let Some(reg) = crate::block_registry::breg() else {
        return true;
    };
    if reg.is_air(clicked) {
        return !ac_flag(me, AcCheckId::Place, 1.0, "clicked air");
    }
    if target != clicked_pos {
        let old = block_at_dim(dim, target.0, target.1, target.2);
        if !reg.is_replaceable(old) {
            return !ac_flag(
                me,
                AcCheckId::Place,
                1.0,
                &format!("target occupied by {}", reg.name_of(old)),
            );
        }
    }
    true
}

/// Survival placement uses up the item (creative and the `off` preset keep the old path).
/// Not an exemption-aware check: an operator in survival uses up items like anyone else.
fn ac_consume_placed_item(me: &Client, held_slot: i16) {
    if ac_policy(AcCheckId::Place, ac_preset_for(&me.name)).is_none() || *me.gm.lock().unwrap() != 0 {
        return;
    }
    let slot = 36 + held_slot;
    let stack = {
        let mut inv = me.inv.lock().unwrap();
        map_take(&mut inv, slot, 1);
        inv.get(&slot).cloned().unwrap_or_default()
    };
    let mut packet = vec![0u8];
    write_varint(&mut packet, 0);
    packet.extend_from_slice(&slot.to_be_bytes());
    packet.extend_from_slice(&slot_wire(&stack));
    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x15, packet);
}

/// Undo a client-side prediction for one cell.
fn ac_resync_block(me: &Client, dim: DimId, (x, y, z): BlockPos) {
    let state = block_at_dim(dim, x, y, z);
    let visible = visible_block_update_state(dim, x, y, z, state);
    let mut body = Vec::new();
    write_pos(&mut body, x, y, z);
    write_varint(&mut body, i64::from(visible));
    let _ = send_packet(&mut me.sock.lock().unwrap(), 0x09, body);
}

// ---------- ground and movement ----------

/// Server-side on-ground: a collision box under the player's feet (with a small margin
/// for edges and latency), or a fluid/climbable cell that cancels fall damage anyway.
fn ac_server_supported(dim: DimId, (x, y, z): (f64, f64, f64)) -> bool {
    let Some(reg) = crate::block_registry::breg() else {
        return true;
    };
    let min = [x - 0.35, y - 0.3, z - 0.35];
    let max = [x + 0.35, y + 0.05, z + 0.35];
    for by in (min[1].floor() as i32 - 1)..=(max[1].floor() as i32) {
        for bz in (min[2].floor() as i32)..=(max[2].floor() as i32) {
            for bx in (min[0].floor() as i32)..=(max[0].floor() as i32) {
                let st = block_at_dim(dim, bx, by, bz);
                if reg.is_air(st) {
                    continue;
                }
                let name = reg.name_of(st).trim_start_matches("minecraft:");
                if matches!(
                    name,
                    "water"
                        | "lava"
                        | "bubble_column"
                        | "cobweb"
                        | "powder_snow"
                        | "scaffolding"
                        | "ladder"
                        | "vine"
                ) || name.ends_with("_vines")
                    || name.ends_with("_vines_plant")
                {
                    return true;
                }
                for (bmin, bmax) in reg.collision_boxes(st) {
                    let wmin = [
                        bx as f64 + bmin[0],
                        by as f64 + bmin[1],
                        bz as f64 + bmin[2],
                    ];
                    let wmax = [
                        bx as f64 + bmax[0],
                        by as f64 + bmax[1],
                        bz as f64 + bmax[2],
                    ];
                    if (0..3).all(|i| wmin[i] < max[i] && wmax[i] > min[i]) {
                        return true;
                    }
                }
            }
        }
    }
    // Boats, minecarts, shulkers and the like can carry a player.
    mobs().lock().unwrap().iter().any(|m| {
        m.dim == dim && {
            let (mx, my, mz, ..) = *m.pos.lock().unwrap();
            (mx - x).abs() < 1.6 && (mz - z).abs() < 1.6 && my <= y + 0.1 && my >= y - 2.5
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcMoveVerdict {
    /// Take the move; `ground` replaces the client's on-ground claim.
    Accept { ground: bool },
    /// Refuse the move and pull the player back.
    Setback,
}

/// On-ground for packets that carry no position (look / status-only).
fn ac_effective_ground(me: &Client, pos: (f64, f64, f64), claimed: bool) -> bool {
    if !claimed || ac_policy(AcCheckId::NoFall, ac_checked_preset(me)).is_none() {
        return claimed;
    }
    let dim = *me.dim.lock().unwrap();
    claimed && ac_server_supported(dim, pos)
}

fn ac_check_move(
    me: &Client,
    from: (f64, f64, f64),
    to: (f64, f64, f64),
    claimed_ground: bool,
) -> AcMoveVerdict {
    let preset = ac_checked_preset(me);
    let movement = ac_policy(AcCheckId::Movement, preset);
    let nofall = ac_policy(AcCheckId::NoFall, preset);
    if movement.is_none() && nofall.is_none() {
        return AcMoveVerdict::Accept {
            ground: claimed_ground,
        };
    }
    let now = Instant::now();
    if let Some(p) = movement {
        let mut state = ac_capture(me, to);
        let (slippery, mod_transport) = ac_with_player(me, |ac| {
            if state.slippery {
                ac.moves.slippery_until = Some(now + Duration::from_secs(2));
            }
            if state.mod_transport {
                ac.moves.mod_transport_until = Some(now + Duration::from_millis(1500));
            }
            (
                ac.moves.slippery_until.is_some_and(|t| now < t),
                ac.moves.mod_transport_until.is_some_and(|t| now < t),
            )
        });
        state.slippery = slippery;
        if mod_transport {
            state.mod_transport = true;
            state.unknown_modifiers = true;
        }
        let allowance = AcAllowance::from_state(&state);
        let need = [
            (to.0 - from.0).hypot(to.2 - from.2),
            (to.1 - from.1).max(0.0),
            (from.1 - to.1).max(0.0),
        ];
        let excess = ac_with_player(me, |ac| ac.moves.evaluate(now, &allowance, &p, need));
        if let Some(excess) = excess {
            let detail = format!(
                "moved h={:.2} up={:.2} down={:.2}, {excess:.2} over limit {:.2}/{:.2}/{:.2} per tick",
                need[0], need[1], need[2], allowance.horizontal * p.slack, allowance.up * p.slack, allowance.down * p.slack
            );
            if ac_flag(me, AcCheckId::Movement, 1.0 + excess.min(10.0), &detail) {
                return AcMoveVerdict::Setback;
            }
        }
    }
    let mut ground = claimed_ground;
    if nofall.is_some() && claimed_ground {
        let dim = *me.dim.lock().unwrap();
        let carried = ac_with_player(me, |ac| {
            ac.moves.carried_until.is_some_and(|t| now < t)
        });
        let supported = carried || ac_server_supported(dim, to);
        if !supported {
            ground = false;
            let falling = from.1 - to.1 > 0.05;
            let report =
                falling && ac_with_player(me, |ac| !std::mem::replace(&mut ac.nofall_reported, true));
            if report && !ac_flag(me, AcCheckId::NoFall, 2.0, "claimed on-ground while falling") {
                // dry run: leave the client's claim alone
                ground = claimed_ground;
            }
        } else {
            ac_with_player(me, |ac| ac.nofall_reported = false);
        }
    }
    AcMoveVerdict::Accept { ground }
}

/// Pull the player back to `to` without refilling their movement credit.
fn ac_setback(me: &Client, to: (f64, f64, f64, f32, f32, bool)) {
    let mut sock = me.sock.lock().unwrap();
    let mut teleport = me.teleport.lock().unwrap();
    let id = teleport.begin();
    *me.pos.lock().unwrap() = to;
    send_prepared_teleport(&mut sock, &mut teleport, (to.0, to.1, to.2, to.3, to.4), id);
}

/// Quietly reload `[anticheat]` from `server.toml` when vanilla `/reload` runs.
/// Missing `server.toml` is a no-op so pure zero-config servers stay silent.
fn ac_reload_quiet() {
    let Ok((doc, warnings)) = server_config_reread() else {
        return;
    };
    for w in warnings {
        eprintln!("[config] {w} (ignored)");
    }
    let config = AcConfig::from_doc(&doc, |name| std::env::var(name).ok());
    ac_install(config);
}

#[cfg(test)]
mod anticheat_unit_tests {
    use super::*;


    fn cfg(text: &str, env: &[(&str, &str)]) -> AcConfig {
        let (doc, _) = server_config_parse(text);
        AcConfig::from_doc(&doc, |k| {
            env.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn toggles_old_keys_and_env_precedence() {
        let d = cfg("", &[]);
        assert_eq!(d, AcConfig::default());
        assert_eq!(
            (
                d.preset,
                d.anti_xray,
                d.anti_fullbright,
                d.check_ops,
                d.alerts
            ),
            (AcPreset::Standard, true, true, false, true)
        );
        let c = cfg(
            "[anticheat]\nantixray = false\nantifullbright = false\ncheck_ops = true\n",
            &[],
        );
        assert_eq!(
            (c.preset, c.anti_xray, c.anti_fullbright, c.check_ops),
            (AcPreset::Standard, false, false, true)
        );
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        // Old keys still work, each with a deprecation note; exempt_ops/ops_bypass invert.
        let c = cfg(
            "[anticheat]\npreset = \"off\"\nalerts = false\nexempt_ops = false\nexempt = [\"Steve\", \"\"]\nanti_xray = false\n",
            &[],
        );
        assert_eq!(
            (
                c.preset,
                c.alerts,
                c.check_ops,
                c.exempt.clone(),
                c.anti_xray
            ),
            (AcPreset::Off, false, true, vec!["steve".to_owned()], false)
        );
        assert_eq!(c.notes.len(), 5, "{:?}", c.notes);
        assert!(c.notes.iter().all(|n| n.contains("deprecated")));
        assert!(!cfg("[anticheat]\nops_bypass = true\n", &[]).check_ops);
        let c = cfg(
            "[anticheat]\nenabled = false\n",
            &[],
        );
        assert_eq!(c.preset, AcPreset::Off);
        assert!(c.notes.is_empty(), "enabled=false is first-class, not deprecated: {:?}", c.notes);
        for old in ["light", "standard", "strict"] {
            let c = cfg(&format!("[anticheat]\npreset = \"{old}\"\n"), &[]);
            assert_eq!(c.preset, AcPreset::Standard, "{old}");
        }
        // A wrong type is reported, not fatal.
        let c = cfg("[anticheat]\nantixray = \"yes\"\n", &[]);
        assert!(c.anti_xray && c.notes.len() == 1, "{:?}", c.notes);
        // The environment wins: on/off and the old preset names.
        let env = [
            ("MC_SPINE_ANTICHEAT", "off"),
            ("MC_SPINE_ANTI_XRAY", "0"),
            ("MC_SPINE_ANTI_FULLBRIGHT", "false"),
        ];
        let c = cfg("[anticheat]\nantixray = true\n", &env);
        assert_eq!(
            (c.preset, c.anti_xray, c.anti_fullbright),
            (AcPreset::Off, false, false)
        );
        let c = cfg(
            "[anticheat]\nenabled = false\n",
            &[("MC_SPINE_ANTICHEAT", "strict")],
        );
        assert_eq!(c.preset, AcPreset::Standard);
        let c = cfg("", &[("MC_SPINE_ANTICHEAT", "sideways")]);
        assert_eq!((c.preset, c.notes.len()), (AcPreset::Standard, 1));
    }

    #[test]
    fn registry_is_off_or_on_and_never_kicks() {
        assert_eq!(AC_REGISTRY.map(|r| r.id), AcCheckId::ALL);
        for check in AcCheckId::ALL {
            assert!(ac_policy(check, AcPreset::Off).is_none(), "{check:?}");
            assert!(ac_policy(check, AcPreset::Standard).is_some(), "{check:?}");
        }
        assert_eq!(ac_action(AcCheckId::Movement), Action::Setback);
        assert_eq!(ac_action(AcCheckId::Place), Action::Cancel);
        assert_eq!(ac_action(AcCheckId::NoFall), Action::Correct);
    }

    #[test]
    fn allowance_follows_attributes_and_effects_and_never_drops_below_vanilla() {
        let base = AcAllowance::from_state(&AcMoverState::vanilla());
        // ~ 0.56 b/t (11 b/s) covers sprint-jumping at 7.1 b/s with slack.
        assert!(base.horizontal > 0.45 && base.horizontal < 0.7, "{base:?}");
        assert!(
            (base.down - 4.2).abs() < 0.1,
            "terminal fall 3.92 + 5 %: {base:?}"
        );
        let mut s = AcMoverState::vanilla();
        s.movement_speed = 0.3;
        s.speed_level = 2;
        let fast = AcAllowance::from_state(&s);
        assert!(fast.horizontal > base.horizontal * 3.5, "{fast:?}");
        let mut s = AcMoverState::vanilla();
        s.movement_speed = 0.01; // slowness / a mod lowering it
        assert_eq!(AcAllowance::from_state(&s), base);
        let mut s = AcMoverState::vanilla();
        s.jump_level = 3;
        s.jump_strength = 0.8;
        assert!(AcAllowance::from_state(&s).up > base.up * 1.8);
        let mut s = AcMoverState::vanilla();
        s.unknown_modifiers = true;
        let lenient = AcAllowance::from_state(&s);
        assert!(lenient.horizontal >= base.horizontal * 2.0 - 1e-9);
        let mut s = AcMoverState::vanilla();
        s.gliding = true;
        assert!(AcAllowance::from_state(&s).horizontal >= 4.0);
    }

    #[test]
    fn credit_absorbs_lag_bursts_but_not_teleports() {
        let p = MovementCheck::policy(AcPreset::Standard).unwrap();
        let a = AcAllowance::from_state(&AcMoverState::vanilla());
        let t0 = Instant::now();
        let mut m = AcMoveState::default();
        assert_eq!(m.evaluate(t0, &a, &p, [0.0; 3]), None);
        // 40 sprint ticks delivered in one burst after a 2 s stall.
        let t1 = t0 + Duration::from_secs(2);
        for _ in 0..40 {
            assert_eq!(m.evaluate(t1, &a, &p, [0.36, 0.0, 0.0]), None);
        }
        // Sustained sprint-jumping, one packet per tick, for a minute.
        let mut t = t1;
        for _ in 0..1200 {
            t += Duration::from_millis(50);
            assert_eq!(m.evaluate(t, &a, &p, [0.36, 0.0, 0.0]), None);
        }
        // A 100-block jump in one packet.
        t += Duration::from_millis(50);
        assert!(m.evaluate(t, &a, &p, [100.0, 0.0, 0.0]).is_some());
        // 1.6x speed runs dry within seconds.
        let mut caught = false;
        for _ in 0..400 {
            t += Duration::from_millis(50);
            if m.evaluate(t, &a, &p, [0.9, 0.0, 0.0]).is_some() {
                caught = true;
                break;
            }
        }
        assert!(caught);
        // A fresh baseline refills credit but still judges the first move.
        let mut f = AcMoveState::default();
        assert!(f.evaluate(t0, &a, &p, [100.0, 0.0, 0.0]).is_some());
        assert_eq!(f.evaluate(t0, &a, &p, [5.0, 0.0, 0.0]), None);
    }

    #[test]
    fn server_velocity_and_grace_are_spent_before_flagging() {
        let p = MovementCheck::policy(AcPreset::Standard).unwrap();
        let a = AcAllowance::from_state(&AcMoverState::vanilla());
        let t0 = Instant::now();
        let mut m = AcMoveState::default();
        m.evaluate(t0, &a, &p, [0.0; 3]);
        let t = t0 + Duration::from_millis(50);
        // Past the two seconds of burst credit (40 ticks x ~0.56 blocks).
        assert!(m.clone().evaluate(t, &a, &p, [40.0, 0.0, 0.0]).is_some());
        m.grant([24.0, 0.0, 0.0], t0);
        assert_eq!(m.evaluate(t, &a, &p, [40.0, 0.0, 0.0]), None);
        let mut g = AcMoveState::default();
        g.evaluate(t0, &a, &p, [0.0; 3]);
        g.grace_until = Some(t0 + Duration::from_secs(1));
        assert_eq!(g.evaluate(t, &a, &p, [500.0, 0.0, 0.0]), None);
    }

    #[test]
    fn carried_contraption_motion_grants_exact_allowance_without_allowing_teleports() {
        let eid = 900_777;
        {
            let mut map = ac_players().lock().unwrap();
            map.insert(eid, PlayerAc::new("Rider"));
        }
        let p = MovementCheck::policy(AcPreset::Standard).unwrap();
        let a = AcAllowance::from_state(&AcMoverState::vanilla());
        let t0 = Instant::now();
        {
            let mut map = ac_players().lock().unwrap();
            map.get_mut(&eid).unwrap().moves.evaluate(t0, &a, &p, [0.0; 3]);
        }
        // Contraption carries rider by (3.0, 1.5, 0.0) in one tick: allowed cleanly!
        ac_note_carried_motion(eid, [3.0, 1.5, 0.0]);
        let t1 = t0 + Duration::from_millis(50);
        let (carry_res, still_carried, tp_res) = {
            let mut map = ac_players().lock().unwrap();
            let st = &mut map.get_mut(&eid).unwrap().moves;
            let carry_res = st.evaluate(t1, &a, &p, [3.0, 1.5, 0.0]);
            let still_carried = st.carried_until.is_some_and(|u| t1 < u);
            let tp_res = st.evaluate(t1, &a, &p, [120.0, 0.0, 0.0]);
            (carry_res, still_carried, tp_res)
        };
        ac_forget(eid);
        assert_eq!(carry_res, None, "carried contraption step must not flag");
        assert!(still_carried, "carried rider stays supported against false NoFall");
        assert!(tp_res.is_some(), "real 120-block teleport must still flag");
    }
}
