// `server.toml`: the one optional settings file.
//
// The server runs without it. On first boot a short, commented file is written next to
// the world so an operator can see the few choices there are; every key in it is
// optional. Environment variables (`MEGU_*`) keep working and win over the file, so
// existing launch scripts behave exactly as before.
//
// Only a small TOML subset is read (tables, `key = value` with strings, integers,
// floats, booleans and arrays of those, `#` comments). That covers every key this file
// has; it keeps the dependency tree at its audited size (see `deny.toml`). A line it
// cannot read, or a key it does not know, is reported and skipped: a typo must never
// stop the server.

/// The file written when none exists. Short on purpose: anti-cheat thresholds are
/// constants in code, derived from vanilla physics and each player's attributes, so
/// the file only switches the optional protections on or off.
const SERVER_CONFIG_DEFAULT_FILE: &str = r#"# megu settings. Every key is optional; delete this file to get it back.
# Environment variables (MEGU_*) override anything set here.

[server]
# port = 25565
# motd = "megu"
# max_players = 20
# The Minecraft release this server runs as: "1.21.1" (default) or "26.3".
# target_version = "1.21.1"
# Opt-in: real directional sun shadows (the same sun vector the client GPU
# renders) modulate spawn light, undead burning and crop growth. Off means
# strict vanilla lighting everywhere.
# directional_shadow_light = false
# Opt-in: O(1) unloaded-chunk catch-up (furnaces, crops, headless
# factories tick without a chunk loader). Off keeps strict Vanilla
# unloaded-chunk behavior.
# lazy_catchup = false

[anticheat]
# Enable or disable movement, fall-damage and placement checks (creative/spectator and ops are skipped by default).
enabled = true
# Hide buried ores (vanilla + #c:ores / #forge:ores), chests and buried block-entity NBT from x-ray clients and mods.
antixray = true
# Hide whole deep sections (caves, mineshafts, strongholds) as solid rock until a player descends; needs antixray.
hide_underground = true
# Hide light from sealed underground caves (gamma/fullbright can't be blocked by a server).
antifullbright = true
# Set true to check operators too (creative and spectator players are never checked).
check_ops = false
"#;

const LIVE_VIEW_DISTANCE_DEFAULT: i32 = 8;
const LIVE_SIMULATION_DISTANCE_DEFAULT: i32 = 8;
const LIVE_MAX_PLAYERS_DEFAULT: usize = 20;
const LIVE_PVP_DEFAULT: bool = true;
const LIVE_ALLOW_FLIGHT_DEFAULT: bool = false;

static LIVE_VIEW_DISTANCE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(LIVE_VIEW_DISTANCE_DEFAULT);
static LIVE_SIMULATION_DISTANCE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(LIVE_SIMULATION_DISTANCE_DEFAULT);
static LIVE_MAX_PLAYERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(LIVE_MAX_PLAYERS_DEFAULT);
static LIVE_PVP_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(LIVE_PVP_DEFAULT);
static LIVE_ALLOW_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(LIVE_ALLOW_FLIGHT_DEFAULT);

/// Install the effective operator settings after file, server.properties and
/// environment precedence has been resolved. `whitelist_enforced` is the
/// boot-time seed for the run-time whitelist switch (`/whitelist on|off`
/// keeps flipping it afterwards — see `set_whitelist_enforced`).
pub(crate) fn apply_runtime_server_config(
    view_distance: Option<i32>,
    simulation_distance: Option<i32>,
    max_players: Option<usize>,
    pvp: Option<bool>,
    allow_flight: Option<bool>,
    whitelist_enforced: Option<bool>,
) {
    LIVE_VIEW_DISTANCE.store(
        view_distance.unwrap_or(LIVE_VIEW_DISTANCE_DEFAULT).clamp(2, 32),
        std::sync::atomic::Ordering::Relaxed,
    );
    LIVE_SIMULATION_DISTANCE.store(
        simulation_distance
            .unwrap_or(LIVE_SIMULATION_DISTANCE_DEFAULT)
            .clamp(2, 32),
        std::sync::atomic::Ordering::Relaxed,
    );
    LIVE_MAX_PLAYERS.store(
        max_players.unwrap_or(LIVE_MAX_PLAYERS_DEFAULT).clamp(1, 1000),
        std::sync::atomic::Ordering::Relaxed,
    );
    LIVE_PVP_ENABLED.store(
        pvp.unwrap_or(LIVE_PVP_DEFAULT),
        std::sync::atomic::Ordering::Relaxed,
    );
    LIVE_ALLOW_FLIGHT.store(
        allow_flight.unwrap_or(LIVE_ALLOW_FLIGHT_DEFAULT),
        std::sync::atomic::Ordering::Relaxed,
    );
    if let Some(enforced) = whitelist_enforced {
        set_whitelist_enforced(enforced);
    }
}

/// The server's chunk-tracking radius (`view-distance`, `MEGU_VIEW_DISTANCE`),
/// clamped to Vanilla's `2..=32`.
pub(crate) fn live_view_distance() -> i32 {
    LIVE_VIEW_DISTANCE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The entity-ticking radius (`simulation-distance`,
/// `MEGU_SIMULATION_DISTANCE`), clamped to `2..=32`.
pub(crate) fn live_simulation_distance() -> i32 {
    LIVE_SIMULATION_DISTANCE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The login cap (`max-players`, `MEGU_MAX_PLAYERS`).
pub(crate) fn live_max_players() -> usize {
    LIVE_MAX_PLAYERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Vanilla `pvp` (`MEGU_PVP`): `false` cancels player-vs-player damage.
pub(crate) fn live_pvp_enabled() -> bool {
    LIVE_PVP_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Vanilla `allow-flight` (`MEGU_ALLOW_FLIGHT`): `false` kicks survival and
/// adventure players who hover 80 ticks without falling (see
/// `flight_kick_tick`); default off like Vanilla.
pub(crate) fn live_allow_flight() -> bool {
    LIVE_ALLOW_FLIGHT.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq)]
enum ServerConfigValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<ServerConfigValue>),
}

impl ServerConfigValue {
    pub(crate) fn as_env(&self) -> String {
        match self {
            ServerConfigValue::Str(s) => s.clone(),
            ServerConfigValue::Int(i) => i.to_string(),
            ServerConfigValue::Float(f) => f.to_string(),
            ServerConfigValue::Bool(b) => {
                if *b {
                    "1".into()
                } else {
                    "0".into()
                }
            }
            ServerConfigValue::Array(items) => items
                .iter()
                .map(ServerConfigValue::as_env)
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

/// `table -> key -> value`; the root table is `""`.
pub(crate) type ServerConfigDoc = BTreeMap<String, BTreeMap<String, ServerConfigValue>>;

/// Parse the TOML subset. Returns the document and a warning per line it skipped.
fn server_config_parse(text: &str) -> (ServerConfigDoc, Vec<String>) {
    let mut doc: ServerConfigDoc = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut table = String::new();
    // `[[name]]` array-of-tables entries are flattened into `name#0`, `name#1`,
    // … synthetic tables — the flat `BTreeMap<table, keys>` document shape
    // stays, and consumers iterate the numbered range in order.
    let mut array_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut pending: Option<(usize, String)> = None; // multi-line array being joined
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = server_config_strip_comment(raw);
        let (line_no, line) = match pending.take() {
            Some((start, mut joined)) => {
                joined.push(' ');
                joined.push_str(line.trim());
                (start, joined)
            }
            None => (line_no, line.trim().to_owned()),
        };
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && !line.contains('=') {
            if let Some(inner) = line
                .strip_prefix("[[")
                .and_then(|l| l.strip_suffix("]]"))
            {
                let base = inner.trim();
                if base.is_empty() || base.starts_with('[') {
                    warnings.push(format!("line {line_no}: cannot read table header `{line}`"));
                } else {
                    let count = array_counts.entry(base.to_owned()).or_insert(0);
                    table = format!("{base}#{count}");
                    *count += 1;
                    doc.entry(table.clone()).or_default();
                }
                continue;
            }
            match line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                Some(name) if !name.trim().is_empty() && !name.starts_with('[') => {
                    table = name.trim().to_owned();
                    doc.entry(table.clone()).or_default();
                }
                _ => warnings.push(format!("line {line_no}: cannot read table header `{line}`")),
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("line {line_no}: expected `key = value`"));
            continue;
        };
        let key = key.trim().trim_matches('"').to_owned();
        let value = value.trim();
        if value.starts_with('[') && server_config_bracket_depth(value) > 0 {
            pending = Some((line_no, line.clone()));
            continue;
        }
        match server_config_parse_value(value) {
            Some(v) if !key.is_empty() => {
                doc.entry(table.clone()).or_default().insert(key, v);
            }
            _ => warnings.push(format!("line {line_no}: cannot read value for `{key}`")),
        }
    }
    if let Some((start, _)) = pending {
        warnings.push(format!("line {start}: array is never closed"));
    }
    (doc, warnings)
}

fn server_config_bracket_depth(s: &str) -> i32 {
    let mut depth = 0;
    let mut in_str: Option<char> = None;
    for c in s.chars() {
        match (in_str, c) {
            (Some(q), c) if c == q => in_str = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => in_str = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn server_config_strip_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match in_str {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None if c == '"' || c == '\'' => in_str = Some(c),
            None if c == '#' => return &line[..i],
            None => {}
        }
    }
    line
}

fn server_config_parse_value(s: &str) -> Option<ServerConfigValue> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                _ => return None,
            }
        }
        return Some(ServerConfigValue::Str(out));
    }
    if let Some(inner) = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Some(ServerConfigValue::Str(inner.to_owned()));
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let mut items = Vec::new();
        let mut current = String::new();
        let mut in_str: Option<char> = None;
        for c in inner.chars() {
            match (in_str, c) {
                (Some(q), c) if c == q => {
                    in_str = None;
                    current.push(c);
                }
                (None, '"' | '\'') => {
                    in_str = Some(c);
                    current.push(c);
                }
                (None, ',') => {
                    if !current.trim().is_empty() {
                        items.push(server_config_parse_value(&current)?);
                    }
                    current.clear();
                }
                _ => current.push(c),
            }
        }
        if !current.trim().is_empty() {
            items.push(server_config_parse_value(&current)?);
        }
        return Some(ServerConfigValue::Array(items));
    }
    match s {
        "true" => return Some(ServerConfigValue::Bool(true)),
        "false" => return Some(ServerConfigValue::Bool(false)),
        _ => {}
    }
    let digits = s.replace('_', "");
    if let Ok(i) = digits.parse::<i64>() {
        return Some(ServerConfigValue::Int(i));
    }
    digits
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .map(ServerConfigValue::Float)
}

