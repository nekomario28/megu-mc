// Anti-cheat foundation: the server decides the limits, the operator only switches things.
//
// `server.toml` has four anti-cheat keys: `antixray`, `antifullbright`,
// `hide_underground` and `check_ops`. The movement, fall and placement checks are always on (only
// `MEGU_ANTICHEAT=off`, meant for tests and development, turns them off). Every
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
/// are always on unless `MEGU_ANTICHEAT=off` (tests, development) or an old file
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
    /// Send deep sections as solid host rock to players still on the surface, so
    /// x-ray clients cannot see caves, mineshafts or strongholds at all
    /// (see `crate::ac_underground_occlusion_enabled`). Needs `anti_xray`.
    pub hide_underground: bool,
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
            hide_underground: true,
            notes: Vec::new(),
        }
    }
}

fn ac_env_bool(v: &str) -> bool {
    crate::anti_xray_value_enabled(v)
}

impl AcConfig {
    /// `[anticheat]` from the parsed file, then the environment on top:
    /// `MEGU_ANTICHEAT` (on/off), `MEGU_ANTI_XRAY`, `MEGU_ANTI_FULLBRIGHT`,
    /// `MEGU_ANTI_XRAY_UNDERGROUND`.
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
        let mut hide_underground = None;
        if let Some(ServerConfigValue::Bool(b)) = get("hide_underground") {
            hide_underground = Some(*b);
        }
        for key in [
            "enabled",
            "antixray",
            "antifullbright",
            "check_ops",
            "hide_underground",
        ] {
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
        if let Some(b) = hide_underground {
            c.hide_underground = b;
        }
        if let Some(v) = env("MEGU_ANTICHEAT") {
            match AcPreset::parse(&v) {
                Some(p) => c.preset = p,
                None => c.notes.push(format!(
                    "[anticheat] MEGU_ANTICHEAT={v:?} is not on/off; ignored"
                )),
            }
        }
        if let Some(v) = env("MEGU_ANTI_XRAY") {
            c.anti_xray = ac_env_bool(&v);
        }
        if let Some(v) = env("MEGU_ANTI_FULLBRIGHT") {
            c.anti_fullbright = ac_env_bool(&v);
        }
        if let Some(v) = env("MEGU_ANTI_XRAY_UNDERGROUND") {
            c.hide_underground = ac_env_bool(&v);
        }
        c
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "checks={} antixray={} antifullbright={} hideunderground={} check_ops={}{}{}",
            self.preset.name(),
            self.anti_xray,
            self.anti_fullbright,
            self.hide_underground,
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
static AC_ANTI_XRAY_UNDERGROUND: AtomicBool = AtomicBool::new(true);

/// Swap in new settings (boot and `/anticheat reload`).
fn ac_install(config: AcConfig) {
    AC_ANTI_XRAY.store(config.anti_xray, Ordering::Relaxed);
    AC_ANTI_FULLBRIGHT.store(config.anti_fullbright, Ordering::Relaxed);
    AC_ANTI_XRAY_UNDERGROUND.store(config.hide_underground, Ordering::Relaxed);
    for note in &config.notes {
        eprintln!("{note}");
    }
    eprintln!("[anticheat] {}", config.summary());
    *AC_CONFIG.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(config));
}

fn ac_config() -> Arc<AcConfig> {
    if let Some(c) = AC_CONFIG.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
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
        anti_xray: std::env::var("MEGU_ANTI_XRAY")
            .map(|v| ac_env_bool(&v))
            .unwrap_or(true),
        anti_fullbright: std::env::var("MEGU_ANTI_FULLBRIGHT")
            .map(|v| ac_env_bool(&v))
            .unwrap_or(!cfg!(test)),
        // Same fixture rule as anti-fullbright: tests keep byte-identical chunks
        // unless a fixture opts the occlusion in explicitly.
        hide_underground: std::env::var("MEGU_ANTI_XRAY_UNDERGROUND")
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
    #[cfg(test)]
    if let Some(v) = TEST_XRAY.with(|c| c.get()) {
        return v;
    }
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

/// Whether the surface player's chunk lane occludes deep sections: the section
/// mask is part of anti-xray, so `antixray = false` turns it off too. Read on
/// the per-player lane decision, not per section.
fn ac_underground_occlusion_enabled() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_UNDERGROUND.with(|c| c.get()) {
        return v && ac_anti_xray_enabled();
    }
    if !ac_booted() {
        static UNBOOTED: OnceLock<bool> = OnceLock::new();
        let on = *UNBOOTED.get_or_init(|| ac_unbooted_default().hide_underground);
        return ac_anti_xray_enabled() && on;
    }
    ac_anti_xray_enabled() && AC_ANTI_XRAY_UNDERGROUND.load(Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    static TEST_XRAY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static TEST_FULLBRIGHT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static TEST_UNDERGROUND: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Per-thread override so a test can flip the anti-xray master switch without
/// changing what concurrent tests see; `None` restores the configured default.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn ac_test_set_anti_xray(on: Option<bool>) {
    TEST_XRAY.with(|c| c.set(on));
}

/// Per-thread override so a light test does not change what concurrent tests see.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn ac_test_set_anti_fullbright(on: Option<bool>) {
    TEST_FULLBRIGHT.with(|c| c.set(on));
}

/// Per-thread override so an occlusion test does not change what concurrent
/// tests see; `None` restores the configured default.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn ac_test_set_underground_occlusion(on: Option<bool>) {
    TEST_UNDERGROUND.with(|c| c.set(on));
}

#[cfg(test)]
static TEST_PRESETS: Mutex<Vec<(String, AcPreset)>> = Mutex::new(Vec::new());

/// Tests run many players against one in-process server; a per-name preset keeps an
/// anti-cheat fixture from changing what concurrent fixtures see.
#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn ac_test_set_player_preset(name: &str, preset: AcPreset) {
    let mut v = TEST_PRESETS.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    v.push((name.to_owned(), preset));
}

fn ac_preset_for(name: &str) -> AcPreset {
    let _ = name; // only the test override is per name; exemptions live in `exemption`
    #[cfg(test)]
    if let Some((_, p)) = TEST_PRESETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
    Attack,
    Protocol,
}