/// Vanilla `server.properties` keys and the environment variable each stands for. A
/// dropped-in Vanilla file is the lowest layer: it only fills a variable that neither
/// the environment nor `server.toml` set. Keys with a dot in their name (`rcon.port`)
/// are still single properties keys; Java has no nested sections. `enforce-whitelist`
/// is listed first so it wins over `white-list` for the shared target variable, the
/// same precedence Vanilla gives it.
const SERVER_PROPERTIES_KEYS: &[(&str, &str)] = &[
    ("server-port", "MEGU_PORT"),
    ("server-ip", "MEGU_BIND_ADDR"),
    ("motd", "MEGU_MOTD"),
    ("max-players", "MEGU_MAX_PLAYERS"),
    ("level-seed", "MEGU_SEED"),
    ("network-compression-threshold", "MEGU_COMPRESSION_THRESHOLD"),
    ("enforce-whitelist", "MEGU_WHITELIST_ENABLED"),
    ("white-list", "MEGU_WHITELIST_ENABLED"),
    ("difficulty", "MEGU_DIFFICULTY"),
    ("gamemode", "MEGU_DEFAULT_GAMEMODE"),
    ("view-distance", "MEGU_VIEW_DISTANCE"),
    ("simulation-distance", "MEGU_SIMULATION_DISTANCE"),
    ("rcon.port", "MEGU_RCON_PORT"),
    ("rcon.password", "MEGU_RCON_PASSWORD"),
    // The listener actually reads `MEGU_RCON_PASS`; the properties layer feeds
    // both so a Vanilla file's `rcon.password` takes effect, not just parses.
    ("rcon.password", "MEGU_RCON_PASS"),
    ("player-idle-timeout", "MEGU_IDLE_TIMEOUT_SECS"),
    ("op-permission-level", "MEGU_OP_PERMISSION_LEVEL"),
    ("pvp", "MEGU_PVP"),
    ("allow-flight", "MEGU_ALLOW_FLIGHT"),
    ("online-mode", "MEGU_ONLINE_MODE"),
    ("enable-command-block", "MEGU_ENABLE_COMMAND_BLOCK"),
    ("resource-pack", "MEGU_RESOURCE_PACK_URL"),
    ("require-resource-pack", "MEGU_RESOURCE_PACK_REQUIRED"),
    ("enforce-secure-profile", "MEGU_ENFORCE_SECURE_PROFILE"),
];

/// Properties keys handled without an env export. `enable-rcon` is Vanilla's
/// switch and defaults off: it gates the `rcon.*` fills inside
/// `server_properties_env`, so a properties file carrying a bare `rcon.port`
/// cannot open the listener (which only reads `MEGU_RCON_PORT`) — and the
/// switch itself never becomes a `MEGU_*` variable nothing would read.
const SERVER_PROPERTIES_HANDLED_KEYS: &[&str] = &["enable-rcon"];

/// Java `.properties` escape sequences. `\n`, `\t`, `\r`, `\f` and `\uXXXX` produce
/// the named character; any other escaped character (including `\=`, `\:` and `\\`)
/// stands for itself.
fn server_properties_unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{0C}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => {
                        out.push('\\');
                        out.push('u');
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => out.push(other),
        }
    }
    out
}

/// Parse a Vanilla `server.properties` body: `#`/`!` comment lines, `key=value` and
/// `key:value` pairs, Java escapes. Later duplicate keys win, as they do in Vanilla.
fn server_properties_parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        // The first unescaped `=` or `:` separates key from value.
        let mut split = None;
        let mut escaped = false;
        for (i, c) in line.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '=' || c == ':' {
                split = Some(i);
                break;
            }
        }
        let (key, value) = match split {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, ""),
        };
        let key = server_properties_unescape(key.trim());
        if key.is_empty() {
            continue;
        }
        out.insert(key, server_properties_unescape(value.trim()));
    }
    out
}

/// `server.properties` lives next to the `server.toml` in use (the working directory
/// for the default `server.toml`, the custom file's directory under `MEGU_CONFIG`).
fn server_properties_path(config_path: &std::path::Path) -> std::path::PathBuf {
    config_path
        .parent()
        .map(|dir| dir.join("server.properties"))
        .unwrap_or_else(|| std::path::PathBuf::from("server.properties"))
}

/// Read the Vanilla file when one exists. Unlike `server.toml` it is never created:
/// the commented `server.toml` stays the clean default, an operator's dropped-in
/// `server.properties` is simply honored.
fn server_properties_read(path: &std::path::Path) -> Option<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(server_properties_parse(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("[config] could not read {}: {e}; skipping", path.display());
            None
        }
    }
}

/// Vanilla keys as `(env var, value)` for every variable not already set. Values
/// needing a unit change are converted here (`player-idle-timeout` is minutes,
/// `MEGU_IDLE_TIMEOUT_SECS` is seconds); `op-permission-level` is clamped to
/// Vanilla's `1..=4` range. Empty values are skipped: Vanilla writes e.g.
/// `server-ip=` to mean "unset".
fn server_properties_env(
    props: &BTreeMap<String, String>,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut emitted = HashSet::new();
    // `enable-rcon=false` (or absent — the Vanilla default) suppresses the
    // `rcon.*` exports entirely: the port alone must not open the socket.
    let rcon_enabled = props
        .get("enable-rcon")
        .map(|value| ops_bool_str(value).unwrap_or(false))
        .unwrap_or(false);
    for &(key, env) in SERVER_PROPERTIES_KEYS {
        if !rcon_enabled
            && matches!(env, "MEGU_RCON_PORT" | "MEGU_RCON_PASS" | "MEGU_RCON_PASSWORD")
        {
            continue;
        }
        if emitted.contains(env) || is_set(env) {
            continue;
        }
        let Some(raw) = props.get(key) else { continue };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let value = match key {
            "player-idle-timeout" => raw
                .parse::<u64>()
                .ok()
                .map(|minutes| minutes.saturating_mul(60).to_string()),
            "op-permission-level" => Some(
                raw.parse::<i64>()
                    .map(|level| level.clamp(1, 4))
                    .unwrap_or(4)
                    .to_string(),
            ),
            _ => Some(raw.to_owned()),
        };
        if let Some(value) = value {
            out.push((env, value));
            emitted.insert(env);
        }
    }
    out
}

/// Properties keys `SERVER_PROPERTIES_KEYS` does not map, one warning each —
/// the same `(ignored)` treatment `server.toml` unknown keys get, so a typo in
/// a dropped-in Vanilla file is reported instead of vanishing silently.
fn server_properties_unknown_keys(props: &BTreeMap<String, String>) -> Vec<String> {
    props
        .keys()
        .filter(|key| {
            !SERVER_PROPERTIES_KEYS.iter().any(|(known, _)| known == key)
                && !SERVER_PROPERTIES_HANDLED_KEYS.contains(&key.as_str())
        })
        .map(|key| format!("unknown key `{key}`"))
        .collect()
}

/// `[server]` keys and the environment variable each one stands for. The file only
/// fills a variable that is not already set, so the environment keeps priority.
const SERVER_CONFIG_KEYS: &[(&str, &str)] = &[
    ("port", "MEGU_PORT"),
    ("bind_addr", "MEGU_BIND_ADDR"),
    ("motd", "MEGU_MOTD"),
    ("max_players", "MEGU_MAX_PLAYERS"),
    ("seed", "MEGU_SEED"),
    ("compression_threshold", "MEGU_COMPRESSION_THRESHOLD"),
    ("allowlist", "MEGU_ALLOWLIST"),
    ("idle_timeout_secs", "MEGU_IDLE_TIMEOUT_SECS"),
    ("login_timeout_secs", "MEGU_LOGIN_TIMEOUT_SECS"),
    ("backup_interval_secs", "MEGU_BACKUP_INTERVAL_SECS"),
    ("max_conn_per_ip", "MEGU_MAX_CONN_PER_IP"),
    ("resource_pack_url", "MEGU_RESOURCE_PACK_URL"),
    ("resource_pack_required", "MEGU_RESOURCE_PACK_REQUIRED"),
    ("rcon_port", "MEGU_RCON_PORT"),
    // Reliability / ops / multi-version keys read by `RuntimeOpsConfig`. Keeping
    // them in the shared table means the boot export reaches the consumers that
    // already read the variable directly (`MEGU_VERSION_DIR` in
    // block_registry, `MEGU_TARGET_VERSION`/`MEGU_MC_VERSION` in
    // save_v2). `target_version` is the canonical Two-Target knob;
    // `target_mc_version` is the pre-Two-Target spelling kept so existing files
    // keep working.
    ("online_mode", "MEGU_ONLINE_MODE"),
    ("velocity_forwarding", "MEGU_VELOCITY_FORWARDING"),
    ("velocity_forwarding_secret", "MEGU_VELOCITY_SECRET"),
    ("autosave_interval_secs", "MEGU_AUTOSAVE_SECS"),
    ("backup_max_keep", "MEGU_BACKUP_MAX_KEEP"),
    ("lan_broadcast", "MEGU_LAN_BROADCAST"),
    ("auto_load_balance", "MEGU_AUTO_LOAD_BALANCE"),
    ("target_version", "MEGU_TARGET_VERSION"),
    ("target_mc_version", "MEGU_MC_VERSION"),
    ("version_pack_dir", "MEGU_VERSION_DIR"),
    // Vanilla `op-permission-level` — the level a fresh `/op` grants. The
    // properties layer exports the same variable for `op-permission-level`.
    ("op_permission_level", "MEGU_OP_PERMISSION_LEVEL"),
    // Opt-in directional shadow gameplay (SERVER-DIRECTIONAL-SHADOW-GAMEPLAY):
    // not a vanilla gamerule, so it fills an env var like every other server
    // key instead of touching the gamerule table.
    ("directional_shadow_light", "MEGU_DIRECTIONAL_SHADOW_LIGHT"),
    // Opt-in unloaded-chunk catch-up: not a vanilla gamerule either, so it
    // fills an env var like every other server key.
    ("lazy_catchup", "MEGU_LAZY_CATCHUP"),
];