impl AcCheckId {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const ALL: [AcCheckId; 5] = [
        AcCheckId::Place,
        AcCheckId::Movement,
        AcCheckId::NoFall,
        AcCheckId::Attack,
        AcCheckId::Protocol,
    ];
    fn index(self) -> usize {
        self as usize
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            AcCheckId::Place => "place",
            AcCheckId::Movement => "movement",
            AcCheckId::NoFall => "nofall",
            AcCheckId::Attack => "attack",
            AcCheckId::Protocol => "protocol",
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
pub(crate) struct AttackCheck;
pub(crate) struct ProtocolCheck;

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

impl AcCheck for AttackCheck {
    const ID: AcCheckId = AcCheckId::Attack;
    const ACTION: Action = Action::Cancel;
    fn policy(preset: AcPreset) -> Option<AcPolicy> {
        match preset {
            AcPreset::Off => None,
            // A single odd hit is evidence, not grounds to cancel legitimate PvP.
            AcPreset::Standard => ac_pol(1.0, 0.0, 1.0),
        }
    }
}

impl AcCheck for ProtocolCheck {
    const ID: AcCheckId = AcCheckId::Protocol;
    const ACTION: Action = Action::Cancel;
    fn policy(preset: AcPreset) -> Option<AcPolicy> {
        match preset {
            AcPreset::Off => None,
            // FSM/CUSUM violations are cumulative evidence: one odd packet is
            // noise, a stream of impossible orderings is a client.
            AcPreset::Standard => ac_pol(1.0, 0.0, 0.5),
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
const AC_REGISTRY: [AcRegistered; 5] = [
    ac_register::<PlaceCheck>(),
    ac_register::<MovementCheck>(),
    ac_register::<NoFallCheck>(),
    ac_register::<AttackCheck>(),
    ac_register::<ProtocolCheck>(),
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
    if let Some(map) = attrs().lock().unwrap_or_else(|e| e.into_inner()).get(&format!("p:{}", me.name)) {
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
    for e in me.effs.lock().unwrap_or_else(|e| e.into_inner()).iter() {
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
        let inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let held = *me.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, comps)) = inv.get(&8) {
            s.depth_strider = enchant_level(comps, 7);
            s.soul_speed = enchant_level(comps, 35);
        }
        s.riptide = [36 + held, 45]
            .iter()
            .filter_map(|slot| inv.get(slot))
            .any(|(_, _, comps)| enchant_level(comps, 31) > 0);
    }
    s.gliding = *me.fall_fly.lock().unwrap_or_else(|e| e.into_inner());
    s.game_mode = *me.gm.lock().unwrap_or_else(|e| e.into_inner());
    let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
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
        for ((need, bonus), credit) in need.iter().zip(&mut self.bonus).zip(&mut self.credit) {
            let from_bonus = need.min(*bonus);
            *bonus -= from_bonus;
            *credit = (*credit - (need - from_bonus)).max(-MOVE_TOLERANCE);
        }
        None
    }

    fn grant(&mut self, extra: [f64; 3], now: Instant) {
        for (bonus, extra) in self.bonus.iter_mut().zip(extra) {
            *bonus = (*bonus + extra).min(BONUS_CAP);
        }
        self.bonus_until = Some(now + Duration::from_secs_f64(BONUS_SECS));
    }
}

/// Per-player state of the `megu_core::anticheat_v2` engine: the
/// confirmed-RTT transaction queue, the behavioural CUSUM channels, the
/// packet-order FSM and the mod-declared motion grant — all bounded, all
/// updated from the live packet checks below.
#[derive(Debug)]
struct PlayerAcV2 {
    tx: megu_core::anticheat_v2::TransactionEchoQueue<32>,
    cusum: megu_core::anticheat_v2::BehavioralCusumDetector,
    fsm: megu_core::anticheat_v2::ProtocolInvariantFsm,
    grant: Option<megu_core::anticheat_v2::AcModGrantToken>,
    last_attack_at: Option<Instant>,
    last_attack_yaw: Option<f32>,
    last_dig_at: Option<Instant>,
    last_place_at: Option<Instant>,
}

impl PlayerAcV2 {
    fn new() -> Self {
        Self {
            tx: megu_core::anticheat_v2::TransactionEchoQueue::new(),
            cusum: megu_core::anticheat_v2::BehavioralCusumDetector::new(),
            fsm: megu_core::anticheat_v2::ProtocolInvariantFsm::new(),
            grant: None,
            last_attack_at: None,
            last_attack_yaw: None,
            last_dig_at: None,
            last_place_at: None,
        }
    }
}

#[derive(Debug)]
struct PlayerAc {
    #[cfg_attr(not(test), allow(dead_code))]
    name: String,
    vl: [f64; 5],
    vl_at: [Option<Instant>; 5],
    alert_at: [Option<Instant>; 5],
    flags: [u64; 5],
    attack_clicks: std::collections::VecDeque<Instant>,
    killaura: AcKillAuraState,
    moves: AcMoveState,
    v2: PlayerAcV2,
    nofall_reported: bool,
    last_game_mode: Option<u8>,
    game_mode_grace_until: Option<Instant>,
}

impl PlayerAc {
    fn new(name: &str) -> PlayerAc {
        PlayerAc {
            name: name.to_owned(),
            vl: [0.0; 5],
            vl_at: [None; 5],
            alert_at: [None; 5],
            flags: [0; 5],
            attack_clicks: std::collections::VecDeque::new(),
            killaura: AcKillAuraState::default(),
            moves: AcMoveState::default(),
            v2: PlayerAcV2::new(),
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

/// Bound on the lag-compensation rewind registry: unknown eids never grow
/// it past this, and `ac_forget` drops departed entries.
const MAX_AC_REWIND_ENTRIES: usize = 512;

fn ac_rewinds(
) -> &'static Mutex<HashMap<i32, megu_core::anticheat_v2::HitboxRewindBuffer<128>>> {
    static R: OnceLock<
        Mutex<HashMap<i32, megu_core::anticheat_v2::HitboxRewindBuffer<128>>>,
    > = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one entity AABB snapshot for lag-compensated reach validation.
fn ac_record_hitbox(eid: i32, tick: u64, aabb: megu_core::bvh::Aabb3) {
    let mut map = ac_rewinds().lock().unwrap_or_else(|e| e.into_inner());
    if !map.contains_key(&eid) && map.len() >= MAX_AC_REWIND_ENTRIES {
        return;
    }
    map.entry(eid)
        .or_insert_with(megu_core::anticheat_v2::HitboxRewindBuffer::new)
        .record(tick, aabb);
}

/// Monotonic milliseconds for the transaction queue — only the *relative*
/// ages matter, so an arbitrary epoch is fine.
fn ac_now_ms() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn ac_with_player<R>(me: &Client, f: impl FnOnce(&mut PlayerAc) -> R) -> R {
    let mut map = ac_players().lock().unwrap_or_else(|e| e.into_inner());
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
    let gm = *me.gm.lock().unwrap_or_else(|e| e.into_inner());
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
    if let Some(map) = ac_rtts().lock().ok().as_mut() {
        map.remove(&eid);
    }
    if let Some(map) = ac_rewinds().lock().ok().as_mut() {
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
    // v2: the velocity grant is a pending transaction until the client
    // echoes progress past it, and an `AcModGrantToken` widens the strict
    // 1-tick envelope by exactly the declared velocity — no blanket
    // exemption needed for knockback, launches or mod carriers.
    p.v2.tx.issue(
        megu_core::anticheat_v2::TxKind::VelocityGrant,
        ac_now_ms(),
    );
    p.v2.grant = Some(megu_core::anticheat_v2::AcModGrantToken::new(v, 40));
}

/// The client echoed a server transaction id (teleport confirm): folds the
/// true round-trip into the EWMA estimate and drains every earlier grant.
fn ac_note_tx_confirm(me: &Client, wire_id: i64) {
    if wire_id < 0 {
        return;
    }
    let now_ms = ac_now_ms();
    let confirmed = ac_with_player(me, |p| {
        p.v2.tx.expire(now_ms, 5_000);
        p.v2.tx.note_echo(wire_id as u64, now_ms)
    });
    if let Some(ms) = confirmed {
        ac_note_player_rtt_ms(me.eid, ms.min(u64::from(u32::MAX)) as u32);
    }
}

/// A transaction with a wire id the client will echo — the teleport
/// confirmation id a prepared/setback teleport already carries.
fn ac_note_tx_grant(eid: i32, wire_id: i64, kind: megu_core::anticheat_v2::TxKind) {
    if wire_id < 0 {
        return;
    }
    if let Some(p) = ac_players()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&eid)
    {
        p.v2.tx.note_grant(wire_id as u64, kind, ac_now_ms());
    }
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
    p.v2.grant = Some(megu_core::anticheat_v2::AcModGrantToken::new(
        [delta[0].abs() + 0.25, delta[1].abs() + 0.25, delta[2].abs() + 0.25],
        5,
    ));
}

/// The server placed the player somewhere: refill credit from the new position.
fn ac_expect_teleport(eid: i32) {
    if let Some(p) = ac_players().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&eid) {
        p.moves.fresh = true;
        // v2: mark a teleport transaction pending — its high-bit id can
        // never collide with a real wire id, and any later client echo
        // drains it in order.
        p.v2.tx.issue(
            megu_core::anticheat_v2::TxKind::Teleport,
            ac_now_ms(),
        );
    }
}

/// Public hook: skip movement checks for `for_time` (custom abilities, cutscenes).
#[allow(dead_code)]
fn ac_grant_grace(eid: i32, for_time: Duration) {
    if let Some(p) = ac_players().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&eid) {
        p.moves.grace_until = Some(Instant::now() + for_time);
    }
}

/// An explosion's knockback is computed client-side from the explosion packet.
fn ac_note_explosion(dim: DimId, pos: [f64; 3], power: f64) {
    let targets: Vec<(i32, (f64, f64, f64))> = client_snapshot()
        .iter()
        .filter(|c| *c.dim.lock().unwrap_or_else(|e| e.into_inner()) == dim)
        .map(|c| {
            let (x, y, z, ..) = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
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

// ---------- lag compensation: bounded grace during real lag ----------
//
// A 50 ms tick that takes 150 ms (autosave, worldgen) or a 300 ms latency burst
// makes an honest client's packets arrive in a clump the movement check would
// read as speed-hacking, so the player rubber-bands for the server's own lag.
// `ac_note_server_tick_ms` feeds the measured tick cost in; when a spike (or a
// player's measured RTT) is inside the transient envelope the movement
// allowance scales proportionally to the missed ticks. Two bounds keep a real
// speed-hacker from hiding in the grace forever: the multiplier is hard-capped
// at `MAX_LAG_COMP_MULTIPLIER`, and a consecutive run of spiked ticks longer
// than `MAX_CONSECUTIVE_LAG_SPIKES` ends the grace outright until a healthy
// tick resets it. A healthy server and a quiet connection multiply by exactly
// 1.0, so the normal path is untouched.

/// Tick work at or under this is healthy (20 TPS is 50 ms; small jitter is not lag).
const LAG_TICK_HEALTHY_MS: u32 = 55;
/// Above this a tick spike is no longer "brief" - the cap clamps the scaling.
const LAG_TICK_SPIKE_MAX_MS: u32 = 500;
/// Round-trip at or under this needs no compensation.
const LAG_RTT_HEALTHY_MS: u32 = 150;
/// Above this a latency burst is pathological - the cap clamps the scaling.
const LAG_RTT_SPIKE_MAX_MS: u32 = 600;
/// The largest multiplier lag can put on the movement envelope, ever.
#[allow(dead_code)]
pub(crate) const MAX_LAG_COMP_MULTIPLIER: f64 = 3.0;
/// How many consecutive spiked ticks the grace covers before it collapses back
/// to the strict allowance: 200 ticks = 10 s of a sustained lagging server,
/// which is an outage, not a hiccup.
const MAX_CONSECUTIVE_LAG_SPIKES: u32 = 200;

/// Server-wide lag-grace state: the last measured tick cost and how many
/// ticks in a row have run long. The spike counter is what bounds abuse -
/// while it is small the server is having a moment, while it is huge the
/// server is degraded and cheats are judged strictly again.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LagCompensationState {
    last_tick_ms: u32,
    consecutive_spike_ticks: u32,
}

impl LagCompensationState {
    /// Record one measured tick duration (ms). A healthy tick resets the
    /// consecutive-spike counter; a spiked one grows it.
    fn note_tick(&mut self, tick_ms: u32) {
        self.last_tick_ms = tick_ms;
        if tick_ms > LAG_TICK_HEALTHY_MS {
            self.consecutive_spike_ticks = self.consecutive_spike_ticks.saturating_add(1);
        } else {
            self.consecutive_spike_ticks = 0;
        }
    }

    /// The grace window is closed once spikes run longer than the bound: a
    /// server that has been lagging for `MAX_CONSECUTIVE_LAG_SPIKES` ticks
    /// straight stops granting movement leniency until a healthy tick.
    fn spike_window_open(&self) -> bool {
        self.consecutive_spike_ticks <= MAX_CONSECUTIVE_LAG_SPIKES
    }

    /// The envelope multiplier for `tick_ms`/`player_rtt_ms` under this state's
    /// spike history: exactly 1.0 when nothing is lagging, proportional to the
    /// missed ticks while lagging, hard-capped, and collapsed back to 1.0 once
    /// the consecutive-spike window runs out.
    fn multiplier(&self, tick_ms: u32, player_rtt_ms: u32) -> f64 {
        if tick_ms <= LAG_TICK_HEALTHY_MS && player_rtt_ms <= LAG_RTT_HEALTHY_MS {
            return 1.0;
        }
        if tick_ms > LAG_TICK_HEALTHY_MS && !self.spike_window_open() {
            return 1.0;
        }
        // One missed tick per 50 ms of overrun: a 150 ms tick admits ~3x burst.
        // Past `LAG_TICK_SPIKE_MAX_MS` the hard cap is already saturated.
        let tick_mult = if tick_ms > LAG_TICK_HEALTHY_MS {
            tick_ms.min(LAG_TICK_SPIKE_MAX_MS) as f64 / 50.0
        } else {
            1.0
        };
        // One burst tick per 200 ms of round-trip: 600 ms admits ~3x burst.
        let rtt_mult = if player_rtt_ms > LAG_RTT_HEALTHY_MS {
            player_rtt_ms.min(LAG_RTT_SPIKE_MAX_MS) as f64 / 200.0
        } else {
            1.0
        };
        tick_mult.max(rtt_mult).min(MAX_LAG_COMP_MULTIPLIER)
    }

    /// `base_max_speed` widened by `multiplier`: returned untouched (exact
    /// `base_max_speed`) on the healthy path.
    fn effective_speed_allowance(
        &self,
        base_max_speed: f64,
        tick_ms: u32,
        player_rtt_ms: u32,
    ) -> f64 {
        if !base_max_speed.is_finite() || base_max_speed <= 0.0 {
            return base_max_speed;
        }
        let mult = self.multiplier(tick_ms, player_rtt_ms);
        if mult <= 1.0 {
            base_max_speed
        } else {
            base_max_speed * mult
        }
    }
}

fn ac_lag_comp() -> &'static Mutex<LagCompensationState> {
    static S: OnceLock<Mutex<LagCompensationState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(LagCompensationState::default()))
}

/// Feed the measured tick-body cost into lag compensation; called once per
/// tick from the tick loop. The value is milliseconds of work, not the period.
#[allow(dead_code)]
pub(crate) fn ac_note_server_tick_ms(tick_ms: u32) {
    ac_lag_comp().lock().unwrap_or_else(|e| e.into_inner()).note_tick(tick_ms);
}

/// The last tick cost the tick loop reported; 0 means "no ticks measured yet"
/// which reads as healthy, so an unwired server keeps the strict envelope.
fn ac_last_tick_ms() -> u32 {
    ac_lag_comp().lock().unwrap_or_else(|e| e.into_inner()).last_tick_ms
}

/// `base_max_speed` as the movement check should treat it right now: exact
/// `base_max_speed` when `tick_ms <= 55` and `player_rtt_ms <= 150`, up to
/// `MAX_LAG_COMP_MULTIPLIER`x during a transient tick spike or latency burst,
/// and strict again once spikes run longer than the bounded window.
#[allow(dead_code)]
pub(crate) fn ac_effective_speed_allowance(
    base_max_speed: f64,
    tick_ms: u32,
    player_rtt_ms: u32,
) -> f64 {
    ac_lag_comp()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .effective_speed_allowance(base_max_speed, tick_ms, player_rtt_ms)
}

/// Server-measured round-trip per player, keyed by entity id. Populated by
/// `ac_note_player_rtt_ms` (keepalive wiring); absent means quiet, not zero-lat.
fn ac_rtts() -> &'static Mutex<HashMap<i32, u32>> {
    static R: OnceLock<Mutex<HashMap<i32, u32>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a player's measured round-trip (ms) for lag compensation.
#[allow(dead_code)]
fn ac_note_player_rtt_ms(eid: i32, rtt_ms: u32) {
    ac_rtts().lock().unwrap_or_else(|e| e.into_inner()).insert(eid, rtt_ms);
}

fn ac_player_rtt_ms(eid: i32) -> u32 {
    ac_rtts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .copied()
        .unwrap_or(0)
}

// ---------- flags, VL, alerts ----------

/// Send one alert line to every operator except the flagged player.
fn ac_notify_ops(me: &Client, line: &str) {
    let packet = system_chat(line);
    for op in client_snapshot() {
        if op.eid != me.eid && player_is_admin(&op.name) {
            if let Ok(mut sock) = op.sock.lock() {
                let _ = send_packet(&mut sock, 0x6c, packet.clone());
            }
        }
    }
}

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
            ac_notify_ops(me, &line);
        }
    }
    true
}

#[cfg_attr(not(test), allow(dead_code))]
fn ac_violation_level(eid: i32, check: AcCheckId) -> f64 {
    let map = ac_players().lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = map.get(&eid) else { return 0.0 };
    let decay = ac_policy(check, ac_preset_for(&p.name))
        .map(|p| p.decay_per_sec)
        .unwrap_or(0.0);
    p.decayed_vl(check, Instant::now(), decay)
}

#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn ac_flag_count(eid: i32, check: AcCheckId) -> u64 {
    ac_players()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .map(|p| p.flags[check.index()])
        .unwrap_or(0)
}