/// `[anticheat]` keys: the ones in the default file, then old ones that still work
/// (with a deprecation note from `anticheat::AcConfig::from_doc`).
const SERVER_CONFIG_ANTICHEAT_KEYS: &[&str] = &[
    "antixray",
    "antifullbright",
    "hide_underground",
    "check_ops",
    "enabled",
    "preset",
    "alerts",
    "exempt",
    "exempt_ops",
    "ops_bypass",
    "anti_xray",
];

/// The file in use: `MEGU_CONFIG`, else `server.toml` in the working directory
/// (where the world files live).
fn server_config_path() -> std::path::PathBuf {
    std::env::var_os("MEGU_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("server.toml"))
}

/// Read the file (writing the default one when it is missing) and return the document.
/// Never fails: problems are printed and the defaults are used.
fn server_config_read_or_create(path: &std::path::Path) -> ServerConfigDoc {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::write(path, SERVER_CONFIG_DEFAULT_FILE) {
                Ok(()) => eprintln!("[config] wrote default {}", path.display()),
                Err(e) => eprintln!(
                    "[config] could not write {}: {e}; using defaults",
                    path.display()
                ),
            }
            SERVER_CONFIG_DEFAULT_FILE.to_owned()
        }
        Err(e) => {
            eprintln!(
                "[config] could not read {}: {e}; using defaults",
                path.display()
            );
            SERVER_CONFIG_DEFAULT_FILE.to_owned()
        }
    };
    let (doc, warnings) = server_config_parse(&text);
    for w in warnings {
        eprintln!("[config] {}: {w} (skipped)", path.display());
    }
    for w in server_config_unknown_keys(&doc) {
        eprintln!("[config] {}: {w} (ignored)", path.display());
    }
    doc
}

fn server_config_unknown_keys(doc: &ServerConfigDoc) -> Vec<String> {
    let mut out = Vec::new();
    for (table, keys) in doc {
        let known: Vec<&str> = match table.as_str() {
            "server" => SERVER_CONFIG_KEYS.iter().map(|(k, _)| *k).collect(),
            "anticheat" => SERVER_CONFIG_ANTICHEAT_KEYS.to_vec(),
            "lifecycle" => vec!["idle_sleep_secs", "idle_shutdown_secs"],
            "remote_control" => vec!["enabled", "bind_addr", "admin_token", "start_only_token"],
            "servers" => vec!["fallback"],
            _ if table.starts_with("servers.") => vec!["host", "port"],
            _ if table.starts_with("worlds#") => {
                vec!["id", "generator", "seed", "pvp", "gamemode", "spawn"]
            }
            _ => {
                out.push(format!("unknown table [{table}]"));
                continue;
            }
        };
        for key in keys.keys() {
            if !known.contains(&key.as_str()) {
                out.push(format!("unknown key `{key}` in [{table}]"));
            }
        }
    }
    out
}

/// `[server]` values as `(env var, value)` for every variable not already set.
fn server_config_server_env(doc: &ServerConfigDoc, is_set: impl Fn(&str) -> bool) -> Vec<(&'static str, String)> {
    let Some(server) = doc.get("server") else {
        return Vec::new();
    };
    SERVER_CONFIG_KEYS
        .iter()
        .filter(|(_, env)| !is_set(env))
        .filter_map(|(key, env)| server.get(*key).map(|v| (*env, v.as_env())))
        .collect()
}

/// Boot step, first thing in `main` while the process is still single-threaded: read the
/// file and export `[server]` keys as the variables the rest of the server already reads.
/// Returns the document so the caller can install the `[anticheat]` table.
fn server_config_boot() -> ServerConfigDoc {
    let path = server_config_path();
    let doc = server_config_read_or_create(&path);
    for (env, value) in server_config_server_env(&doc, |name| std::env::var_os(name).is_some()) {
        // Single-threaded here (called before any thread is spawned).
        std::env::set_var(env, value);
    }
    // A Vanilla `server.properties` next to the config sits under both layers above:
    // it only fills variables still unset after the environment and `server.toml`.
    if let Some(props) = server_properties_read(&server_properties_path(&path)) {
        for (env, value) in server_properties_env(&props, |name| std::env::var_os(name).is_some()) {
            std::env::set_var(env, value);
        }
        for w in server_properties_unknown_keys(&props) {
            eprintln!("[config] {}: {w} (ignored)", server_properties_path(&path).display());
        }
    }
    // `MEGU_WHITELIST` is the historical variable; `MEGU_WHITELIST_ENABLED` is
    // the canonical name `white-list`/`enforce-whitelist` export into. Both
    // stay supported, the explicit legacy variable winning when both are set.
    let whitelist_enforced = std::env::var("MEGU_WHITELIST")
        .ok()
        .and_then(|value| ops_bool_str(&value))
        .or_else(|| {
            std::env::var("MEGU_WHITELIST_ENABLED")
                .ok()
                .and_then(|value| ops_bool_str(&value))
        });
    apply_runtime_server_config(
        std::env::var("MEGU_VIEW_DISTANCE")
            .ok()
            .and_then(|value| value.parse().ok()),
        std::env::var("MEGU_SIMULATION_DISTANCE")
            .ok()
            .and_then(|value| value.parse().ok()),
        std::env::var("MEGU_MAX_PLAYERS")
            .ok()
            .and_then(|value| value.parse().ok()),
        std::env::var("MEGU_PVP")
            .ok()
            .and_then(|value| ops_bool_str(&value)),
        std::env::var("MEGU_ALLOW_FLIGHT")
            .ok()
            .and_then(|value| ops_bool_str(&value)),
        whitelist_enforced,
    );
    // The environment now carries every file layer, so resolving the ops knobs
    // sees the same values the env-reading consumers do. Reported once, like
    // the unknown-key warnings above.
    let ops = RuntimeOpsConfig::from_doc(&doc, |name| std::env::var(name).ok());
    for note in &ops.notes {
        eprintln!("[config] {note}");
    }
    eprintln!("[config] {}", ops.summary());
    let _ = RUNTIME_OPS.set(ops);
    for w in apply_operator_tables(&doc) {
        eprintln!("[config] {}: {w} (skipped)", path.display());
    }
    // The opt-in directional-shadow lane resolves from the environment the
    // file layers just populated; single-threaded still, like every other
    // boot switch. Anything the strict reader cannot parse stays off.
    directional_shadow_light_set(
        std::env::var("MEGU_DIRECTIONAL_SHADOW_LIGHT")
            .ok()
            .as_deref()
            .and_then(ops_bool_str)
            .unwrap_or(false),
    );
    // Same single-threaded resolution for the opt-in unloaded-chunk
    // catch-up lane; anything the strict reader cannot parse stays off.
    lazy_catchup_set(
        std::env::var("MEGU_LAZY_CATCHUP")
            .ok()
            .as_deref()
            .and_then(ops_bool_str)
            .unwrap_or(false),
    );
    doc
}

/// Re-read the file for a live reload (only the tables that can change at run time are
/// used by the caller). Returns the document and every warning about it.
fn server_config_reread() -> Result<(ServerConfigDoc, Vec<String>), String> {
    let path = server_config_path();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let (doc, mut warnings) = server_config_parse(&text);
    warnings.extend(server_config_unknown_keys(&doc));
    Ok((doc, warnings))
}

// ----------------------------------------------------- operator tables ----

/// `[lifecycle]` — the sleep/shutdown policy for an empty server. `None` when
/// the table is absent; the loop that owns `ServerLifecycleController` reads
/// this once at startup.
#[allow(dead_code)] // consumed by the supervisor loop owned by the main.rs lane
pub(crate) struct LifecycleBootConfig {
    /// Seconds with zero players before the world saves and the server
    /// hibernates (`sleeping_motd` + `RejectLogin` wake).
    pub idle_sleep_secs: u64,
    /// Seconds with zero players before the server exits instead — when set,
    /// it wins over hibernation.
    pub idle_shutdown_secs: Option<u64>,
}

/// `[remote_control]` — the loopback admin page's bind and tokens. `None`
/// when `enabled` is absent or not true.
#[allow(dead_code)] // consumed by the remote-control listener owned by the main.rs lane
pub(crate) struct RemoteControlBootConfig {
    pub bind_addr: String,
    pub admin_token: String,
    pub start_only_token: Option<String>,
}

/// What the operator tables resolved to at boot.
#[allow(dead_code)] // fields are read by the lanes that own the supervisor and the admin listener
pub(crate) struct OperatorTablesConfig {
    pub lifecycle: Option<LifecycleBootConfig>,
    pub remote_control: Option<RemoteControlBootConfig>,
}

static OPERATOR_TABLES: OnceLock<OperatorTablesConfig> = OnceLock::new();

/// The boot-resolved `[lifecycle]`/`[remote_control]` pair — empty before
/// `server_config_boot` runs or when neither table is present.
#[allow(dead_code)]
pub(crate) fn operator_tables() -> &'static OperatorTablesConfig {
    OPERATOR_TABLES.get_or_init(|| OperatorTablesConfig {
        lifecycle: None,
        remote_control: None,
    })
}

fn cfg_table_str<'a>(table: &'a BTreeMap<String, ServerConfigValue>, key: &str) -> Option<&'a str> {
    match table.get(key) {
        Some(ServerConfigValue::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn cfg_table_i64(table: &BTreeMap<String, ServerConfigValue>, key: &str) -> Option<i64> {
    match table.get(key) {
        Some(ServerConfigValue::Int(i)) => Some(*i),
        _ => None,
    }
}

fn cfg_table_bool(table: &BTreeMap<String, ServerConfigValue>, key: &str) -> Option<bool> {
    match table.get(key) {
        Some(ServerConfigValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Wire the `[servers]`, `[[worlds]]`, `[lifecycle]` and `[remote_control]`
/// tables into the live registries the commands read. Problems are reported
/// like the other config warnings — a bad entry is skipped, never fatal.
fn apply_operator_tables(doc: &ServerConfigDoc) -> Vec<String> {
    let mut warnings = Vec::new();
    // [servers.<name>] hub directory + [servers] fallback.
    {
        let mut directory = hub_directory().lock().unwrap_or_else(|e| e.into_inner());
        for (table, keys) in doc {
            let Some(name) = table.strip_prefix("servers.") else {
                continue;
            };
            match (cfg_table_str(keys, "host"), cfg_table_i64(keys, "port")) {
                (Some(host), Some(port)) => {
                    if let Err(e) = directory.add_server(name, host, port.clamp(0, u16::MAX as i64) as u16)
                    {
                        warnings.push(format!("[{table}] {e}"));
                    }
                }
                _ => warnings.push(format!("[{table}] needs a `host` string and a `port` integer")),
            }
        }
        if let Some(servers) = doc.get("servers") {
            if let Some(fallback) = cfg_table_str(servers, "fallback") {
                if let Err(e) = directory.set_fallback(fallback) {
                    warnings.push(format!("[servers] {e}"));
                }
            }
        }
    }
    // [[worlds]] — each entry becomes a managed world plus its live
    // `megu:<id>` dimension registration.
    for (table, keys) in doc {
        if !table.starts_with("worlds#") {
            continue;
        }
        let Some(id) = cfg_table_str(keys, "id").map(str::to_owned) else {
            warnings.push(format!("[{table}] needs an `id` string"));
            continue;
        };
        let generator = match cfg_table_str(keys, "generator").unwrap_or("overworld") {
            "overworld" => WorldGenerator::Overworld,
            "flat" => WorldGenerator::Flat,
            "void" => WorldGenerator::Void,
            other => {
                warnings.push(format!(
                    "[{table}] unknown generator {other:?}; expected overworld|flat|void"
                ));
                continue;
            }
        };
        let spawn: [f64; 3] = match keys.get("spawn") {
            Some(ServerConfigValue::Array(items)) if items.len() == 3 => {
                let mut out = [0.0; 3];
                let mut ok = true;
                for (i, item) in items.iter().enumerate() {
                    match item {
                        ServerConfigValue::Int(n) => out[i] = *n as f64,
                        ServerConfigValue::Float(f) => out[i] = *f,
                        _ => ok = false,
                    }
                }
                if !ok {
                    warnings.push(format!("[{table}] `spawn` entries must be numbers"));
                    continue;
                }
                out
            }
            Some(_) => {
                warnings.push(format!("[{table}] `spawn` must be `[x, y, z]`"));
                continue;
            }
            None => [0.5, 64.0, 0.5],
        };
        if let Err(e) = register_managed_world_dimension(&id, generator) {
            warnings.push(format!("[{table}] {e}"));
            continue;
        }
        let world = ManagedWorld {
            id: id.clone(),
            generator,
            seed: cfg_table_i64(keys, "seed")
                .map(|v| v as u64)
                .unwrap_or_else(|| WORLD_SEED.load(Ordering::Relaxed)),
            pvp: cfg_table_bool(keys, "pvp").unwrap_or_else(live_pvp_enabled),
            default_gamemode: match cfg_table_str(keys, "gamemode").unwrap_or("survival") {
                "creative" => WorldGameMode::Creative,
                "adventure" => WorldGameMode::Adventure,
                "spectator" => WorldGameMode::Spectator,
                _ => WorldGameMode::Survival,
            },
            spawn,
        };
        if let Err(e) = managed_worlds()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(world)
        {
            warnings.push(format!("[{table}] {e}"));
        }
    }
    // [lifecycle] / [remote_control] — parked for the lanes that own the
    // supervisor loop and the admin listener.
    let lifecycle = doc.get("lifecycle").map(|keys| LifecycleBootConfig {
        idle_sleep_secs: cfg_table_i64(keys, "idle_sleep_secs")
            .map(|v| v.max(0) as u64)
            .unwrap_or(300),
        idle_shutdown_secs: cfg_table_i64(keys, "idle_shutdown_secs").map(|v| v.max(0) as u64),
    });
    let remote_control = doc
        .get("remote_control")
        .filter(|keys| cfg_table_bool(keys, "enabled").unwrap_or(false))
        .and_then(|keys| {
            let admin_token = cfg_table_str(keys, "admin_token")?.to_owned();
            Some(RemoteControlBootConfig {
                bind_addr: cfg_table_str(keys, "bind_addr")
                    .unwrap_or("127.0.0.1:25575")
                    .to_owned(),
                admin_token,
                start_only_token: cfg_table_str(keys, "start_only_token").map(str::to_owned),
            })
        });
    let _ = OPERATOR_TABLES.set(OperatorTablesConfig {
        lifecycle,
        remote_control,
    });
    warnings
}

// ---------- production ops knobs ----------
//
// The typed view of the `[server]` keys the reliability, operator-UX and
// multi-version lanes read. Resolution is deliberately small: the file fills
// what the environment leaves unset and `MEGU_*` always wins, exactly like
// `AcConfig::from_doc`. A dropped-in vanilla `server.properties` reaches the
// same variables through `server_config_boot` (`online-mode` is the only one
// of these that has a vanilla key).

/// The MC release the bundled data pack targets; `MEGU_TARGET_VERSION` and
/// the `[server] target_version` key move it together (the legacy
/// `MEGU_MC_VERSION` / `target_mc_version` pair still fills it).
pub(crate) const RUNTIME_OPS_DEFAULT_MC_VERSION: &str = "1.21.1";
/// The two first-class MC releases this server can run as
/// (`specs/multi-version-architecture.md`): the native `1.21.1` line and the
/// latest vanilla `26.3` line. `target_version` must name one of these; any
/// other value fails closed to the default.
pub(crate) const RUNTIME_OPS_TARGET_VERSIONS: &[&str] = &["1.21.1", "26.3"];
// Autosave cadence bounds: tight enough that a typo cannot stall saving for a
// day or spin the writer every tick.
const AUTOSAVE_SECS_DEFAULT: u64 = 300;
const AUTOSAVE_SECS_MIN: u64 = 10;
const AUTOSAVE_SECS_MAX: u64 = 86_400;
const BACKUP_MAX_KEEP_DEFAULT: usize = 5;

/// The ops-facing switches for authentication, persistence, LAN/UX smoothing
/// and version-pack selection. Every field is optional; the defaults keep a
/// loopback development server behaving exactly as it did before the lanes
/// landed (offline mode, no forwarding, five-minute autosave, five backups,
/// no LAN broadcast, load smoothing on, bundled 1.21.1 data).
#[derive(Clone, PartialEq)]
pub(crate) struct RuntimeOpsConfig {
    /// Mojang session authentication (vanilla `online-mode`,
    /// `MEGU_ONLINE_MODE`). Off keeps the loopback test behaviour: no
    /// encryption request, offline UUIDs.
    pub(crate) online_mode: bool,
    /// Velocity modern forwarding (`MEGU_VELOCITY_FORWARDING`): the login
    /// handshake trusts the signed `velocity:player_info` payload instead of a
    /// Mojang session lookup. Meaningful only with a secret.
    pub(crate) velocity_forwarding: bool,
    /// Shared secret Velocity signs forwarding data with
    /// (`MEGU_VELOCITY_SECRET`). Never printed: `Debug` and `summary()`
    /// show `[REDACTED]`; the raw value leaves the struct only through
    /// `velocity_forwarding_secret()`.
    velocity_forwarding_secret: Option<String>,
    /// Autosave cadence for the background save worker
    /// (`MEGU_AUTOSAVE_SECS`), clamped to
    /// `AUTOSAVE_SECS_MIN..=AUTOSAVE_SECS_MAX`.
    pub(crate) autosave_interval_secs: u64,
    /// Generations `rotate_world_backups` keeps (`MEGU_BACKUP_MAX_KEEP`).
    pub(crate) backup_max_keep: usize,
    /// Vanilla-style LAN broadcast announcements (`MEGU_LAN_BROADCAST`) so
    /// the server appears in the client multiplayer list.
    pub(crate) lan_broadcast: bool,
    /// `AutoLoadBalancer` tick smoothing (`MEGU_AUTO_LOAD_BALANCE`). On
    /// unless an operator explicitly turns it off.
    pub(crate) auto_load_balance: bool,
    /// The MC release the active version pack describes
    /// (`MEGU_TARGET_VERSION` / `[server] target_version`, validated
    /// against `RUNTIME_OPS_TARGET_VERSIONS`).
    pub(crate) target_version: String,
    /// Explicit version-pack directory (`MEGU_VERSION_DIR`); unset means
    /// the bundled tables.
    pub(crate) version_pack_dir: Option<String>,
    /// One line per value that had to be repaired, ignored or is incomplete,
    /// like `AcConfig::notes`.
    pub(crate) notes: Vec<String>,
}

impl Default for RuntimeOpsConfig {
    fn default() -> Self {
        RuntimeOpsConfig {
            online_mode: false,
            velocity_forwarding: false,
            velocity_forwarding_secret: None,
            autosave_interval_secs: AUTOSAVE_SECS_DEFAULT,
            backup_max_keep: BACKUP_MAX_KEEP_DEFAULT,
            lan_broadcast: false,
            auto_load_balance: true,
            target_version: RUNTIME_OPS_DEFAULT_MC_VERSION.to_owned(),
            version_pack_dir: None,
            notes: Vec::new(),
        }
    }
}

impl std::fmt::Debug for RuntimeOpsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOpsConfig")
            .field("online_mode", &self.online_mode)
            .field("velocity_forwarding", &self.velocity_forwarding)
            .field(
                "velocity_forwarding_secret",
                &self.redacted_secret(),
            )
            .field("autosave_interval_secs", &self.autosave_interval_secs)
            .field("backup_max_keep", &self.backup_max_keep)
            .field("lan_broadcast", &self.lan_broadcast)
            .field("auto_load_balance", &self.auto_load_balance)
            .field("target_version", &self.target_version)
            .field("version_pack_dir", &self.version_pack_dir)
            .field("notes", &self.notes)
            .finish()
    }
}

/// Strict boolean reading of an env-var string: the same spellings `truthy`
/// accepts, plus the explicit-off set, so a mistyped value is a note rather
/// than a silent `false`.
fn ops_bool_str(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn ops_bool(
    notes: &mut Vec<String>,
    label: &str,
    file: Option<&ServerConfigValue>,
    env: Option<String>,
    default: bool,
) -> bool {
    let file = match file {
        Some(ServerConfigValue::Bool(b)) => Some(*b),
        Some(v) => {
            notes.push(format!("[server] {label} must be true or false, got {v:?}"));
            None
        }
        None => None,
    };
    match env.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => match ops_bool_str(v) {
            Some(b) => b,
            None => {
                notes.push(format!("{label}: unrecognized value `{v}`; using {}", file.unwrap_or(default)));
                file.unwrap_or(default)
            }
        },
        None => file.unwrap_or(default),
    }
}

fn ops_u64(
    notes: &mut Vec<String>,
    label: &str,
    file: Option<&ServerConfigValue>,
    env: Option<String>,
    default: u64,
) -> u64 {
    let file = match file {
        Some(ServerConfigValue::Int(i)) if *i >= 0 => Some(*i as u64),
        Some(v) => {
            notes.push(format!("[server] {label} must be a non-negative integer, got {v:?}"));
            None
        }
        None => None,
    };
    match env.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                notes.push(format!("{label}: unrecognized value `{v}`; using {}", file.unwrap_or(default)));
                file.unwrap_or(default)
            }
        },
        None => file.unwrap_or(default),
    }
}