// ---------- melee attack ----------

/// Protocol yaw faces +Z at zero degrees and pitch is positive downwards.
/// Ignore almost coincident positions where a meaningful direction is undefined.
fn attack_angle_off_axis(yaw: f32, pitch: f32, eye: [f64; 3], target: [f64; 3]) -> Option<f64> {
    let delta = [target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]];
    let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
    if distance < 0.75 || !distance.is_finite() || !yaw.is_finite() || !pitch.is_finite() {
        return None;
    }
    let (yaw, pitch) = ((yaw as f64).to_radians(), (pitch as f64).to_radians());
    let facing = [-yaw.sin() * pitch.cos(), -pitch.sin(), yaw.cos() * pitch.cos()];
    let dot = (facing[0] * delta[0] + facing[1] * delta[1] + facing[2] * delta[2]) / distance;
    Some(dot.clamp(-1.0, 1.0).acos().to_degrees())
}

/// Sample the short melee ray; exclude the eye's and target's own block cells.
/// A hitbox can intersect its containing block without that block occluding it.
fn attack_segment_obstructed(
    eye: [f64; 3],
    target: [f64; 3],
    mut solid: impl FnMut(i32, i32, i32) -> bool,
) -> bool {
    let delta = [target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]];
    let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
    if !distance.is_finite() || distance == 0.0 {
        return false;
    }
    let cell = |p: [f64; 3]| (p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32);
    let start_cell = cell(eye);
    let end_cell = cell(target);
    let steps = (distance * 4.0).ceil() as usize;
    for i in 1..steps {
        let t = i as f64 / steps as f64;
        let pos = cell([
            eye[0] + delta[0] * t,
            eye[1] + delta[1] * t,
            eye[2] + delta[2] * t,
        ]);
        if pos != start_cell && pos != end_cell && solid(pos.0, pos.1, pos.2) {
            return true;
        }
    }
    false
}

/// Count clicks in the last second, bounded even under packet floods. The
/// caller supplies time so the 25 CPS threshold can be tested deterministically.
fn attack_click_window(clicks: &mut std::collections::VecDeque<Instant>, now: Instant) -> usize {
    while clicks.front().is_some_and(|t| now.saturating_duration_since(*t) >= Duration::from_secs(1)) {
        clicks.pop_front();
    }
    clicks.push_back(now);
    if clicks.len() > 32 {
        clicks.pop_front();
    }
    clicks.len()
}

/// Accumulate weak evidence across hits, cancelling only sustained anomalies.
/// This lets lag spikes and legitimate fast turns pass without an immediate setback.
fn ac_check_attack(me: &Client, dim: DimId, target: [f64; 3], target_eid: i32) -> bool {
    if ac_policy(AcCheckId::Attack, ac_checked_preset(me)).is_none() {
        return true;
    }
    let now = Instant::now();
    let clicks = ac_with_player(me, |p| attack_click_window(&mut p.attack_clicks, now));
    if clicks > 25 {
        ac_flag(me, AcCheckId::Attack, 1.0, "more than 25 attacks per second");
    }
    let (x, y, z, yaw, pitch, _) = *me.pos.lock().unwrap_or_else(|e| e.into_inner());
    // Canonical pose-aware eye: a crawling or sneaking attacker aims from
    // 0.40/1.27, not a hardcoded standing 1.62.
    let eye = [x, client_eye_y(me, dim, x, y, z), z];
    // v2 behavioural channels — AutoClicker cadence + KillAura yaw snaps,
    // notify evidence exactly like the killaura pass below.
    let v2_flag = ac_with_player(me, |p| {
        let mut out = None;
        if let Some(prev) = p.v2.last_attack_at {
            out = p.v2.cusum.note_click_interval(
                now.saturating_duration_since(prev).as_secs_f64(),
            );
        }
        if let Some(prev_yaw) = p.v2.last_attack_yaw {
            let d = (yaw - prev_yaw).abs() % 360.0;
            let d = if d > 180.0 { 360.0 - d } else { d };
            if out.is_none() {
                out = p.v2.cusum.note_yaw_delta(f64::from(d));
            }
        }
        p.v2.last_attack_at = Some(now);
        p.v2.last_attack_yaw = Some(yaw);
        out
    });
    if let Some(flag) = v2_flag {
        ac_flag(me, AcCheckId::Attack, 1.5, &format!("v2 behavioural {flag:?}"));
    }
    // v2 lag-compensated rewind: NaN-safely test the attack ray against
    // the victim's rewind-interpolated, RTT-fattened box — judged on the
    // *pre-attack* history, then the new position is recorded.
    let tick = TICK_IDX.load(Ordering::Relaxed);
    let rewind_miss = {
        let map = ac_rewinds().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&target_eid).is_some_and(|buf| {
            // One sample cannot rewind; two or more give a real verdict.
            buf.len() >= 2
                && buf
                    .verify_ray_reach(
                        eye,
                        [
                            target[0] - eye[0],
                            target[1] - eye[1],
                            target[2] - eye[2],
                        ],
                        3.6,
                        tick,
                        (ac_player_rtt_ms(me.eid) as f32 / 1000.0 * 3.0)
                            .clamp(0.05, 0.5),
                    )
                    .is_none()
        })
    };
    if rewind_miss {
        ac_flag(me, AcCheckId::Attack, 1.0, "v2 lag-compensated reach miss");
    }
    let feet_y = target[1] - 0.9;
    ac_record_hitbox(
        target_eid,
        tick,
        megu_core::bvh::Aabb3::new(
            [target[0] as f32 - 0.3, feet_y as f32, target[2] as f32 - 0.3],
            [
                target[0] as f32 + 0.3,
                feet_y as f32 + 1.8,
                target[2] as f32 + 0.3,
            ],
        ),
    );
    let angle = attack_angle_off_axis(yaw, pitch, eye, target);
    if angle.is_some_and(|angle| angle > 85.0) {
        ac_flag(me, AcCheckId::Attack, 1.0, "attack outside view angle");
    }
    // OPS-ANTICHEAT-5: notify-first kill-aura heuristic. It observes cadence,
    // yaw snaps, target switching and view-angle misses, but its verdict only
    // ever reaches operators — it never feeds the cancel decision above.
    let target_key = (
        (target[0] * 4.0).round() as i64,
        (target[1] * 4.0).round() as i64,
        (target[2] * 4.0).round() as i64,
    );
    if let Some(alert) =
        ac_with_player(me, |p| p.killaura.record_attack(now, yaw, target_key, angle))
    {
        let line = format!("[AC] {} {alert}", me.name);
        eprintln!("[anticheat] {line}");
        if ac_config().alerts {
            ac_notify_ops(me, &line);
        }
    }
    if let Some(reg) = crate::block_registry::breg() {
        if attack_segment_obstructed(eye, target, |x, y, z| {
            reg.is_solid(block_at_dim(dim, x, y, z))
        }) {
            ac_flag(me, AcCheckId::Attack, 2.0, "attack through solid block");
        }
    }
    ac_violation_level(me.eid, AcCheckId::Attack) < 6.0
}

// ---------- OPS-ANTICHEAT-5: notify-first kill-aura heuristic ----------

/// One observed attack inside the sliding cadence window.
#[derive(Clone, Copy, Debug)]
struct AcAttackSample {
    at: Instant,
    yaw: f32,
    /// Quarter-block-quantized target position: the same entity keeps the
    /// same key while it moves, a different entity almost never collides.
    target: (i64, i64, i64),
}

/// Samples older than this leave the cadence window.
const KA_WINDOW: Duration = Duration::from_secs(1);
/// Hard cap so a packet flood cannot grow the window without bound.
const KA_WINDOW_CAP: usize = 64;
/// Two attacks closer than this with a >120° yaw snap is the aura signature;
/// a single one is normal flick-aim PvP.
const KA_SNAP_WINDOW: Duration = Duration::from_millis(150);
const KA_SNAP_DEG: f64 = 120.0;
const KA_SNAP_VL: f64 = 1.5;
/// Switching targets at the same pace is weaker evidence than a snap.
const KA_SWITCH_VL: f64 = 0.75;
/// Hits landing outside this view angle are weak evidence on their own.
const KA_FOV_DEG: f64 = 75.0;
const KA_FOV_VL: f64 = 0.75;
/// Sustained >10 attacks/second is inhuman for a legitimate client; a short
/// burst stays under the VL alert threshold on its own.
const KA_CPS_LIMIT: usize = 10;
const KA_CPS_VL: f64 = 0.5;
/// VL decays fast: the alert threshold needs several stacked signals, so one
/// snap, one lag burst or one odd hit can never page an operator.
const KA_DECAY_PER_SEC: f64 = 1.5;
const KA_ALERT_VL: f64 = 4.0;
const KA_ALERT_INTERVAL: Duration = Duration::from_secs(5);

/// Per-player kill-aura suspicion. Notify-first by contract: `record_attack`
/// returns `Some(alert)` for operators and nothing here ever cancels a hit or
/// disconnects a player.
#[derive(Debug, Default)]
struct AcKillAuraState {
    window: std::collections::VecDeque<AcAttackSample>,
    vl: f64,
    vl_at: Option<Instant>,
    last_alert_at: Option<Instant>,
}

/// Smallest angle between two protocol yaws, in degrees (0..=180). Callers
/// guard non-finite input; NaN would poison every comparison below.
fn ac_yaw_delta_deg(a: f32, b: f32) -> f64 {
    let d = (a - b).abs().rem_euclid(360.0);
    let d = if d > 180.0 { 360.0 - d } else { d };
    f64::from(d)
}

impl AcKillAuraState {
    /// Record one melee attack. `yaw` is the attacker's view yaw, `target` the
    /// quantized target key and `angle_deg` the server-measured angle between
    /// view and target (`None` when it cannot be computed). Returns the
    /// operator alert once sustained multi-signal suspicion crosses the VL
    /// threshold, at most once per `KA_ALERT_INTERVAL`.
    fn record_attack(
        &mut self,
        now: Instant,
        yaw: f32,
        target: (i64, i64, i64),
        angle_deg: Option<f64>,
    ) -> Option<String> {
        while self
            .window
            .front()
            .is_some_and(|s| now.saturating_duration_since(s.at) >= KA_WINDOW)
        {
            self.window.pop_front();
        }
        let prev = self.window.back().copied();
        let mut gain = 0.0;
        let mut signals: Vec<&'static str> = Vec::new();
        if let Some(p) = prev {
            if now.saturating_duration_since(p.at) < KA_SNAP_WINDOW {
                if yaw.is_finite() && p.yaw.is_finite() && ac_yaw_delta_deg(yaw, p.yaw) > KA_SNAP_DEG
                {
                    gain += KA_SNAP_VL;
                    signals.push("yaw snap");
                }
                if target != p.target {
                    gain += KA_SWITCH_VL;
                    signals.push("target switch");
                }
            }
        }
        self.window.push_back(AcAttackSample { at: now, yaw, target });
        if self.window.len() > KA_WINDOW_CAP {
            self.window.pop_front();
        }
        if self.window.len() > KA_CPS_LIMIT {
            gain += KA_CPS_VL;
            signals.push("attack cadence");
        }
        if angle_deg.is_some_and(|a| a > KA_FOV_DEG) {
            gain += KA_FOV_VL;
            signals.push("out-of-FOV hit");
        }
        if gain <= 0.0 {
            return None;
        }
        let elapsed = self
            .vl_at
            .map(|t| now.saturating_duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        let vl = (self.vl - elapsed * KA_DECAY_PER_SEC).max(0.0) + gain;
        self.vl = vl;
        self.vl_at = Some(now);
        let alert_due = self
            .last_alert_at
            .is_none_or(|t| now.saturating_duration_since(t) >= KA_ALERT_INTERVAL);
        if vl >= KA_ALERT_VL && alert_due {
            self.last_alert_at = Some(now);
            // Re-arm: the next alert has to be re-earned by fresh anomalies
            // instead of riding one sustained burst forever.
            self.vl = KA_ALERT_VL;
            return Some(format!(
                "suspected kill-aura VL {vl:.1}: {}",
                signals.join(", ")
            ));
        }
        None
    }
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
    if ac_policy(AcCheckId::Place, ac_preset_for(&me.name)).is_none() || *me.gm.lock().unwrap_or_else(|e| e.into_inner()) != 0 {
        return;
    }
    let slot = 36 + held_slot;
    let stack = {
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        map_take(&mut inv, slot, 1);
        inv.get(&slot).cloned().unwrap_or_default()
    };
    let mut packet = vec![0u8];
    write_varint(&mut packet, i64::from(inv_state_next(me)));
    packet.extend_from_slice(&slot.to_be_bytes());
    packet.extend_from_slice(&slot_wire(&stack));
    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x15, packet);
}

// ---------- anticheat_v2 packet-order & cadence observers ----------
//
// Every `block_dig`, `use_on`, `entity_action` and item-use packet feeds
// the per-player `ProtocolInvariantFsm` and `BehavioralCusumDetector`.
// Violations flag the `protocol` check — cumulative evidence, never a
// single-packet kick.

/// `block_dig` (0x24): dig-order invariants, sequence monotonicity and the
/// FastBreak cadence channel. `instant_possible` is the server-computed
/// insta-mine verdict for this block+tool (or `true` when the dig could
/// not be judged — a rejected START may still legally precede a FINISH).
fn ac_v2_note_dig(me: &Client, status: i64, instant_possible: bool, seq: i64) {
    if ac_policy(AcCheckId::Protocol, ac_checked_preset(me)).is_none() {
        return;
    }
    let now = Instant::now();
    let (violation, behavioural) = ac_with_player(me, |p| {
        let v = p
            .v2
            .fsm
            .note_dig_status(status, instant_possible)
            .err()
            .or_else(|| p.v2.fsm.note_sequence(seq).err());
        let c = if status == 0 {
            p.v2
                .last_dig_at
                .replace(now)
                .and_then(|prev| {
                    p.v2
                        .cusum
                        .note_dig_interval(now.saturating_duration_since(prev).as_secs_f64())
                })
        } else {
            None
        };
        if status == 5 {
            p.v2.fsm.note_use_item_stop();
        }
        (v, c)
    });
    if let Some(v) = violation {
        ac_flag(
            me,
            AcCheckId::Protocol,
            1.0,
            &format!("v2 protocol invariant {v:?}"),
        );
    }
    if let Some(f) = behavioural {
        ac_flag(me, AcCheckId::Protocol, 1.0, &format!("v2 behavioural {f:?}"));
    }
}