fn ops_str(
    notes: &mut Vec<String>,
    label: &str,
    file: Option<&ServerConfigValue>,
    env: Option<String>,
) -> Option<String> {
    let file = match file {
        Some(ServerConfigValue::Str(s)) => Some(s.clone()),
        Some(v) => {
            notes.push(format!("[server] {label} must be a string, got {v:?}"));
            None
        }
        None => None,
    };
    env.map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or(file)
        .filter(|s| !s.is_empty())
}

/// The MC release the server runs as. Layers, highest first:
/// `MEGU_TARGET_VERSION`, `[server] target_version`, then the legacy pair
/// `MEGU_MC_VERSION` / `[server] target_mc_version`. A layer must name one
/// of `RUNTIME_OPS_TARGET_VERSIONS`; an unrecognized value is noted and the
/// next layer is tried, so a typo can never make the server claim a release it
/// does not implement.
fn ops_target_version(
    notes: &mut Vec<String>,
    file: impl Fn(&str) -> Option<ServerConfigValue>,
    env: impl Fn(&str) -> Option<String>,
) -> String {
    let mut file_str = |key: &str| match file(key) {
        Some(ServerConfigValue::Str(s)) => Some(s),
        Some(v) => {
            notes.push(format!("[server] {key} must be a string, got {v:?}"));
            None
        }
        None => None,
    };
    let layers = [
        env("MEGU_TARGET_VERSION"),
        file_str("target_version"),
        env("MEGU_MC_VERSION"),
        file_str("target_mc_version"),
    ];
    for raw in layers.into_iter().flatten() {
        let value = raw.trim();
        if value.is_empty() {
            continue;
        }
        if RUNTIME_OPS_TARGET_VERSIONS.contains(&value) {
            return value.to_owned();
        }
        notes.push(format!(
            "target_version `{value}` is not a supported release (expected \"1.21.1\" or \"26.3\"); ignoring"
        ));
    }
    RUNTIME_OPS_DEFAULT_MC_VERSION.to_owned()
}

impl RuntimeOpsConfig {
    /// `[server]` from the parsed file, then the environment on top:
    /// `MEGU_ONLINE_MODE`, `MEGU_VELOCITY_FORWARDING`,
    /// `MEGU_VELOCITY_SECRET`, `MEGU_AUTOSAVE_SECS`,
    /// `MEGU_BACKUP_MAX_KEEP`, `MEGU_LAN_BROADCAST`,
    /// `MEGU_AUTO_LOAD_BALANCE`, `MEGU_TARGET_VERSION` (and the legacy
    /// `MEGU_MC_VERSION`), `MEGU_VERSION_DIR`.
    pub(crate) fn from_doc(
        doc: &ServerConfigDoc,
        env: impl Fn(&str) -> Option<String>,
    ) -> RuntimeOpsConfig {
        let table = doc.get("server");
        let get = |k: &str| table.and_then(|t| t.get(k));
        let mut notes = Vec::new();
        let mut c = RuntimeOpsConfig {
            online_mode: ops_bool(&mut notes, "online_mode", get("online_mode"), env("MEGU_ONLINE_MODE"), false),
            velocity_forwarding: ops_bool(&mut notes, "velocity_forwarding", get("velocity_forwarding"), env("MEGU_VELOCITY_FORWARDING"), false),
            velocity_forwarding_secret: ops_str(&mut notes, "velocity_forwarding_secret", get("velocity_forwarding_secret"), env("MEGU_VELOCITY_SECRET")),
            autosave_interval_secs: {
                let raw = ops_u64(&mut notes, "autosave_interval_secs", get("autosave_interval_secs"), env("MEGU_AUTOSAVE_SECS"), AUTOSAVE_SECS_DEFAULT);
                let clamped = raw.clamp(AUTOSAVE_SECS_MIN, AUTOSAVE_SECS_MAX);
                if raw != clamped {
                    notes.push(format!(
                        "autosave_interval_secs {raw} outside {AUTOSAVE_SECS_MIN}..={AUTOSAVE_SECS_MAX}; clamped to {clamped}"
                    ));
                }
                clamped
            },
            backup_max_keep: {
                let raw = ops_u64(&mut notes, "backup_max_keep", get("backup_max_keep"), env("MEGU_BACKUP_MAX_KEEP"), BACKUP_MAX_KEEP_DEFAULT as u64);
                usize::try_from(raw).unwrap_or_else(|_| {
                    notes.push(format!("backup_max_keep {raw} does not fit; using {BACKUP_MAX_KEEP_DEFAULT}"));
                    BACKUP_MAX_KEEP_DEFAULT
                })
            },
            lan_broadcast: ops_bool(&mut notes, "lan_broadcast", get("lan_broadcast"), env("MEGU_LAN_BROADCAST"), false),
            auto_load_balance: ops_bool(&mut notes, "auto_load_balance", get("auto_load_balance"), env("MEGU_AUTO_LOAD_BALANCE"), true),
            target_version: ops_target_version(&mut notes, |k| get(k).cloned(), |n| env(n)),
            version_pack_dir: ops_str(&mut notes, "version_pack_dir", get("version_pack_dir"), env("MEGU_VERSION_DIR")),
            notes: Vec::new(),
        };
        if c.velocity_forwarding && c.velocity_forwarding_secret.is_none() {
            notes.push(
                "velocity_forwarding is on but no secret is set (velocity_forwarding_secret / MEGU_VELOCITY_SECRET); forwarding data cannot be verified"
                    .to_owned(),
            );
        }
        if !c.velocity_forwarding && c.velocity_forwarding_secret.is_some() {
            notes.push(
                "velocity_forwarding_secret is set but velocity_forwarding is off; the secret has no effect"
                    .to_owned(),
            );
        }
        c.notes = notes;
        c
    }

    /// The forwarding secret for the handshake code — the only place the raw
    /// value leaves this struct. `None` means forwarding cannot be verified.
    #[allow(dead_code)]
    pub(crate) fn velocity_forwarding_secret(&self) -> Option<&str> {
        self.velocity_forwarding_secret.as_deref()
    }

    fn redacted_secret(&self) -> Option<&'static str> {
        self.velocity_forwarding_secret.as_ref().map(|_| "[REDACTED]")
    }

    /// One operator-log line naming every effective knob; the forwarding
    /// secret stays `[REDACTED]` so a pasted startup log cannot leak it.
    pub(crate) fn summary(&self) -> String {
        format!(
            "ops: online_mode={} velocity_forwarding={} (secret {}) autosave={}s backups={} lan={} load_balance={} target_version={} pack_dir={}",
            self.online_mode,
            self.velocity_forwarding,
            if self.velocity_forwarding_secret.is_some() { "[REDACTED]" } else { "unset" },
            self.autosave_interval_secs,
            self.backup_max_keep,
            self.lan_broadcast,
            self.auto_load_balance,
            self.target_version,
            self.version_pack_dir.as_deref().unwrap_or("<bundled>"),
        )
    }
}

// ---------- opt-in directional shadow gameplay ----------
//
// SERVER-DIRECTIONAL-SHADOW-GAMEPLAY: real 3D sun shadows — the same vector
// `client-port`'s GPU shadow map renders — feed spawn light, undead burning
// and crop growth. Strictly opt-in: `false` keeps every light, burn and grow
// path bit-exact Vanilla 1.21.1, and `sample_directional_shadow` answers
// without casting a single ray. Not a gamerule, so the vanilla gamerule
// table and its persistence stay untouched. Layers: `[server]
// directional_shadow_light` fills `MEGU_DIRECTIONAL_SHADOW_LIGHT` like every
// other server key, the environment wins, `server_config_boot` resolves the
// switch below while still single-threaded.