/// `use_on` (0x38): face-enum bounds, sequence monotonicity and the
/// Scaffold placement-cadence/yaw-lock channel.
fn ac_v2_note_use_on(me: &Client, face: i64, seq: i64, yaw: f32) {
    if ac_policy(AcCheckId::Protocol, ac_checked_preset(me)).is_none() {
        return;
    }
    let now = Instant::now();
    let (violation, behavioural) = ac_with_player(me, |p| {
        let v = p
            .v2
            .fsm
            .note_face(face)
            .err()
            .or_else(|| p.v2.fsm.note_sequence(seq).err());
        let c = p
            .v2
            .last_place_at
            .replace(now)
            .and_then(|prev| {
                p.v2.cusum.note_place(
                    now.saturating_duration_since(prev).as_secs_f64(),
                    yaw,
                )
            });
        (v, c)
    });
    if let Some(v) = violation {
        ac_flag(
            me,
            AcCheckId::Protocol,
            1.0,
            &format!("v2 protocol invariant {v:?}"),
        );
    }
    if let Some(f) = behavioural {
        ac_flag(me, AcCheckId::Protocol, 1.0, &format!("v2 behavioural {f:?}"));
    }
}

/// `entity_action` 3/4: sprint flag changes — sprinting while an item use
/// is open is impossible for a vanilla client.
fn ac_v2_note_sprint(me: &Client, sprinting: bool) {
    if ac_policy(AcCheckId::Protocol, ac_checked_preset(me)).is_none() {
        return;
    }
    let violation = ac_with_player(me, |p| p.v2.fsm.note_sprint(sprinting).err());
    if let Some(v) = violation {
        ac_flag(
            me,
            AcCheckId::Protocol,
            1.0,
            &format!("v2 protocol invariant {v:?}"),
        );
    }
}

/// An item use begins (use_item / use_on that activates food, shield,
/// bow): illegal while the sprint flag is still up.
fn ac_v2_note_use_item(me: &Client) {
    if ac_policy(AcCheckId::Protocol, ac_checked_preset(me)).is_none() {
        return;
    }
    let violation = ac_with_player(me, |p| p.v2.fsm.note_use_item().err());
    if let Some(v) = violation {
        ac_flag(
            me,
            AcCheckId::Protocol,
            1.0,
            &format!("v2 protocol invariant {v:?}"),
        );
    }
}

/// Undo a client-side prediction for one cell.
fn ac_resync_block(me: &Client, dim: DimId, (x, y, z): BlockPos) {
    let state = block_at_dim(dim, x, y, z);
    let visible = visible_block_update_state(dim, x, y, z, state);
    let mut body = Vec::new();
    write_pos(&mut body, x, y, z);
    write_varint(&mut body, i64::from(visible));
    let _ = send_packet(&mut me.sock.lock().unwrap_or_else(|e| e.into_inner()), 0x09, body);
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
    mobs().lock().unwrap_or_else(|e| e.into_inner()).iter().any(|m| {
        m.dim == dim && {
            let (mx, my, mz, ..) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
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
    let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
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
        // Lag compensation: a measured tick spike or latency burst widens the
        // burst envelope by the missed ticks (hard-capped and time-bounded, so
        // a real speed hack still runs out). A healthy server and a quiet
        // connection leave the allowance bit-exact.
        let allowance = {
            let lag =
                ac_effective_speed_allowance(1.0, ac_last_tick_ms(), ac_player_rtt_ms(me.eid));
            if lag > 1.0 {
                AcAllowance {
                    horizontal: allowance.horizontal * lag,
                    up: allowance.up * lag,
                    down: allowance.down * lag,
                }
            } else {
                allowance
            }
        };
        let need = [
            (to.0 - from.0).hypot(to.2 - from.2),
            (to.1 - from.1).max(0.0),
            (from.1 - to.1).max(0.0),
        ];
        // v2 strict 1-tick envelope over the same mover state, per elapsed
        // tick, honoring the live `AcModGrantToken`. A move the token
        // bucket absorbed but the deterministic predictor calls impossible
        // is still real evidence.
        let (v2_exempt, verdict) = ac_with_player(me, |ac| {
            let exempt = ac.moves.fresh
                || ac.moves.grace_until.is_some_and(|t| now < t)
                || state.unknown_modifiers;
            let ticks = ac
                .moves
                .last
                .map(|l| now.saturating_duration_since(l).as_secs_f64() * 20.0)
                .unwrap_or(1.0)
                .max(1.0);
            let env = megu_core::anticheat_v2::PhysicsEnv {
                gravity: state.gravity,
                jump_velocity: state.jump_strength
                    + 0.1 * f64::from(state.jump_level.max(0)),
                movement_speed: state.movement_speed,
                speed_level: state.speed_level,
                friction: if state.slippery {
                    megu_core::anticheat_v2::SLIP_ICE
                } else {
                    megu_core::anticheat_v2::SLIP_GROUND
                },
                on_ground: claimed_ground,
                in_water: state.in_fluid,
                in_lava: false,
                on_climbable: false,
                fall_flying: state.gliding || state.riptide,
                levitation_level: state.levitation_level,
                flying: matches!(state.game_mode, 1 | 3),
            };
            let per_tick = [need[0] / ticks, need[1] / ticks, need[2] / ticks];
            let verdict = megu_core::anticheat_v2::PhysicsPredictionStep::evaluate(
                &env,
                per_tick,
                ac.v2.grant.as_ref(),
            );
            if let Some(g) = ac.v2.grant.as_mut() {
                g.tick_down();
                if g.expired() {
                    ac.v2.grant = None;
                }
            }
            (exempt, verdict)
        });
        let excess = ac_with_player(me, |ac| ac.moves.evaluate(now, &allowance, &p, need));
        if let Some(excess) = excess {
            let detail = format!(
                "moved h={:.2} up={:.2} down={:.2}, {excess:.2} over limit {:.2}/{:.2}/{:.2} per tick",
                need[0], need[1], need[2], allowance.horizontal * p.slack, allowance.up * p.slack, allowance.down * p.slack
            );
            if ac_flag(me, AcCheckId::Movement, 1.0 + excess.min(10.0), &detail) {
                return AcMoveVerdict::Setback;
            }
        } else if !v2_exempt && !verdict.within {
            let worst = verdict
                .excess_residual
                .iter()
                .copied()
                .fold(0.0, f64::max);
            if worst > 0.5 {
                ac_flag(
                    me,
                    AcCheckId::Movement,
                    0.5 + worst.min(4.0) * 0.25,
                    &format!("v2 physics envelope exceeded by {worst:.2} blocks/tick"),
                );
            }
        }
        // Lag-compensation rewind: the accepted position becomes a
        // timestamped hitbox snapshot for reach validation.
        ac_record_hitbox(
            me.eid,
            TICK_IDX.load(Ordering::Relaxed),
            megu_core::bvh::Aabb3::new(
                [to.0 as f32 - 0.3, to.1 as f32, to.2 as f32 - 0.3],
                [to.0 as f32 + 0.3, to.1 as f32 + 1.8, to.2 as f32 + 0.3],
            ),
        );
    }
    let mut ground = claimed_ground;
    if nofall.is_some() && claimed_ground {
        let dim = *me.dim.lock().unwrap_or_else(|e| e.into_inner());
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
    let mut sock = me.sock.lock().unwrap_or_else(|e| e.into_inner());
    let mut teleport = me.teleport.lock().unwrap_or_else(|e| e.into_inner());
    let id = teleport.begin();
    ac_note_tx_grant(me.eid, i64::from(id), megu_core::anticheat_v2::TxKind::Teleport);
    *me.pos.lock().unwrap_or_else(|e| e.into_inner()) = to;
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

// ---------- OPS-ANTICHEAT-5: sign text & creative item sanitizers ----------
//
// Pure policy helpers for the sign-edit/chat and creative-inventory packet
// lanes. They sit next to the checks they extend; the packet handlers call
// them when those lanes land, so in non-test builds they are dead code
// and the `ops_ac5_*` tests below are what keep them honest.

/// Vanilla caps one sign line at 384 characters.
#[allow(dead_code)]
const AC_SIGN_MAX_CHARS: usize = 384;

/// `§` formatting codes vanilla recognises: 0-9/a-f colours, k l m n o r.
#[allow(dead_code)]
fn ac_is_format_code(c: char) -> bool {
    matches!(c, '0'..='9' | 'a'..='f' | 'k' | 'l' | 'm' | 'n' | 'o' | 'r')
}

/// The sanitized line plus whether anything had to change, so the packet
/// lane can tell the player (or an operator) cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct AcSignLine {
    pub line: String,
    pub was_sanitized: bool,
}

/// Sign/chat line hardening. ASCII control characters (`< 0x20`, `0x7f`)
/// are always stripped and the line is capped at the vanilla 384 chars.
/// Non-operators (`perm_level < 2`) additionally lose every `§` formatting
/// code — `§` plus its format character — while operators keep legitimate
/// formatting.
#[allow(dead_code)]
pub(crate) fn ac_sanitize_sign_line(line: &str, perm_level: u8) -> AcSignLine {
    let op = perm_level >= 2;
    let mut out = String::with_capacity(line.len().min(AC_SIGN_MAX_CHARS * 4));
    let mut sanitized = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        let code = c as u32;
        if code < 0x20 || code == 0x7f {
            sanitized = true;
            continue;
        }
        if c == '§' {
            if op {
                // Operators keep `§` and whatever follows it verbatim.
                out.push(c);
                continue;
            }
            sanitized = true;
            // Swallow the format character too so "§4red" cannot leave a
            // stray "4red" behind; a `§` before ordinary text only loses
            // the `§` itself.
            if chars.peek().is_some_and(|n| ac_is_format_code(*n)) {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    if out.chars().count() > AC_SIGN_MAX_CHARS {
        out = out.chars().take(AC_SIGN_MAX_CHARS).collect();
        sanitized = true;
    }
    // A truncation boundary can strand a `§` whose code was cut off.
    if out.ends_with('§') {
        out.pop();
        sanitized = true;
    }
    AcSignLine {
        line: out,
        was_sanitized: sanitized,
    }
}

/// Component payloads that are a single unnamed NBT tag — mirrors the
/// `slot_read_nbt` arms of `slot_read_component`. `54`
/// (`minecraft:bees`) trails two VarInts after the tag; any bytes past the
/// parsed tag are preserved verbatim on rewrite.
#[allow(dead_code)]
const AC_NBT_COMPONENTS: [i64; 11] = [0, 19, 27, 36, 37, 38, 39, 43, 54, 55, 56];
/// Nesting deeper than this is a payload bomb, not data.
#[allow(dead_code)]
const AC_NBT_MAX_DEPTH: usize = 16;
/// One component payload larger than this is refused before parsing.
#[allow(dead_code)]
const AC_NBT_MAX_PAYLOAD: usize = 32 * 1024;
/// `|Amount|` beyond this on an AttributeModifiers entry is a hacked item
/// (the 100k-damage sword lives here), not a modifier.
#[allow(dead_code)]
const AC_ATTR_AMOUNT_MAX: f64 = 10_000.0;

/// NBT keys that hand a pasted item server-side power: command blocks,
/// entity spawners and spawn-egg data. Matched at every depth,
/// case-sensitively — the vanilla keys are exact and lookalikes are inert.
#[allow(dead_code)]
fn ac_nbt_key_banned(key: &str) -> bool {
    matches!(key, "Command" | "auto" | "SpawnData" | "SpawnPotentials")
}

/// Whether an AttributeModifiers entry's `Amount` is a finite, sane
/// number. Entries without a numeric amount are left to the recursion.
#[allow(dead_code)]
fn ac_attr_amount_ok(entry: &Tag) -> bool {
    let Tag::Compound(map) = entry else {
        return true;
    };
    for key in ["Amount", "amount"] {
        let Some(tag) = map.get(&NbtString::text(key)) else {
            continue;
        };
        let v = match tag {
            Tag::Double(bits) => f64::from_bits(*bits),
            Tag::Float(bits) => f64::from(f32::from_bits(*bits)),
            Tag::Int(v) => f64::from(*v),
            Tag::Long(v) => *v as f64,
            Tag::Short(v) => f64::from(*v),
            Tag::Byte(v) => f64::from(*v),
            _ => continue,
        };
        return v.is_finite() && v.abs() <= AC_ATTR_AMOUNT_MAX;
    }
    true
}

/// `AttributeModifiers` list: drop entries with non-finite/extreme amounts
/// and sanitize whatever survives like any other node.
#[allow(dead_code)]
fn ac_sanitize_attr_list(tag: &mut Tag, depth: usize, changed: &mut bool) {
    let Tag::List(_, entries) = tag else {
        return;
    };
    let changed = &mut *changed;
    entries.retain_mut(|entry| {
        if !ac_attr_amount_ok(entry) {
            *changed = true;
            return false;
        }
        ac_sanitize_nbt(entry, depth + 1, &mut *changed)
    });
}

/// Strip banned keys, extreme attribute amounts and over-deep subtrees in
/// place. Returns `false` when the node itself must be dropped (it sits
/// deeper than `AC_NBT_MAX_DEPTH`); `changed` records whether the tree
/// actually differs afterwards so clean payloads keep their bytes.
#[allow(dead_code)]
fn ac_sanitize_nbt(tag: &mut Tag, depth: usize, changed: &mut bool) -> bool {
    if depth > AC_NBT_MAX_DEPTH {
        *changed = true;
        return false;
    }
    match tag {
        Tag::Compound(map) => {
            let keys: Vec<NbtString> = map.keys().cloned().collect();
            for key in keys {
                let name = key.to_utf8().ok();
                if name.as_deref().is_some_and(ac_nbt_key_banned) {
                    map.remove(&key);
                    *changed = true;
                    continue;
                }
                let is_attr = name.is_some_and(|k| k == "AttributeModifiers");
                let drop = match map.get_mut(&key) {
                    Some(v @ Tag::List(_, _)) if is_attr => {
                        ac_sanitize_attr_list(v, depth, changed);
                        false
                    }
                    Some(v) => !ac_sanitize_nbt(v, depth + 1, changed),
                    None => false,
                };
                if drop {
                    map.remove(&key);
                }
            }
            true
        }
        Tag::List(_, values) => {
            let changed = &mut *changed;
            values.retain_mut(|v| ac_sanitize_nbt(v, depth + 1, &mut *changed));
            true
        }
        _ => true,
    }
}

/// Sanitize one NBT-bearing component payload. `None` = drop the component
/// outright: oversized, unparseable, or a non-compound root a non-OP
/// creative client has no business sending (fail closed — nested danger
/// cannot be ruled out without a parse).
#[allow(dead_code)]
fn ac_sanitize_nbt_payload(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() > AC_NBT_MAX_PAYLOAD {
        return None;
    }
    let mut rest = data;
    let mut tag = nbt::read_network_nbt(&mut rest).ok()?;
    let mut changed = false;
    ac_sanitize_nbt(&mut tag, 0, &mut changed);
    if !changed {
        return Some(data.to_vec());
    }
    let mut out = nbt::write_network_nbt(&tag).ok()?;
    // `minecraft:bees` (54) and friends trail bytes after the NBT tag.
    out.extend_from_slice(&data[data.len() - rest.len()..]);
    Some(out)
}

/// Clamp a creative-submitted count into `0..=max_stack_size.clamp(1, 99)`.
/// `None` rejects impossible submissions (negative or above the absolute
/// 99 ceiling) outright instead of repairing them into legal-looking
/// stacks.
#[allow(dead_code)]
pub(crate) fn ac_sanitize_creative_count(count: i64, max_stack_size: i64) -> Option<i64> {
    if !(0..=99).contains(&count) {
        return None;
    }
    Some(count.min(max_stack_size.clamp(1, 99)))
}

/// What [`ac_sanitize_creative_item`] changed, for operator notification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct AcCreativeSanitize {
    /// Anything was stripped, clamped or dropped.
    pub was_sanitized: bool,
    /// NBT-bearing components removed outright.
    pub components_removed: usize,
}

/// Creative `set_creative_slot` hardening. The count clamp applies to
/// every submitter; the NBT strip applies only to non-operators
/// (`perm_level < 2`), so creative clients cannot paste command blocks,
/// spawners, spawn data or 100k-damage attribute modifiers into items.
#[allow(dead_code)]
pub(crate) fn ac_sanitize_creative_item(st: &mut Stack, perm_level: u8) -> AcCreativeSanitize {
    let mut out = AcCreativeSanitize::default();
    let max = item_stack_max(st.0, &st.2);
    match ac_sanitize_creative_count(i64::from(st.1), max) {
        Some(c) => {
            let c = c as u8;
            if c != st.1 {
                st.1 = c;
                out.was_sanitized = true;
            }
        }
        None => {
            // Impossible count: empty the stack rather than guess a fix.
            st.1 = 0;
            out.was_sanitized = true;
        }
    }
    if perm_level >= 2 || st.2.is_empty() {
        return out;
    }
    let mut kept: Comps = Vec::with_capacity(st.2.len());
    for (ty, data) in std::mem::take(&mut st.2) {
        if AC_NBT_COMPONENTS.contains(&ty) {
            match ac_sanitize_nbt_payload(&data) {
                Some(clean) => {
                    if clean != data {
                        out.was_sanitized = true;
                    }
                    kept.push((ty, clean));
                }
                None => {
                    out.components_removed += 1;
                    out.was_sanitized = true;
                }
            }
        } else {
            kept.push((ty, data));
        }
    }
    st.2 = kept;
    out
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
            ("MEGU_ANTICHEAT", "off"),
            ("MEGU_ANTI_XRAY", "0"),
            ("MEGU_ANTI_FULLBRIGHT", "false"),
        ];
        let c = cfg("[anticheat]\nantixray = true\n", &env);
        assert_eq!(
            (c.preset, c.anti_xray, c.anti_fullbright),
            (AcPreset::Off, false, false)
        );
        let c = cfg(
            "[anticheat]\nenabled = false\n",
            &[("MEGU_ANTICHEAT", "strict")],
        );
        assert_eq!(c.preset, AcPreset::Standard);
        let c = cfg("", &[("MEGU_ANTICHEAT", "sideways")]);
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
        assert_eq!(ac_action(AcCheckId::Attack), Action::Cancel);
    }

    #[test]
    fn attack_angle_follows_protocol_yaw_and_pitch() {
        let eye = [0.0, 1.62, 0.0];
        assert!(attack_angle_off_axis(0.0, 0.0, eye, [0.0, 1.62, 3.0]).unwrap() < 0.01);
        assert!(attack_angle_off_axis(90.0, 0.0, eye, [-3.0, 1.62, 0.0]).unwrap() < 0.01);
        assert!(attack_angle_off_axis(0.0, 0.0, eye, [0.0, 1.62, -3.0]).unwrap() > 170.0);
        assert_eq!(attack_angle_off_axis(0.0, 0.0, eye, eye), None);
    }

    #[test]
    fn attack_solid_line_of_sight_excludes_target_cell() {
        let eye = [0.5, 1.5, 0.5];
        let target = [3.5, 1.5, 0.5];
        assert!(attack_segment_obstructed(eye, target, |x, _, _| x == 2));
        assert!(!attack_segment_obstructed(eye, target, |x, _, _| x == 3));
        assert!(!attack_segment_obstructed(eye, target, |_, _, _| false));
    }

    #[test]
    fn attack_clicks_keep_bounded_sliding_second() {
        let now = Instant::now();
        let mut clicks = std::collections::VecDeque::new();
        for _ in 0..26 {
            attack_click_window(&mut clicks, now);
        }
        assert_eq!(clicks.len(), 26);
        assert_eq!(attack_click_window(&mut clicks, now + Duration::from_secs(1)), 1);
        for _ in 0..100 {
            attack_click_window(&mut clicks, now + Duration::from_secs(2));
        }
        assert_eq!(clicks.len(), 32);
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
            let mut map = ac_players().lock().unwrap_or_else(|e| e.into_inner());
            map.insert(eid, PlayerAc::new("Rider"));
        }
        let p = MovementCheck::policy(AcPreset::Standard).unwrap();
        let a = AcAllowance::from_state(&AcMoverState::vanilla());
        let t0 = Instant::now();
        {
            let mut map = ac_players().lock().unwrap_or_else(|e| e.into_inner());
            map.get_mut(&eid).unwrap().moves.evaluate(t0, &a, &p, [0.0; 3]);
        }
        // Contraption carries rider by (3.0, 1.5, 0.0) in one tick: allowed cleanly!
        ac_note_carried_motion(eid, [3.0, 1.5, 0.0]);
        let t1 = t0 + Duration::from_millis(50);
        let (carry_res, still_carried, tp_res) = {
            let mut map = ac_players().lock().unwrap_or_else(|e| e.into_inner());
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

    // ---------- OPS-2: lag-compensated anti-rubberband grace ----------

    /// Reset the shared lag state so a test does not inherit another's spikes.
    fn ops2_reset_lag() {
        *ac_lag_comp().lock().unwrap_or_else(|e| e.into_inner()) = LagCompensationState::default();
    }

    /// The healthy path is bit-exact: nothing about the checks gets weaker when
    /// the server and the connection are fine.
    #[test]
    fn ops2_lag_compensation_returns_exact_base_when_healthy() {
        ops2_reset_lag();
        for (tick, rtt) in [(0, 0), (50, 0), (55, 150), (30, 149), (1, 60)] {
            assert_eq!(
                ac_effective_speed_allowance(0.5641, tick, rtt),
                0.5641,
                "tick={tick} rtt={rtt} must not touch the allowance"
            );
        }
        // Non-finite or non-positive bases pass through, never amplified.
        assert_eq!(ac_effective_speed_allowance(0.0, 500, 600), 0.0);
        assert_eq!(ac_effective_speed_allowance(-2.0, 500, 600), -2.0);
        assert!(ac_effective_speed_allowance(f64::NAN, 500, 600).is_nan());
    }

    /// Transient tick spikes and latency bursts scale the envelope by the
    /// missed ticks, always hard-capped at `MAX_LAG_COMP_MULTIPLIER`.
    #[test]
    fn ops2_lag_compensation_scales_by_missed_ticks_bounded_by_cap() {
        ops2_reset_lag();
        // tick_ms/50 = missed ticks: 100 ms admits 2x, 150 ms hits the 3x cap.
        assert_eq!(ac_effective_speed_allowance(0.5, 100, 0), 1.0);
        assert_eq!(ac_effective_speed_allowance(0.5, 150, 0), 1.5);
        assert_eq!(ac_effective_speed_allowance(0.5, 500, 0), 1.5);
        assert_eq!(ac_effective_speed_allowance(0.5, 60_000, 0), 1.5);
        // rtt_ms/200: 300 ms admits 1.5x, 600 ms hits the cap.
        assert_eq!(ac_effective_speed_allowance(0.5, 0, 300), 0.75);
        assert_eq!(ac_effective_speed_allowance(0.5, 0, 600), 1.5);
        assert_eq!(ac_effective_speed_allowance(0.5, 0, 999_999), 1.5);
        // Sources take the max under the cap, never multiply together past it.
        assert_eq!(ac_effective_speed_allowance(0.5, 500, 600), 1.5);
        // Just inside the transient bands still helps a little.
        assert!(ac_effective_speed_allowance(0.5, 60, 0) > 0.5);
        assert!(ac_effective_speed_allowance(0.5, 0, 250) > 0.5);
    }

    /// The consecutive-spike window bounds abuse: once spikes run past
    /// `MAX_CONSECUTIVE_LAG_SPIKES` the grace collapses to strict until a
    /// healthy tick resets it.
    #[test]
    fn ops2_lag_compensation_spike_window_bounds_abuse() {
        let mut state = LagCompensationState::default();
        state.note_tick(120);
        assert!(state.multiplier(120, 0) > 1.0, "one spike grants grace");
        for _ in 0..MAX_CONSECUTIVE_LAG_SPIKES {
            state.note_tick(120);
        }
        // 201 consecutive spiked ticks = an outage, not a hiccup: strict again.
        assert_eq!(state.effective_speed_allowance(0.5, 120, 0), 0.5);
        assert_eq!(state.multiplier(300, 0), 1.0);
        // A single healthy tick reopens the window for the next real hiccup.
        state.note_tick(50);
        state.note_tick(120);
        assert!(state.effective_speed_allowance(0.5, 120, 0) > 0.5);
    }

    /// The global feed: `ac_note_server_tick_ms` and `ac_note_player_rtt_ms`
    /// drive what `ac_check_move` reads. Done in one test so parallel tests
    /// never see half-set shared state.
    #[test]
    fn ops2_lag_compensation_tick_feed_and_rtt_hook_drive_allowance() {
        // `ops2_reset_lag`/`ac_note_server_tick_ms` write the shared lag feed
        // `wire_ops_game_tick` asserts on — serialize against it.
        let _guard = crate::test_support::global_world_test_guard();
        ops2_reset_lag();
        assert_eq!(ac_last_tick_ms(), 0, "no ticks reported reads as healthy");
        ac_note_server_tick_ms(200);
        assert_eq!(ac_last_tick_ms(), 200);
        // The same value the move check passes in now scales the envelope.
        assert_eq!(
            ac_effective_speed_allowance(0.5, ac_last_tick_ms(), 0),
            1.5
        );
        ac_note_server_tick_ms(40);
        assert_eq!(ac_last_tick_ms(), 40);
        assert_eq!(ac_effective_speed_allowance(0.5, ac_last_tick_ms(), 0), 0.5);

        let eid = 900_778;
        ac_note_player_rtt_ms(eid, 300);
        assert_eq!(ac_player_rtt_ms(eid), 300);
        assert_eq!(
            ac_effective_speed_allowance(0.5, 50, ac_player_rtt_ms(eid)),
            0.75
        );
        ac_forget(eid);
        assert_eq!(ac_player_rtt_ms(eid), 0);
        assert_eq!(
            ac_effective_speed_allowance(0.5, 50, ac_player_rtt_ms(eid)),
            0.5
        );
        ops2_reset_lag();
    }

    /// End to end through the token bucket: a legit player whose movement
    /// clumped during a spike is absorbed by the widened envelope instead of
    /// rubber-banding, while a sustained speed-hacker still drains the capped
    /// envelope and gets flagged.
    #[test]
    fn ops2_lag_compensation_prevents_rubberband_but_catches_sustained_hacks() {
        ops2_reset_lag();
        let p = MovementCheck::policy(AcPreset::Standard).unwrap();
        let base = AcAllowance::from_state(&AcMoverState::vanilla());
        // A 150 ms spike multiplies the envelope by exactly the cap.
        let mult = ac_effective_speed_allowance(1.0, 150, 0);
        assert_eq!(mult, MAX_LAG_COMP_MULTIPLIER);
        let lagged = AcAllowance {
            horizontal: base.horizontal * mult,
            up: base.up * mult,
            down: base.down * mult,
        };

        // Legit player: ~2.6x the vanilla rate for a while (packets clumped by
        // the lag). Strict limits flag it quickly - a rubber-band - but the
        // lag-compensated envelope absorbs the whole burst.
        let honest = [base.horizontal * 2.6, 0.0, 0.0];
        let t0 = Instant::now();
        let mut strict = AcMoveState::default();
        strict.evaluate(t0, &base, &p, [0.0; 3]);
        let mut flagged = false;
        let mut t = t0;
        for _ in 0..60 {
            t += Duration::from_millis(50);
            if strict.evaluate(t, &base, &p, honest).is_some() {
                flagged = true;
                break;
            }
        }
        assert!(flagged, "2.6x sustained must flag under strict limits");
        let mut compensated = AcMoveState::default();
        compensated.evaluate(t0, &lagged, &p, [0.0; 3]);
        let mut t = t0;
        for _ in 0..200 {
            t += Duration::from_millis(50);
            assert_eq!(compensated.evaluate(t, &lagged, &p, honest), None);
        }

        // Sustained speed-hack at 4x: over the 3x cap, so credit still drains
        // every tick and flags well inside the grace window.
        let hack = [base.horizontal * 4.0, 0.0, 0.0];
        let mut hacker = AcMoveState::default();
        hacker.evaluate(t0, &lagged, &p, [0.0; 3]);
        let mut t = t0;
        let mut caught = false;
        for _ in 0..400 {
            t += Duration::from_millis(50);
            if hacker.evaluate(t, &lagged, &p, hack).is_some() {
                caught = true;
                break;
            }
        }
        assert!(caught, "4x sustained hack must flag even at max lag comp");
        ops2_reset_lag();
    }

    // ---------- OPS-ANTICHEAT-5: sign/creative sanitizers + kill-aura ----------

    #[test]
    fn ops_ac5_sign_strips_section_codes_for_non_op_and_allows_op() {
        use super::{ac_sanitize_sign_line, AcSignLine};
        // Non-OP: `§` plus its format char is gone, literal text survives.
        let r = ac_sanitize_sign_line("§4red §lbold\u{7}bell", 0);
        assert_eq!(r.line, "red boldbell");
        assert!(r.was_sanitized);
        // A `§` before ordinary text loses only the `§` itself.
        let r = ac_sanitize_sign_line("costs §5 gold", 1);
        assert_eq!(r.line, "costs  gold");
        assert!(r.was_sanitized);
        // Control chars are stripped for everyone; OP keeps `§` formatting.
        let r = ac_sanitize_sign_line("§4red\u{1}§l!", 2);
        assert_eq!(r.line, "§4red§l!");
        assert!(r.was_sanitized);
        let r = ac_sanitize_sign_line("plain §akill\u{7f}", 4);
        assert_eq!(r.line, "plain §akill");
        assert!(r.was_sanitized);
        // Vanilla's 384-char cap applies to both permission levels.
        let long = "x".repeat(500);
        for level in [0, 3] {
            let r = ac_sanitize_sign_line(&long, level);
            assert_eq!(r.line.chars().count(), 384, "level {level}");
            assert!(r.was_sanitized);
        }
        // Truncation can strand a `§`; it must not survive the cap.
        let mut edge = "y".repeat(383);
        edge.push('§');
        edge.push('4');
        edge.push('z');
        let r = ac_sanitize_sign_line(&edge, 2);
        assert_eq!(r.line.chars().count(), 383);
        assert!(!r.line.ends_with('§'));
        assert!(r.was_sanitized);
        // A clean line is reported untouched.
        assert_eq!(
            ac_sanitize_sign_line("hello", 0),
            AcSignLine {
                line: "hello".to_owned(),
                was_sanitized: false
            }
        );
    }

    /// Build an unnamed-compound NBT payload the way a component carries it.
    fn ops5_nbt_compound(entries: &[(&str, crate::nbt::Tag)]) -> Vec<u8> {
        let mut map = std::collections::BTreeMap::new();
        for (k, v) in entries {
            map.insert(crate::nbt::NbtString::text(k), v.clone());
        }
        crate::nbt::write_network_nbt(&crate::nbt::Tag::Compound(map)).unwrap()
    }

    #[test]
    fn ops_ac5_creative_clamps_stack_count_and_strips_dangerous_nbt() {
        use super::{ac_sanitize_creative_count, ac_sanitize_creative_item};
        // Count policy: legal counts clamp to the stack max; impossible ones
        // (negative, >99) are rejected outright.
        assert_eq!(ac_sanitize_creative_count(50, 64), Some(50));
        assert_eq!(ac_sanitize_creative_count(50, 1), Some(1));
        assert_eq!(ac_sanitize_creative_count(0, 64), Some(0));
        assert_eq!(ac_sanitize_creative_count(-1, 64), None);
        assert_eq!(ac_sanitize_creative_count(100, 64), None);
        assert_eq!(ac_sanitize_creative_count(99, 0), Some(1));
        // On a real stack the wire count is u8; >99 empties the stack.
        let stone = name_to_item("stone").unwrap();
        let mut st: Stack = (stone, 100, Vec::new());
        let r = ac_sanitize_creative_item(&mut st, 0);
        assert_eq!(st.1, 0);
        assert!(r.was_sanitized);
        // Non-OP: banned keys are stripped at any depth, safe data survives.
        let mut data_map = std::collections::BTreeMap::new();
        data_map.insert(
            crate::nbt::NbtString::text("Command"),
            crate::nbt::Tag::String(crate::nbt::NbtString::text("op attacker")),
        );
        data_map.insert(
            crate::nbt::NbtString::text("safe"),
            crate::nbt::Tag::Int(7),
        );
        let inner = crate::nbt::Tag::Compound(data_map);
        let payload = ops5_nbt_compound(&[
            ("wrapper", crate::nbt::Tag::Compound(
                [(crate::nbt::NbtString::text("deep"), inner.clone())]
                    .into_iter()
                    .collect(),
            )),
            ("SpawnPotentials", crate::nbt::Tag::List(10, vec![inner.clone()])),
        ]);
        let mut st: Stack = (stone, 5, vec![(0, payload.clone())]);
        let r = ac_sanitize_creative_item(&mut st, 0);
        assert!(r.was_sanitized);
        assert_eq!(r.components_removed, 0);
        let mut rest = st.2[0].1.as_slice();
        let tag = crate::nbt::read_network_nbt(&mut rest).unwrap();
        let root = tag.get("wrapper").unwrap();
        // "deep" -> {Command, safe}: Command gone even two levels down.
        let deep = root.get("deep").unwrap();
        assert!(deep.get("Command").is_none());
        assert_eq!(deep.get("safe").and_then(|t| t.int()), Some(7));
        // SpawnPotentials removed at the root.
        assert!(tag.get("SpawnPotentials").is_none());
        // Extreme attribute amounts are dropped, sane ones kept.
        let mut mod_ok = std::collections::BTreeMap::new();
        mod_ok.insert(
            crate::nbt::NbtString::text("Amount"),
            crate::nbt::Tag::Double(2.5f64.to_bits()),
        );
        let mut mod_bad = std::collections::BTreeMap::new();
        mod_bad.insert(
            crate::nbt::NbtString::text("Amount"),
            crate::nbt::Tag::Double(100_000_000.0f64.to_bits()),
        );
        let attr = ops5_nbt_compound(&[(
            "AttributeModifiers",
            crate::nbt::Tag::List(
                10,
                vec![
                    crate::nbt::Tag::Compound(mod_ok),
                    crate::nbt::Tag::Compound(mod_bad),
                ],
            ),
        )]);
        let mut st: Stack = (stone, 1, vec![(0, attr)]);
        let r = ac_sanitize_creative_item(&mut st, 1);
        assert!(r.was_sanitized);
        let mut rest = st.2[0].1.as_slice();
        let tag = crate::nbt::read_network_nbt(&mut rest).unwrap();
        let list = tag.get("AttributeModifiers").unwrap().list().unwrap();
        assert_eq!(list.len(), 1, "only the sane modifier survives");
        // Non-compound or oversized NBT payloads are dropped fail-closed.
        let string_root = vec![8u8, 0, 3, b'f', b'o', b'o'];
        let mut st: Stack = (stone, 1, vec![(0, string_root)]);
        let r = ac_sanitize_creative_item(&mut st, 0);
        assert_eq!(r.components_removed, 1);
        assert!(st.2.is_empty());
        let huge = vec![10u8; 40 * 1024];
        let mut st: Stack = (stone, 1, vec![(0, huge)]);
        let r = ac_sanitize_creative_item(&mut st, 0);
        assert_eq!(r.components_removed, 1);
        // OP keeps the dangerous NBT untouched (count clamp still applies).
        let mut st: Stack = (stone, 1, vec![(0, payload)]);
        let before = st.2.clone();
        let r = ac_sanitize_creative_item(&mut st, 4);
        assert!(!r.was_sanitized);
        assert_eq!(st.2, before);
    }

    #[test]
    fn ops_ac5_killaura_ignores_normal_pvp_and_alerts_sustained_snap_switch() {
        let t0 = Instant::now();
        // Normal PvP: ~2 attacks/second, small yaw drift, one target, hits in
        // view — no signal ever accumulates.
        let mut ka = AcKillAuraState::default();
        for i in 0..20 {
            let now = t0 + Duration::from_millis(500 * i);
            assert_eq!(
                ka.record_attack(now, 10.0 + i as f32 * 0.5, (4, 8, 12), Some(5.0)),
                None,
                "attack {i}"
            );
        }
        // A lag burst of identical attacks is cadence noise, not an aura.
        // (Well past the PvP samples above so the 1 s window starts empty.)
        for i in 0..8 {
            assert_eq!(
                ka.record_attack(t0 + Duration::from_secs(20), 10.0, (4, 8, 12), None),
                None,
                "burst {i}"
            );
        }
        // One yaw snap plus one target switch plus one out-of-FOV hit is
        // evidence — annoying but never an alert on its own.
        let mut odd = AcKillAuraState::default();
        assert_eq!(odd.record_attack(t0, 0.0, (1, 1, 1), None), None);
        assert_eq!(
            odd.record_attack(t0 + Duration::from_millis(100), 200.0, (9, 9, 9), Some(80.0)),
            None
        );
        // Sustained aura cadence: snap + switch + out-of-FOV every 80 ms.
        // VL crosses the alert threshold within a few hits and pages once.
        let mut aura = AcKillAuraState::default();
        let mut alerts = Vec::new();
        for i in 0..12 {
            let now = t0 + Duration::from_millis(80 * i);
            let (yaw, target) = if i % 2 == 0 { (0.0, (1, 1, 1)) } else { (200.0, (9, 9, 9)) };
            if let Some(alert) = aura.record_attack(now, yaw, target, Some(80.0)) {
                alerts.push((i, alert));
            }
        }
        assert_eq!(alerts.len(), 1, "exactly one alert inside 12 hits: {alerts:?}");
        assert!(alerts[0].1.contains("kill-aura"));
        assert!(alerts[0].0 >= 2, "single/double hits must not alert: {alerts:?}");
        // The 5 s re-arm means a continuing burst cannot spam operators.
        let mut spam = AcKillAuraState::default();
        let mut count = 0;
        for i in 0..40 {
            let now = t0 + Duration::from_millis(80 * i);
            let (yaw, target) = if i % 2 == 0 { (0.0, (1, 1, 1)) } else { (200.0, (9, 9, 9)) };
            if spam.record_attack(now, yaw, target, Some(80.0)).is_some() {
                count += 1;
            }
        }
        assert!(count <= 2, "40 hits in 3.2 s may page at most twice: {count}");
    }
}