/// The runtime switch. An atomic, not a resolved `RuntimeOpsConfig` field, so
/// a live admin toggle can flip it without a reboot.
static DIRECTIONAL_SHADOW_LIGHT: AtomicBool = AtomicBool::new(false);

/// Whether the directional shadow gameplay lane is live right now.
pub(crate) fn directional_shadow_light_enabled() -> bool {
    DIRECTIONAL_SHADOW_LIGHT.load(Ordering::Relaxed)
}

/// The runtime admin toggle: flips the atomic the live probes read, no
/// reboot needed. `server_config_boot` resolves the boot value through it
/// and tests flip it directly.
pub(crate) fn directional_shadow_light_set(on: bool) {
    DIRECTIONAL_SHADOW_LIGHT.store(on, Ordering::Relaxed);
}

/// The feature config a probe builds per query: the live switch plus the
/// tuned defaults (`DirectionalShadowConfig::default` keeps every lane on so
/// the single switch is the whole policy).
pub(crate) fn directional_shadow_cfg() -> DirectionalShadowConfig {
    DirectionalShadowConfig {
        enabled: directional_shadow_light_enabled(),
        ..DirectionalShadowConfig::default()
    }
}

// ---------- opt-in unloaded-chunk catch-up ----------
//
// LAZY-CATCHUP: furnaces, crops and headless factories keep ticking while
// their chunk is unloaded, resolved in O(1) per due machine instead of a
// chunk-loader scan. Strictly opt-in: `false` keeps every unloaded-chunk
// path bit-exact Vanilla 1.21.1. Not a gamerule, so the vanilla gamerule
// table and its persistence stay untouched. Layers: `[server]
// lazy_catchup` fills `MEGU_LAZY_CATCHUP` like every other server key, the
// environment wins, `server_config_boot` resolves the switch below while
// still single-threaded.

/// The runtime switch. An atomic, not a resolved `RuntimeOpsConfig` field, so
/// a live admin toggle can flip it without a reboot.
pub(crate) static LAZY_CATCHUP_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether the unloaded-chunk catch-up lane is live right now.
pub(crate) fn lazy_catchup_enabled() -> bool {
    LAZY_CATCHUP_ENABLED.load(Ordering::Relaxed)
}

/// The runtime admin toggle: flips the atomic the live probes read, no
/// reboot needed. `server_config_boot` resolves the boot value through it
/// and tests flip it directly.
pub(crate) fn lazy_catchup_set(enabled: bool) {
    LAZY_CATCHUP_ENABLED.store(enabled, Ordering::Relaxed);
}

static RUNTIME_OPS: OnceLock<RuntimeOpsConfig> = OnceLock::new();

/// The resolved ops knobs, defaulting until `server_config_boot` installs the
/// values the file and environment produced. U1/U2 wiring reads this instead of
/// re-parsing so every consumer sees the same clamped choices.
#[allow(dead_code)]
pub(crate) fn runtime_ops() -> &'static RuntimeOpsConfig {
    RUNTIME_OPS.get_or_init(RuntimeOpsConfig::default)
}

#[cfg(test)]
mod server_config_tests {
    use super::*;

    #[test]
    fn live_runtime_accessors_apply_overrides_and_clamps() {
        // The `live_*` atomics are process-global; every test mutating them
        // (here and in the ops/protocol suites) holds the world guard.
        let _guard = world_guard();
        assert_eq!(live_view_distance(), LIVE_VIEW_DISTANCE_DEFAULT);
        assert_eq!(live_simulation_distance(), LIVE_SIMULATION_DISTANCE_DEFAULT);
        assert_eq!(live_max_players(), LIVE_MAX_PLAYERS_DEFAULT);

        apply_runtime_server_config(Some(1), Some(40), Some(0), None, None, None);
        assert_eq!(live_view_distance(), 2);
        assert_eq!(live_simulation_distance(), 32);
        assert_eq!(live_max_players(), 1);

        apply_runtime_server_config(Some(12), Some(6), Some(200), None, None, None);
        assert_eq!(live_view_distance(), 12);
        assert_eq!(live_simulation_distance(), 6);
        assert_eq!(live_max_players(), 200);

        apply_runtime_server_config(None, None, None, None, None, None);
        assert_eq!(live_view_distance(), LIVE_VIEW_DISTANCE_DEFAULT);
        assert_eq!(live_simulation_distance(), LIVE_SIMULATION_DISTANCE_DEFAULT);
        assert_eq!(live_max_players(), LIVE_MAX_PLAYERS_DEFAULT);
    }

    #[test]
    fn live_runtime_pvp_flight_and_whitelist_seed_apply() {
        // The gameplay flags ride the same install: `pvp=false` and
        // `allow-flight=true` reach their getters, and the whitelist seed
        // drives the shared enforcement switch `/whitelist on|off` flips.
        let _guard = world_guard();
        apply_runtime_server_config(None, None, None, Some(false), Some(true), Some(true));
        assert!(!live_pvp_enabled());
        assert!(live_allow_flight());
        assert!(live_whitelist_enforced());

        apply_runtime_server_config(None, None, None, Some(true), Some(false), Some(false));
        assert!(live_pvp_enabled());
        assert!(!live_allow_flight());
        assert!(!live_whitelist_enforced());
    }

    #[test]
    fn server_properties_gameplay_and_admin_keys_export() {
        let props = server_properties_parse(
            "view-distance=12\n\
             simulation-distance=6\n\
             max-players=42\n\
             white-list=true\n\
             pvp=false\n\
             allow-flight=true\n\
             enable-command-block=true\n\
             resource-pack=https://example.invalid/pack.zip\n\
             require-resource-pack=true\n\
             enforce-secure-profile=true\n\
             enable-rcon=true\n",
        );
        let env = server_properties_env(&props, |_| false);
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert_eq!(got["MEGU_VIEW_DISTANCE"], "12");
        assert_eq!(got["MEGU_SIMULATION_DISTANCE"], "6");
        assert_eq!(got["MEGU_MAX_PLAYERS"], "42");
        assert_eq!(got["MEGU_WHITELIST_ENABLED"], "true");
        assert_eq!(got["MEGU_PVP"], "false");
        assert_eq!(got["MEGU_ALLOW_FLIGHT"], "true");
        assert_eq!(got["MEGU_ENABLE_COMMAND_BLOCK"], "true");
        assert_eq!(
            got["MEGU_RESOURCE_PACK_URL"],
            "https://example.invalid/pack.zip"
        );
        assert_eq!(got["MEGU_RESOURCE_PACK_REQUIRED"], "true");
        assert_eq!(got["MEGU_ENFORCE_SECURE_PROFILE"], "true");
        // `enable-rcon` is consumed semantically: it exports nothing itself
        // but is still a recognized (not unknown) key.
        assert!(!got.contains_key("MEGU_ENABLE_RCON"));
        // Every one of these keys is mapped or handled, so none is reported
        // unknown.
        assert!(server_properties_unknown_keys(&props).is_empty());
    }

    #[test]
    fn server_properties_enable_rcon_gates_the_rcon_fills() {
        // Vanilla semantics: `enable-rcon` defaults off, so a bare `rcon.port`
        // must not open the listener that reads `MEGU_RCON_PORT`.
        for body in ["rcon.port=25575\n", "rcon.port=25575\nenable-rcon=false\n"] {
            let props = server_properties_parse(body);
            let got: BTreeMap<&str, String> =
                server_properties_env(&props, |_| false).into_iter().collect();
            assert!(
                !got.contains_key("MEGU_RCON_PORT")
                    && !got.contains_key("MEGU_RCON_PASS")
                    && !got.contains_key("MEGU_RCON_PASSWORD"),
                "rcon fills must be suppressed without enable-rcon=true: {body:?}"
            );
            assert!(server_properties_unknown_keys(&props).is_empty());
        }
        // `enable-rcon=true` releases the port, password and RCON fills.
        let props = server_properties_parse(
            "enable-rcon=true\nrcon.port=25575\nrcon.password=secret\n",
        );
        let got: BTreeMap<&str, String> =
            server_properties_env(&props, |_| false).into_iter().collect();
        assert_eq!(got["MEGU_RCON_PORT"], "25575");
        assert_eq!(got["MEGU_RCON_PASS"], "secret");
        assert_eq!(got["MEGU_RCON_PASSWORD"], "secret");
        // An already-set `MEGU_RCON_PORT` is upper-layer state: the gate only
        // decides what the properties file itself fills.
        let got: BTreeMap<&str, String> = server_properties_env(
            &server_properties_parse("rcon.port=25575\n"),
            |name| name == "MEGU_RCON_PORT",
        )
        .into_iter()
        .collect();
        assert!(!got.contains_key("MEGU_RCON_PORT"));
    }

    #[test]
    fn server_properties_unknown_keys_are_reported_not_dropped() {
        let props = server_properties_parse(
            "view-distance=12\n\
             viwe-distance=4\n\
             motd=hi\n\
             not-a-real-key=x\n",
        );
        let unknown = server_properties_unknown_keys(&props);
        assert_eq!(
            unknown,
            vec![
                "unknown key `not-a-real-key`".to_owned(),
                "unknown key `viwe-distance`".to_owned(),
            ]
        );
        // The real keys still exported while the typos were only warnings.
        let env = server_properties_env(&props, |_| false);
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert_eq!(got["MEGU_VIEW_DISTANCE"], "12");
        assert_eq!(got["MEGU_MOTD"], "hi");
    }

    #[test]
    fn default_file_is_five_switches_with_one_line_comments() {
        let (doc, warnings) = server_config_parse(SERVER_CONFIG_DEFAULT_FILE);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(server_config_unknown_keys(&doc).is_empty());
        let keys: Vec<&str> = doc["anticheat"].keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "antifullbright",
                "antixray",
                "check_ops",
                "enabled",
                "hide_underground"
            ]
        );
        assert_eq!(doc["anticheat"]["enabled"], ServerConfigValue::Bool(true));
        assert_eq!(doc["anticheat"]["antixray"], ServerConfigValue::Bool(true));
        assert_eq!(doc["anticheat"]["antifullbright"], ServerConfigValue::Bool(true));
        assert_eq!(doc["anticheat"]["hide_underground"], ServerConfigValue::Bool(true));
        assert_eq!(doc["anticheat"]["check_ops"], ServerConfigValue::Bool(false));
        let c = AcConfig::from_doc(&doc, |_| None);
        assert_eq!(c, AcConfig::default());
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        // Commented `[server]` keys stay unset so the built-in defaults apply.
        assert!(doc["server"].is_empty());
    }

    #[test]
    fn subset_values_comments_and_multiline_arrays() {
        let (doc, warnings) = server_config_parse(
            "a = 1 # c\n[t]\ns = \"x # not comment\"\nl = 'lit'\nf = 1.5\nb = false\narr = [\n \"A\", # first\n \"B\",\n]\nn = 25_565\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(doc[""]["a"], ServerConfigValue::Int(1));
        assert_eq!(doc["t"]["s"], ServerConfigValue::Str("x # not comment".into()));
        assert_eq!(doc["t"]["l"], ServerConfigValue::Str("lit".into()));
        assert_eq!(doc["t"]["f"], ServerConfigValue::Float(1.5));
        assert_eq!(doc["t"]["b"], ServerConfigValue::Bool(false));
        assert_eq!(
            doc["t"]["arr"],
            ServerConfigValue::Array(vec![ServerConfigValue::Str("A".into()), ServerConfigValue::Str("B".into())])
        );
        assert_eq!(doc["t"]["n"], ServerConfigValue::Int(25565));
    }

    #[test]
    fn bad_lines_and_unknown_keys_warn_instead_of_failing() {
        let (doc, warnings) = server_config_parse(
            "[anticheat]\npreset = standard\nalerts = true\nspeed_limit = 3\n[oops\n",
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(doc["anticheat"]["alerts"], ServerConfigValue::Bool(true));
        let unknown = server_config_unknown_keys(&doc);
        assert_eq!(
            unknown,
            vec!["unknown key `speed_limit` in [anticheat]".to_owned()]
        );
    }

    #[test]
    fn environment_wins_over_server_table() {
        let (doc, _) = server_config_parse("[server]\nport = 25570\nmotd = \"hi\"\nallowlist = [\"A\", \"B\"]\n");
        let env = server_config_server_env(&doc, |name| name == "MEGU_PORT");
        assert_eq!(
            env,
            vec![
                ("MEGU_MOTD", "hi".to_owned()),
                ("MEGU_ALLOWLIST", "A,B".to_owned())
            ]
        );
    }

    #[test]
    fn server_properties_parses_vanilla_keys_and_respects_env_and_toml_precedence() {
        let props = server_properties_parse(
            "# vanilla comment\n\
             ! also a comment\n\
             server-port=25565\n\
             server-ip=\n\
             motd=A\\: escaped server\\nline two\n\
             max-players: 5\n\
             player-idle-timeout=10\n\
             op-permission-level=9\n\
             white-list=true\n\
             enforce-whitelist=false\n\
             escaped\\=key=v\n\
             unicode=\\u00e9\n",
        );
        assert_eq!(props["server-port"], "25565");
        assert_eq!(props["motd"], "A: escaped server\nline two");
        assert_eq!(props["max-players"], "5");
        assert_eq!(props["escaped=key"], "v");
        assert_eq!(props["unicode"], "é");
        // Empty values parse but are never exported: Vanilla writes `server-ip=` for unset.
        assert_eq!(props["server-ip"], "");

        // `is_set` stands for "a higher layer already decided this variable":
        // the environment wins over everything, `server.toml` over the properties.
        let decided: HashSet<&str> = ["MEGU_PORT", "MEGU_MOTD"].into_iter().collect();
        let env = server_properties_env(&props, |name| decided.contains(name));
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert!(!got.contains_key("MEGU_PORT")); // server.toml won
        assert!(!got.contains_key("MEGU_MOTD")); // environment won
        assert!(!got.contains_key("MEGU_BIND_ADDR")); // `server-ip=` skipped
        assert_eq!(got["MEGU_MAX_PLAYERS"], "5");
        assert_eq!(got["MEGU_IDLE_TIMEOUT_SECS"], "600"); // minutes × 60
        assert_eq!(got["MEGU_OP_PERMISSION_LEVEL"], "4"); // clamped to 1..=4
        // `enforce-whitelist` wins over `white-list` for the shared variable.
        assert_eq!(got["MEGU_WHITELIST_ENABLED"], "false");
    }

    #[test]
    fn missing_file_is_created_once_and_never_overwritten() {
        // Under the build tree, not /tmp.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("megu-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.toml");
        let _ = std::fs::remove_file(&path);
        let doc = server_config_read_or_create(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SERVER_CONFIG_DEFAULT_FILE);
        assert_eq!(doc["anticheat"]["antixray"], ServerConfigValue::Bool(true));
        std::fs::write(&path, "[anticheat]\nantixray = false\n").unwrap();
        let doc = server_config_read_or_create(&path);
        assert_eq!(doc["anticheat"]["antixray"], ServerConfigValue::Bool(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_ops_defaults_keep_pre_lane_behaviour() {
        // The generated default file resolves to exactly the knobs a loopback
        // dev server had before the ops lanes landed.
        let (doc, warnings) = server_config_parse(SERVER_CONFIG_DEFAULT_FILE);
        assert!(warnings.is_empty(), "{warnings:?}");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert_eq!(c, RuntimeOpsConfig::default());
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        assert!(!c.online_mode);
        assert!(!c.velocity_forwarding);
        assert_eq!(c.velocity_forwarding_secret(), None);
        assert_eq!(c.autosave_interval_secs, 300);
        assert_eq!(c.backup_max_keep, 5);
        assert!(!c.lan_broadcast);
        assert!(c.auto_load_balance);
        assert_eq!(c.target_version, "1.21.1");
        assert_eq!(c.version_pack_dir, None);
    }

    #[test]
    fn runtime_ops_reads_server_table_and_environment_wins() {
        let (doc, warnings) = server_config_parse(
            "[server]\n\
             online_mode = true\n\
             velocity_forwarding = true\n\
             velocity_forwarding_secret = \"file-secret\"\n\
             autosave_interval_secs = 60\n\
             backup_max_keep = 3\n\
             lan_broadcast = true\n\
             auto_load_balance = false\n\
             target_version = \"26.3\"\n\
             version_pack_dir = \"packs/1214\"\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(server_config_unknown_keys(&doc).is_empty());
        let env = |name: &str| match name {
            "MEGU_ONLINE_MODE" => Some("0".to_owned()),
            "MEGU_AUTOSAVE_SECS" => Some("45".to_owned()),
            "MEGU_BACKUP_MAX_KEEP" => Some("8".to_owned()),
            "MEGU_TARGET_VERSION" => Some("1.21.1".to_owned()),
            _ => None,
        };
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        // The environment wins over the file on every knob it sets.
        assert!(!c.online_mode);
        assert_eq!(c.autosave_interval_secs, 45);
        assert_eq!(c.backup_max_keep, 8);
        assert_eq!(c.target_version, "1.21.1");
        // Everything else comes from the file.
        assert!(c.velocity_forwarding);
        assert_eq!(c.velocity_forwarding_secret(), Some("file-secret"));
        assert!(c.lan_broadcast);
        assert!(!c.auto_load_balance);
        assert_eq!(c.version_pack_dir.as_deref(), Some("packs/1214"));
        assert!(c.notes.is_empty(), "{:?}", c.notes);
    }

    #[test]
    fn runtime_ops_new_keys_export_through_the_env_layer() {
        // A `[server]` key with no environment override lands in the same
        // `MEGU_*` variable its env-reading consumers already watch.
        let (doc, _) = server_config_parse(
            "[server]\nonline_mode = true\ntarget_version = \"26.3\"\ntarget_mc_version = \"1.21.1\"\nversion_pack_dir = \"p\"\n",
        );
        let env = server_config_server_env(&doc, |name| name == "MEGU_ONLINE_MODE");
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert!(!got.contains_key("MEGU_ONLINE_MODE")); // env already won
        assert_eq!(got["MEGU_TARGET_VERSION"], "26.3");
        assert_eq!(got["MEGU_MC_VERSION"], "1.21.1");
        assert_eq!(got["MEGU_VERSION_DIR"], "p");
        // And resolving the file sees the value the env layer would have set.
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert!(c.online_mode);
    }

    #[test]
    fn online_mode_maps_from_vanilla_server_properties() {
        let props = server_properties_parse("online-mode=true\nmotd=x\n");
        let env = server_properties_env(&props, |_| false);
        let got: BTreeMap<&str, String> = env.into_iter().collect();
        assert_eq!(got["MEGU_ONLINE_MODE"], "true");
        // A higher layer that already decided the variable keeps it.
        let env = server_properties_env(&props, |n| n == "MEGU_ONLINE_MODE");
        assert!(env.iter().all(|(name, _)| *name != "MEGU_ONLINE_MODE"));
    }

    #[test]
    fn runtime_ops_secret_is_redacted_in_debug_and_summary() {
        let (doc, _) = server_config_parse(
            "[server]\nvelocity_forwarding = true\nvelocity_forwarding_secret = \"hunter2\"\n",
        );
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        let debug = format!("{c:?}");
        let summary = c.summary();
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(!summary.contains("hunter2"), "{summary}");
        assert!(summary.contains("[REDACTED]"), "{summary}");
        // The real value is still reachable through the dedicated accessor.
        assert_eq!(c.velocity_forwarding_secret(), Some("hunter2"));
    }

    #[test]
    fn runtime_ops_clamps_autosave_and_notes_bad_values() {
        let (doc, _) = server_config_parse(
            "[server]\nautosave_interval_secs = 2\nbackup_max_keep = \"many\"\nonline_mode = 7\n",
        );
        let env = |name: &str| match name {
            "MEGU_AUTOSAVE_SECS" => Some("notanum".to_owned()),
            _ => None,
        };
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        // The unparsable env value falls back to the file's 2, which is then
        // clamped into the safe band.
        assert_eq!(c.autosave_interval_secs, AUTOSAVE_SECS_MIN);
        assert_eq!(c.backup_max_keep, BACKUP_MAX_KEEP_DEFAULT);
        assert!(!c.online_mode);
        assert_eq!(c.notes.len(), 4, "{:?}", c.notes);
        assert!(c.notes.iter().any(|n| n.contains("unrecognized value `notanum`")));
        assert!(c.notes.iter().any(|n| n.contains("clamped")));
        assert!(c.notes.iter().any(|n| n.contains("backup_max_keep")));
        assert!(c.notes.iter().any(|n| n.contains("online_mode")));
    }

    #[test]
    fn runtime_ops_autosave_upper_bound_and_huge_keep_are_bounded() {
        let (doc, _) = server_config_parse("[server]\nautosave_interval_secs = 99999999\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert_eq!(c.autosave_interval_secs, AUTOSAVE_SECS_MAX);
        assert!(c.notes.iter().any(|n| n.contains("clamped")));
    }

    #[test]
    fn runtime_ops_flags_velocity_forwarding_without_a_secret() {
        let (doc, _) = server_config_parse("[server]\nvelocity_forwarding = true\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert!(c.notes.iter().any(|n| n.contains("velocity_forwarding") && n.contains("secret")));
        let (doc, _) = server_config_parse("[server]\nvelocity_forwarding_secret = \"s\"\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert!(c.notes.iter().any(|n| n.contains("no effect")));
    }

    #[test]
    fn runtime_ops_env_bool_spellings_and_empty_env_falls_back() {
        let doc = ServerConfigDoc::new();
        let env = |name: &str| match name {
            "MEGU_ONLINE_MODE" => Some("TRUE".to_owned()),
            "MEGU_LAN_BROADCAST" => Some(" yes ".to_owned()),
            "MEGU_AUTO_LOAD_BALANCE" => Some("".to_owned()),
            _ => None,
        };
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        assert!(c.online_mode);
        assert!(c.lan_broadcast);
        // An empty env var is unset, so the default still applies.
        assert!(c.auto_load_balance);
        assert!(c.notes.is_empty(), "{:?}", c.notes);
    }

    #[test]
    fn runtime_ops_target_version_accepts_both_targets_and_env_wins() {
        // `[server] target_version` accepts exactly the two first-class targets.
        for version in RUNTIME_OPS_TARGET_VERSIONS {
            let (doc, _) = server_config_parse(&format!(
                "[server]\ntarget_version = \"{version}\"\n"
            ));
            assert!(server_config_unknown_keys(&doc).is_empty());
            let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
            assert_eq!(c.target_version, *version);
            assert!(c.notes.is_empty(), "{:?}", c.notes);
        }
        // `MEGU_TARGET_VERSION` beats the file.
        let (doc, _) = server_config_parse("[server]\ntarget_version = \"26.3\"\n");
        let env = |name: &str| (name == "MEGU_TARGET_VERSION").then(|| "1.21.1".to_owned());
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        assert_eq!(c.target_version, "1.21.1");
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        // The canonical pair outranks the legacy one entirely.
        let env = |name: &str| match name {
            "MEGU_TARGET_VERSION" => Some("26.3".to_owned()),
            "MEGU_MC_VERSION" => Some("1.21.1".to_owned()),
            _ => None,
        };
        let c = RuntimeOpsConfig::from_doc(&doc, env);
        assert_eq!(c.target_version, "26.3");
    }

    #[test]
    fn runtime_ops_target_version_legacy_pair_and_fail_closed() {
        // The pre-Two-Target `target_mc_version` / `MEGU_MC_VERSION` pair
        // still fills the knob when the canonical one is unset.
        let (doc, _) = server_config_parse("[server]\ntarget_mc_version = \"26.3\"\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert_eq!(c.target_version, "26.3");
        assert!(c.notes.is_empty(), "{:?}", c.notes);
        let c = RuntimeOpsConfig::from_doc(&doc, |name| {
            (name == "MEGU_MC_VERSION").then(|| "1.21.1".to_owned())
        });
        assert_eq!(c.target_version, "1.21.1");
        // An unknown file value is noted and fails closed to the default.
        let (doc, _) = server_config_parse("[server]\ntarget_version = \"1.21.4\"\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert_eq!(c.target_version, "1.21.1");
        assert!(c
            .notes
            .iter()
            .any(|n| n.contains("1.21.4") && n.contains("not a supported release")));
        // An invalid env value falls through to a valid file layer.
        let (doc, _) = server_config_parse("[server]\ntarget_version = \"26.3\"\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |name| {
            (name == "MEGU_TARGET_VERSION").then(|| "beta".to_owned())
        });
        assert_eq!(c.target_version, "26.3");
        assert!(c.notes.iter().any(|n| n.contains("beta")));
        // Every layer invalid means the default, with one note per bad value.
        let (doc, _) = server_config_parse("[server]\ntarget_mc_version = \"snapshot-25w99a\"\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |name| {
            (name == "MEGU_MC_VERSION").then(|| "1.99".to_owned())
        });
        assert_eq!(c.target_version, "1.21.1");
        assert_eq!(
            c.notes
                .iter()
                .filter(|n| n.contains("not a supported release"))
                .count(),
            2
        );
        // A non-string file value is a note, not a crash or a guessed version.
        let (doc, _) = server_config_parse("[server]\ntarget_version = 26.3\n");
        let c = RuntimeOpsConfig::from_doc(&doc, |_| None);
        assert_eq!(c.target_version, "1.21.1");
        assert!(c.notes.iter().any(|n| n.contains("must be a string")));
    }
}

#[cfg(test)]
mod admin_wire_config_tests {
    use super::*;

    /// The listen-port default is 25565 everywhere: `server.toml` parses it,
    /// exports `MEGU_PORT` and the properties layer accepts it too.
    #[test]
    fn port_25565_is_the_default_across_layers() {
        let (doc, warnings) = server_config_parse("[server]\nport = 25565\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        let env = server_config_server_env(&doc, |_| false);
        assert!(env.contains(&("MEGU_PORT", "25565".to_owned())));
        let props = server_properties_parse("server-port=25565\n");
        let env = server_properties_env(&props, |_| false);
        assert!(env.contains(&("MEGU_PORT", "25565".to_owned())));
    }

    /// `[[worlds]]` array-of-tables flatten into `worlds#N`, `[servers.<name>]`
    /// dotted tables and `[lifecycle]`/`[remote_control]` parse, and the
    /// unknown-key check accepts them.
    #[test]
    fn operator_tables_parse_without_unknown_key_warnings() {
        let _guard = world_guard();
        let text = r#"
[servers]
fallback = "lobby"

[servers.lobby]
host = "127.0.0.1"
port = 25566

[servers.game]
host = "10.0.0.5"
port = 25567

[[worlds]]
id = "farm"
generator = "flat"
pvp = false
spawn = [0.5, 64.0, 8.5]

[[worlds]]
id = "voidlab"
generator = "void"

[lifecycle]
idle_sleep_secs = 120
idle_shutdown_secs = 3600

[remote_control]
enabled = true
bind_addr = "127.0.0.1:25575"
admin_token = "secret"
"#;
        let (doc, warnings) = server_config_parse(text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(doc.contains_key("servers"));
        assert!(doc.contains_key("servers.lobby"));
        assert!(doc.contains_key("servers.game"));
        assert!(doc.contains_key("worlds#0"));
        assert!(doc.contains_key("worlds#1"));
        assert_eq!(
            doc["worlds#0"]["id"],
            ServerConfigValue::Str("farm".to_owned())
        );
        assert_eq!(
            doc["worlds#1"]["generator"],
            ServerConfigValue::Str("void".to_owned())
        );
        assert!(server_config_unknown_keys(&doc).is_empty());

        let notes = apply_operator_tables(&doc);
        assert!(notes.is_empty(), "{notes:?}");
        let directory = hub_directory().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(directory.resolve("lobby"), Some(("127.0.0.1", 25566)));
        assert_eq!(
            directory.fallback().map(|(n, h, p)| (n.to_owned(), h.to_owned(), p)),
            Some(("lobby".to_owned(), "127.0.0.1".to_owned(), 25566))
        );
        drop(directory);
        let worlds = managed_worlds().lock().unwrap_or_else(|e| e.into_inner());
        let farm = worlds.get("farm").expect("farm world applied");
        assert_eq!(farm.spawn, [0.5, 64.0, 8.5]);
        assert!(!farm.pvp);
        drop(worlds);
        assert!(DimId::parse_dimension_id("megu:farm").is_some());
        assert!(DimId::parse_dimension_id("megu:voidlab").is_some());

        let tables = operator_tables();
        let lifecycle = tables.lifecycle.as_ref().expect("[lifecycle] resolved");
        assert_eq!(lifecycle.idle_sleep_secs, 120);
        assert_eq!(lifecycle.idle_shutdown_secs, Some(3600));
        let rc = tables
            .remote_control
            .as_ref()
            .expect("enabled [remote_control] resolves");
        assert_eq!(rc.bind_addr, "127.0.0.1:25575");
        assert_eq!(rc.admin_token, "secret");

        let mut directory = hub_directory().lock().unwrap_or_else(|e| e.into_inner());
        directory.remove_server("lobby");
        directory.remove_server("game");
        drop(directory);
        let mut worlds = managed_worlds().lock().unwrap_or_else(|e| e.into_inner());
        worlds.remove("farm");
        worlds.remove("voidlab");
    }
}
